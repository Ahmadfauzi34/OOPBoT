//! Helper filesystem bersama (pembacaan JSON aman & penulisan file atomik).

use serde_json::{json, Value};
use std::io::Write;
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};

static ATOMIC_WRITE_SEQ: AtomicU64 = AtomicU64::new(0);

/// Membaca file JSON dari disk dan mengembalikan objek kosong `{}` jika file tidak ada atau tidak valid.
pub(crate) fn read_json_file(path: &Path) -> Value {
    if let Ok(content) = std::fs::read_to_string(path) {
        if let Ok(val) = serde_json::from_str(&content) {
            return val;
        }
    }
    json!({})
}

/// Menulis teks secara atomik ke `path` melalui file sementara di direktori yang sama + `sync_all` + `rename`.
/// Jika `unix_mode` diberikan (misalnya `Some(0o600)`), permission disetel pada file sementara sebelum `rename`.
pub(crate) fn write_text_atomic(
    path: &Path,
    content: &str,
    unix_mode: Option<u32>,
) -> std::io::Result<()> {
    let parent = path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    std::fs::create_dir_all(parent)?;

    let seq = ATOMIC_WRITE_SEQ.fetch_add(1, Ordering::Relaxed);
    let pid = std::process::id();
    let base_name = path
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("file");
    let tmp_path = parent.join(format!(".{}.tmp.{}.{}", base_name, pid, seq));

    let write_res = (|| -> std::io::Result<()> {
        #[cfg(unix)]
        let mut file = {
            use std::os::unix::fs::OpenOptionsExt;
            let mut opts = std::fs::OpenOptions::new();
            opts.write(true).create_new(true);
            if let Some(mode) = unix_mode {
                opts.mode(mode);
            }
            opts.open(&tmp_path)?
        };
        #[cfg(not(unix))]
        let mut file = {
            let _ = unix_mode;
            std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&tmp_path)?
        };

        file.write_all(content.as_bytes())?;
        file.sync_all()?;
        drop(file);

        #[cfg(unix)]
        if let Some(mode) = unix_mode {
            use std::os::unix::fs::PermissionsExt;
            let perms = std::fs::Permissions::from_mode(mode);
            std::fs::set_permissions(&tmp_path, perms)?;
        }

        std::fs::rename(&tmp_path, path)?;
        Ok(())
    })();

    if write_res.is_err() {
        let _ = std::fs::remove_file(&tmp_path);
    }
    write_res
}

/// Menulis nilai JSON terformat rapi secara atomik ke `path`.
pub(crate) fn write_json_atomic(
    path: &Path,
    val: &Value,
    unix_mode: Option<u32>,
) -> std::io::Result<()> {
    let text = serde_json::to_string_pretty(val)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
    write_text_atomic(path, &format!("{}\n", text), unix_mode)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn write_json_atomic_writes_and_sets_mode_and_cleans_up_on_error() {
        let dir = std::env::temp_dir().join(format!("oopbot_common_fs_atomic_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let target = dir.join("nested/auth.json");

        write_json_atomic(&target, &json!({ "k": "v" }), Some(0o600)).unwrap();
        let parsed = read_json_file(&target);
        assert_eq!(parsed["k"], "v");

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&target).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o600);
        }

        // Writing when target is an existing directory must fail and clean up its temp file
        let blocked = dir.join("blocked_dir");
        std::fs::create_dir_all(&blocked).unwrap();
        assert!(write_text_atomic(&blocked, "oops", None).is_err());

        let leftovers: Vec<_> = std::fs::read_dir(&dir)
            .unwrap()
            .filter_map(|e| e.ok())
            .filter(|e| e.file_name().to_string_lossy().contains(".tmp."))
            .collect();
        assert!(leftovers.is_empty(), "leftover tmp files: {:?}", leftovers);

        let _ = std::fs::remove_dir_all(&dir);
    }
}
