//! Layer 1 Context Management (`src/context/`):
//! anggaran token, kebijakan compaction, cut-point deterministik (anti-belah pasangan tool call),
//! dan orkestrasi [`ContextManager`] yang agnostik terhadap provider HTTP maupun interface CLI/RPC.
//!
//! Batas DAG: modul ini hanya boleh mengimpor `crate::common` (Layer 0) + crate standar,
//! dan TIDAK BOLEH mengimpor `agent/`, `plugins/`, `interfaces/`, `llm/`, `tools/`, atau `config/`.

pub mod budget;
pub mod compactor;

#[cfg(test)]
mod tests;

pub use budget::{
    estimate_text_tokens, estimate_value_tokens, evaluate_budget, BudgetSnapshot, BudgetStatus,
    ContextPolicy, DEFAULT_AUTO_COMPACT_RATIO, DEFAULT_KEEP_RECENT_TOKENS,
    DEFAULT_MAX_INPUT_TOKENS, MIN_RESERVE_OUTPUT_TOKENS,
};
pub use compactor::{
    assemble_compaction_prompt, assemble_compaction_prompt_with_limit,
    extract_existing_compaction_summary, is_function_response_message,
    is_valid_canonical_cut_point, plan_canonical_compaction,
    plan_canonical_compaction_with_prompt_limit, rebuild_canonical_history,
    truncate_head_tail_chars, truncate_head_tail_within_chars, CompactionPlan,
    COMPACTION_SUMMARY_CLOSE, COMPACTION_SUMMARY_OPEN, MAX_COMPACTION_INLINE_IMAGES,
    MAX_COMPACTION_INSTRUCTIONS_CHARS, MAX_COMPACTION_MSG_CHARS, MAX_COMPACTION_PROMPT_CHARS,
    MAX_COMPACTION_TOOL_CHARS,
};

use crate::common::BoxError;
use async_trait::async_trait;
use serde_json::Value;

/// Dependency Inversion untuk meringkas konteks tanpa membuat `context/` bergantung pada `llm/`.
#[async_trait]
pub trait ContextSummarizer: Send + Sync {
    async fn summarize(
        &self,
        compaction_prompt: &str,
        cancel_rx: tokio::sync::watch::Receiver<bool>,
    ) -> Result<String, BoxError>;

    /// Varian multimodal yang meneruskan lampiran `inlineData` dari pesan yang diringkas
    /// agar detail visual ikut ditangkap dalam ringkasan konteks.
    async fn summarize_with_images(
        &self,
        compaction_prompt: &str,
        inline_images: &[Value],
        cancel_rx: tokio::sync::watch::Receiver<bool>,
    ) -> Result<String, BoxError> {
        let _ = inline_images;
        self.summarize(compaction_prompt, cancel_rx).await
    }
}

/// Adapter persistensi konteks per interface (mis. `tabs.json` atomik di CLI atau in-memory di tes).
pub trait ContextStore: Send + Sync {
    fn save_serialized(&self, content: &str) -> Result<(), String>;
}

/// Hasil penyiapan konteks sebelum/selama pemanggilan LLM.
#[derive(Debug, Clone)]
pub struct PreparedContext {
    pub history: Vec<Value>,
    pub budget: BudgetSnapshot,
    pub compacted: bool,
    pub summary: Option<String>,
    pub summarized_messages: usize,
    pub kept_messages: usize,
}

/// Orkestrator siklus hidup konteks (evaluasi token budget + compaction terkoordinasi).
#[derive(Debug, Clone)]
pub struct ContextManager {
    policy: ContextPolicy,
}

impl ContextManager {
    pub fn new(policy: ContextPolicy) -> Self {
        Self { policy }
    }

    pub fn policy(&self) -> &ContextPolicy {
        &self.policy
    }

    pub fn evaluate(
        &self,
        system_instruction: &str,
        tool_declarations: &[Value],
        history: &[Value],
        last_observed_input_tokens: Option<u64>,
    ) -> BudgetSnapshot {
        evaluate_budget(
            &self.policy,
            system_instruction,
            tool_declarations,
            history,
            last_observed_input_tokens,
        )
    }

    /// Evaluasi budget; jika `NeedsCompaction` / `OverLimit` atau `force == true`,
    /// jalankan `plan_canonical_compaction_with_prompt_limit` → `summarizer.summarize_with_images()` → `rebuild_canonical_history()`.
    #[allow(clippy::too_many_arguments)]
    pub async fn prepare_or_compact(
        &self,
        system_instruction: &str,
        tool_declarations: &[Value],
        history: &[Value],
        last_observed_input_tokens: Option<u64>,
        force: bool,
        custom_instructions: Option<&str>,
        summarizer: &dyn ContextSummarizer,
        cancel_rx: tokio::sync::watch::Receiver<bool>,
    ) -> Result<PreparedContext, BoxError> {
        let initial_budget = self.evaluate(
            system_instruction,
            tool_declarations,
            history,
            last_observed_input_tokens,
        );

        let should_compact = force
            || matches!(
                initial_budget.status,
                BudgetStatus::NeedsCompaction | BudgetStatus::OverLimit
            );

        if !should_compact {
            return Ok(PreparedContext {
                history: history.to_vec(),
                budget: initial_budget,
                compacted: false,
                summary: None,
                summarized_messages: 0,
                kept_messages: history.len(),
            });
        }

        let Some(plan) = plan_canonical_compaction_with_prompt_limit(
            history,
            self.policy.keep_recent_tokens,
            None,
            custom_instructions,
            self.policy.max_compaction_prompt_chars(),
        ) else {
            return Ok(PreparedContext {
                history: history.to_vec(),
                budget: initial_budget,
                compacted: false,
                summary: None,
                summarized_messages: 0,
                kept_messages: history.len(),
            });
        };

        // Pada auto-compaction (`!force`), bila ekor yang dipertahankan (`kept_tail`) saja sudah
        // melampaui `usable_limit`, meringkas histori lama tidak dapat menyelesaikan overflow;
        // lewati panggilan summarizer agar tidak membuang satu request LLM sia-sia.
        if !force {
            let tail_budget = self.evaluate(
                system_instruction,
                tool_declarations,
                &history[plan.cut_index..],
                None,
            );
            if tail_budget.status == BudgetStatus::OverLimit {
                return Ok(PreparedContext {
                    history: history.to_vec(),
                    budget: initial_budget,
                    compacted: false,
                    summary: None,
                    summarized_messages: 0,
                    kept_messages: history.len(),
                });
            }
        }

        let summary = summarizer
            .summarize_with_images(&plan.prompt, &plan.inline_images, cancel_rx)
            .await?;
        let rebuilt = rebuild_canonical_history(&summary, &history[plan.cut_index..]);
        let post_budget = self.evaluate(system_instruction, tool_declarations, &rebuilt, None);

        Ok(PreparedContext {
            history: rebuilt,
            budget: post_budget,
            compacted: true,
            summary: Some(summary),
            summarized_messages: plan.summarized_messages,
            kept_messages: plan.kept_messages,
        })
    }
}
