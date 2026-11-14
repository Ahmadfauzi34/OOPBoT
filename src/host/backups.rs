use super::{write_json_file, TMP_SEQ};
use crate::util::{
    civil_from_days, epoch_ms, epoch_ms_to_iso, iso_now,
    parse_iso_to_epoch_ms_opt as parse_iso_to_epoch_ms,
};
use serde_json::{json, Value};
use std::path::{Path, PathBuf};
use std::time::UNIX_EPOCH;

pub(super) const BACKUP_FORMAT: &str = "pipkin-config-backup";
pub(super) const BACKUP_VERSION: u64 = 1;
pub(super) const BACKUP_EXCLUDED: &[&str] = &["sessions", "bin", "tmp", "pi-debug.log", "crashes.json"];

pub(super) fn backups_dir(config_dir: &Path) -> PathBuf {
    config_dir.parent().unwrap_or(config_dir).join("backups")
}

pub(super) fn validate_backup_path(config_dir: &Path, name: &str) -> Result<PathBuf, Value> {
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

pub(super) fn compact_iso_stamp() -> String {
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


pub(super) fn copy_config_tree(
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

pub(super) fn read_backup_manifest_from_tar(archive: &Path) -> Result<(Value, Vec<String>), (&'static str, &'static str, String)> {
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

pub(super) fn validate_backup_entries(entries: &[String]) -> Result<(), (&'static str, &'static str, String)> {
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

pub(super) fn backup_info_json(archive: &Path, name: &str) -> Value {
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

pub(super) fn handle_list_backups(config_dir: &Path) -> Value {
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

pub(super) fn handle_backup_create(
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

pub(super) fn handle_backup_delete(config_dir: &Path, params: &Value) -> Value {
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

pub(super) fn handle_backup_restore(
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

