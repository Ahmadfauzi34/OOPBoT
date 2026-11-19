//! Single source of truth untuk baca/tulis berkas konfigurasi & kredensial:
//! - `auth.json` (selalu ditulis secara atomik dengan permission Unix `0o600`)
//! - `models.json` (daftar model hasil scan, `baseUrl`, dan `modelOverrides`)
//! - `settings.json` (`defaultProvider`, `defaultModel`, `defaultThinkingLevel`)
//! - `~/.config/oopbot/.env` (token pengguna untuk CLI/REPL)

use super::catalog::ModelInfo;
use crate::common::fs::{read_json_file, write_json_atomic, write_text_atomic};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::path::{Path, PathBuf};

/// Ekstrak kredensial (`(token, "oauth" | "api_key")`) dari `auth.json` untuk provider tertentu.
pub fn extract_auth_credentials(
    auth_json: &Value,
    provider_id: &str,
) -> Option<(String, &'static str)> {
    let entry = auth_json.get(provider_id)?;
    if let Some(s) = entry
        .as_str()
        .map(str::trim)
        .filter(|s| !s.is_empty() && *s != "no-api-key")
    {
        return Some((s.to_string(), "api_key"));
    }

    let obj = entry.as_object()?;
    let declared_type = obj.get("type").and_then(|v| v.as_str()).unwrap_or_default();

    let oauth_candidates = [
        obj.get("access").and_then(|v| v.as_str()),
        obj.get("access_token").and_then(|v| v.as_str()),
        obj.get("accessToken").and_then(|v| v.as_str()),
        obj.get("auth")
            .and_then(|a| a.get("access"))
            .and_then(|v| v.as_str()),
        obj.get("auth")
            .and_then(|a| a.get("access_token"))
            .and_then(|v| v.as_str()),
        obj.get("auth")
            .and_then(|a| a.get("accessToken"))
            .and_then(|v| v.as_str()),
    ];
    let api_candidates = [
        obj.get("apiKey").and_then(|v| v.as_str()),
        obj.get("key").and_then(|v| v.as_str()),
        obj.get("auth")
            .and_then(|a| a.get("apiKey"))
            .and_then(|v| v.as_str()),
        obj.get("auth")
            .and_then(|a| a.get("key"))
            .and_then(|v| v.as_str()),
    ];
    let token_fallback = obj.get("token").and_then(|v| v.as_str()).or_else(|| {
        obj.get("auth")
            .and_then(|a| a.get("token"))
            .and_then(|v| v.as_str())
    });

    if declared_type == "oauth" {
        if let Some(tok) = oauth_candidates
            .into_iter()
            .flatten()
            .chain(token_fallback)
            .map(str::trim)
            .find(|s| !s.is_empty() && *s != "no-api-key")
        {
            return Some((tok.to_string(), "oauth"));
        }
    }

    if let Some(key) = api_candidates
        .into_iter()
        .flatten()
        .map(str::trim)
        .find(|s| !s.is_empty() && *s != "no-api-key")
    {
        return Some((key.to_string(), "api_key"));
    }

    if let Some(tok) = oauth_candidates
        .into_iter()
        .flatten()
        .map(str::trim)
        .find(|s| !s.is_empty() && *s != "no-api-key")
    {
        return Some((tok.to_string(), "oauth"));
    }

    if let Some(tok) = token_fallback
        .map(str::trim)
        .filter(|s| !s.is_empty() && *s != "no-api-key")
    {
        let kind = if declared_type == "oauth" {
            "oauth"
        } else {
            "api_key"
        };
        return Some((tok.to_string(), kind));
    }

    None
}

/// Simpan API key provider ke `auth.json` secara atomik dengan permission `0o600`.
/// Bila `base_url` diberikan (dan tidak kosong), perbarui pula entri `models.json`.
pub(crate) fn save_auth_key(
    config_dir: &Path,
    provider_id: &str,
    api_key: &str,
    base_url: Option<&str>,
) -> std::io::Result<()> {
    let auth_path = config_dir.join("auth.json");
    let mut auth_json = read_json_file(&auth_path);
    if !auth_json.is_object() {
        auth_json = json!({});
    }
    auth_json[provider_id] = json!({
        "type": "api_key",
        "apiKey": api_key,
    });
    write_json_atomic(&auth_path, &auth_json, Some(0o600))?;

    if let Some(url) = base_url.filter(|u| !u.is_empty()) {
        let models_path = config_dir.join("models.json");
        let mut models_json = read_json_file(&models_path);
        if !models_json.is_object() {
            models_json = json!({ "providers": {} });
        }
        if !models_json["providers"].is_object() {
            models_json["providers"] = json!({});
        }
        if !models_json["providers"][provider_id].is_object() {
            models_json["providers"][provider_id] = json!({});
        }
        models_json["providers"][provider_id]["baseUrl"] = json!(url);
        models_json["providers"][provider_id]["apiKey"] = json!(api_key);
        write_json_atomic(&models_path, &models_json, None)?;
    }
    Ok(())
}

/// Hapus kredensial `provider_id` dari `auth.json` dan tulis ulang secara atomik dengan mode `0o600`.
pub(crate) fn remove_auth_key(config_dir: &Path, provider_id: &str) -> std::io::Result<()> {
    let auth_path = config_dir.join("auth.json");
    let mut auth_json = read_json_file(&auth_path);
    if let Some(obj) = auth_json.as_object_mut() {
        obj.remove(provider_id);
        write_json_atomic(&auth_path, &auth_json, Some(0o600))?;
    }
    Ok(())
}

/// Perbarui default provider, model, dan thinking level di `settings.json` secara atomik.
pub(crate) fn save_settings_defaults(
    config_dir: &Path,
    default_provider: Option<&str>,
    default_model: Option<&str>,
    default_thinking_level: Option<&str>,
) -> std::io::Result<()> {
    let settings_path = config_dir.join("settings.json");
    let mut settings = read_json_file(&settings_path);
    if !settings.is_object() {
        settings = json!({});
    }
    if let Some(p) = default_provider {
        settings["defaultProvider"] = json!(p);
    }
    if let Some(m) = default_model {
        settings["defaultModel"] = json!(m);
    }
    if let Some(l) = default_thinking_level {
        settings["defaultThinkingLevel"] = json!(l);
    }
    write_json_atomic(&settings_path, &settings, None)
}

/// Simpan daftar model hasil scan `/models` untuk `provider_id` ke `models.json`.
pub(crate) fn save_scanned_models(
    config_dir: &Path,
    provider_id: &str,
    scanned: &[ModelInfo],
    api_key: &str,
) -> std::io::Result<()> {
    if scanned.is_empty() {
        return Ok(());
    }
    let models_path = config_dir.join("models.json");
    let mut models_json = read_json_file(&models_path);
    if !models_json.is_object() {
        models_json = json!({ "providers": {} });
    }
    if !models_json["providers"].is_object() {
        models_json["providers"] = json!({});
    }
    if !models_json["providers"][provider_id].is_object() {
        models_json["providers"][provider_id] = json!({});
    }
    let serialized: Vec<Value> = scanned
        .iter()
        .map(|m| m.to_oopbot_json(Some(provider_id)))
        .collect();
    models_json["providers"][provider_id]["models"] = json!(serialized);
    if !api_key.is_empty() {
        models_json["providers"][provider_id]["scannedApiKey"] = json!(api_key);
    }
    write_json_atomic(&models_path, &models_json, None)
}

/// Simpan patch override properti model tertentu ke `models.json`.
pub(crate) fn save_model_override(
    config_dir: &Path,
    provider_id: &str,
    model_id: &str,
    override_patch: &Value,
) -> std::io::Result<()> {
    let Some(patch_obj) = override_patch.as_object() else {
        return Ok(());
    };
    let models_path = config_dir.join("models.json");
    let mut models_json = read_json_file(&models_path);
    if !models_json.is_object() {
        models_json = json!({ "providers": {} });
    }
    if !models_json["providers"].is_object() {
        models_json["providers"] = json!({});
    }
    if !models_json["providers"][provider_id].is_object() {
        models_json["providers"][provider_id] = json!({});
    }
    if !models_json["providers"][provider_id]["modelOverrides"].is_object() {
        models_json["providers"][provider_id]["modelOverrides"] = json!({});
    }
    if !models_json["providers"][provider_id]["modelOverrides"][model_id].is_object() {
        models_json["providers"][provider_id]["modelOverrides"][model_id] = json!({});
    }
    for (k, v) in patch_obj {
        models_json["providers"][provider_id]["modelOverrides"][model_id][k] = v.clone();
    }
    write_json_atomic(&models_path, &models_json, None)
}

/// Baca pasangan `KEY=VALUE` dari `~/.config/oopbot/.env`.
pub(crate) fn read_user_env_map() -> HashMap<String, String> {
    let mut map = HashMap::new();
    let Ok(home) = std::env::var("HOME") else {
        return map;
    };
    let Ok(content) = std::fs::read_to_string(PathBuf::from(home).join(".config/oopbot/.env"))
    else {
        return map;
    };
    for line in content.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() || trimmed.starts_with('#') {
            continue;
        }
        if let Some((k, v)) = trimmed.split_once('=') {
            let key = k.trim().to_string();
            let val = v.trim().trim_matches('"').trim_matches('\'').to_string();
            if !val.is_empty() {
                map.insert(key, val);
            }
        }
    }
    map
}

/// Simpan kredensial CLI/REPL aktif ke `~/.config/oopbot/.env` secara atomik dengan mode `0o600`.
pub(crate) fn save_token_to_user_env(api_key: &str, base_url: Option<&str>, model: &str) {
    let Ok(home) = std::env::var("HOME") else {
        return;
    };
    let env_path = PathBuf::from(home).join(".config/oopbot/.env");
    let mut content = String::new();
    let key = api_key.trim();
    let url_lower = base_url.unwrap_or("").to_lowercase();

    if key.starts_with("sk-or-") || url_lower.contains("openrouter.ai") {
        content.push_str(&format!("OPENROUTER_API_KEY={}\n", key));
        if let Some(url) = base_url {
            content.push_str(&format!("OPENROUTER_BASE_URL={}\n", url));
        }
        content.push_str(&format!("OOPBOT_MODEL={}\n", model));
    } else if url_lower.contains("deepseek.com") {
        content.push_str(&format!("DEEPSEEK_API_KEY={}\n", key));
        if let Some(url) = base_url {
            content.push_str(&format!("DEEPSEEK_BASE_URL={}\n", url));
        }
        content.push_str(&format!("OOPBOT_MODEL={}\n", model));
    } else if url_lower.contains("anthropic.com") || key.starts_with("sk-ant-") {
        content.push_str(&format!("ANTHROPIC_API_KEY={}\n", key));
        if let Some(url) = base_url {
            content.push_str(&format!("ANTHROPIC_BASE_URL={}\n", url));
        }
        content.push_str(&format!("OOPBOT_MODEL={}\n", model));
    } else if url_lower.contains("openai.com") {
        content.push_str(&format!("OPENAI_API_KEY={}\n", key));
        if let Some(url) = base_url {
            content.push_str(&format!("OPENAI_BASE_URL={}\n", url));
        }
        content.push_str(&format!("OOPBOT_MODEL={}\n", model));
    } else if url_lower.contains("googleapis.com") || key.starts_with("AIzaSy") {
        content.push_str(&format!("GEMINI_API_KEY={}\nGEMINI_MODEL={}\n", key, model));
        if let Some(url) = base_url {
            content.push_str(&format!("GEMINI_BASE_URL={}\n", url));
        }
    } else {
        content.push_str(&format!("OOPBOT_API_KEY={}\n", key));
        if let Some(url) = base_url {
            content.push_str(&format!("OOPBOT_BASE_URL={}\n", url));
        }
        content.push_str(&format!("OOPBOT_MODEL={}\n", model));
    }
    let _ = write_text_atomic(&env_path, &content, Some(0o600));
}
