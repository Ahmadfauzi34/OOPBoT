//! Handler slash command interaktif (`/scan`, `/token`, `/models`, `/model`, `/thinking`, `/tabs`, `/new`, `/tab`, `/close`, `/steps`, `/context`, `/compact`).

use super::ui::{
    render_context_budget, render_models, render_tabs, render_timeline_steps,
    warn_if_persist_failed,
};
use crate::agent::AgentHarness;

/// Tangani slash command interaktif; kembalikan `true` jika input sudah ditangani.
pub async fn handle_slash_command(agent: &AgentHarness, input: &str) -> bool {
    let mut parts = input.splitn(2, char::is_whitespace);
    let cmd = parts.next().unwrap_or("");
    let arg = parts.next().map(str::trim).unwrap_or("");

    match cmd {
        "/scan" => {
            println!("🔍 Memindai daftar model dari provider...");
            match agent.scan_models().await {
                Ok(models) => {
                    println!("✅ Ditemukan {} model!", models.len());
                    render_models(agent);
                }
                Err(e) => eprintln!("❌ Gagal scan model: {}\n", e),
            }
            true
        }
        "/token" => {
            if arg.is_empty() {
                println!("Gunakan: `/token <API_KEY> [BASE_URL]`\n");
                return true;
            }
            let mut tok_parts = arg.split_whitespace();
            let api_key = tok_parts.next().unwrap_or("");
            let base_url = tok_parts.next();
            println!("🔑 Memperbarui token & memindai daftar model...");
            match agent.update_provider_token(api_key, base_url).await {
                Ok(models) => {
                    warn_if_persist_failed(agent);
                    println!(
                        "✅ Token disimpan ke ~/.config/oopbot/.env & {} model berhasil di-scan!",
                        models.len()
                    );
                    if let Some(warn) = agent.check_active_tab_provider_mismatch() {
                        eprintln!("⚠️  {} Gunakan `/model <id>` untuk memilih model yang sesuai.", warn);
                    }
                    render_models(agent);
                }
                Err(e) => {
                    warn_if_persist_failed(agent);
                    eprintln!("⚠️  Token disimpan, namun auto-scan gagal: {}\n", e);
                    if let Some(warn) = agent.check_active_tab_provider_mismatch() {
                        eprintln!(
                            "⚠️  {} Gunakan `/model <id>` untuk memilih model yang sesuai.\n",
                            warn
                        );
                    }
                }
            }
            true
        }
        "/models" => {
            render_models(agent);
            true
        }
        "/model" => {
            if arg.is_empty() {
                render_models(agent);
            } else {
                let new_model = agent.set_active_model(arg);
                warn_if_persist_failed(agent);
                println!("✅ Model tab aktif diubah ke: {}\n", new_model);
            }
            true
        }
        "/thinking" => {
            if arg.is_empty() {
                let levels = agent.active_model_info().available_thinking_levels();
                println!(
                    "🧠 Level thinking saat ini: {} (tersedia: {})\n",
                    agent.thinking_level(),
                    levels.join(", ")
                );
            } else {
                let clamped = agent.set_thinking_level(arg);
                println!("✅ Level thinking diubah ke: {}\n", clamped);
            }
            true
        }
        "/tabs" => {
            render_tabs(agent);
            true
        }
        "/new" => {
            let model_opt = (!arg.is_empty()).then_some(arg);
            let created = agent.new_tab(model_opt);
            warn_if_persist_failed(agent);
            println!(
                "➕ Pindah ke Tab baru #{} ({}) | model: {}\n",
                created.id, created.title, created.model
            );
            true
        }
        "/tab" => {
            if arg.is_empty() {
                render_tabs(agent);
            } else if let Some(tab) = agent.switch_tab(arg) {
                warn_if_persist_failed(agent);
                println!(
                    "🔀 Aktif di Tab #{} \"{}\" | model: {}\n",
                    tab.id, tab.title, tab.model
                );
                if let Some(warn) = agent.check_active_tab_provider_mismatch() {
                    eprintln!(
                        "⚠️  {} Gunakan `/model <id>` untuk menyesuaikan model tab ini.\n",
                        warn
                    );
                }
            } else {
                println!("❌ Tab '{}' tidak ditemukan.\n", arg);
            }
            true
        }
        "/close" => {
            if arg.is_empty() {
                println!("Gunakan `/close <nomor_tab>`\n");
            } else if agent.close_tab(arg) {
                warn_if_persist_failed(agent);
                let (id, title, model) = agent.active_tab_summary();
                println!(
                    "🗑️  Tab ditutup. Sekarang di Tab #{} \"{}\" ({})\n",
                    id, title, model
                );
                if let Some(warn) = agent.check_active_tab_provider_mismatch() {
                    eprintln!(
                        "⚠️  {} Gunakan `/model <id>` untuk menyesuaikan model tab ini.\n",
                        warn
                    );
                }
            } else {
                println!("❌ Tab '{}' tidak ditemukan.\n", arg);
            }
            true
        }
        "/steps" => {
            render_timeline_steps(agent);
            true
        }
        "/context" => {
            render_context_budget(agent);
            true
        }
        "/compact" => {
            println!("🗜️  Memadatkan konteks tab aktif...");
            let instructions = (!arg.is_empty()).then_some(arg);
            let (_tx, cancel_rx) = tokio::sync::watch::channel(false);
            match agent.compact_active_tab(instructions, cancel_rx).await {
                Ok(prep) => {
                    if prep.compacted {
                        warn_if_persist_failed(agent);
                    }
                    let (stdout_msg, stderr_warn) = format_compact_outcome(&prep);
                    println!("{}", stdout_msg);
                    if let Some(warn) = stderr_warn {
                        eprintln!("{}", warn);
                    }
                    println!();
                }
                Err(e) => {
                    eprintln!("❌ Gagal memadatkan konteks: {}", e);
                    if let Some(warn) = format_over_limit_warning(&agent.context_budget()) {
                        eprintln!("{}", warn);
                    }
                    eprintln!();
                }
            }
            true
        }
        _ => false,
    }
}

fn format_over_limit_warning(budget: &crate::context::BudgetSnapshot) -> Option<String> {
    (budget.status == crate::context::BudgetStatus::OverLimit).then(|| {
        format!(
            "⚠️  Peringatan: konteks masih melampaui batas model (~{} / {} token usable). Giliran berikutnya dapat diblokir; gunakan `reset` atau perpendek pesan/lampiran.",
            budget.effective_tokens(),
            budget.usable_limit
        )
    })
}

fn format_compact_outcome(prep: &crate::context::PreparedContext) -> (String, Option<String>) {
    let stdout_msg = if prep.compacted {
        format!(
            "✅ Konteks dipadatkan ({} pesan diringkas, {} dipertahankan). Estimasi baru: ~{} / {} token.",
            prep.summarized_messages,
            prep.kept_messages,
            prep.budget.total_estimated_tokens,
            prep.budget.usable_limit
        )
    } else {
        "ℹ️  Tidak ada pesan baru yang cukup untuk dipadatkan (gunakan `/compact <instruksi>` untuk memadatkan ulang ringkasan).".to_string()
    };
    let stderr_warn = format_over_limit_warning(&prep.budget);
    (stdout_msg, stderr_warn)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::{Approver, NoopEventSink, WorkspaceTabs};
    use crate::llm::LlmClient;
    use crate::tools::ToolRegistry;
    use async_trait::async_trait;
    use serde_json::json;

    struct DenyAll;
    #[async_trait]
    impl Approver for DenyAll {
        async fn approve(&self, _tool: &str, _preview: &str) -> bool {
            false
        }
    }

    #[tokio::test]
    async fn compact_noop_synthetic_over_limit_still_emits_warning() {
        let client = LlmClient::new_with_base_url(
            "test-key".into(),
            "tiny-model".into(),
            Some("http://127.0.0.1:9/v1".into()),
        )
        .unwrap();
        let mut tabs = WorkspaceTabs::new("tiny-model");
        tabs.active_mut().history = crate::context::rebuild_canonical_history(
            &"ringkasan super panjang ".repeat(220),
            &[],
        );
        let mut agent = AgentHarness::with_event_sink_and_tabs(
            "OOPBoT",
            client,
            ToolRegistry::new(),
            Box::new(DenyAll),
            Box::new(NoopEventSink),
            2,
            tabs,
            None,
        );
        agent.set_system_instruction("sys".into());
        agent.configure_model(
            "tiny-model",
            &json!({ "api": "openai-completions", "contextWindow": 800, "maxTokens": 100 }),
        );

        let (_tx, cancel_rx) = tokio::sync::watch::channel(false);
        let prep = agent.compact_active_tab(None, cancel_rx).await.unwrap();
        assert!(!prep.compacted, "synthetic-only without instructions must be no-op");
        assert_eq!(prep.budget.status, crate::context::BudgetStatus::OverLimit);

        let (stdout_msg, stderr_warn) = format_compact_outcome(&prep);
        assert!(stdout_msg.contains("Tidak ada pesan baru"));
        let warn = stderr_warn.expect("no-op /compact that remains OverLimit MUST emit warning");
        assert!(warn.contains("melampaui batas model"));
    }
}
