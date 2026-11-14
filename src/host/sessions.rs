use super::{decode_base64, read_remote_stand_in, TMP_SEQ};
use crate::util::{epoch_ms, parse_iso_to_epoch_ms_opt as parse_iso_to_epoch_ms};
use serde_json::{json, Value};
use std::path::{Path, PathBuf};
use std::time::UNIX_EPOCH;

pub(super) fn is_valid_session_id(id: &str) -> bool {
    !id.is_empty()
        && id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
}

pub(super) fn subagent_transcript_files(config_dir: &Path, session_path: &Path, session_id: &str) -> Vec<PathBuf> {
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
pub(super) fn handle_list_sessions(config_dir: &Path, home: &str) -> Value {
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

pub(super) fn collect_session_files(dir: &Path, out: &mut Vec<PathBuf>) {
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

pub(super) fn handle_session_messages(params: &Value) -> Value {
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


pub(super) fn safe_attachment_name(raw: &str) -> String {
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

pub(super) fn unique_attachment_path(dir: &Path, safe_name: &str) -> PathBuf {
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

pub(super) fn handle_save_attachments(params: &Value, config_dir: &Path) -> Value {
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


pub(super) fn handle_subagent_sessions(config_dir: &Path, params: &Value) -> Value {
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


pub(super) fn handle_delete_sessions(params: &Value) -> Value {
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

