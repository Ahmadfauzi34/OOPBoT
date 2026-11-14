use super::{save_auth_key, write_json_file};
use crate::models::{
    builtin_providers, extract_auth_credentials, parse_models_response, resolve_model_scope,
    ModelCatalog, ModelInfo,
};
use crate::util::read_json_file;
use serde_json::{json, Value};
use std::collections::HashSet;
use std::env;
use std::path::Path;

pub(super) fn read_json_object_with_error(path: &Path) -> (Value, Option<String>) {
    match std::fs::read_to_string(path) {
        Ok(content) => {
            if content.trim().is_empty() {
                return (json!({}), None);
            }
            match serde_json::from_str::<Value>(&content) {
                Ok(val) if val.is_object() => (val, None),
                Ok(_) => (
                    json!({}),
                    Some(format!("{} must contain a JSON object", path.file_name().unwrap_or_default().to_string_lossy())),
                ),
                Err(err) => (json!({}), Some(format!("Invalid JSON: {}", err))),
            }
        }
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => (json!({}), None),
        Err(err) => (json!({}), Some(err.to_string())),
    }
}

/// Menghasilkan skema `AuthStatus` lengkap sesuai kebutuhan UI Android (`HostModels.kt:140-182`)
pub(super) fn handle_auth_status(config_dir: &Path) -> Value {
    let auth_path = config_dir.join("auth.json");
    let auth_json = read_json_file(&auth_path);

    let models_path = config_dir.join("models.json");
    let (models_json, models_error) = read_json_object_with_error(&models_path);

    let settings_path = config_dir.join("settings.json");
    let (settings_json, settings_error) = read_json_object_with_error(&settings_path);

    let catalog = ModelCatalog::builtin();
    let ref_providers = builtin_providers();

    // 6 built-in providers bawaan Android
    let builtin_specs = [
        ("google", "Google Gemini", "GEMINI_API_KEY", "GOOGLE_API_KEY"),
        ("anthropic", "Anthropic Claude", "ANTHROPIC_API_KEY", ""),
        ("openai-codex", "OpenAI", "OPENAI_API_KEY", ""),
        ("openrouter", "OpenRouter", "OPENROUTER_API_KEY", ""),
        ("zai", "Z.ai Coding Plan", "ZAI_API_KEY", ""),
        ("kimi-coding", "Kimi Code", "KIMI_API_KEY", ""),
    ];

    // Bangun universe semua model yang dikenal (builtin + custom/extra dari models.json) untuk scope resolution
    let mut universe: Vec<ModelInfo> = Vec::new();
    for bp in &ref_providers {
        for m in &bp.models {
            let mut item = m.clone();
            item.provider = bp.id.clone();
            universe.push(item);
        }
    }
    if let Some(providers_obj) = models_json.get("providers").and_then(|p| p.as_object()) {
        for (pid, pval) in providers_obj {
            let api = pval["api"].as_str().unwrap_or("openai-completions");
            if let Some(arr) = pval.get("models").and_then(|v| v.as_array()) {
                for m_item in arr {
                    if let Some(info) = ModelInfo::from_pipkin_json(m_item, pid, api) {
                        if !universe.iter().any(|u| u.provider == *pid && u.id == info.id) {
                            universe.push(info);
                        }
                    }
                }
            }
        }
    }

    // `null` scope: settings.json tidak punya enabledModels (atau array kosong / invalid JSON), sehingga semua model tampil.
    let scope_refs: Option<Vec<String>> = if settings_error.is_none() {
        settings_json
            .get("enabledModels")
            .and_then(|v| v.as_array())
            .filter(|arr| !arr.is_empty())
            .map(|arr| {
                let patterns: Vec<String> = arr
                    .iter()
                    .filter_map(|v| v.as_str().map(str::to_string))
                    .collect();
                resolve_model_scope(&patterns, &universe)
                    .into_iter()
                    .map(|(m, _)| format!("{}/{}", m.provider, m.id))
                    .collect()
            })
    } else {
        None
    };

    let mut builtin = Vec::new();
    for (id, name, env_key1, env_key2) in builtin_specs {
        let auth_cred = extract_auth_credentials(&auth_json, id);
        let has_env = (!env_key1.is_empty()
            && env::var(env_key1)
                .map(|v| !v.trim().is_empty())
                .unwrap_or(false))
            || (!env_key2.is_empty()
                && env::var(env_key2)
                    .map(|v| !v.trim().is_empty())
                    .unwrap_or(false));

        let (is_configured, auth_type) = if let Some((_tok, kind)) = auth_cred {
            (true, json!(kind))
        } else if has_env {
            (true, json!("api_key"))
        } else {
            (false, Value::Null)
        };

        let provider_filter = match id {
            "openai-codex" => "openai",
            "kimi-coding" => "kimi",
            other => other,
        };

        let overrides = models_json
            .get("providers")
            .and_then(|p| p.get(id))
            .and_then(|p| p.get("modelOverrides"));
        let mut models_for_provider: Vec<Value> = catalog
            .list()
            .iter()
            .filter(|m| m.provider == provider_filter || m.provider == id)
            .map(|m| {
                let mut cloned = m.clone();
                if let Some(ov) = overrides.and_then(|o| o.get(&cloned.id)) {
                    cloned.apply_override(ov);
                }
                cloned.to_pipkin_json(Some(id))
            })
            .collect();

        // Extra IDs + extra models dari models.json
        let mut extra_ids = Vec::new();
        if let Some(entry) = models_json.get("providers").and_then(|p| p.get(id)) {
            let entry_api = entry["api"].as_str().unwrap_or("openai-completions");
            if let Some(m_arr) = entry.get("models").and_then(|m| m.as_array()) {
                for m in m_arr {
                    if let Some(mid) = m.get("id").and_then(|i| i.as_str()) {
                        extra_ids.push(mid.to_string());
                        if !models_for_provider
                            .iter()
                            .any(|existing| existing["id"].as_str() == Some(mid))
                        {
                            if let Some(info) = ModelInfo::from_pipkin_json(m, id, entry_api) {
                                models_for_provider.push(info.to_pipkin_json(Some(id)));
                            }
                        }
                    }
                }
            }
        }

        // Enabled models hanya di-populate bila scope aktif (`Some(...)`); jika unbounded maka `null`.
        let enabled = scope_refs.as_ref().map(|refs| {
            refs.iter()
                .filter_map(|ref_str| {
                    ref_str
                        .split_once('/')
                        .and_then(|(p, m)| if p == id { Some(m.to_string()) } else { None })
                })
                .collect::<Vec<String>>()
        });

        builtin.push(json!({
            "id": id,
            "name": name,
            "configured": is_configured,
            "type": auth_type,
            "models": models_for_provider,
            "extraIds": extra_ids,
            "enabled": enabled,
        }));
    }

    // Custom providers dari models.json
    let mut custom = Vec::new();
    let builtin_id_set: HashSet<&str> = [
        "google",
        "anthropic",
        "openai",
        "openai-codex",
        "openrouter",
        "zai",
        "kimi",
        "kimi-coding",
    ]
    .into_iter()
    .collect();

    if let Some(providers_obj) = models_json.get("providers").and_then(|p| p.as_object()) {
        for (id, val) in providers_obj {
            if builtin_id_set.contains(id.as_str()) {
                continue;
            }
            let base_url = val["baseUrl"].as_str().unwrap_or_default();
            let api = val["api"].as_str().unwrap_or("openai-completions");
            let raw_key = val["apiKey"].as_str().unwrap_or_default();
            let api_key = if raw_key == "no-api-key" { "" } else { raw_key };
            let models = val["models"].as_array().cloned().unwrap_or_default();
            custom.push(json!({
                "id": id,
                "baseUrl": base_url,
                "api": api,
                "apiKey": api_key,
                "models": models,
            }));
        }
    }

    let npm_packages: Vec<String> = settings_json
        .get("packages")
        .and_then(|v| v.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|p| {
                    p.as_str()
                        .map(str::to_string)
                        .or_else(|| p.get("source").and_then(|s| s.as_str()).map(str::to_string))
                })
                .collect()
        })
        .unwrap_or_default();

    let mut data = json!({
        "builtin": builtin,
        "custom": custom,
        "scope": scope_refs,
        "npmPackages": npm_packages,
    });
    if let Some(err) = models_error {
        data["modelsError"] = json!(err);
    }
    if let Some(err) = settings_error {
        data["settingsError"] = json!(err);
    }

    json!({
        "type": "pipkin_result",
        "ok": true,
        "data": data
    })
}

pub(super) fn handle_logout(config_dir: &Path, params: &Value) -> Value {
    let provider_id = params["providerId"].as_str().unwrap_or_default();
    if !provider_id.is_empty() {
        let auth_path = config_dir.join("auth.json");
        let mut auth_json = read_json_file(&auth_path);
        if let Some(obj) = auth_json.as_object_mut() {
            obj.remove(provider_id);
            let _ = write_json_file(&auth_path, &auth_json);
        }
    }
    json!({
        "type": "pipkin_result",
        "ok": true,
        "data": {}
    })
}

pub(super) fn handle_provider_save(config_dir: &Path, params: &Value) -> Value {
    let id = params["id"].as_str().unwrap_or_default();
    if !id.is_empty() {
        let models_path = config_dir.join("models.json");
        let mut models_json = read_json_file(&models_path);
        if !models_json.is_object() {
            models_json = json!({ "providers": {} });
        }
        if !models_json["providers"].is_object() {
            models_json["providers"] = json!({});
        }

        let base_url = params["baseUrl"].as_str().unwrap_or_default();
        let api = params["api"].as_str().unwrap_or("openai-completions");
        let api_key = params["apiKey"].as_str().unwrap_or_default();
        let models = params.get("models").cloned().unwrap_or(json!([]));

        models_json["providers"][id] = json!({
            "baseUrl": base_url,
            "api": api,
            "apiKey": api_key,
            "models": models,
        });
        let _ = write_json_file(&models_path, &models_json);

        if !api_key.is_empty() {
            save_auth_key(config_dir, id, api_key);
        }
    }
    json!({
        "type": "pipkin_result",
        "ok": true,
        "data": {}
    })
}

pub(super) fn handle_provider_remove(config_dir: &Path, params: &Value) -> Value {
    let id = params["id"].as_str().unwrap_or_default();
    if !id.is_empty() {
        let models_path = config_dir.join("models.json");
        let mut models_json = read_json_file(&models_path);
        if let Some(providers) = models_json.get_mut("providers").and_then(|p| p.as_object_mut()) {
            providers.remove(id);
            let _ = write_json_file(&models_path, &models_json);
        }
        let auth_path = config_dir.join("auth.json");
        let mut auth_json = read_json_file(&auth_path);
        if let Some(obj) = auth_json.as_object_mut() {
            obj.remove(id);
            let _ = write_json_file(&auth_path, &auth_json);
        }
    }
    json!({
        "type": "pipkin_result",
        "ok": true,
        "data": {}
    })
}

pub(super) fn handle_subscription_models(params: &Value) -> Value {
    let provider_id = params["providerId"].as_str().unwrap_or_default();
    let catalog = ModelCatalog::builtin();
    let filter = match provider_id {
        "openai-codex" => "openai",
        "kimi-coding" => "kimi",
        other => other,
    };
    let models_json: Vec<Value> = catalog
        .list()
        .iter()
        .filter(|m| m.provider == filter || m.provider == provider_id)
        .map(|m| m.to_pipkin_json(Some(provider_id)))
        .collect();

    json!({
        "type": "pipkin_result",
        "ok": true,
        "data": { "models": models_json }
    })
}

pub(super) fn handle_subscription_save(config_dir: &Path, params: &Value) -> Value {
    let provider_id = params["providerId"].as_str().unwrap_or_default();
    if !provider_id.is_empty() {
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
        if let Some(models) = params.get("models") {
            models_json["providers"][provider_id]["models"] = models.clone();
        }
        let _ = write_json_file(&models_path, &models_json);

        if let Some(enabled) = params.get("enabled").and_then(|e| e.as_array()) {
            let settings_path = config_dir.join("settings.json");
            let mut settings = read_json_file(&settings_path);
            if !settings.is_object() {
                settings = json!({});
            }
            let mut enabled_models = settings
                .get("enabledModels")
                .and_then(|v| v.as_array())
                .cloned()
                .unwrap_or_default();

            // Hapus model lama untuk provider ini
            enabled_models.retain(|item| {
                if let Some(s) = item.as_str() {
                    !s.starts_with(&format!("{}/", provider_id))
                } else {
                    true
                }
            });

            // Tambahkan model baru
            for m in enabled {
                if let Some(m_str) = m.as_str() {
                    enabled_models.push(json!(format!("{}/{}", provider_id, m_str)));
                }
            }
            settings["enabledModels"] = json!(enabled_models);
            let _ = write_json_file(&settings_path, &settings);
        }
    }
    json!({
        "type": "pipkin_result",
        "ok": true,
        "data": {}
    })
}

pub(super) fn handle_local_models_sync(config_dir: &Path, params: &Value) -> Value {
    const LOCAL_PROVIDER: &str = "pipkin-local";
    const LEGACY_LOCAL_PROVIDER: &str = "oopbot-local";
    const PI_AI_CONTEXT_MARGIN: u64 = 4096;
    const KEPT_SHARE: u64 = 16;

    let base_url = params["baseUrl"].as_str().unwrap_or_default();
    let api_key = params["apiKey"].as_str().unwrap_or_default();
    let raw_models = params
        .get("models")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default();

    let models_path = config_dir.join("models.json");
    let mut models_json = read_json_file(&models_path);
    if !models_json.is_object() {
        models_json = json!({ "providers": {} });
    }
    if !models_json["providers"].is_object() {
        models_json["providers"] = json!({});
    }
    let before_models = models_json.clone();

    let settings_path = config_dir.join("settings.json");
    let (mut settings_json, settings_err) = read_json_object_with_error(&settings_path);
    let before_settings = settings_json.clone();

    let mut model_ids: Vec<String> = Vec::new();
    let mut normalized_models: Vec<Value> = Vec::new();
    for m in &raw_models {
        if let Some(id) = m.get("id").and_then(|v| v.as_str()).filter(|s| !s.is_empty()) {
            model_ids.push(id.to_string());
            let mut obj = m.as_object().cloned().unwrap_or_default();
            obj.insert("id".to_string(), json!(id));
            obj.insert("input".to_string(), json!(["text"]));
            obj.insert(
                "cost".to_string(),
                json!({ "input": 0, "output": 0, "cacheRead": 0, "cacheWrite": 0 }),
            );
            normalized_models.push(Value::Object(obj));
        }
    }

    if let Some(providers_obj) = models_json
        .get_mut("providers")
        .and_then(|v| v.as_object_mut())
    {
        providers_obj.remove(LEGACY_LOCAL_PROVIDER);
        if normalized_models.is_empty() {
            providers_obj.remove(LOCAL_PROVIDER);
        } else {
            providers_obj.insert(
                LOCAL_PROVIDER.to_string(),
                json!({
                    "baseUrl": base_url,
                    "api": "openai-completions",
                    "apiKey": api_key,
                    "compat": {
                        "sendSessionAffinityHeaders": true,
                        "sessionAffinityFormat": "openai-nosession",
                        "supportsDeveloperRole": false,
                        "supportsStore": false,
                        "supportsReasoningEffort": true,
                        "supportsUsageInStreaming": true,
                        "maxTokensField": "max_tokens",
                        "thinkingFormat": "openai"
                    },
                    "models": normalized_models,
                }),
            );
        }
    }

    let models_changed = models_json != before_models;
    if models_changed {
        if let Err(e) = write_json_file(&models_path, &models_json) {
            return json!({
                "type": "pipkin_result",
                "ok": false,
                "code": "io_error",
                "error": format!("Failed to write models.json: {}", e)
            });
        }
    }

    let mut settings_changed = false;
    if settings_err.is_none() {
        // 1. Update enabledModels scope jika sudah aktif (non-empty)
        if let Some(enabled_arr) = settings_json
            .get("enabledModels")
            .and_then(|v| v.as_array())
            .filter(|a| !a.is_empty())
            .cloned()
        {
            let mut next_enabled: Vec<Value> = enabled_arr
                .into_iter()
                .filter(|item| {
                    item.as_str()
                        .map(|s| {
                            !s.starts_with(&format!("{}/", LEGACY_LOCAL_PROVIDER))
                                && !s.starts_with(&format!("{}/", LOCAL_PROVIDER))
                        })
                        .unwrap_or(true)
                })
                .collect();
            for mid in &model_ids {
                let ref_str = format!("{}/{}", LOCAL_PROVIDER, mid);
                if !next_enabled
                    .iter()
                    .any(|item| item.as_str() == Some(ref_str.as_str()))
                {
                    next_enabled.push(json!(ref_str));
                }
            }
            settings_json["enabledModels"] = json!(next_enabled);
        }

        // 2. Update compaction.modelOverrides untuk tiap model lokal yang punya contextWindow & maxTokens
        let mut overrides = settings_json
            .get("compaction")
            .and_then(|c| c.get("modelOverrides"))
            .and_then(|o| o.as_object())
            .cloned()
            .unwrap_or_default();
        overrides.retain(|k, _| {
            !k.starts_with(&format!("{}/", LOCAL_PROVIDER))
                && !k.starts_with(&format!("{}/", LEGACY_LOCAL_PROVIDER))
        });
        for m in &raw_models {
            let Some(id) = m.get("id").and_then(|v| v.as_str()).filter(|s| !s.is_empty()) else {
                continue;
            };
            if let (Some(ctx), Some(max_tok)) = (
                m.get("contextWindow").and_then(|v| v.as_u64()),
                m.get("maxTokens").and_then(|v| v.as_u64()),
            ) {
                overrides.insert(
                    format!("{}/{}", LOCAL_PROVIDER, id),
                    json!({
                        "reserveTokens": max_tok + PI_AI_CONTEXT_MARGIN,
                        "keepRecentTokens": ctx / KEPT_SHARE,
                    }),
                );
            }
        }
        if overrides.is_empty() {
            if let Some(comp) = settings_json
                .get_mut("compaction")
                .and_then(|v| v.as_object_mut())
            {
                comp.remove("modelOverrides");
            }
        } else {
            if !settings_json["compaction"].is_object() {
                settings_json["compaction"] = json!({});
            }
            settings_json["compaction"]["modelOverrides"] = Value::Object(overrides);
        }

        settings_changed = settings_json != before_settings;
        if settings_changed {
            if let Err(e) = write_json_file(&settings_path, &settings_json) {
                return json!({
                    "type": "pipkin_result",
                    "ok": false,
                    "code": "io_error",
                    "error": format!("Failed to write settings.json: {}", e)
                });
            }
        }
    }

    json!({
        "type": "pipkin_result",
        "ok": true,
        "data": { "changed": models_changed || settings_changed }
    })
}

pub(super) fn http_status_error_code(status: u16) -> &'static str {
    match status {
        401 | 403 => "unauthorized",
        404 => "not_found",
        429 => "rate_limited",
        500..=599 => "server_error",
        _ => "http_error",
    }
}

pub(super) fn is_html_body(body: &str) -> bool {
    let trimmed = body.trim_start().to_ascii_lowercase();
    trimmed.starts_with("<!doctype")
        || trimmed.starts_with("<html")
        || trimmed.starts_with("<head")
        || trimmed.starts_with("<body")
}

pub(super) fn extract_server_error_message(body: &str) -> String {
    if is_html_body(body) {
        return String::new();
    }
    if let Ok(parsed) = serde_json::from_str::<Value>(body) {
        let candidates = [
            parsed["error"]["message"].as_str(),
            parsed["error"].as_str(),
            parsed["message"].as_str(),
            parsed["detail"].as_str(),
        ];
        if let Some(msg) = candidates
            .into_iter()
            .flatten()
            .map(str::trim)
            .find(|s| !s.is_empty())
        {
            return msg.chars().take(300).collect();
        }
    }
    body.trim().chars().take(300).collect()
}

pub(super) fn is_valid_http_url(raw: &str) -> bool {
    let lower = raw.to_ascii_lowercase();
    if let Some(rest) = lower.strip_prefix("http://").or_else(|| lower.strip_prefix("https://")) {
        let host = rest.split('/').next().unwrap_or("");
        !host.is_empty() && !host.contains(char::is_whitespace)
    } else {
        false
    }
}

pub(super) async fn handle_provider_models(params: &Value) -> Value {
    let base_url = params["baseUrl"].as_str().unwrap_or("").trim();
    let api_key = params["apiKey"].as_str().unwrap_or("").trim();
    let api = params["api"].as_str().unwrap_or("openai-completions").trim();

    if !is_valid_http_url(base_url) {
        return json!({
            "type": "pipkin_result",
            "ok": false,
            "code": "bad_url",
            "error": format!("{} is not an http:// or https:// URL", base_url),
            "data": {}
        });
    }

    let client = match reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(15))
        .build()
    {
        Ok(c) => c,
        Err(e) => {
            return json!({
                "type": "pipkin_result",
                "ok": false,
                "code": "network",
                "error": format!("Could not reach {}: {}", base_url, e),
                "data": {}
            });
        }
    };

    let clean_base = base_url.trim_end_matches('/');
    let is_google = api == "google-generative-ai"
        || (clean_base.contains("googleapis.com") && !clean_base.contains("/openai"));
    let is_anthropic = api == "anthropic-messages" || clean_base.contains("anthropic.com");

    let url = if is_google {
        format!("{}/models?pageSize=100", clean_base)
    } else {
        format!("{}/models", clean_base)
    };

    let mut req = client.get(&url);
    if is_google {
        if !api_key.is_empty() {
            req = req.header("x-goog-api-key", api_key);
        }
    } else if is_anthropic {
        if !api_key.is_empty() {
            req = req
                .header("x-api-key", api_key)
                .header("anthropic-version", "2023-06-01");
        }
    } else {
        let bearer = if api_key.is_empty() {
            "no-api-key"
        } else {
            api_key
        };
        req = req.header("Authorization", format!("Bearer {}", bearer));
    }

    let resp = match req.send().await {
        Ok(r) => r,
        Err(e) => {
            if e.is_timeout() {
                return json!({
                    "type": "pipkin_result",
                    "ok": false,
                    "code": "timeout",
                    "error": format!("No response from {} within 15 s", base_url),
                    "data": {}
                });
            }
            let cause = e.to_string();
            return json!({
                "type": "pipkin_result",
                "ok": false,
                "code": "network",
                "error": format!("Could not reach {}: {}", base_url, cause),
                "i18n": {
                    "key": "provider_unreachable_because",
                    "args": { "url": base_url, "cause": cause }
                },
                "data": {}
            });
        }
    };

    let status = resp.status();
    let body_text = match resp.text().await {
        Ok(t) => t,
        Err(e) => {
            if e.is_timeout() {
                return json!({
                    "type": "pipkin_result",
                    "ok": false,
                    "code": "timeout",
                    "error": format!("No response from {} within 15 s", base_url),
                    "data": {}
                });
            }
            return json!({
                "type": "pipkin_result",
                "ok": false,
                "code": "network",
                "error": format!("Could not reach {}: {}", base_url, e),
                "data": {}
            });
        }
    };

    if !status.is_success() {
        let code = http_status_error_code(status.as_u16());
        let said = extract_server_error_message(&body_text);
        if said.is_empty() {
            return json!({
                "type": "pipkin_result",
                "ok": false,
                "code": code,
                "error": format!("HTTP {}", status.as_u16()),
                "i18n": { "key": "provider_http_status", "args": { "status": status.as_u16() } },
                "data": {}
            });
        }
        return json!({
            "type": "pipkin_result",
            "ok": false,
            "code": code,
            "error": format!("HTTP {}: {}", status.as_u16(), said),
            "i18n": {
                "key": "provider_http_status_said",
                "args": { "status": status.as_u16(), "message": said }
            },
            "data": {}
        });
    }

    let body_json: Value = match serde_json::from_str(&body_text) {
        Ok(v) => v,
        Err(_) => {
            if is_html_body(&body_text) {
                return json!({
                    "type": "pipkin_result",
                    "ok": false,
                    "code": "bad_response",
                    "error": format!("{} returned a web page, not JSON", url),
                    "i18n": { "key": "provider_web_page", "args": { "url": url } },
                    "data": {}
                });
            }
            let preview: String = body_text.chars().take(300).collect();
            return json!({
                "type": "pipkin_result",
                "ok": false,
                "code": "bad_response",
                "error": format!("The server did not return JSON: {}", preview),
                "i18n": { "key": "provider_not_json", "args": { "body": preview } },
                "data": {}
            });
        }
    };

    let has_openai_null_list =
        body_json.get("object").and_then(|v| v.as_str()) == Some("list")
            && body_json.get("data").is_some_and(|v| v.is_null());
    let has_valid_list = has_openai_null_list
        || body_json.get("data").and_then(|v| v.as_array()).is_some()
        || body_json.get("models").and_then(|v| v.as_array()).is_some()
        || body_json.is_array();

    if !has_valid_list {
        return json!({
            "type": "pipkin_result",
            "ok": false,
            "code": "bad_response",
            "error": "The server's /models response has no data list",
            "i18n": { "key": "provider_no_data_list" },
            "data": {}
        });
    }

    let provider_hint = if is_google {
        "google"
    } else if is_anthropic {
        "anthropic"
    } else if clean_base.contains("openrouter.ai") {
        "openrouter"
    } else {
        "custom"
    };
    let models = parse_models_response(&body_json, provider_hint);
    let models_json: Vec<Value> = models
        .iter()
        .map(|m| {
            let mut item = m.to_pipkin_json(None);
            item["api"] = json!(api);
            item
        })
        .collect();

    json!({
        "type": "pipkin_result",
        "ok": true,
        "data": { "models": models_json }
    })
}

pub(super) fn handle_model_defaults(params: &Value) -> Value {
    let catalog = ModelCatalog::builtin();
    let mut models_json = Vec::new();
    if let Some(ids) = params["ids"].as_array() {
        for id_val in ids {
            if let Some(id_str) = id_val.as_str() {
                if let Some(m) = catalog.find(id_str) {
                    models_json.push(m.to_pipkin_json(None));
                } else {
                    let api = params["api"].as_str().unwrap_or("openai-completions");
                    models_json.push(json!({
                        "id": id_str,
                        "name": id_str,
                        "api": api,
                    }));
                }
            }
        }
    }
    json!({
        "type": "pipkin_result",
        "ok": true,
        "data": { "models": models_json }
    })
}

pub(super) fn handle_save_default_model(config_dir: &Path, params: &Value) -> Value {
    let settings_path = config_dir.join("settings.json");
    let mut settings = read_json_file(&settings_path);
    if !settings.is_object() {
        settings = json!({});
    }

    if let Some(provider) = params["provider"].as_str() {
        settings["defaultProvider"] = json!(provider);
    }
    if let Some(model_id) = params["modelId"].as_str() {
        settings["defaultModel"] = json!(model_id);
    }
    if let Some(lvl) = params["thinkingLevel"].as_str() {
        settings["defaultThinkingLevel"] = json!(lvl);
    }

    let _ = write_json_file(&settings_path, &settings);

    json!({
        "type": "pipkin_result",
        "ok": true,
        "data": {}
    })
}

