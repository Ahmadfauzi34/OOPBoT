//! Sub-command `oopbot host <mode> <port>`
//!
//! Jembatan komunikasi native Termux-ke-Android langsung via Tokio / standard TCP socket,
//! menggantikan `node host.mjs` sehingga Termux 100% bebas dari ketergantungan Node.js.
//!
//! Mendukung mode:
//! - `rpc`     : Menghubungkan sesi percakapan agent langsung ke TCP socket Android.
//! - `ensure`  : Verifikasi kesiapan engine native OOPBoT (tanpa npm / node).
//! - `hello`   : Handshake info lingkungan Termux (direktori, storage, dll).
//! - `utility` : Penanganan one-off request (daftar sesi, model default, auth status, dll).

use crate::gemini::BoxError;
use crate::models::ModelCatalog;
use crate::rpc::{set_output_sink, RpcServer};
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
    let token = match env::var("PIPKIN_TOKEN") {
        Ok(t) if !t.trim().is_empty() => t.trim().to_string(),
        _ => {
            let mut line = String::new();
            std::io::stdin().read_line(&mut line)?;
            line.trim().to_string()
        }
    };

    if token.is_empty() {
        return Err("PIPKIN_TOKEN kosong atau tidak ditemukan".into());
    }

    let home = env::var("HOME").unwrap_or_else(|_| ".".into());
    let _prefix = env::var("PREFIX").unwrap_or_else(|_| "/data/data/com.termux/files/usr".into());
    let config_dir = env::var("PI_CODING_AGENT_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from(&home).join(".pipkin/pi"));
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

    // 3. Kirim handshake `pipkin_hello`
    let hello = json!({
        "type": "pipkin_hello",
        "protocol": PROTOCOL_VERSION,
        "token": token,
        "mode": mode,
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
        "rpc" => {
            handle_rpc_mode(params, &config_dir, reader, writer).await
        }
        "utility" => {
            handle_utility_mode(&config_dir, &home, reader, writer).await
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
            "auth_status" => handle_auth_status(config_dir),
            "provider_models" => handle_provider_models(),
            "model_defaults" => handle_model_defaults(),
            "default_model" => handle_save_default_model(config_dir, &params),
            "delete_sessions" => handle_delete_sessions(&params),
            _ => json!({
                "type": "pipkin_result",
                "id": id,
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

fn handle_list_sessions(config_dir: &Path, home: &str) -> Value {
    let mut sessions = Vec::new();
    let sessions_dir = config_dir.join("sessions");
    if let Ok(entries) = std::fs::read_dir(sessions_dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) == Some("jsonl") {
                if let Ok(content) = std::fs::read_to_string(&path) {
                    if let Some(first_line) = content.lines().next() {
                        if let Ok(meta) = serde_json::from_str::<Value>(first_line) {
                            let id = meta["id"].as_str().unwrap_or_default().to_string();
                            let cwd = meta["cwd"].as_str().unwrap_or(home).to_string();
                            let timestamp = meta["timestamp"].as_str().unwrap_or_default();
                            sessions.push(json!({
                                "path": path.to_string_lossy(),
                                "id": id,
                                "cwd": cwd,
                                "cwdExists": Path::new(&cwd).exists(),
                                "createdAt": timestamp,
                                "subagents": 0,
                            }));
                        }
                    }
                }
            }
        }
    }
    json!({
        "type": "pipkin_result",
        "ok": true,
        "data": { "sessions": sessions }
    })
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

fn handle_auth_status(config_dir: &Path) -> Value {
    let settings_path = config_dir.join("settings.json");
    let has_settings = settings_path.exists();
    let has_env_key = env::var("GEMINI_API_KEY").is_ok()
        || env::var("OPENROUTER_API_KEY").is_ok()
        || env::var("DEEPSEEK_API_KEY").is_ok()
        || env::var("OPENAI_API_KEY").is_ok()
        || env::var("OOPBOT_API_KEY").is_ok();

    json!({
        "type": "pipkin_result",
        "ok": true,
        "data": {
            "anyConfigured": has_settings || has_env_key,
            "providers": []
        }
    })
}

fn handle_provider_models() -> Value {
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

fn handle_model_defaults() -> Value {
    json!({
        "type": "pipkin_result",
        "ok": true,
        "data": {
            "google": "gemini-2.5-flash",
            "openrouter": "deepseek/deepseek-chat",
            "deepseek": "deepseek-chat"
        }
    })
}

fn handle_save_default_model(config_dir: &Path, params: &Value) -> Value {
    let settings_path = config_dir.join("settings.json");
    let mut settings = if let Ok(content) = std::fs::read_to_string(&settings_path) {
        serde_json::from_str::<Value>(&content).unwrap_or(json!({}))
    } else {
        json!({})
    };

    if let Some(provider) = params["provider"].as_str() {
        settings["defaultProvider"] = json!(provider);
    }
    if let Some(model_id) = params["modelId"].as_str() {
        settings["defaultModel"] = json!(model_id);
    }
    if let Some(lvl) = params["thinkingLevel"].as_str() {
        settings["defaultThinkingLevel"] = json!(lvl);
    }

    if let Ok(out) = serde_json::to_string_pretty(&settings) {
        let _ = std::fs::write(settings_path, out);
    }

    json!({
        "type": "pipkin_result",
        "ok": true,
        "data": {}
    })
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    #[test]
    fn test_handle_provider_models() {
        let res = handle_provider_models();
        assert_eq!(res["type"], "pipkin_result");
        assert_eq!(res["ok"], true);
        let models = res["data"]["models"].as_array().unwrap();
        assert!(!models.is_empty());
    }

    #[test]
    fn test_handle_model_defaults() {
        let res = handle_model_defaults();
        assert_eq!(res["type"], "pipkin_result");
        assert_eq!(res["ok"], true);
        assert_eq!(res["data"]["google"], "gemini-2.5-flash");
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
}
