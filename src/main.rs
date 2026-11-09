use oopbot::agent::{Agent, CliApprover, GeminiAgent};
use oopbot::events::{StepRecord, TimelineEntry};
use oopbot::gemini::{BoxError, GeminiClient};
use oopbot::models::builtin_catalog_json;
use oopbot::tools::ToolRegistry;
use serde_json::json;
use std::env;
use std::io::{self, Write};
use std::path::PathBuf;

const DEFAULT_MAX_STEPS: usize = 25;


struct Cli {
    model: Option<String>,
    auto_approve: bool,
    max_steps: usize,
    prompt: Option<String>,
    mode: Option<String>,
    session: Option<String>,
    system_append: Option<String>,
    extensions: Vec<String>,
    host_mode: Option<String>,
    host_port: Option<u16>,
}

fn print_help() {
    println!(
        "oopbot {ver} — Universal Agent Harness for Termux & Android (Multi-Provider)

PEMAKAIAN:
    oopbot [OPSI]

OPSI:
    -m, --model <NAMA>     Model AI (default sesuai provider aktif atau $OOPBOT_MODEL)
    -p, --prompt <TEKS>    Jalankan 1 prompt non-interaktif lalu keluar
        --mode <MODE>      Mode mesin:
                           - `rpc`         : daemon JSONL untuk aplikasi Android Pipkin
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
    tools                  Lihat daftar tool
    reset                  Hapus memori tab aktif
    keluar / exit          Keluar",
        ver = env!("CARGO_PKG_VERSION"),
        steps = DEFAULT_MAX_STEPS,
    );
}


fn parse_args() -> Result<Cli, String> {
    let mut cli = Cli {
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
    println!("\n📦 Daftar Model (pilih dengan `/model <nomor|id>`, atau `/scan` untuk refresh):");
    for (idx, m) in agent.list_models().iter().enumerate() {
        let mark = if m.id == active { "●" } else { " " };
        let ctx = m
            .input_token_limit
            .map(|n| format!(" [ctx: {}k]", n / 1000))
            .unwrap_or_default();
        println!(
            "  {} [{:>2}] {}{} ({})",
            mark,
            idx + 1,
            m.id,
            ctx,
            m.label
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
async fn handle_slash_command(agent: &GeminiAgent, input: &str) -> bool {
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
                Err(e) => eprintln!(
                    "⚠️  Token disimpan, namun auto-scan gagal: {}\n",
                    e
                ),
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

struct CliCredentials {
    provider_name: String,
    api_key: String,
    base_url: Option<String>,
    model: String,
}

fn resolve_cli_credentials(cli_model: Option<&str>) -> Option<CliCredentials> {
    // 1. Cek OOPBOT_API_KEY (universal / kustom / lokal)
    if let Ok(key) = env::var("OOPBOT_API_KEY").map(|s| s.trim().to_string()) {
        if !key.is_empty() {
            let base_url = env::var("OOPBOT_BASE_URL").ok().filter(|s| !s.trim().is_empty());
            let model = cli_model
                .map(|s| s.to_string())
                .or_else(|| env::var("OOPBOT_MODEL").ok().filter(|s| !s.trim().is_empty()))
                .unwrap_or_else(|| {
                    if key.starts_with("sk-or-") {
                        "deepseek/deepseek-chat".to_string()
                    } else {
                        "default".to_string()
                    }
                });
            return Some(CliCredentials {
                provider_name: "OOPBoT / Kustom".to_string(),
                api_key: key,
                base_url,
                model,
            });
        }
    }

    // 2. Cek OPENROUTER_API_KEY
    if let Ok(key) = env::var("OPENROUTER_API_KEY").map(|s| s.trim().to_string()) {
        if !key.is_empty() {
            let base_url = env::var("OPENROUTER_BASE_URL")
                .ok()
                .filter(|s| !s.trim().is_empty())
                .unwrap_or_else(|| "https://openrouter.ai/api/v1".to_string());
            let model = cli_model
                .map(|s| s.to_string())
                .or_else(|| {
                    env::var("OPENROUTER_MODEL")
                        .or_else(|_| env::var("OOPBOT_MODEL"))
                        .ok()
                        .filter(|s| !s.trim().is_empty())
                })
                .unwrap_or_else(|| "deepseek/deepseek-chat".to_string());
            return Some(CliCredentials {
                provider_name: "OpenRouter".to_string(),
                api_key: key,
                base_url: Some(base_url),
                model,
            });
        }
    }

    // 3. Cek DEEPSEEK_API_KEY
    if let Ok(key) = env::var("DEEPSEEK_API_KEY").map(|s| s.trim().to_string()) {
        if !key.is_empty() {
            let base_url = env::var("DEEPSEEK_BASE_URL")
                .ok()
                .filter(|s| !s.trim().is_empty())
                .unwrap_or_else(|| "https://api.deepseek.com/v1".to_string());
            let model = cli_model
                .map(|s| s.to_string())
                .or_else(|| {
                    env::var("DEEPSEEK_MODEL")
                        .or_else(|_| env::var("OOPBOT_MODEL"))
                        .ok()
                        .filter(|s| !s.trim().is_empty())
                })
                .unwrap_or_else(|| "deepseek-chat".to_string());
            return Some(CliCredentials {
                provider_name: "DeepSeek".to_string(),
                api_key: key,
                base_url: Some(base_url),
                model,
            });
        }
    }

    // 4. Cek OPENAI_API_KEY
    if let Ok(key) = env::var("OPENAI_API_KEY").map(|s| s.trim().to_string()) {
        if !key.is_empty() {
            let base_url = env::var("OPENAI_BASE_URL")
                .ok()
                .filter(|s| !s.trim().is_empty())
                .unwrap_or_else(|| "https://api.openai.com/v1".to_string());
            let model = cli_model
                .map(|s| s.to_string())
                .or_else(|| {
                    env::var("OPENAI_MODEL")
                        .or_else(|_| env::var("OOPBOT_MODEL"))
                        .ok()
                        .filter(|s| !s.trim().is_empty())
                })
                .unwrap_or_else(|| "gpt-4o-mini".to_string());
            return Some(CliCredentials {
                provider_name: "OpenAI".to_string(),
                api_key: key,
                base_url: Some(base_url),
                model,
            });
        }
    }

    // 5. Cek ANTHROPIC_API_KEY
    if let Ok(key) = env::var("ANTHROPIC_API_KEY").map(|s| s.trim().to_string()) {
        if !key.is_empty() {
            let base_url = env::var("ANTHROPIC_BASE_URL")
                .ok()
                .filter(|s| !s.trim().is_empty())
                .unwrap_or_else(|| "https://api.anthropic.com".to_string());
            let model = cli_model
                .map(|s| s.to_string())
                .or_else(|| {
                    env::var("ANTHROPIC_MODEL")
                        .or_else(|_| env::var("OOPBOT_MODEL"))
                        .ok()
                        .filter(|s| !s.trim().is_empty())
                })
                .unwrap_or_else(|| "claude-3-5-haiku-20241022".to_string());
            return Some(CliCredentials {
                provider_name: "Anthropic".to_string(),
                api_key: key,
                base_url: Some(base_url),
                model,
            });
        }
    }

    // 6. Cek GEMINI_API_KEY
    if let Ok(key) = env::var("GEMINI_API_KEY").map(|s| s.trim().to_string()) {
        if !key.is_empty() {
            let base_url = env::var("GEMINI_BASE_URL")
                .ok()
                .filter(|s| !s.trim().is_empty())
                .unwrap_or_else(|| "https://generativelanguage.googleapis.com/v1beta".to_string());
            let model = cli_model
                .map(|s| s.to_string())
                .or_else(|| {
                    env::var("GEMINI_MODEL")
                        .or_else(|_| env::var("OOPBOT_MODEL"))
                        .ok()
                        .filter(|s| !s.trim().is_empty())
                })
                .unwrap_or_else(|| "gemini-2.5-flash".to_string());
            return Some(CliCredentials {
                provider_name: "Google Gemini".to_string(),
                api_key: key,
                base_url: Some(base_url),
                model,
            });
        }
    }

    None
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

    match cli.mode.as_deref() {
        Some("host") => {
            let host_mode = cli.host_mode.unwrap_or_else(|| "rpc".to_string());
            let port = cli.host_port.unwrap_or(0);
            if port == 0 {
                eprintln!("❌ Port host tidak valid");
                std::process::exit(1);
            }
            return oopbot::host::run_host_mode(&host_mode, port).await;
        }
        Some("rpc") => {
            return oopbot::rpc::run_rpc_mode(cli.session, cli.system_append, cli.extensions).await;
        }
        Some("catalog") => {
            println!("{}", serde_json::to_string(&builtin_catalog_json())?);
            return Ok(());
        }
        Some("scan-models") => {
            let (api_key, base_url) = if let Some(k) = env::var("OOPBOT_SCAN_API_KEY").ok().filter(|s| !s.trim().is_empty()) {
                (k.trim().to_string(), env::var("OOPBOT_SCAN_BASE_URL").ok())

            } else if let Some(c) = resolve_cli_credentials(None) {
                (c.api_key, c.base_url)
            } else {
                (String::new(), None)
            };
            let client = GeminiClient::new_with_base_url(api_key.clone(), "default".to_string(), base_url)?;
            match client.scan_models().await {
                Ok(scanned) => {
                    let models: Vec<_> = scanned.iter().map(|m| m.to_pipkin_json(None)).collect();
                    println!("{}", json!({ "ok": true, "models": models }));
                }
                Err(e) => {
                    println!("{}", json!({ "ok": false, "error": e.to_string() }));
                }
            }
            return Ok(());
        }
        _ => {}
    }

    let creds = match resolve_cli_credentials(cli.model.as_deref()) {
        Some(c) => c,
        None => {
            eprintln!(
                "❌ Kunci API (API Key) belum ditemukan.\n\n\
                 OOPBoT mendukung berbagai provider AI secara mandiri:\n\
                 1. OpenRouter (Ratusan model: DeepSeek V3/R1, Claude 3.5, Llama):\n\
                    export OPENROUTER_API_KEY=\"sk-or-v1-...\"\n\n\
                 2. DeepSeek (Hemat & bertenaga untuk koding):\n\
                    export DEEPSEEK_API_KEY=\"sk-...\"\n\n\
                 3. Google Gemini:\n\
                    export GEMINI_API_KEY=\"...\"\n\n\
                 4. OpenAI:\n\
                    export OPENAI_API_KEY=\"sk-...\"\n\n\
                 5. Anthropic Claude:\n\
                    export ANTHROPIC_API_KEY=\"sk-ant-...\"\n\n\
                 6. Server Kustom / Lokal (Ollama, LMStudio, Groq, Together):\n\
                    export OOPBOT_API_KEY=\"...\"\n\
                    export OOPBOT_BASE_URL=\"http://localhost:11434/v1\"\n\
                    export OOPBOT_MODEL=\"llama3.2\"\n\n\
                 Atau simpan salah satu konfigurasi di atas ke file:\n\
                    ~/.config/oopbot/.env\n"
            );
            std::process::exit(1);
        }
    };

    let mut tools = ToolRegistry::with_defaults();
    let mut plugins = oopbot::plugins::PluginRegistry::with_defaults();
    for ext in &cli.extensions {
        plugins.activate(ext);
    }
    plugins.register_active_tools(&mut tools);

    let client = GeminiClient::new_with_base_url(creds.api_key, creds.model.clone(), creds.base_url)?;
    let agent = GeminiAgent::new(
        "OOPBoT",
        client,
        tools,
        Box::new(CliApprover::new(cli.auto_approve)),
        cli.max_steps,
    );
    agent.set_active_model(&creds.model);

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
        "🤖 {} v{} | Provider: {} | Model: {} | Tools: {}",
        agent.get_name(),
        env!("CARGO_PKG_VERSION"),
        creds.provider_name,
        creds.model,
        agent.tool_names().join(", ")
    );
    println!("💡 Perintah: /scan, /token <key>, /models, /model <id>, /tabs, /new, /tab <no>, /steps, reset, keluar");
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

        if input.starts_with('/') && handle_slash_command(&agent, input).await {
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
