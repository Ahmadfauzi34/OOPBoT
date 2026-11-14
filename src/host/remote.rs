use super::{sha256_hex, write_json_file, TMP_SEQ};
use crate::util::{epoch_ms, read_json_file};
use serde_json::{json, Value};
use std::env;
use std::path::{Path, PathBuf};

pub(super) fn remote_ssh_dir(config_dir: &Path) -> PathBuf {
    config_dir.parent().unwrap_or(config_dir).join("ssh")
}

pub(super) fn remote_remotes_dir(config_dir: &Path) -> PathBuf {
    config_dir.parent().unwrap_or(config_dir).join("remotes")
}

pub(super) fn read_remote_stand_in(config_dir: &Path, cwd: &str) -> Option<(String, String)> {
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

pub(super) fn find_termux_bin(name: &str) -> Option<PathBuf> {
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

pub(super) fn is_valid_host_id(id: &str) -> bool {
    id.len() == 12 && id.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
}

pub(super) fn generate_host_id() -> String {
    let seed = format!(
        "{}-{}-{}",
        epoch_ms(),
        std::process::id(),
        TMP_SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
    );
    sha256_hex(seed.as_bytes())[..12].to_string()
}

pub(super) fn parse_remote_address(address: &str) -> Option<(Option<&str>, &str)> {
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

pub(super) fn is_valid_ssh_alias(alias: &str) -> bool {
    !alias.is_empty()
        && alias
            .bytes()
            .next()
            .is_some_and(|b| b.is_ascii_alphanumeric() || b == b'_')
        && alias
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'.' | b'-'))
}

pub(super) fn load_remote_hosts_list(ssh_dir: &Path) -> Vec<Value> {
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

pub(super) fn write_remote_hosts_and_ssh_config(ssh_dir: &Path, home: &str, hosts: &[Value]) -> Result<(), String> {
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

pub(super) fn list_remote_stand_ins(remotes_dir: &Path) -> Vec<Value> {
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

pub(super) fn handle_remote_hosts(config_dir: &Path, _home: &str) -> Value {
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

pub(super) fn invalid_remote_host(field: &str, key: &str, msg: &str) -> Value {
    json!({
        "type": "pipkin_result",
        "ok": false,
        "code": "remote_host_invalid",
        "error": msg,
        "i18n": { "key": key, "args": { "field": field } },
        "data": {}
    })
}

pub(super) fn handle_remote_host_save(config_dir: &Path, home: &str, params: &Value) -> Value {
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

pub(super) fn disconnect_ssh_control(ssh_dir: &Path, host_id: &str) {
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

pub(super) fn handle_remote_host_remove(config_dir: &Path, home: &str, params: &Value) -> Value {
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

pub(super) fn normalize_remote_path(raw: &str) -> Result<String, Value> {
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

pub(super) fn handle_remote_workspace(config_dir: &Path, params: &Value) -> Value {
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

pub(super) fn handle_remote_status(config_dir: &Path, params: &Value) -> Value {
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

pub(super) fn handle_remote_disconnect(config_dir: &Path, params: &Value) -> Value {
    let id = params["id"].as_str().unwrap_or("");
    let ssh_dir = remote_ssh_dir(config_dir);
    disconnect_ssh_control(&ssh_dir, id);
    json!({
        "type": "pipkin_result",
        "ok": true,
        "data": {}
    })
}

pub(super) fn handle_remote_forget_host_key(config_dir: &Path, params: &Value) -> Value {
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

pub(super) fn handle_remote_key(config_dir: &Path) -> Value {
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

pub(super) fn handle_remote_key_new(config_dir: &Path) -> Value {
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

