use super::{RpcMutableState, RpcRuntime};
use crate::gemini::DEFAULT_GEMINI_BASE_URL;
use crate::models::{
    builtin_providers, infer_api_from_provider, infer_input_from_id, infer_reasoning_from_id,
    resolve_model_scope, ModelCatalog, ModelInfo,
};
use crate::util::read_json_file;
use serde_json::{json, Value};
use std::collections::HashMap;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone)]
pub(super) struct ProviderEndpoint {
    pub(super) name: String,
    pub(super) base_url: String,
    pub(super) api: String,
    pub(super) api_key: String,
    pub(super) has_auth: bool,
    pub(super) models: Vec<ModelInfo>,
}


impl RpcRuntime {
    pub(super) fn model_info_from_state(m: &RpcMutableState) -> ModelInfo {
        if let Some(ep) = m.providers.get(&m.active_provider) {
            if let Some(info) = ep
                .models
                .iter()
                .find(|info| info.id.eq_ignore_ascii_case(&m.active_model))
            {
                let mut cloned = info.clone();
                cloned.provider = m.active_provider.clone();
                return cloned;
            }
        }
        for (pid, ep) in &m.providers {
            if let Some(info) = ep
                .models
                .iter()
                .find(|info| info.id.eq_ignore_ascii_case(&m.active_model))
            {
                let mut cloned = info.clone();
                cloned.provider = pid.clone();
                return cloned;
            }
        }
        if let Some(info) = m.catalog.find(&m.active_model) {
            let mut cloned = info.clone();
            cloned.provider = m.active_provider.clone();
            return cloned;
        }
        let default_api = m
            .providers
            .get(&m.active_provider)
            .map(|p| p.api.clone())
            .unwrap_or_else(|| infer_api_from_provider(&m.active_provider));
        ModelInfo {
            id: m.active_model.clone(),
            label: m.active_model.clone(),
            provider: m.active_provider.clone(),
            supports_tools: true,
            input_token_limit: Some(1_048_576),
            output_token_limit: Some(65_536),
            description: "Active model".to_string(),
            api: default_api,
            reasoning: infer_reasoning_from_id(&m.active_model),
            thinking_level_map: None,
            input: infer_input_from_id(&m.active_model),
            cost: None,
        }
    }

    /// Cari `ModelInfo` lengkap untuk model aktif saat ini beserta seluruh override dari `models.json`.
    pub fn active_model_info(&self) -> ModelInfo {
        let m = self.mutable.lock().unwrap();
        Self::model_info_from_state(&m)
    }

    pub(super) fn sync_agent_catalog(&self) {
        let models = {
            let m = self.mutable.lock().unwrap();
            m.catalog.list().to_vec()
        };
        for model in models {
            self.agent.upsert_model(model);
        }
    }

    pub(super) fn sync_agent_active_model(&self) {
        let (info, model_id) = {
            let m = self.mutable.lock().unwrap();
            (Self::model_info_from_state(&m), m.active_model.clone())
        };
        self.agent.upsert_model(info);
        self.agent.set_active_model(&model_id);
    }


    pub(super) fn current_model_json(&self) -> Value {
        let m = self.mutable.lock().unwrap();
        Self::model_info_from_state(&m).to_pipkin_json(Some(&m.active_provider))
    }

    pub(super) fn available_models_json(&self) -> Vec<Value> {
        let (providers, active_provider, catalog_list) = {
            let m = self.mutable.lock().unwrap();
            (
                m.providers.clone(),
                m.active_provider.clone(),
                m.catalog.list().to_vec(),
            )
        };
        let mut all_models: Vec<ModelInfo> = Vec::new();
        let any_authenticated = providers.values().any(|ep| ep.has_auth);

        let mut provider_ids: Vec<String> = providers.keys().cloned().collect();
        provider_ids.sort_by(|a, b| {
            let a_builtin = a == "google";
            let b_builtin = b == "google";
            b_builtin.cmp(&a_builtin).then_with(|| a.cmp(b))
        });

        for pid in provider_ids {
            let Some(ep) = providers.get(&pid) else {
                continue;
            };
            if any_authenticated && !ep.has_auth && pid != active_provider {
                continue;
            }
            if !any_authenticated && pid != "google" && pid != active_provider {
                continue;
            }
            for model in &ep.models {
                let mut item = model.clone();
                item.provider = pid.clone();
                all_models.push(item);
            }
        }

        if all_models.is_empty() {
            for model in catalog_list {
                let mut item = model;
                item.provider = "google".to_string();
                all_models.push(item);
            }
        }

        let settings = read_json_file(&self.config_dir.join("settings.json"));
        if let Some(patterns_arr) = settings["enabledModels"].as_array() {
            let patterns: Vec<String> = patterns_arr
                .iter()
                .filter_map(|v| v.as_str().map(str::to_string))
                .collect();
            if !patterns.is_empty() {
                let scoped = resolve_model_scope(&patterns, &all_models);
                return scoped
                    .into_iter()
                    .map(|(m, _)| m.to_pipkin_json(Some(&m.provider)))
                    .collect();
            }
        }

        all_models
            .iter()
            .map(|m| m.to_pipkin_json(Some(&m.provider)))
            .collect()
    }


    pub(super) fn refresh_credentials(&self) {
        let (catalog_clone, active_provider) = {
            let m = self.mutable.lock().unwrap();
            (m.catalog.clone(), m.active_provider.clone())
        };
        let new_providers = load_providers(&self.config_dir, &catalog_clone);
        let ep_opt = new_providers.get(&active_provider).cloned();
        {
            let mut m = self.mutable.lock().unwrap();
            m.providers = new_providers;
        }
        if let Some(ep) = ep_opt {
            self.agent
                .update_credentials(ep.api_key, Some(ep.base_url));
        }
    }

    pub(super) fn sync_thinking_level_for_active_model(&self) {
        let clamped_change = {
            let mut m = self.mutable.lock().unwrap();
            let clamped = Self::model_info_from_state(&m).clamp_thinking_level(&m.thinking_level);
            let changed = clamped != m.thinking_level;
            m.thinking_level = clamped.clone();
            (clamped, changed)
        };
        if clamped_change.1 {
            Self::send_line(&json!({
                "type": "thinking_level_changed",
                "level": clamped_change.0,
            }));
        }
        self.agent.set_thinking_level(&clamped_change.0);
    }

    pub(super) fn needs_auto_scan(&self) -> Option<(String, ProviderEndpoint)> {
        let m = self.mutable.lock().unwrap();
        let active_provider = m.active_provider.clone();
        let ep = m.providers.get(&active_provider)?.clone();
        if !ep.has_auth || ep.api_key.trim().is_empty() {
            return None;
        }
        let models_path = self.config_dir.join("models.json");
        let models_json = read_json_file(&models_path);
        let prov_entry = &models_json["providers"][&active_provider];
        let has_saved_models = prov_entry["models"]
            .as_array()
            .map(|a| !a.is_empty())
            .unwrap_or(false);
        let scanned_key = prov_entry["scannedApiKey"].as_str().unwrap_or("");
        if has_saved_models && scanned_key == ep.api_key {
            None
        } else {
            Some((active_provider, ep))
        }
    }

    pub(super) async fn maybe_auto_scan(&self) {
        let _scan_guard = self.http_scan_lock.lock().await;
        let Some((active_provider, ep)) = self.needs_auto_scan() else {
            return;
        };

        if let Ok(scanned) = self.agent.scan_models().await {
            if !scanned.is_empty() {
                save_scanned_models_to_config(
                    &self.config_dir,
                    &active_provider,
                    &scanned,
                    &ep.api_key,
                );
                let (saved_prov, saved_model, saved_think) = {
                    let mut m = self.mutable.lock().unwrap();
                    m.catalog.replace_with_scanned(scanned.clone());
                    if !scanned
                        .iter()
                        .any(|item| item.id.eq_ignore_ascii_case(&m.active_model))
                    {
                        if let Some(pref) = scanned
                            .iter()
                            .find(|item| item.id.contains("flash") && !item.id.contains("lite"))
                        {
                            m.active_model = pref.id.clone();
                        } else if let Some(first) = scanned.first() {
                            m.active_model = first.id.clone();
                        }
                    }
                    (
                        m.active_provider.clone(),
                        m.active_model.clone(),
                        m.thinking_level.clone(),
                    )
                };
                self.sync_agent_catalog();
                self.sync_agent_active_model();
                self.refresh_credentials();
                self.sync_thinking_level_for_active_model();
                save_settings_fields(
                    &self.config_dir,
                    Some(&saved_prov),
                    Some(&saved_model),
                    Some(&saved_think),
                );
            }
        }
    }

}

pub(super) fn load_providers(config_dir: &Path, catalog: &ModelCatalog) -> HashMap<String, ProviderEndpoint> {
    let mut map = HashMap::new();
    let auth_json = read_json_file(&config_dir.join("auth.json"));
    let models_json = read_json_file(&config_dir.join("models.json"));

    // 1. Muat seluruh provider referensi bawaan dari src/models.rs
    for entry in builtin_providers() {
        let api_key = extract_auth_key(&auth_json, &entry.id);
        let has_auth = !api_key.is_empty();
        let mut models = entry.models;
        if entry.id == "google" {
            // Sinkronkan model tambahan bila catalog sudah pernah di-scan di sesi ini
            for m in catalog.list() {
                if !models.iter().any(|existing| existing.id == m.id) {
                    let mut cloned = m.clone();
                    cloned.provider = "google".to_string();
                    models.push(cloned);
                }
            }
        }
        map.insert(
            entry.id.clone(),
            ProviderEndpoint {
                name: entry.name,
                base_url: entry.base_url,
                api: entry.api,
                api_key,
                has_auth,
                models,
            },
        );
    }

    // 2. Terapkan konfigurasi aplikasi dari models.json (custom providers, models[], dan modelOverrides{})
    if let Some(providers_obj) = models_json["providers"].as_object() {
        for (id, pval) in providers_obj {
            let existing = map.get(id).cloned();
            let base_url = pval["baseUrl"]
                .as_str()
                .filter(|s| !s.trim().is_empty())
                .map(str::to_string)
                .or_else(|| existing.as_ref().map(|p| p.base_url.clone()))
                .unwrap_or_else(|| DEFAULT_GEMINI_BASE_URL.to_string());
            let api = pval["api"]
                .as_str()
                .filter(|s| !s.trim().is_empty())
                .map(str::to_string)
                .or_else(|| existing.as_ref().map(|p| p.api.clone()))
                .unwrap_or_else(|| infer_api_from_provider(id));
            let name = pval["name"]
                .as_str()
                .filter(|s| !s.trim().is_empty())
                .map(str::to_string)
                .or_else(|| existing.as_ref().map(|p| p.name.clone()))
                .unwrap_or_else(|| id.clone());

            let raw_model_key = pval["apiKey"].as_str().unwrap_or("");
            let is_keyless_local = raw_model_key == "no-api-key"
                || id == "pipkin-local"
                || id == "oopbot-local"
                || base_url.contains("127.0.0.1")
                || base_url.contains("localhost");
            let api_key = if !raw_model_key.is_empty() && raw_model_key != "no-api-key" {
                raw_model_key.to_string()
            } else {
                extract_auth_key(&auth_json, id)
            };
            let has_auth = !api_key.is_empty() || is_keyless_local;

            let overrides = pval.get("modelOverrides");

            // Jika models.json memiliki array "models" (hasil scan dari token atau konfigurasi aplikasi),
            // jadikan array tersebut sebagai sumber otoritatif tanpa mencampur model hardcode bawaan!
            let mut merged_models: Vec<ModelInfo> = Vec::new();
            if let Some(arr) = pval["models"].as_array().filter(|a| !a.is_empty()) {
                for item in arr {
                    if let Some(mut info) = ModelInfo::from_pipkin_json(item, id, &api) {
                        if let Some(ov) = overrides.and_then(|o| o.get(&info.id)) {
                            info.apply_override(ov);
                        }
                        if let Some(slot) = merged_models.iter_mut().find(|m| m.id == info.id) {
                            *slot = info;
                        } else {
                            merged_models.push(info);
                        }
                    }
                }
            } else {
                merged_models = existing
                    .map(|p| p.models)
                    .unwrap_or_default()
                    .into_iter()
                    .map(|mut m| {
                        m.provider = id.clone();
                        m.api = api.clone();
                        if let Some(ov) = overrides.and_then(|o| o.get(&m.id)) {
                            m.apply_override(ov);
                        }
                        m
                    })
                    .collect();
            }

            map.insert(
                id.clone(),
                ProviderEndpoint {
                    name,
                    base_url,
                    api,
                    api_key,
                    has_auth,
                    models: merged_models,
                },
            );
        }
    }

    // 3. Fallback ke ~/.config/oopbot/.env atau environment variables
    let env_map = read_oopbot_env_map();
    let get_env_or_file = |key: &str| -> Option<String> {
        std::env::var(key)
            .ok()
            .filter(|v| !v.trim().is_empty())
            .or_else(|| env_map.get(key).cloned())
    };

    if let Some(key) = get_env_or_file("OPENROUTER_API_KEY") {
        if let Some(p) = map.get_mut("openrouter") {
            if p.api_key.is_empty() {
                p.api_key = key;
            }
            p.has_auth = true;
        }
    }
    if let Some(key) = get_env_or_file("GEMINI_API_KEY").or_else(|| get_env_or_file("GOOGLE_API_KEY")) {
        if let Some(p) = map.get_mut("google") {
            if p.api_key.is_empty() {
                p.api_key = key;
            }
            p.has_auth = true;
        }
    }
    if let Some(key) = get_env_or_file("OPENAI_API_KEY") {
        if let Some(p) = map.get_mut("openai-codex") {
            if p.api_key.is_empty() {
                p.api_key = key;
            }
            p.has_auth = true;
        }
    }
    if let Some(key) = get_env_or_file("ANTHROPIC_API_KEY") {
        if let Some(p) = map.get_mut("anthropic") {
            if p.api_key.is_empty() {
                p.api_key = key;
            }
            p.has_auth = true;
        }
    }
    if let Some(key) = get_env_or_file("ZAI_API_KEY") {
        if let Some(p) = map.get_mut("zai") {
            if p.api_key.is_empty() {
                p.api_key = key;
            }
            p.has_auth = true;
        }
    }
    if let Some(key) = get_env_or_file("KIMI_API_KEY") {
        if let Some(p) = map.get_mut("kimi-coding") {
            if p.api_key.is_empty() {
                p.api_key = key;
            }
            p.has_auth = true;
        }
    }


    map
}

pub(super) fn extract_auth_key(auth_json: &Value, provider_id: &str) -> String {
    crate::models::extract_auth_credentials(auth_json, provider_id)
        .map(|(k, _)| k)
        .unwrap_or_default()
}

pub(super) fn save_settings_fields(
    config_dir: &Path,
    default_provider: Option<&str>,
    default_model: Option<&str>,
    default_thinking_level: Option<&str>,
) {
    let settings_path = config_dir.join("settings.json");
    let mut settings = read_json_file(&settings_path);
    if !settings.is_object() {
        settings = json!({});
    }
    if let Some(p) = default_provider.filter(|s| !s.is_empty()) {
        settings["defaultProvider"] = json!(p);
    }
    if let Some(m) = default_model.filter(|s| !s.is_empty()) {
        settings["defaultModel"] = json!(m);
    }
    if let Some(l) = default_thinking_level.filter(|s| !s.is_empty()) {
        settings["defaultThinkingLevel"] = json!(l);
    }
    if let Ok(text) = serde_json::to_string_pretty(&settings) {
        let _ = std::fs::write(&settings_path, format!("{}\n", text));
    }
}

pub(super) fn save_scanned_models_to_config(
    config_dir: &Path,
    provider_id: &str,
    scanned: &[ModelInfo],
    api_key: &str,
) {
    if scanned.is_empty() {
        return;
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
        .map(|m| m.to_pipkin_json(Some(provider_id)))
        .collect();
    models_json["providers"][provider_id]["models"] = json!(serialized);
    if !api_key.is_empty() {
        models_json["providers"][provider_id]["scannedApiKey"] = json!(api_key);
    }
    if let Ok(text) = serde_json::to_string_pretty(&models_json) {
        let _ = std::fs::write(&models_path, format!("{}\n", text));
    }
}

pub(super) fn save_model_override_to_config(
    config_dir: &Path,
    provider_id: &str,
    model_id: &str,
    override_patch: &Value,
) {
    let Some(patch_obj) = override_patch.as_object() else {
        return;
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
    if let Ok(text) = serde_json::to_string_pretty(&models_json) {
        let _ = std::fs::write(&models_path, format!("{}\n", text));
    }
}

pub(super) fn save_auth_key(config_dir: &Path, provider_id: &str, api_key: &str, base_url: Option<&str>) {
    let auth_path = config_dir.join("auth.json");
    let mut auth_json = read_json_file(&auth_path);
    if !auth_json.is_object() {
        auth_json = json!({});
    }
    auth_json[provider_id] = json!({
        "type": "api_key",
        "apiKey": api_key,
    });
    if let Ok(text) = serde_json::to_string_pretty(&auth_json) {
        let _ = std::fs::write(&auth_path, format!("{}\n", text));
    }

    if let Some(url) = base_url.filter(|u| !u.is_empty()) {
        let models_path = config_dir.join("models.json");
        let mut models_json = read_json_file(&models_path);
        if !models_json.is_object() {
            models_json = json!({ "providers": {} });
        }
        if !models_json["providers"].is_object() {
            models_json["providers"] = json!({});
        }
        models_json["providers"][provider_id]["baseUrl"] = json!(url);
        models_json["providers"][provider_id]["apiKey"] = json!(api_key);
        if let Ok(text) = serde_json::to_string_pretty(&models_json) {
            let _ = std::fs::write(&models_path, format!("{}\n", text));
        }
    }
}

pub(super) fn read_oopbot_env_map() -> HashMap<String, String> {
    let mut map = HashMap::new();
    let Ok(home) = std::env::var("HOME") else {
        return map;
    };
    let Ok(content) = std::fs::read_to_string(PathBuf::from(home).join(".config/oopbot/.env")) else {
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

