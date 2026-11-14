use super::TMP_SEQ;
use crate::gemini::BoxError;
use serde_json::{json, Value};
use std::io::{BufWriter, Write};
use std::net::TcpStream;
use std::path::{Path, PathBuf};

const OOPBOT_RELEASES_BASE: &str = "https://github.com/Ahmadfauzi34/OOPBoT/releases";
const OOPBOT_RELEASE_ASSET: &str = "oopbot-aarch64-linux-android";

#[derive(Debug, PartialEq, Eq)]
pub(super) struct EnsureError {
    pub(super) code: &'static str,
    pub(super) message: String,
    pub(super) i18n: Option<Value>,
}

pub(super) fn oopbot_release_urls(expected_version: &str) -> Vec<String> {
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

pub(super) fn parse_oopbot_version_output(stdout: &str) -> Option<String> {
    let first_line = stdout.lines().next()?.trim();
    let mut parts = first_line.split_whitespace();
    match (parts.next(), parts.next()) {
        (Some("oopbot"), Some(ver)) if !ver.is_empty() => Some(ver.to_string()),
        _ => None,
    }
}

pub(super) fn is_executable_file_busy(err: &std::io::Error) -> bool {
    if err.kind() == std::io::ErrorKind::ExecutableFileBusy {
        return true;
    }
    #[cfg(unix)]
    {
        err.raw_os_error() == Some(26)
    }
    #[cfg(not(unix))]
    {
        false
    }
}

pub(super) fn read_oopbot_binary_version_detail(path: &Path) -> Result<String, String> {
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

pub(super) fn read_oopbot_binary_version(path: &Path) -> Option<String> {
    read_oopbot_binary_version_detail(path).ok()
}

#[cfg(unix)]
pub(super) fn set_executable_permissions(path: &Path) -> std::io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    let mut perms = std::fs::metadata(path)?.permissions();
    perms.set_mode(0o755);
    std::fs::set_permissions(path, perms)
}

#[cfg(not(unix))]
pub(super) fn set_executable_permissions(_path: &Path) -> std::io::Result<()> {
    Ok(())
}

pub(super) fn atomic_write_binary(target: &Path, bytes: &[u8]) -> std::io::Result<()> {
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

pub(super) fn sync_secondary_targets(
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

pub(super) async fn ensure_oopbot_binary_with<F>(
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

pub(super) async fn handle_ensure_mode(
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

pub(super) fn check_rpc_expected_version(params: &Value, installed_version: &str) -> Option<Value> {
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

