//! Estimator token dan evaluasi anggaran konteks (`ContextPolicy`, `BudgetSnapshot`, `BudgetStatus`).
//!
//! Modul ini murni (deterministik, bebas I/O & jaringan) sehingga dapat dipakai bersama
//! oleh `agent/`, CLI, maupun RPC dan diuji tanpa mock HTTP.

use serde_json::Value;

pub const DEFAULT_MAX_INPUT_TOKENS: u64 = 128_000;
pub const MIN_RESERVE_OUTPUT_TOKENS: u64 = 8_192;
pub const DEFAULT_AUTO_COMPACT_RATIO: f32 = 0.80;
pub const DEFAULT_KEEP_RECENT_TOKENS: u64 = 20_000;

/// Kebijakan anggaran konteks untuk satu model/sesi.
#[derive(Debug, Clone)]
pub struct ContextPolicy {
    /// Batas context window model (diambil dari `ModelInfo.input_token_limit`, fallback 128k).
    pub max_input_tokens: u64,
    /// Cadangan token untuk output model + margin keamanan.
    pub reserve_output_tokens: u64,
    /// Rasio pemicu auto-compaction terhadap `usable_limit` (default `0.80`).
    pub auto_compact_ratio: f32,
    /// Target token pesan terbaru yang tetap dipertahankan utuh saat compaction.
    pub keep_recent_tokens: u64,
}

impl Default for ContextPolicy {
    fn default() -> Self {
        Self::for_model_limits(Some(DEFAULT_MAX_INPUT_TOKENS), Some(MIN_RESERVE_OUTPUT_TOKENS))
    }
}

impl ContextPolicy {
    /// Bangun kebijakan konteks yang aman dari limit input/output model aktif.
    pub fn for_model_limits(
        max_input_tokens: Option<u64>,
        max_output_tokens: Option<u64>,
    ) -> Self {
        let max_in = max_input_tokens
            .filter(|&n| n > 0)
            .unwrap_or(DEFAULT_MAX_INPUT_TOKENS);
        // Cadangkan output tokens secukupnya, maksimal 25% dari context window agar usable_limit tidak 0.
        let desired_reserve = max_output_tokens
            .filter(|&n| n > 0)
            .unwrap_or(MIN_RESERVE_OUTPUT_TOKENS)
            .max(MIN_RESERVE_OUTPUT_TOKENS);
        let max_reserve = (max_in / 4).max(1);
        let reserve = desired_reserve.min(max_reserve);
        let usable = max_in.saturating_sub(reserve).max(1);
        let keep_recent = DEFAULT_KEEP_RECENT_TOKENS.min((usable / 3).max(1));

        Self {
            max_input_tokens: max_in,
            reserve_output_tokens: reserve,
            auto_compact_ratio: DEFAULT_AUTO_COMPACT_RATIO,
            keep_recent_tokens: keep_recent,
        }
    }

    /// Kapasitas input efektif setelah dikurangi cadangan output.
    pub fn usable_limit(&self) -> u64 {
        self.max_input_tokens
            .saturating_sub(self.reserve_output_tokens)
            .max(1)
    }

    /// Ambang token pemicu `NeedsCompaction`.
    pub fn compact_threshold(&self) -> u64 {
        let ratio = self.auto_compact_ratio.clamp(0.1, 0.99);
        ((self.usable_limit() as f64) * (ratio as f64)).round() as u64
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BudgetStatus {
    /// Masih di bawah ambang `auto_compact_ratio`.
    WithinBudget,
    /// Melampaui ambang pemicu -> disarankan compaction sebelum/di antara step LLM.
    NeedsCompaction,
    /// Melampaui batas keras `usable_limit` -> wajib compaction sebelum request dikirim.
    OverLimit,
}

#[derive(Debug, Clone)]
pub struct BudgetSnapshot {
    pub system_tokens: u64,
    pub tool_schema_tokens: u64,
    pub history_tokens: u64,
    pub total_estimated_tokens: u64,
    /// Token input aktual dari respons LLM terakhir (bila tersedia), dipakai mengkalibrasi batas.
    pub last_observed_input_tokens: Option<u64>,
    pub usable_limit: u64,
    pub max_input_tokens: u64,
    pub status: BudgetStatus,
}

impl BudgetSnapshot {
    /// Nilai token efektif yang dipakai untuk keputusan compaction
    /// (`max(estimasi_lokal, observasi_provider_terakhir)`).
    pub fn effective_tokens(&self) -> u64 {
        self.total_estimated_tokens
            .max(self.last_observed_input_tokens.unwrap_or(0))
    }

    /// Persentase pemakaian terhadap `usable_limit` (0.0 ..= 100.0+).
    pub fn usage_percent(&self) -> f64 {
        if self.usable_limit == 0 {
            return 100.0;
        }
        (self.effective_tokens() as f64 / self.usable_limit() as f64) * 100.0
    }

    fn usable_limit(&self) -> u64 {
        self.usable_limit.max(1)
    }
}

/// Estimasi kasar jumlah token dari teks (~4 karakter per token).
pub fn estimate_text_tokens(text: &str) -> u64 {
    let chars = text.chars().count() as u64;
    if chars == 0 {
        0
    } else {
        chars.div_ceil(4).max(1)
    }
}

/// Estimasi jumlah token dari satu entri `Value` JSON kanonik.
/// Memperlakukan blob base64 gambar (`inlineData.data`) dengan biaya tetap agar tidak
/// menggelembungkan estimasi teks secara palsu.
pub fn estimate_value_tokens(val: &Value) -> u64 {
    match val {
        Value::Null => 1,
        Value::Bool(_) => 1,
        Value::Number(n) => estimate_text_tokens(&n.to_string()),
        Value::String(s) => estimate_text_tokens(s),
        Value::Array(arr) => arr.iter().map(estimate_value_tokens).sum::<u64>() + 2,
        Value::Object(map) => {
            if let Some(inline) = map.get("inlineData").and_then(|v| v.as_object()) {
                let data_len = inline
                    .get("data")
                    .and_then(|v| v.as_str())
                    .map(str::len)
                    .unwrap_or(0);
                // Estimasi konservatif untuk 1 gambar inline (~258..1024 token tergantung ukuran).
                return ((data_len as u64) / 2_048).clamp(258, 1_536);
            }
            let mut sum = 4u64;
            for (k, v) in map {
                sum += estimate_text_tokens(k) + estimate_value_tokens(v);
            }
            sum
        }
    }
}

/// Evaluasi penggunaan anggaran konteks sebelum mengirim request ke LLM.
pub fn evaluate_budget(
    policy: &ContextPolicy,
    system_instruction: &str,
    tool_declarations: &[Value],
    history: &[Value],
    last_observed_input_tokens: Option<u64>,
) -> BudgetSnapshot {
    let system_tokens = estimate_text_tokens(system_instruction);
    let tool_schema_tokens: u64 = tool_declarations.iter().map(estimate_value_tokens).sum();
    let history_tokens: u64 = history.iter().map(estimate_value_tokens).sum();
    let total_estimated_tokens = system_tokens + tool_schema_tokens + history_tokens;

    let effective = total_estimated_tokens.max(last_observed_input_tokens.unwrap_or(0));
    let usable_limit = policy.usable_limit();
    let threshold = policy.compact_threshold();

    let status = if effective >= usable_limit {
        BudgetStatus::OverLimit
    } else if effective >= threshold {
        BudgetStatus::NeedsCompaction
    } else {
        BudgetStatus::WithinBudget
    };

    BudgetSnapshot {
        system_tokens,
        tool_schema_tokens,
        history_tokens,
        total_estimated_tokens,
        last_observed_input_tokens,
        usable_limit,
        max_input_tokens: policy.max_input_tokens,
        status,
    }
}
