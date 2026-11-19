//! Model data katalog (`ModelInfo`, `ModelCost`, `ProviderCatalogEntry`, `ModelCatalog`)
//! serta filter scope (`glob_match`, `resolve_model_scope`).

use super::builtin_providers::builtin_providers;
use super::discovery::{infer_api_from_provider, infer_input_from_id, infer_reasoning_from_id};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::{BTreeMap, HashSet};

fn default_true() -> bool {
    true
}

fn default_api() -> String {
    "google-generative-ai".to_string()
}

fn default_input_modalities() -> Vec<String> {
    vec!["text".to_string(), "image".to_string()]
}

pub const CANONICAL_THINKING_LEVELS: &[&str] =
    &["off", "minimal", "low", "medium", "high", "xhigh"];

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ModelCost {
    #[serde(default)]
    pub input: f64,
    #[serde(default)]
    pub output: f64,
    #[serde(rename = "cacheRead", default)]
    pub cache_read: f64,
    #[serde(rename = "cacheWrite", default)]
    pub cache_write: f64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ModelInfo {
    /// ID model API (misal: `gemini-3.5-flash-lite`, `gemini-2.5-pro`).
    pub id: String,
    /// Nama tampilan ramah UI.
    pub label: String,
    /// Nama provider (misal: `google`, `openrouter`, `oopbot-local`).
    pub provider: String,
    /// Apakah mendukung tool/function calling.
    #[serde(default = "default_true")]
    pub supports_tools: bool,
    /// Batas token input (context window) bila dilaporkan server.
    pub input_token_limit: Option<u64>,
    /// Batas token output (`maxTokens`) bila dilaporkan server atau diatur aplikasi.
    pub output_token_limit: Option<u64>,
    /// Deskripsi singkat untuk pemilih model di UI/CLI.
    #[serde(default)]
    pub description: String,
    /// Jenis protokol API (`google-generative-ai`, `openai-completions`, `openai-responses`, `anthropic-messages`).
    #[serde(default = "default_api")]
    pub api: String,
    /// Apakah model mendukung mode penalaran (`thinking` / `reasoning`).
    #[serde(default = "default_true")]
    pub reasoning: bool,
    /// Pemetaan level thinking aplikasi (`minimal`..`xhigh`) ke nilai effort provider.
    #[serde(
        rename = "thinkingLevelMap",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub thinking_level_map: Option<BTreeMap<String, Option<String>>>,
    /// Modalitas input yang didukung (`["text", "image"]` atau `["text"]`).
    #[serde(default = "default_input_modalities")]
    pub input: Vec<String>,
    /// Informasi biaya per 1 juta token (`input`, `output`, `cacheRead`, `cacheWrite`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cost: Option<ModelCost>,
}

impl ModelInfo {
    /// Konversi ke objek JSON standar protokol OOPBoT (`AgentProcess.kt`).
    pub fn to_oopbot_json(&self, provider_override: Option<&str>) -> Value {
        let provider = provider_override.unwrap_or(&self.provider);
        let input = if self.input.is_empty() {
            vec!["text".to_string(), "image".to_string()]
        } else {
            self.input.clone()
        };
        let mut obj = json!({
            "provider": provider,
            "id": self.id,
            "name": self.label,
            "api": self.api,
            "reasoning": self.reasoning,
            "input": input,
            "contextWindow": self.input_token_limit.unwrap_or(1_048_576),
            "maxTokens": self.output_token_limit.unwrap_or(65_536),
        });
        if let Some(map) = &self.thinking_level_map {
            obj["thinkingLevelMap"] = json!(map);
        }
        if let Some(cost) = &self.cost {
            obj["cost"] = json!({
                "input": cost.input,
                "output": cost.output,
                "cacheRead": cost.cache_read,
                "cacheWrite": cost.cache_write,
            });
        }
        obj
    }

    /// Terapkan override dari `models.json` (`modelOverrides.<id>` atau definisi model kustom)
    /// sehingga seluruh pengaturan dari aplikasi Android langsung berlaku di Rust.
    pub fn apply_override(&mut self, ov: &Value) {
        if !ov.is_object() {
            return;
        }
        if let Some(name) = ov["name"]
            .as_str()
            .or_else(|| ov["label"].as_str())
            .filter(|s| !s.trim().is_empty())
        {
            self.label = name.to_string();
        }
        if let Some(api) = ov["api"].as_str().filter(|s| !s.trim().is_empty()) {
            self.api = api.to_string();
        }
        if let Some(reasoning) = ov["reasoning"].as_bool() {
            self.reasoning = reasoning;
            if !reasoning {
                self.thinking_level_map = None;
            }
        }
        if let Some(cw) = ov["contextWindow"]
            .as_u64()
            .or_else(|| ov["input_token_limit"].as_u64())
            .filter(|&n| n > 0)
        {
            self.input_token_limit = Some(cw);
        }
        if let Some(mt) = ov["maxTokens"]
            .as_u64()
            .or_else(|| ov["output_token_limit"].as_u64())
            .filter(|&n| n > 0)
        {
            self.output_token_limit = Some(mt);
        }
        if let Some(inp_arr) = ov["input"].as_array() {
            let parsed: Vec<String> = inp_arr
                .iter()
                .filter_map(|v| v.as_str().map(str::to_string))
                .collect();
            if !parsed.is_empty() {
                self.input = parsed;
            }
        }
        if let Some(tlm_val) = ov.get("thinkingLevelMap") {
            if tlm_val.is_null() {
                self.thinking_level_map = None;
            } else if let Some(tlm_obj) = tlm_val.as_object() {
                let mut map = BTreeMap::new();
                for (k, v) in tlm_obj {
                    let mapped = v.as_str().map(str::to_string);
                    map.insert(k.clone(), mapped);
                }
                self.thinking_level_map = Some(map);
            }
        }
        if let Some(cost_obj) = ov.get("cost").and_then(|v| v.as_object()) {
            let prev = self.cost.clone().unwrap_or(ModelCost {
                input: 0.0,
                output: 0.0,
                cache_read: 0.0,
                cache_write: 0.0,
            });
            self.cost = Some(ModelCost {
                input: cost_obj
                    .get("input")
                    .and_then(|v| v.as_f64())
                    .unwrap_or(prev.input),
                output: cost_obj
                    .get("output")
                    .and_then(|v| v.as_f64())
                    .unwrap_or(prev.output),
                cache_read: cost_obj
                    .get("cacheRead")
                    .or_else(|| cost_obj.get("cache_read"))
                    .and_then(|v| v.as_f64())
                    .unwrap_or(prev.cache_read),
                cache_write: cost_obj
                    .get("cacheWrite")
                    .or_else(|| cost_obj.get("cache_write"))
                    .and_then(|v| v.as_f64())
                    .unwrap_or(prev.cache_write),
            });
        }
    }

    /// Bangun `ModelInfo` dari item `models` di `models.json` (baik string ID maupun objek penuh).
    pub fn from_oopbot_json(item: &Value, default_provider: &str, default_api: &str) -> Option<Self> {
        if let Some(id) = item.as_str().filter(|s| !s.trim().is_empty()) {
            return Some(Self {
                id: id.to_string(),
                label: id.to_string(),
                provider: default_provider.to_string(),
                supports_tools: true,
                input_token_limit: Some(128_000),
                output_token_limit: Some(16_384),
                description: format!("Provider: {}", default_provider),
                api: default_api.to_string(),
                reasoning: infer_reasoning_from_id(id),
                thinking_level_map: None,
                input: infer_input_from_id(id),
                cost: None,
            });
        }
        let id = item["id"].as_str().filter(|s| !s.trim().is_empty())?;
        let provider = item["provider"]
            .as_str()
            .filter(|s| !s.trim().is_empty())
            .unwrap_or(default_provider);
        let mut info = Self {
            id: id.to_string(),
            label: item["name"]
                .as_str()
                .or_else(|| item["label"].as_str())
                .filter(|s| !s.trim().is_empty())
                .unwrap_or(id)
                .to_string(),
            provider: provider.to_string(),
            supports_tools: item["supports_tools"].as_bool().unwrap_or(true),
            input_token_limit: item["contextWindow"]
                .as_u64()
                .or_else(|| item["input_token_limit"].as_u64())
                .or(Some(128_000)),
            output_token_limit: item["maxTokens"]
                .as_u64()
                .or_else(|| item["output_token_limit"].as_u64())
                .or(Some(16_384)),
            description: item["description"]
                .as_str()
                .unwrap_or("Configured model")
                .to_string(),
            api: item["api"]
                .as_str()
                .filter(|s| !s.trim().is_empty())
                .unwrap_or(default_api)
                .to_string(),
            reasoning: item["reasoning"]
                .as_bool()
                .unwrap_or_else(|| infer_reasoning_from_id(id)),
            thinking_level_map: None,
            input: infer_input_from_id(id),
            cost: None,
        };
        info.apply_override(item);
        Some(info)
    }

    /// Daftar level thinking yang tersedia untuk model ini sesuai pengaturan aplikasi.
    pub fn available_thinking_levels(&self) -> Vec<String> {
        if !self.reasoning {
            return vec!["off".to_string()];
        }
        let mut levels = vec!["off".to_string()];
        if let Some(map) = &self.thinking_level_map {
            for &lvl in &CANONICAL_THINKING_LEVELS[1..] {
                if let Some(Some(mapped)) = map.get(lvl) {
                    if !mapped.is_empty() && mapped != "none" && mapped != "off" {
                        levels.push(lvl.to_string());
                    }
                }
            }
            if levels.len() == 1 {
                for (k, v) in map {
                    if k != "off"
                        && v.as_deref()
                            .map(|s| !s.is_empty() && s != "none" && s != "off")
                            .unwrap_or(false)
                        && !levels.contains(k)
                    {
                        levels.push(k.clone());
                    }
                }
            }
            return levels;
        }
        for &lvl in &CANONICAL_THINKING_LEVELS[1..] {
            levels.push(lvl.to_string());
        }
        levels
    }

    /// Pastikan level thinking valid untuk model ini; bila tidak didukung, kembalikan fallback terbaik.
    pub fn clamp_thinking_level(&self, requested: &str) -> String {
        let supported = self.available_thinking_levels();
        if supported.iter().any(|l| l == requested) {
            return requested.to_string();
        }
        if !self.reasoning || supported.len() <= 1 {
            return "off".to_string();
        }
        if supported.iter().any(|l| l == "medium") {
            return "medium".to_string();
        }
        supported
            .get(1)
            .cloned()
            .unwrap_or_else(|| "off".to_string())
    }
}

/// Definisi satu provider di katalog referensi bawaan Rust.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProviderCatalogEntry {
    pub id: String,
    pub name: String,
    #[serde(rename = "baseUrl")]
    pub base_url: String,
    pub api: String,
    pub models: Vec<ModelInfo>,
}

impl ProviderCatalogEntry {
    pub fn to_oopbot_json(&self) -> Value {
        let models: Vec<Value> = self
            .models
            .iter()
            .map(|m| m.to_oopbot_json(Some(&self.id)))
            .collect();
        json!({
            "id": self.id,
            "name": self.name,
            "baseUrl": self.base_url,
            "api": self.api,
            "models": models,
        })
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModelCatalog {
    models: Vec<ModelInfo>,
    default_model_id: String,
}

impl Default for ModelCatalog {
    fn default() -> Self {
        Self::builtin()
    }
}

impl ModelCatalog {
    /// Katalog model bawaan dari provider referensi utama (`google`).
    pub fn builtin() -> Self {
        let google_models = builtin_providers()
            .into_iter()
            .find(|p| p.id == "google")
            .map(|p| {
                p.models
                    .into_iter()
                    .map(|mut m| {
                        m.provider = "gemini".to_string();
                        m
                    })
                    .collect()
            })
            .unwrap_or_default();
        Self {
            models: google_models,
            default_model_id: "gemini-3.5-flash-lite".into(),
        }
    }

    /// Katalog gabungan seluruh provider bawaan.
    pub fn all_builtin() -> Self {
        let mut models = Vec::new();
        for p in builtin_providers() {
            models.extend(p.models);
        }
        Self {
            models,
            default_model_id: "gemini-3.5-flash-lite".into(),
        }
    }

    pub fn list(&self) -> &[ModelInfo] {
        &self.models
    }

    pub fn default_model(&self) -> &str {
        &self.default_model_id
    }

    pub fn set_default_model(&mut self, model_id: &str) {
        if self.find(model_id).is_none() {
            self.register_custom(model_id, model_id);
        }
        self.default_model_id = model_id.to_string();
    }

    pub fn find(&self, query: &str) -> Option<&ModelInfo> {
        if let Ok(idx) = query.parse::<usize>() {
            if idx >= 1 && idx <= self.models.len() {
                return Some(&self.models[idx - 1]);
            }
        }
        self.models
            .iter()
            .find(|m| m.id.eq_ignore_ascii_case(query))
            .or_else(|| {
                let stripped = query.split('/').next_back().unwrap_or(query);
                self.models
                    .iter()
                    .find(|m| m.id.eq_ignore_ascii_case(stripped))
            })
    }

    pub fn register_custom(&mut self, id: &str, label: &str) {
        if self.models.iter().any(|m| m.id == id) {
            return;
        }
        self.models.push(ModelInfo {
            id: id.to_string(),
            label: label.to_string(),
            provider: "custom".into(),
            supports_tools: true,
            input_token_limit: None,
            output_token_limit: None,
            description: "Model kustom".into(),
            api: infer_api_from_provider("custom"),
            reasoning: infer_reasoning_from_id(id),
            thinking_level_map: None,
            input: infer_input_from_id(id),
            cost: None,
        });
    }

    pub fn upsert_model(&mut self, info: ModelInfo) {
        if let Some(existing) = self.models.iter_mut().find(|m| m.id == info.id) {
            *existing = info;
        } else {
            self.models.push(info);
        }
    }

    /// Perbarui katalog dengan hasil scan live dari endpoint `/models`.
    /// Mengembalikan jumlah model hasil scan.
    pub fn replace_with_scanned(&mut self, scanned: Vec<ModelInfo>) -> usize {
        if scanned.is_empty() {
            return 0;
        }
        let count = scanned.len();
        self.models = scanned;
        if self.find(&self.default_model_id).is_none() {
            if let Some(first) = self.models.first() {
                self.default_model_id = first.id.clone();
            }
        }
        count
    }
}

/// Cocokkan teks dengan pola glob sederhana (`*` dan `?`, case-insensitive).
pub fn glob_match(pattern: &str, text: &str) -> bool {
    let p: Vec<char> = pattern.to_ascii_lowercase().chars().collect();
    let t: Vec<char> = text.to_ascii_lowercase().chars().collect();
    let (mut pi, mut ti) = (0usize, 0usize);
    let (mut star_pi, mut star_ti) = (None::<usize>, 0usize);

    while ti < t.len() {
        if pi < p.len() && (p[pi] == '?' || p[pi] == t[ti]) {
            pi += 1;
            ti += 1;
        } else if pi < p.len() && p[pi] == '*' {
            star_pi = Some(pi);
            pi += 1;
            star_ti = ti;
        } else if let Some(sp) = star_pi {
            pi = sp + 1;
            star_ti += 1;
            ti = star_ti;
        } else {
            return false;
        }
    }
    while pi < p.len() && p[pi] == '*' {
        pi += 1;
    }
    pi == p.len()
}

/// Filter dan urutkan daftar model yang tersedia berdasarkan `enabledModels` di `settings.json`
/// (mendukung format `<provider>/<modelId>`, `<modelId>`, wildcard `*`, serta akhiran `:thinkingLevel`).
pub fn resolve_model_scope(
    patterns: &[String],
    available: &[ModelInfo],
) -> Vec<(ModelInfo, Option<String>)> {
    let mut scoped = Vec::new();
    let mut seen = HashSet::new();

    for raw_pattern in patterns {
        let trimmed = raw_pattern.trim();
        if trimmed.is_empty() {
            continue;
        }
        let (core, thinking_level) = if let Some(colon_idx) = trimmed.rfind(':') {
            let suffix = &trimmed[colon_idx + 1..];
            if colon_idx > 0 && CANONICAL_THINKING_LEVELS.contains(&suffix) {
                (&trimmed[..colon_idx], Some(suffix.to_string()))
            } else {
                (trimmed, None)
            }
        } else {
            (trimmed, None)
        };

        for model in available {
            let full_ref = format!("{}/{}", model.provider, model.id);
            let matched = if core.contains('/') {
                glob_match(core, &full_ref)
            } else {
                glob_match(core, &model.id) || glob_match(core, &model.label)
            };
            if matched && seen.insert(full_ref) {
                scoped.push((model.clone(), thinking_level.clone()));
            }
        }
    }
    scoped
}
