use crate::gemini::BoxError;
use serde_json::{json, Value};
use std::io::{BufRead, BufReader, BufWriter, Write};
use std::net::TcpStream;
use std::path::{Path, PathBuf};
use std::time::UNIX_EPOCH;

/// Menangani mode `browse`: penjelajahan direktori untuk pemilih folder workspace di UI Android
pub(super) async fn handle_browse_mode(
    _params: Value,
    home: &str,
    reader: BufReader<TcpStream>,
    writer: BufWriter<TcpStream>,
) -> Result<(), BoxError> {
    run_browse_loop(home, reader, writer)
}

pub(super) fn run_browse_loop<R: BufRead, W: Write>(
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
                let reply = match resolve_browse_path_checked(raw_path, home) {
                    None => json!({
                        "type": "pipkin_listing",
                        "id": id,
                        "path": raw_path,
                        "parent": Value::Null,
                        "dirs": [],
                        "error": "Paths start with / or ~.",
                        "errorI18n": { "key": "browse_path_not_absolute" }
                    }),
                    Some(resolved) => {
                        let parent = if resolved == Path::new("/") {
                            None
                        } else {
                            resolved.parent().and_then(|p| {
                                let s = p.to_string_lossy().to_string();
                                if s.is_empty() { None } else { Some(s) }
                            })
                        };

                        match std::fs::read_dir(&resolved) {
                            Err(e) => {
                                let (msg, i18n) = describe_browse_io_error(&e);
                                json!({
                                    "type": "pipkin_listing",
                                    "id": id,
                                    "path": resolved.to_string_lossy(),
                                    "parent": parent,
                                    "dirs": [],
                                    "error": msg,
                                    "errorI18n": i18n
                                })
                            }
                            Ok(entries) => {
                                let mut dirs = Vec::new();
                                for entry in entries.flatten() {
                                    let p = entry.path();
                                    if p.is_dir() {
                                        dirs.push(entry.file_name().to_string_lossy().to_string());
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

                                json!({
                                    "type": "pipkin_listing",
                                    "id": id,
                                    "path": resolved.to_string_lossy(),
                                    "parent": parent,
                                    "dirs": dirs
                                })
                            }
                        }
                    }
                };
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
pub(super) async fn handle_files_mode(
    _params: Value,
    reader: BufReader<TcpStream>,
    writer: BufWriter<TcpStream>,
) -> Result<(), BoxError> {
    run_files_loop(reader, writer)
}

pub(super) fn is_safe_relative_subpath(raw: &str) -> bool {
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

pub(super) fn nearest_existing_canonical(path: &Path) -> Option<PathBuf> {
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

#[derive(Debug, Clone)]
pub(super) struct FilesErr {
    code: &'static str,
    msg: String,
    i18n: Value,
}

impl FilesErr {
    fn simple(code: &'static str, i18n_key: &'static str, msg: impl Into<String>) -> Self {
        Self {
            code,
            msg: msg.into(),
            i18n: json!({ "key": i18n_key }),
        }
    }
}

fn describe_files_io_error(e: &std::io::Error) -> FilesErr {
    use std::io::ErrorKind;
    match e.kind() {
        ErrorKind::PermissionDenied => {
            return FilesErr::simple(
                "files_no_access",
                "files_no_access",
                "Termux can't open this.",
            );
        }
        ErrorKind::NotFound => {
            return FilesErr::simple(
                "files_gone",
                "files_gone",
                "This file or folder no longer exists.",
            );
        }
        _ => {}
    }

    #[cfg(any(target_os = "linux", target_os = "android"))]
    if let Some(errno) = e.raw_os_error() {
        match errno {
            13 | 1 => {
                // EACCES / EPERM
                return FilesErr::simple(
                    "files_no_access",
                    "files_no_access",
                    "Termux can't open this.",
                );
            }
            2 => {
                // ENOENT
                return FilesErr::simple(
                    "files_gone",
                    "files_gone",
                    "This file or folder no longer exists.",
                );
            }
            20 => {
                // ENOTDIR
                return FilesErr::simple(
                    "not_a_folder",
                    "not_a_folder",
                    "This isn't a folder.",
                );
            }
            21 => {
                // EISDIR
                return FilesErr::simple(
                    "files_is_folder",
                    "files_is_folder",
                    "This is a folder.",
                );
            }
            40 => {
                // ELOOP
                return FilesErr::simple(
                    "files_link_loop",
                    "files_link_loop",
                    "This link goes round in circles.",
                );
            }
            30 => {
                // EROFS
                return FilesErr::simple(
                    "files_read_only",
                    "files_read_only",
                    "This storage is read-only.",
                );
            }
            28 => {
                // ENOSPC
                return FilesErr::simple(
                    "files_device_full",
                    "files_device_full",
                    "The device is full.",
                );
            }
            _ => {}
        }
    }

    match e.raw_os_error() {
        Some(code) => FilesErr {
            code: "files_unreadable",
            msg: format!("Can't read this (OS {}).", code),
            i18n: json!({ "key": "files_unreadable", "args": { "code": code.to_string() } }),
        },
        None => FilesErr::simple(
            "files_unreadable_unknown",
            "files_unreadable_unknown",
            "Can't read this (unknown error).",
        ),
    }
}

fn files_err_reply(kind: &str, id: i64, path: Option<&str>, ferr: &FilesErr, include_entries: bool) -> Value {
    let mut obj = serde_json::Map::new();
    obj.insert("type".to_string(), json!(kind));
    obj.insert("id".to_string(), json!(id));
    if let Some(p) = path {
        obj.insert("path".to_string(), json!(p));
    }
    obj.insert("error".to_string(), json!(ferr.msg));
    obj.insert("errorI18n".to_string(), ferr.i18n.clone());
    obj.insert("errorCode".to_string(), json!(ferr.code));
    if include_entries {
        obj.insert("entries".to_string(), json!([]));
    }
    Value::Object(obj)
}

pub(super) fn resolve_scoped_path(
    root: &str,
    raw_path: &str,
) -> Result<(PathBuf, PathBuf), FilesErr> {
    let root_path = Path::new(root);
    if root.trim().is_empty() || !root_path.is_absolute() {
        return Err(FilesErr::simple(
            "outside_workspace",
            "files_outside_workspace",
            "Workspace root must be an absolute path.",
        ));
    }
    let canonical_root = std::fs::canonicalize(root_path).map_err(|e| describe_files_io_error(&e))?;
    if !is_safe_relative_subpath(raw_path) {
        return Err(FilesErr::simple(
            "outside_workspace",
            "files_outside_workspace",
            "That path is outside the workspace.",
        ));
    }
    let target = if raw_path == "." || raw_path.is_empty() {
        canonical_root.clone()
    } else {
        canonical_root.join(raw_path.replace('\\', "/"))
    };
    if let Ok(canon_target) = std::fs::canonicalize(&target) {
        if !canon_target.starts_with(&canonical_root) {
            return Err(FilesErr::simple(
                "outside_workspace",
                "files_outside_workspace",
                "That path is outside the workspace.",
            ));
        }
        return Ok((canonical_root, canon_target));
    }
    // Path belum ada atau broken symlink: pastikan ancestor terdekat (dan symlink target jika ada) tidak keluar root
    if let Ok(meta) = std::fs::symlink_metadata(&target) {
        if meta.file_type().is_symlink() {
            return Err(FilesErr::simple(
                "outside_workspace",
                "files_outside_workspace",
                "That symlink target cannot be accessed.",
            ));
        }
    }
    if let Some(canon_parent) = target.parent().and_then(nearest_existing_canonical) {
        if !canon_parent.starts_with(&canonical_root) {
            return Err(FilesErr::simple(
                "outside_workspace",
                "files_outside_workspace",
                "That path is outside the workspace.",
            ));
        }
    }
    Ok((canonical_root, target))
}

pub(super) fn run_files_loop<R: BufRead, W: Write>(
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
                    Err(ferr) => files_err_reply("pipkin_files_listing", id, Some(path), &ferr, true),
                    Ok((canonical_root, target_dir)) => {
                        match std::fs::read_dir(&target_dir) {
                            Err(e) => {
                                let ferr = describe_files_io_error(&e);
                                files_err_reply("pipkin_files_listing", id, Some(path), &ferr, true)
                            }
                            Ok(dir_entries) => {
                                let mut entries = Vec::new();
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
                        }
                    }
                };
                writeln!(writer, "{}", serde_json::to_string(&reply)?)?;
                writer.flush()?;
            }
            Some("pipkin_files_read") | Some("oopbot_files_read") => {
                let reply = match resolve_scoped_path(root, path) {
                    Err(ferr) => files_err_reply("pipkin_files_file", id, Some(path), &ferr, false),
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
                            Ok(m) if m.is_dir() => files_err_reply(
                                "pipkin_files_file",
                                id,
                                Some(path),
                                &FilesErr::simple(
                                    "files_is_folder",
                                    "files_is_folder",
                                    "This is a folder.",
                                ),
                                false,
                            ),
                            Ok(m) if !m.is_file() => files_err_reply(
                                "pipkin_files_file",
                                id,
                                Some(path),
                                &FilesErr::simple(
                                    "files_not_regular",
                                    "files_not_regular",
                                    "This isn't a regular file.",
                                ),
                                false,
                            ),
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
                                    } else {
                                        match std::fs::read(&file_path) {
                                            Ok(bytes) => {
                                                let b64 = encode_base64(&bytes);
                                                json!({
                                                    "type": "pipkin_files_file",
                                                    "id": id,
                                                    "path": path,
                                                    "size": size,
                                                    "kind": "image",
                                                    "content": b64
                                                })
                                            }
                                            Err(e) => {
                                                let ferr = describe_files_io_error(&e);
                                                files_err_reply("pipkin_files_file", id, Some(path), &ferr, false)
                                            }
                                        }
                                    }
                                } else if size > TEXT_LIMIT {
                                    match std::fs::File::open(&file_path).and_then(|mut f| {
                                        let mut sniff_buf = vec![0u8; 8192];
                                        let n = std::io::Read::read(&mut f, &mut sniff_buf)?;
                                        sniff_buf.truncate(n);
                                        Ok(sniff_buf)
                                    }) {
                                        Ok(sniff_buf) => {
                                            let is_text = is_probably_text(&sniff_buf);
                                            json!({
                                                "type": "pipkin_files_file",
                                                "id": id,
                                                "path": path,
                                                "size": size,
                                                "kind": if is_text { "too_large" } else { "binary" }
                                            })
                                        }
                                        Err(e) => {
                                            let ferr = describe_files_io_error(&e);
                                            files_err_reply("pipkin_files_file", id, Some(path), &ferr, false)
                                        }
                                    }
                                } else {
                                    match std::fs::read(&file_path) {
                                        Ok(bytes) => {
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
                                        }
                                        Err(e) => {
                                            let ferr = describe_files_io_error(&e);
                                            files_err_reply("pipkin_files_file", id, Some(path), &ferr, false)
                                        }
                                    }
                                }
                            }
                            Err(e) => {
                                let ferr = describe_files_io_error(&e);
                                files_err_reply("pipkin_files_file", id, Some(path), &ferr, false)
                            }
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
                    let ferr = FilesErr::simple(
                        "files_folder_name_invalid",
                        "files_folder_name_invalid",
                        "Folder names cannot be empty, contain / or be . or ..",
                    );
                    files_err_reply("pipkin_files_made", id, None, &ferr, false)
                } else {
                    match resolve_scoped_path(root, path) {
                        Err(ferr) => files_err_reply("pipkin_files_made", id, None, &ferr, false),
                        Ok((_canonical_root, target_parent)) => {
                            let new_folder = target_parent.join(name);
                            let created_rel = if path == "." || path.is_empty() {
                                name.to_string()
                            } else {
                                format!("{}/{}", path.trim_end_matches('/'), name)
                            };
                            match std::fs::create_dir(&new_folder) {
                                Ok(()) => json!({
                                    "type": "pipkin_files_made",
                                    "id": id,
                                    "path": created_rel
                                }),
                                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                                    let ferr = FilesErr {
                                        code: "files_name_taken",
                                        msg: format!("A file or folder named {} already exists.", name),
                                        i18n: json!({
                                            "key": "files_name_taken",
                                            "args": { "name": name }
                                        }),
                                    };
                                    files_err_reply("pipkin_files_made", id, None, &ferr, false)
                                }
                                Err(e) => {
                                    let ferr = describe_files_io_error(&e);
                                    files_err_reply("pipkin_files_made", id, None, &ferr, false)
                                }
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

pub(super) fn decode_base64(input: &str) -> Option<Vec<u8>> {
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
            if !compact.len().is_multiple_of(4) {
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

pub(super) fn encode_base64(bytes: &[u8]) -> String {
    const TABLE: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
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

#[cfg(test)]
pub(super) fn resolve_browse_path(raw: &str, home: &str) -> PathBuf {
    if raw == "~" {
        PathBuf::from(home)
    } else if let Some(stripped) = raw.strip_prefix("~/") {
        PathBuf::from(home).join(stripped)
    } else {
        PathBuf::from(raw)
    }
}

pub(super) fn resolve_browse_path_checked(raw: &str, home: &str) -> Option<PathBuf> {
    if raw == "~" {
        Some(PathBuf::from(home))
    } else if let Some(stripped) = raw.strip_prefix("~/") {
        Some(PathBuf::from(home).join(stripped))
    } else if Path::new(raw).is_absolute() {
        Some(PathBuf::from(raw))
    } else {
        None
    }
}

fn describe_browse_io_error(e: &std::io::Error) -> (String, Value) {
    use std::io::ErrorKind;
    match e.kind() {
        ErrorKind::PermissionDenied => {
            return (
                "Termux can't open this folder.".to_string(),
                json!({ "key": "browse_no_access" }),
            );
        }
        ErrorKind::NotFound => {
            return (
                "This folder doesn't exist.".to_string(),
                json!({ "key": "browse_folder_missing" }),
            );
        }
        _ => {}
    }
    #[cfg(any(target_os = "linux", target_os = "android"))]
    if e.raw_os_error() == Some(20) {
        return (
            "This isn't a folder.".to_string(),
            json!({ "key": "not_a_folder" }),
        );
    }
    match e.raw_os_error() {
        Some(code) => (
            format!("Can't list this folder (OS {}).", code),
            json!({ "key": "browse_unlistable", "args": { "code": code.to_string() } }),
        ),
        None => (
            "Can't list this folder (unknown error).".to_string(),
            json!({ "key": "browse_unlistable_unknown" }),
        ),
    }
}

pub(super) fn is_probably_text(bytes: &[u8]) -> bool {
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

pub(super) fn search_folders_recursive(
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


