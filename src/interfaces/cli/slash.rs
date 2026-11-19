//! Handler slash command interaktif (`/scan`, `/token`, `/models`, `/model`, `/thinking`, `/tabs`, `/new`, `/tab`, `/close`, `/steps`).

use super::ui::{render_models, render_tabs, render_timeline_steps};
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
        _ => false,
    }
}
