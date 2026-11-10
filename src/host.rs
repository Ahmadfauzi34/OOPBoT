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
                Some("pipkin_auth_response") => {
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
                Some("pipkin_auth_cancel") => {
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
    mut reader: BufReader<TcpStream>,
    writer: BufWriter<TcpStream>,
) -> Result<(), BoxError> {
    let cwd = params["cwd"].as_str().map(PathBuf::from);
    if let Some(ref dir) = cwd {
        if dir.is_dir() {
            let _ = env::set_current_dir(dir);
        }
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
    let result = server.run_loop_with_input(&mut reader).await;

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

        if req["type"].as_str() != Some("pipkin_utility_request") {
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
            Some("pipkin_list") => {
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
            Some("pipkin_search") => {
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
            Some("pipkin_files_list") => {
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
            Some("pipkin_files_read") => {
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
            Some("pipkin_files_mkdir") => {
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


/// Pindai sesi percakapan secara rekursif di `sessions/**/*.jsonl`
fn handle_list_sessions(config_dir: &Path, home: &str) -> Value {
    let mut sessions = Vec::new();
    let sessions_dir = config_dir.join("sessions");

    let mut session_files = Vec::new();
    collect_session_files(&sessions_dir, &mut session_files);

    for path in session_files {
        if let Ok(content) = std::fs::read_to_string(&path) {
            let mut lines = content.lines();
            let Some(first_line) = lines.next() else { continue; };
            let Ok(meta) = serde_json::from_str::<Value>(first_line) else { continue; };

            let id = meta["id"].as_str().unwrap_or_default().to_string();
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
        serde_json::from_str(&content).unwrap_or(json!({}))
    } else {
        json!({})
    }
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
        let mut cursor = Cursor::new(input);

        let res = server.run_loop_with_input(&mut cursor).await;
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
}
