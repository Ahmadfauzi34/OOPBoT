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
use crate::models::{
    builtin_providers, extract_auth_credentials, parse_models_response, resolve_model_scope,
    ModelCatalog, ModelInfo,
};
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

const OOPBOT_RELEASES_BASE: &str = "https://github.com/Ahmadfauzi34/OOPBoT/releases";
const OOPBOT_RELEASE_ASSET: &str = "oopbot-aarch64-linux-android";
static TMP_SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

#[derive(Debug, PartialEq, Eq)]
struct EnsureError {
    code: &'static str,
    message: String,
    i18n: Option<Value>,
}

fn oopbot_release_urls(expected_version: &str) -> Vec<String> {
    let trimmed = expected_version.trim();
    if trimmed.is_empty() {
        vec![format!("{}/latest/download/{}", OOPBOT_RELEASES_BASE, OOPBOT_RELEASE_ASSET)]
    } else {
        vec![
            format!("{}/download/v{}/{}", OOPBOT_RELEASES_BASE, trimmed, OOPBOT_RELEASE_ASSET),
            format!("{}/latest/download/{}", OOPBOT_RELEASES_BASE, OOPBOT_RELEASE_ASSET),
        ]
    }
}

fn parse_oopbot_version_output(stdout: &str) -> Option<String> {
    let first_line = stdout.lines().next()?.trim();
    let mut parts = first_line.split_whitespace();
    match (parts.next(), parts.next()) {
        (Some("oopbot"), Some(ver)) if !ver.is_empty() => Some(ver.to_string()),
        _ => None,
    }
}

fn is_executable_file_busy(err: &std::io::Error) -> bool {
    if err.kind() == std::io::ErrorKind::ExecutableFileBusy {
        return true;
    }
    #[cfg(unix)]
    {
        return err.raw_os_error() == Some(26);
    }
    #[cfg(not(unix))]
    {
        false
    }
}

fn read_oopbot_binary_version_detail(path: &Path) -> Result<String, String> {
    let meta = std::fs::metadata(path).map_err(|e| format!("metadata failed: {}", e))?;
    if !meta.is_file() || meta.len() == 0 {
        return Err("binary is missing or empty".to_string());
    }

    // Di Linux multi-threaded (termasuk test paralel atau child spawn bersamaan),
    // fork() pada thread lain sesaat saat File::create masih terbuka bisa menyebabkan
    // execve() mengembalikan ETXTBSY sampai child selesai execve().
    const ETXTBSY_RETRY_DELAYS_MS: &[u64] = &[2, 5, 10, 20, 40, 80, 100, 150];
    let mut attempt = 0usize;
    let output = loop {
        match std::process::Command::new(path).arg("--version").output() {
            Ok(out) => break out,
            Err(err) if is_executable_file_busy(&err) && attempt < ETXTBSY_RETRY_DELAYS_MS.len() => {
                std::thread::sleep(std::time::Duration::from_millis(ETXTBSY_RETRY_DELAYS_MS[attempt]));
                attempt += 1;
            }
            Err(err) => return Err(format!("exec failed: {}", err)),
        }
    };

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
        return Err(if stderr.is_empty() {
            format!("exited with {}", output.status)
        } else {
            format!("exited with {}: {}", output.status, stderr)
        });
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    parse_oopbot_version_output(&stdout)
        .ok_or_else(|| format!("unexpected --version output: {:?}", stdout.trim()))
}

fn read_oopbot_binary_version(path: &Path) -> Option<String> {
    read_oopbot_binary_version_detail(path).ok()
}

#[cfg(unix)]
fn set_executable_permissions(path: &Path) -> std::io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    let mut perms = std::fs::metadata(path)?.permissions();
    perms.set_mode(0o755);
    std::fs::set_permissions(path, perms)
}

#[cfg(not(unix))]
fn set_executable_permissions(_path: &Path) -> std::io::Result<()> {
    Ok(())
}

fn atomic_write_binary(target: &Path, bytes: &[u8]) -> std::io::Result<()> {
    if let Some(parent) = target.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let name = target
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or("oopbot");
    let seq = TMP_SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let tmp = target.with_file_name(format!(".{}.{}.{}.tmp", name, std::process::id(), seq));
    let write_res = (|| -> std::io::Result<()> {
        {
            let mut file = std::fs::File::create(&tmp)?;
            file.write_all(bytes)?;
            file.sync_all()?;
        }
        set_executable_permissions(&tmp)?;
        std::fs::rename(&tmp, target)
    })();
    if write_res.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    write_res
}

fn sync_secondary_targets(
    secondary_targets: &[PathBuf],
    primary_target: &Path,
    bytes: Option<&[u8]>,
    version: &str,
) {
    for target in secondary_targets {
        if target == primary_target {
            continue;
        }
        if target.exists() {
            let payload = if let Some(b) = bytes {
                Some(b.to_vec())
            } else {
                std::fs::read(primary_target).ok()
            };
            if let Some(ref data) = payload {
                let _ = atomic_write_binary(target, data);
            }
        }
        if let Some(parent) = target.parent() {
            let version_file = parent.join("oopbot.version");
            if version_file.exists() {
                let _ = std::fs::write(&version_file, format!("{}\n", version));
            }
        }
    }
}

async fn ensure_oopbot_binary_with<F>(
    current_version: &str,
    expected_version: &str,
    reinstall: bool,
    primary_target: &Path,
    secondary_targets: &[PathBuf],
    release_urls: &[String],
    mut on_progress: F,
) -> Result<String, EnsureError>
where
    F: FnMut(&str),
{
    let expected = expected_version.trim();
    if !reinstall {
        if let Some(disk_ver) = read_oopbot_binary_version(primary_target) {
            if expected.is_empty() || disk_ver == expected {
                on_progress(&format!("OOPBoT v{} native engine ready", disk_ver));
                sync_secondary_targets(secondary_targets, primary_target, None, &disk_ver);
                return Ok(disk_ver);
            }
        }
    }

    on_progress(&format!(
        "Installing OOPBoT native engine{}...",
        if expected.is_empty() { String::new() } else { format!(" v{}", expected) }
    ));

    let use_no_proxy = release_urls.iter().any(|u| {
        u.starts_with("http://127.0.0.1") || u.starts_with("http://localhost")
    });
    let mut builder = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(120));
    if use_no_proxy {
        builder = builder.no_proxy();
    }
    let client = builder.build().map_err(|e| EnsureError {
        code: "npm_failed",
        message: format!("Failed to initialize HTTP client: {}", e),
        i18n: None,
    })?;

    if let Some(parent) = primary_target.parent() {
        std::fs::create_dir_all(parent).map_err(|e| EnsureError {
            code: "upgrade_failed",
            message: format!("Failed to prepare {}: {}", parent.display(), e),
            i18n: None,
        })?;
    }

    let mut last_status: Option<u16> = None;
    let mut mismatch_msg: Option<String> = None;
    let mut saw_network_error = false;

    for url in release_urls {
        on_progress(&format!("Downloading {}", url));
        let resp = match client.get(url).send().await {
            Ok(r) => r,
            Err(_) => {
                saw_network_error = true;
                continue;
            }
        };
        if !resp.status().is_success() {
            last_status = Some(resp.status().as_u16());
            continue;
        }
        let bytes = match resp.bytes().await {
            Ok(b) => b,
            Err(_) => {
                saw_network_error = true;
                continue;
            }
        };
        if bytes.is_empty() {
            mismatch_msg = Some("Downloaded oopbot binary was empty".to_string());
            continue;
        }

        let name = primary_target
            .file_name()
            .and_then(|s| s.to_str())
            .unwrap_or("oopbot");
        let seq = TMP_SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let tmp_path = primary_target.with_file_name(format!(".{}.{}.{}.tmp", name, std::process::id(), seq));

        if let Err(e) = (|| -> std::io::Result<()> {
            {
                let mut file = std::fs::File::create(&tmp_path)?;
                file.write_all(&bytes)?;
                file.sync_all()?;
            }
            set_executable_permissions(&tmp_path)
        })() {
            let _ = std::fs::remove_file(&tmp_path);
            return Err(EnsureError {
                code: "upgrade_failed",
                message: format!("Failed to stage oopbot binary: {}", e),
                i18n: None,
            });
        }

        let candidate_ver = match read_oopbot_binary_version_detail(&tmp_path) {
            Ok(v) => v,
            Err(detail) => {
                let _ = std::fs::remove_file(&tmp_path);
                mismatch_msg = Some(format!("Downloaded oopbot binary failed --version check ({})", detail));
                continue;
            }
        };

        if !expected.is_empty() && candidate_ver != expected {
            let _ = std::fs::remove_file(&tmp_path);
            mismatch_msg = Some(format!(
                "Downloaded oopbot {}, expected {} (current {})",
                candidate_ver, expected, current_version
            ));
            continue;
        }

        if let Err(e) = std::fs::rename(&tmp_path, primary_target) {
            let _ = std::fs::remove_file(&tmp_path);
            return Err(EnsureError {
                code: "upgrade_failed",
                message: format!("Failed to install {}: {}", primary_target.display(), e),
                i18n: None,
            });
        }

        sync_secondary_targets(secondary_targets, primary_target, Some(&bytes), &candidate_ver);
        return Ok(candidate_ver);
    }

    if saw_network_error && last_status.is_none() && mismatch_msg.is_none() {
        return Err(EnsureError {
            code: "npm_offline",
            message: "Couldn't download oopbot. Check your internet connection, then retry.".to_string(),
            i18n: Some(json!({ "key": "agent_download_offline" })),
        });
    }
    if let Some(msg) = mismatch_msg {
        return Err(EnsureError {
            code: "upgrade_failed",
            message: msg,
            i18n: None,
        });
    }
    let status = last_status.unwrap_or(1);
    Err(EnsureError {
        code: "npm_failed",
        message: format!("Failed to download oopbot (HTTP {})", status),
        i18n: Some(json!({
            "key": "command_failed",
            "args": { "command": "download oopbot", "exit": status }
        })),
    })
}

async fn handle_ensure_mode(
    params: Value,
    prefix: &Path,
    oopbot_root: Option<&Path>,
    mut writer: BufWriter<TcpStream>,
) -> Result<(), BoxError> {
    let expected_version = params["expectedVersion"].as_str().unwrap_or("").trim().to_string();
    let reinstall = params["reinstall"].as_bool().unwrap_or(false);
    let primary_target = prefix.join("bin").join("oopbot");
    let mut secondary_targets = Vec::new();
    if let Some(root) = oopbot_root {
        secondary_targets.push(root.join("bin").join("oopbot"));
    }
    let release_urls = oopbot_release_urls(&expected_version);

    let result = ensure_oopbot_binary_with(
        env!("CARGO_PKG_VERSION"),
        &expected_version,
        reinstall,
        &primary_target,
        &secondary_targets,
        &release_urls,
        |line| {
            let progress = json!({
                "type": "pipkin_progress",
                "line": line
            });
            if let Ok(s) = serde_json::to_string(&progress) {
                let _ = writeln!(writer, "{}", s);
                let _ = writer.flush();
            }
        },
    )
    .await;

    let response = match result {
        Ok(ver) => json!({
            "type": "pipkin_result",
            "ok": true,
            "data": {
                "oopbotVersion": ver,
                "piVersion": ver
            }
        }),
        Err(err) => {
            let mut obj = json!({
                "type": "pipkin_result",
                "ok": false,
                "error": err.message,
                "code": err.code
            });
            if let Some(i18n) = err.i18n {
                obj["i18n"] = i18n;
            }
            obj
        }
    };
    writeln!(writer, "{}", serde_json::to_string(&response)?)?;
    writer.flush()?;
    Ok(())
}

fn check_rpc_expected_version(params: &Value, installed_version: &str) -> Option<Value> {
    let expected = params["expectedVersion"].as_str().unwrap_or("").trim();
    if !expected.is_empty() && expected != installed_version {
        Some(json!({
            "type": "pipkin_result",
            "ok": false,
            "error": format!("Agent {} is installed; OOPBoT needs {}", installed_version, expected),
            "code": "pi_version",
            "i18n": {
                "key": "agent_version_mismatch",
                "args": {
                    "installed": installed_version,
                    "expected": expected
                }
            }
        }))
    } else {
        None
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

fn is_safe_relative_subpath(raw: &str) -> bool {
    if raw.contains('\0') {
        return false;
    }
    let normalized = raw.replace('\\', "/");
    let p = Path::new(&normalized);
    if p.is_absolute() {
        return false;
    }
    for comp in p.components() {
        match comp {
            std::path::Component::ParentDir | std::path::Component::RootDir | std::path::Component::Prefix(_) => {
                return false;
            }
            std::path::Component::Normal(_) | std::path::Component::CurDir => {}
        }
    }
    true
}

fn nearest_existing_canonical(path: &Path) -> Option<PathBuf> {
    let mut cur = path.to_path_buf();
    loop {
        if cur.exists() || std::fs::symlink_metadata(&cur).is_ok() {
            return std::fs::canonicalize(&cur).ok();
        }
        if !cur.pop() {
            return None;
        }
    }
}

fn resolve_scoped_path(
    root: &str,
    raw_path: &str,
) -> Result<(PathBuf, PathBuf), (&'static str, &'static str)> {
    let root_path = Path::new(root);
    if root.trim().is_empty() || !root_path.is_absolute() {
        return Err(("outside_workspace", "Workspace root must be an absolute path."));
    }
    let canonical_root = std::fs::canonicalize(root_path)
        .map_err(|_| ("files_gone", "Workspace root folder no longer exists."))?;
    if !is_safe_relative_subpath(raw_path) {
        return Err(("outside_workspace", "That path is outside the workspace."));
    }
    let target = if raw_path == "." || raw_path.is_empty() {
        canonical_root.clone()
    } else {
        canonical_root.join(raw_path.replace('\\', "/"))
    };
    if let Ok(canon_target) = std::fs::canonicalize(&target) {
        if !canon_target.starts_with(&canonical_root) {
            return Err(("outside_workspace", "That path is outside the workspace."));
        }
        return Ok((canonical_root, canon_target));
    }
    // Path belum ada atau broken symlink: pastikan ancestor terdekat (dan symlink target jika ada) tidak keluar root
    if let Ok(meta) = std::fs::symlink_metadata(&target) {
        if meta.file_type().is_symlink() {
            return Err(("outside_workspace", "That symlink target cannot be accessed."));
        }
    }
    if let Some(canon_parent) = target.parent().and_then(nearest_existing_canonical) {
        if !canon_parent.starts_with(&canonical_root) {
            return Err(("outside_workspace", "That path is outside the workspace."));
        }
    }
    Ok((canonical_root, target))
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
        let root = req["root"].as_str().unwrap_or("");
        let path = req["path"].as_str().unwrap_or(".");

        match req["type"].as_str() {
            Some("pipkin_files_list") | Some("oopbot_files_list") => {
                let reply = match resolve_scoped_path(root, path) {
                    Err((code, msg)) => json!({
                        "type": "pipkin_files_listing",
                        "id": id,
                        "path": path,
                        "error": msg,
                        "errorCode": code,
                        "entries": []
                    }),
                    Ok((canonical_root, target_dir)) => {
                        let mut entries = Vec::new();
                        if let Ok(dir_entries) = std::fs::read_dir(&target_dir) {
                            for entry in dir_entries.flatten() {
                                let p = entry.path();
                                let Ok(canon_entry) = std::fs::canonicalize(&p) else {
                                    continue;
                                };
                                if !canon_entry.starts_with(&canonical_root) {
                                    continue;
                                }
                                let name = entry.file_name().to_string_lossy().to_string();
                                let is_dir = canon_entry.is_dir();
                                let meta = std::fs::metadata(&canon_entry).ok();
                                let size = if is_dir {
                                    0
                                } else {
                                    meta.as_ref().map(|m| m.len()).unwrap_or(0)
                                };
                                let modified = meta
                                    .and_then(|m| m.modified().ok())
                                    .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
                                    .map(|d| d.as_millis() as u64)
                                    .unwrap_or(0);

                                let rel_path = if path == "." || path.is_empty() {
                                    name.clone()
                                } else {
                                    format!("{}/{}", path.trim_end_matches('/'), name)
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
                                _ => a["name"]
                                    .as_str()
                                    .unwrap_or("")
                                    .to_lowercase()
                                    .cmp(&b["name"].as_str().unwrap_or("").to_lowercase()),
                            }
                        });

                        json!({
                            "type": "pipkin_files_listing",
                            "id": id,
                            "path": path,
                            "entries": entries
                        })
                    }
                };
                writeln!(writer, "{}", serde_json::to_string(&reply)?)?;
                writer.flush()?;
            }
            Some("pipkin_files_read") | Some("oopbot_files_read") => {
                let reply = match resolve_scoped_path(root, path) {
                    Err((code, msg)) => json!({
                        "type": "pipkin_files_file",
                        "id": id,
                        "path": path,
                        "error": msg,
                        "errorCode": code
                    }),
                    Ok((_canonical_root, file_path)) => {
                        const TEXT_LIMIT: u64 = 2 * 1024 * 1024; // 2 MiB
                        const IMAGE_LIMIT: u64 = 8 * 1024 * 1024; // 8 MiB

                        let is_image = {
                            let ext = file_path
                                .extension()
                                .and_then(|e| e.to_str())
                                .unwrap_or("")
                                .to_lowercase();
                            matches!(
                                ext.as_str(),
                                "png" | "jpg" | "jpeg" | "gif" | "webp" | "bmp" | "ico"
                            )
                        };

                        let meta = std::fs::metadata(&file_path);
                        match meta {
                            Ok(m) if m.is_dir() => json!({
                                "type": "pipkin_files_file",
                                "id": id,
                                "path": path,
                                "error": "This is a folder.",
                                "errorCode": "files_is_folder"
                            }),
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
                                    let read_bytes = file
                                        .as_mut()
                                        .and_then(|f| std::io::Read::read(f, &mut sniff_buf).ok())
                                        .unwrap_or(0);
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
                                    if let Ok(text) = String::from_utf8(bytes) {
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
                            Err(_) => json!({
                                "type": "pipkin_files_file",
                                "id": id,
                                "path": path,
                                "error": "This file or folder no longer exists.",
                                "errorCode": "files_gone"
                            }),
                        }
                    }
                };
                writeln!(writer, "{}", serde_json::to_string(&reply)?)?;
                writer.flush()?;
            }
            Some("pipkin_files_mkdir") | Some("oopbot_files_mkdir") => {
                let name = req["name"].as_str().unwrap_or("").trim();
                let reply = if name.is_empty()
                    || name == "."
                    || name == ".."
                    || name.contains('/')
                    || name.contains('\\')
                    || name.contains('\0')
                {
                    json!({
                        "type": "pipkin_files_made",
                        "id": id,
                        "error": "Folder names cannot be empty, contain / or be . or ..",
                        "errorCode": "files_folder_name_invalid"
                    })
                } else {
                    match resolve_scoped_path(root, path) {
                        Err((code, msg)) => json!({
                            "type": "pipkin_files_made",
                            "id": id,
                            "error": msg,
                            "errorCode": code
                        }),
                        Ok((_canonical_root, target_parent)) => {
                            let new_folder = target_parent.join(name);
                            let created_rel = if path == "." || path.is_empty() {
                                name.to_string()
                            } else {
                                format!("{}/{}", path.trim_end_matches('/'), name)
                            };
                            if let Err(e) = std::fs::create_dir_all(&new_folder) {
                                json!({
                                    "type": "pipkin_files_made",
                                    "id": id,
                                    "error": format!("Failed to create folder: {}", e),
                                    "errorCode": "files_mkdir_failed"
                                })
                            } else {
                                json!({
                                    "type": "pipkin_files_made",
                                    "id": id,
                                    "path": created_rel
                                })
                            }
                        }
                    }
                };
                writeln!(writer, "{}", serde_json::to_string(&reply)?)?;
                writer.flush()?;
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

fn resolve_terminal_shell_with(
    env_shell: Option<&str>,
    env_prefix: Option<&str>,
) -> (String, Vec<String>) {
    if let Some(sh) = env_shell.map(str::trim).filter(|s| !s.is_empty()) {
        if !sh.contains('/') || Path::new(sh).is_file() {
            let arg = if sh.ends_with("/sh") || sh == "sh" {
                "-i"
            } else {
                "-l"
            };
            return (sh.to_string(), vec![arg.to_string()]);
        }
    }

    let prefix = env_prefix
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .unwrap_or("/data/data/com.termux/files/usr");
    let candidates = [
        (format!("{}/bin/bash", prefix), "-l"),
        (format!("{}/bin/zsh", prefix), "-l"),
        (format!("{}/bin/sh", prefix), "-i"),
        ("/bin/bash".to_string(), "-l"),
        ("/bin/sh".to_string(), "-i"),
    ];
    for (cand, flag) in candidates {
        if Path::new(&cand).is_file() {
            return (cand, vec![flag.to_string()]);
        }
    }
    ("sh".to_string(), vec!["-i".to_string()])
}

fn resolve_terminal_shell() -> (String, Vec<String>) {
    let env_shell = env::var("SHELL").ok();
    let env_prefix = env::var("PREFIX").ok();
    resolve_terminal_shell_with(env_shell.as_deref(), env_prefix.as_deref())
}

#[cfg(unix)]
mod pty_unix {
    use std::ffi::{CStr, CString, c_char, c_int, c_ulong, c_ushort};
    use std::fs::File;
    use std::os::unix::io::{AsRawFd, FromRawFd, OwnedFd};
    use std::os::unix::process::CommandExt;

    const O_RDWR: c_int = 2;
    const O_NOCTTY: c_int = 0x100;
    const TIOCSCTTY: c_ulong = 0x540e;
    const TIOCSWINSZ: c_ulong = 0x5414;

    #[repr(C)]
    struct Winsize {
        ws_row: c_ushort,
        ws_col: c_ushort,
        ws_xpixel: c_ushort,
        ws_ypixel: c_ushort,
    }

    extern "C" {
        fn posix_openpt(flags: c_int) -> c_int;
        fn grantpt(fd: c_int) -> c_int;
        fn unlockpt(fd: c_int) -> c_int;
        fn ptsname_r(fd: c_int, buf: *mut c_char, buflen: usize) -> c_int;
        fn setsid() -> c_int;
        fn ioctl(fd: c_int, request: c_ulong, ...) -> c_int;
        fn open(pathname: *const c_char, flags: c_int, ...) -> c_int;
        fn dup2(oldfd: c_int, newfd: c_int) -> c_int;
        fn close(fd: c_int) -> c_int;
    }

    pub struct PtySession {
        pub master: File,
        pub child: std::process::Child,
    }

    pub fn set_winsize(master: &File, rows: u16, cols: u16) {
        let ws = Winsize {
            ws_row: rows.max(1),
            ws_col: cols.max(1),
            ws_xpixel: 0,
            ws_ypixel: 0,
        };
        unsafe {
            let _ = ioctl(master.as_raw_fd(), TIOCSWINSZ, &ws);
        }
    }

    pub fn spawn_shell(
        shell: &str,
        args: &[String],
        cwd: &str,
        rows: u16,
        cols: u16,
    ) -> std::io::Result<PtySession> {
        let master_raw = unsafe { posix_openpt(O_RDWR | O_NOCTTY) };
        if master_raw < 0 {
            return Err(std::io::Error::last_os_error());
        }
        let master_fd = unsafe { OwnedFd::from_raw_fd(master_raw) };

        if unsafe { grantpt(master_fd.as_raw_fd()) } != 0
            || unsafe { unlockpt(master_fd.as_raw_fd()) } != 0
        {
            return Err(std::io::Error::last_os_error());
        }

        let mut name_buf = [0 as c_char; 256];
        let rc = unsafe { ptsname_r(master_fd.as_raw_fd(), name_buf.as_mut_ptr(), name_buf.len()) };
        if rc != 0 {
            return Err(std::io::Error::from_raw_os_error(rc));
        }
        let slave_name = unsafe { CStr::from_ptr(name_buf.as_ptr()) };
        let slave_cstr = CString::new(slave_name.to_bytes())
            .map_err(|_| std::io::Error::new(std::io::ErrorKind::InvalidInput, "NUL in ptsname"))?;

        let master_file = File::from(master_fd);
        set_winsize(&master_file, rows, cols);
        let master_fd_to_close = master_file.as_raw_fd();

        let mut cmd = std::process::Command::new(shell);
        cmd.args(args)
            .current_dir(cwd)
            .env("TERM", "xterm-256color")
            .env("COLORTERM", "truecolor");

        unsafe {
            cmd.pre_exec(move || {
                let _ = close(master_fd_to_close);
                if setsid() < 0 {
                    return Err(std::io::Error::last_os_error());
                }
                let slave = open(slave_cstr.as_ptr(), O_RDWR);
                if slave < 0 {
                    return Err(std::io::Error::last_os_error());
                }
                let _ = ioctl(slave, TIOCSCTTY, 0 as c_int);
                if dup2(slave, 0) < 0 || dup2(slave, 1) < 0 || dup2(slave, 2) < 0 {
                    let err = std::io::Error::last_os_error();
                    let _ = close(slave);
                    return Err(err);
                }
                if slave > 2 {
                    let _ = close(slave);
                }
                Ok(())
            });
        }

        let child = cmd.spawn()?;
        Ok(PtySession {
            master: master_file,
            child,
        })
    }
}

/// Menangani mode `terminal`: terminal interaktif shell native dengan PTY asli dan streaming Base64
async fn handle_terminal_mode(
    params: Value,
    reader: BufReader<TcpStream>,
    writer: BufWriter<TcpStream>,
) -> Result<(), BoxError> {
    run_terminal_loop(params, reader, writer)
}

fn run_terminal_loop<R: BufRead + Send + 'static, W: Write + Send + 'static>(
    params: Value,
    mut reader: R,
    writer: W,
) -> Result<(), BoxError> {
    let cwd = params["cwd"].as_str().unwrap_or(".").to_string();
    let rows = params["rows"].as_u64().unwrap_or(24).clamp(1, u16::MAX as u64) as u16;
    let cols = params["cols"].as_u64().unwrap_or(80).clamp(1, u16::MAX as u64) as u16;

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

    let (shell, shell_args) = resolve_terminal_shell();
    let shared_writer = Arc::new(Mutex::new(writer));

    #[cfg(unix)]
    {
        let mut pty = pty_unix::spawn_shell(&shell, &shell_args, &cwd, rows, cols)?;
        let mut pty_read = pty.master.try_clone()?;

        enum TermEvent {
            Command(Value),
            InputEof,
            PtyClosed,
        }
        let (tx, rx) = std::sync::mpsc::channel::<TermEvent>();

        let tx_pty = tx.clone();
        let writer_out = shared_writer.clone();
        let stdout_handle = std::thread::spawn(move || {
            let mut buf = [0u8; 4096];
            loop {
                match std::io::Read::read(&mut pty_read, &mut buf) {
                    Ok(0) | Err(_) => break,
                    Ok(n) => {
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
            }
            let _ = tx_pty.send(TermEvent::PtyClosed);
        });

        let tx_in = tx;
        let _input_handle = std::thread::spawn(move || {
            let mut line = String::new();
            loop {
                line.clear();
                match reader.read_line(&mut line) {
                    Ok(0) | Err(_) => {
                        let _ = tx_in.send(TermEvent::InputEof);
                        break;
                    }
                    Ok(_) => {
                        let trimmed = line.trim();
                        if trimmed.is_empty() {
                            continue;
                        }
                        if let Ok(req) = serde_json::from_str::<Value>(trimmed) {
                            if tx_in.send(TermEvent::Command(req)).is_err() {
                                break;
                            }
                        }
                    }
                }
            }
        });

        let mut pty_write = pty.master;
        while let Ok(ev) = rx.recv() {
            match ev {
                TermEvent::Command(req) => match req["type"].as_str() {
                    Some("input") => {
                        if let Some(data_b64) = req["data"].as_str() {
                            if let Some(bytes) = decode_base64(data_b64) {
                                let _ = std::io::Write::write_all(&mut pty_write, &bytes);
                                let _ = std::io::Write::flush(&mut pty_write);
                            }
                        }
                    }
                    Some("resize") => {
                        let r = req["rows"].as_u64().unwrap_or(rows as u64).clamp(1, u16::MAX as u64) as u16;
                        let c = req["cols"].as_u64().unwrap_or(cols as u64).clamp(1, u16::MAX as u64) as u16;
                        pty_unix::set_winsize(&pty_write, r, c);
                    }
                    _ => {}
                },
                TermEvent::InputEof => {
                    let _ = pty.child.kill();
                    break;
                }
                TermEvent::PtyClosed => {
                    break;
                }
            }
        }

        drop(pty_write);
        let status = pty.child.wait().ok();
        let _ = stdout_handle.join();

        let exit_code = status.and_then(|s| s.code());
        let exit_msg = json!({
            "type": "pipkin_exit",
            "code": exit_code,
            "signal": null,
            "stderr": ""
        });
        if let Ok(mut w) = shared_writer.lock() {
            let _ = writeln!(w, "{}", serde_json::to_string(&exit_msg)?);
            let _ = w.flush();
        }
        Ok(())
    }

    #[cfg(not(unix))]
    {
        let _ = (shell, shell_args, reader, shared_writer, rows, cols);
        Err("Terminal PTY is only supported on Unix targets".into())
    }
}


fn is_valid_session_id(id: &str) -> bool {
    !id.is_empty()
        && id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
}

fn days_from_civil(y: i64, m: u32, d: u32) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = (y - era * 400) as u32;
    let m_adj = if m > 2 { m - 3 } else { m + 9 };
    let doy = (153 * m_adj + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe as i64 - 719_468
}

fn parse_iso_to_epoch_ms(raw: &str) -> Option<u64> {
    let s = raw.trim();
    if s.len() < 19 {
        return None;
    }
    let bytes = s.as_bytes();
    if bytes[4] != b'-' || bytes[7] != b'-' || (bytes[10] != b'T' && bytes[10] != b't' && bytes[10] != b' ') {
        return None;
    }
    if bytes[13] != b':' || bytes[16] != b':' {
        return None;
    }
    let year: i64 = s.get(0..4)?.parse().ok()?;
    let month: u32 = s.get(5..7)?.parse().ok()?;
    let day: u32 = s.get(8..10)?.parse().ok()?;
    let hour: u32 = s.get(11..13)?.parse().ok()?;
    let min: u32 = s.get(14..16)?.parse().ok()?;
    let sec: u32 = s.get(17..19)?.parse().ok()?;
    if !(1..=12).contains(&month) || !(1..=31).contains(&day) || hour > 23 || min > 59 || sec > 60 {
        return None;
    }

    let mut rest = &s[19..];
    let mut millis: i64 = 0;
    if let Some(after_dot) = rest.strip_prefix('.') {
        let digits_len = after_dot
            .bytes()
            .take_while(|b| b.is_ascii_digit())
            .count();
        if digits_len > 0 {
            let frac = &after_dot[..digits_len.min(3)];
            let mut val: i64 = frac.parse().ok()?;
            for _ in frac.len()..3 {
                val *= 10;
            }
            millis = val;
            rest = &after_dot[digits_len..];
        }
    }

    let offset_secs: i64 = if rest.is_empty() || rest.eq_ignore_ascii_case("Z") {
        0
    } else if (rest.starts_with('+') || rest.starts_with('-')) && rest.len() >= 5 {
        let sign: i64 = if rest.starts_with('-') { -1 } else { 1 };
        let body = &rest[1..];
        let (oh, om) = if let Some((h, m)) = body.split_once(':') {
            (h.parse::<i64>().ok()?, m.get(..2)?.parse::<i64>().ok()?)
        } else {
            (body.get(..2)?.parse::<i64>().ok()?, body.get(2..4)?.parse::<i64>().ok()?)
        };
        sign * (oh * 3600 + om * 60)
    } else {
        return None;
    };

    let days = days_from_civil(year, month, day);
    let secs = days * 86_400 + hour as i64 * 3600 + min as i64 * 60 + sec as i64 - offset_secs;
    if secs < 0 {
        return None;
    }
    Some((secs as u64).saturating_mul(1000).saturating_add(millis as u64))
}

fn subagent_transcript_files(config_dir: &Path, session_path: &Path, session_id: &str) -> Vec<PathBuf> {
    if !is_valid_session_id(session_id) {
        return Vec::new();
    }
    let Some(parent) = session_path.parent() else {
        return Vec::new();
    };
    let folder = parent.join("subagents").join(session_id);
    let Ok(canon_sessions) = std::fs::canonicalize(config_dir.join("sessions")) else {
        return Vec::new();
    };
    let Ok(canon_folder) = std::fs::canonicalize(&folder) else {
        return Vec::new();
    };
    if !canon_folder.starts_with(&canon_sessions) {
        return Vec::new();
    }
    let mut files = Vec::new();
    if let Ok(entries) = std::fs::read_dir(&canon_folder) {
        for entry in entries.flatten() {
            let p = entry.path();
            if p.extension().and_then(|e| e.to_str()) == Some("jsonl") && p.is_file() {
                files.push(p);
            }
        }
    }
    files.sort();
    files
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
            let mut name = meta
                .get("name")
                .and_then(|n| n.as_str())
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(str::to_string);

            let file_mtime = std::fs::metadata(&path)
                .and_then(|m| m.modified())
                .ok()
                .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
                .map(|d| d.as_millis() as u64)
                .unwrap_or_else(epoch_ms);

            let created = meta["timestamp"]
                .as_u64()
                .or_else(|| meta["timestamp"].as_str().and_then(parse_iso_to_epoch_ms))
                .or_else(|| meta["created"].as_u64())
                .or_else(|| meta["created"].as_str().and_then(parse_iso_to_epoch_ms))
                .unwrap_or(file_mtime);

            let mut message_count = 0;
            let mut first_message = String::new();

            for line in lines {
                if let Ok(val) = serde_json::from_str::<Value>(line) {
                    match val["type"].as_str() {
                        Some("session_info") => {
                            name = val
                                .get("name")
                                .and_then(|n| n.as_str())
                                .map(str::trim)
                                .filter(|s| !s.is_empty())
                                .map(str::to_string);
                        }
                        Some("message") => {
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
                        _ => {}
                    }
                }
            }

            let cwd_exists = Path::new(&cwd).is_dir();
            let subagents = subagent_transcript_files(config_dir, &path, &id).len();

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
                "subagents": subagents,
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
                if path.file_name().and_then(|n| n.to_str()) == Some("subagents") {
                    continue;
                }
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

fn read_json_object_with_error(path: &Path) -> (Value, Option<String>) {
    match std::fs::read_to_string(path) {
        Ok(content) => {
            if content.trim().is_empty() {
                return (json!({}), None);
            }
            match serde_json::from_str::<Value>(&content) {
                Ok(val) if val.is_object() => (val, None),
                Ok(_) => (
                    json!({}),
                    Some(format!("{} must contain a JSON object", path.file_name().unwrap_or_default().to_string_lossy())),
                ),
                Err(err) => (json!({}), Some(format!("Invalid JSON: {}", err))),
            }
        }
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => (json!({}), None),
        Err(err) => (json!({}), Some(err.to_string())),
    }
}

/// Menghasilkan skema `AuthStatus` lengkap sesuai kebutuhan UI Android (`HostModels.kt:140-182`)
fn handle_auth_status(config_dir: &Path) -> Value {
    let auth_path = config_dir.join("auth.json");
    let auth_json = read_json_file(&auth_path);

    let models_path = config_dir.join("models.json");
    let (models_json, models_error) = read_json_object_with_error(&models_path);

    let settings_path = config_dir.join("settings.json");
    let (settings_json, settings_error) = read_json_object_with_error(&settings_path);

    let catalog = ModelCatalog::builtin();
    let ref_providers = builtin_providers();

    // 6 built-in providers bawaan Android
    let builtin_specs = [
        ("google", "Google Gemini", "GEMINI_API_KEY", "GOOGLE_API_KEY"),
        ("anthropic", "Anthropic Claude", "ANTHROPIC_API_KEY", ""),
        ("openai-codex", "OpenAI", "OPENAI_API_KEY", ""),
        ("openrouter", "OpenRouter", "OPENROUTER_API_KEY", ""),
        ("zai", "Z.ai Coding Plan", "ZAI_API_KEY", ""),
        ("kimi-coding", "Kimi Code", "KIMI_API_KEY", ""),
    ];

    // Bangun universe semua model yang dikenal (builtin + custom/extra dari models.json) untuk scope resolution
    let mut universe: Vec<ModelInfo> = Vec::new();
    for bp in &ref_providers {
        for m in &bp.models {
            let mut item = m.clone();
            item.provider = bp.id.clone();
            universe.push(item);
        }
    }
    if let Some(providers_obj) = models_json.get("providers").and_then(|p| p.as_object()) {
        for (pid, pval) in providers_obj {
            let api = pval["api"].as_str().unwrap_or("openai-completions");
            if let Some(arr) = pval.get("models").and_then(|v| v.as_array()) {
                for m_item in arr {
                    if let Some(info) = ModelInfo::from_pipkin_json(m_item, pid, api) {
                        if !universe.iter().any(|u| u.provider == *pid && u.id == info.id) {
                            universe.push(info);
                        }
                    }
                }
            }
        }
    }

    // `null` scope: settings.json tidak punya enabledModels (atau array kosong / invalid JSON), sehingga semua model tampil.
    let scope_refs: Option<Vec<String>> = if settings_error.is_none() {
        settings_json
            .get("enabledModels")
            .and_then(|v| v.as_array())
            .filter(|arr| !arr.is_empty())
            .map(|arr| {
                let patterns: Vec<String> = arr
                    .iter()
                    .filter_map(|v| v.as_str().map(str::to_string))
                    .collect();
                resolve_model_scope(&patterns, &universe)
                    .into_iter()
                    .map(|(m, _)| format!("{}/{}", m.provider, m.id))
                    .collect()
            })
    } else {
        None
    };

    let mut builtin = Vec::new();
    for (id, name, env_key1, env_key2) in builtin_specs {
        let auth_cred = extract_auth_credentials(&auth_json, id);
        let has_env = (!env_key1.is_empty()
            && env::var(env_key1)
                .map(|v| !v.trim().is_empty())
                .unwrap_or(false))
            || (!env_key2.is_empty()
                && env::var(env_key2)
                    .map(|v| !v.trim().is_empty())
                    .unwrap_or(false));

        let (is_configured, auth_type) = if let Some((_tok, kind)) = auth_cred {
            (true, json!(kind))
        } else if has_env {
            (true, json!("api_key"))
        } else {
            (false, Value::Null)
        };

        let provider_filter = match id {
            "openai-codex" => "openai",
            "kimi-coding" => "kimi",
            other => other,
        };

        let overrides = models_json
            .get("providers")
            .and_then(|p| p.get(id))
            .and_then(|p| p.get("modelOverrides"));
        let mut models_for_provider: Vec<Value> = catalog
            .list()
            .iter()
            .filter(|m| m.provider == provider_filter || m.provider == id)
            .map(|m| {
                let mut cloned = m.clone();
                if let Some(ov) = overrides.and_then(|o| o.get(&cloned.id)) {
                    cloned.apply_override(ov);
                }
                cloned.to_pipkin_json(Some(id))
            })
            .collect();

        // Extra IDs + extra models dari models.json
        let mut extra_ids = Vec::new();
        if let Some(entry) = models_json.get("providers").and_then(|p| p.get(id)) {
            let entry_api = entry["api"].as_str().unwrap_or("openai-completions");
            if let Some(m_arr) = entry.get("models").and_then(|m| m.as_array()) {
                for m in m_arr {
                    if let Some(mid) = m.get("id").and_then(|i| i.as_str()) {
                        extra_ids.push(mid.to_string());
                        if !models_for_provider
                            .iter()
                            .any(|existing| existing["id"].as_str() == Some(mid))
                        {
                            if let Some(info) = ModelInfo::from_pipkin_json(m, id, entry_api) {
                                models_for_provider.push(info.to_pipkin_json(Some(id)));
                            }
                        }
                    }
                }
            }
        }

        // Enabled models hanya di-populate bila scope aktif (`Some(...)`); jika unbounded maka `null`.
        let enabled = scope_refs.as_ref().map(|refs| {
            refs.iter()
                .filter_map(|ref_str| {
                    ref_str
                        .split_once('/')
                        .and_then(|(p, m)| if p == id { Some(m.to_string()) } else { None })
                })
                .collect::<Vec<String>>()
        });

        builtin.push(json!({
            "id": id,
            "name": name,
            "configured": is_configured,
            "type": auth_type,
            "models": models_for_provider,
            "extraIds": extra_ids,
            "enabled": enabled,
        }));
    }

    // Custom providers dari models.json
    let mut custom = Vec::new();
    let builtin_id_set: HashSet<&str> = [
        "google",
        "anthropic",
        "openai",
        "openai-codex",
        "openrouter",
        "zai",
        "kimi",
        "kimi-coding",
    ]
    .into_iter()
    .collect();

    if let Some(providers_obj) = models_json.get("providers").and_then(|p| p.as_object()) {
        for (id, val) in providers_obj {
            if builtin_id_set.contains(id.as_str()) {
                continue;
            }
            let base_url = val["baseUrl"].as_str().unwrap_or_default();
            let api = val["api"].as_str().unwrap_or("openai-completions");
            let raw_key = val["apiKey"].as_str().unwrap_or_default();
            let api_key = if raw_key == "no-api-key" { "" } else { raw_key };
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

    let npm_packages: Vec<String> = settings_json
        .get("packages")
        .and_then(|v| v.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|p| {
                    p.as_str()
                        .map(str::to_string)
                        .or_else(|| p.get("source").and_then(|s| s.as_str()).map(str::to_string))
                })
                .collect()
        })
        .unwrap_or_default();

    let mut data = json!({
        "builtin": builtin,
        "custom": custom,
        "scope": scope_refs,
        "npmPackages": npm_packages,
    });
    if let Some(err) = models_error {
        data["modelsError"] = json!(err);
    }
    if let Some(err) = settings_error {
        data["settingsError"] = json!(err);
    }

    json!({
        "type": "pipkin_result",
        "ok": true,
        "data": data
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
    const LOCAL_PROVIDER: &str = "pipkin-local";
    const LEGACY_LOCAL_PROVIDER: &str = "oopbot-local";
    const PI_AI_CONTEXT_MARGIN: u64 = 4096;
    const KEPT_SHARE: u64 = 16;

    let base_url = params["baseUrl"].as_str().unwrap_or_default();
    let api_key = params["apiKey"].as_str().unwrap_or_default();
    let raw_models = params
        .get("models")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default();

    let models_path = config_dir.join("models.json");
    let mut models_json = read_json_file(&models_path);
    if !models_json.is_object() {
        models_json = json!({ "providers": {} });
    }
    if !models_json["providers"].is_object() {
        models_json["providers"] = json!({});
    }
    let before_models = models_json.clone();

    let settings_path = config_dir.join("settings.json");
    let (mut settings_json, settings_err) = read_json_object_with_error(&settings_path);
    let before_settings = settings_json.clone();

    let mut model_ids: Vec<String> = Vec::new();
    let mut normalized_models: Vec<Value> = Vec::new();
    for m in &raw_models {
        if let Some(id) = m.get("id").and_then(|v| v.as_str()).filter(|s| !s.is_empty()) {
            model_ids.push(id.to_string());
            let mut obj = m.as_object().cloned().unwrap_or_default();
            obj.insert("id".to_string(), json!(id));
            obj.insert("input".to_string(), json!(["text"]));
            obj.insert(
                "cost".to_string(),
                json!({ "input": 0, "output": 0, "cacheRead": 0, "cacheWrite": 0 }),
            );
            normalized_models.push(Value::Object(obj));
        }
    }

    if let Some(providers_obj) = models_json
        .get_mut("providers")
        .and_then(|v| v.as_object_mut())
    {
        providers_obj.remove(LEGACY_LOCAL_PROVIDER);
        if normalized_models.is_empty() {
            providers_obj.remove(LOCAL_PROVIDER);
        } else {
            providers_obj.insert(
                LOCAL_PROVIDER.to_string(),
                json!({
                    "baseUrl": base_url,
                    "api": "openai-completions",
                    "apiKey": api_key,
                    "compat": {
                        "sendSessionAffinityHeaders": true,
                        "sessionAffinityFormat": "openai-nosession",
                        "supportsDeveloperRole": false,
                        "supportsStore": false,
                        "supportsReasoningEffort": true,
                        "supportsUsageInStreaming": true,
                        "maxTokensField": "max_tokens",
                        "thinkingFormat": "openai"
                    },
                    "models": normalized_models,
                }),
            );
        }
    }

    let models_changed = models_json != before_models;
    if models_changed {
        if let Err(e) = write_json_file(&models_path, &models_json) {
            return json!({
                "type": "pipkin_result",
                "ok": false,
                "code": "io_error",
                "error": format!("Failed to write models.json: {}", e)
            });
        }
    }

    let mut settings_changed = false;
    if settings_err.is_none() {
        // 1. Update enabledModels scope jika sudah aktif (non-empty)
        if let Some(enabled_arr) = settings_json
            .get("enabledModels")
            .and_then(|v| v.as_array())
            .filter(|a| !a.is_empty())
            .cloned()
        {
            let mut next_enabled: Vec<Value> = enabled_arr
                .into_iter()
                .filter(|item| {
                    item.as_str()
                        .map(|s| {
                            !s.starts_with(&format!("{}/", LEGACY_LOCAL_PROVIDER))
                                && !s.starts_with(&format!("{}/", LOCAL_PROVIDER))
                        })
                        .unwrap_or(true)
                })
                .collect();
            for mid in &model_ids {
                let ref_str = format!("{}/{}", LOCAL_PROVIDER, mid);
                if !next_enabled
                    .iter()
                    .any(|item| item.as_str() == Some(ref_str.as_str()))
                {
                    next_enabled.push(json!(ref_str));
                }
            }
            settings_json["enabledModels"] = json!(next_enabled);
        }

        // 2. Update compaction.modelOverrides untuk tiap model lokal yang punya contextWindow & maxTokens
        let mut overrides = settings_json
            .get("compaction")
            .and_then(|c| c.get("modelOverrides"))
            .and_then(|o| o.as_object())
            .cloned()
            .unwrap_or_default();
        overrides.retain(|k, _| {
            !k.starts_with(&format!("{}/", LOCAL_PROVIDER))
                && !k.starts_with(&format!("{}/", LEGACY_LOCAL_PROVIDER))
        });
        for m in &raw_models {
            let Some(id) = m.get("id").and_then(|v| v.as_str()).filter(|s| !s.is_empty()) else {
                continue;
            };
            if let (Some(ctx), Some(max_tok)) = (
                m.get("contextWindow").and_then(|v| v.as_u64()),
                m.get("maxTokens").and_then(|v| v.as_u64()),
            ) {
                overrides.insert(
                    format!("{}/{}", LOCAL_PROVIDER, id),
                    json!({
                        "reserveTokens": max_tok + PI_AI_CONTEXT_MARGIN,
                        "keepRecentTokens": ctx / KEPT_SHARE,
                    }),
                );
            }
        }
        if overrides.is_empty() {
            if let Some(comp) = settings_json
                .get_mut("compaction")
                .and_then(|v| v.as_object_mut())
            {
                comp.remove("modelOverrides");
            }
        } else {
            if !settings_json["compaction"].is_object() {
                settings_json["compaction"] = json!({});
            }
            settings_json["compaction"]["modelOverrides"] = Value::Object(overrides);
        }

        settings_changed = settings_json != before_settings;
        if settings_changed {
            if let Err(e) = write_json_file(&settings_path, &settings_json) {
                return json!({
                    "type": "pipkin_result",
                    "ok": false,
                    "code": "io_error",
                    "error": format!("Failed to write settings.json: {}", e)
                });
            }
        }
    }

    json!({
        "type": "pipkin_result",
        "ok": true,
        "data": { "changed": models_changed || settings_changed }
    })
}

fn http_status_error_code(status: u16) -> &'static str {
    match status {
        401 | 403 => "unauthorized",
        404 => "not_found",
        429 => "rate_limited",
        500..=599 => "server_error",
        _ => "http_error",
    }
}

fn is_html_body(body: &str) -> bool {
    let trimmed = body.trim_start().to_ascii_lowercase();
    trimmed.starts_with("<!doctype")
        || trimmed.starts_with("<html")
        || trimmed.starts_with("<head")
        || trimmed.starts_with("<body")
}

fn extract_server_error_message(body: &str) -> String {
    if is_html_body(body) {
        return String::new();
    }
    if let Ok(parsed) = serde_json::from_str::<Value>(body) {
        let candidates = [
            parsed["error"]["message"].as_str(),
            parsed["error"].as_str(),
            parsed["message"].as_str(),
            parsed["detail"].as_str(),
        ];
        if let Some(msg) = candidates
            .into_iter()
            .flatten()
            .map(str::trim)
            .find(|s| !s.is_empty())
        {
            return msg.chars().take(300).collect();
        }
    }
    body.trim().chars().take(300).collect()
}

fn is_valid_http_url(raw: &str) -> bool {
    let lower = raw.to_ascii_lowercase();
    if let Some(rest) = lower.strip_prefix("http://").or_else(|| lower.strip_prefix("https://")) {
        let host = rest.split('/').next().unwrap_or("");
        !host.is_empty() && !host.contains(char::is_whitespace)
    } else {
        false
    }
}

async fn handle_provider_models(params: &Value) -> Value {
    let base_url = params["baseUrl"].as_str().unwrap_or("").trim();
    let api_key = params["apiKey"].as_str().unwrap_or("").trim();
    let api = params["api"].as_str().unwrap_or("openai-completions").trim();

    if !is_valid_http_url(base_url) {
        return json!({
            "type": "pipkin_result",
            "ok": false,
            "code": "bad_url",
            "error": format!("{} is not an http:// or https:// URL", base_url),
            "data": {}
        });
    }

    let client = match reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(15))
        .build()
    {
        Ok(c) => c,
        Err(e) => {
            return json!({
                "type": "pipkin_result",
                "ok": false,
                "code": "network",
                "error": format!("Could not reach {}: {}", base_url, e),
                "data": {}
            });
        }
    };

    let clean_base = base_url.trim_end_matches('/');
    let is_google = api == "google-generative-ai"
        || (clean_base.contains("googleapis.com") && !clean_base.contains("/openai"));
    let is_anthropic = api == "anthropic-messages" || clean_base.contains("anthropic.com");

    let url = if is_google {
        format!("{}/models?pageSize=100", clean_base)
    } else {
        format!("{}/models", clean_base)
    };

    let mut req = client.get(&url);
    if is_google {
        if !api_key.is_empty() {
            req = req.header("x-goog-api-key", api_key);
        }
    } else if is_anthropic {
        if !api_key.is_empty() {
            req = req
                .header("x-api-key", api_key)
                .header("anthropic-version", "2023-06-01");
        }
    } else {
        let bearer = if api_key.is_empty() {
            "no-api-key"
        } else {
            api_key
        };
        req = req.header("Authorization", format!("Bearer {}", bearer));
    }

    let resp = match req.send().await {
        Ok(r) => r,
        Err(e) => {
            if e.is_timeout() {
                return json!({
                    "type": "pipkin_result",
                    "ok": false,
                    "code": "timeout",
                    "error": format!("No response from {} within 15 s", base_url),
                    "data": {}
                });
            }
            let cause = e.to_string();
            return json!({
                "type": "pipkin_result",
                "ok": false,
                "code": "network",
                "error": format!("Could not reach {}: {}", base_url, cause),
                "i18n": {
                    "key": "provider_unreachable_because",
                    "args": { "url": base_url, "cause": cause }
                },
                "data": {}
            });
        }
    };

    let status = resp.status();
    let body_text = match resp.text().await {
        Ok(t) => t,
        Err(e) => {
            if e.is_timeout() {
                return json!({
                    "type": "pipkin_result",
                    "ok": false,
                    "code": "timeout",
                    "error": format!("No response from {} within 15 s", base_url),
                    "data": {}
                });
            }
            return json!({
                "type": "pipkin_result",
                "ok": false,
                "code": "network",
                "error": format!("Could not reach {}: {}", base_url, e),
                "data": {}
            });
        }
    };

    if !status.is_success() {
        let code = http_status_error_code(status.as_u16());
        let said = extract_server_error_message(&body_text);
        if said.is_empty() {
            return json!({
                "type": "pipkin_result",
                "ok": false,
                "code": code,
                "error": format!("HTTP {}", status.as_u16()),
                "i18n": { "key": "provider_http_status", "args": { "status": status.as_u16() } },
                "data": {}
            });
        }
        return json!({
            "type": "pipkin_result",
            "ok": false,
            "code": code,
            "error": format!("HTTP {}: {}", status.as_u16(), said),
            "i18n": {
                "key": "provider_http_status_said",
                "args": { "status": status.as_u16(), "message": said }
            },
            "data": {}
        });
    }

    let body_json: Value = match serde_json::from_str(&body_text) {
        Ok(v) => v,
        Err(_) => {
            if is_html_body(&body_text) {
                return json!({
                    "type": "pipkin_result",
                    "ok": false,
                    "code": "bad_response",
                    "error": format!("{} returned a web page, not JSON", url),
                    "i18n": { "key": "provider_web_page", "args": { "url": url } },
                    "data": {}
                });
            }
            let preview: String = body_text.chars().take(300).collect();
            return json!({
                "type": "pipkin_result",
                "ok": false,
                "code": "bad_response",
                "error": format!("The server did not return JSON: {}", preview),
                "i18n": { "key": "provider_not_json", "args": { "body": preview } },
                "data": {}
            });
        }
    };

    let has_openai_null_list =
        body_json.get("object").and_then(|v| v.as_str()) == Some("list")
            && body_json.get("data").is_some_and(|v| v.is_null());
    let has_valid_list = has_openai_null_list
        || body_json.get("data").and_then(|v| v.as_array()).is_some()
        || body_json.get("models").and_then(|v| v.as_array()).is_some()
        || body_json.is_array();

    if !has_valid_list {
        return json!({
            "type": "pipkin_result",
            "ok": false,
            "code": "bad_response",
            "error": "The server's /models response has no data list",
            "i18n": { "key": "provider_no_data_list" },
            "data": {}
        });
    }

    let provider_hint = if is_google {
        "google"
    } else if is_anthropic {
        "anthropic"
    } else if clean_base.contains("openrouter.ai") {
        "openrouter"
    } else {
        "custom"
    };
    let models = parse_models_response(&body_json, provider_hint);
    let models_json: Vec<Value> = models
        .iter()
        .map(|m| {
            let mut item = m.to_pipkin_json(None);
            item["api"] = json!(api);
            item
        })
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

fn safe_attachment_name(raw: &str) -> String {
    let normalized = raw.replace('\\', "/");
    let leaf = normalized.rsplit('/').next().unwrap_or("");
    let cleaned: String = leaf.chars().filter(|&c| c != '\0').collect();
    let trimmed = cleaned.trim_start_matches('.').trim();
    if trimmed.is_empty() {
        "file".to_string()
    } else {
        trimmed.to_string()
    }
}

fn unique_attachment_path(dir: &Path, safe_name: &str) -> PathBuf {
    let candidate = dir.join(safe_name);
    if !candidate.exists() {
        return candidate;
    }
    let path_obj = Path::new(safe_name);
    let stem = path_obj
        .file_stem()
        .and_then(|s| s.to_str())
        .filter(|s| !s.is_empty())
        .unwrap_or(safe_name);
    let ext = path_obj
        .extension()
        .and_then(|e| e.to_str())
        .filter(|e| !e.is_empty());
    for n in 2u32.. {
        let numbered = match ext {
            Some(e) => format!("{}-{}.{}", stem, n, e),
            None => format!("{}-{}", stem, n),
        };
        let next = dir.join(numbered);
        if !next.exists() {
            return next;
        }
    }
    unreachable!()
}

fn handle_save_attachments(params: &Value, config_dir: &Path) -> Value {
    let Some(files) = params.get("files").and_then(|f| f.as_array()) else {
        return json!({
            "type": "pipkin_result",
            "ok": false,
            "code": "bad_params",
            "error": "Parameter 'files' must be an array."
        });
    };
    if files.is_empty() {
        return json!({
            "type": "pipkin_result",
            "ok": false,
            "code": "bad_params",
            "error": "Parameter 'files' must not be empty."
        });
    }

    if let Some(cwd) = params.get("cwd").and_then(|v| v.as_str()).filter(|s| !s.is_empty()) {
        if read_remote_stand_in(config_dir, cwd).is_some() {
            return json!({
                "type": "pipkin_result",
                "ok": false,
                "code": "remote_not_connected",
                "error": "Remote workspace attachment upload requires an active SSH connection."
            });
        }
    }

    let mut decoded_entries: Vec<(String, Vec<u8>)> = Vec::with_capacity(files.len());
    for file in files {
        let Some(raw_name) = file.get("name").and_then(|v| v.as_str()) else {
            return json!({
                "type": "pipkin_result",
                "ok": false,
                "code": "bad_params",
                "error": "Each attachment must have a 'name' string."
            });
        };
        let Some(data) = file.get("data").and_then(|v| v.as_str()) else {
            return json!({
                "type": "pipkin_result",
                "ok": false,
                "code": "bad_params",
                "error": "Each attachment must have a base64 'data' string."
            });
        };
        let Some(bytes) = decode_base64(data) else {
            return json!({
                "type": "pipkin_result",
                "ok": false,
                "code": "bad_params",
                "error": format!("Invalid base64 payload for attachment '{}'.", raw_name)
            });
        };
        decoded_entries.push((safe_attachment_name(raw_name), bytes));
    }

    let seq = TMP_SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let attach_dir = config_dir
        .join("attachments")
        .join(format!("{}_{}_{}", epoch_ms(), std::process::id(), seq));
    if let Err(e) = std::fs::create_dir_all(&attach_dir) {
        return json!({
            "type": "pipkin_result",
            "ok": false,
            "code": "io_error",
            "error": format!("Failed to create attachments directory: {}", e)
        });
    }

    let Ok(canon_dir) = std::fs::canonicalize(&attach_dir) else {
        return json!({
            "type": "pipkin_result",
            "ok": false,
            "code": "io_error",
            "error": "Failed to resolve attachments directory."
        });
    };

    let mut paths = Vec::with_capacity(decoded_entries.len());
    for (name, bytes) in decoded_entries {
        let file_path = unique_attachment_path(&canon_dir, &name);
        if !file_path.starts_with(&canon_dir) {
            return json!({
                "type": "pipkin_result",
                "ok": false,
                "code": "outside_workspace",
                "error": "Attachment path escaped target directory."
            });
        }
        if let Err(e) = std::fs::write(&file_path, bytes) {
            return json!({
                "type": "pipkin_result",
                "ok": false,
                "code": "io_error",
                "error": format!("Failed to write attachment: {}", e)
            });
        }
        paths.push(file_path.to_string_lossy().to_string());
    }

    json!({
        "type": "pipkin_result",
        "ok": true,
        "data": { "paths": paths }
    })
}

const BACKUP_FORMAT: &str = "pipkin-config-backup";
const BACKUP_VERSION: u64 = 1;
const BACKUP_EXCLUDED: &[&str] = &["sessions", "bin", "tmp", "pi-debug.log", "crashes.json"];

fn backups_dir(config_dir: &Path) -> PathBuf {
    config_dir.parent().unwrap_or(config_dir).join("backups")
}

fn validate_backup_path(config_dir: &Path, name: &str) -> Result<PathBuf, Value> {
    let bad = name.is_empty()
        || name.starts_with('.')
        || !name.ends_with(".tar.gz")
        || name.contains('/')
        || name.contains('\\')
        || name.contains('\0');
    if bad {
        return Err(json!({
            "type": "pipkin_result",
            "ok": false,
            "code": "bad_name",
            "error": format!("Not a backup name: {}", name),
            "data": {}
        }));
    }
    Ok(backups_dir(config_dir).join(name))
}

fn compact_iso_stamp() -> String {
    let ms = epoch_ms();
    let secs = ms / 1000;
    let days = secs / 86_400;
    let rem = secs % 86_400;
    let hours = rem / 3600;
    let minutes = (rem % 3600) / 60;
    let seconds = rem % 60;
    let (year, month, day) = civil_from_days(days as i64);
    format!(
        "{:04}{:02}{:02}T{:02}{:02}{:02}Z",
        year, month, day, hours, minutes, seconds
    )
}

fn epoch_ms_to_iso(ms: u64) -> String {
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

fn copy_config_tree(
    src_dir: &Path,
    dst_dir: &Path,
    depth: usize,
    count: &mut usize,
    progress_cb: &mut dyn FnMut(&str, &str, usize),
) -> Result<(), String> {
    std::fs::create_dir_all(dst_dir).map_err(|e| e.to_string())?;
    let Ok(entries) = std::fs::read_dir(src_dir) else {
        return Ok(());
    };
    let mut items: Vec<_> = entries.flatten().collect();
    items.sort_by_key(|e| e.file_name());
    for entry in items {
        let name = entry.file_name().to_string_lossy().to_string();
        if depth == 0 && BACKUP_EXCLUDED.contains(&name.as_str()) {
            continue;
        }
        let src_path = entry.path();
        let dst_path = dst_dir.join(&name);
        let Ok(meta) = std::fs::symlink_metadata(&src_path) else {
            continue;
        };
        if meta.file_type().is_symlink() {
            #[cfg(unix)]
            if let Ok(target) = std::fs::read_link(&src_path) {
                let _ = std::os::unix::fs::symlink(target, &dst_path);
                *count += 1;
            }
        } else if meta.is_dir() {
            *count += 1;
            if *count == 1 || (*count).is_multiple_of(25) {
                let verb_noun = if *count == 1 { "item" } else { "items" };
                progress_cb(
                    &format!("Backed up {} {}", *count, verb_noun),
                    "backup_progress_backed_up",
                    *count,
                );
            }
            copy_config_tree(&src_path, &dst_path, depth + 1, count, progress_cb)?;
        } else if meta.is_file() {
            std::fs::copy(&src_path, &dst_path).map_err(|e| e.to_string())?;
            *count += 1;
            if *count == 1 || (*count).is_multiple_of(25) {
                let verb_noun = if *count == 1 { "item" } else { "items" };
                progress_cb(
                    &format!("Backed up {} {}", *count, verb_noun),
                    "backup_progress_backed_up",
                    *count,
                );
            }
        }
    }
    Ok(())
}

fn read_backup_manifest_from_tar(archive: &Path) -> Result<(Value, Vec<String>), (&'static str, &'static str, String)> {
    let list_out = std::process::Command::new("tar")
        .arg("-tzf")
        .arg(archive)
        .output()
        .map_err(|e| ("bad_backup", "backup_unreadable", format!("tar failed: {}", e)))?;
    if !list_out.status.success() {
        return Err((
            "bad_backup",
            "backup_damaged",
            format!(
                "The backup is damaged: {}",
                String::from_utf8_lossy(&list_out.stderr).trim()
            ),
        ));
    }
    let raw_list = String::from_utf8_lossy(&list_out.stdout);
    let entries: Vec<String> = raw_list
        .lines()
        .map(|l| l.trim().trim_start_matches("./").to_string())
        .filter(|l| !l.is_empty())
        .collect();
    if entries.is_empty() {
        return Err(("not_backup", "archive_empty", "The archive is empty".to_string()));
    }
    let first_raw = raw_list.lines().next().unwrap_or("").trim();
    if entries[0] != "manifest.json" {
        return Err((
            "not_backup",
            "backup_not_pipkin",
            "This file is not a Pipkin configuration backup".to_string(),
        ));
    }

    let manifest_out = std::process::Command::new("tar")
        .arg("-xOzf")
        .arg(archive)
        .arg(first_raw)
        .output()
        .map_err(|e| ("bad_backup", "backup_unreadable", format!("tar read failed: {}", e)))?;
    if !manifest_out.status.success() {
        return Err((
            "not_backup",
            "backup_not_pipkin",
            "This file is not a Pipkin configuration backup".to_string(),
        ));
    }
    let parsed: Value = serde_json::from_slice(&manifest_out.stdout).map_err(|_| {
        (
            "not_backup",
            "backup_not_pipkin",
            "This file is not a Pipkin configuration backup".to_string(),
        )
    })?;
    if parsed.get("format").and_then(|v| v.as_str()) != Some(BACKUP_FORMAT) {
        return Err((
            "not_backup",
            "backup_not_pipkin",
            "This file is not a Pipkin configuration backup".to_string(),
        ));
    }
    if parsed.get("version").and_then(|v| v.as_u64()) != Some(BACKUP_VERSION) {
        return Err((
            "backup_version",
            "backup_from_newer_pipkin",
            "This backup is from a newer Pipkin; update Pipkin to restore it".to_string(),
        ));
    }
    Ok((parsed, entries))
}

fn validate_backup_entries(entries: &[String]) -> Result<(), (&'static str, &'static str, String)> {
    let mut seen_paths = std::collections::HashSet::new();
    for raw_entry in entries.iter().skip(1) {
        let trimmed = raw_entry.trim_end_matches('/');
        if trimmed.is_empty() || trimmed == "config" {
            continue;
        }
        if !trimmed.starts_with("config/") {
            return Err((
                "bad_backup",
                "backup_path_outside_config",
                format!("The backup holds a file outside the configuration: {}", trimmed),
            ));
        }
        let rel = &trimmed["config/".len()..];
        let segs: Vec<&str> = rel.split('/').collect();
        if segs.iter().any(|s| s.is_empty() || *s == "." || *s == ".." || s.contains('\0')) {
            return Err((
                "bad_backup",
                "backup_path_unsafe",
                format!("The backup holds an unsafe path: {}", trimmed),
            ));
        }
        if let Some(first) = segs.first() {
            if BACKUP_EXCLUDED.contains(first) {
                return Err((
                    "bad_backup",
                    "backup_holds_excluded",
                    format!("The backup holds {}, which a restore never replaces", first),
                ));
            }
        }
        if !seen_paths.insert(rel.to_string()) {
            return Err((
                "bad_backup",
                "backup_path_twice",
                format!("The backup holds {} twice", rel),
            ));
        }
    }
    Ok(())
}

fn backup_info_json(archive: &Path, name: &str) -> Value {
    let meta = std::fs::metadata(archive).ok();
    let size = meta.as_ref().map(|m| m.len()).unwrap_or(0);
    let mtime_ms = meta
        .and_then(|m| m.modified().ok())
        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
        .map(|d| d.as_millis() as u64)
        .unwrap_or_else(epoch_ms);
    let fallback_iso = epoch_ms_to_iso(mtime_ms);

    match read_backup_manifest_from_tar(archive) {
        Ok((manifest, _)) => {
            let created_at = manifest
                .get("createdAt")
                .and_then(|v| v.as_str())
                .filter(|s| !s.is_empty() && parse_iso_to_epoch_ms(s).is_some())
                .unwrap_or(&fallback_iso);
            let pi_version = manifest.get("piVersion").cloned().unwrap_or(Value::Null);
            json!({
                "name": name,
                "size": size,
                "createdAt": created_at,
                "piVersion": pi_version,
                "error": Value::Null
            })
        }
        Err((_code, i18n_key, message)) => json!({
            "name": name,
            "size": size,
            "createdAt": fallback_iso,
            "piVersion": Value::Null,
            "error": message,
            "errorI18n": { "key": i18n_key }
        }),
    }
}

fn handle_list_backups(config_dir: &Path) -> Value {
    let dir = backups_dir(config_dir);
    let mut backups = Vec::new();
    if let Ok(entries) = std::fs::read_dir(&dir) {
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().to_string();
            if name.starts_with('.') || !name.ends_with(".tar.gz") {
                continue;
            }
            let path = entry.path();
            if path.is_file() {
                backups.push(backup_info_json(&path, &name));
            }
        }
    }
    backups.sort_by(|a, b| {
        let ca = a["createdAt"].as_str().unwrap_or("");
        let cb = b["createdAt"].as_str().unwrap_or("");
        let na = a["name"].as_str().unwrap_or("");
        let nb = b["name"].as_str().unwrap_or("");
        cb.cmp(ca).then_with(|| nb.cmp(na))
    });
    json!({
        "type": "pipkin_result",
        "ok": true,
        "data": {
            "dir": dir.to_string_lossy(),
            "backups": backups
        }
    })
}

fn handle_backup_create(
    config_dir: &Path,
    progress_cb: &mut dyn FnMut(&str, &str, usize),
) -> Value {
    let dir = backups_dir(config_dir);
    if let Err(e) = std::fs::create_dir_all(&dir) {
        return json!({
            "type": "pipkin_result",
            "ok": false,
            "code": "io_error",
            "error": format!("Failed to create backups directory: {}", e),
            "data": {}
        });
    }

    let stamp = compact_iso_stamp();
    let mut candidate_name = format!("pipkin-config-{}.tar.gz", stamp);
    let mut counter = 2u32;
    while dir.join(&candidate_name).exists() {
        candidate_name = format!("pipkin-config-{}-{}.tar.gz", stamp, counter);
        counter += 1;
    }

    let seq = TMP_SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let staging = dir.join(format!(".staging-{}-{}", std::process::id(), seq));
    let partial = dir.join(format!(".{}.partial-{}", candidate_name, std::process::id()));
    let final_path = dir.join(&candidate_name);

    let cleanup = |staging: &Path, partial: &Path| {
        let _ = std::fs::remove_dir_all(staging);
        let _ = std::fs::remove_file(partial);
    };

    if let Err(e) = std::fs::create_dir_all(staging.join("config")) {
        cleanup(&staging, &partial);
        return json!({
            "type": "pipkin_result",
            "ok": false,
            "code": "io_error",
            "error": format!("Failed to stage backup: {}", e),
            "data": {}
        });
    }

    let created_at = iso_now();
    let manifest = json!({
        "format": BACKUP_FORMAT,
        "version": BACKUP_VERSION,
        "createdAt": created_at,
        "piVersion": env!("CARGO_PKG_VERSION"),
    });
    if let Err(e) = write_json_file(&staging.join("manifest.json"), &manifest) {
        cleanup(&staging, &partial);
        return json!({
            "type": "pipkin_result",
            "ok": false,
            "code": "io_error",
            "error": format!("Failed to write backup manifest: {}", e),
            "data": {}
        });
    }

    let mut count = 0usize;
    if let Err(e) = copy_config_tree(config_dir, &staging.join("config"), 0, &mut count, progress_cb)
    {
        cleanup(&staging, &partial);
        return json!({
            "type": "pipkin_result",
            "ok": false,
            "code": "io_error",
            "error": format!("Failed to copy configuration: {}", e),
            "data": {}
        });
    }

    let tar_out = std::process::Command::new("tar")
        .arg("-czf")
        .arg(&partial)
        .arg("-C")
        .arg(&staging)
        .arg("manifest.json")
        .arg("config")
        .output();

    let _ = std::fs::remove_dir_all(&staging);
    match tar_out {
        Ok(o) if o.status.success() => {}
        Ok(o) => {
            let _ = std::fs::remove_file(&partial);
            return json!({
                "type": "pipkin_result",
                "ok": false,
                "code": "io_error",
                "error": format!("tar failed: {}", String::from_utf8_lossy(&o.stderr).trim()),
                "data": {}
            });
        }
        Err(e) => {
            let _ = std::fs::remove_file(&partial);
            return json!({
                "type": "pipkin_result",
                "ok": false,
                "code": "io_error",
                "error": format!("tar not available: {}", e),
                "data": {}
            });
        }
    }

    if let Err(e) = std::fs::rename(&partial, &final_path) {
        let _ = std::fs::remove_file(&partial);
        return json!({
            "type": "pipkin_result",
            "ok": false,
            "code": "io_error",
            "error": format!("Failed to finalize backup: {}", e),
            "data": {}
        });
    }

    let info = backup_info_json(&final_path, &candidate_name);
    json!({
        "type": "pipkin_result",
        "ok": true,
        "data": { "backup": info }
    })
}

fn handle_backup_delete(config_dir: &Path, params: &Value) -> Value {
    let name = params["name"].as_str().unwrap_or("");
    let path = match validate_backup_path(config_dir, name) {
        Ok(p) => p,
        Err(err_res) => return err_res,
    };
    if let Err(e) = std::fs::remove_file(&path) {
        if e.kind() != std::io::ErrorKind::NotFound {
            return json!({
                "type": "pipkin_result",
                "ok": false,
                "code": "io_error",
                "error": format!("Failed to delete backup: {}", e),
                "data": {}
            });
        }
    }
    json!({
        "type": "pipkin_result",
        "ok": true,
        "data": {}
    })
}

fn handle_backup_restore(
    config_dir: &Path,
    params: &Value,
    progress_cb: &mut dyn FnMut(&str, &str, usize),
) -> Value {
    let name = params["name"].as_str().unwrap_or("");
    let archive = match validate_backup_path(config_dir, name) {
        Ok(p) => p,
        Err(err_res) => return err_res,
    };
    if !archive.is_file() {
        return json!({
            "type": "pipkin_result",
            "ok": false,
            "code": "not_found",
            "error": "This backup no longer exists",
            "i18n": { "key": "backup_missing" },
            "data": {}
        });
    }

    let (_manifest, entries) = match read_backup_manifest_from_tar(&archive) {
        Ok(v) => v,
        Err((code, key, msg)) => {
            return json!({
                "type": "pipkin_result",
                "ok": false,
                "code": code,
                "error": msg,
                "i18n": { "key": key },
                "data": {}
            });
        }
    };
    if let Err((code, key, msg)) = validate_backup_entries(&entries) {
        return json!({
            "type": "pipkin_result",
            "ok": false,
            "code": code,
            "error": msg,
            "i18n": { "key": key },
            "data": {}
        });
    }

    let parent = config_dir.parent().unwrap_or(config_dir);
    let seq = TMP_SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let id = format!("{}-{}-{}", epoch_ms(), std::process::id(), seq);
    let staging = parent.join(format!(
        "{}.restore-{}",
        config_dir.file_name().and_then(|s| s.to_str()).unwrap_or("agent"),
        id
    ));
    let old_dir = parent.join(format!(
        "{}.old-{}",
        config_dir.file_name().and_then(|s| s.to_str()).unwrap_or("agent"),
        id
    ));
    if let Err(e) = std::fs::create_dir_all(&staging) {
        return json!({
            "type": "pipkin_result",
            "ok": false,
            "code": "io_error",
            "error": format!("Failed to create restore staging folder: {}", e),
            "data": {}
        });
    }

    let tar_x = std::process::Command::new("tar")
        .arg("-xzf")
        .arg(&archive)
        .arg("-C")
        .arg(&staging)
        .output();
    match tar_x {
        Ok(o) if o.status.success() => {}
        Ok(o) => {
            let _ = std::fs::remove_dir_all(&staging);
            return json!({
                "type": "pipkin_result",
                "ok": false,
                "code": "bad_backup",
                "error": format!("The backup is damaged: {}", String::from_utf8_lossy(&o.stderr).trim()),
                "data": {}
            });
        }
        Err(e) => {
            let _ = std::fs::remove_dir_all(&staging);
            return json!({
                "type": "pipkin_result",
                "ok": false,
                "code": "io_error",
                "error": format!("tar failed: {}", e),
                "data": {}
            });
        }
    }

    let staged_config = staging.join("config");
    progress_cb("Restoring configuration", "backup_progress_restored", entries.len().saturating_sub(1));

    let _ = std::fs::create_dir_all(config_dir);
    let _ = std::fs::create_dir_all(&old_dir);
    if let Ok(cur_entries) = std::fs::read_dir(config_dir) {
        for entry in cur_entries.flatten() {
            let ename = entry.file_name().to_string_lossy().to_string();
            if BACKUP_EXCLUDED.contains(&ename.as_str()) {
                continue;
            }
            let _ = std::fs::rename(entry.path(), old_dir.join(&ename));
        }
    }

    if let Ok(restored_entries) = std::fs::read_dir(&staged_config) {
        for entry in restored_entries.flatten() {
            let ename = entry.file_name().to_string_lossy().to_string();
            if BACKUP_EXCLUDED.contains(&ename.as_str()) {
                continue;
            }
            if let Err(e) = std::fs::rename(entry.path(), config_dir.join(&ename)) {
                // Rollback non-excluded entries if rename fails
                if let Ok(old_entries) = std::fs::read_dir(&old_dir) {
                    for old_entry in old_entries.flatten() {
                        let _ = std::fs::rename(
                            old_entry.path(),
                            config_dir.join(old_entry.file_name()),
                        );
                    }
                }
                let _ = std::fs::remove_dir_all(&old_dir);
                let _ = std::fs::remove_dir_all(&staging);
                return json!({
                    "type": "pipkin_result",
                    "ok": false,
                    "code": "io_error",
                    "error": format!("Failed to move restored files into place: {}", e),
                    "data": {}
                });
            }
        }
    }

    let _ = std::fs::remove_dir_all(&old_dir);
    let _ = std::fs::remove_dir_all(&staging);

    json!({
        "type": "pipkin_result",
        "ok": true,
        "data": {}
    })
}

fn handle_subagent_sessions(config_dir: &Path, params: &Value) -> Value {
    let Some(raw_path) = params.get("path").and_then(|v| v.as_str()).filter(|s| !s.is_empty()) else {
        return json!({
            "type": "pipkin_result",
            "ok": false,
            "code": "bad_params",
            "error": "Parameter 'path' is required.",
            "data": {}
        });
    };

    let sessions_root = config_dir.join("sessions");
    let candidate = PathBuf::from(raw_path);
    let target = if candidate.is_absolute() {
        candidate
    } else {
        sessions_root.join(candidate)
    };
    let Ok(canon_root) = std::fs::canonicalize(&sessions_root) else {
        return json!({
            "type": "pipkin_result",
            "ok": true,
            "data": { "sessions": [] }
        });
    };
    let Ok(canon_parent) = std::fs::canonicalize(&target) else {
        return json!({
            "type": "pipkin_result",
            "ok": true,
            "data": { "sessions": [] }
        });
    };
    if !canon_parent.starts_with(&canon_root) || !canon_parent.is_file() {
        return json!({
            "type": "pipkin_result",
            "ok": false,
            "code": "outside_workspace",
            "error": "Session path is outside sessions directory.",
            "data": {}
        });
    }

    let Ok(content) = std::fs::read_to_string(&canon_parent) else {
        return json!({
            "type": "pipkin_result",
            "ok": true,
            "data": { "sessions": [] }
        });
    };
    let Some(first_line) = content.lines().next() else {
        return json!({
            "type": "pipkin_result",
            "ok": true,
            "data": { "sessions": [] }
        });
    };
    let Ok(header) = serde_json::from_str::<Value>(first_line) else {
        return json!({
            "type": "pipkin_result",
            "ok": true,
            "data": { "sessions": [] }
        });
    };
    let Some(sess_id) = header.get("id").and_then(|v| v.as_str()).filter(|s| is_valid_session_id(s)) else {
        return json!({
            "type": "pipkin_result",
            "ok": true,
            "data": { "sessions": [] }
        });
    };

    let files = subagent_transcript_files(config_dir, &canon_parent, sess_id);
    let mut sessions = Vec::new();
    for file in files {
        let Ok(text) = std::fs::read_to_string(&file) else {
            continue;
        };
        let mut hdr: Option<Value> = None;
        let mut link: Option<Value> = None;
        let mut msg_count = 0usize;
        for line in text.lines() {
            let trimmed = line.trim();
            if trimmed.is_empty() {
                continue;
            }
            let Ok(entry) = serde_json::from_str::<Value>(trimmed) else {
                continue;
            };
            if !entry.is_object() {
                continue;
            }
            if hdr.is_none() {
                if entry["type"].as_str() != Some("session") {
                    hdr = None;
                    break;
                }
                hdr = Some(entry);
            } else if entry["type"].as_str() == Some("message") {
                msg_count += 1;
            } else if entry["type"].as_str() == Some("custom")
                && entry["customType"].as_str() == Some("pipkin-subagent")
                && entry["data"].is_object()
                && link.is_none()
            {
                link = Some(entry["data"].clone());
            }
        }
        let (Some(h), Some(l)) = (hdr, link) else {
            continue;
        };
        let created = h
            .get("timestamp")
            .and_then(|v| {
                v.as_u64()
                    .or_else(|| v.as_str().and_then(parse_iso_to_epoch_ms))
            });
        let modified = std::fs::metadata(&file)
            .and_then(|m| m.modified())
            .ok()
            .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0);
        sessions.push(json!({
            "path": file.to_string_lossy(),
            "toolCallId": l.get("toolCallId").and_then(|v| v.as_str()),
            "agent": l.get("agent").and_then(|v| v.as_str()),
            "description": l.get("description").and_then(|v| v.as_str()),
            "created": created,
            "modified": modified,
            "messageCount": msg_count
        }));
    }
    sessions.sort_by_key(|s| s["created"].as_u64().unwrap_or(0));
    json!({
        "type": "pipkin_result",
        "ok": true,
        "data": { "sessions": sessions }
    })
}

fn remote_ssh_dir(config_dir: &Path) -> PathBuf {
    config_dir.parent().unwrap_or(config_dir).join("ssh")
}

fn remote_remotes_dir(config_dir: &Path) -> PathBuf {
    config_dir.parent().unwrap_or(config_dir).join("remotes")
}

fn read_remote_stand_in(config_dir: &Path, cwd: &str) -> Option<(String, String)> {
    let remotes_dir = remote_remotes_dir(config_dir);
    let canon_remotes = std::fs::canonicalize(&remotes_dir).ok()?;
    let canon_cwd = std::fs::canonicalize(cwd).ok()?;
    if !canon_cwd.starts_with(&canon_remotes) {
        return None;
    }
    let manifest_text = std::fs::read_to_string(canon_cwd.join("remote.json")).ok()?;
    let manifest: Value = serde_json::from_str(&manifest_text).ok()?;
    if manifest.get("version").and_then(|v| v.as_u64()) != Some(1) {
        return None;
    }
    let host_id = manifest.get("hostId")?.as_str()?.to_string();
    let path = manifest.get("path")?.as_str()?.to_string();
    if !path.starts_with('/') {
        return None;
    }
    Some((host_id, path))
}

fn find_termux_bin(name: &str) -> Option<PathBuf> {
    if let Ok(prefix) = env::var("PREFIX") {
        let p = Path::new(&prefix).join("bin").join(name);
        if p.is_file() {
            return Some(p);
        }
    }
    for dir in ["/data/data/com.termux/files/usr/bin", "/usr/bin", "/bin"] {
        let p = Path::new(dir).join(name);
        if p.is_file() {
            return Some(p);
        }
    }
    None
}

fn is_valid_host_id(id: &str) -> bool {
    id.len() == 12 && id.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
}

fn generate_host_id() -> String {
    let seed = format!(
        "{}-{}-{}",
        epoch_ms(),
        std::process::id(),
        TMP_SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
    );
    sha256_hex(seed.as_bytes())[..12].to_string()
}

fn parse_remote_address(address: &str) -> Option<(Option<&str>, &str)> {
    let (user, host) = match address.rfind('@') {
        Some(idx) => {
            let u = &address[..idx];
            if u.is_empty()
                || u.starts_with('-')
                || u.chars()
                    .any(|c| c.is_whitespace() || matches!(c, '"' | '\'' | '#' | '\\' | '@'))
            {
                return None;
            }
            (Some(u), &address[idx + 1..])
        }
        None => (None, address),
    };
    if host.is_empty() {
        return None;
    }
    if let Some(inner) = host.strip_prefix('[').and_then(|s| s.strip_suffix(']')) {
        if !inner.is_empty()
            && inner.contains(':')
            && inner
                .chars()
                .all(|c| c.is_ascii_hexdigit() || matches!(c, '.' | ':' | '%' | 'A'..='Z' | 'a'..='z' | '0'..='9' | '_' | '-'))
        {
            return Some((user, inner));
        }
        return None;
    }
    let first = host.bytes().next()?;
    if !(first.is_ascii_alphanumeric() || first == b'_') {
        return None;
    }
    if host
        .bytes()
        .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'.' | b'-'))
    {
        Some((user, host))
    } else {
        None
    }
}

fn is_valid_ssh_alias(alias: &str) -> bool {
    !alias.is_empty()
        && alias
            .bytes()
            .next()
            .is_some_and(|b| b.is_ascii_alphanumeric() || b == b'_')
        && alias
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'.' | b'-'))
}

fn load_remote_hosts_list(ssh_dir: &Path) -> Vec<Value> {
    let doc = read_json_file(&ssh_dir.join("hosts.json"));
    doc.get("hosts")
        .and_then(|v| v.as_array())
        .map(|arr| {
            arr.iter()
                .filter(|h| {
                    h.is_object()
                        && h["id"].as_str().is_some_and(is_valid_host_id)
                        && h["name"].as_str().is_some()
                        && matches!(h["auth"].as_str(), Some("key" | "password" | "config"))
                        && h["address"].as_str().is_some()
                })
                .cloned()
                .collect()
        })
        .unwrap_or_default()
}

fn write_remote_hosts_and_ssh_config(ssh_dir: &Path, home: &str, hosts: &[Value]) -> Result<(), String> {
    let sockets = ssh_dir.join("s");
    std::fs::create_dir_all(&sockets).map_err(|e| e.to_string())?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(ssh_dir, std::fs::Permissions::from_mode(0o700));
        let _ = std::fs::set_permissions(&sockets, std::fs::Permissions::from_mode(0o700));
    }

    let hosts_path = ssh_dir.join("hosts.json");
    write_json_file(&hosts_path, &json!({ "version": 1, "hosts": hosts }))
        .map_err(|e| e.to_string())?;

    let known_hosts = ssh_dir.join("known_hosts");
    let key_file = ssh_dir.join("id_ed25519");
    let quote_word = |v: &str| {
        if v.chars().any(char::is_whitespace) {
            format!("\"{}\"", v)
        } else {
            v.to_string()
        }
    };
    let mut lines = vec![
        "# Written by Pipkin from hosts.json; changes here are overwritten.".to_string(),
        "Host *".to_string(),
        "  ServerAliveInterval 15".to_string(),
        "  ServerAliveCountMax 2".to_string(),
        "  ConnectTimeout 15".to_string(),
        format!(
            "  UserKnownHostsFile {} {}",
            quote_word(&known_hosts.to_string_lossy()),
            quote_word(&Path::new(home).join(".ssh/known_hosts").to_string_lossy())
        ),
        "  HashKnownHosts no".to_string(),
    ];
    for h in hosts {
        let auth = h["auth"].as_str().unwrap_or("");
        if auth == "config" {
            continue;
        }
        let id = h["id"].as_str().unwrap_or("");
        let addr = h["address"].as_str().unwrap_or("");
        let Some((user, host)) = parse_remote_address(addr) else {
            continue;
        };
        let port = h["port"].as_u64().unwrap_or(22);
        lines.push(String::new());
        lines.push(format!("Host pipkin-{}", id));
        lines.push(format!("  HostName {}", host));
        if let Some(u) = user {
            lines.push(format!("  User {}", u));
        }
        lines.push(format!("  Port {}", port));
        lines.push(format!("  IdentityFile {}", quote_word(&key_file.to_string_lossy())));
        lines.push("  IdentitiesOnly yes".to_string());
        lines.push(format!(
            "  PreferredAuthentications {}",
            if auth == "password" {
                "password,keyboard-interactive"
            } else {
                "publickey"
            }
        ));
    }
    lines.push(String::new());
    lines.push("Match all".to_string());
    lines.push(format!(
        "  Include {}",
        quote_word(&Path::new(home).join(".ssh/config").to_string_lossy())
    ));
    lines.push(String::new());
    let cfg_path = ssh_dir.join("config");
    std::fs::write(&cfg_path, lines.join("\n")).map_err(|e| e.to_string())?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&hosts_path, std::fs::Permissions::from_mode(0o600));
        let _ = std::fs::set_permissions(&cfg_path, std::fs::Permissions::from_mode(0o600));
    }
    Ok(())
}

fn list_remote_stand_ins(remotes_dir: &Path) -> Vec<Value> {
    let mut out = Vec::new();
    let Ok(host_dirs) = std::fs::read_dir(remotes_dir) else {
        return out;
    };
    for host_entry in host_dirs.flatten() {
        let Ok(hashes) = std::fs::read_dir(host_entry.path()) else {
            continue;
        };
        for hash_entry in hashes.flatten() {
            let cwd_path = hash_entry.path();
            let Ok(text) = std::fs::read_to_string(cwd_path.join("remote.json")) else {
                continue;
            };
            let Ok(manifest) = serde_json::from_str::<Value>(&text) else {
                continue;
            };
            if manifest.get("version").and_then(|v| v.as_u64()) != Some(1) {
                continue;
            }
            if let (Some(hid), Some(rpath)) = (
                manifest.get("hostId").and_then(|v| v.as_str()),
                manifest.get("path").and_then(|v| v.as_str()).filter(|s| s.starts_with('/')),
            ) {
                out.push(json!({
                    "cwd": cwd_path.to_string_lossy(),
                    "hostId": hid,
                    "path": rpath
                }));
            }
        }
    }
    out
}

fn handle_remote_hosts(config_dir: &Path, _home: &str) -> Value {
    let ssh_dir = remote_ssh_dir(config_dir);
    let remotes_dir = remote_remotes_dir(config_dir);
    let ssh_installed = find_termux_bin("ssh").is_some();
    let public_key = std::fs::read_to_string(ssh_dir.join("id_ed25519.pub"))
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty());
    let hosts = load_remote_hosts_list(&ssh_dir);
    let workspaces = list_remote_stand_ins(&remotes_dir);
    json!({
        "type": "pipkin_result",
        "ok": true,
        "data": {
            "sshInstalled": ssh_installed,
            "publicKey": public_key,
            "hosts": hosts,
            "workspaces": workspaces
        }
    })
}

fn invalid_remote_host(field: &str, key: &str, msg: &str) -> Value {
    json!({
        "type": "pipkin_result",
        "ok": false,
        "code": "remote_host_invalid",
        "error": msg,
        "i18n": { "key": key, "args": { "field": field } },
        "data": {}
    })
}

fn handle_remote_host_save(config_dir: &Path, home: &str, params: &Value) -> Value {
    let Some(draft) = params.get("host").and_then(|v| v.as_object()) else {
        return json!({
            "type": "pipkin_result",
            "ok": false,
            "code": "bad_params",
            "error": "Missing host",
            "data": {}
        });
    };
    let id = match draft.get("id") {
        None | Some(Value::Null) => None,
        Some(Value::String(s)) if is_valid_host_id(s) => Some(s.clone()),
        _ => {
            return json!({
                "type": "pipkin_result",
                "ok": false,
                "code": "bad_params",
                "error": "Bad host id",
                "data": {}
            });
        }
    };
    let Some(auth) = draft
        .get("auth")
        .and_then(|v| v.as_str())
        .filter(|s| matches!(*s, "key" | "password" | "config"))
    else {
        return json!({
            "type": "pipkin_result",
            "ok": false,
            "code": "bad_params",
            "error": "Bad host sign-in method",
            "data": {}
        });
    };

    let name = draft
        .get("name")
        .and_then(|v| v.as_str())
        .map(str::trim)
        .unwrap_or("");
    if name.is_empty() {
        return invalid_remote_host("name", "remote_name_empty", "Enter a name.");
    }
    if name.chars().any(char::is_control) {
        return invalid_remote_host(
            "name",
            "remote_name_invalid",
            "The name can't contain control characters.",
        );
    }

    let ssh_dir = remote_ssh_dir(config_dir);
    let mut hosts = load_remote_hosts_list(&ssh_dir);
    let folded = name.to_ascii_lowercase();
    if hosts.iter().any(|h| {
        h["id"].as_str() != id.as_deref()
            && h["name"]
                .as_str()
                .is_some_and(|n| n.to_ascii_lowercase() == folded)
    }) {
        return json!({
            "type": "pipkin_result",
            "ok": false,
            "code": "remote_host_invalid",
            "error": format!("Another host is named {}.", name),
            "i18n": { "key": "remote_name_taken", "args": { "field": "name", "name": name } },
            "data": {}
        });
    }

    let address = draft
        .get("address")
        .and_then(|v| v.as_str())
        .map(str::trim)
        .unwrap_or("");
    if address.is_empty() {
        return invalid_remote_host("address", "remote_address_empty", "Enter an address.");
    }
    let port: Option<u64> = if auth == "config" {
        if !is_valid_ssh_alias(address) {
            return invalid_remote_host(
                "address",
                "remote_alias_invalid",
                &format!("{} isn't a Host alias.", address),
            );
        }
        None
    } else {
        if parse_remote_address(address).is_none() {
            return invalid_remote_host(
                "address",
                "remote_address_invalid",
                &format!("{} isn't an address like user@host.", address),
            );
        }
        let Some(p) = draft.get("port").and_then(|v| v.as_u64()).filter(|p| (1..=65535).contains(p)) else {
            return invalid_remote_host(
                "port",
                "remote_port_invalid",
                "The port must be a number from 1 to 65535.",
            );
        };
        Some(p)
    };

    let existing_idx = id
        .as_deref()
        .and_then(|eid| hosts.iter().position(|h| h["id"].as_str() == Some(eid)));
    if id.is_some() && existing_idx.is_none() {
        return json!({
            "type": "pipkin_result",
            "ok": false,
            "code": "remote_host_unknown",
            "error": "Remote host not found.",
            "data": {}
        });
    }

    let home_val = existing_idx
        .and_then(|idx| {
            let ex = &hosts[idx];
            let moved = ex["address"].as_str() != Some(address)
                || ex["port"].as_u64() != port
                || ex["auth"].as_str() != Some(auth);
            if moved {
                None
            } else {
                ex.get("home").cloned()
            }
        })
        .unwrap_or(Value::Null);

    let saved_id = id.unwrap_or_else(generate_host_id);
    let saved = json!({
        "id": saved_id,
        "name": name,
        "auth": auth,
        "address": address,
        "port": port,
        "home": home_val
    });

    match existing_idx {
        Some(idx) => hosts[idx] = saved.clone(),
        None => hosts.push(saved.clone()),
    }

    if let Err(e) = write_remote_hosts_and_ssh_config(&ssh_dir, home, &hosts) {
        return json!({
            "type": "pipkin_result",
            "ok": false,
            "code": "io_error",
            "error": format!("Failed to write hosts config: {}", e),
            "data": {}
        });
    }

    json!({
        "type": "pipkin_result",
        "ok": true,
        "data": { "host": saved }
    })
}

fn disconnect_ssh_control(ssh_dir: &Path, host_id: &str) {
    let sock = ssh_dir.join("s").join(host_id);
    if sock.exists() {
        if let Some(ssh) = find_termux_bin("ssh") {
            let _ = std::process::Command::new(ssh)
                .args(["-S", &sock.to_string_lossy(), "-O", "exit", "pipkin-ctrl"])
                .output();
        }
        let _ = std::fs::remove_file(&sock);
    }
}

fn handle_remote_host_remove(config_dir: &Path, home: &str, params: &Value) -> Value {
    let id = params["id"].as_str().unwrap_or("");
    let ssh_dir = remote_ssh_dir(config_dir);
    let mut hosts = load_remote_hosts_list(&ssh_dir);
    let before = hosts.len();
    hosts.retain(|h| h["id"].as_str() != Some(id));
    if hosts.len() != before {
        disconnect_ssh_control(&ssh_dir, id);
        let _ = write_remote_hosts_and_ssh_config(&ssh_dir, home, &hosts);
    }
    json!({
        "type": "pipkin_result",
        "ok": true,
        "data": {}
    })
}

fn normalize_remote_path(raw: &str) -> Result<String, Value> {
    if !raw.starts_with('/') || raw.contains('\0') {
        return Err(json!({
            "type": "pipkin_result",
            "ok": false,
            "code": "bad_params",
            "error": format!("Not an absolute remote path: {}", raw),
            "data": {}
        }));
    }
    let mut parts: Vec<&str> = Vec::new();
    for seg in raw.split('/') {
        match seg {
            "" | "." => {}
            ".." => {
                parts.pop();
            }
            other => parts.push(other),
        }
    }
    if parts.is_empty() {
        Ok("/".to_string())
    } else {
        Ok(format!("/{}", parts.join("/")))
    }
}

fn handle_remote_workspace(config_dir: &Path, params: &Value) -> Value {
    let host_id = params["hostId"].as_str().unwrap_or("");
    let raw_path = params["path"].as_str().unwrap_or("");
    let ssh_dir = remote_ssh_dir(config_dir);
    let hosts = load_remote_hosts_list(&ssh_dir);
    if !hosts.iter().any(|h| h["id"].as_str() == Some(host_id)) {
        return json!({
            "type": "pipkin_result",
            "ok": false,
            "code": "remote_host_unknown",
            "error": "Remote host not found.",
            "data": {}
        });
    }
    let norm = match normalize_remote_path(raw_path) {
        Ok(p) => p,
        Err(err_res) => return err_res,
    };
    let hash = sha256_hex(norm.as_bytes())[..16].to_string();
    let cwd_dir = remote_remotes_dir(config_dir).join(host_id).join(hash);
    if let Err(e) = std::fs::create_dir_all(&cwd_dir) {
        return json!({
            "type": "pipkin_result",
            "ok": false,
            "code": "io_error",
            "error": format!("Failed to create stand-in folder: {}", e),
            "data": {}
        });
    }
    let manifest_path = cwd_dir.join("remote.json");
    if let Err(e) = write_json_file(
        &manifest_path,
        &json!({ "version": 1, "hostId": host_id, "path": norm }),
    ) {
        return json!({
            "type": "pipkin_result",
            "ok": false,
            "code": "io_error",
            "error": format!("Failed to write stand-in manifest: {}", e),
            "data": {}
        });
    }
    json!({
        "type": "pipkin_result",
        "ok": true,
        "data": { "cwd": cwd_dir.to_string_lossy() }
    })
}

fn handle_remote_status(config_dir: &Path, params: &Value) -> Value {
    let ssh_dir = remote_ssh_dir(config_dir);
    let hosts = load_remote_hosts_list(&ssh_dir);
    let ids: Vec<String> = params
        .get("ids")
        .and_then(|v| v.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|v| v.as_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_default();
    let mut connected = serde_json::Map::new();
    let ssh_bin = find_termux_bin("ssh");
    for id in ids {
        let known = hosts.iter().any(|h| h["id"].as_str() == Some(&id));
        let sock = ssh_dir.join("s").join(&id);
        let is_up = if known && sock.exists() {
            ssh_bin
                .as_ref()
                .and_then(|ssh| {
                    std::process::Command::new(ssh)
                        .args(["-S", &sock.to_string_lossy(), "-O", "check", "pipkin-ctrl"])
                        .output()
                        .ok()
                })
                .is_some_and(|o| o.status.success())
        } else {
            false
        };
        connected.insert(id, json!(is_up));
    }
    json!({
        "type": "pipkin_result",
        "ok": true,
        "data": { "connected": Value::Object(connected) }
    })
}

fn handle_remote_disconnect(config_dir: &Path, params: &Value) -> Value {
    let id = params["id"].as_str().unwrap_or("");
    let ssh_dir = remote_ssh_dir(config_dir);
    disconnect_ssh_control(&ssh_dir, id);
    json!({
        "type": "pipkin_result",
        "ok": true,
        "data": {}
    })
}

fn handle_remote_forget_host_key(config_dir: &Path, params: &Value) -> Value {
    let id = params["id"].as_str().unwrap_or("");
    let ssh_dir = remote_ssh_dir(config_dir);
    let hosts = load_remote_hosts_list(&ssh_dir);
    let Some(host) = hosts.into_iter().find(|h| h["id"].as_str() == Some(id)) else {
        return json!({
            "type": "pipkin_result",
            "ok": false,
            "code": "remote_host_unknown",
            "error": "Remote host not found.",
            "data": {}
        });
    };
    let kh = ssh_dir.join("known_hosts");
    if kh.is_file() {
        let addr = host["address"].as_str().unwrap_or("");
        let target_host = parse_remote_address(addr).map(|(_, h)| h).unwrap_or(addr);
        if let Ok(content) = std::fs::read_to_string(&kh) {
            let filtered: Vec<&str> = content
                .lines()
                .filter(|line| !target_host.is_empty() && !line.contains(target_host))
                .collect();
            let _ = std::fs::write(&kh, filtered.join("\n"));
        }
    }
    json!({
        "type": "pipkin_result",
        "ok": true,
        "data": {}
    })
}

fn handle_remote_key(config_dir: &Path) -> Value {
    let ssh_dir = remote_ssh_dir(config_dir);
    let pub_path = ssh_dir.join("id_ed25519.pub");
    if let Ok(existing) = std::fs::read_to_string(&pub_path) {
        let trimmed = existing.trim();
        if !trimmed.is_empty() {
            return json!({
                "type": "pipkin_result",
                "ok": true,
                "data": { "publicKey": trimmed }
            });
        }
    }
    handle_remote_key_new(config_dir)
}

fn handle_remote_key_new(config_dir: &Path) -> Value {
    let Some(keygen) = find_termux_bin("ssh-keygen") else {
        return json!({
            "type": "pipkin_result",
            "ok": false,
            "code": "ssh_missing",
            "error": "OpenSSH is not installed.",
            "i18n": { "key": "ssh_not_installed" },
            "data": {}
        });
    };
    let ssh_dir = remote_ssh_dir(config_dir);
    if let Err(e) = std::fs::create_dir_all(&ssh_dir) {
        return json!({
            "type": "pipkin_result",
            "ok": false,
            "code": "io_error",
            "error": format!("Failed to prepare ssh directory: {}", e),
            "data": {}
        });
    }
    let key_path = ssh_dir.join("id_ed25519");
    let pub_path = ssh_dir.join("id_ed25519.pub");
    let _ = std::fs::remove_file(&key_path);
    let _ = std::fs::remove_file(&pub_path);
    let out = std::process::Command::new(keygen)
        .args(["-t", "ed25519", "-N", "", "-C", "pipkin", "-f"])
        .arg(&key_path)
        .output();
    match out {
        Ok(o) if o.status.success() => {
            let pub_key = std::fs::read_to_string(&pub_path).unwrap_or_default();
            for h in load_remote_hosts_list(&ssh_dir) {
                if h["auth"].as_str() == Some("key") {
                    if let Some(hid) = h["id"].as_str() {
                        disconnect_ssh_control(&ssh_dir, hid);
                    }
                }
            }
            json!({
                "type": "pipkin_result",
                "ok": true,
                "data": { "publicKey": pub_key.trim() }
            })
        }
        Ok(o) => json!({
            "type": "pipkin_result",
            "ok": false,
            "code": "io_error",
            "error": format!("ssh-keygen failed: {}", String::from_utf8_lossy(&o.stderr).trim()),
            "data": {}
        }),
        Err(e) => json!({
            "type": "pipkin_result",
            "ok": false,
            "code": "ssh_missing",
            "error": format!("OpenSSH failed to run: {}", e),
            "data": {}
        }),
    }
}

fn decode_base64(input: &str) -> Option<Vec<u8>> {
    let mut table = [255u8; 256];
    for (i, &c) in b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/"
        .iter()
        .enumerate()
    {
        table[c as usize] = i as u8;
    }

    let compact: Vec<u8> = input
        .bytes()
        .filter(|b| !b.is_ascii_whitespace())
        .collect();
    if compact.is_empty() {
        return Some(Vec::new());
    }
    let eq_pos = compact.iter().position(|&b| b == b'=');
    let body = match eq_pos {
        Some(pos) => {
            if compact.len() % 4 != 0 {
                return None;
            }
            let pad = &compact[pos..];
            if pad.len() > 2 || !pad.iter().all(|&b| b == b'=') {
                return None;
            }
            &compact[..pos]
        }
        None => &compact[..],
    };
    if body.len() % 4 == 1 {
        return None;
    }

    let mut out = Vec::with_capacity(body.len() * 3 / 4);
    let mut buf = 0u32;
    let mut bits = 0u32;
    for &b in body {
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

    fn spawn_provider_models_mock(
        responses: Vec<(u16, String, String)>,
    ) -> (
        String,
        Arc<Mutex<Vec<String>>>,
        std::thread::JoinHandle<()>,
    ) {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let base_url = format!("http://127.0.0.1:{}", port);
        let captured = Arc::new(Mutex::new(Vec::new()));
        let captured_clone = Arc::clone(&captured);
        let handle = std::thread::spawn(move || {
            for (status, content_type, body) in responses {
                let (mut stream, _) = listener.accept().unwrap();
                let mut buf = [0u8; 8192];
                let n = std::io::Read::read(&mut stream, &mut buf).unwrap_or(0);
                let req = String::from_utf8_lossy(&buf[..n]).to_string();
                captured_clone.lock().unwrap().push(req);
                let reason = match status {
                    200 => "OK",
                    401 => "Unauthorized",
                    404 => "Not Found",
                    429 => "Too Many Requests",
                    _ => "Error",
                };
                let reply = format!(
                    "HTTP/1.1 {} {}\r\nContent-Type: {}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                    status,
                    reason,
                    content_type,
                    body.len(),
                    body
                );
                let _ = stream.write_all(reply.as_bytes());
                let _ = stream.flush();
            }
        });
        (base_url, captured, handle)
    }

    #[tokio::test]
    async fn reg005_handle_provider_models_fails_closed_on_bad_url_and_http_errors() {
        // 1. Empty / non-HTTP URL -> bad_url (never returns builtin catalog)
        let empty = handle_provider_models(&json!({})).await;
        assert_eq!(empty["ok"], false);
        assert_eq!(empty["code"], "bad_url");

        let ftp = handle_provider_models(&json!({ "baseUrl": "ftp://example.com" })).await;
        assert_eq!(ftp["ok"], false);
        assert_eq!(ftp["code"], "bad_url");

        // 2. HTTP 401 -> unauthorized, 404 -> not_found, HTML/invalid JSON -> bad_response
        let (base_url, _req_log, server) = spawn_provider_models_mock(vec![
            (
                401,
                "application/json".to_string(),
                json!({ "error": { "message": "Invalid API key" } }).to_string(),
            ),
            (404, "text/plain".to_string(), "Not found".to_string()),
            (200, "text/html".to_string(), "<!doctype html><html>login</html>".to_string()),
        ]);

        let unauth = handle_provider_models(&json!({
            "baseUrl": base_url,
            "apiKey": "bad-key",
            "api": "openai-completions"
        }))
        .await;
        assert_eq!(unauth["ok"], false);
        assert_eq!(unauth["code"], "unauthorized");
        assert!(unauth["error"].as_str().unwrap().contains("Invalid API key"));

        let nf = handle_provider_models(&json!({
            "baseUrl": base_url,
            "apiKey": "k",
            "api": "openai-completions"
        }))
        .await;
        assert_eq!(nf["ok"], false);
        assert_eq!(nf["code"], "not_found");

        let html = handle_provider_models(&json!({
            "baseUrl": base_url,
            "apiKey": "k",
            "api": "openai-completions"
        }))
        .await;
        assert_eq!(html["ok"], false);
        assert_eq!(html["code"], "bad_response");
        assert_eq!(html["i18n"]["key"], "provider_web_page");

        server.join().unwrap();
    }

    #[tokio::test]
    async fn reg005_handle_provider_models_sends_protocol_specific_headers() {
        let (base_url, captured, server) = spawn_provider_models_mock(vec![
            (
                200,
                "application/json".to_string(),
                json!({ "models": [{ "name": "models/gemini-2.5-flash", "displayName": "Gemini 2.5 Flash" }] })
                    .to_string(),
            ),
            (
                200,
                "application/json".to_string(),
                json!({ "data": [{ "id": "claude-sonnet-4-5", "display_name": "Claude Sonnet 4.5" }] })
                    .to_string(),
            ),
            (
                200,
                "application/json".to_string(),
                json!({ "data": [{ "id": "local-qwen" }] }).to_string(),
            ),
        ]);

        // Google uses x-goog-api-key and /models?pageSize=100 (NO Authorization: Bearer)
        let google_res = handle_provider_models(&json!({
            "baseUrl": base_url,
            "apiKey": "AIza-test",
            "api": "google-generative-ai"
        }))
        .await;
        assert_eq!(google_res["ok"], true);
        assert_eq!(google_res["data"]["models"][0]["id"], "gemini-2.5-flash");

        // Anthropic uses x-api-key + anthropic-version
        let ant_res = handle_provider_models(&json!({
            "baseUrl": base_url,
            "apiKey": "sk-ant-test",
            "api": "anthropic-messages"
        }))
        .await;
        assert_eq!(ant_res["ok"], true);
        assert_eq!(ant_res["data"]["models"][0]["id"], "claude-sonnet-4-5");

        // OpenAI-compatible uses Bearer token (falling back to no-api-key)
        let oai_res = handle_provider_models(&json!({
            "baseUrl": base_url,
            "apiKey": "",
            "api": "openai-completions"
        }))
        .await;
        assert_eq!(oai_res["ok"], true);
        assert_eq!(oai_res["data"]["models"][0]["id"], "local-qwen");

        server.join().unwrap();
        let reqs = captured.lock().unwrap();
        assert_eq!(reqs.len(), 3);

        let g_lower = reqs[0].to_ascii_lowercase();
        assert!(reqs[0].starts_with("GET /models?pageSize=100 "));
        assert!(g_lower.contains("x-goog-api-key: aiza-test"));
        assert!(!g_lower.contains("authorization: bearer"));

        let a_lower = reqs[1].to_ascii_lowercase();
        assert!(reqs[1].starts_with("GET /models "));
        assert!(a_lower.contains("x-api-key: sk-ant-test"));
        assert!(a_lower.contains("anthropic-version: 2023-06-01"));

        let o_lower = reqs[2].to_ascii_lowercase();
        assert!(reqs[2].starts_with("GET /models "));
        assert!(o_lower.contains("authorization: bearer no-api-key"));
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
        let _sink_guard = crate::rpc::output_sink_test_mutex().lock().await;
        let temp_dir = std::env::temp_dir().join(format!(
            "oopbot_host_rpc_test_{}_{}",
            std::process::id(),
            epoch_ms()
        ));
        let _ = std::fs::create_dir_all(&temp_dir);

        let raw = Arc::new(Mutex::new(Vec::new()));
        let output_buf = Arc::new(Mutex::new(Box::new(SharedBuf(raw.clone())) as Box<dyn Write + Send + Sync>));
        set_output_sink(Some(output_buf));

        let mut server = RpcServer::new_in_dir(temp_dir.clone(), None, None, vec![]).unwrap();
        let input = b"{\"type\":\"get_state\",\"id\":\"test_1\"}\n";
        let cursor = Cursor::new(input);

        let res = server.run_loop_with_input(cursor).await;
        assert!(res.is_ok());

        set_output_sink(None);
        let _ = std::fs::remove_dir_all(&temp_dir);

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

    fn spawn_mock_release_server(
        routes: Vec<(String, u16, Vec<u8>)>,
        max_requests: usize,
    ) -> (
        String,
        Arc<std::sync::atomic::AtomicUsize>,
        std::thread::JoinHandle<()>,
    ) {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let base_url = format!("http://127.0.0.1:{}", addr.port());
        let hits = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let hits_clone = Arc::clone(&hits);

        let handle = std::thread::spawn(move || {
            use std::io::Read;
            for _ in 0..max_requests {
                let Ok((mut stream, _)) = listener.accept() else {
                    break;
                };
                hits_clone.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                let mut buf = [0u8; 2048];
                let n = stream.read(&mut buf).unwrap_or(0);
                let req = String::from_utf8_lossy(&buf[..n]);
                let path = req
                    .lines()
                    .next()
                    .and_then(|l| l.split_whitespace().nth(1))
                    .unwrap_or("/");
                let (status, body) = routes
                    .iter()
                    .find(|(p, _, _)| p == path)
                    .map(|(_, s, b)| (*s, b.as_slice()))
                    .unwrap_or((404, b"not found".as_slice()));
                let reason = if status == 200 { "OK" } else { "Not Found" };
                let header = format!(
                    "HTTP/1.1 {} {}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    status,
                    reason,
                    body.len()
                );
                let _ = stream.write_all(header.as_bytes());
                let _ = stream.write_all(body);
                let _ = stream.flush();
            }
        });
        (base_url, hits, handle)
    }

    fn write_fake_oopbot(path: &Path, version: &str) {
        let body = format!("#!/bin/sh\necho \"oopbot {}\"\n", version);
        atomic_write_binary(path, body.as_bytes()).unwrap();
    }

    #[tokio::test]
    async fn test_ensure_oopbot_upgrades_stale_primary_and_secondary_from_release() {
        let seq = TMP_SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let temp_dir = std::env::temp_dir().join(format!("oopbot_ensure_upg_{}_{}", epoch_ms(), seq));
        let primary = temp_dir.join("usr/bin/oopbot");
        let secondary = temp_dir.join("root/bin/oopbot");
        let version_file = temp_dir.join("root/bin/oopbot.version");
        write_fake_oopbot(&primary, "0.5.2");
        write_fake_oopbot(&secondary, "0.5.2");
        std::fs::write(&version_file, "0.5.2\n").unwrap();

        let new_script = b"#!/bin/sh\necho \"oopbot 0.5.4\"\n".to_vec();
        let (base_url, hits, server) = spawn_mock_release_server(
            vec![("/v0.5.4/oopbot".to_string(), 200, new_script)],
            1,
        );
        let urls = vec![
            format!("{}/v0.5.4/oopbot", base_url),
            format!("{}/latest/oopbot", base_url),
        ];
        let mut logs = Vec::new();
        let res = ensure_oopbot_binary_with(
            "0.5.2",
            "0.5.4",
            false,
            &primary,
            &[secondary.clone()],
            &urls,
            |line| logs.push(line.to_string()),
        )
        .await;

        server.join().unwrap();
        assert_eq!(res, Ok("0.5.4".to_string()));
        assert_eq!(hits.load(std::sync::atomic::Ordering::SeqCst), 1);
        assert_eq!(read_oopbot_binary_version(&primary).as_deref(), Some("0.5.4"));
        assert_eq!(read_oopbot_binary_version(&secondary).as_deref(), Some("0.5.4"));
        assert_eq!(std::fs::read_to_string(&version_file).unwrap().trim(), "0.5.4");
        assert!(!logs.is_empty());

        let _ = std::fs::remove_dir_all(&temp_dir);
    }

    #[tokio::test]
    async fn test_ensure_oopbot_falls_back_on_404_and_rejects_version_mismatch() {
        let seq = TMP_SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let temp_dir = std::env::temp_dir().join(format!("oopbot_ensure_fb_{}_{}", epoch_ms(), seq));
        let primary = temp_dir.join("usr/bin/oopbot");
        write_fake_oopbot(&primary, "0.5.2");

        // 1. Primary tag 404 -> falls back to latest 200 with matching version
        let new_script = b"#!/bin/sh\necho \"oopbot 0.5.4\"\n".to_vec();
        let (base_url, hits, server) = spawn_mock_release_server(
            vec![("/latest/oopbot".to_string(), 200, new_script)],
            2,
        );
        let urls = vec![
            format!("{}/v0.5.4/oopbot", base_url),
            format!("{}/latest/oopbot", base_url),
        ];
        let res = ensure_oopbot_binary_with(
            "0.5.2",
            "0.5.4",
            false,
            &primary,
            &[],
            &urls,
            |_| {},
        )
        .await;
        server.join().unwrap();
        assert_eq!(res, Ok("0.5.4".to_string()));
        assert_eq!(hits.load(std::sync::atomic::Ordering::SeqCst), 2);
        assert_eq!(read_oopbot_binary_version(&primary).as_deref(), Some("0.5.4"));

        // 2. Mismatched version from server fails closed with upgrade_failed and keeps 0.5.4 intact
        let wrong_script = b"#!/bin/sh\necho \"oopbot 0.5.3\"\n".to_vec();
        let (bad_url, _bad_hits, bad_server) = spawn_mock_release_server(
            vec![("/v0.5.5/oopbot".to_string(), 200, wrong_script)],
            1,
        );
        let err = ensure_oopbot_binary_with(
            "0.5.4",
            "0.5.5",
            false,
            &primary,
            &[],
            &[format!("{}/v0.5.5/oopbot", bad_url)],
            |_| {},
        )
        .await
        .unwrap_err();
        bad_server.join().unwrap();
        assert_eq!(err.code, "upgrade_failed");
        assert_eq!(read_oopbot_binary_version(&primary).as_deref(), Some("0.5.4"));

        // 3. Unreachable endpoint reports npm_offline and keeps 0.5.4 intact
        let dead_listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let dead_port = dead_listener.local_addr().unwrap().port();
        drop(dead_listener);
        let offline_err = ensure_oopbot_binary_with(
            "0.5.4",
            "0.5.5",
            false,
            &primary,
            &[],
            &[format!("http://127.0.0.1:{}/v0.5.5/oopbot", dead_port)],
            |_| {},
        )
        .await
        .unwrap_err();
        assert_eq!(offline_err.code, "npm_offline");
        assert_eq!(read_oopbot_binary_version(&primary).as_deref(), Some("0.5.4"));

        let _ = std::fs::remove_dir_all(&temp_dir);
    }

    #[tokio::test]
    async fn test_ensure_oopbot_skips_download_when_version_matches_and_syncs_secondary() {
        let seq = TMP_SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let temp_dir = std::env::temp_dir().join(format!("oopbot_ensure_skip_{}_{}", epoch_ms(), seq));
        let primary = temp_dir.join("usr/bin/oopbot");
        let secondary = temp_dir.join("root/bin/oopbot");
        write_fake_oopbot(&primary, "0.5.4");
        write_fake_oopbot(&secondary, "0.5.2");

        let (base_url, hits, server) = spawn_mock_release_server(vec![], 0);
        server.join().unwrap();
        let urls = vec![format!("{}/v0.5.4/oopbot", base_url)];
        let res = ensure_oopbot_binary_with(
            "0.5.4",
            "0.5.4",
            false,
            &primary,
            &[secondary.clone()],
            &urls,
            |_| {},
        )
        .await;

        assert_eq!(res, Ok("0.5.4".to_string()));
        assert_eq!(hits.load(std::sync::atomic::Ordering::SeqCst), 0);
        assert_eq!(read_oopbot_binary_version(&secondary).as_deref(), Some("0.5.4"));

        let _ = std::fs::remove_dir_all(&temp_dir);
    }

    #[test]
    fn test_check_rpc_expected_version() {
        assert!(check_rpc_expected_version(&json!({}), "0.5.4").is_none());
        assert!(check_rpc_expected_version(&json!({ "expectedVersion": "0.5.4" }), "0.5.4").is_none());

        let refusal = check_rpc_expected_version(&json!({ "expectedVersion": "0.5.5" }), "0.5.4").unwrap();
        assert_eq!(refusal["type"], "pipkin_result");
        assert_eq!(refusal["ok"], false);
        assert_eq!(refusal["code"], "pi_version");
        assert_eq!(refusal["i18n"]["key"], "agent_version_mismatch");
    }

    #[test]
    fn test_read_oopbot_binary_version_detail_diagnostics() {
        let seq = TMP_SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let temp_dir = std::env::temp_dir().join(format!("oopbot_ver_diag_{}_{}", epoch_ms(), seq));
        std::fs::create_dir_all(&temp_dir).unwrap();

        let missing = temp_dir.join("missing");
        assert!(read_oopbot_binary_version_detail(&missing).unwrap_err().contains("metadata failed"));

        let empty = temp_dir.join("empty");
        std::fs::write(&empty, b"").unwrap();
        assert_eq!(
            read_oopbot_binary_version_detail(&empty).unwrap_err(),
            "binary is missing or empty"
        );

        let fail_script = temp_dir.join("fail");
        atomic_write_binary(&fail_script, b"#!/bin/sh\necho boom >&2\nexit 2\n").unwrap();
        let fail_err = read_oopbot_binary_version_detail(&fail_script).unwrap_err();
        assert!(fail_err.contains("exited with"));
        assert!(fail_err.contains("boom"));

        let bad_out = temp_dir.join("bad_out");
        atomic_write_binary(&bad_out, b"#!/bin/sh\necho not-oopbot\n").unwrap();
        let bad_err = read_oopbot_binary_version_detail(&bad_out).unwrap_err();
        assert!(bad_err.contains("unexpected --version output"));

        let busy_err = std::io::Error::from(std::io::ErrorKind::ExecutableFileBusy);
        assert!(is_executable_file_busy(&busy_err));

        let _ = std::fs::remove_dir_all(&temp_dir);
    }

    #[test]
    fn test_read_oopbot_binary_version_concurrent_fork_resilience() {
        let seq = TMP_SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let temp_dir = std::env::temp_dir().join(format!("oopbot_ver_fork_{}_{}", epoch_ms(), seq));
        std::fs::create_dir_all(&temp_dir).unwrap();

        let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let stop_bg = Arc::clone(&stop);
        let forker = std::thread::spawn(move || {
            while !stop_bg.load(std::sync::atomic::Ordering::Relaxed) {
                let _ = std::process::Command::new("sh").arg("-c").arg(":").status();
            }
        });

        for i in 0..25 {
            let bin = temp_dir.join(format!("oopbot_{}", i));
            write_fake_oopbot(&bin, "0.5.4");
            let ver = read_oopbot_binary_version_detail(&bin).unwrap();
            assert_eq!(ver, "0.5.4");
        }

        stop.store(true, std::sync::atomic::Ordering::Relaxed);
        forker.join().unwrap();
        let _ = std::fs::remove_dir_all(&temp_dir);
    }

    #[tokio::test]
    async fn test_pkg_version_055_rejects_054_rpc_and_upgrades_054_binary() {
        let pkg_ver = env!("CARGO_PKG_VERSION");
        assert_eq!(pkg_ver, "0.5.5");

        let refusal = check_rpc_expected_version(&json!({ "expectedVersion": pkg_ver }), "0.5.4").unwrap();
        assert_eq!(refusal["ok"], false);
        assert_eq!(refusal["code"], "pi_version");

        let seq = TMP_SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let temp_dir = std::env::temp_dir().join(format!("oopbot_ensure_055_{}_{}", epoch_ms(), seq));
        let primary = temp_dir.join("usr/bin/oopbot");
        let secondary = temp_dir.join("root/bin/oopbot");
        let version_file = temp_dir.join("root/bin/oopbot.version");
        write_fake_oopbot(&primary, "0.5.4");
        write_fake_oopbot(&secondary, "0.5.4");
        std::fs::write(&version_file, "0.5.4\n").unwrap();

        let new_script = format!("#!/bin/sh\necho \"oopbot {}\"\n", pkg_ver).into_bytes();
        let (base_url, hits, server) = spawn_mock_release_server(
            vec![(format!("/v{}/oopbot", pkg_ver), 200, new_script)],
            1,
        );
        let urls = vec![format!("{}/v{}/oopbot", base_url, pkg_ver)];
        let res = ensure_oopbot_binary_with(
            "0.5.4",
            pkg_ver,
            false,
            &primary,
            &[secondary.clone()],
            &urls,
            |_| {},
        )
        .await;
        server.join().unwrap();

        assert_eq!(res, Ok(pkg_ver.to_string()));
        assert_eq!(hits.load(std::sync::atomic::Ordering::SeqCst), 1);
        assert_eq!(read_oopbot_binary_version(&primary).as_deref(), Some(pkg_ver));
        assert_eq!(read_oopbot_binary_version(&secondary).as_deref(), Some(pkg_ver));
        assert_eq!(std::fs::read_to_string(&version_file).unwrap().trim(), pkg_ver);

        let _ = std::fs::remove_dir_all(&temp_dir);
    }

    #[test]
    fn reg005_auth_status_oauth_unrestricted_scope_and_corrupt_json_errors() {
        let seq = TMP_SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let temp_dir = std::env::temp_dir().join(format!("oopbot_auth_reg005_{}_{}", epoch_ms(), seq));
        std::fs::create_dir_all(&temp_dir).unwrap();

        // 1. OAuth credentials for anthropic, unrestricted empty enabledModels, no OOPBOT_API_KEY leak
        std::fs::write(
            temp_dir.join("auth.json"),
            json!({
                "anthropic": { "type": "oauth", "access": "oauth-access-token-123" },
                "openrouter": { "type": "api_key", "apiKey": "sk-or-1" }
            })
            .to_string(),
        )
        .unwrap();
        std::fs::write(
            temp_dir.join("settings.json"),
            json!({ "enabledModels": [] }).to_string(),
        )
        .unwrap();

        let res = handle_auth_status(&temp_dir);
        assert_eq!(res["ok"], true);
        assert!(res["data"]["scope"].is_null(), "empty enabledModels must emit null scope");
        let builtin = res["data"]["builtin"].as_array().unwrap();
        let anthropic = builtin.iter().find(|p| p["id"] == "anthropic").unwrap();
        assert_eq!(anthropic["configured"], true);
        assert_eq!(anthropic["type"], "oauth");
        assert!(anthropic["enabled"].is_null(), "unrestricted scope must leave builtin[].enabled null");

        let zai = builtin.iter().find(|p| p["id"] == "zai").unwrap();
        assert_eq!(zai["configured"], false);
        assert!(zai["type"].is_null());

        // 2. Non-empty glob pattern in enabledModels activates scope + builtin[].enabled arrays
        std::fs::write(
            temp_dir.join("settings.json"),
            json!({ "enabledModels": ["anthropic/claude-*"] }).to_string(),
        )
        .unwrap();
        let scoped_res = handle_auth_status(&temp_dir);
        let scoped_builtin = scoped_res["data"]["builtin"].as_array().unwrap();
        let scoped_anthropic = scoped_builtin.iter().find(|p| p["id"] == "anthropic").unwrap();
        assert!(!scoped_anthropic["enabled"].as_array().unwrap().is_empty());
        let scoped_google = scoped_builtin.iter().find(|p| p["id"] == "google").unwrap();
        assert_eq!(scoped_google["enabled"].as_array().unwrap().len(), 0);

        // 3. Corrupt JSON surfaces modelsError & settingsError instead of silent empty state
        std::fs::write(temp_dir.join("models.json"), "{bad json").unwrap();
        std::fs::write(temp_dir.join("settings.json"), "not-an-object").unwrap();
        let err_res = handle_auth_status(&temp_dir);
        assert!(err_res["data"]["modelsError"].as_str().unwrap().contains("Invalid JSON"));
        assert!(err_res["data"]["settingsError"].as_str().unwrap().contains("Invalid JSON"));

        let _ = std::fs::remove_dir_all(&temp_dir);
    }

    #[test]
    fn reg008_local_models_sync_uses_pipkin_local_only_compaction_and_changed_flag() {
        let seq = TMP_SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let temp_dir = std::env::temp_dir().join(format!("oopbot_local_reg008_{}_{}", epoch_ms(), seq));
        std::fs::create_dir_all(&temp_dir).unwrap();

        // Seed legacy oopbot-local so we verify cleanup
        std::fs::write(
            temp_dir.join("models.json"),
            json!({ "providers": { "oopbot-local": { "baseUrl": "http://127.0.0.1:1/v1", "models": [] } } }).to_string(),
        )
        .unwrap();

        let params = json!({
            "baseUrl": "http://127.0.0.1:11434/v1",
            "apiKey": "no-api-key",
            "models": [
                { "id": "qwen2.5-7b", "name": "Qwen 2.5", "contextWindow": 16384, "maxTokens": 2048 }
            ]
        });

        let first = handle_local_models_sync(&temp_dir, &params);
        assert_eq!(first["ok"], true);
        assert_eq!(first["data"]["changed"], true);

        let models_doc = read_json_file(&temp_dir.join("models.json"));
        assert!(models_doc["providers"].get("pipkin-local").is_some());
        assert!(models_doc["providers"].get("oopbot-local").is_none());
        assert_eq!(
            models_doc["providers"]["pipkin-local"]["compat"]["sendSessionAffinityHeaders"],
            true
        );
        assert_eq!(
            models_doc["providers"]["pipkin-local"]["models"][0]["cost"]["input"],
            0
        );

        let settings_doc = read_json_file(&temp_dir.join("settings.json"));
        let ov = &settings_doc["compaction"]["modelOverrides"]["pipkin-local/qwen2.5-7b"];
        assert_eq!(ov["reserveTokens"], 2048 + 4096);
        assert_eq!(ov["keepRecentTokens"], 16384 / 16);

        // Repeat with identical params must report changed: false
        let second = handle_local_models_sync(&temp_dir, &params);
        assert_eq!(second["ok"], true);
        assert_eq!(second["data"]["changed"], false);

        // Empty list removes provider and compaction override
        let cleared = handle_local_models_sync(
            &temp_dir,
            &json!({ "baseUrl": "http://127.0.0.1:11434/v1", "apiKey": "", "models": [] }),
        );
        assert_eq!(cleared["data"]["changed"], true);
        let models_after = read_json_file(&temp_dir.join("models.json"));
        assert!(models_after["providers"].get("pipkin-local").is_none());
        let settings_after = read_json_file(&temp_dir.join("settings.json"));
        assert!(settings_after["compaction"].get("modelOverrides").is_none());

        let _ = std::fs::remove_dir_all(&temp_dir);
    }

    #[test]
    fn reg009_list_sessions_parses_iso_timestamp_tracks_renames_and_subagents() {
        let seq = TMP_SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let temp_dir = std::env::temp_dir().join(format!("oopbot_sess_reg009_{}_{}", epoch_ms(), seq));
        let sess_dir = temp_dir.join("sessions").join("proj");
        std::fs::create_dir_all(sess_dir.join("subagents").join("s-iso")).unwrap();

        let main_file = sess_dir.join("chat.jsonl");
        let lines = [
            json!({
                "type": "session",
                "id": "s-iso",
                "cwd": temp_dir.to_string_lossy(),
                "timestamp": "2026-10-15T14:30:00.500Z",
                "name": "Initial Name"
            })
            .to_string(),
            json!({
                "type": "session_info",
                "name": "Renamed Session Title"
            })
            .to_string(),
            json!({
                "type": "message",
                "message": { "role": "user", "content": "Hello" }
            })
            .to_string(),
        ]
        .join("\n");
        std::fs::write(&main_file, format!("{}\n", lines)).unwrap();

        // Create a subagent transcript under sessions/proj/subagents/s-iso/ (must NOT be listed as top-level session!)
        std::fs::write(
            sess_dir.join("subagents").join("s-iso").join("child1.jsonl"),
            json!({ "type": "session", "id": "sub-1", "cwd": temp_dir.to_string_lossy() }).to_string(),
        )
        .unwrap();

        let res = handle_list_sessions(&temp_dir, "/home");
        let sessions = res["data"]["sessions"].as_array().unwrap();
        assert_eq!(sessions.len(), 1);
        assert_eq!(sessions[0]["id"], "s-iso");
        assert_eq!(sessions[0]["name"], "Renamed Session Title");
        assert_eq!(sessions[0]["subagents"], 1);
        // 2026-10-15T14:30:00.500Z in epoch ms (1792074600500)
        assert_eq!(
            sessions[0]["created"],
            parse_iso_to_epoch_ms("2026-10-15T14:30:00.500Z").unwrap()
        );

        let _ = std::fs::remove_dir_all(&temp_dir);
    }

    #[test]
    fn reg010_save_attachments_and_files_containment_reject_traversal() {
        let seq = TMP_SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let temp_dir = std::env::temp_dir().join(format!("oopbot_sec_reg010_{}_{}", epoch_ms(), seq));
        let workspace = temp_dir.join("ws");
        let outside = temp_dir.join("outside");
        std::fs::create_dir_all(&workspace).unwrap();
        std::fs::create_dir_all(&outside).unwrap();
        std::fs::write(outside.join("secret.txt"), "top-secret").unwrap();

        // 1. Attachments validation & safe filename + deduplication
        let missing = handle_save_attachments(&json!({}), &temp_dir);
        assert_eq!(missing["ok"], false);
        assert_eq!(missing["code"], "bad_params");

        let bad_b64 = handle_save_attachments(
            &json!({ "files": [{ "name": "a.txt", "data": "!!!" }] }),
            &temp_dir,
        );
        assert_eq!(bad_b64["ok"], false);
        assert_eq!(bad_b64["code"], "bad_params");

        let saved = handle_save_attachments(
            &json!({
                "files": [
                    { "name": "../../.hidden.txt", "data": "SGVsbG8=" },
                    { "name": "..\\..\\hidden.txt", "data": "V29ybGQ=" }
                ]
            }),
            &temp_dir,
        );
        assert_eq!(saved["ok"], true);
        let paths = saved["data"]["paths"].as_array().unwrap();
        assert_eq!(paths.len(), 2);
        let p0 = PathBuf::from(paths[0].as_str().unwrap());
        let p1 = PathBuf::from(paths[1].as_str().unwrap());
        assert_eq!(p0.file_name().unwrap(), "hidden.txt");
        assert_eq!(p1.file_name().unwrap(), "hidden-2.txt");
        assert!(p0.starts_with(std::fs::canonicalize(temp_dir.join("attachments")).unwrap()));

        // 2. Files loop blocks .. traversal and escaping symlinks
        #[cfg(unix)]
        let _ = std::os::unix::fs::symlink(outside.join("secret.txt"), workspace.join("escape_link.txt"));

        let ws_str = workspace.to_string_lossy();
        let reqs = format!(
            "{{\"type\":\"pipkin_files_read\",\"id\":1,\"root\":\"{}\",\"path\":\"../outside/secret.txt\"}}\n\
             {{\"type\":\"pipkin_files_read\",\"id\":2,\"root\":\"{}\",\"path\":\"escape_link.txt\"}}\n\
             {{\"type\":\"pipkin_files_list\",\"id\":3,\"root\":\"{}\",\"path\":\".\"}}\n\
             {{\"type\":\"pipkin_files_mkdir\",\"id\":4,\"root\":\"{}\",\"path\":\"..\",\"name\":\"bad\"}}\n",
            ws_str, ws_str, ws_str, ws_str
        );
        let mut out = Vec::new();
        run_files_loop(Cursor::new(reqs.into_bytes()), &mut out).unwrap();
        let out_str = String::from_utf8_lossy(&out);
        let lines: Vec<Value> = out_str
            .lines()
            .filter_map(|l| serde_json::from_str::<Value>(l).ok())
            .collect();

        let r1 = lines.iter().find(|v| v["id"] == 1).unwrap();
        assert_eq!(r1["errorCode"], "outside_workspace");

        #[cfg(unix)]
        {
            let r2 = lines.iter().find(|v| v["id"] == 2).unwrap();
            assert_eq!(r2["errorCode"], "outside_workspace");
            let r3 = lines.iter().find(|v| v["id"] == 3).unwrap();
            assert_eq!(r3["entries"].as_array().unwrap().len(), 0);
        }

        let r4 = lines.iter().find(|v| v["id"] == 4).unwrap();
        assert_eq!(r4["errorCode"], "outside_workspace");

        let _ = std::fs::remove_dir_all(&temp_dir);
    }

    #[test]
    fn reg004_backups_create_list_restore_delete_and_fail_closed_on_corrupt_or_bad_name() {
        let seq = TMP_SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let root_dir =
            std::env::temp_dir().join(format!("oopbot_bkp_reg004_{}_{}", epoch_ms(), seq));
        let config_dir = root_dir.join("agent");
        std::fs::create_dir_all(config_dir.join("sessions/proj")).unwrap();
        std::fs::write(config_dir.join("settings.json"), r#"{"defaultProvider":"google"}"#).unwrap();
        std::fs::write(config_dir.join("sessions/proj/s1.jsonl"), "live-session").unwrap();
        std::fs::write(config_dir.join("pi-debug.log"), "keep-me-log").unwrap();

        let mut progress_msgs = Vec::new();
        let created = handle_backup_create(&config_dir, &mut |line, _key, _count| {
            progress_msgs.push(line.to_string());
        });
        assert_eq!(created["ok"], true);
        let backup = &created["data"]["backup"];
        let bkp_name = backup["name"].as_str().unwrap().to_string();
        assert!(bkp_name.starts_with("pipkin-config-"));
        assert!(bkp_name.ends_with(".tar.gz"));
        assert!(backup["size"].as_u64().unwrap() > 0);
        assert!(backup["error"].is_null());

        let listed = handle_list_backups(&config_dir);
        assert_eq!(listed["ok"], true);
        let arr = listed["data"]["backups"].as_array().unwrap();
        assert_eq!(arr.len(), 1);
        assert_eq!(arr[0]["name"], bkp_name);

        // Mutate settings and verify restore restores settings while keeping sessions/logs
        std::fs::write(config_dir.join("settings.json"), r#"{"defaultProvider":"corrupted"}"#).unwrap();
        std::fs::write(config_dir.join("extra.txt"), "remove-on-restore").unwrap();
        let restored = handle_backup_restore(
            &config_dir,
            &json!({ "name": bkp_name }),
            &mut |_, _, _| {},
        );
        assert_eq!(restored["ok"], true);
        assert_eq!(
            std::fs::read_to_string(config_dir.join("settings.json")).unwrap(),
            r#"{"defaultProvider":"google"}"#
        );
        assert!(!config_dir.join("extra.txt").exists());
        assert_eq!(
            std::fs::read_to_string(config_dir.join("sessions/proj/s1.jsonl")).unwrap(),
            "live-session"
        );
        assert_eq!(
            std::fs::read_to_string(config_dir.join("pi-debug.log")).unwrap(),
            "keep-me-log"
        );

        // Corrupt archive is listed with error and rejected on restore
        let bkp_dir = backups_dir(&config_dir);
        std::fs::write(bkp_dir.join("corrupt.tar.gz"), b"not-a-gzip").unwrap();
        let listed_with_bad = handle_list_backups(&config_dir);
        let bad_entry = listed_with_bad["data"]["backups"]
            .as_array()
            .unwrap()
            .iter()
            .find(|b| b["name"] == "corrupt.tar.gz")
            .unwrap();
        assert!(bad_entry["error"].as_str().is_some());

        let restore_corrupt =
            handle_backup_restore(&config_dir, &json!({ "name": "corrupt.tar.gz" }), &mut |_, _, _| {});
        assert_eq!(restore_corrupt["ok"], false);
        assert_eq!(restore_corrupt["code"], "bad_backup");

        // Unsafe name rejected
        let bad_del = handle_backup_delete(&config_dir, &json!({ "name": "../evil.tar.gz" }));
        assert_eq!(bad_del["ok"], false);
        assert_eq!(bad_del["code"], "bad_name");

        let ok_del = handle_backup_delete(&config_dir, &json!({ "name": bkp_name }));
        assert_eq!(ok_del["ok"], true);
        assert!(!bkp_dir.join(&bkp_name).exists());

        let _ = std::fs::remove_dir_all(&root_dir);
    }

    #[test]
    fn reg004_subagent_sessions_reads_pipkin_subagent_transcripts_and_ignores_unrelated() {
        let seq = TMP_SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let config_dir =
            std::env::temp_dir().join(format!("oopbot_sub_reg004_{}_{}", epoch_ms(), seq));
        let proj_dir = config_dir.join("sessions/proj");
        std::fs::create_dir_all(proj_dir.join("subagents/parent1")).unwrap();

        let parent_file = proj_dir.join("parent.jsonl");
        std::fs::write(
            &parent_file,
            json!({ "type": "session", "id": "parent1", "timestamp": "2026-10-15T10:00:00.000Z" })
                .to_string(),
        )
        .unwrap();

        let sub_file = proj_dir.join("subagents/parent1/run1.jsonl");
        let sub_lines = [
            json!({ "type": "session", "id": "sub_a", "timestamp": "2026-10-15T10:01:00.000Z" })
                .to_string(),
            json!({
                "type": "custom",
                "customType": "pipkin-subagent",
                "data": { "toolCallId": "call_1", "agent": "research", "description": "Audit files" }
            })
            .to_string(),
            json!({ "type": "message", "message": { "role": "user", "content": "hi" } }).to_string(),
        ]
        .join("\n");
        std::fs::write(&sub_file, format!("{}\n", sub_lines)).unwrap();
        // Unrelated file without pipkin-subagent link must be ignored
        std::fs::write(
            proj_dir.join("subagents/parent1/orphan.jsonl"),
            json!({ "type": "session", "id": "sub_b" }).to_string(),
        )
        .unwrap();

        let res = handle_subagent_sessions(
            &config_dir,
            &json!({ "path": parent_file.to_string_lossy() }),
        );
        assert_eq!(res["ok"], true);
        let list = res["data"]["sessions"].as_array().unwrap();
        assert_eq!(list.len(), 1);
        assert_eq!(list[0]["toolCallId"], "call_1");
        assert_eq!(list[0]["agent"], "research");
        assert_eq!(list[0]["description"], "Audit files");
        assert_eq!(list[0]["messageCount"], 1);
        assert_eq!(
            list[0]["created"],
            parse_iso_to_epoch_ms("2026-10-15T10:01:00.000Z").unwrap()
        );

        let _ = std::fs::remove_dir_all(&config_dir);
    }

    #[test]
    fn reg004_remote_hosts_save_workspace_remove_and_fail_closed() {
        let seq = TMP_SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let root_dir =
            std::env::temp_dir().join(format!("oopbot_rem_reg004_{}_{}", epoch_ms(), seq));
        let config_dir = root_dir.join("agent");
        let home_dir = root_dir.join("home");
        std::fs::create_dir_all(&config_dir).unwrap();
        std::fs::create_dir_all(&home_dir).unwrap();
        let home_str = home_dir.to_string_lossy().to_string();

        // Validation error maps to remote_host_invalid + field
        let invalid = handle_remote_host_save(
            &config_dir,
            &home_str,
            &json!({ "host": { "name": "", "auth": "key", "address": "u@h", "port": 22 } }),
        );
        assert_eq!(invalid["ok"], false);
        assert_eq!(invalid["code"], "remote_host_invalid");
        assert_eq!(invalid["i18n"]["args"]["field"], "name");

        // Save valid host -> returns host.id & writes hosts.json + ssh/config
        let saved = handle_remote_host_save(
            &config_dir,
            &home_str,
            &json!({ "host": { "name": "Dev Box", "auth": "key", "address": "dev@example.com", "port": 2222 } }),
        );
        assert_eq!(saved["ok"], true);
        let host_id = saved["data"]["host"]["id"].as_str().unwrap().to_string();
        assert_eq!(host_id.len(), 12);

        // Create remote workspace stand-in
        let ws = handle_remote_workspace(
            &config_dir,
            &json!({ "hostId": host_id, "path": "/home/dev/proj/" }),
        );
        assert_eq!(ws["ok"], true);
        let cwd = ws["data"]["cwd"].as_str().unwrap().to_string();
        assert!(Path::new(&cwd).join("remote.json").is_file());

        // Remote stand-in rejects silent local attachment write
        let att_err = handle_save_attachments(
            &json!({
                "cwd": cwd,
                "files": [{ "name": "a.txt", "data": "SGVsbG8=" }]
            }),
            &config_dir,
        );
        assert_eq!(att_err["ok"], false);
        assert_eq!(att_err["code"], "remote_not_connected");

        let listed = handle_remote_hosts(&config_dir, &home_str);
        assert_eq!(listed["data"]["hosts"].as_array().unwrap().len(), 1);
        assert_eq!(listed["data"]["workspaces"].as_array().unwrap().len(), 1);

        let status = handle_remote_status(&config_dir, &json!({ "ids": [&host_id] }));
        assert_eq!(status["data"]["connected"][&host_id], false);

        // Removing host retains workspace stand-ins for archived chats
        let removed = handle_remote_host_remove(&config_dir, &home_str, &json!({ "id": host_id }));
        assert_eq!(removed["ok"], true);
        let listed_after = handle_remote_hosts(&config_dir, &home_str);
        assert_eq!(listed_after["data"]["hosts"].as_array().unwrap().len(), 0);
        assert_eq!(listed_after["data"]["workspaces"].as_array().unwrap().len(), 1);

        let _ = std::fs::remove_dir_all(&root_dir);
    }

    #[test]
    fn reg007_terminal_pty_echo_and_resize_without_tmp_hardcode() {
        let seq = TMP_SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let temp_dir =
            std::env::temp_dir().join(format!("oopbot_term_reg007_{}_{}", epoch_ms(), seq));
        let prefix_bin = temp_dir.join("usr/bin");
        std::fs::create_dir_all(&prefix_bin).unwrap();
        std::fs::write(prefix_bin.join("bash"), b"#!/bin/sh\n").unwrap();

        let (resolved, args) = resolve_terminal_shell_with(
            None,
            Some(&temp_dir.join("usr").to_string_lossy()),
        );
        assert!(resolved.ends_with("/usr/bin/bash"));
        assert_eq!(args, vec!["-l"]);

        #[cfg(unix)]
        {
            let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
            let addr = listener.local_addr().unwrap();
            let cwd_str = temp_dir.to_string_lossy().to_string();

            let client = std::net::TcpStream::connect(addr).unwrap();
            let (server_stream, _) = listener.accept().unwrap();

            let srv_read = std::io::BufReader::new(server_stream.try_clone().unwrap());
            let srv_write = std::io::BufWriter::new(server_stream);

            let server_handle = std::thread::spawn(move || {
                run_terminal_loop(
                    json!({ "cwd": cwd_str, "rows": 24, "cols": 80 }),
                    srv_read,
                    srv_write,
                )
                .unwrap();
            });

            let mut cli_read = std::io::BufReader::new(client.try_clone().unwrap());
            let mut cli_write = client;

            let cmd_b64 = encode_base64(b"stty size; printf 'PTY_OK\\n'; exit\n");
            let resize_msg = json!({ "type": "resize", "rows": 42, "cols": 111 }).to_string();
            let input_msg = json!({ "type": "input", "data": cmd_b64 }).to_string();
            writeln!(cli_write, "{}", resize_msg).unwrap();
            writeln!(cli_write, "{}", input_msg).unwrap();
            cli_write.flush().unwrap();

            let mut captured = String::new();
            let mut saw_exit = false;
            let mut line = String::new();
            while let Ok(n) = std::io::BufRead::read_line(&mut cli_read, &mut line) {
                if n == 0 {
                    break;
                }
                if let Ok(v) = serde_json::from_str::<Value>(line.trim()) {
                    match v["type"].as_str() {
                        Some("output") => {
                            if let Some(d) = v["data"].as_str().and_then(decode_base64) {
                                captured.push_str(&String::from_utf8_lossy(&d));
                            }
                        }
                        Some("pipkin_exit") => {
                            saw_exit = true;
                            break;
                        }
                        _ => {}
                    }
                }
                line.clear();
            }

            drop(cli_write);
            server_handle.join().unwrap();
            assert!(saw_exit, "terminal must emit pipkin_exit");
            assert!(captured.contains("PTY_OK"), "PTY output missed marker: {}", captured);
            assert!(captured.contains("42 111"), "PTY TIOCSWINSZ resize did not propagate: {}", captured);
        }

        let _ = std::fs::remove_dir_all(&temp_dir);
    }
}
