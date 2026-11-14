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
//! - `browse`         : Penjelajahan direktori workspace (pipkin_places, pipkin_list, pipkin_search).
//! - `files`          : File Explorer workspace (pipkin_files_list, pipkin_files_read, pipkin_files_mkdir).
//! - `git`            : Tab Git Changes (pipkin_git_detect, pipkin_git_changes, pipkin_git_commits, pipkin_git_commit_diff).
//! - `terminal`       : Terminal interaktif shell bawaan (streaming stdout/stderr Base64 & input).
//! - `utility`        : Penanganan one-off request (daftar sesi, model default, auth status, attachments, dll).
//! - `package_install`: Penanganan instalasi paket ekstensi runtime.
//! - `backup_create`  : Pembuatan snapshot backup konfigurasi.
//! - `backup_restore` : Pemulihan snapshot konfigurasi dari file backup.


mod backups;
mod browse_files;
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
use ensure::*;
#[cfg(test)]
use git::*;
#[cfg(not(test))]
use git::handle_git_mode;
use providers::*;
use remote::*;
use sessions::*;
use terminal::*;

use crate::gemini::BoxError;
use crate::rpc::{set_output_sink, RpcServer};
use crate::util::read_json_file;
use serde_json::{json, Value};
use std::env;
use std::io::{BufRead, BufReader, BufWriter, Write};
use std::net::TcpStream;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

const PROTOCOL_VERSION: i64 = 1;

/// Jalankan mode `oopbot host <mode> <port>`
pub async fn run_host_mode(mode: &str, port: u16) -> Result<(), BoxError> {
    // 1. Ambil token dari environment atau baca 1 baris dari stdin (Dual-Support: OOPBOT_TOKEN / PIPKIN_TOKEN)
    let token = match env::var("OOPBOT_TOKEN").or_else(|_| env::var("PIPKIN_TOKEN")) {
        Ok(t) if !t.trim().is_empty() => t.trim().to_string(),
        _ => {
            let mut line = String::new();
            std::io::stdin().read_line(&mut line)?;
            line.trim().to_string()
        }
    };

    if token.is_empty() {
        return Err("OOPBOT_TOKEN / PIPKIN_TOKEN kosong atau tidak ditemukan".into());
    }

    let home = env::var("HOME").unwrap_or_else(|_| ".".into());
    let prefix = env::var("PREFIX").unwrap_or_else(|_| "/data/data/com.termux/files/usr".into());
    let oopbot_root = env::var("OOPBOT_ROOT")
        .or_else(|_| env::var("PIPKIN_ROOT"))
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .map(PathBuf::from);
    let config_dir = env::var("OOPBOT_CONFIG_DIR")
        .or_else(|_| env::var("PI_CODING_AGENT_DIR"))
        .map(PathBuf::from)
        .unwrap_or_else(|_| {
            let oopbot_dir = PathBuf::from(&home).join(".oopbot");
            if oopbot_dir.exists() {
                oopbot_dir
            } else {
                PathBuf::from(&home).join(".pipkin/pi")
            }
        });
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

    // 3. Kirim handshake `pipkin_hello` (Dual-Support: oopbotVersion & piVersion)
    let hello = json!({
        "type": "pipkin_hello",
        "protocol": PROTOCOL_VERSION,
        "token": token,
        "mode": mode,
        "oopbotVersion": env!("CARGO_PKG_VERSION"),
        "piVersion": env!("CARGO_PKG_VERSION"),
        "nodeVersion": "native-tokio",
        "configDir": config_dir.to_string_lossy(),
        "home": home,
        "sharedStorage": shared_storage,
    });

    writeln!(writer, "{}", serde_json::to_string(&hello)?)?;
    writer.flush()?;

    // 4. Baca request pertama dari Android (`pipkin_request`)
    let mut req_line = String::new();
    let bytes_read = reader.read_line(&mut req_line)?;
    if bytes_read == 0 {
        return Ok(());
    }

    let request: Value = match serde_json::from_str(req_line.trim()) {
        Ok(v) => v,
        Err(_) => {
            let err = json!({
                "type": "pipkin_result",
                "ok": false,
                "error": "Expected a valid pipkin_request JSON",
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
                "type": "pipkin_result",
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
        "rpc" => {
            handle_rpc_mode(params, &config_dir, reader, writer).await
        }
        "utility" => {
            handle_utility_mode(&config_dir, &home, reader, writer).await
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
                "type": "pipkin_result",
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
                "type": "pipkin_result",
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
                "type": "pipkin_result",
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
                    "type": "pipkin_progress",
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
                    "type": "pipkin_progress",
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
                "type": "pipkin_result",
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
        "type": "pipkin_auth_prompt",
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
                Some("pipkin_auth_response") | Some("oopbot_auth_response") => {
                    if let Some(val) = resp["value"].as_str() {
                        let key = val.trim();
                        if !key.is_empty() {
                            save_auth_key(config_dir, &provider_id, key);
                        }
                    }
                    let res = json!({
                        "type": "pipkin_result",
                        "ok": true,
                        "data": {}
                    });
                    writeln!(writer, "{}", serde_json::to_string(&res)?)?;
                    writer.flush()?;
                    return Ok(());
                }
                Some("pipkin_auth_cancel") | Some("oopbot_auth_cancel") => {
                    let err = json!({
                        "type": "pipkin_result",
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
        "type": "pipkin_result",
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
    _config_dir: &Path,
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
                "type": "pipkin_result",
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

    // Ambil ekstensi yang diaktifkan
    let mut extensions = Vec::new();
    if let Some(ext_arr) = params["pipkinExtensions"].as_array() {
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
            "type": "pipkin_exit",
            "code": 0,
            "signal": Value::Null,
            "stderr": ""
        });
        let _ = writeln!(w, "{}", serde_json::to_string(&exit_msg)?);
        let _ = w.flush();
    }

    result
}

/// Menangani mode `utility`: menjawab request `sessions`, `auth_status`, `provider_models`, dll.
async fn handle_utility_mode(
    config_dir: &Path,
    home: &str,
    mut reader: BufReader<TcpStream>,
    mut writer: BufWriter<TcpStream>,
) -> Result<(), BoxError> {
    // Kirim `pipkin_utility_ready`
    let ready = json!({ "type": "pipkin_utility_ready" });
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
        if req_type != "pipkin_utility_request" && req_type != "oopbot_utility_request" {
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
                "type": "pipkin_result",
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
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let text = serde_json::to_string_pretty(val)?;
    std::fs::write(path, format!("{}\n", text))?;
    Ok(())
}

pub(super) fn save_auth_key(config_dir: &Path, provider_id: &str, api_key: &str) {
    let auth_path = config_dir.join("auth.json");
    let mut auth_json = read_json_file(&auth_path);
    if !auth_json.is_object() {
        auth_json = json!({});
    }
    auth_json[provider_id] = json!({
        "type": "api_key",
        "apiKey": api_key,
    });
    let _ = write_json_file(&auth_path, &auth_json);
}

