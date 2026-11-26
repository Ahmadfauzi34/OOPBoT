//! Adapter antarmuka CLI untuk binary `oopbot`:
//! parsing argumen, pembacaan `.env`, resolusi kredensial, mode `--mode scan-models`,
//! eksekusi single prompt (`-p`), dan REPL interaktif.

pub mod credentials;
pub mod repl;
pub mod slash;
pub mod ui;

pub use ui::{CliApprover, CliEventSink};

use crate::agent::{Agent, AgentHarness};
use crate::common::BoxError;
use crate::llm::LlmClient;
use crate::plugins::PluginRegistry;
use crate::tools::ToolRegistry;
use credentials::{print_missing_credentials_help, resolve_cli_credentials};
use serde_json::json;
use std::env;
use std::path::PathBuf;
use ui::print_error_chain;

pub const DEFAULT_MAX_STEPS: usize = 25;

pub struct CliArgs {
    pub model: Option<String>,
    pub auto_approve: bool,
    pub max_steps: usize,
    pub prompt: Option<String>,
    pub mode: Option<String>,
    pub session: Option<String>,
    pub system_append: Option<String>,
    pub extensions: Vec<String>,
    pub host_mode: Option<String>,
    pub host_port: Option<u16>,
}

pub fn print_help() {
    println!(
        "oopbot {ver} — Universal Agent Harness for Termux & Android (Multi-Provider)

PEMAKAIAN:
    oopbot [OPSI]

OPSI:
    -m, --model <NAMA>     Model AI (default sesuai provider aktif atau $OOPBOT_MODEL)
    -p, --prompt <TEKS>    Jalankan 1 prompt non-interaktif lalu keluar
        --mode <MODE>      Mode mesin:
                           - `rpc`         : daemon JSONL untuk aplikasi Android OOPBoT
                           - `catalog`     : cetak katalog referensi provider & model JSON
                           - `scan-models` : scan daftar model live dari provider & cetak JSON
        --session <PATH>   Path file sesi JSONL (untuk `--mode rpc`)
    -y, --yes              Setujui otomatis semua tool (HATI-HATI)
        --max-steps <N>    Batas langkah tool per pesan (default {steps})
    -h, --help             Tampilkan bantuan
    -V, --version          Tampilkan versi

PROVIDER & KONFIGURASI (.env di direktori kerja atau ~/.config/oopbot/.env):
    Mendukung berbagai provider AI (pilih salah satu):
    - OpenRouter   : OPENROUTER_API_KEY=sk-or-v1-...
    - DeepSeek     : DEEPSEEK_API_KEY=sk-...
    - Google Gemini: GEMINI_API_KEY=...
    - OpenAI       : OPENAI_API_KEY=sk-...
    - Anthropic    : ANTHROPIC_API_KEY=sk-ant-...
    - Kustom/Lokal : OOPBOT_API_KEY=... & OOPBOT_BASE_URL=http://localhost:11434/v1
    - Model Opsi   : OOPBOT_MODEL=...

PERINTAH DI DALAM CHAT:
    /scan                  Scan otomatis daftar model dari provider aktif
    /token <KEY> [URL]     Ganti API key (& opsional base URL) + auto-scan model
    /models                Lihat daftar model & model aktif
    /model <NO|ID>         Ganti model untuk tab aktif (misal: /model 2)
    /thinking <LEVEL>      Atur level penalaran (off|minimal|low|medium|high|xhigh)
    /tabs                  Lihat semua tab sesi percakapan
    /new [MODEL]           Buat tab baru (opsional langsung pilih model)
    /tab <NO|ID>           Pindah ke tab tertentu
    /close <NO|ID>         Tutup tab tertentu
    /steps                 Tampilkan lipatan proses kerja (StepsFold) di tab aktif
    /context               Lihat estimasi penggunaan token & anggaran konteks tab aktif
    /compact [INSTRUKSI]   Padatkan konteks tab aktif secara manual (opsional instruksi)
    tools                  Lihat daftar tool
    reset                  Hapus memori tab aktif
    keluar / exit          Keluar",
        ver = env!("CARGO_PKG_VERSION"),
        steps = DEFAULT_MAX_STEPS,
    );
}

pub fn parse_args() -> Result<CliArgs, String> {
    let mut cli = CliArgs {
        model: None,
        auto_approve: false,
        max_steps: DEFAULT_MAX_STEPS,
        prompt: None,
        mode: None,
        session: None,
        system_append: None,
        extensions: Vec::new(),
        host_mode: None,
        host_port: None,
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
            "host" => {
                cli.mode = Some("host".to_string());
                cli.host_mode = args.next();
                if let Some(port_str) = args.next() {
                    cli.host_port = port_str.parse().ok();
                }
            }
            "--port" => {
                let port_str = args.next().ok_or("--port butuh nilai angka")?;
                cli.host_port = port_str.parse().ok();
            }
            "--mode" => {
                cli.mode = Some(args.next().ok_or("--mode butuh nilai")?);
            }
            "--session" => {
                cli.session = Some(args.next().ok_or("--session butuh path")?);
            }
            "--extension" | "--plugin" => {
                let ext = args.next().ok_or("--extension/--plugin butuh nilai")?;
                cli.extensions.push(ext);
            }
            "--append-system-prompt" => {
                cli.system_append = Some(args.next().ok_or("--append-system-prompt butuh teks")?);
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

/// Muat `.env` dari direktori kerja, lalu `~/.config/oopbot/.env`.
pub fn load_env() {
    dotenvy::dotenv().ok();
    if let Ok(home) = env::var("HOME") {
        let path = PathBuf::from(home).join(".config/oopbot/.env");
        dotenvy::from_path(path).ok();
    }
}

/// Mode `--mode scan-models`: scan daftar model live dari provider & cetak JSON ke stdout.
pub async fn run_scan_models() -> Result<(), BoxError> {
    let (api_key, base_url) = if let Some(k) = env::var("OOPBOT_SCAN_API_KEY")
        .ok()
        .filter(|s| !s.trim().is_empty())
    {
        (k.trim().to_string(), env::var("OOPBOT_SCAN_BASE_URL").ok())
    } else if let Some(c) = resolve_cli_credentials(None) {
        (c.api_key, c.base_url)
    } else {
        (String::new(), None)
    };
    let client = LlmClient::new_with_base_url(api_key, "default".to_string(), base_url)?;
    match client.scan_models().await {
        Ok(scanned) => {
            let models: Vec<_> = scanned.iter().map(|m| m.to_oopbot_json(None)).collect();
            println!("{}", json!({ "ok": true, "models": models }));
        }
        Err(e) => {
            println!("{}", json!({ "ok": false, "error": e.to_string() }));
        }
    }
    Ok(())
}

/// Gabungkan guideline plugin aktif + `--append-system-prompt` ke system
/// instruction CLI — cerminan persis `RpcRuntime::new_in_dir_with_cwd`
/// (`interfaces/rpc/runtime.rs:286-293`): plugin dulu, lalu append user.
/// Tanpa extension dan tanpa flag, hasilnya byte-identik dengan base.
fn append_cli_system_instruction(
    mut base: String,
    plugins: &PluginRegistry,
    append: Option<&str>,
) -> String {
    let plugin_append = plugins.system_prompt_append();
    if !plugin_append.is_empty() {
        base.push_str("\n\n");
        base.push_str(&plugin_append);
    }
    if let Some(text) = append.filter(|s| !s.trim().is_empty()) {
        base.push_str("\n\n");
        base.push_str(text);
    }
    base
}

/// Jalankan mode CLI interaktif (REPL) atau mode satu kali jalan (`-p <PROMPT>`).
pub async fn run_interactive_or_prompt(cli: CliArgs) -> Result<(), BoxError> {
    let creds = match resolve_cli_credentials(cli.model.as_deref()) {
        Some(c) => c,
        None => {
            print_missing_credentials_help();
            std::process::exit(1);
        }
    };

    let mut tools = ToolRegistry::with_defaults();
    let mut plugins = PluginRegistry::with_defaults();
    for ext in &cli.extensions {
        plugins.activate(ext);
    }
    plugins.register_active_tools(&mut tools);

    let client = LlmClient::new_with_base_url(creds.api_key, creds.model.clone(), creds.base_url)?;
    let mut agent = AgentHarness::with_event_sink(
        "OOPBoT",
        client,
        tools,
        Box::new(CliApprover::new(cli.auto_approve)),
        Box::new(CliEventSink),
        cli.max_steps,
    );
    ui::warn_if_persist_failed(&agent);
    agent.set_system_instruction(append_cli_system_instruction(
        agent.system_instruction().to_owned(),
        &plugins,
        cli.system_append.as_deref(),
    ));
    agent.set_auto_compact(true);
    let active_model = if cli.model.is_some() {
        agent.set_active_model(&creds.model)
    } else {
        let (model, mismatch_warn) = agent.sync_client_to_active_tab_model_or_fallback(&creds.model);
        if let Some(warn) = mismatch_warn {
            eprintln!("⚠️  {}\n", warn);
        }
        model
    };
    ui::warn_if_persist_failed(&agent);

    // Mode satu kali jalan (-p "prompt")
    if let Some(prompt) = cli.prompt {
        match agent.chat(&prompt).await {
            Ok(reply) => {
                ui::warn_if_persist_failed(&agent);
                println!("\n{}", reply);
                return Ok(());
            }
            Err(err) => {
                ui::warn_if_persist_failed(&agent);
                print_error_chain(err.as_ref());
                std::process::exit(1);
            }
        }
    }

    repl::run_repl(&agent, &creds.provider_name, &active_model, cli.auto_approve).await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cli_system_instruction_tanpa_plugin_dan_flag_tidak_berubah() {
        let plugins = PluginRegistry::with_defaults();
        assert_eq!(
            append_cli_system_instruction("BASE".into(), &plugins, None),
            "BASE"
        );
        assert_eq!(
            append_cli_system_instruction("BASE".into(), &plugins, Some("   ")),
            "BASE"
        );
    }

    #[test]
    fn cli_system_instruction_memuat_guideline_plugin_aktif() {
        let mut plugins = PluginRegistry::with_defaults();
        plugins.activate("oopbot-todo");
        plugins.activate("oopbot-web-access");
        let out = append_cli_system_instruction("BASE".into(), &plugins, None);
        assert!(out.starts_with("BASE"));
        assert!(out.contains("Todo & Task Management Guidelines"));
        assert!(out.contains("Web Access Guidelines"));
    }

    #[test]
    fn cli_system_instruction_append_user_setelah_plugin() {
        let mut plugins = PluginRegistry::with_defaults();
        plugins.activate("oopbot-todo");
        let out = append_cli_system_instruction(
            "BASE".into(),
            &plugins,
            Some("Jawab selalu dalam Bahasa Jawa."),
        );
        let guideline = out.find("Todo & Task Management Guidelines").unwrap();
        let user = out.find("Jawab selalu dalam Bahasa Jawa.").unwrap();
        assert!(guideline < user);
    }
}
