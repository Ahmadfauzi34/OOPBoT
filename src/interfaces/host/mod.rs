//! Sub-command `oopbot host <mode> <port>`
//!
//! Jembatan komunikasi native Termux-ke-Android langsung via Tokio / standard TCP socket,
//! menggantikan `node host.mjs` sehingga Termux 100% bebas dari ketergantungan Node.js.
//!
//! Mendukung mode:
//! - `rpc`            : Menghubungkan sesi percakapan agent langsung ke TCP socket Android.
//! - `ensure`         : Verifikasi kesiapan engine native OOPBoT (tanpa npm / node).
//! - `hello`          : Handshake info lingkungan Termux (direktori, storage, dll).
//! - `login`          : Handshake autentikasi interaktif (input API key via prompt Android).
//! - `browse`         : Penjelajahan direktori workspace (oopbot_places, oopbot_list, oopbot_search).
//! - `files`          : File Explorer workspace (oopbot_files_list, oopbot_files_read, oopbot_files_mkdir).
//! - `git`            : Tab Git Changes (oopbot_git_detect, oopbot_git_changes, oopbot_git_commits, oopbot_git_commit_diff).
//! - `terminal`       : Terminal interaktif shell bawaan (streaming stdout/stderr Base64 & input).
//! - `utility`        : Penanganan one-off request (daftar sesi, model default, auth status, attachments, dll).
//! - `package_install`: Penanganan instalasi paket ekstensi runtime.
//! - `backup_create`  : Pembuatan snapshot backup konfigurasi.
//! - `backup_restore` : Pemulihan snapshot konfigurasi dari file backup.


mod backups;
mod browse_files;
mod engines;
mod ensure;
mod git;
mod providers;
mod remote;
mod sessions;
mod terminal;

#[cfg(test)]
mod tests;

pub use git::{parse_git_diff, sha256_hex};

use backups::*;
use browse_files::*;
use engines::*;
use ensure::*;
#[cfg(test)]
use git::*;
#[cfg(not(test))]
use git::handle_git_mode;
use providers::*;
use remote::*;
use sessions::*;
use terminal::*;

use crate::common::fs::write_json_atomic;
use crate::common::BoxError;
use crate::config::{find_builtin_external_engine, load_engine_manifest, parse_acp_engine_selector};
use crate::interfaces::acp::{AcpBridgeConfig, AcpLaunchConfig, AcpRpcBridge};
use crate::interfaces::rpc::{set_output_sink, RpcServer};
use serde_json::{json, Value};
use std::env;
use std::io::{BufRead, BufReader, BufWriter, Write};
use std::net::TcpStream;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

const PROTOCOL_VERSION: i64 = 1;

/// Jalankan mode `oopbot host <mode> <port>`
pub async fn run_host_mode(mode: &str, port: u16) -> Result<(), BoxError> {
    // 1. Ambil token dari environment atau baca 1 baris dari stdin
    let token = match env::var("OOPBOT_TOKEN") {
        Ok(t) if !t.trim().is_empty() => t.trim().to_string(),
        _ => {
            let mut line = String::new();
            std::io::stdin().read_line(&mut line)?;
            line.trim().to_string()
        }
    };

    if token.is_empty() {
        return Err("OOPBOT_TOKEN kosong atau tidak ditemukan".into());
    }

    let home = env::var("HOME").unwrap_or_else(|_| ".".into());
    let prefix = env::var("PREFIX").unwrap_or_else(|_| "/data/data/com.termux/files/usr".into());
    let oopbot_root = env::var("OOPBOT_ROOT")
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .map(PathBuf::from);
    let config_dir = env::var("OOPBOT_CONFIG_DIR")
        .ok()
        .filter(|s| !s.trim().is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(&home).join(".oopbot"));
    let _ = std::fs::create_dir_all(&config_dir);

    // Cek shared storage
    let shared_storage = Path::new("/sdcard").exists()
        || env::var("EXTERNAL_STORAGE")
            .map(|s| Path::new(&s).exists())
            .unwrap_or(false);

    // 2. Hubungkan ke HostServer Android di 127.0.0.1:<port>
    let stream = TcpStream::connect(("127.0.0.1", port))?;
    let write_stream = stream.try_clone()?;
    let mut reader = BufReader::new(stream);
    let mut writer = BufWriter::new(write_stream);

    // 3. Kirim handshake `oopbot_hello`
    let hello = json!({
        "type": "oopbot_hello",
        "protocol": PROTOCOL_VERSION,
        "token": token,
        "mode": mode,
        "oopbotVersion": env!("CARGO_PKG_VERSION"),
        "nodeVersion": "native-tokio",
        "configDir": config_dir.to_string_lossy(),
        "home": home,
        "sharedStorage": shared_storage,
    });

    writeln!(writer, "{}", serde_json::to_string(&hello)?)?;
    writer.flush()?;

    // 4. Baca request pertama dari Android (`oopbot_request`)
    let mut req_line = String::new();
    let bytes_read = reader.read_line(&mut req_line)?;
    if bytes_read == 0 {
        return Ok(());
    }

    let request: Value = match serde_json::from_str(req_line.trim()) {
        Ok(v) => v,
        Err(_) => {
            let err = json!({
                "type": "oopbot_result",
                "ok": false,
                "error": "Expected a valid oopbot_request JSON",
                "code": "bad_request"
            });
            writeln!(writer, "{}", serde_json::to_string(&err)?)?;
            writer.flush()?;
            return Ok(());
        }
    };

    let params = request.get("params").cloned().unwrap_or(json!({}));

    // 5. Tangani request sesuai mode
    match mode {
        "ensure" => {
            handle_ensure_mode(params, Path::new(&prefix), oopbot_root.as_deref(), writer).await
        }
        "hello" => {
            let result = json!({
                "type": "oopbot_result",
                "ok": true,
                "data": {}
            });
            writeln!(writer, "{}", serde_json::to_string(&result)?)?;
            writer.flush()?;
            Ok(())
        }
        "login" => {
            handle_login_mode(params, &config_dir, reader, writer).await
        }
        "engine_install" => {
            handle_engine_install_mode(params, &config_dir, Path::new(&prefix), writer).await
        }
        "engine_login" => {
            handle_engine_login_mode(params, &config_dir, reader, writer).await
        }
        "rpc" => {
            handle_rpc_mode(params, &config_dir, reader, writer).await
        }
        "utility" => {
            handle_utility_mode(&config_dir, Path::new(&prefix), &home, reader, writer).await
        }
        "browse" => {
            handle_browse_mode(params, &home, reader, writer).await
        }
        "files" => {
            handle_files_mode(params, reader, writer).await
        }
        "git" => {
            handle_git_mode(reader, writer).await
        }
        "terminal" => {
            handle_terminal_mode(params, reader, writer).await
        }
        "package_install" => {
            let result = json!({
                "type": "oopbot_result",
                "ok": false,
                "code": "unsupported",
                "error": "Native OOPBoT engine does not run npm/pi package installs.",
                "data": {}
            });
            writeln!(writer, "{}", serde_json::to_string(&result)?)?;
            writer.flush()?;
            Ok(())
        }
        "update_extensions" => {
            let result = json!({
                "type": "oopbot_result",
                "ok": false,
                "code": "unsupported",
                "error": "Native OOPBoT engine does not run npm extension updates.",
                "data": {}
            });
            writeln!(writer, "{}", serde_json::to_string(&result)?)?;
            writer.flush()?;
            Ok(())
        }
        "remote_connect" | "remote_install" => {
            let result = json!({
                "type": "oopbot_result",
                "ok": false,
                "code": "unsupported",
                "error": "Interactive SSH connection is not supported in OOPBoT host mode yet.",
                "data": {}
            });
            writeln!(writer, "{}", serde_json::to_string(&result)?)?;
            writer.flush()?;
            Ok(())
        }
        "backup_create" => {
            let mut progress_cb = |line: &str, key: &str, count: usize| {
                let msg = json!({
                    "type": "oopbot_progress",
                    "line": line,
                    "lineI18n": { "key": key, "args": { "count": count } }
                });
                let _ = writeln!(writer, "{}", serde_json::to_string(&msg).unwrap_or_default());
                let _ = writer.flush();
            };
            let result = handle_backup_create(&config_dir, &mut progress_cb);
            writeln!(writer, "{}", serde_json::to_string(&result)?)?;
            writer.flush()?;
            Ok(())
        }
        "backup_restore" => {
            let mut progress_cb = |line: &str, key: &str, count: usize| {
                let msg = json!({
                    "type": "oopbot_progress",
                    "line": line,
                    "lineI18n": { "key": key, "args": { "count": count } }
                });
                let _ = writeln!(writer, "{}", serde_json::to_string(&msg).unwrap_or_default());
                let _ = writer.flush();
            };
            let result = handle_backup_restore(&config_dir, &params, &mut progress_cb);
            writeln!(writer, "{}", serde_json::to_string(&result)?)?;
            writer.flush()?;
            Ok(())
        }
        other => {
            let result = json!({
                "type": "oopbot_result",
                "ok": false,
                "code": "unsupported",
                "error": format!("Unsupported host mode: {}", other),
                "data": {}
            });
            writeln!(writer, "{}", serde_json::to_string(&result)?)?;
            writer.flush()?;
            Ok(())
        }
    }
}


pub(super) static TMP_SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);


/// Menangani mode `login`: alur autentikasi interaktif (prompt API key ke UI Android)
async fn handle_login_mode(
    params: Value,
    config_dir: &Path,
    mut reader: BufReader<TcpStream>,
    mut writer: BufWriter<TcpStream>,
) -> Result<(), BoxError> {
    let provider_id = params["providerId"].as_str().unwrap_or("google").to_string();
    let provider_label = match provider_id.as_str() {
        "google" => "Google Gemini",
        "anthropic" => "Anthropic Claude",
        "openai" | "openai-codex" => "OpenAI",
        "openrouter" => "OpenRouter",
        "zai" => "Z.ai",
        "kimi-coding" => "Kimi",
        other => other,
    };

    // Kirim prompt ke UI Android
    let prompt_msg = json!({
        "type": "oopbot_auth_prompt",
        "id": 1,
        "prompt": {
            "type": "input",
            "message": format!("Enter your {} API key", provider_label),
            "placeholder": "API Key"
        }
    });
    writeln!(writer, "{}", serde_json::to_string(&prompt_msg)?)?;
    writer.flush()?;

    // Baca jawaban dari Android
    let mut resp_line = String::new();
    while let Ok(bytes) = reader.read_line(&mut resp_line) {
        if bytes == 0 {
            break;
        }
        let trimmed = resp_line.trim();
        if trimmed.is_empty() {
            resp_line.clear();
            continue;
        }

        if let Ok(resp) = serde_json::from_str::<Value>(trimmed) {
            match resp["type"].as_str() {
                Some("oopbot_auth_response") => {
                    if let Some(val) = resp["value"].as_str() {
                        let key = val.trim();
                        if !key.is_empty() {
                            if let Err(e) = save_auth_key(config_dir, &provider_id, key) {
                                let err = json!({
                                    "type": "oopbot_result",
                                    "ok": false,
                                    "error": format!("Failed to save API key: {}", e),
                                    "code": "io_error",
                                    "data": {}
                                });
                                writeln!(writer, "{}", serde_json::to_string(&err)?)?;
                                writer.flush()?;
                                return Ok(());
                            }
                        }
                    }
                    let res = json!({
                        "type": "oopbot_result",
                        "ok": true,
                        "data": {}
                    });
                    writeln!(writer, "{}", serde_json::to_string(&res)?)?;
                    writer.flush()?;
                    return Ok(());
                }
                Some("oopbot_auth_cancel") => {
                    let err = json!({
                        "type": "oopbot_result",
                        "ok": false,
                        "error": "Login cancelled",
                        "code": "cancelled"
                    });
                    writeln!(writer, "{}", serde_json::to_string(&err)?)?;
                    writer.flush()?;
                    return Ok(());
                }
                _ => {}
            }
        }
        resp_line.clear();
    }

    let res = json!({
        "type": "oopbot_result",
        "ok": true,
        "data": {}
    });
    writeln!(writer, "{}", serde_json::to_string(&res)?)?;
    writer.flush()?;
    Ok(())
}

/// Menangani mode `rpc`: mengalirkan komunikasi Agent langsung ke TCP socket Android
async fn handle_rpc_mode(
    params: Value,
    config_dir: &Path,
    reader: BufReader<TcpStream>,
    mut writer: BufWriter<TcpStream>,
) -> Result<(), BoxError> {
    if let Some(err) = check_rpc_expected_version(&params, env!("CARGO_PKG_VERSION")) {
        writeln!(writer, "{}", serde_json::to_string(&err)?)?;
        writer.flush()?;
        return Ok(());
    }

    let cwd = params["cwd"].as_str().map(PathBuf::from);
    if let Some(ref dir) = cwd {
        if !dir.is_dir() {
            let err = json!({
                "type": "oopbot_result",
                "ok": false,
                "error": "Folder not found",
                "code": "cwd_missing"
            });
            let mut w = writer;
            writeln!(w, "{}", serde_json::to_string(&err)?)?;
            w.flush()?;
            return Ok(());
        }
        let _ = env::set_current_dir(dir);
    }

    let session_path = params["sessionPath"].as_str().map(str::to_string);

    // Deteksi apakah sesi ini ditargetkan ke External ACP Engine (`engine: "acp:<id>"` atau header sesi)
    let engine_selector = params["engine"]
        .as_str()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .or_else(|| {
            session_path
                .as_deref()
                .and_then(|p| detect_session_engine(Path::new(p)))
        });

    if let Some(engine_id) = engine_selector
        .as_deref()
        .and_then(parse_acp_engine_selector)
    {
        let Some(manifest) = load_engine_manifest(config_dir, engine_id) else {
            let err = json!({
                "type": "oopbot_result",
                "ok": false,
                "code": "engine_not_installed",
                "error": format!("External engine '{}' is not installed. Please install it from Add a provider.", engine_id),
                "data": { "engineId": engine_id }
            });
            writeln!(writer, "{}", serde_json::to_string(&err)?)?;
            writer.flush()?;
            return Ok(());
        };

        if !Path::new(&manifest.command).is_file() {
            let err = json!({
                "type": "oopbot_result",
                "ok": false,
                "code": "engine_not_installed",
                "error": format!("Engine '{}' binary missing at {}. Please reinstall.", engine_id, manifest.command),
                "data": { "engineId": engine_id }
            });
            writeln!(writer, "{}", serde_json::to_string(&err)?)?;
            writer.flush()?;
            return Ok(());
        }

        let resolved_cwd = cwd
            .clone()
            .or_else(|| env::current_dir().ok())
            .unwrap_or_else(|| PathBuf::from("."));
        let display_name = find_builtin_external_engine(engine_id)
            .map(|s| s.display_name.to_string())
            .unwrap_or_else(|| engine_id.to_string());

        let writer_arc: Arc<Mutex<Box<dyn Write + Send + Sync>>> =
            Arc::new(Mutex::new(Box::new(writer)));

        let cfg = AcpBridgeConfig {
            engine_id: engine_id.to_string(),
            display_name,
            config_dir: config_dir.to_path_buf(),
            cwd: resolved_cwd.clone(),
            session_path: session_path.map(PathBuf::from),
            launch: AcpLaunchConfig {
                command: PathBuf::from(&manifest.command),
                args: manifest.args.clone(),
                cwd: resolved_cwd,
                env: Vec::new(),
            },
        };

        let bridge = match AcpRpcBridge::start(cfg, writer_arc.clone()).await {
            Ok(b) => b,
            Err(e) => {
                if let Ok(mut w) = writer_arc.lock() {
                    let err = json!({
                        "type": "oopbot_result",
                        "ok": false,
                        "code": "acp_start_failed",
                        "error": format!("Failed to start ACP engine '{}': {}", engine_id, e),
                        "data": { "engineId": engine_id }
                    });
                    let _ = writeln!(w, "{}", serde_json::to_string(&err)?);
                    let _ = w.flush();
                }
                return Ok(());
            }
        };

        let result = bridge.run_loop_with_input(reader).await;

        if let Ok(mut w) = writer_arc.lock() {
            let exit_msg = json!({
                "type": "oopbot_exit",
                "code": 0,
                "signal": Value::Null,
                "stderr": ""
            });
            let _ = writeln!(w, "{}", serde_json::to_string(&exit_msg)?);
            let _ = w.flush();
        }

        return result;
    }

    // Ambil ekstensi yang diaktifkan
    let mut extensions = Vec::new();
    if let Some(ext_arr) = params["oopbotExtensions"].as_array() {
        for ext in ext_arr {
            if let Some(name) = ext["name"].as_str() {
                extensions.push(name.to_string());
            }
        }
    }

    // Bungkus writer ke dalam Arc<Mutex<Box<dyn Write + Send + Sync>>>
    let writer_arc: Arc<Mutex<Box<dyn Write + Send + Sync>>> =
        Arc::new(Mutex::new(Box::new(writer)));
    set_output_sink(Some(writer_arc.clone()));

    // Inisialisasi RpcServer
    let mut server = RpcServer::new(session_path, None, extensions)?;

    // Jalankan RPC loop membaca dari socket Android
    let result = server.run_loop_with_input(reader).await;

    // Bersihkan output sink
    set_output_sink(None);

    // Kirim sinyal exit ke Android
    if let Ok(mut w) = writer_arc.lock() {
        let exit_msg = json!({
            "type": "oopbot_exit",
            "code": 0,
            "signal": Value::Null,
            "stderr": ""
        });
        let _ = writeln!(w, "{}", serde_json::to_string(&exit_msg)?);
        let _ = w.flush();
    }

    result
}

/// Menangani mode `utility`: menjawab request `sessions`, `auth_status`, `provider_models`, `engine_list`, dll.
async fn handle_utility_mode(
    config_dir: &Path,
    prefix: &Path,
    home: &str,
    mut reader: BufReader<TcpStream>,
    mut writer: BufWriter<TcpStream>,
) -> Result<(), BoxError> {
    // Kirim `oopbot_utility_ready`
    let ready = json!({ "type": "oopbot_utility_ready" });
    writeln!(writer, "{}", serde_json::to_string(&ready)?)?;
    writer.flush()?;

    let mut line = String::new();
    while let Ok(bytes) = reader.read_line(&mut line) {
        if bytes == 0 {
            break;
        }
        let trimmed = line.trim();
        if trimmed.is_empty() {
            line.clear();
            continue;
        }

        let Ok(req) = serde_json::from_str::<Value>(trimmed) else {
            line.clear();
            continue;
        };

        let req_type = req["type"].as_str().unwrap_or_default();
        if req_type != "oopbot_utility_request" {
            line.clear();
            continue;
        }

        let id = req["id"].as_i64().unwrap_or(0);
        let op = req["op"].as_str().unwrap_or_default();
        let params = req.get("params").cloned().unwrap_or(json!({}));

        let response = match op {
            "sessions" => handle_list_sessions(config_dir, home),
            "session_messages" => handle_session_messages(&params),
            "delete_sessions" => handle_delete_sessions(&params),
            "auth_status" => handle_auth_status(config_dir),
            "logout" => handle_logout(config_dir, &params),
            "provider_models" => handle_provider_models(&params).await,
            "model_defaults" => handle_model_defaults(&params),
            "subscription_models" => handle_subscription_models(&params),
            "subscription_save" => handle_subscription_save(config_dir, &params),
            "provider_save" => handle_provider_save(config_dir, &params),
            "provider_remove" => handle_provider_remove(config_dir, &params),
            "local_models_sync" => handle_local_models_sync(config_dir, &params),
            "default_model" => handle_save_default_model(config_dir, &params),
            "save_attachments" => handle_save_attachments(&params, config_dir),
            "subagent_sessions" => handle_subagent_sessions(config_dir, &params),
            "backups" => handle_list_backups(config_dir),
            "backup_delete" => handle_backup_delete(config_dir, &params),
            "engine_list" => handle_engine_list(config_dir, prefix, home),
            "engine_uninstall" => handle_engine_uninstall(config_dir, &params),
            "remote_hosts" => handle_remote_hosts(config_dir, home),
            "remote_host_save" => handle_remote_host_save(config_dir, home, &params),
            "remote_host_remove" => handle_remote_host_remove(config_dir, home, &params),
            "remote_workspace" => handle_remote_workspace(config_dir, &params),
            "remote_status" => handle_remote_status(config_dir, &params),
            "remote_disconnect" => handle_remote_disconnect(config_dir, &params),
            "remote_forget_host_key" => handle_remote_forget_host_key(config_dir, &params),
            "remote_key" => handle_remote_key(config_dir),
            "remote_key_new" => handle_remote_key_new(config_dir),
            other => json!({
                "type": "oopbot_result",
                "ok": false,
                "code": "unknown_op",
                "error": format!("Unknown utility operation: {}", other),
                "data": {}
            }),
        };

        let mut final_res = response;
        final_res["id"] = json!(id);
        writeln!(writer, "{}", serde_json::to_string(&final_res)?)?;
        writer.flush()?;

        line.clear();
    }

    Ok(())
}


pub(super) fn write_json_file(path: &Path, val: &Value) -> Result<(), BoxError> {
    write_json_atomic(path, val, None)?;
    Ok(())
}

pub(super) fn write_json_file_mode(
    path: &Path,
    val: &Value,
    mode: Option<u32>,
) -> Result<(), BoxError> {
    write_json_atomic(path, val, mode)?;
    Ok(())
}

pub(super) fn save_auth_key(
    config_dir: &Path,
    provider_id: &str,
    api_key: &str,
) -> Result<(), BoxError> {
    crate::config::store::save_auth_key(config_dir, provider_id, api_key, None)?;
    Ok(())
}

