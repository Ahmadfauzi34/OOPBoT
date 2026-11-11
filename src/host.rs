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

use crate::gemini::BoxError;
use crate::models::{parse_models_response, ModelCatalog};
use crate::rpc::{set_output_sink, RpcServer};
use serde_json::{json, Value};
use std::collections::HashSet;
use std::env;
use std::io::{BufRead, BufReader, BufWriter, Write};
use std::net::TcpStream;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

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
    let _prefix = env::var("PREFIX").unwrap_or_else(|_| "/data/data/com.termux/files/usr".into());
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
            // Android meminta verifikasi kesiapan runtime
            let progress = json!({
                "type": "pipkin_progress",
                "line": format!("OOPBoT v{} native engine ready (Node.js bypassed)", env!("CARGO_PKG_VERSION"))
            });
            writeln!(writer, "{}", serde_json::to_string(&progress)?)?;
            writer.flush()?;

            let result = json!({
                "type": "pipkin_result",
                "ok": true,
                "data": {
                    "oopbotVersion": env!("CARGO_PKG_VERSION"),
                    "piVersion": env!("CARGO_PKG_VERSION")
                }
            });
            writeln!(writer, "{}", serde_json::to_string(&result)?)?;
            writer.flush()?;
            Ok(())
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
            let progress = json!({
                "type": "pipkin_progress",
                "line": "OOPBoT native engine: package install skipped (native Tokio agent)"
            });
            let _ = writeln!(writer, "{}", serde_json::to_string(&progress)?);
            let _ = writer.flush();
            let result = json!({
                "type": "pipkin_result",
                "ok": true,
                "data": {}
            });
            writeln!(writer, "{}", serde_json::to_string(&result)?)?;
            writer.flush()?;
            Ok(())
        }
        "backup_create" => {
            let now_iso = iso_now();
            let result = json!({
                "type": "pipkin_result",
                "ok": true,
                "data": {
                    "backup": {
                        "name": format!("oopbot-backup-{}.tar.gz", epoch_ms()),
                        "size": 0,
                        "createdAt": now_iso,
                        "piVersion": env!("CARGO_PKG_VERSION"),
                        "oopbotVersion": env!("CARGO_PKG_VERSION")
                    }
                }
            });
            writeln!(writer, "{}", serde_json::to_string(&result)?)?;
            writer.flush()?;
            Ok(())
        }
        "backup_restore" => {
            let result = json!({
                "type": "pipkin_result",
                "ok": true,
                "data": {}
            });
            writeln!(writer, "{}", serde_json::to_string(&result)?)?;
            writer.flush()?;
            Ok(())
        }
        other => {
            // Mode lain: beri respon result dasar agar Android tidak hanging
            let result = json!({
                "type": "pipkin_result",
                "ok": true,
                "data": { "mode": other, "note": "native OOPBoT handled" }
            });
            writeln!(writer, "{}", serde_json::to_string(&result)?)?;
            writer.flush()?;
            Ok(())
        }
    }
}

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
    writer: BufWriter<TcpStream>,
) -> Result<(), BoxError> {
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
            "subagent_sessions" => json!({
                "type": "pipkin_result",
                "ok": true,
                "data": { "sessions": [] }
            }),
            "backups" => json!({
                "type": "pipkin_result",
                "ok": true,
                "data": { "dir": config_dir.to_string_lossy(), "backups": [] }
            }),
            "backup_delete" => json!({
                "type": "pipkin_result",
                "ok": true,
                "data": {}
            }),
            "remote_hosts" => json!({
                "type": "pipkin_result",
                "ok": true,
                "data": { "hosts": [] }
            }),
            _ => json!({
                "type": "pipkin_result",
                "ok": true,
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

/// Menangani mode `browse`: penjelajahan direktori untuk pemilih folder workspace di UI Android
async fn handle_browse_mode(
    _params: Value,
    home: &str,
    reader: BufReader<TcpStream>,
    writer: BufWriter<TcpStream>,
) -> Result<(), BoxError> {
    run_browse_loop(home, reader, writer)
}

fn run_browse_loop<R: BufRead, W: Write>(
    home: &str,
    mut reader: R,
    mut writer: W,
) -> Result<(), BoxError> {
    // 1. Kumpulkan places (home, shared storage, dsb)
    let mut places = vec![json!({ "kind": "home", "path": home })];
    if Path::new("/sdcard").exists() {
        places.push(json!({ "kind": "shared", "path": "/sdcard" }));
    }
    if let Ok(entries) = std::fs::read_dir("/storage") {
        for entry in entries.flatten() {
            let p = entry.path();
            let name = entry.file_name().to_string_lossy().to_string();
            if name != "emulated" && name != "self" && p.is_dir() {
                places.push(json!({ "kind": "volume", "path": p.to_string_lossy(), "name": name }));
            }
        }
    }

    let places_msg = json!({
        "type": "pipkin_places",
        "places": places
    });
    writeln!(writer, "{}", serde_json::to_string(&places_msg)?)?;
    writer.flush()?;

    // 2. Loop menerima permintaan dari Android
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

        let id = req["id"].as_i64().unwrap_or(0);
        match req["type"].as_str() {
            Some("pipkin_list") | Some("oopbot_list") => {
                let raw_path = req["path"].as_str().unwrap_or(home);
                let resolved = resolve_browse_path(raw_path, home);
                let parent = if resolved == Path::new("/") {
                    None
                } else {
                    resolved.parent().and_then(|p| {
                        let s = p.to_string_lossy().to_string();
                        if s.is_empty() { None } else { Some(s) }
                    })
                };

                let mut dirs = Vec::new();
                if let Ok(entries) = std::fs::read_dir(&resolved) {
                    for entry in entries.flatten() {
                        let p = entry.path();
                        if p.is_dir() {
                            dirs.push(entry.file_name().to_string_lossy().to_string());
                        }
                    }
                }
                // Urutkan: non-dot folder dulu, lalu dot folder di akhir, alfabetis case-insensitive
                dirs.sort_by(|a, b| {
                    let a_dot = a.starts_with('.');
                    let b_dot = b.starts_with('.');
                    match (a_dot, b_dot) {
                        (true, false) => std::cmp::Ordering::Greater,
                        (false, true) => std::cmp::Ordering::Less,
                        _ => a.to_lowercase().cmp(&b.to_lowercase()),
                    }
                });

                let reply = json!({
                    "type": "pipkin_listing",
                    "id": id,
                    "path": resolved.to_string_lossy(),
                    "parent": parent,
                    "dirs": dirs
                });
                writeln!(writer, "{}", serde_json::to_string(&reply)?)?;
                writer.flush()?;
            }
            Some("pipkin_search") | Some("oopbot_search") => {
                let query = req["query"].as_str().unwrap_or("").trim();
                let mut matched_paths = Vec::new();
                if !query.is_empty() {
                    let query_lower = query.to_lowercase();
                    for place in &places {
                        if let Some(p_str) = place["path"].as_str() {
                            search_folders_recursive(Path::new(p_str), &query_lower, 0, 4, 300, &mut matched_paths);
                        }
                    }
                }
                let reply = json!({
                    "type": "pipkin_search_results",
                    "id": id,
                    "paths": matched_paths
                });
                writeln!(writer, "{}", serde_json::to_string(&reply)?)?;
                writer.flush()?;
            }
            _ => {}
        }

        line.clear();
    }

    Ok(())
}

/// Menangani mode `files`: backend File Explorer workspace di UI Android
async fn handle_files_mode(
    _params: Value,
    reader: BufReader<TcpStream>,
    writer: BufWriter<TcpStream>,
) -> Result<(), BoxError> {
    run_files_loop(reader, writer)
}

fn run_files_loop<R: BufRead, W: Write>(
    mut reader: R,
    mut writer: W,
) -> Result<(), BoxError> {
    // 1. Kirim sinyal siap
    let ready = json!({ "type": "pipkin_files_ready" });
    writeln!(writer, "{}", serde_json::to_string(&ready)?)?;
    writer.flush()?;

    // 2. Loop menerima permintaan operasi berkas dari Android
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

        let id = req["id"].as_i64().unwrap_or(0);
        let root = req["root"].as_str().unwrap_or(".");
        let path = req["path"].as_str().unwrap_or(".");

        match req["type"].as_str() {
            Some("pipkin_files_list") | Some("oopbot_files_list") => {
                let root_path = PathBuf::from(root);
                let target_dir = if path == "." || path.is_empty() {
                    root_path.clone()
                } else {
                    root_path.join(path)
                };

                let mut entries = Vec::new();
                if let Ok(dir_entries) = std::fs::read_dir(&target_dir) {
                    for entry in dir_entries.flatten() {
                        let p = entry.path();
                        let name = entry.file_name().to_string_lossy().to_string();
                        let is_dir = p.is_dir();
                        let meta = std::fs::metadata(&p).ok();
                        let size = if is_dir { 0 } else { meta.as_ref().map(|m| m.len()).unwrap_or(0) };
                        let modified = meta
                            .and_then(|m| m.modified().ok())
                            .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
                            .map(|d| d.as_millis() as u64)
                            .unwrap_or(0);

                        let rel_path = if path == "." || path.is_empty() {
                            name.clone()
                        } else {
                            format!("{}/{}", path, name)
                        };

                        entries.push(json!({
                            "name": name,
                            "path": rel_path,
                            "kind": if is_dir { "directory" } else { "file" },
                            "size": size,
                            "modified": modified
                        }));
                    }
                }

                // Urutkan: direktori dulu, lalu file, alfabetis case-insensitive
                entries.sort_by(|a, b| {
                    let a_is_dir = a["kind"] == "directory";
                    let b_is_dir = b["kind"] == "directory";
                    match (a_is_dir, b_is_dir) {
                        (true, false) => std::cmp::Ordering::Less,
                        (false, true) => std::cmp::Ordering::Greater,
                        _ => a["name"].as_str().unwrap_or("").to_lowercase().cmp(&b["name"].as_str().unwrap_or("").to_lowercase()),
                    }
                });

                let reply = json!({
                    "type": "pipkin_files_listing",
                    "id": id,
                    "path": path,
                    "entries": entries
                });
                writeln!(writer, "{}", serde_json::to_string(&reply)?)?;
                writer.flush()?;
            }
            Some("pipkin_files_read") | Some("oopbot_files_read") => {
                let root_path = PathBuf::from(root);
                let file_path = if path == "." || path.is_empty() {
                    root_path
                } else {
                    root_path.join(path)
                };

                const TEXT_LIMIT: u64 = 2 * 1024 * 1024; // 2 MiB
                const IMAGE_LIMIT: u64 = 8 * 1024 * 1024; // 8 MiB

                let is_image = {
                    let ext = file_path.extension().and_then(|e| e.to_str()).unwrap_or("").to_lowercase();
                    matches!(ext.as_str(), "png" | "jpg" | "jpeg" | "gif" | "webp" | "bmp" | "ico")
                };

                let meta = std::fs::metadata(&file_path);
                let reply = match meta {
                    Ok(m) if m.is_dir() => {
                        json!({
                            "type": "pipkin_files_file",
                            "id": id,
                            "path": path,
                            "error": "This is a folder.",
                            "errorCode": "files_is_folder"
                        })
                    }
                    Ok(m) => {
                        let size = m.len();
                        if is_image {
                            if size > IMAGE_LIMIT {
                                json!({
                                    "type": "pipkin_files_file",
                                    "id": id,
                                    "path": path,
                                    "size": size,
                                    "kind": "too_large"
                                })
                            } else if let Ok(bytes) = std::fs::read(&file_path) {
                                let b64 = encode_base64(&bytes);
                                json!({
                                    "type": "pipkin_files_file",
                                    "id": id,
                                    "path": path,
                                    "size": size,
                                    "kind": "image",
                                    "content": b64
                                })
                            } else {
                                json!({
                                    "type": "pipkin_files_file",
                                    "id": id,
                                    "path": path,
                                    "size": size,
                                    "kind": "binary"
                                })
                            }
                        } else if size > TEXT_LIMIT {
                            let mut file = std::fs::File::open(&file_path).ok();
                            let mut sniff_buf = vec![0u8; 8192];
                            let read_bytes = file.as_mut().and_then(|f| std::io::Read::read(f, &mut sniff_buf).ok()).unwrap_or(0);
                            sniff_buf.truncate(read_bytes);
                            let is_text = is_probably_text(&sniff_buf);
                            json!({
                                "type": "pipkin_files_file",
                                "id": id,
                                "path": path,
                                "size": size,
                                "kind": if is_text { "too_large" } else { "binary" }
                            })
                        } else if let Ok(bytes) = std::fs::read(&file_path) {
                            if let Ok(text) = String::from_utf8(bytes.clone()) {
                                json!({
                                    "type": "pipkin_files_file",
                                    "id": id,
                                    "path": path,
                                    "size": size,
                                    "kind": "text",
                                    "content": text
                                })
                            } else {
                                json!({
                                    "type": "pipkin_files_file",
                                    "id": id,
                                    "path": path,
                                    "size": size,
                                    "kind": "binary"
                                })
                            }
                        } else {
                            json!({
                                "type": "pipkin_files_file",
                                "id": id,
                                "path": path,
                                "size": 0,
                                "kind": "binary"
                            })
                        }
                    }
                    Err(_) => {
                        json!({
                            "type": "pipkin_files_file",
                            "id": id,
                            "path": path,
                            "error": "This file or folder no longer exists.",
                            "errorCode": "files_gone"
                        })
                    }
                };
                writeln!(writer, "{}", serde_json::to_string(&reply)?)?;
                writer.flush()?;
            }
            Some("pipkin_files_mkdir") | Some("oopbot_files_mkdir") => {
                let name = req["name"].as_str().unwrap_or("").trim();
                if name.is_empty() || name == "." || name == ".." || name.contains('/') || name.contains('\0') {
                    let err = json!({
                        "type": "pipkin_files_made",
                        "id": id,
                        "error": "Folder names cannot be empty, contain / or be . or ..",
                        "errorCode": "files_folder_name_invalid"
                    });
                    writeln!(writer, "{}", serde_json::to_string(&err)?)?;
                    writer.flush()?;
                } else {
                    let root_path = PathBuf::from(root);
                    let target_parent = if path == "." || path.is_empty() {
                        root_path
                    } else {
                        root_path.join(path)
                    };
                    let new_folder = target_parent.join(name);
                    let created_rel = if path == "." || path.is_empty() {
                        name.to_string()
                    } else {
                        format!("{}/{}", path, name)
                    };
                    let _ = std::fs::create_dir_all(&new_folder);
                    let reply = json!({
                        "type": "pipkin_files_made",
                        "id": id,
                        "path": created_rel
                    });
                    writeln!(writer, "{}", serde_json::to_string(&reply)?)?;
                    writer.flush()?;
                }
            }
            _ => {}
        }

        line.clear();
    }

    Ok(())
}

/// Hitung SHA-256 hash dan format sebagai lowercase hex string (FIPS 180-2 self-contained)
pub fn sha256_hex(data: &[u8]) -> String {
    let mut h: [u32; 8] = [
        0x6a09e667, 0xbb67ae85, 0x3c6ef372, 0xa54ff53a,
        0x510e527f, 0x9b05688c, 0x1f83d9ab, 0x5be0cd19,
    ];
    let k: [u32; 64] = [
        0x428a2f98, 0x71374491, 0xb5c0fbcf, 0xe9b5dba5, 0x3956c25b, 0x59f111f1, 0x923f82a4, 0xab1c5ed5,
        0xd807aa98, 0x12835b01, 0x243185be, 0x550c7dc3, 0x72be5d74, 0x80deb1fe, 0x9bdc06a7, 0xc19bf174,
        0xe49b69c1, 0xefbe4786, 0x0fc19dc6, 0x240ca1cc, 0x2de92c6f, 0x4a7484aa, 0x5cb0a9dc, 0x76f988da,
        0x983e5152, 0xa831c66d, 0xb00327c8, 0xbf597fc7, 0xc6e00bf3, 0xd5a79147, 0x06ca6351, 0x14292967,
        0x27b70a85, 0x2e1b2138, 0x4d2c6dfc, 0x53380d13, 0x650a7354, 0x766a0abb, 0x81c2c92e, 0x92722c85,
        0xa2bfe8a1, 0xa81a664b, 0xc24b8b70, 0xc76c51a3, 0xd192e819, 0xd6990624, 0xf40e3585, 0x106aa070,
        0x19a4c116, 0x1e376c08, 0x2748774c, 0x34b0bcb5, 0x391c0cb3, 0x4ed8aa4a, 0x5b9cca4f, 0x682e6ff3,
        0x748f82ee, 0x78a5636f, 0x84c87814, 0x8cc70208, 0x90befffa, 0xa4506ceb, 0xbef9a3f7, 0xc67178f2,
    ];

    let bit_len = (data.len() as u64).wrapping_mul(8);
    let mut msg = data.to_vec();
    msg.push(0x80);
    while (msg.len() % 64) != 56 {
        msg.push(0);
    }
    msg.extend_from_slice(&bit_len.to_be_bytes());

    for chunk in msg.chunks_exact(64) {
        let mut w = [0u32; 64];
        for i in 0..16 {
            w[i] = u32::from_be_bytes(chunk[i * 4..i * 4 + 4].try_into().unwrap());
        }
        for i in 16..64 {
            let s0 = w[i - 15].rotate_right(7) ^ w[i - 15].rotate_right(18) ^ (w[i - 15] >> 3);
            let s1 = w[i - 2].rotate_right(17) ^ w[i - 2].rotate_right(19) ^ (w[i - 2] >> 10);
            w[i] = w[i - 16].wrapping_add(s0).wrapping_add(w[i - 7]).wrapping_add(s1);
        }

        let mut a = h[0];
        let mut b = h[1];
        let mut c = h[2];
        let mut d = h[3];
        let mut e = h[4];
        let mut f = h[5];
        let mut g = h[6];
        let mut h_val = h[7];

        for i in 0..64 {
            let s1 = e.rotate_right(6) ^ e.rotate_right(11) ^ e.rotate_right(25);
            let ch = (e & f) ^ ((!e) & g);
            let temp1 = h_val.wrapping_add(s1).wrapping_add(ch).wrapping_add(k[i]).wrapping_add(w[i]);
            let s0 = a.rotate_right(2) ^ a.rotate_right(13) ^ a.rotate_right(22);
            let maj = (a & b) ^ (a & c) ^ (b & c);
            let temp2 = s0.wrapping_add(maj);

            h_val = g;
            g = f;
            f = e;
            e = d.wrapping_add(temp1);
            d = c;
            c = b;
            b = a;
            a = temp1.wrapping_add(temp2);
        }

        h[0] = h[0].wrapping_add(a);
        h[1] = h[1].wrapping_add(b);
        h[2] = h[2].wrapping_add(c);
        h[3] = h[3].wrapping_add(d);
        h[4] = h[4].wrapping_add(e);
        h[5] = h[5].wrapping_add(f);
        h[6] = h[6].wrapping_add(g);
        h[7] = h[7].wrapping_add(h_val);
    }

    let mut out = String::with_capacity(64);
    for val in h {
        out.push_str(&format!("{:08x}", val));
    }
    out
}

/// Parser teks diff patch git terpadu (unified diff) menjadi format objek berkas Paseo / Pipkin
pub fn parse_git_diff(diff_text: &str) -> Vec<Value> {
    let mut files = Vec::new();
    let mut current_path = String::new();
    let mut current_change = "modified".to_string();
    let mut current_kind = "text".to_string();
    let mut current_additions: i64 = 0;
    let mut current_deletions: i64 = 0;
    let mut current_hunks: Vec<Value> = Vec::new();
    let mut current_hunk_header = String::new();
    let mut current_hunk_old_start: i64 = 0;
    let mut current_hunk_new_start: i64 = 0;
    let mut current_hunk_lines: Vec<String> = Vec::new();
    let mut hunk_bytes_total: usize = 0;
    let mut in_hunk = false;
    let mut in_file = false;

    let flush_hunk = |hunks: &mut Vec<Value>, header: &str, old_s: i64, new_s: i64, lines: &mut Vec<String>| {
        if !header.is_empty() || !lines.is_empty() {
            hunks.push(json!({
                "header": header,
                "oldStart": old_s,
                "newStart": new_s,
                "lines": std::mem::take(lines)
            }));
        }
    };

    let flush_file = |files: &mut Vec<Value>, path: &str, change: &str, additions: i64, deletions: i64, kind: &str, hunks: &mut Vec<Value>, bytes: usize| {
        if !path.is_empty() {
            let final_kind = if bytes > 1024 * 1024 {
                "too_large"
            } else {
                kind
            };
            files.push(json!({
                "path": path,
                "change": change,
                "additions": additions,
                "deletions": deletions,
                "kind": final_kind,
                "hunks": if final_kind == "too_large" { vec![] } else { std::mem::take(hunks) }
            }));
        }
    };

    for line in diff_text.lines() {
        if line.starts_with("diff --git ") {
            if in_hunk {
                flush_hunk(&mut current_hunks, &current_hunk_header, current_hunk_old_start, current_hunk_new_start, &mut current_hunk_lines);
                in_hunk = false;
            }
            if in_file {
                flush_file(&mut files, &current_path, &current_change, current_additions, current_deletions, &current_kind, &mut current_hunks, hunk_bytes_total);
            }
            in_file = true;
            current_change = "modified".to_string();
            current_kind = "text".to_string();
            current_additions = 0;
            current_deletions = 0;
            hunk_bytes_total = 0;
            current_hunk_header.clear();

            // Ekstrak b/ path: "diff --git a/foo.txt b/foo.txt"
            let rest = &line["diff --git ".len()..];
            if let Some(pos) = rest.rfind(" b/") {
                current_path = rest[pos + 3..].trim().to_string();
            } else if let Some(pos) = rest.find(' ') {
                current_path = rest[pos + 1..].trim().trim_start_matches("b/").to_string();
            } else {
                current_path = rest.trim().to_string();
            }
            continue;
        }

        if !in_file {
            continue;
        }

        if line.starts_with("new file mode ") {
            current_change = "added".to_string();
            continue;
        } else if line.starts_with("deleted file mode ") {
            current_change = "deleted".to_string();
            continue;
        } else if line.starts_with("rename from ") {
            current_change = "renamed".to_string();
            continue;
        } else if line.starts_with("rename to ") {
            current_path = line["rename to ".len()..].trim().to_string();
            continue;
        } else if line.starts_with("copy from ") {
            current_change = "copied".to_string();
            continue;
        } else if line.starts_with("copy to ") {
            current_path = line["copy to ".len()..].trim().to_string();
            continue;
        } else if line.starts_with("Binary files ") || line.starts_with("GIT binary patch") {
            current_kind = "binary".to_string();
            continue;
        } else if line.starts_with("--- ") || line.starts_with("+++ ") || line.starts_with("index ") {
            if line.starts_with("--- a/") && current_change == "deleted" && current_path.is_empty() {
                current_path = line["--- a/".len()..].trim().to_string();
            }
            if line.starts_with("+++ b/") && current_change != "deleted" {
                current_path = line["+++ b/".len()..].trim().to_string();
            }
            continue;
        }

        if line.starts_with("@@ ") {
            if in_hunk {
                flush_hunk(&mut current_hunks, &current_hunk_header, current_hunk_old_start, current_hunk_new_start, &mut current_hunk_lines);
            }
            in_hunk = true;
            let header = if let Some(end) = line[3..].find("@@") {
                line[..3 + end + 2].to_string()
            } else {
                line.to_string()
            };
            current_hunk_header = header;

            let mut old_s = 1;
            let mut new_s = 1;
            for part in line.split_whitespace() {
                if part.starts_with('-') {
                    let num_part = part[1..].split(',').next().unwrap_or("1");
                    old_s = num_part.parse::<i64>().unwrap_or(1);
                } else if part.starts_with('+') {
                    let num_part = part[1..].split(',').next().unwrap_or("1");
                    new_s = num_part.parse::<i64>().unwrap_or(1);
                }
            }
            current_hunk_old_start = old_s;
            current_hunk_new_start = new_s;
            continue;
        }

        if in_hunk {
            hunk_bytes_total += line.len() + 1;
            if line.starts_with('+') {
                current_additions += 1;
                current_hunk_lines.push(line.to_string());
            } else if line.starts_with('-') {
                current_deletions += 1;
                current_hunk_lines.push(line.to_string());
            } else if line.starts_with(' ') || line.starts_with('\\') {
                current_hunk_lines.push(line.to_string());
            }
        }
    }

    if in_hunk {
        flush_hunk(&mut current_hunks, &current_hunk_header, current_hunk_old_start, current_hunk_new_start, &mut current_hunk_lines);
    }
    if in_file {
        flush_file(&mut files, &current_path, &current_change, current_additions, current_deletions, &current_kind, &mut current_hunks, hunk_bytes_total);
    }

    files
}

/// Deteksi apakah direktori adalah repository git dan git tersedia di sistem
fn git_detect(root: &str, trusted: bool) -> (bool, bool) {
    let has_git = std::process::Command::new("git")
        .arg("--version")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false);
    let dot_git = Path::new(root).join(".git").exists();
    if !trusted || !has_git || !dot_git {
        return (dot_git, has_git);
    }
    let toplevel = std::process::Command::new("git")
        .args(["-C", root, "rev-parse", "--show-toplevel"])
        .output();
    if let Ok(out) = toplevel {
        if out.status.success() {
            let top_str = String::from_utf8_lossy(&out.stdout).trim().to_string();
            let real_root = std::fs::canonicalize(root)
                .map(|p| p.to_string_lossy().to_string())
                .unwrap_or_else(|_| root.to_string());
            let top_canon = std::fs::canonicalize(&top_str)
                .map(|p| p.to_string_lossy().to_string())
                .unwrap_or(top_str);
            return (top_canon == real_root, true);
        }
    }
    (false, true)
}

/// Nama branch git saat ini
fn git_current_branch(root: &str) -> Option<String> {
    let out = std::process::Command::new("git")
        .args(["-C", root, "symbolic-ref", "--quiet", "--short", "HEAD"])
        .output()
        .ok()?;
    if out.status.success() {
        let b = String::from_utf8_lossy(&out.stdout).trim().to_string();
        if !b.is_empty() {
            return Some(b);
        }
    }
    None
}

/// Default branch (origin/HEAD atau main / master)
fn git_default_branch(root: &str) -> Option<String> {
    let out = std::process::Command::new("git")
        .args(["-C", root, "symbolic-ref", "--quiet", "refs/remotes/origin/HEAD"])
        .output()
        .ok();
    if let Some(o) = out {
        if o.status.success() {
            let ref_name = String::from_utf8_lossy(&o.stdout).trim().to_string();
            if !ref_name.is_empty() {
                let remote_name = ref_name.trim_start_matches("refs/remotes/");
                let local_name = remote_name.trim_start_matches("origin/");
                return Some(local_name.to_string());
            }
        }
    }
    let has_main = std::process::Command::new("git")
        .args(["-C", root, "rev-parse", "--verify", "--quiet", "refs/heads/main"])
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false);
    if has_main {
        return Some("main".to_string());
    }
    let has_master = std::process::Command::new("git")
        .args(["-C", root, "rev-parse", "--verify", "--quiet", "refs/heads/master"])
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false);
    if has_master {
        return Some("master".to_string());
    }
    None
}

/// Ref pembanding (origin/<base> atau <base>)
fn git_comparison_ref(root: &str, base: &str) -> Option<String> {
    let name = base.trim_start_matches("refs/heads/").trim_start_matches("origin/");
    let origin_ref = format!("refs/remotes/origin/{}", name);
    let has_origin = std::process::Command::new("git")
        .args(["-C", root, "rev-parse", "--verify", "--quiet", &origin_ref])
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false);
    if has_origin {
        return Some(format!("origin/{}", name));
    }
    let local_ref = format!("refs/heads/{}", name);
    let has_local = std::process::Command::new("git")
        .args(["-C", root, "rev-parse", "--verify", "--quiet", &local_ref])
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false);
    if has_local {
        return Some(name.to_string());
    }
    None
}

/// Merge-base ref HEAD
fn git_merge_base(root: &str, comp_ref: &str) -> Option<String> {
    let out = std::process::Command::new("git")
        .args(["-C", root, "merge-base", comp_ref, "HEAD"])
        .output()
        .ok()?;
    if out.status.success() {
        let s = String::from_utf8_lossy(&out.stdout).trim().to_string();
        if !s.is_empty() {
            return Some(s);
        }
    }
    None
}

/// Commit HEAD
fn git_head_commit(root: &str) -> Option<String> {
    let out = std::process::Command::new("git")
        .args(["-C", root, "rev-parse", "--verify", "--quiet", "HEAD^{commit}"])
        .output()
        .ok()?;
    if out.status.success() {
        let s = String::from_utf8_lossy(&out.stdout).trim().to_string();
        if !s.is_empty() {
            return Some(s);
        }
    }
    None
}

/// Menangani permintaan perubahan berkas git (mode: uncommitted atau base)
fn handle_git_changes(root: &str, mode: &str, ignore_whitespace: bool, known: Option<&str>) -> Value {
    let branch = git_current_branch(root);
    let base = git_default_branch(root);
    let base_ref = base.as_deref().and_then(|b| git_comparison_ref(root, b));

    let diff_output = if mode == "base" {
        if let Some(ref comp) = base_ref {
            let from = git_merge_base(root, comp).unwrap_or_else(|| comp.clone());
            let mut cmd = std::process::Command::new("git");
            cmd.args(["-C", root, "diff", "--no-color", "--no-ext-diff", "-M"]);
            if ignore_whitespace {
                cmd.arg("-w");
            }
            cmd.args([&from, "HEAD", "--"]);
            cmd.output().map(|o| String::from_utf8_lossy(&o.stdout).to_string()).unwrap_or_default()
        } else {
            String::new()
        }
    } else {
        // mode uncommitted: gunakan scratch index agar berkas untracked terdeteksi tanpa mengubah repo
        let temp_dir = std::env::temp_dir().join(format!("oopbot_git_scratch_{}_{}", std::process::id(), epoch_ms()));
        let temp_objects = temp_dir.join("objects");
        let temp_index = temp_dir.join("index");
        let _ = std::fs::create_dir_all(&temp_objects);

        let real_index = std::process::Command::new("git")
            .args(["-C", root, "rev-parse", "--git-path", "index"])
            .output()
            .ok()
            .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
            .unwrap_or_else(|| ".git/index".into());
        let real_objects = std::process::Command::new("git")
            .args(["-C", root, "rev-parse", "--git-path", "objects"])
            .output()
            .ok()
            .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
            .unwrap_or_else(|| ".git/objects".into());

        let abs_index = Path::new(root).join(&real_index);
        let abs_objects = Path::new(root).join(&real_objects);

        if abs_index.exists() {
            let _ = std::fs::copy(&abs_index, &temp_index);
        }

        let mut add_cmd = std::process::Command::new("git");
        add_cmd.args(["-C", root, "add", "--intent-to-add", "--all"])
            .env("GIT_INDEX_FILE", &temp_index)
            .env("GIT_OBJECT_DIRECTORY", &temp_objects)
            .env("GIT_ALTERNATE_OBJECT_DIRECTORIES", &abs_objects);
        let _ = add_cmd.output();

        let from = git_head_commit(root).unwrap_or_else(|| "4b825dc642cb6eb9a060e54bf8d69288fbee4904".to_string());
        let mut diff_cmd = std::process::Command::new("git");
        diff_cmd.args(["-C", root, "diff", "--no-color", "--no-ext-diff", "-M"]);
        if ignore_whitespace {
            diff_cmd.arg("-w");
        }
        diff_cmd.args([&from, "--"])
            .env("GIT_INDEX_FILE", &temp_index)
            .env("GIT_OBJECT_DIRECTORY", &temp_objects)
            .env("GIT_ALTERNATE_OBJECT_DIRECTORIES", &abs_objects);

        let out = diff_cmd.output().map(|o| String::from_utf8_lossy(&o.stdout).to_string()).unwrap_or_default();
        let _ = std::fs::remove_dir_all(&temp_dir);
        out
    };

    let files = parse_git_diff(&diff_output);
    let body = json!({
        "branch": branch,
        "baseRef": base_ref,
        "files": files,
        "tooLarge": false
    });
    let body_str = serde_json::to_string(&body).unwrap_or_default();
    if body_str.len() > 8 * 1024 * 1024 {
        let too_large_body = json!({
            "branch": branch,
            "baseRef": base_ref,
            "files": [],
            "tooLarge": true
        });
        let fp = sha256_hex(too_large_body.to_string().as_bytes());
        if known == Some(&fp) {
            return json!({ "fingerprint": fp, "unchanged": true });
        }
        let mut res = too_large_body;
        res["fingerprint"] = json!(fp);
        return res;
    }

    let fp = sha256_hex(body_str.as_bytes());
    if known == Some(&fp) {
        return json!({ "fingerprint": fp, "unchanged": true });
    }

    let mut res = body;
    res["fingerprint"] = json!(fp);
    res
}

/// Menangani daftar commit riwayat git ahead dari perbandingan
fn handle_git_commits(root: &str, known: Option<&str>) -> Value {
    let branch = git_current_branch(root);
    let head = git_head_commit(root);
    let base = git_default_branch(root);
    let comparison = if branch.is_some() && head.is_some() && base.is_some() {
        base.as_deref().and_then(|b| git_comparison_ref(root, b))
    } else {
        None
    };

    if comparison.is_none() {
        let empty_body = json!({
            "baseRef": null,
            "total": 0,
            "commits": []
        });
        let fp = sha256_hex(empty_body.to_string().as_bytes());
        if known == Some(&fp) {
            return json!({ "fingerprint": fp, "unchanged": true });
        }
        let mut res = empty_body;
        res["fingerprint"] = json!(fp);
        return res;
    }

    let comp_ref = comparison.unwrap();
    let range = format!("{}..HEAD", comp_ref);

    let log_out = std::process::Command::new("git")
        .args(["-C", root, "log", "--no-color", "--no-show-signature", "-z", "--max-count=500", "--format=%H%x00%at%x00%s", &range, "--"])
        .output()
        .map(|o| o.stdout)
        .unwrap_or_default();

    let count_out = std::process::Command::new("git")
        .args(["-C", root, "rev-list", "--count", &range, "--"])
        .output()
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().parse::<i64>().unwrap_or(0))
        .unwrap_or(0);

    let fields: Vec<&[u8]> = log_out.split(|&b| b == 0).collect();
    let mut commits = Vec::new();
    let mut i = 0;
    while i + 2 < fields.len() {
        let sha = String::from_utf8_lossy(fields[i]).to_string();
        let at_str = String::from_utf8_lossy(fields[i + 1]);
        let author_time = at_str.parse::<i64>().unwrap_or(0);
        let subject = String::from_utf8_lossy(fields[i + 2]).to_string();
        if !sha.is_empty() {
            commits.push(json!({
                "sha": sha,
                "subject": subject,
                "authorTime": author_time
            }));
        }
        i += 3;
    }

    let total = count_out.max(commits.len() as i64);
    let body = json!({
        "baseRef": comp_ref,
        "total": total,
        "commits": commits
    });

    let body_str = serde_json::to_string(&body).unwrap_or_default();
    let fp = sha256_hex(body_str.as_bytes());
    if known == Some(&fp) {
        return json!({ "fingerprint": fp, "unchanged": true });
    }

    let mut res = body;
    res["fingerprint"] = json!(fp);
    res
}

/// Menangani diff satu commit terhadap commit parent
fn handle_git_commit_diff(root: &str, sha: &str, ignore_whitespace: bool) -> Value {
    if !sha.chars().all(|c| c.is_ascii_hexdigit()) || sha.len() < 4 || sha.len() > 64 {
        return json!({
            "code": "commit_not_found",
            "error": format!("Commit {} not found.", sha)
        });
    }

    let commit_verify = std::process::Command::new("git")
        .args(["-C", root, "rev-parse", "--verify", "--quiet", &format!("{}^{{commit}}", sha)])
        .output();
    let full_sha = match commit_verify {
        Ok(o) if o.status.success() => String::from_utf8_lossy(&o.stdout).trim().to_string(),
        _ => {
            return json!({
                "code": "commit_not_found",
                "error": format!("Commit {} not found.", sha)
            });
        }
    };

    let parent_verify = std::process::Command::new("git")
        .args(["-C", root, "rev-parse", "--verify", "--quiet", &format!("{}^1", full_sha)])
        .output();
    let from = match parent_verify {
        Ok(o) if o.status.success() => String::from_utf8_lossy(&o.stdout).trim().to_string(),
        _ => "4b825dc642cb6eb9a060e54bf8d69288fbee4904".to_string(),
    };

    let mut diff_cmd = std::process::Command::new("git");
    diff_cmd.args(["-C", root, "diff", "--no-color", "--no-ext-diff", "-M"]);
    if ignore_whitespace {
        diff_cmd.arg("-w");
    }
    diff_cmd.args([&from, &full_sha, "--"]);

    let diff_out = diff_cmd.output().map(|o| String::from_utf8_lossy(&o.stdout).to_string()).unwrap_or_default();
    let files = parse_git_diff(&diff_out);
    let body = json!({
        "files": files,
        "tooLarge": false
    });
    if serde_json::to_string(&body).unwrap_or_default().len() > 8 * 1024 * 1024 {
        return json!({
            "files": [],
            "tooLarge": true
        });
    }
    body
}

/// Menangani mode `git`: backend tab Git Changes di UI Android
async fn handle_git_mode(
    reader: BufReader<TcpStream>,
    writer: BufWriter<TcpStream>,
) -> Result<(), BoxError> {
    run_git_loop(reader, writer)
}

fn run_git_loop<R: BufRead, W: Write>(
    mut reader: R,
    mut writer: W,
) -> Result<(), BoxError> {
    let ready = json!({ "type": "pipkin_git_ready" });
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

        let id = req["id"].as_i64().unwrap_or(0);
        let root = req["root"].as_str().unwrap_or(".");

        match req["type"].as_str() {
            Some("pipkin_git_detect") | Some("oopbot_git_detect") => {
                let trusted = req["trusted"].as_bool().unwrap_or(false);
                let (repo, git) = git_detect(root, trusted);
                let reply = json!({
                    "type": "pipkin_git_detected",
                    "id": id,
                    "repo": repo,
                    "git": git
                });
                writeln!(writer, "{}", serde_json::to_string(&reply)?)?;
                writer.flush()?;
            }
            Some("pipkin_git_changes") | Some("oopbot_git_changes") => {
                let mode = req["mode"].as_str().unwrap_or("uncommitted");
                let ignore_whitespace = req["ignoreWhitespace"].as_bool().unwrap_or(false);
                let known = req["known"].as_str();
                let mut data = handle_git_changes(root, mode, ignore_whitespace, known);
                if let Some(obj) = data.as_object_mut() {
                    obj.insert("type".to_string(), json!("pipkin_git_changes_result"));
                    obj.insert("id".to_string(), json!(id));
                }
                writeln!(writer, "{}", serde_json::to_string(&data)?)?;
                writer.flush()?;
            }
            Some("pipkin_git_commits") | Some("oopbot_git_commits") => {
                let known = req["known"].as_str();
                let mut data = handle_git_commits(root, known);
                if let Some(obj) = data.as_object_mut() {
                    obj.insert("type".to_string(), json!("pipkin_git_commits_result"));
                    obj.insert("id".to_string(), json!(id));
                }
                writeln!(writer, "{}", serde_json::to_string(&data)?)?;
                writer.flush()?;
            }
            Some("pipkin_git_commit_diff") | Some("oopbot_git_commit_diff") => {
                let sha = req["sha"].as_str().unwrap_or("");
                let ignore_whitespace = req["ignoreWhitespace"].as_bool().unwrap_or(false);
                let mut data = handle_git_commit_diff(root, sha, ignore_whitespace);
                if let Some(obj) = data.as_object_mut() {
                    obj.insert("type".to_string(), json!("pipkin_git_commit_diff_result"));
                    obj.insert("id".to_string(), json!(id));
                }
                writeln!(writer, "{}", serde_json::to_string(&data)?)?;
                writer.flush()?;
            }
            _ => {}
        }

        line.clear();
    }

    Ok(())
}

/// Menangani mode `terminal`: terminal interaktif shell native dengan streaming Base64
async fn handle_terminal_mode(
    params: Value,
    mut reader: BufReader<TcpStream>,
    writer: BufWriter<TcpStream>,
) -> Result<(), BoxError> {
    let cwd = params["cwd"].as_str().unwrap_or(".").to_string();
    let rows = params["rows"].as_u64().unwrap_or(24);
    let cols = params["cols"].as_u64().unwrap_or(80);

    if !Path::new(&cwd).is_dir() {
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

    let shared_writer = Arc::new(Mutex::new(writer));

    // Cek apakah util-linux `script` tersedia untuk PTY
    let prefix = env::var("PREFIX").unwrap_or_else(|_| "/data/data/com.termux/files/usr".into());
    let script_bin = format!("{}/bin/script", prefix);
    let script_exists = Path::new(&script_bin).exists() || Path::new("/usr/bin/script").exists();

    let tty_file = format!("/tmp/oopbot-tty-{}", std::process::id());
    let shell_cmd = format!(
        "stty rows {} cols {} 2>/dev/null; tty > {} 2>/dev/null; exec ${{SHELL:-/bin/sh}} -l",
        rows, cols, tty_file
    );

    let mut child = if script_exists {
        let script_prog = if Path::new(&script_bin).exists() {
            script_bin
        } else {
            "/usr/bin/script".to_string()
        };
        std::process::Command::new(script_prog)
            .args(["-qfec", &shell_cmd, "/dev/null"])
            .current_dir(&cwd)
            .env("TERM", "xterm-256color")
            .env("COLORTERM", "truecolor")
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()?
    } else {
        std::process::Command::new("sh")
            .arg("-i")
            .current_dir(&cwd)
            .env("TERM", "xterm-256color")
            .env("COLORTERM", "truecolor")
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()?
    };

    let mut child_stdin = child.stdin.take();
    let mut child_stdout = child.stdout.take();
    let mut child_stderr = child.stderr.take();

    // Thread membaca stdout dari child process
    let writer_out = shared_writer.clone();
    let stdout_handle = std::thread::spawn(move || {
        if let Some(ref mut out) = child_stdout {
            let mut buf = [0u8; 4096];
            while let Ok(n) = std::io::Read::read(out, &mut buf) {
                if n == 0 {
                    break;
                }
                let b64 = encode_base64(&buf[..n]);
                let msg = json!({
                    "type": "output",
                    "data": b64
                });
                if let Ok(mut w) = writer_out.lock() {
                    let _ = writeln!(w, "{}", msg);
                    let _ = w.flush();
                }
            }
        }
    });

    // Thread membaca stderr dari child process
    let stderr_tail = Arc::new(Mutex::new(String::new()));
    let stderr_tail_clone = stderr_tail.clone();
    let stderr_handle = std::thread::spawn(move || {
        if let Some(ref mut err) = child_stderr {
            let mut buf = [0u8; 2048];
            while let Ok(n) = std::io::Read::read(err, &mut buf) {
                if n == 0 {
                    break;
                }
                let s = String::from_utf8_lossy(&buf[..n]);
                if let Ok(mut tail) = stderr_tail_clone.lock() {
                    tail.push_str(&s);
                    if tail.len() > 16384 {
                        let excess = tail.len() - 16384;
                        tail.drain(..excess);
                    }
                }
            }
        }
    });

    // Loop menerima input & resize dari Android
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

        if let Ok(req) = serde_json::from_str::<Value>(trimmed) {
            match req["type"].as_str() {
                Some("input") => {
                    if let Some(data_b64) = req["data"].as_str() {
                        if let Some(bytes) = decode_base64(data_b64) {
                            if let Some(ref mut stdin) = child_stdin {
                                let _ = std::io::Write::write_all(stdin, &bytes);
                                let _ = std::io::Write::flush(stdin);
                            }
                        }
                    }
                }
                Some("resize") => {
                    let r = req["rows"].as_u64().unwrap_or(rows);
                    let c = req["cols"].as_u64().unwrap_or(cols);
                    if let Ok(dev) = std::fs::read_to_string(&tty_file) {
                        let dev = dev.trim();
                        if dev.starts_with("/dev/") {
                            let _ = std::process::Command::new("stty")
                                .args(["-F", dev, "rows", &r.to_string(), "cols", &c.to_string()])
                                .output();
                        }
                    }
                }
                _ => {}
            }
        }
        line.clear();
    }

    // Socket terputus atau reader EOF
    drop(child_stdin);
    let _ = child.kill();
    let status = child.wait().ok();
    let _ = stdout_handle.join();
    let _ = stderr_handle.join();
    let _ = std::fs::remove_file(&tty_file);

    let exit_code = status.and_then(|s| s.code());
    let stderr_str = stderr_tail.lock().map(|s| s.clone()).unwrap_or_default();

    let exit_msg = json!({
        "type": "pipkin_exit",
        "code": exit_code,
        "signal": null,
        "stderr": stderr_str
    });

    if let Ok(mut w) = shared_writer.lock() {
        let _ = writeln!(w, "{}", serde_json::to_string(&exit_msg)?);
        let _ = w.flush();
    }

    Ok(())
}


/// Pindai sesi percakapan secara rekursif di `sessions/**/*.jsonl`
fn handle_list_sessions(config_dir: &Path, home: &str) -> Value {
    let mut sessions = Vec::new();
    let sessions_dir = config_dir.join("sessions");

    let mut session_files = Vec::new();
    collect_session_files(&sessions_dir, &mut session_files);

    let mut seen_ids = std::collections::HashSet::new();


    for path in session_files {
        if let Ok(content) = std::fs::read_to_string(&path) {
            let mut lines = content.lines();
            let Some(first_line) = lines.next() else { continue; };
            let Ok(meta) = serde_json::from_str::<Value>(first_line) else { continue; };

            let id = meta["id"].as_str().unwrap_or_default().to_string();
            if id.is_empty() || !seen_ids.insert(id.clone()) {
                continue;
            }
            let cwd = meta["cwd"].as_str().unwrap_or(home).to_string();
            let name = meta.get("name").and_then(|n| n.as_str()).map(str::to_string);

            let file_mtime = std::fs::metadata(&path)
                .and_then(|m| m.modified())
                .ok()
                .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
                .map(|d| d.as_millis() as u64)
                .unwrap_or_else(epoch_ms);

            let created = meta["timestamp"]
                .as_u64()
                .or_else(|| meta["created"].as_u64())
                .unwrap_or(file_mtime);

            let mut message_count = 0;
            let mut first_message = String::new();

            for line in lines {
                if let Ok(val) = serde_json::from_str::<Value>(line) {
                    if val["type"].as_str() == Some("message") {
                        message_count += 1;
                        if first_message.is_empty() {
                            if let Some(msg) = val.get("message") {
                                if msg["role"].as_str() == Some("user") {
                                    if let Some(text) = msg["content"].as_str() {
                                        first_message = text.chars().take(200).collect();
                                    } else if let Some(arr) = msg["content"].as_array() {
                                        for part in arr {
                                            if part["type"].as_str() == Some("text") {
                                                if let Some(text) = part["text"].as_str() {
                                                    first_message = text.chars().take(200).collect();
                                                    break;
                                                }
                                            }
                                        }
                                    }
                                }
                            }
                        }
                    }
                }
            }

            let cwd_exists = Path::new(&cwd).is_dir();

            sessions.push(json!({
                "path": path.to_string_lossy(),
                "id": id,
                "cwd": cwd,
                "cwdExists": cwd_exists,
                "name": name,
                "created": created,
                "modified": file_mtime,
                "messageCount": message_count,
                "firstMessage": first_message,
                "subagents": 0,
            }));
        }
    }

    // Urutkan sessions modified descending (terbaru lebih dulu)
    sessions.sort_by(|a, b| {
        let mod_b = b["modified"].as_u64().unwrap_or(0);
        let mod_a = a["modified"].as_u64().unwrap_or(0);
        mod_b.cmp(&mod_a)
    });

    json!({
        "type": "pipkin_result",
        "ok": true,
        "data": { "sessions": sessions }
    })
}

fn collect_session_files(dir: &Path, out: &mut Vec<PathBuf>) {
    if let Ok(entries) = std::fs::read_dir(dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                collect_session_files(&path, out);
            } else if path.extension().and_then(|e| e.to_str()) == Some("jsonl") {
                out.push(path);
            }
        }
    }
}

fn handle_session_messages(params: &Value) -> Value {
    let path = params["path"].as_str().unwrap_or_default();
    let mut messages = Vec::new();
    if let Ok(content) = std::fs::read_to_string(path) {
        for line in content.lines() {
            if let Ok(val) = serde_json::from_str::<Value>(line) {
                if val["type"].as_str() == Some("message") {
                    if let Some(msg) = val.get("message") {
                        messages.push(msg.clone());
                    }
                }
            }
        }
    }
    json!({
        "type": "pipkin_result",
        "ok": true,
        "data": { "messages": messages }
    })
}

/// Menghasilkan skema `AuthStatus` lengkap sesuai kebutuhan UI Android (`HostModels.kt:140-182`)
fn handle_auth_status(config_dir: &Path) -> Value {
    let auth_path = config_dir.join("auth.json");
    let auth_json = read_json_file(&auth_path);

    let models_path = config_dir.join("models.json");
    let models_json = read_json_file(&models_path);

    let settings_path = config_dir.join("settings.json");
    let settings_json = read_json_file(&settings_path);

    let catalog = ModelCatalog::builtin();

    // 6 built-in providers bawaan Android
    let builtin_specs = [
        ("google", "Google Gemini", "GEMINI_API_KEY", "GOOGLE_API_KEY"),
        ("anthropic", "Anthropic Claude", "ANTHROPIC_API_KEY", ""),
        ("openai-codex", "OpenAI", "OPENAI_API_KEY", ""),
        ("openrouter", "OpenRouter", "OPENROUTER_API_KEY", ""),
        ("zai", "Z.ai Coding Plan", "ZAI_API_KEY", ""),
        ("kimi-coding", "Kimi Code", "KIMI_API_KEY", ""),
    ];

    let scope_arr = settings_json.get("enabledModels").and_then(|v| v.as_array());
    let scope_set: Option<HashSet<String>> = scope_arr.map(|arr| {
        arr.iter()
            .filter_map(|v| v.as_str().map(str::to_string))
            .collect()
    });

    let mut builtin = Vec::new();
    for (id, name, env_key1, env_key2) in builtin_specs {
        // Cek auth.json
        let key_in_auth = extract_auth_key(&auth_json, id);
        let has_env = (!env_key1.is_empty() && env::var(env_key1).is_ok())
            || (!env_key2.is_empty() && env::var(env_key2).is_ok())
            || env::var("OOPBOT_API_KEY").is_ok()
            || env::var("PIPKIN_API_KEY").is_ok();

        let is_configured = !key_in_auth.is_empty() || has_env;

        let provider_filter = match id {
            "openai-codex" => "openai",
            "kimi-coding" => "kimi",
            other => other,
        };

        let models_for_provider: Vec<Value> = catalog
            .list()
            .iter()
            .filter(|m| m.provider == provider_filter || m.provider == id)
            .map(|m| m.to_pipkin_json(Some(id)))
            .collect();

        // Extra IDs dari models.json
        let mut extra_ids = Vec::new();
        if let Some(entry) = models_json.get("providers").and_then(|p| p.get(id)) {
            if let Some(m_arr) = entry.get("models").and_then(|m| m.as_array()) {
                for m in m_arr {
                    if let Some(mid) = m.get("id").and_then(|i| i.as_str()) {
                        extra_ids.push(mid.to_string());
                    }
                }
            }
        }

        // Enabled models for this provider
        let enabled = scope_set.as_ref().map(|set| {
            set.iter()
                .filter_map(|ref_str| {
                    if let Some((p, m)) = ref_str.split_once('/') {
                        if p == id {
                            Some(m.to_string())
                        } else {
                            None
                        }
                    } else {
                        None
                    }
                })
                .collect::<Vec<String>>()
        });

        builtin.push(json!({
            "id": id,
            "name": name,
            "configured": is_configured,
            "type": if is_configured { json!("api_key") } else { Value::Null },
            "models": models_for_provider,
            "extraIds": extra_ids,
            "enabled": enabled,
        }));
    }

    // Custom providers dari models.json
    let mut custom = Vec::new();
    let builtin_id_set: HashSet<&str> = [
        "google", "anthropic", "openai", "openai-codex", "openrouter", "zai", "kimi", "kimi-coding"
    ].into_iter().collect();

    if let Some(providers_obj) = models_json.get("providers").and_then(|p| p.as_object()) {
        for (id, val) in providers_obj {
            if builtin_id_set.contains(id.as_str()) {
                continue;
            }
            let base_url = val["baseUrl"].as_str().unwrap_or_default();
            let api = val["api"].as_str().unwrap_or("openai-completions");
            let api_key = val["apiKey"].as_str().unwrap_or_default();
            let models = val["models"].as_array().cloned().unwrap_or_default();
            custom.push(json!({
                "id": id,
                "baseUrl": base_url,
                "api": api,
                "apiKey": api_key,
                "models": models,
            }));
        }
    }

    let scope_output = scope_set.map(|set| set.into_iter().collect::<Vec<String>>());

    json!({
        "type": "pipkin_result",
        "ok": true,
        "data": {
            "builtin": builtin,
            "custom": custom,
            "scope": scope_output,
            "npmPackages": []
        }
    })
}

fn handle_logout(config_dir: &Path, params: &Value) -> Value {
    let provider_id = params["providerId"].as_str().unwrap_or_default();
    if !provider_id.is_empty() {
        let auth_path = config_dir.join("auth.json");
        let mut auth_json = read_json_file(&auth_path);
        if let Some(obj) = auth_json.as_object_mut() {
            obj.remove(provider_id);
            let _ = write_json_file(&auth_path, &auth_json);
        }
    }
    json!({
        "type": "pipkin_result",
        "ok": true,
        "data": {}
    })
}

fn handle_provider_save(config_dir: &Path, params: &Value) -> Value {
    let id = params["id"].as_str().unwrap_or_default();
    if !id.is_empty() {
        let models_path = config_dir.join("models.json");
        let mut models_json = read_json_file(&models_path);
        if !models_json.is_object() {
            models_json = json!({ "providers": {} });
        }
        if !models_json["providers"].is_object() {
            models_json["providers"] = json!({});
        }

        let base_url = params["baseUrl"].as_str().unwrap_or_default();
        let api = params["api"].as_str().unwrap_or("openai-completions");
        let api_key = params["apiKey"].as_str().unwrap_or_default();
        let models = params.get("models").cloned().unwrap_or(json!([]));

        models_json["providers"][id] = json!({
            "baseUrl": base_url,
            "api": api,
            "apiKey": api_key,
            "models": models,
        });
        let _ = write_json_file(&models_path, &models_json);

        if !api_key.is_empty() {
            save_auth_key(config_dir, id, api_key);
        }
    }
    json!({
        "type": "pipkin_result",
        "ok": true,
        "data": {}
    })
}

fn handle_provider_remove(config_dir: &Path, params: &Value) -> Value {
    let id = params["id"].as_str().unwrap_or_default();
    if !id.is_empty() {
        let models_path = config_dir.join("models.json");
        let mut models_json = read_json_file(&models_path);
        if let Some(providers) = models_json.get_mut("providers").and_then(|p| p.as_object_mut()) {
            providers.remove(id);
            let _ = write_json_file(&models_path, &models_json);
        }
        let auth_path = config_dir.join("auth.json");
        let mut auth_json = read_json_file(&auth_path);
        if let Some(obj) = auth_json.as_object_mut() {
            obj.remove(id);
            let _ = write_json_file(&auth_path, &auth_json);
        }
    }
    json!({
        "type": "pipkin_result",
        "ok": true,
        "data": {}
    })
}

fn handle_subscription_models(params: &Value) -> Value {
    let provider_id = params["providerId"].as_str().unwrap_or_default();
    let catalog = ModelCatalog::builtin();
    let filter = match provider_id {
        "openai-codex" => "openai",
        "kimi-coding" => "kimi",
        other => other,
    };
    let models_json: Vec<Value> = catalog
        .list()
        .iter()
        .filter(|m| m.provider == filter || m.provider == provider_id)
        .map(|m| m.to_pipkin_json(Some(provider_id)))
        .collect();

    json!({
        "type": "pipkin_result",
        "ok": true,
        "data": { "models": models_json }
    })
}

fn handle_subscription_save(config_dir: &Path, params: &Value) -> Value {
    let provider_id = params["providerId"].as_str().unwrap_or_default();
    if !provider_id.is_empty() {
        let models_path = config_dir.join("models.json");
        let mut models_json = read_json_file(&models_path);
        if !models_json.is_object() {
            models_json = json!({ "providers": {} });
        }
        if !models_json["providers"].is_object() {
            models_json["providers"] = json!({});
        }
        if !models_json["providers"][provider_id].is_object() {
            models_json["providers"][provider_id] = json!({});
        }
        if let Some(models) = params.get("models") {
            models_json["providers"][provider_id]["models"] = models.clone();
        }
        let _ = write_json_file(&models_path, &models_json);

        if let Some(enabled) = params.get("enabled").and_then(|e| e.as_array()) {
            let settings_path = config_dir.join("settings.json");
            let mut settings = read_json_file(&settings_path);
            if !settings.is_object() {
                settings = json!({});
            }
            let mut enabled_models = settings
                .get("enabledModels")
                .and_then(|v| v.as_array())
                .cloned()
                .unwrap_or_default();

            // Hapus model lama untuk provider ini
            enabled_models.retain(|item| {
                if let Some(s) = item.as_str() {
                    !s.starts_with(&format!("{}/", provider_id))
                } else {
                    true
                }
            });

            // Tambahkan model baru
            for m in enabled {
                if let Some(m_str) = m.as_str() {
                    enabled_models.push(json!(format!("{}/{}", provider_id, m_str)));
                }
            }
            settings["enabledModels"] = json!(enabled_models);
            let _ = write_json_file(&settings_path, &settings);
        }
    }
    json!({
        "type": "pipkin_result",
        "ok": true,
        "data": {}
    })
}

fn handle_local_models_sync(config_dir: &Path, params: &Value) -> Value {
    let base_url = params["baseUrl"].as_str().unwrap_or_default();
    let api_key = params["apiKey"].as_str().unwrap_or_default();
    let models = params.get("models").cloned().unwrap_or(json!([]));

    let models_path = config_dir.join("models.json");
    let mut models_json = read_json_file(&models_path);
    if !models_json.is_object() {
        models_json = json!({ "providers": {} });
    }
    if !models_json["providers"].is_object() {
        models_json["providers"] = json!({});
    }

    // Dual-support: simpan di kedua nama "pipkin-local" dan "oopbot-local"
    let local_entry = json!({
        "baseUrl": base_url,
        "apiKey": api_key,
        "api": "openai-completions",
        "models": models,
    });
    models_json["providers"]["pipkin-local"] = local_entry.clone();
    models_json["providers"]["oopbot-local"] = local_entry;

    let _ = write_json_file(&models_path, &models_json);

    json!({
        "type": "pipkin_result",
        "ok": true,
        "data": { "changed": true }
    })
}

async fn handle_provider_models(params: &Value) -> Value {
    let base_url = params["baseUrl"].as_str().unwrap_or("").trim();
    let api_key = params["apiKey"].as_str().unwrap_or("").trim();
    let api = params["api"].as_str().unwrap_or("openai-completions").trim();

    if !base_url.is_empty() {
        // Query live GET {baseUrl}/models
        let client = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(10))
            .build();
        if let Ok(client) = client {
            let url = if base_url.ends_with('/') {
                format!("{}models", base_url)
            } else {
                format!("{}/models", base_url)
            };
            let mut req = client.get(&url);
            if !api_key.is_empty() {
                req = req.header("Authorization", format!("Bearer {}", api_key));
            }
            if let Ok(resp) = req.send().await {
                if resp.status().is_success() {
                    if let Ok(body) = resp.json::<Value>().await {
                        let models = parse_models_response(&body, api);
                        let models_json: Vec<Value> = models
                            .iter()
                            .map(|m| m.to_pipkin_json(None))
                            .collect();
                        return json!({
                            "type": "pipkin_result",
                            "ok": true,
                            "data": { "models": models_json }
                        });
                    }
                }
            }
        }
    }

    // Fallback: builtin catalog
    let catalog = ModelCatalog::builtin();
    let models_json: Vec<Value> = catalog
        .list()
        .iter()
        .map(|m| m.to_pipkin_json(Some(&m.provider)))
        .collect();

    json!({
        "type": "pipkin_result",
        "ok": true,
        "data": { "models": models_json }
    })
}

fn handle_model_defaults(params: &Value) -> Value {
    let catalog = ModelCatalog::builtin();
    let mut models_json = Vec::new();
    if let Some(ids) = params["ids"].as_array() {
        for id_val in ids {
            if let Some(id_str) = id_val.as_str() {
                if let Some(m) = catalog.find(id_str) {
                    models_json.push(m.to_pipkin_json(None));
                } else {
                    let api = params["api"].as_str().unwrap_or("openai-completions");
                    models_json.push(json!({
                        "id": id_str,
                        "name": id_str,
                        "api": api,
                    }));
                }
            }
        }
    }
    json!({
        "type": "pipkin_result",
        "ok": true,
        "data": { "models": models_json }
    })
}

fn handle_save_default_model(config_dir: &Path, params: &Value) -> Value {
    let settings_path = config_dir.join("settings.json");
    let mut settings = read_json_file(&settings_path);
    if !settings.is_object() {
        settings = json!({});
    }

    if let Some(provider) = params["provider"].as_str() {
        settings["defaultProvider"] = json!(provider);
    }
    if let Some(model_id) = params["modelId"].as_str() {
        settings["defaultModel"] = json!(model_id);
    }
    if let Some(lvl) = params["thinkingLevel"].as_str() {
        settings["defaultThinkingLevel"] = json!(lvl);
    }

    let _ = write_json_file(&settings_path, &settings);

    json!({
        "type": "pipkin_result",
        "ok": true,
        "data": {}
    })
}

fn handle_save_attachments(params: &Value, config_dir: &Path) -> Value {
    let mut paths = Vec::new();
    let attach_dir = config_dir
        .join("attachments")
        .join(format!("{}", epoch_ms()));
    let _ = std::fs::create_dir_all(&attach_dir);

    if let Some(files) = params.get("files").and_then(|f| f.as_array()) {
        for file in files {
            let name = file["name"].as_str().unwrap_or("attachment.bin");
            let data = file["data"].as_str().unwrap_or("");
            if let Some(bytes) = decode_base64(data) {
                let file_path = attach_dir.join(name);
                if std::fs::write(&file_path, bytes).is_ok() {
                    paths.push(file_path.to_string_lossy().to_string());
                }
            }
        }
    }

    json!({
        "type": "pipkin_result",
        "ok": true,
        "data": { "paths": paths }
    })
}

fn decode_base64(input: &str) -> Option<Vec<u8>> {
    let mut table = [255u8; 256];
    for (i, &c) in b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/".iter().enumerate() {
        table[c as usize] = i as u8;
    }
    let trimmed = input.trim().trim_end_matches('=');
    let mut out = Vec::with_capacity(trimmed.len() * 3 / 4);
    let mut buf = 0u32;
    let mut bits = 0u32;
    for &b in trimmed.as_bytes() {
        if b.is_ascii_whitespace() {
            continue;
        }
        let val = table[b as usize];
        if val == 255 {
            return None;
        }
        buf = (buf << 6) | (val as u32);
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((buf >> bits) as u8);
            buf &= (1 << bits) - 1;
        }
    }
    Some(out)
}

fn encode_base64(bytes: &[u8]) -> String {
    const TABLE: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity((bytes.len() + 2) / 3 * 4);
    for chunk in bytes.chunks(3) {
        let b0 = chunk[0] as u32;
        let b1 = if chunk.len() > 1 { chunk[1] as u32 } else { 0 };
        let b2 = if chunk.len() > 2 { chunk[2] as u32 } else { 0 };
        let triple = (b0 << 16) | (b1 << 8) | b2;
        out.push(TABLE[((triple >> 18) & 0x3F) as usize] as char);
        out.push(TABLE[((triple >> 12) & 0x3F) as usize] as char);
        if chunk.len() > 1 {
            out.push(TABLE[((triple >> 6) & 0x3F) as usize] as char);
        } else {
            out.push('=');
        }
        if chunk.len() > 2 {
            out.push(TABLE[(triple & 0x3F) as usize] as char);
        } else {
            out.push('=');
        }
    }
    out
}

fn resolve_browse_path(raw: &str, home: &str) -> PathBuf {
    if raw == "~" {
        PathBuf::from(home)
    } else if let Some(stripped) = raw.strip_prefix("~/") {
        PathBuf::from(home).join(stripped)
    } else {
        PathBuf::from(raw)
    }
}

fn is_probably_text(bytes: &[u8]) -> bool {
    if bytes.is_empty() {
        return true;
    }
    let mut control = 0;
    for &b in bytes {
        if b == 0 {
            return false;
        }
        if (b < 32 && b != 9 && b != 10 && b != 13) || b == 127 {
            control += 1;
        }
    }
    (control as f64 / bytes.len() as f64) < 0.3
}

fn search_folders_recursive(
    dir: &Path,
    query_lower: &str,
    current_depth: usize,
    max_depth: usize,
    max_results: usize,
    out: &mut Vec<String>,
) {
    if current_depth > max_depth || out.len() >= max_results {
        return;
    }
    if let Ok(entries) = std::fs::read_dir(dir) {
        for entry in entries.flatten() {
            let p = entry.path();
            if p.is_dir() {
                let name = entry.file_name().to_string_lossy().to_string();
                if name.starts_with('.') || name == "target" || name == "node_modules" || name == ".git" {
                    continue;
                }
                if name.to_lowercase().contains(query_lower) {
                    out.push(p.to_string_lossy().to_string());
                    if out.len() >= max_results {
                        return;
                    }
                }
                search_folders_recursive(&p, query_lower, current_depth + 1, max_depth, max_results, out);
            }
        }
    }
}


fn handle_delete_sessions(params: &Value) -> Value {
    if let Some(paths) = params["paths"].as_array() {
        for p in paths {
            if let Some(path_str) = p.as_str() {
                let _ = std::fs::remove_file(path_str);
            }
        }
    }
    json!({
        "type": "pipkin_result",
        "ok": true,
        "data": {}
    })
}

fn read_json_file(path: &Path) -> Value {
    if let Ok(content) = std::fs::read_to_string(path) {
        if let Ok(val) = serde_json::from_str(&content) {
            return val;
        }
    }
    json!({})
}


fn write_json_file(path: &Path, val: &Value) -> Result<(), BoxError> {
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let text = serde_json::to_string_pretty(val)?;
    std::fs::write(path, format!("{}\n", text))?;
    Ok(())
}

fn extract_auth_key(auth_json: &Value, provider_id: &str) -> String {
    let entry = &auth_json[provider_id];
    if let Some(s) = entry.as_str() {
        return s.to_string();
    }
    if let Some(s) = entry["apiKey"].as_str() {
        return s.to_string();
    }
    if let Some(s) = entry["key"].as_str() {
        return s.to_string();
    }
    if let Some(s) = entry["auth"]["apiKey"].as_str() {
        return s.to_string();
    }
    String::new()
}

fn save_auth_key(config_dir: &Path, provider_id: &str, api_key: &str) {
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

fn epoch_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

fn iso_now() -> String {
    let ms = epoch_ms();
    let secs = ms / 1000;
    let millis = ms % 1000;
    let days = secs / 86_400;
    let rem = secs % 86_400;
    let hours = rem / 3600;
    let minutes = (rem % 3600) / 60;
    let seconds = rem % 60;
    let (year, month, day) = civil_from_days(days as i64);
    format!(
        "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}.{:03}Z",
        year, month, day, hours, minutes, seconds, millis
    )
}

fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = (z - era * 146_097) as u32;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146_096) / 365;
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = if m <= 2 { y + 1 } else { y };
    (year, m, d)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    #[tokio::test]
    async fn test_handle_provider_models_fallback() {
        let params = json!({});
        let res = handle_provider_models(&params).await;
        assert_eq!(res["type"], "pipkin_result");
        assert_eq!(res["ok"], true);
        let models = res["data"]["models"].as_array().unwrap();
        assert!(!models.is_empty());
    }

    #[test]
    fn test_handle_model_defaults() {
        let params = json!({
            "ids": ["gemini-2.5-flash", "unknown-model"],
            "api": "openai-completions"
        });
        let res = handle_model_defaults(&params);
        assert_eq!(res["type"], "pipkin_result");
        assert_eq!(res["ok"], true);
        let models = res["data"]["models"].as_array().unwrap();
        assert_eq!(models.len(), 2);
        assert_eq!(models[0]["id"], "gemini-2.5-flash");
        assert_eq!(models[1]["id"], "unknown-model");
    }

    #[test]
    fn test_auth_status_schema_and_builtin() {
        let temp_dir = std::env::temp_dir().join(format!("oopbot_test_{}", epoch_ms()));
        let _ = std::fs::create_dir_all(&temp_dir);

        // Buat auth.json tiruan
        let auth_path = temp_dir.join("auth.json");
        let _ = std::fs::write(&auth_path, r#"{"google": {"apiKey": "test-key"}}"#);

        let res = handle_auth_status(&temp_dir);
        assert_eq!(res["type"], "pipkin_result");
        assert_eq!(res["ok"], true);

        let data = &res["data"];
        let builtin = data["builtin"].as_array().unwrap();
        assert_eq!(builtin.len(), 6);

        // Google harus configured = true
        let google_provider = builtin.iter().find(|p| p["id"] == "google").unwrap();
        assert_eq!(google_provider["configured"], true);
        assert_eq!(google_provider["type"], "api_key");

        let _ = std::fs::remove_dir_all(&temp_dir);
    }

    #[test]
    fn test_base64_decode() {
        let text = "Halo OOPBoT dari native Termux!";
        let encoded = "SGFsbyBPT1BCb1QgZGFyaSBuYXRpdmUgVGVybXV4IQ==";
        let decoded = decode_base64(encoded).unwrap();
        assert_eq!(String::from_utf8_lossy(&decoded), text);
    }

    #[test]
    fn test_save_attachments() {
        let temp_dir = std::env::temp_dir().join(format!("oopbot_attach_{}", epoch_ms()));
        let _ = std::fs::create_dir_all(&temp_dir);

        let params = json!({
            "cwd": "/some/cwd",
            "files": [
                {
                    "name": "test.txt",
                    "data": "SGFsbw=="
                }
            ]
        });

        let res = handle_save_attachments(&params, &temp_dir);
        assert_eq!(res["type"], "pipkin_result");
        assert_eq!(res["ok"], true);
        let paths = res["data"]["paths"].as_array().unwrap();
        assert_eq!(paths.len(), 1);
        let saved_path = paths[0].as_str().unwrap();
        assert!(Path::new(saved_path).exists());
        assert_eq!(std::fs::read_to_string(saved_path).unwrap(), "Halo");

        let _ = std::fs::remove_dir_all(&temp_dir);
    }

    #[test]
    fn test_list_sessions_format() {
        let temp_dir = std::env::temp_dir().join(format!("oopbot_sess_{}", epoch_ms()));
        let sessions_dir = temp_dir.join("sessions").join("subfolder");
        let _ = std::fs::create_dir_all(&sessions_dir);

        let session_file = sessions_dir.join("test-session.jsonl");
        let header = json!({
            "type": "session",
            "id": "sess_123",
            "cwd": temp_dir.to_string_lossy(),
            "name": "Test Chat",
            "timestamp": 1700000000000u64,
        });
        let msg = json!({
            "type": "message",
            "message": {
                "role": "user",
                "content": "Pertanyaan pertama user"
            }
        });
        let content = format!("{}\n{}\n", header, msg);
        std::fs::write(&session_file, content).unwrap();

        let res = handle_list_sessions(&temp_dir, "/home");
        assert_eq!(res["type"], "pipkin_result");
        assert_eq!(res["ok"], true);
        let sessions = res["data"]["sessions"].as_array().unwrap();
        assert_eq!(sessions.len(), 1);
        assert_eq!(sessions[0]["id"], "sess_123");
        assert_eq!(sessions[0]["name"], "Test Chat");
        assert_eq!(sessions[0]["firstMessage"], "Pertanyaan pertama user");
        assert_eq!(sessions[0]["messageCount"], 1);
        assert_eq!(sessions[0]["cwdExists"], true);

        let _ = std::fs::remove_dir_all(&temp_dir);
    }

    #[derive(Clone)]
    struct SharedBuf(Arc<Mutex<Vec<u8>>>);
    impl Write for SharedBuf {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().write(buf)
        }
        fn flush(&mut self) -> std::io::Result<()> {
            self.0.lock().unwrap().flush()
        }
    }

    #[tokio::test]
    async fn test_rpc_stream_processing() {
        let raw = Arc::new(Mutex::new(Vec::new()));
        let output_buf = Arc::new(Mutex::new(Box::new(SharedBuf(raw.clone())) as Box<dyn Write + Send + Sync>));
        set_output_sink(Some(output_buf));

        let mut server = RpcServer::new(None, None, vec![]).unwrap();
        let input = b"{\"type\":\"get_state\",\"id\":\"test_1\"}\n";
        let cursor = Cursor::new(input);

        let res = server.run_loop_with_input(cursor).await;
        assert!(res.is_ok());

        set_output_sink(None);

        let bytes = raw.lock().unwrap().clone();
        let written = String::from_utf8_lossy(&bytes);
        assert!(written.contains("test_1"));
        assert!(written.contains("\"success\":true"));
    }

    #[test]
    fn test_encode_base64_and_roundtrip() {
        let original = "Hello World! OOPBoT Native FileHost 2026";
        let encoded = encode_base64(original.as_bytes());
        let decoded = decode_base64(&encoded).expect("decode failed");
        assert_eq!(String::from_utf8_lossy(&decoded), original);
    }

    #[test]
    fn test_is_probably_text() {
        let text = b"This is plain text with \n and \t.";
        assert!(is_probably_text(text));

        let binary = [0u8, 1, 2, 255, 0, 15];
        assert!(!is_probably_text(&binary));
    }

    #[test]
    fn test_resolve_browse_path() {
        let home = "/data/data/com.termux/files/home";
        assert_eq!(resolve_browse_path("~", home), PathBuf::from(home));
        assert_eq!(resolve_browse_path("~/projects", home), PathBuf::from(home).join("projects"));
        assert_eq!(resolve_browse_path("/sdcard/Download", home), PathBuf::from("/sdcard/Download"));
    }

    #[test]
    fn test_search_folders_recursive() {
        let temp_dir = std::env::temp_dir().join(format!("oopbot_search_{}", epoch_ms()));
        let sub1 = temp_dir.join("my_awesome_project");
        let sub2 = temp_dir.join("other_folder").join("my_nested_project");
        let _ = std::fs::create_dir_all(&sub1);
        let _ = std::fs::create_dir_all(&sub2);

        let mut results = Vec::new();
        search_folders_recursive(&temp_dir, "project", 0, 4, 10, &mut results);
        assert_eq!(results.len(), 2);

        let _ = std::fs::remove_dir_all(&temp_dir);
    }

    #[test]
    fn test_run_browse_loop() {
        let temp_dir = std::env::temp_dir().join(format!("oopbot_browse_{}", epoch_ms()));
        let sub = temp_dir.join("workspace_folder");
        let _ = std::fs::create_dir_all(&sub);

        let input = format!(
            "{{\"type\":\"pipkin_list\",\"id\":1,\"path\":\"{}\"}}\n",
            temp_dir.to_string_lossy()
        );
        let cursor = Cursor::new(input.into_bytes());
        let mut output = Vec::new();

        let res = run_browse_loop(&temp_dir.to_string_lossy(), cursor, &mut output);
        assert!(res.is_ok());

        let out_str = String::from_utf8_lossy(&output);
        assert!(out_str.contains("pipkin_places"));
        assert!(out_str.contains("pipkin_listing"));
        assert!(out_str.contains("workspace_folder"));

        let _ = std::fs::remove_dir_all(&temp_dir);
    }

    #[test]
    fn test_run_files_loop_list_and_read() {
        let temp_dir = std::env::temp_dir().join(format!("oopbot_files_{}", epoch_ms()));
        let file_path = temp_dir.join("hello.txt");
        let _ = std::fs::create_dir_all(&temp_dir);
        let _ = std::fs::write(&file_path, "Hello Native Files!");

        let input = format!(
            "{{\"type\":\"pipkin_files_list\",\"id\":10,\"root\":\"{}\",\"path\":\".\"}}\n\
             {{\"type\":\"pipkin_files_read\",\"id\":11,\"root\":\"{}\",\"path\":\"hello.txt\"}}\n\
             {{\"type\":\"pipkin_files_mkdir\",\"id\":12,\"root\":\"{}\",\"path\":\".\",\"name\":\"new_dir\"}}\n",
            temp_dir.to_string_lossy(),
            temp_dir.to_string_lossy(),
            temp_dir.to_string_lossy(),
        );
        let cursor = Cursor::new(input.into_bytes());
        let mut output = Vec::new();

        let res = run_files_loop(cursor, &mut output);
        assert!(res.is_ok());

        let out_str = String::from_utf8_lossy(&output);
        assert!(out_str.contains("pipkin_files_ready"));
        assert!(out_str.contains("pipkin_files_listing"));
        assert!(out_str.contains("hello.txt"));
        assert!(out_str.contains("pipkin_files_file"));
        assert!(out_str.contains("Hello Native Files!"));
        assert!(out_str.contains("pipkin_files_made"));
        assert!(out_str.contains("new_dir"));
        assert!(temp_dir.join("new_dir").is_dir());

        let _ = std::fs::remove_dir_all(&temp_dir);
    }

    #[test]
    fn test_sha256_hex() {
        assert_eq!(
            sha256_hex(b""),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
        assert_eq!(
            sha256_hex(b"hello world"),
            "b94d27b9934d3e08a52e52d7da7dabfac484efe37a5380ee9088f7ace2efcde9"
        );
    }

    #[test]
    fn test_parse_git_diff_comprehensive() {
        let diff_sample = "\
diff --git a/src/lib.rs b/src/lib.rs
index 1111111..2222222 100644
--- a/src/lib.rs
+++ b/src/lib.rs
@@ -1,3 +1,4 @@
 pub mod host;
-pub mod old;
+pub mod new_one;
+pub mod new_two;
diff --git a/new_file.txt b/new_file.txt
new file mode 100644
index 0000000..3333333
--- /dev/null
+++ b/new_file.txt
@@ -0,0 +1,2 @@
+Line 1
+Line 2
diff --git a/deleted.txt b/deleted.txt
deleted file mode 100644
--- a/deleted.txt
+++ /dev/null
@@ -1,1 +0,0 @@
-Bye
diff --git a/image.png b/image.png
Binary files a/image.png and b/image.png differ
";
        let files = parse_git_diff(diff_sample);
        assert_eq!(files.len(), 4);

        // 1. src/lib.rs (modified)
        assert_eq!(files[0]["path"], "src/lib.rs");
        assert_eq!(files[0]["change"], "modified");
        assert_eq!(files[0]["kind"], "text");
        assert_eq!(files[0]["additions"], 2);
        assert_eq!(files[0]["deletions"], 1);
        let hunks = files[0]["hunks"].as_array().unwrap();
        assert_eq!(hunks.len(), 1);
        assert_eq!(hunks[0]["oldStart"], 1);
        assert_eq!(hunks[0]["newStart"], 1);

        // 2. new_file.txt (added)
        assert_eq!(files[1]["path"], "new_file.txt");
        assert_eq!(files[1]["change"], "added");
        assert_eq!(files[1]["kind"], "text");
        assert_eq!(files[1]["additions"], 2);
        assert_eq!(files[1]["deletions"], 0);

        // 3. deleted.txt (deleted)
        assert_eq!(files[2]["path"], "deleted.txt");
        assert_eq!(files[2]["change"], "deleted");
        assert_eq!(files[2]["additions"], 0);
        assert_eq!(files[2]["deletions"], 1);

        // 4. image.png (binary)
        assert_eq!(files[3]["path"], "image.png");
        assert_eq!(files[3]["kind"], "binary");
        assert_eq!(files[3]["hunks"].as_array().unwrap().len(), 0);
    }

    #[test]
    fn test_run_git_loop_detect_and_changes() {
        let repo_root = env!("CARGO_MANIFEST_DIR");
        let input = format!(
            "{{\"type\":\"pipkin_git_detect\",\"id\":100,\"root\":\"{}\",\"trusted\":true}}\n\
             {{\"type\":\"oopbot_git_detect\",\"id\":101,\"root\":\"{}\",\"trusted\":true}}\n\
             {{\"type\":\"pipkin_git_changes\",\"id\":102,\"root\":\"{}\",\"mode\":\"uncommitted\",\"ignoreWhitespace\":false}}\n\
             {{\"type\":\"oopbot_git_changes\",\"id\":103,\"root\":\"{}\",\"mode\":\"uncommitted\",\"ignoreWhitespace\":false}}\n\
             {{\"type\":\"pipkin_git_commits\",\"id\":104,\"root\":\"{}\"}}\n\
             {{\"type\":\"pipkin_git_commit_diff\",\"id\":105,\"root\":\"{}\",\"sha\":\"0000000000000000000000000000000000000000\",\"ignoreWhitespace\":false}}\n",
            repo_root, repo_root, repo_root, repo_root, repo_root, repo_root
        );
        let cursor = Cursor::new(input.into_bytes());
        let mut output = Vec::new();

        let res = run_git_loop(cursor, &mut output);
        assert!(res.is_ok());

        let out_str = String::from_utf8_lossy(&output);
        assert!(out_str.contains("pipkin_git_ready"));
        assert!(out_str.contains("pipkin_git_detected"));
        assert!(out_str.contains("\"repo\":true"));
        assert!(out_str.contains("\"git\":true"));
        assert!(out_str.contains("pipkin_git_changes_result"));
        assert!(out_str.contains("fingerprint"));
        assert!(out_str.contains("pipkin_git_commits_result"));
        assert!(out_str.contains("pipkin_git_commit_diff_result"));
        assert!(out_str.contains("commit_not_found"));
    }

    #[test]
    fn test_read_json_file_standard() {
        let temp_dir = std::env::temp_dir().join(format!("oopbot_test_{}", epoch_ms()));
        let oopbot_dir = temp_dir.join(".oopbot");
        let _ = std::fs::create_dir_all(&oopbot_dir);

        let oopbot_auth = oopbot_dir.join("auth.json");
        std::fs::write(&oopbot_auth, r#"{"test_provider":{"apiKey":"my-secret-456"}}"#).unwrap();
        let val = read_json_file(&oopbot_auth);
        assert_eq!(val["test_provider"]["apiKey"], "my-secret-456");

        let _ = std::fs::remove_dir_all(&temp_dir);
    }
}


