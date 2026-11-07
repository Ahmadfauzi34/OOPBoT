mod agent;
mod gemini;
mod tools;

use agent::{Agent, CliApprover, GeminiAgent};
use gemini::{BoxError, GeminiClient};
use std::env;
use std::io::{self, Write};
use std::path::PathBuf;
use tools::ToolRegistry;

const DEFAULT_MODEL: &str = "gemini-3.5-flash-lite";
const DEFAULT_MAX_STEPS: usize = 25;

struct Cli {
    model: Option<String>,
    auto_approve: bool,
    max_steps: usize,
}

fn print_help() {
    println!(
        "oopbot {ver} — agent harness Gemini untuk Termux

PEMAKAIAN:
    oopbot [OPSI]

OPSI:
    -m, --model <NAMA>     Model Gemini (default: $GEMINI_MODEL atau {model})
    -y, --yes              Setujui otomatis semua tool (HATI-HATI)
        --max-steps <N>    Batas langkah tool per pesan (default {steps})
    -h, --help             Tampilkan bantuan
    -V, --version          Tampilkan versi

KONFIGURASI (.env di direktori kerja atau ~/.config/oopbot/.env):
    GEMINI_API_KEY=...     (wajib)
    GEMINI_MODEL=...       (opsional)

PERINTAH DI DALAM CHAT:
    keluar / exit          Keluar
    reset                  Hapus memori percakapan
    tools                  Lihat daftar tool",
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
/// Variabel yang sudah ada di environment tidak ditimpa.
fn load_env() {
    dotenvy::dotenv().ok();
    if let Ok(home) = env::var("HOME") {
        let path = PathBuf::from(home).join(".config/oopbot/.env");
        dotenvy::from_path(path).ok();
    }
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
        .or_else(|| env::var("GEMINI_MODEL").ok().filter(|m| !m.trim().is_empty()))
        .unwrap_or_else(|| DEFAULT_MODEL.to_string());

    let agent = GeminiAgent::new(
        "OOPBoT",
        GeminiClient::new(api_key, model)?,
        ToolRegistry::with_defaults(),
        Box::new(CliApprover::new(cli.auto_approve)),
        cli.max_steps,
    );

    println!("==================================================");
    println!("🤖 {} v{} | model: {}", agent.get_name(), env!("CARGO_PKG_VERSION"), agent.model());
    println!("🔧 Tools: {}", agent.tool_names().join(", "));
    if cli.auto_approve {
        println!("⚠️  Mode --yes: semua tool dijalankan tanpa konfirmasi!");
    }
    println!("Ketik 'keluar' untuk berhenti, 'reset' untuk hapus memori.");
    println!("==================================================\n");

    loop {
        print!("👤 Anda: ");
        io::stdout().flush()?;

        let mut input = String::new();
        if io::stdin().read_line(&mut input)? == 0 {
            // EOF (Ctrl+D)
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
                println!("🔧 {}", agent.tool_names().join(", "));
                continue;
            }
            _ => {}
        }

        println!("🤖 {} sedang berpikir...", agent.get_name());
        match agent.chat(input).await {
            Ok(reply) => {
                println!("\n🤖 {}:\n{}\n", agent.get_name(), reply);
                println!("--------------------------------------------------");
            }
            Err(err) => {
                // Tampilkan rantai penyebab (DNS/TLS/koneksi) agar mudah didiagnosis.
                let mut msg = err.to_string();
                let mut src = err.source();
                while let Some(s) = src {
                    msg.push_str(&format!("\n   ↳ {}", s));
                    src = s.source();
                }
                eprintln!("\n❌ Terjadi error: {}\n", msg);
            }
        }
    }

    Ok(())
}
