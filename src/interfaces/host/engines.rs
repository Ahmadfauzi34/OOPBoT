//! Manajemen External CLI Agent Engines (ACP) di sisi Termux Host Bridge:
//! - `engine_list`: daftar engine bawaan + status instalasi, `glibc`, dan autentikasi.
//! - `engine_install`: unduh & pasang rilis Termux (auto-install `glibc-repo`/`glibc`, staging + atomic swap + rollback).
//! - `engine_login`: alur login interaktif (`agy acp --login`) dengan penerusan URL OAuth & input kode dari Android.
//! - `engine_uninstall`: penghapusan bersih `~/.oopbot/engines/<id>/`.
//! - `detect_session_engine`: deteksi tag `"engine": "acp:<id>"` dari header file sesi JSONL.

use super::{sha256_hex, TMP_SEQ};
use crate::common::time::iso_now;
use crate::common::BoxError;
use crate::config::{
    builtin_external_engines, engine_dir, find_builtin_external_engine, is_safe_engine_id,
    load_engine_manifest, parse_acp_engine_selector, save_engine_manifest, ExternalEngineManifest,
    ExternalEngineSpec,
};
use serde_json::{json, Value};
use std::io::{BufRead, BufReader, BufWriter, Write};
use std::net::TcpStream;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::atomic::Ordering;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt};

const ANTIGRAVITY_STANDALONE_ASSET: &str = "antigravity-termux-standalone.tar.gz";

/// Cek apakah Termux glibc dynamic loader tersedia di `$PREFIX/glibc/lib/ld-linux-aarch64.so.1`.
pub(super) fn is_termux_glibc_ready(prefix: &Path) -> bool {
    prefix.join("glibc/lib/ld-linux-aarch64.so.1").is_file()
}

/// Cek ringan apakah engine eksternal sudah memiliki sesi autentikasi tersimpan di home user.
pub(super) fn check_engine_authenticated(engine_id: &str, home: &str) -> bool {
    if engine_id.eq_ignore_ascii_case("antigravity") {
        if std::env::var("GEMINI_API_KEY")
            .ok()
            .filter(|s| !s.trim().is_empty())
            .is_some()
        {
            return true;
        }
        let cli_dir = Path::new(home).join(".gemini/antigravity-cli");
        for candidate in [
            "oauth_creds.json",
            "credentials.json",
            "auth.json",
            "token.json",
        ] {
            if let Ok(meta) = std::fs::metadata(cli_dir.join(candidate)) {
                if meta.is_file() && meta.len() > 2 {
                    return true;
                }
            }
        }
        return false;
    }
    false
}

/// Deteksi engine dari header baris pertama file sesi JSONL (jika sesi sebelumnya dibuat oleh ACP engine).
pub(super) fn detect_session_engine(session_path: &Path) -> Option<String> {
    let file = std::fs::File::open(session_path).ok()?;
    let mut reader = BufReader::new(file);
    let mut first_line = String::new();
    let n = reader.read_line(&mut first_line).ok()?;
    if n == 0 {
        return None;
    }
    let val: Value = serde_json::from_str(first_line.trim()).ok()?;
    if val["type"].as_str() != Some("session") {
        return None;
    }
    val["engine"]
        .as_str()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
}

/// Utility op `engine_list`: kembalikan status seluruh external engine bawaan untuk UI `Add a provider`.
pub(super) fn handle_engine_list(config_dir: &Path, prefix: &Path, home: &str) -> Value {
    let glibc_ready = is_termux_glibc_ready(prefix);
    let engines: Vec<Value> = builtin_external_engines()
        .iter()
        .map(|spec| {
            let manifest = load_engine_manifest(config_dir, spec.id);
            let installed = manifest
                .as_ref()
                .map(|m| Path::new(&m.command).is_file())
                .unwrap_or(false);
            let authenticated = installed && check_engine_authenticated(spec.id, home);
            json!({
                "id": spec.id,
                "selector": format!("acp:{}", spec.id),
                "displayName": spec.display_name,
                "subtitle": spec.subtitle,
                "protocol": spec.protocol,
                "githubRepo": spec.github_repo,
                "installed": installed,
                "installedVersion": manifest.as_ref().map(|m| m.installed_version.as_str()),
                "command": manifest.as_ref().map(|m| m.command.as_str()),
                "requiresTermuxGlibc": spec.requires_termux_glibc,
                "glibcReady": !spec.requires_termux_glibc || glibc_ready,
                "authenticated": authenticated,
            })
        })
        .collect();

    json!({
        "type": "oopbot_result",
        "ok": true,
        "data": { "engines": engines }
    })
}

/// Utility op `engine_uninstall`: hapus folder `~/.oopbot/engines/<id>/`.
pub(super) fn handle_engine_uninstall(config_dir: &Path, params: &Value) -> Value {
    let raw_id = params["engineId"]
        .as_str()
        .or_else(|| params["id"].as_str())
        .unwrap_or_default();
    let engine_id = parse_acp_engine_selector(raw_id).unwrap_or(raw_id.trim());
    if !is_safe_engine_id(engine_id) {
        return json!({
            "type": "oopbot_result",
            "ok": false,
            "code": "invalid_engine",
            "error": format!("Invalid engine id: '{}'", raw_id),
            "data": {}
        });
    }

    let dir = engine_dir(config_dir, engine_id);
    if dir.exists() {
        if let Err(e) = std::fs::remove_dir_all(&dir) {
            return json!({
                "type": "oopbot_result",
                "ok": false,
                "code": "io_error",
                "error": format!("Gagal menghapus engine '{}': {}", engine_id, e),
                "data": {}
            });
        }
    }

    json!({
        "type": "oopbot_result",
        "ok": true,
        "data": { "id": engine_id, "removed": true }
    })
}

fn emit_progress(writer: &mut BufWriter<TcpStream>, step: &str, line: &str) {
    let msg = json!({
        "type": "oopbot_progress",
        "step": step,
        "line": line,
    });
    let _ = writeln!(writer, "{}", serde_json::to_string(&msg).unwrap_or_default());
    let _ = writer.flush();
}

/// Resolusi `(version, download_url)` dari GitHub Releases API dengan fallback ke URL `latest/download`.
async fn resolve_github_release_asset(
    client: &reqwest::Client,
    spec: &ExternalEngineSpec,
) -> (String, String) {
    let fallback_url = format!(
        "https://github.com/{}/releases/latest/download/{}",
        spec.github_repo, ANTIGRAVITY_STANDALONE_ASSET
    );
    let api_url = format!("https://api.github.com/repos/{}/releases/latest", spec.github_repo);
    let Ok(resp) = client
        .get(&api_url)
        .header("User-Agent", format!("oopbot/{}", env!("CARGO_PKG_VERSION")))
        .header("Accept", "application/vnd.github+json")
        .send()
        .await
    else {
        return ("latest".to_string(), fallback_url);
    };
    if !resp.status().is_success() {
        return ("latest".to_string(), fallback_url);
    }
    let Ok(body) = resp.json::<Value>().await else {
        return ("latest".to_string(), fallback_url);
    };
    let tag = body["tag_name"]
        .as_str()
        .map(|s| s.trim_start_matches('v').to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "latest".to_string());
    let asset_url = body["assets"]
        .as_array()
        .and_then(|assets| {
            assets.iter().find_map(|a| {
                let name = a["name"].as_str()?;
                if name.ends_with(".tar.gz") || name.ends_with(".tgz") {
                    a["browser_download_url"].as_str().map(str::to_string)
                } else {
                    None
                }
            })
        })
        .unwrap_or(fallback_url);
    (tag, asset_url)
}

/// Cari path binary target (mis. `bin/agy`) di dalam folder hasil ekstrak tarball (root atau 1 level nested).
fn locate_extracted_binary(staging_root: &Path, rel_path: &str) -> Option<(PathBuf, PathBuf)> {
    let direct = staging_root.join(rel_path);
    if direct.is_file() {
        return Some((staging_root.to_path_buf(), direct));
    }
    let file_name = Path::new(rel_path).file_name()?;
    let entries = std::fs::read_dir(staging_root).ok()?;
    for entry in entries.flatten() {
        let sub = entry.path();
        if sub.is_dir() {
            let nested = sub.join(rel_path);
            if nested.is_file() {
                return Some((sub, nested));
            }
            let flat = sub.join(file_name);
            if flat.is_file() {
                return Some((sub, flat));
            }
        }
    }
    None
}

/// Instalasi inti engine eksternal dari URL tarball dengan staging, verifikasi opsional, dan atomic swap + rollback.
pub(super) async fn install_engine_archive<F>(
    config_dir: &Path,
    spec: &ExternalEngineSpec,
    archive_url: &str,
    version_hint: &str,
    verify_binary: bool,
    mut on_progress: F,
) -> Result<ExternalEngineManifest, String>
where
    F: FnMut(&str, &str),
{
    let seq = TMP_SEQ.fetch_add(1, Ordering::Relaxed);
    let engines_root = config_dir.join("engines");
    std::fs::create_dir_all(&engines_root)
        .map_err(|e| format!("Gagal membuat direktori engines: {}", e))?;

    let staging_dir = engines_root.join(format!(
        ".{}.staging.{}.{}",
        spec.id,
        std::process::id(),
        seq
    ));
    let archive_path = engines_root.join(format!(
        ".{}.download.{}.{}.tar.gz",
        spec.id,
        std::process::id(),
        seq
    ));
    std::fs::create_dir_all(&staging_dir)
        .map_err(|e| format!("Gagal membuat direktori staging: {}", e))?;

    let cleanup = |stg: &Path, arc: &Path| {
        let _ = std::fs::remove_dir_all(stg);
        let _ = std::fs::remove_file(arc);
    };

    on_progress("download", &format!("Downloading {}...", spec.display_name));
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(180))
        .build()
        .map_err(|e| {
            cleanup(&staging_dir, &archive_path);
            format!("Gagal inisialisasi HTTP client: {}", e)
        })?;

    let resp = client
        .get(archive_url)
        .header("User-Agent", format!("oopbot/{}", env!("CARGO_PKG_VERSION")))
        .send()
        .await
        .map_err(|e| {
            cleanup(&staging_dir, &archive_path);
            format!("Gagal mengunduh arsip {}: {}", archive_url, e)
        })?;
    if !resp.status().is_success() {
        let status = resp.status();
        cleanup(&staging_dir, &archive_path);
        return Err(format!("Server mengembalikan HTTP {} saat mengunduh arsip", status));
    }
    let bytes = resp.bytes().await.map_err(|e| {
        cleanup(&staging_dir, &archive_path);
        format!("Gagal membaca body arsip: {}", e)
    })?;
    if bytes.is_empty() {
        cleanup(&staging_dir, &archive_path);
        return Err("Arsip unduhan kosong (0 byte)".to_string());
    }
    std::fs::write(&archive_path, &bytes).map_err(|e| {
        cleanup(&staging_dir, &archive_path);
        format!("Gagal menulis arsip sementara: {}", e)
    })?;

    on_progress("extract", "Extracting standalone archive...");
    let tar_out = std::process::Command::new("tar")
        .args(["-xzf"])
        .arg(&archive_path)
        .arg("-C")
        .arg(&staging_dir)
        .output()
        .map_err(|e| {
            cleanup(&staging_dir, &archive_path);
            format!("Gagal menjalankan tar: {}", e)
        })?;
    let _ = std::fs::remove_file(&archive_path);
    if !tar_out.status.success() {
        let err = String::from_utf8_lossy(&tar_out.stderr).trim().to_string();
        let _ = std::fs::remove_dir_all(&staging_dir);
        return Err(format!("Ekstraksi arsip gagal: {}", err));
    }

    let Some((payload_root, staged_bin)) = locate_extracted_binary(&staging_dir, spec.binary_rel_path)
    else {
        let _ = std::fs::remove_dir_all(&staging_dir);
        return Err(format!(
            "Binary '{}' tidak ditemukan di dalam arsip",
            spec.binary_rel_path
        ));
    };

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if let Ok(meta) = std::fs::metadata(&staged_bin) {
            let mut perms = meta.permissions();
            perms.set_mode(0o755);
            let _ = std::fs::set_permissions(&staged_bin, perms);
        }
    }

    let mut resolved_version = version_hint.trim().trim_start_matches('v').to_string();
    if verify_binary {
        on_progress("verify", &format!("Verifying {} binary...", spec.display_name));
        let ver_out = std::process::Command::new(&staged_bin)
            .arg("--version")
            .output()
            .map_err(|e| {
                let _ = std::fs::remove_dir_all(&staging_dir);
                format!("Gagal mengeksekusi binary terpasang: {}", e)
            })?;
        if !ver_out.status.success() {
            let stderr = String::from_utf8_lossy(&ver_out.stderr).trim().to_string();
            let _ = std::fs::remove_dir_all(&staging_dir);
            return Err(format!(
                "Verifikasi binary gagal ({}): {}",
                ver_out.status, stderr
            ));
        }
        let stdout = String::from_utf8_lossy(&ver_out.stdout).trim().to_string();
        if let Some(tok) = stdout
            .split_whitespace()
            .find(|t| t.chars().any(|c| c.is_ascii_digit()))
        {
            resolved_version = tok.trim_start_matches('v').to_string();
        }
    }
    if resolved_version.is_empty() {
        resolved_version = "installed".to_string();
    }

    let bin_bytes = std::fs::read(&staged_bin).unwrap_or_default();
    let digest = sha256_hex(&bin_bytes);
    let rel_from_payload = staged_bin
        .strip_prefix(&payload_root)
        .unwrap_or_else(|_| Path::new(spec.binary_rel_path))
        .to_path_buf();

    // Atomic swap direktori instalasi dengan backup + rollback otomatis bila gagal.
    on_progress("finalize", "Activating installed engine...");
    let target_dir = engine_dir(config_dir, spec.id);
    let backup_dir = engines_root.join(format!(
        ".{}.backup.{}.{}",
        spec.id,
        std::process::id(),
        seq
    ));
    let had_existing = target_dir.exists();
    if had_existing {
        std::fs::rename(&target_dir, &backup_dir).map_err(|e| {
            let _ = std::fs::remove_dir_all(&staging_dir);
            format!("Gagal membackup instalasi lama: {}", e)
        })?;
    }

    let swap_res = std::fs::rename(&payload_root, &target_dir);
    if let Err(e) = swap_res {
        if had_existing {
            let _ = std::fs::rename(&backup_dir, &target_dir);
        }
        let _ = std::fs::remove_dir_all(&staging_dir);
        return Err(format!("Gagal memindahkan hasil instalasi: {}", e));
    }
    if staging_dir.exists() {
        let _ = std::fs::remove_dir_all(&staging_dir);
    }
    if backup_dir.exists() {
        let _ = std::fs::remove_dir_all(&backup_dir);
    }

    let final_bin = target_dir.join(rel_from_payload);
    let manifest = ExternalEngineManifest {
        id: spec.id.to_string(),
        protocol: spec.protocol,
        display_name: spec.display_name.to_string(),
        source_repo: spec.github_repo.to_string(),
        installed_version: resolved_version,
        command: final_bin.to_string_lossy().to_string(),
        args: spec.acp_args.iter().map(|s| s.to_string()).collect(),
        installed_at: iso_now(),
        sha256: Some(digest),
    };
    save_engine_manifest(config_dir, &manifest)?;
    Ok(manifest)
}

/// Host mode `engine_install`: pasang prasyarat Termux (`glibc-repo` & `glibc`) lalu unduh & pasang engine ACP.
pub(super) async fn handle_engine_install_mode(
    params: Value,
    config_dir: &Path,
    prefix: &Path,
    mut writer: BufWriter<TcpStream>,
) -> Result<(), BoxError> {
    let raw_id = params["engineId"]
        .as_str()
        .or_else(|| params["id"].as_str())
        .unwrap_or("antigravity");
    let engine_id = parse_acp_engine_selector(raw_id).unwrap_or(raw_id.trim());
    let Some(spec) = find_builtin_external_engine(engine_id) else {
        let err = json!({
            "type": "oopbot_result",
            "ok": false,
            "code": "unknown_engine",
            "error": format!("Engine '{}' tidak dikenal", raw_id),
            "data": {}
        });
        writeln!(writer, "{}", serde_json::to_string(&err)?)?;
        writer.flush()?;
        return Ok(());
    };

    let skip_prereqs = params["skipPrereqs"].as_bool().unwrap_or(false);
    if spec.requires_termux_glibc && !skip_prereqs && !is_termux_glibc_ready(prefix) {
        emit_progress(
            &mut writer,
            "prereqs",
            "Checking Termux glibc runtime (pkg install -y glibc-repo glibc)...",
        );
        let manual_cmd = "pkg install -y glibc-repo && pkg install -y glibc";
        let pkg_status = tokio::process::Command::new("sh")
            .args(["-c", manual_cmd])
            .stdin(Stdio::null())
            .output()
            .await;
        match pkg_status {
            Ok(out) if out.status.success() && is_termux_glibc_ready(prefix) => {
                emit_progress(&mut writer, "prereqs", "Termux glibc runtime ready.");
            }
            Ok(out) => {
                let detail = String::from_utf8_lossy(&out.stderr).trim().to_string();
                let err = json!({
                    "type": "oopbot_result",
                    "ok": false,
                    "code": "prereq_failed",
                    "error": if detail.is_empty() {
                        "Gagal memasang Termux glibc runtime secara otomatis".to_string()
                    } else {
                        format!("Gagal memasang Termux glibc: {}", detail)
                    },
                    "data": { "manualCommand": manual_cmd }
                });
                writeln!(writer, "{}", serde_json::to_string(&err)?)?;
                writer.flush()?;
                return Ok(());
            }
            Err(e) => {
                let err = json!({
                    "type": "oopbot_result",
                    "ok": false,
                    "code": "prereq_failed",
                    "error": format!("Gagal menjalankan installer glibc: {}", e),
                    "data": { "manualCommand": manual_cmd }
                });
                writeln!(writer, "{}", serde_json::to_string(&err)?)?;
                writer.flush()?;
                return Ok(());
            }
        }
    }

    let (version_hint, download_url) = if let Some(custom_url) =
        params["downloadUrl"].as_str().filter(|s| !s.trim().is_empty())
    {
        (
            params["version"]
                .as_str()
                .unwrap_or("custom")
                .to_string(),
            custom_url.to_string(),
        )
    } else {
        emit_progress(&mut writer, "resolve", "Checking latest release...");
        let client = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(20))
            .build()?;
        resolve_github_release_asset(&client, spec).await
    };

    let verify_binary = !params["skipVerify"].as_bool().unwrap_or(false);
    let res = install_engine_archive(
        config_dir,
        spec,
        &download_url,
        &version_hint,
        verify_binary,
        |step, line| emit_progress(&mut writer, step, line),
    )
    .await;

    let final_msg = match res {
        Ok(manifest) => json!({
            "type": "oopbot_result",
            "ok": true,
            "data": { "engine": manifest }
        }),
        Err(err) => json!({
            "type": "oopbot_result",
            "ok": false,
            "code": "install_failed",
            "error": err,
            "data": {}
        }),
    };
    writeln!(writer, "{}", serde_json::to_string(&final_msg)?)?;
    writer.flush()?;
    Ok(())
}

fn extract_first_https_url(line: &str) -> Option<String> {
    let start = line.find("https://")?;
    let rest = &line[start..];
    let url: String = rest
        .chars()
        .take_while(|c| !c.is_whitespace() && !matches!(c, '"' | '\'' | '<' | '>' | ')' | ']'))
        .collect();
    if url.len() > "https://".len() {
        Some(url)
    } else {
        None
    }
}

/// Host mode `engine_login`: jalankan alur login interaktif (`agy acp --login`) dengan penerusan URL OAuth & input kode.
pub(super) async fn handle_engine_login_mode(
    params: Value,
    config_dir: &Path,
    reader: BufReader<TcpStream>,
    mut writer: BufWriter<TcpStream>,
) -> Result<(), BoxError> {
    let raw_id = params["engineId"]
        .as_str()
        .or_else(|| params["id"].as_str())
        .unwrap_or("antigravity");
    let engine_id = parse_acp_engine_selector(raw_id).unwrap_or(raw_id.trim());
    let Some(spec) = find_builtin_external_engine(engine_id) else {
        let err = json!({
            "type": "oopbot_result",
            "ok": false,
            "code": "unknown_engine",
            "error": format!("Engine '{}' tidak dikenal", raw_id),
            "data": {}
        });
        writeln!(writer, "{}", serde_json::to_string(&err)?)?;
        writer.flush()?;
        return Ok(());
    };

    let Some(manifest) = load_engine_manifest(config_dir, spec.id) else {
        let err = json!({
            "type": "oopbot_result",
            "ok": false,
            "code": "engine_not_installed",
            "error": format!("Engine '{}' belum diunduh. Unduh plugin terlebih dahulu.", spec.display_name),
            "data": {}
        });
        writeln!(writer, "{}", serde_json::to_string(&err)?)?;
        writer.flush()?;
        return Ok(());
    };

    let login_args: Vec<String> = params["loginArgs"]
        .as_array()
        .map(|arr| {
            arr.iter()
                .filter_map(|v| v.as_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_else(|| spec.login_args.iter().map(|s| s.to_string()).collect());

    let mut child = tokio::process::Command::new(&manifest.command)
        .args(&login_args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .map_err(|e| format!("Gagal memulai proses login '{}': {}", spec.display_name, e))?;

    let mut child_stdin = child.stdin.take();
    let stdout = child.stdout.take();
    let stderr = child.stderr.take();

    let (proc_tx, mut proc_rx) = tokio::sync::mpsc::unbounded_channel::<String>();
    if let Some(out) = stdout {
        let tx = proc_tx.clone();
        tokio::spawn(async move {
            let mut lines = tokio::io::BufReader::new(out).lines();
            while let Ok(Some(l)) = lines.next_line().await {
                let _ = tx.send(l);
            }
        });
    }
    if let Some(err) = stderr {
        let tx = proc_tx.clone();
        tokio::spawn(async move {
            let mut lines = tokio::io::BufReader::new(err).lines();
            while let Ok(Some(l)) = lines.next_line().await {
                let _ = tx.send(l);
            }
        });
    }
    drop(proc_tx);

    let (android_tx, mut android_rx) = tokio::sync::mpsc::unbounded_channel::<Value>();
    std::thread::spawn(move || {
        let mut r = reader;
        let mut line = String::new();
        loop {
            line.clear();
            match r.read_line(&mut line) {
                Ok(0) | Err(_) => break,
                Ok(_) => {
                    if let Ok(v) = serde_json::from_str::<Value>(line.trim()) {
                        if android_tx.send(v).is_err() {
                            break;
                        }
                    }
                }
            }
        }
    });

    let mut sent_url = false;
    let mut cancelled = false;
    let mut last_lines: Vec<String> = Vec::new();

    loop {
        tokio::select! {
            maybe_line = proc_rx.recv() => {
                match maybe_line {
                    Some(l) => {
                        if last_lines.len() >= 12 {
                            last_lines.remove(0);
                        }
                        last_lines.push(l.clone());
                        emit_progress(&mut writer, "login", &l);
                        if !sent_url {
                            if let Some(url) = extract_first_https_url(&l) {
                                sent_url = true;
                                let url_msg = json!({
                                    "type": "oopbot_auth_url",
                                    "url": url,
                                });
                                let _ = writeln!(writer, "{}", serde_json::to_string(&url_msg)?);
                                let _ = writer.flush();
                            }
                        }
                    }
                    None => break,
                }
            }
            maybe_android = android_rx.recv() => {
                if let Some(msg) = maybe_android {
                    match msg["type"].as_str() {
                        Some("oopbot_auth_response") => {
                            if let Some(code) = msg["value"].as_str() {
                                if let Some(stdin) = child_stdin.as_mut() {
                                    let _ = stdin.write_all(format!("{}\n", code.trim()).as_bytes()).await;
                                    let _ = stdin.flush().await;
                                }
                            }
                        }
                        Some("oopbot_auth_cancel") => {
                            cancelled = true;
                            let _ = child.start_kill();
                            break;
                        }
                        _ => {}
                    }
                }
            }
        }
    }

    let status = child.wait().await?;
    let final_res = if cancelled {
        json!({
            "type": "oopbot_result",
            "ok": false,
            "code": "cancelled",
            "error": "Login cancelled",
            "data": {}
        })
    } else if status.success() {
        json!({
            "type": "oopbot_result",
            "ok": true,
            "data": { "engineId": spec.id, "authenticated": true }
        })
    } else {
        json!({
            "type": "oopbot_result",
            "ok": false,
            "code": "login_failed",
            "error": format!(
                "Login {} gagal ({}): {}",
                spec.display_name,
                status,
                last_lines.join(" | ")
            ),
            "data": {}
        })
    };
    writeln!(writer, "{}", serde_json::to_string(&final_res)?)?;
    writer.flush()?;
    Ok(())
}
