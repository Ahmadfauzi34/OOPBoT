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

pub(super) fn resolve_scoped_path(
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

pub(super) fn resolve_browse_path(raw: &str, home: &str) -> PathBuf {
    if raw == "~" {
        PathBuf::from(home)
    } else if let Some(stripped) = raw.strip_prefix("~/") {
        PathBuf::from(home).join(stripped)
    } else {
        PathBuf::from(raw)
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


