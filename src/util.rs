//! Utilitas bersama lintas modul (waktu UTC/ISO-8601 dan pembacaan file JSON).

use serde_json::{json, Value};
use std::io::Write;
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

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

/// Mengembalikan timestamp Unix saat ini dalam milidetik.
pub(crate) fn epoch_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// Mengonversi timestamp Unix (milidetik) ke format ISO-8601 (`YYYY-MM-DDTHH:MM:SS.mmmZ`).
pub(crate) fn epoch_ms_to_iso(ms: u64) -> String {
    let secs = ms / 1000;
    let millis = ms % 1000;
    // Format ISO-8601 sederhana yang valid untuk Date.parse di Node.js
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

/// Mengembalikan timestamp UTC saat ini dalam format ISO-8601 (`YYYY-MM-DDTHH:MM:SS.mmmZ`).
pub(crate) fn iso_now() -> String {
    epoch_ms_to_iso(epoch_ms())
}

/// Mengonversi tanggal kalender Gregorian `(year, month, day)` menjadi jumlah hari sejak 1970-01-01.
pub(crate) fn days_from_civil(y: i64, m: u32, d: u32) -> i64 {
    let y = y - if m <= 2 { 1 } else { 0 };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = (y - era * 400) as u32;
    let m_adj = if m > 2 { m - 3 } else { m + 9 };
    let doy = (153 * m_adj + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe as i64 - 719_468
}

/// Konversi jumlah hari sejak 1970-01-01 ke `(year, month, day)` (algoritma Howard Hinnant).
pub(crate) fn civil_from_days(z: i64) -> (i64, u32, u32) {
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

/// Mem-parse string ISO-8601 (`YYYY-MM-DD[T| ]HH:MM:SS[.mmm][Z|+HH:MM|-HHMM]`) menjadi milidetik sejak epoch; `None` bila format tidak valid.
pub(crate) fn parse_iso_to_epoch_ms_opt(raw: &str) -> Option<u64> {
    let s = raw.trim();
    if s.len() < 19 {
        return None;
    }
    let bytes = s.as_bytes();
    if bytes[4] != b'-'
        || bytes[7] != b'-'
        || (bytes[10] != b'T' && bytes[10] != b't' && bytes[10] != b' ')
    {
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
    if !(1..=12).contains(&month) || !(1..=31).contains(&day) || hour > 23 || min > 59 || sec > 60
    {
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
            (
                body.get(..2)?.parse::<i64>().ok()?,
                body.get(2..4)?.parse::<i64>().ok()?,
            )
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

/// Mem-parse string opsional ISO-8601 menjadi milidetik sejak epoch, fallback `0` bila kosong/tidak valid.
pub(crate) fn parse_iso_to_epoch_ms(iso: Option<&str>) -> u64 {
    iso.and_then(parse_iso_to_epoch_ms_opt).unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn write_json_atomic_writes_and_sets_mode_and_cleans_up_on_error() {
        let dir = std::env::temp_dir().join(format!("oopbot_util_atomic_{}", std::process::id()));
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
