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
use tokio::io::{AsyncReadExt, AsyncWriteExt};

const ANTIGRAVITY_STANDALONE_ASSET: &str = "antigravity-termux-standalone.tar.gz";

/// Cek apakah Termux glibc dynamic loader tersedia di `$PREFIX/glibc/lib/ld-linux-aarch64.so.1`.
pub(super) fn is_termux_glibc_ready(prefix: &Path) -> bool {
    prefix.join("glibc/lib/ld-linux-aarch64.so.1").is_file()
}

/// Pastikan env minimal Termux (`PREFIX`, `TERMUX_VERSION`) tersedia saat menjalankan binary
/// standalone Termux seperti `agy` dari `RunCommandService` (`bash --noprofile --norc -c`).
pub(super) fn termux_subprocess_env(prefix: &Path) -> Vec<(String, String)> {
    if !prefix.join("bin").is_dir() {
        return Vec::new();
    }
    let mut env_pairs = Vec::new();
    let prefix_missing = std::env::var("PREFIX")
        .ok()
        .map(|s| s.trim().is_empty())
        .unwrap_or(true);
    if prefix_missing {
        env_pairs.push(("PREFIX".to_string(), prefix.to_string_lossy().to_string()));
    }
    let termux_ver_missing = std::env::var("TERMUX_VERSION")
        .ok()
        .map(|s| s.trim().is_empty())
        .unwrap_or(true);
    if termux_ver_missing {
        env_pairs.push(("TERMUX_VERSION".to_string(), "0.118.0".to_string()));
    }
    env_pairs
}

fn cpuinfo_lacks_lse_atomics() -> bool {
    if std::env::consts::ARCH != "aarch64" {
        return false;
    }
    let Ok(cpuinfo) = std::fs::read_to_string("/proc/cpuinfo") else {
        return false;
    };
    let Some(features_line) = cpuinfo
        .lines()
        .find(|line| line.to_ascii_lowercase().starts_with("features"))
    else {
        return false;
    };
    !features_line
        .split_whitespace()
        .any(|tok| tok == "atomics")
}

/// Kumpulkan paket Termux yang dibutuhkan agar engine eksternal (seperti `agy`) dapat berjalan.
pub(super) fn missing_termux_engine_prereqs(
    prefix: &Path,
    spec: &ExternalEngineSpec,
) -> Vec<&'static str> {
    let mut pkgs = Vec::new();
    if spec.requires_termux_glibc {
        if !is_termux_glibc_ready(prefix) {
            pkgs.push("glibc-repo");
            pkgs.push("glibc");
        }
        if !prefix.join("etc/resolv.conf").is_file() {
            pkgs.push("resolv-conf");
        }
        if !prefix.join("etc/tls/cert.pem").is_file() {
            pkgs.push("ca-certificates");
        }
    }
    if spec.requires_arm_lse_or_qemu
        && !prefix.join("bin/qemu-aarch64").is_file()
        && cpuinfo_lacks_lse_atomics()
    {
        pkgs.push("qemu-user-aarch64");
    }
    pkgs
}

pub(super) fn build_termux_prereq_command(pkgs: &[&str]) -> String {
    let has_glibc_repo = pkgs.contains(&"glibc-repo");
    let rest: Vec<&str> = pkgs
        .iter()
        .copied()
        .filter(|p| *p != "glibc-repo")
        .collect();
    if has_glibc_repo {
        if rest.is_empty() {
            "pkg install -y glibc-repo".to_string()
        } else {
            format!("pkg install -y glibc-repo && pkg install -y {}", rest.join(" "))
        }
    } else {
        format!("pkg install -y {}", rest.join(" "))
    }
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
        for rel in [
            ".gemini/antigravity-cli",
            ".gemini/antigravity",
            ".antigravity",
            ".config/antigravity",
        ] {
            let cli_dir = Path::new(home).join(rel);
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
            let manifest_auth = manifest
                .as_ref()
                .and_then(|m| m.authenticated)
                .unwrap_or(false);
            let authenticated =
                installed && (manifest_auth || check_engine_authenticated(spec.id, home));
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

/// Cari path binary target (mis. `agy` atau `bin/agy`) di dalam folder hasil ekstrak tarball
/// (baik di root arsip, di dalam `bin/`, atau di dalam 1 folder wrapper).
fn locate_extracted_binary(staging_root: &Path, rel_path: &str) -> Option<(PathBuf, PathBuf)> {
    let direct = staging_root.join(rel_path);
    if direct.is_file() {
        return Some((staging_root.to_path_buf(), direct));
    }
    let file_name = Path::new(rel_path).file_name()?;
    let root_flat = staging_root.join(file_name);
    if root_flat.is_file() {
        return Some((staging_root.to_path_buf(), root_flat));
    }
    let root_bin = staging_root.join("bin").join(file_name);
    if root_bin.is_file() {
        return Some((staging_root.to_path_buf(), root_bin));
    }
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
            let sub_bin = sub.join("bin").join(file_name);
            if sub_bin.is_file() {
                return Some((sub, sub_bin));
            }
        }
    }
    None
}

#[cfg(unix)]
fn mark_binary_and_siblings_executable(staged_bin: &Path) {
    use std::os::unix::fs::PermissionsExt;
    let set_exec = |p: &Path| {
        if let Ok(meta) = std::fs::metadata(p) {
            if meta.is_file() {
                let mut perms = meta.permissions();
                perms.set_mode(0o755);
                let _ = std::fs::set_permissions(p, perms);
            }
        }
    };
    set_exec(staged_bin);
    // Pastikan twin binary di folder yang sama (mis. `agy.va39` di samping `agy`) juga executable.
    if let (Some(parent), Some(stem)) = (
        staged_bin.parent(),
        staged_bin.file_name().and_then(|n| n.to_str()),
    ) {
        let prefix = format!("{}.", stem);
        if let Ok(entries) = std::fs::read_dir(parent) {
            for entry in entries.flatten() {
                let p = entry.path();
                if let Some(name) = p.file_name().and_then(|n| n.to_str()) {
                    if name.starts_with(&prefix) {
                        set_exec(&p);
                    }
                }
            }
        }
    }
}

#[cfg(not(unix))]
fn mark_binary_and_siblings_executable(_staged_bin: &Path) {}

/// Instalasi inti engine eksternal dari URL tarball dengan staging, verifikasi opsional, dan atomic swap + rollback.
#[cfg_attr(not(test), allow(dead_code))]
pub(super) async fn install_engine_archive<F>(
    config_dir: &Path,
    spec: &ExternalEngineSpec,
    archive_url: &str,
    version_hint: &str,
    verify_binary: bool,
    on_progress: F,
) -> Result<ExternalEngineManifest, String>
where
    F: FnMut(&str, &str),
{
    install_engine_archive_with_prefix(
        config_dir,
        None,
        spec,
        archive_url,
        version_hint,
        verify_binary,
        on_progress,
    )
    .await
}

pub(super) async fn install_engine_archive_with_prefix<F>(
    config_dir: &Path,
    prefix: Option<&Path>,
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
        .connect_timeout(std::time::Duration::from_secs(30))
        .timeout(std::time::Duration::from_secs(600))
        .build()
        .map_err(|e| {
            cleanup(&staging_dir, &archive_path);
            format!("Gagal inisialisasi HTTP client: {}", e)
        })?;

    let mut resp = client
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

    let total_bytes = resp.content_length();
    let mut out_file = std::fs::File::create(&archive_path).map_err(|e| {
        cleanup(&staging_dir, &archive_path);
        format!("Gagal membuat arsip sementara: {}", e)
    })?;
    let mut downloaded: u64 = 0;
    let mut last_reported_mb: u64 = 0;
    while let Some(chunk) = resp.chunk().await.map_err(|e| {
        cleanup(&staging_dir, &archive_path);
        format!("Gagal membaca body arsip: {}", e)
    })? {
        out_file.write_all(&chunk).map_err(|e| {
            cleanup(&staging_dir, &archive_path);
            format!("Gagal menulis arsip sementara: {}", e)
        })?;
        downloaded += chunk.len() as u64;
        let mb = downloaded / (1024 * 1024);
        if mb >= last_reported_mb + 5 {
            last_reported_mb = mb;
            let msg = match total_bytes {
                Some(total) if total > 0 => format!(
                    "Downloading {}... {} / {} MB",
                    spec.display_name,
                    mb,
                    (total / (1024 * 1024)).max(1)
                ),
                _ => format!("Downloading {}... {} MB", spec.display_name, mb),
            };
            on_progress("download", &msg);
        }
    }
    drop(out_file);
    if downloaded == 0 {
        cleanup(&staging_dir, &archive_path);
        return Err("Arsip unduhan kosong (0 byte)".to_string());
    }

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

    mark_binary_and_siblings_executable(&staged_bin);

    let mut resolved_version = version_hint.trim().trim_start_matches('v').to_string();
    if verify_binary {
        on_progress("verify", &format!("Verifying {} binary...", spec.display_name));
        let mut ver_cmd = std::process::Command::new(&staged_bin);
        ver_cmd.arg("--version");
        if let Some(p) = prefix {
            for (k, v) in termux_subprocess_env(p) {
                ver_cmd.env(k, v);
            }
        }
        let ver_out = ver_cmd.output().map_err(|e| {
            let _ = std::fs::remove_dir_all(&staging_dir);
            format!("Gagal mengeksekusi binary terpasang: {}", e)
        })?;
        if !ver_out.status.success() {
            let stderr = String::from_utf8_lossy(&ver_out.stderr).trim().to_string();
            let stdout = String::from_utf8_lossy(&ver_out.stdout).trim().to_string();
            let detail = match (stderr.is_empty(), stdout.is_empty()) {
                (false, _) => stderr,
                (true, false) => stdout,
                (true, true) => String::new(),
            };
            let _ = std::fs::remove_dir_all(&staging_dir);
            return Err(format!(
                "Verifikasi binary gagal ({}): {}",
                ver_out.status, detail
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
        authenticated: None,
        default_model: None,
        default_thinking_level: None,
    };
    save_engine_manifest(config_dir, &manifest)?;
    Ok(manifest)
}

/// Host mode `engine_install`: pasang prasyarat Termux (`glibc-repo`, `glibc`, `resolv-conf`, dll.)
/// lalu unduh & pasang engine ACP.
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
    let missing_pkgs = if skip_prereqs {
        Vec::new()
    } else {
        missing_termux_engine_prereqs(prefix, spec)
    };
    if !missing_pkgs.is_empty() {
        let manual_cmd = build_termux_prereq_command(&missing_pkgs);
        emit_progress(
            &mut writer,
            "prereqs",
            &format!("Installing Termux runtime packages ({})...", missing_pkgs.join(", ")),
        );
        let pkg_status = tokio::process::Command::new("sh")
            .args(["-c", &manual_cmd])
            .stdin(Stdio::null())
            .output()
            .await;
        match pkg_status {
            Ok(out) if out.status.success() && missing_termux_engine_prereqs(prefix, spec).is_empty() => {
                emit_progress(&mut writer, "prereqs", "Termux runtime packages ready.");
            }
            Ok(out) => {
                let detail = String::from_utf8_lossy(&out.stderr).trim().to_string();
                let err = json!({
                    "type": "oopbot_result",
                    "ok": false,
                    "code": "prereq_failed",
                    "error": if detail.is_empty() {
                        format!("Gagal memasang prasyarat Termux ({}) secara otomatis", missing_pkgs.join(", "))
                    } else {
                        format!("Gagal memasang prasyarat Termux ({}): {}", missing_pkgs.join(", "), detail)
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
                    "error": format!("Gagal menjalankan installer prasyarat Termux: {}", e),
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
    let res = install_engine_archive_with_prefix(
        config_dir,
        Some(prefix),
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
        .take_while(|c| {
            !c.is_whitespace()
                && !c.is_control()
                && !matches!(c, '"' | '\'' | '<' | '>' | ')' | ']' | '\\')
        })
        .collect();
    if url.len() > "https://".len() {
        Some(url)
    } else {
        None
    }
}

fn looks_like_code_prompt(line: &str) -> bool {
    let lower = line.trim().to_ascii_lowercase();
    if lower.is_empty() {
        return false;
    }
    let has_keyword = lower.contains("code")
        || lower.contains("kode")
        || lower.contains("token")
        || lower.contains("paste");
    has_keyword && (lower.ends_with(':') || lower.ends_with('?'))
}

fn emit_manual_code_prompt(writer: &mut BufWriter<TcpStream>, id: u64, message: &str) {
    let prompt_msg = json!({
        "type": "oopbot_auth_prompt",
        "id": id,
        "prompt": {
            "type": "manual_code",
            "message": message,
            "placeholder": "4/0A..."
        }
    });
    let _ = writeln!(writer, "{}", serde_json::to_string(&prompt_msg).unwrap_or_default());
    let _ = writer.flush();
}

fn spawn_login_pipe_reader<R>(
    mut pipe: R,
    tx: tokio::sync::mpsc::UnboundedSender<String>,
) where
    R: tokio::io::AsyncRead + Unpin + Send + 'static,
{
    tokio::spawn(async move {
        let mut buf = [0u8; 2048];
        let mut pending = Vec::<u8>::new();
        let flush_pending = |pending: &mut Vec<u8>| {
            if pending.is_empty() {
                return;
            }
            let text = String::from_utf8_lossy(pending).trim_end_matches('\r').to_string();
            pending.clear();
            if !text.trim().is_empty() {
                let _ = tx.send(text);
            }
        };
        loop {
            let read_res = tokio::time::timeout(
                std::time::Duration::from_millis(150),
                pipe.read(&mut buf),
            )
            .await;
            match read_res {
                Ok(Ok(0)) | Ok(Err(_)) => {
                    flush_pending(&mut pending);
                    break;
                }
                Ok(Ok(n)) => {
                    for &b in &buf[..n] {
                        if b == b'\n' || b == b'\r' {
                            flush_pending(&mut pending);
                        } else {
                            pending.push(b);
                        }
                    }
                }
                Err(_) => {
                    // Flush prompt tanpa newline (mis. "Enter authorization code: ") agar UI tidak menunggu.
                    flush_pending(&mut pending);
                }
            }
        }
    });
}

/// Host mode `engine_login`: jalankan alur login interaktif (`agy acp --login`) dengan penerusan URL OAuth & input kode.
pub(super) async fn handle_engine_login_mode(
    params: Value,
    config_dir: &Path,
    prefix: &Path,
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

    let Some(mut manifest) = load_engine_manifest(config_dir, spec.id) else {
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

    let mut cmd = tokio::process::Command::new(&manifest.command);
    cmd.args(&login_args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    for (k, v) in termux_subprocess_env(prefix) {
        cmd.env(k, v);
    }
    let mut child = cmd
        .spawn()
        .map_err(|e| format!("Gagal memulai proses login '{}': {}", spec.display_name, e))?;

    let mut child_stdin = child.stdin.take();
    let stdout = child.stdout.take();
    let stderr = child.stderr.take();

    let (proc_tx, mut proc_rx) = tokio::sync::mpsc::unbounded_channel::<String>();
    if let Some(out) = stdout {
        spawn_login_pipe_reader(out, proc_tx.clone());
    }
    if let Some(err) = stderr {
        spawn_login_pipe_reader(err, proc_tx.clone());
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
    let mut prompt_id: u64 = 0;
    let mut sent_response = false;
    let mut android_closed = false;
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
                                if prompt_id == 0 {
                                    prompt_id = 1;
                                    emit_manual_code_prompt(
                                        &mut writer,
                                        prompt_id,
                                        "Paste the authorization code from your browser",
                                    );
                                }
                            }
                        }
                        if prompt_id == 0 && looks_like_code_prompt(&l) {
                            prompt_id = 1;
                            emit_manual_code_prompt(&mut writer, prompt_id, l.trim());
                        }
                    }
                    None => break,
                }
            }
            maybe_android = android_rx.recv(), if !android_closed => {
                match maybe_android {
                    Some(msg) => match msg["type"].as_str() {
                        Some("oopbot_auth_response") => {
                            if let Some(code) = msg["value"].as_str() {
                                sent_response = true;
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
                    None => {
                        android_closed = true;
                        if !sent_response {
                            cancelled = true;
                            let _ = child.start_kill();
                            break;
                        }
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
        manifest.authenticated = Some(true);
        let _ = save_engine_manifest(config_dir, &manifest);
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
