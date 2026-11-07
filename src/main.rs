use oopbot::agent::{Agent, CliApprover, GeminiAgent};
use oopbot::events::{StepRecord, TimelineEntry};
use oopbot::gemini::{BoxError, GeminiClient};
use oopbot::tools::ToolRegistry;
use std::env;
use std::io::{self, Write};
use std::path::PathBuf;

const DEFAULT_MODEL: &str = "gemini-3.5-flash-lite";
const DEFAULT_MAX_STEPS: usize = 25;

struct Cli {
    model: Option<String>,
    auto_approve: bool,
    max_steps: usize,
    prompt: Option<String>,
}

fn print_help() {
    println!(
        "oopbot {ver} — hybrid agent harness Gemini (Termux CLI + liboopbot.so)

PEMAKAIAN:
    oopbot [OPSI]

OPSI:
    -m, --model <NAMA>     Model Gemini (default: $GEMINI_MODEL atau {model})
    -p, --prompt <TEKS>    Jalankan 1 prompt non-interaktif lalu keluar
    -y, --yes              Setujui otomatis semua tool (HATI-HATI)
        --max-steps <N>    Batas langkah tool per pesan (default {steps})
    -h, --help             Tampilkan bantuan
    -V, --version          Tampilkan versi

KONFIGURASI (.env di direktori kerja atau ~/.config/oopbot/.env):
    GEMINI_API_KEY=...     (wajib)
    GEMINI_MODEL=...       (opsional)

PERINTAH DI DALAM CHAT (ALA PIPKIN):
    /models                Lihat daftar model & model aktif
    /model <NO|ID>         Ganti model untuk tab aktif (misal: /model 2)
    /tabs                  Lihat semua tab sesi percakapan
    /new [MODEL]           Buat tab baru (opsional langsung pilih model)
    /tab <NO|ID>           Pindah ke tab tertentu
    /close <NO|ID>         Tutup tab tertentu
    /steps                 Tampilkan lipatan proses kerja (StepsFold) di tab aktif
    tools                  Lihat daftar tool
    reset                  Hapus memori tab aktif
    keluar / exit          Keluar",
        ver = env!("CARGO_PKG_VERSION"),
        model = DEFAULT_MODEL,
        steps = DEFAULT_MAX_STEPS,
    );
}

fn parse_args() -> Result<Cli, String> {
    let mut cli = Cli {
        model: None,
        auto_approve: false,
        max_steps: DEFAULT_MAX_STEPS,
        prompt: None,
    };
    let mut args = env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "-h" | "--help" => {
                print_help();
                std::process::exit(0);
            }
            "-V" | "--version" => {
                println!("oopbot {}", env!("CARGO_PKG_VERSION"));
                std::process::exit(0);
            }
            "-y" | "--yes" => cli.auto_approve = true,
            "-m" | "--model" => {
                cli.model = Some(args.next().ok_or("--model butuh nilai")?);
            }
            "-p" | "--prompt" => {
                cli.prompt = Some(args.next().ok_or("--prompt butuh teks")?);
            }
            "--max-steps" => {
                cli.max_steps = args
                    .next()
                    .and_then(|v| v.parse().ok())
                    .filter(|n: &usize| *n > 0)
                    .ok_or("--max-steps butuh angka > 0")?;
            }
            other => return Err(format!("opsi tidak dikenal: {} (lihat --help)", other)),
        }
    }
    Ok(cli)
}

/// Muat .env dari direktori kerja, lalu ~/.config/oopbot/.env.
fn load_env() {
    dotenvy::dotenv().ok();
    if let Ok(home) = env::var("HOME") {
        let path = PathBuf::from(home).join(".config/oopbot/.env");
        dotenvy::from_path(path).ok();
    }
}

fn render_models(agent: &GeminiAgent) {
    let active = agent.model();
    println!("\n📦 Daftar Model (pilih dengan `/model <nomor|id>`):");
    for (idx, m) in agent.list_models().iter().enumerate() {
        let mark = if m.id == active { "●" } else { " " };
        println!(
            "  {} [{}] {} ({}) — {}",
            mark,
            idx + 1,
            m.id,
            m.label,
            m.description
        );
    }
    println!();
}

fn render_tabs(agent: &GeminiAgent) {
    let (active_idx, tabs) = agent.list_tabs();
    println!("\n🗂️  Daftar Tab Sesi (`/new`, `/tab <no>`, `/close <no>`):");
    for (idx, t) in tabs.iter().enumerate() {
        let mark = if idx == active_idx { "●" } else { " " };
        println!(
            "  {} [{}] #{} \"{}\" | model: {} | {} item timeline",
            mark,
            idx + 1,
            t.id,
            t.title,
            t.model,
            t.timeline.len()
        );
    }
    println!();
}

fn render_timeline_steps(agent: &GeminiAgent) {
    let entries = agent.active_timeline();
    if entries.is_empty() {
        println!("\n📭 Belum ada langkah di tab ini.\n");
        return;
    }
    println!("\n📜 Timeline Tab Aktif:");
    for entry in entries {
        match entry {
            TimelineEntry::User { text } => println!("  👤 User: {}", text),
            TimelineEntry::StepsFold { outcome, steps } => {
                println!("  ▸ Steps ({:?}, {} langkah):", outcome, steps.len());
                for s in steps {
                    match s {
                        StepRecord::Thinking { text } => println!("      💭 {}", text),
                        StepRecord::ToolCall {
                            tool_name,
                            preview,
                            status,
                            duration_ms,
                            ..
                        } => println!(
                            "      🔧 {} ({}) → {:?} [{} ms]",
                            tool_name, preview, status, duration_ms
                        ),
                    }
                }
            }
            TimelineEntry::Assistant { text } => {
                let first_line = text.lines().next().unwrap_or("");
                println!("  🤖 Asisten: {}", first_line);
            }
        }
    }
    println!();
}

/// Tangani slash command interaktif; kembalikan `true` jika input sudah ditangani.
fn handle_slash_command(agent: &GeminiAgent, input: &str) -> bool {
    let mut parts = input.splitn(2, char::is_whitespace);
    let cmd = parts.next().unwrap_or("");
    let arg = parts.next().map(str::trim).unwrap_or("");

    match cmd {
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
                println!("🗑️  Tab ditutup. Sekarang di Tab #{} \"{}\" ({})\n", id, title, model);
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

fn print_error_chain(err: &dyn std::error::Error) {
    let mut msg = err.to_string();
    let mut src = err.source();
    while let Some(s) = src {
        msg.push_str(&format!("\n   ↳ {}", s));
        src = s.source();
    }
    eprintln!("\n❌ Terjadi error: {}\n", msg);
}

#[tokio::main]
async fn main() -> Result<(), BoxError> {
    let cli = match parse_args() {
        Ok(c) => c,
        Err(e) => {
            eprintln!("❌ {}", e);
            std::process::exit(2);
        }
    };
    load_env();

    let api_key = match env::var("GEMINI_API_KEY") {
        Ok(k) if !k.trim().is_empty() => k.trim().to_string(),
        _ => {
            eprintln!(
                "❌ GEMINI_API_KEY tidak ditemukan.\n   Buat file ~/.config/oopbot/.env berisi:\n   GEMINI_API_KEY=xxxxx"
            );
            std::process::exit(1);
        }
    };
    let model = cli
        .model
        .clone()
        .or_else(|| env::var("GEMINI_MODEL").ok().filter(|m| !m.trim().is_empty()))
        .unwrap_or_else(|| DEFAULT_MODEL.to_string());

    let agent = GeminiAgent::new(
        "OOPBoT",
        GeminiClient::new(api_key, model.clone())?,
        ToolRegistry::with_defaults(),
        Box::new(CliApprover::new(cli.auto_approve)),
        cli.max_steps,
    );
    if cli.model.is_some() {
        agent.set_active_model(&model);
    }

    // Mode satu kali jalan (-p "prompt")
    if let Some(prompt) = cli.prompt {
        match agent.chat(&prompt).await {
            Ok(reply) => {
                println!("\n{}", reply);
                return Ok(());
            }
            Err(err) => {
                print_error_chain(err.as_ref());
                std::process::exit(1);
            }
        }
    }

    println!("==================================================");
    println!(
        "🤖 {} v{} | Tools: {}",
        agent.get_name(),
        env!("CARGO_PKG_VERSION"),
        agent.tool_names().join(", ")
    );
    println!("💡 Perintah: /models, /model <id>, /tabs, /new, /tab <no>, /steps, reset, keluar");
    if cli.auto_approve {
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
                continue;
            }
            "tools" => {
                println!("🔧 {}\n", agent.tool_names().join(", "));
                continue;
            }
            _ => {}
        }

        if input.starts_with('/') && handle_slash_command(&agent, input) {
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
