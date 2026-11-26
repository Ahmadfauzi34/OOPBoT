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
                    println!(
                        "✅ Token disimpan ke ~/.config/oopbot/.env & {} model berhasil di-scan!",
                        models.len()
                    );
                    render_models(agent);
                }
                Err(e) => eprintln!("⚠️  Token disimpan, namun auto-scan gagal: {}\n", e),
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
                Ok(prep) if prep.compacted => {
                    warn_if_persist_failed(agent);
                    println!(
                        "✅ Konteks dipadatkan ({} pesan diringkas, {} dipertahankan). Estimasi baru: ~{} / {} token.",
                        prep.summarized_messages,
                        prep.kept_messages,
                        prep.budget.total_estimated_tokens,
                        prep.budget.usable_limit
                    );
                    if prep.budget.status == crate::context::BudgetStatus::OverLimit {
                        eprintln!(
                            "⚠️  Peringatan: konteks masih melampaui batas model (~{} / {} token usable). Giliran berikutnya dapat diblokir; gunakan `reset` atau perpendek pesan/lampiran.",
                            prep.budget.effective_tokens(),
                            prep.budget.usable_limit
                        );
                    }
                    println!();
                }
                Ok(_) => {
                    println!("ℹ️  Tidak ada pesan baru yang cukup untuk dipadatkan (gunakan `/compact <instruksi>` untuk memadatkan ulang ringkasan).\n");
                }
                Err(e) => eprintln!("❌ Gagal memadatkan konteks: {}\n", e),
            }
            true
        }
        _ => false,
    }
}
