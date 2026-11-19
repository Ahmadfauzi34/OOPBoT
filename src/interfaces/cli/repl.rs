//! Loop REPL interaktif di terminal CLI.

use super::slash::handle_slash_command;
use super::ui::print_error_chain;
use crate::agent::{Agent, AgentHarness};
use crate::common::BoxError;
use std::io::{self, Write};

pub async fn run_repl(
    agent: &AgentHarness,
    provider_name: &str,
    model: &str,
    auto_approve: bool,
) -> Result<(), BoxError> {
    println!("==================================================");
    println!(
        "🤖 {} v{} | Provider: {} | Model: {} | Tools: {}",
        agent.get_name(),
        env!("CARGO_PKG_VERSION"),
        provider_name,
        model,
        agent.tool_names().join(", ")
    );
    println!("💡 Perintah: /scan, /token <key>, /models, /model <id>, /tabs, /new, /tab <no>, /steps, reset, keluar");
    if auto_approve {
        println!("⚠️  Mode --yes: semua tool dijalankan tanpa konfirmasi!");
    }
    println!("==================================================\n");

    loop {
        let (tab_id, _tab_title, tab_model) = agent.active_tab_summary();
        print!("👤 [Tab #{} · {}] Anda: ", tab_id, tab_model);
        io::stdout().flush()?;

        let mut input = String::new();
        if io::stdin().read_line(&mut input)? == 0 {
            println!("\n👋 Sampai jumpa!");
            break;
        }
        let input = input.trim();

        match input.to_lowercase().as_str() {
            "" => continue,
            "keluar" | "exit" | "quit" => {
                println!("👋 Sampai jumpa!");
                break;
            }
            "reset" => {
                agent.clear_memory();
                println!(
                    "🧠 Memori Tab #{} ({}) telah dibersihkan.",
                    tab_id,
                    agent.get_name()
                );
                continue;
            }
            "tools" => {
                println!("🔧 {}\n", agent.tool_names().join(", "));
                continue;
            }
            _ => {}
        }

        if input.starts_with('/') && handle_slash_command(agent, input).await {
            continue;
        }

        match agent.chat(input).await {
            Ok(reply) => {
                println!("\n🤖 {}:\n{}\n", agent.get_name(), reply);
                println!("--------------------------------------------------");
            }
            Err(err) => print_error_chain(err.as_ref()),
        }
    }

    Ok(())
}
