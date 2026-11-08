//! Mode RPC (`oopbot --mode rpc`) yang kompatibel 100% dengan protokol JSONL
//! aplikasi Android Pipkin (`AgentProcess.kt` & `ChatTimeline.kt`).
//!
//! Menggantikan `@earendil-works/pi-coding-agent` (`pi --mode rpc`) di Termux
//! dengan binary native Rust `oopbot`.

use crate::gemini::{
    function_response_part, user_message_with_images, BoxError, FunctionCall, GeminiClient,
    GenerationOptions, DEFAULT_GEMINI_BASE_URL,
};
use crate::models::{
    builtin_providers, infer_api_from_provider, infer_input_from_id, infer_reasoning_from_id,
    resolve_model_scope, ModelCatalog, ModelInfo,
};
use crate::plugins::PluginRegistry;
use crate::tools::ToolRegistry;
use serde_json::{json, Value};
use std::collections::HashMap;
use std::io::{self, BufRead, Write};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

#[derive(Debug, Clone)]
struct ProviderEndpoint {
    name: String,
    base_url: String,
    api: String,
    api_key: String,
    has_auth: bool,
    models: Vec<ModelInfo>,
}

pub struct RpcServer {
    cwd: PathBuf,
    config_dir: PathBuf,
    session_file: PathBuf,
    session_id: String,
    session_name: Option<String>,
    active_provider: String,
    active_model: String,
    thinking_level: String,
    system_instruction: String,
    providers: HashMap<String, ProviderEndpoint>,
    catalog: ModelCatalog,
    client: GeminiClient,
    tools: ToolRegistry,
    plugins: PluginRegistry,
    /// Riwayat pesan dalam format Pipkin (`role: "user" | "assistant" | "toolResult"`).
    ui_messages: Vec<Value>,
    /// Riwayat pesan dalam format `GeminiClient` (`role: "user" | "model", parts: [...]`).
    llm_history: Vec<Value>,
    entry_seq: usize,
    total_tokens: u64,
    total_cost: f64,
}

impl RpcServer {
    pub fn new(
        session_path: Option<String>,
        system_append: Option<String>,
        extensions: Vec<String>,
    ) -> Result<Self, BoxError> {
        let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
        let config_dir = std::env::var("PI_CODING_AGENT_DIR")
            .map(PathBuf::from)
            .unwrap_or_else(|_| {
                let home = std::env::var("HOME").unwrap_or_else(|_| ".".into());
                PathBuf::from(home).join(".pipkin/pi")
            });
        let _ = std::fs::create_dir_all(&config_dir);

        let mut tools = ToolRegistry::with_defaults();
        let mut plugins = PluginRegistry::with_defaults();

        // Aktifkan plugin berdasarkan argumen --extension / --plugin
        for ext in &extensions {
            plugins.activate(ext);
        }
        plugins.register_active_tools(&mut tools);

        let mut system_instruction = default_rpc_system_instruction(&cwd, &tools);
        let plugin_system_append = plugins.system_prompt_append();
        if !plugin_system_append.is_empty() {
            system_instruction.push_str("\n\n");
            system_instruction.push_str(&plugin_system_append);
        }
        if let Some(append) = system_append.filter(|s| !s.trim().is_empty()) {
            system_instruction.push_str("\n\n");
            system_instruction.push_str(&append);
        }

        let catalog = ModelCatalog::builtin();
        let mut providers = load_providers(&config_dir, &catalog);

        let settings = read_json_file(&config_dir.join("settings.json"));
        let mut active_provider = settings["defaultProvider"]
            .as_str()
            .filter(|s| !s.trim().is_empty())
            .unwrap_or("google")
            .to_string();
        let mut active_model = settings["defaultModel"]
            .as_str()
            .filter(|s| !s.trim().is_empty())
            .unwrap_or_else(|| catalog.default_model())
            .to_string();
        let requested_thinking = settings["defaultThinkingLevel"]
            .as_str()
            .filter(|s| !s.trim().is_empty())
            .unwrap_or("medium")
            .to_string();

        // Jika provider default belum terkonfigurasi tetapi provider lain sudah punya key/lokal, pilih yang siap pakai.
        if !providers
            .get(&active_provider)
            .map(|p| p.has_auth)
            .unwrap_or(false)
        {
            if let Some((pid, ep)) = providers.iter().find(|(_, p)| p.has_auth && !p.models.is_empty()) {
                active_provider = pid.clone();
                if !ep.models.iter().any(|m| m.id == active_model) {
                    if let Some(first) = ep.models.first() {
                        active_model = first.id.clone();
                    }
                }
            }
        }

        let (api_key, base_url) = providers
            .get(&active_provider)
            .map(|p| (p.api_key.clone(), p.base_url.clone()))
            .unwrap_or_else(|| (String::new(), DEFAULT_GEMINI_BASE_URL.to_string()));

        let client = GeminiClient::new(api_key.clone(), active_model.clone())?;
        client.update_credentials(api_key, Some(base_url));

        // Pastikan google selalu terdaftar di map providers.
        providers
            .entry("google".to_string())
            .or_insert_with(|| ProviderEndpoint {
                name: "Google Gemini (OOPBoT)".to_string(),
                base_url: DEFAULT_GEMINI_BASE_URL.to_string(),
                api: "google-generative-ai".to_string(),
                api_key: String::new(),
                has_auth: false,
                models: catalog.list().to_vec(),
            });

        let (session_file, session_id, session_name, ui_messages, llm_history, entry_seq) =
            init_or_load_session(&config_dir, &cwd, session_path.as_deref());

        let mut server = Self {
            cwd,
            config_dir,
            session_file,
            session_id,
            session_name,
            active_provider,
            active_model,
            thinking_level: requested_thinking.clone(),
            system_instruction,
            providers,
            catalog,
            client,
            tools,
            plugins,
            ui_messages,
            llm_history,
            entry_seq,
            total_tokens: 0,
            total_cost: 0.0,
        };
        server.thinking_level = server
            .active_model_info()
            .clamp_thinking_level(&requested_thinking);
        Ok(server)
    }

    /// Pancarkan event widget aktif (misal checklist Todo) ke antarmuka aplikasi Android via RPC JSONL.
    pub fn emit_active_widgets(&self) {
        for plugin in self.plugins.active_plugins() {
            if plugin.id() == "pipkin-todo" {
                if let Some(lines) = plugin.render_widget("todos") {
                    Self::send_line(&json!({
                        "type": "extension_ui_request",
                        "id": "todos-widget",
                        "method": "setWidget",
                        "widgetKey": "todos",
                        "widgetLines": lines,
                        "widgetPlacement": "aboveEditor"
                    }));
                }
            }
        }
    }

    fn send_line(value: &Value) {
        if let Ok(line) = serde_json::to_string(value) {
            let mut out = io::stdout().lock();
            let _ = writeln!(out, "{}", line);
            let _ = out.flush();
        }
    }

    fn send_response(id: &str, command: &str, data: Value) {
        Self::send_line(&json!({
            "type": "response",
            "id": id,
            "command": command,
            "success": true,
            "data": data,
        }));
    }

    fn send_error(id: &str, command: &str, error: &str) {
        Self::send_line(&json!({
            "type": "response",
            "id": id,
            "command": command,
            "success": false,
            "error": error,
        }));
    }

    /// Cari `ModelInfo` lengkap untuk model aktif saat ini beserta seluruh override dari `models.json`.
    pub fn active_model_info(&self) -> ModelInfo {
        if let Some(ep) = self.providers.get(&self.active_provider) {
            if let Some(m) = ep
                .models
                .iter()
                .find(|m| m.id.eq_ignore_ascii_case(&self.active_model))
            {
                let mut cloned = m.clone();
                cloned.provider = self.active_provider.clone();
                return cloned;
            }
        }
        for (pid, ep) in &self.providers {
            if let Some(m) = ep
                .models
                .iter()
                .find(|m| m.id.eq_ignore_ascii_case(&self.active_model))
            {
                let mut cloned = m.clone();
                cloned.provider = pid.clone();
                return cloned;
            }
        }
        if let Some(m) = self.catalog.find(&self.active_model) {
            let mut cloned = m.clone();
            cloned.provider = self.active_provider.clone();
            return cloned;
        }
        let default_api = self
            .providers
            .get(&self.active_provider)
            .map(|p| p.api.clone())
            .unwrap_or_else(|| infer_api_from_provider(&self.active_provider));
        ModelInfo {
            id: self.active_model.clone(),
            label: self.active_model.clone(),
            provider: self.active_provider.clone(),
            supports_tools: true,
            input_token_limit: Some(1_048_576),
            output_token_limit: Some(65_536),
            description: "Active model".to_string(),
            api: default_api,
            reasoning: infer_reasoning_from_id(&self.active_model),
            thinking_level_map: None,
            input: infer_input_from_id(&self.active_model),
            cost: None,
        }
    }

    fn current_model_json(&self) -> Value {
        self.active_model_info()
            .to_pipkin_json(Some(&self.active_provider))
    }

    fn available_models_json(&self) -> Vec<Value> {
        let mut all_models: Vec<ModelInfo> = Vec::new();
        let any_authenticated = self.providers.values().any(|ep| ep.has_auth);

        // Urutkan provider secara deterministik (built-in lebih dulu, lalu custom).
        let mut provider_ids: Vec<String> = self.providers.keys().cloned().collect();
        provider_ids.sort_by(|a, b| {
            let a_builtin = a == "google";
            let b_builtin = b == "google";
            b_builtin.cmp(&a_builtin).then_with(|| a.cmp(b))
        });

        for pid in provider_ids {
            let Some(ep) = self.providers.get(&pid) else {
                continue;
            };
            // Hanya tampilkan model dari provider yang sudah dikonfigurasi di aplikasi
            // (atau provider aktif/google bila belum ada satupun provider yang login).
            if any_authenticated && !ep.has_auth && pid != self.active_provider {
                continue;
            }
            if !any_authenticated && pid != "google" && pid != self.active_provider {
                continue;
            }
            for m in &ep.models {
                let mut item = m.clone();
                item.provider = pid.clone();
                all_models.push(item);
            }
        }

        if all_models.is_empty() {
            for m in self.catalog.list() {
                let mut item = m.clone();
                item.provider = "google".to_string();
                all_models.push(item);
            }
        }

        // Terapkan filter `enabledModels` dari `settings.json` bila diatur oleh aplikasi Android.
        let settings = read_json_file(&self.config_dir.join("settings.json"));
        if let Some(patterns_arr) = settings["enabledModels"].as_array() {
            let patterns: Vec<String> = patterns_arr
                .iter()
                .filter_map(|v| v.as_str().map(str::to_string))
                .collect();
            if !patterns.is_empty() {
                let scoped = resolve_model_scope(&patterns, &all_models);
                if !scoped.is_empty() {
                    return scoped
                        .into_iter()
                        .map(|(m, _)| m.to_pipkin_json(Some(&m.provider)))
                        .collect();
                }
            }
        }

        all_models
            .iter()
            .map(|m| m.to_pipkin_json(Some(&m.provider)))
            .collect()
    }

    fn state_json(&self) -> Value {
        json!({
            "sessionFile": self.session_file.to_string_lossy(),
            "sessionId": self.session_id,
            "sessionName": self.session_name,
            "isStreaming": false,
            "isCompacting": false,
            "model": self.current_model_json(),
            "thinkingLevel": self.thinking_level,
        })
    }

    fn append_session_entry(&mut self, entry_type: &str, mut payload: Value) {
        if let Some(parent) = self.session_file.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        if !self.session_file.exists() {
            let header = json!({
                "type": "session",
                "version": 3,
                "id": self.session_id,
                "timestamp": iso_now(),
                "cwd": self.cwd.to_string_lossy(),
            });
            if let Ok(line) = serde_json::to_string(&header) {
                let _ = std::fs::write(&self.session_file, format!("{}\n", line));
            }
        }

        let id = format!("e{}", self.entry_seq);
        let parent_id = if self.entry_seq == 0 {
            Value::Null
        } else {
            json!(format!("e{}", self.entry_seq - 1))
        };
        self.entry_seq += 1;

        payload["type"] = json!(entry_type);
        payload["id"] = json!(id);
        payload["parentId"] = parent_id;
        payload["timestamp"] = json!(iso_now());

        if let Ok(line) = serde_json::to_string(&payload) {
            if let Ok(mut file) = std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(&self.session_file)
            {
                let _ = writeln!(file, "{}", line);
            }
        }
    }

    fn refresh_credentials(&mut self) {
        self.providers = load_providers(&self.config_dir, &self.catalog);
        if let Some(ep) = self.providers.get(&self.active_provider) {
            self.client
                .update_credentials(ep.api_key.clone(), Some(ep.base_url.clone()));
        }
    }

    fn sync_thinking_level_for_active_model(&mut self) {
        let clamped = self
            .active_model_info()
            .clamp_thinking_level(&self.thinking_level);
        if clamped != self.thinking_level {
            self.thinking_level = clamped.clone();
            Self::send_line(&json!({
                "type": "thinking_level_changed",
                "level": clamped,
            }));
        }
    }

    /// Deteksi otomatis apakah token provider aktif (atau provider terotentikasi) belum pernah di-scan
    /// atau token baru saja berganti/ditempelkan di aplikasi Pipkin.
    /// Jika ya, lakukan auto-scan langsung ke `/models`, simpan hasilnya ke `models.json`,
    /// dan perbarui model aktif agar sesuai dengan kapabilitas aktual token tersebut.
    async fn maybe_auto_scan(&mut self) {
        let ep = match self.providers.get(&self.active_provider) {
            Some(p) => p.clone(),
            None => return,
        };
        if !ep.has_auth || ep.api_key.trim().is_empty() {
            return;
        }

        let models_path = self.config_dir.join("models.json");
        let models_json = read_json_file(&models_path);
        let prov_entry = &models_json["providers"][&self.active_provider];
        let has_saved_models = prov_entry["models"]
            .as_array()
            .map(|a| !a.is_empty())
            .unwrap_or(false);
        let scanned_key = prov_entry["scannedApiKey"].as_str().unwrap_or("");

        // Bila sudah tersimpan model hasil scan dan API key belum berubah, tidak perlu scan ulang.
        if has_saved_models && scanned_key == ep.api_key {
            return;
        }

        // Jalankan auto-scan langsung dari token yang ditempel di aplikasi
        if let Ok(scanned) = self.client.scan_models().await {
            if !scanned.is_empty() {
                save_scanned_models_to_config(
                    &self.config_dir,
                    &self.active_provider,
                    &scanned,
                    &ep.api_key,
                );
                self.catalog.replace_with_scanned(scanned.clone());
                // Jika model aktif saat ini tidak ada di daftar yang di-scan,
                // ganti ke model hasil scan yang optimal (preferensi: flash non-lite jika ada, atau pertama).
                if !scanned
                    .iter()
                    .any(|m| m.id.eq_ignore_ascii_case(&self.active_model))
                {
                    if let Some(pref) = scanned
                        .iter()
                        .find(|m| m.id.contains("flash") && !m.id.contains("lite"))
                    {
                        self.active_model = pref.id.clone();
                    } else if let Some(first) = scanned.first() {
                        self.active_model = first.id.clone();
                    }
                }
                self.client.set_default_model(&self.active_model);
                self.refresh_credentials();
                self.sync_thinking_level_for_active_model();
                save_settings_fields(
                    &self.config_dir,
                    Some(&self.active_provider),
                    Some(&self.active_model),
                    Some(&self.thinking_level),
                );
            }
        }
    }

    async fn handle_slash_command(&mut self, cmd_line: &str) -> Option<String> {
        let parts: Vec<&str> = cmd_line.split_whitespace().collect();
        let cmd = parts.first().copied()?;
        match cmd {
            "/scan" => {
                self.refresh_credentials();
                match self.client.scan_models().await {
                    Ok(scanned) => {
                        let count = scanned.len();
                        let preview: Vec<String> = scanned
                            .iter()
                            .take(15)
                            .map(|m| {
                                let think_badge = if m.reasoning { " 🧠" } else { "" };
                                format!("- `{}` ({}){}", m.id, m.label, think_badge)
                            })
                            .collect();
                        self.catalog.replace_with_scanned(scanned.clone());
                        let cur_key = self
                            .providers
                            .get(&self.active_provider)
                            .map(|p| p.api_key.as_str())
                            .unwrap_or("");
                        save_scanned_models_to_config(
                            &self.config_dir,
                            &self.active_provider,
                            &scanned,
                            cur_key,
                        );
                        self.client.set_default_model(&self.active_model);
                        self.refresh_credentials();
                        Some(format!(
                            "Berhasil men-scan **{} model** dari `{}` dan menyimpannya ke `models.json`:\n{}\n\nGunakan pemilih model di atas atau ketik `/model <ID>` untuk mengganti model.",
                            count,
                            self.client.base_url(),
                            preview.join("\n")
                        ))
                    }
                    Err(e) => Some(format!("Gagal scan model: {}", e)),
                }
            }
            "/token" => {
                if parts.len() < 2 {
                    return Some(
                        "Format: `/token <API_KEY> [BASE_URL]`\nContoh:\n- `/token AIzaSy...`\n- `/token sk-or-v1-... https://openrouter.ai/api/v1`"
                            .to_string(),
                    );
                }
                let api_key = parts[1].trim();
                let base_url = parts.get(2).map(|s| s.trim().to_string());
                self.client
                    .update_credentials(api_key.to_string(), base_url.clone());
                save_auth_key(
                    &self.config_dir,
                    &self.active_provider,
                    api_key,
                    base_url.as_deref(),
                );
                match self.client.scan_models().await {
                    Ok(scanned) => {
                        let count = scanned.len();
                        if !scanned.iter().any(|m| m.id == self.active_model) {
                            if let Some(first) = scanned.first() {
                                self.active_model = first.id.clone();
                            }
                        }
                        self.catalog.replace_with_scanned(scanned.clone());
                        self.client.set_default_model(&self.active_model);
                        save_scanned_models_to_config(
                            &self.config_dir,
                            &self.active_provider,
                            &scanned,
                            api_key,
                        );
                        self.refresh_credentials();
                        save_settings_fields(
                            &self.config_dir,
                            Some(&self.active_provider),
                            Some(&self.active_model),
                            None,
                        );
                        Some(format!(
                            "Token berhasil disimpan & **{} model** terdeteksi otomatis! Model aktif sekarang: `{}` (`{}`).",
                            count, self.active_model, self.active_provider
                        ))
                    }
                    Err(e) => {
                        self.refresh_credentials();
                        Some(format!(
                            "Token disimpan, namun auto-scan `/models` gagal: {}",
                            e
                        ))
                    }
                }
            }
            "/models" => {
                self.refresh_credentials();
                let avail = self.available_models_json();
                let lines: Vec<String> = avail
                    .iter()
                    .enumerate()
                    .map(|(i, m)| {
                        let pid = m["provider"].as_str().unwrap_or("");
                        let mid = m["id"].as_str().unwrap_or("");
                        let name = m["name"].as_str().unwrap_or(mid);
                        let cw = m["contextWindow"].as_u64().unwrap_or(0) / 1000;
                        let think = if m["reasoning"].as_bool() == Some(true) {
                            " · 🧠 thinking"
                        } else {
                            ""
                        };
                        let mark = if pid == self.active_provider && mid == self.active_model {
                            " **(aktif)**"
                        } else {
                            ""
                        };
                        format!(
                            "{}. `{}/{}` — {} ({}k ctx{}){}",
                            i + 1,
                            pid,
                            mid,
                            name,
                            cw,
                            think,
                            mark
                        )
                    })
                    .collect();
                Some(format!(
                    "**Daftar Model Tersedia ({}):**\n{}",
                    avail.len(),
                    lines.join("\n")
                ))
            }
            "/model" => {
                if parts.len() < 2 {
                    let info = self.active_model_info();
                    return Some(format!(
                        "Model aktif saat ini: `{}/{}` (**{}**)\n- API: `{}`\n- Context window: `{}` token\n- Max output: `{}` token\n- Thinking: `{}` (level aktif: `{}`)\n\nGunakan `/model <ID>` atau `/model <provider>/<ID>` untuk mengganti.",
                        self.active_provider,
                        self.active_model,
                        info.label,
                        info.api,
                        info.input_token_limit.unwrap_or(1_048_576),
                        info.output_token_limit.unwrap_or(65_536),
                        info.reasoning,
                        self.thinking_level
                    ));
                }
                let target = parts[1].trim();
                if let Some((prov, mid)) = target.split_once('/') {
                    if self.providers.contains_key(prov) {
                        self.active_provider = prov.to_string();
                        self.active_model = mid.to_string();
                    } else {
                        self.active_model = target.to_string();
                    }
                } else if let Some(info) = self.catalog.find(target) {
                    self.active_model = info.id.clone();
                } else {
                    // Cek apakah ada di salah satu provider
                    let mut found = None;
                    for (pid, ep) in &self.providers {
                        if let Some(m) = ep
                            .models
                            .iter()
                            .find(|m| m.id.eq_ignore_ascii_case(target))
                        {
                            found = Some((pid.clone(), m.id.clone()));
                            break;
                        }
                    }
                    if let Some((pid, mid)) = found {
                        self.active_provider = pid;
                        self.active_model = mid;
                    } else {
                        self.catalog.register_custom(target, target);
                        self.active_model = target.to_string();
                    }
                }
                self.client.set_default_model(&self.active_model);
                self.refresh_credentials();
                self.sync_thinking_level_for_active_model();
                save_settings_fields(
                    &self.config_dir,
                    Some(&self.active_provider),
                    Some(&self.active_model),
                    Some(&self.thinking_level),
                );
                Some(format!(
                    "Model aktif diganti ke `{}/{}` (thinking: `{}`).",
                    self.active_provider, self.active_model, self.thinking_level
                ))
            }
            "/thinking" => {
                let info = self.active_model_info();
                let supported = info.available_thinking_levels();
                if parts.len() < 2 {
                    return Some(format!(
                        "Level thinking saat ini: `{}` (didukung oleh `{}`: `{}`).\nGunakan `/thinking <{}>` untuk mengubah.",
                        self.thinking_level,
                        self.active_model,
                        supported.join(", "),
                        supported.join("|")
                    ));
                }
                let requested = parts[1].trim().to_ascii_lowercase();
                let clamped = info.clamp_thinking_level(&requested);
                self.thinking_level = clamped.clone();
                save_settings_fields(&self.config_dir, None, None, Some(&clamped));
                Self::send_line(&json!({
                    "type": "thinking_level_changed",
                    "level": clamped,
                }));
                Some(format!(
                    "Level thinking diatur ke `{}` untuk model `{}`.",
                    self.thinking_level, self.active_model
                ))
            }
            "/provider" => {
                if parts.len() < 2 {
                    let mut lines = Vec::new();
                    for (pid, ep) in &self.providers {
                        let mark = if *pid == self.active_provider {
                            " **(aktif)**"
                        } else {
                            ""
                        };
                        let auth_badge = if ep.has_auth { "✅ siap" } else { "⚪ belum ada key" };
                        lines.push(format!(
                            "- `{}` ({}) — {} · {} model{}",
                            pid,
                            ep.base_url,
                            auth_badge,
                            ep.models.len(),
                            mark
                        ));
                    }
                    lines.sort();
                    return Some(format!(
                        "**Daftar Provider:**\n{}\n\nGunakan `/provider <ID>` untuk berpindah provider.",
                        lines.join("\n")
                    ));
                }
                let target_pid = parts[1].trim();
                if let Some(ep) = self.providers.get(target_pid).cloned() {
                    self.active_provider = target_pid.to_string();
                    if let Some(first) = ep.models.first() {
                        self.active_model = first.id.clone();
                    }
                    self.refresh_credentials();
                    self.sync_thinking_level_for_active_model();
                    save_settings_fields(
                        &self.config_dir,
                        Some(&self.active_provider),
                        Some(&self.active_model),
                        Some(&self.thinking_level),
                    );
                    Some(format!(
                        "Provider aktif diganti ke `{}` dengan model `{}`.",
                        self.active_provider, self.active_model
                    ))
                } else {
                    Some(format!("Provider `{}` tidak ditemukan.", target_pid))
                }
            }
            "/max-tokens" => {
                if parts.len() < 2 {
                    let cur = self
                        .active_model_info()
                        .output_token_limit
                        .unwrap_or(65_536);
                    return Some(format!(
                        "Batas `maxTokens` model `{}` saat ini: `{}`.\nGunakan `/max-tokens <angka>` untuk mengubah dan menyimpan ke `models.json`.",
                        self.active_model, cur
                    ));
                }
                let Ok(max_tok) = parts[1].trim().parse::<u64>() else {
                    return Some("Nilai `/max-tokens` harus berupa angka bulat positif.".to_string());
                };
                if max_tok == 0 {
                    return Some("Nilai `/max-tokens` harus lebih besar dari 0.".to_string());
                }
                save_model_override_to_config(
                    &self.config_dir,
                    &self.active_provider,
                    &self.active_model,
                    &json!({ "maxTokens": max_tok }),
                );
                self.refresh_credentials();
                Some(format!(
                    "`maxTokens` untuk `{}/{}` berhasil disimpan ke `{}`.",
                    self.active_provider, self.active_model, max_tok
                ))
            }
            "/clear" => {
                self.ui_messages.clear();
                self.llm_history.clear();
                Some("Memori percakapan sesi ini telah dibersihkan.".to_string())
            }
            _ => None,
        }
    }

    async fn execute_prompt(&mut self, prompt: &str, images: &[(String, String)]) {
        self.refresh_credentials();
        self.sync_thinking_level_for_active_model();
        let started_ms = epoch_ms();

        Self::send_line(&json!({ "type": "agent_start" }));

        let mut user_content_blocks = Vec::new();
        if !prompt.is_empty() {
            user_content_blocks.push(json!({ "type": "text", "text": prompt }));
        }
        for (mime, data) in images {
            user_content_blocks.push(json!({
                "type": "image",
                "mimeType": mime,
                "data": data,
            }));
        }
        if user_content_blocks.is_empty() {
            user_content_blocks.push(json!({ "type": "text", "text": "" }));
        }

        let user_msg = json!({
            "role": "user",
            "content": user_content_blocks,
            "timestamp": started_ms,
        });
        Self::send_line(&json!({ "type": "message_start", "message": user_msg }));
        Self::send_line(&json!({ "type": "message_end", "message": user_msg }));
        self.ui_messages.push(user_msg.clone());
        self.append_session_entry("message", json!({ "message": user_msg }));

        // Cek apakah perintah slash internal (/scan, /token, /models, /model, /thinking, /provider, /max-tokens, /clear)
        if prompt.trim_start().starts_with('/') && images.is_empty() {
            if let Some(reply_text) = self.handle_slash_command(prompt.trim()).await {
                self.emit_assistant_reply_with_thoughts(&reply_text, &[], started_ms);
                Self::send_line(&json!({ "type": "turn_end" }));
                Self::send_line(&json!({ "type": "agent_end" }));
                Self::send_line(&json!({ "type": "agent_settled", "aborted": false }));
                return;
            }
        }

        let provider_has_auth = self
            .providers
            .get(&self.active_provider)
            .map(|p| p.has_auth)
            .unwrap_or(false);
        if !provider_has_auth && self.client.api_key().trim().is_empty() {
            let msg = "API key belum diatur. Silakan buka **Settings › Providers & models** di Pipkin untuk menempelkan API key (Google Gemini / OpenRouter / Custom), atau ketik langsung di sini:\n\n`/token <API_KEY>`";
            self.emit_assistant_reply_with_thoughts(msg, &[], started_ms);
            Self::send_line(&json!({ "type": "turn_end" }));
            Self::send_line(&json!({ "type": "agent_end" }));
            Self::send_line(&json!({ "type": "agent_settled", "aborted": false }));
            return;
        }

        let active_info = self.active_model_info();
        let gen_options = GenerationOptions {
            api: Some(active_info.api.clone()),
            max_tokens: active_info.output_token_limit,
            thinking_level: Some(self.thinking_level.clone()),
            reasoning: active_info.reasoning,
            thinking_level_map: active_info.thinking_level_map.clone(),
            session_id: Some(self.session_id.clone()),
        };

        self.llm_history
            .push(user_message_with_images(prompt, images));
        let decls = self.tools.declarations();
        let max_steps = 25usize;

        for step_idx in 1..=max_steps {
            let step_start_ms = epoch_ms();
            let turn_res = self
                .client
                .generate_with_options(
                    &self.active_model,
                    &self.system_instruction,
                    &self.llm_history,
                    &decls,
                    &gen_options,
                )
                .await;

            let turn = match turn_res {
                Ok(t) => t,
                Err(err) => {
                    let err_msg = json!({
                        "role": "assistant",
                        "content": [],
                        "stopReason": "error",
                        "errorMessage": err.to_string(),
                        "timestamp": step_start_ms,
                        "durationMs": epoch_ms().saturating_sub(step_start_ms),
                    });
                    Self::send_line(&json!({ "type": "message_start", "message": err_msg }));
                    Self::send_line(&json!({ "type": "message_end", "message": err_msg }));
                    self.ui_messages.push(err_msg.clone());
                    self.append_session_entry("message", json!({ "message": err_msg }));
                    break;
                }
            };

            self.llm_history.push(turn.raw_content.clone());
            if let Some(usage) = turn.usage {
                self.total_tokens = usage.input_tokens + usage.output_tokens;
                if let Some(cost) = &active_info.cost {
                    self.total_cost += (usage.input_tokens as f64 / 1_000_000.0) * cost.input
                        + (usage.output_tokens as f64 / 1_000_000.0) * cost.output;
                }
            } else {
                let est_in = (prompt.len() as u64 / 4) + 60;
                let est_out = (turn.text.len() as u64 / 4) + 60;
                self.total_tokens += est_in + est_out;
                if let Some(cost) = &active_info.cost {
                    self.total_cost += (est_in as f64 / 1_000_000.0) * cost.input
                        + (est_out as f64 / 1_000_000.0) * cost.output;
                }
            }

            if turn.function_calls.is_empty() {
                self.emit_assistant_reply_with_thoughts(&turn.text, &turn.thoughts, step_start_ms);
                break;
            }

            // Ada pemanggilan tool: pancarkan message_start (assistant) + thinking + toolCall blocks
            Self::send_line(&json!({
                "type": "message_start",
                "message": {
                    "role": "assistant",
                    "content": [],
                    "timestamp": step_start_ms
                }
            }));

            let mut content_blocks = Vec::new();
            let mut content_idx = 0usize;

            for thought in &turn.thoughts {
                Self::send_line(&json!({
                    "type": "message_update",
                    "assistantMessageEvent": {
                        "type": "thinking_start",
                        "contentIndex": content_idx
                    }
                }));
                Self::send_line(&json!({
                    "type": "message_update",
                    "assistantMessageEvent": {
                        "type": "thinking_end",
                        "contentIndex": content_idx,
                        "content": thought
                    }
                }));
                content_blocks.push(json!({
                    "type": "thinking",
                    "thinking": thought
                }));
                content_idx += 1;
            }

            if !turn.text.is_empty() {
                Self::send_line(&json!({
                    "type": "message_update",
                    "assistantMessageEvent": {
                        "type": "thinking_start",
                        "contentIndex": content_idx
                    }
                }));
                Self::send_line(&json!({
                    "type": "message_update",
                    "assistantMessageEvent": {
                        "type": "thinking_end",
                        "contentIndex": content_idx,
                        "content": turn.text
                    }
                }));
                content_blocks.push(json!({
                    "type": "thinking",
                    "thinking": turn.text
                }));
                content_idx += 1;
            }

            let mut mapped_calls = Vec::with_capacity(turn.function_calls.len());
            for (call_i, call) in turn.function_calls.iter().enumerate() {
                let call_id = call
                    .id
                    .clone()
                    .unwrap_or_else(|| format!("call_{}_{}", step_idx, call_i));
                let (ui_tool_name, ui_args) = map_tool_for_pipkin_ui(&call.name, &call.args);

                Self::send_line(&json!({
                    "type": "message_update",
                    "assistantMessageEvent": {
                        "type": "toolcall_start",
                        "contentIndex": content_idx,
                        "id": call_id,
                        "toolName": ui_tool_name
                    }
                }));
                Self::send_line(&json!({
                    "type": "message_update",
                    "assistantMessageEvent": {
                        "type": "toolcall_end",
                        "contentIndex": content_idx,
                        "toolCall": {
                            "id": call_id,
                            "name": ui_tool_name,
                            "arguments": ui_args
                        }
                    }
                }));
                content_blocks.push(json!({
                    "type": "toolCall",
                    "id": call_id,
                    "name": ui_tool_name,
                    "arguments": ui_args
                }));
                content_idx += 1;
                mapped_calls.push((call.clone(), call_id, ui_tool_name, ui_args));
            }

            let assistant_tool_msg = json!({
                "role": "assistant",
                "content": content_blocks,
                "stopReason": "toolUse",
                "timestamp": step_start_ms,
                "durationMs": epoch_ms().saturating_sub(step_start_ms),
            });
            Self::send_line(&json!({
                "type": "message_end",
                "message": assistant_tool_msg
            }));
            self.ui_messages.push(assistant_tool_msg.clone());
            self.append_session_entry("message", json!({ "message": assistant_tool_msg }));

            // Eksekusi setiap tool dan kirimkan event tool_execution_start / end + toolResult
            let mut llm_response_parts = Vec::with_capacity(mapped_calls.len());
            for (call, call_id, ui_tool_name, ui_args) in mapped_calls {
                Self::send_line(&json!({
                    "type": "tool_execution_start",
                    "toolCallId": call_id,
                    "toolName": ui_tool_name,
                    "args": ui_args
                }));

                let (llm_val, display_text, is_error) = self.execute_single_tool(&call).await;
                let result_obj = json!({
                    "content": [{ "type": "text", "text": display_text }]
                });

                Self::send_line(&json!({
                    "type": "tool_execution_end",
                    "toolCallId": call_id,
                    "toolName": ui_tool_name,
                    "isError": is_error,
                    "result": result_obj
                }));

                let tool_result_msg = json!({
                    "role": "toolResult",
                    "toolCallId": call_id,
                    "toolName": ui_tool_name,
                    "isError": is_error,
                    "content": [{ "type": "text", "text": display_text }],
                    "timestamp": epoch_ms(),
                });
                Self::send_line(&json!({
                    "type": "message_end",
                    "message": tool_result_msg
                }));
                self.ui_messages.push(tool_result_msg.clone());
                self.append_session_entry("message", json!({ "message": tool_result_msg }));

                llm_response_parts.push(function_response_part(&call, llm_val));
            }

            self.emit_active_widgets();

            self.llm_history
                .push(json!({ "role": "user", "parts": llm_response_parts }));
        }

        Self::send_line(&json!({ "type": "turn_end" }));
        Self::send_line(&json!({ "type": "agent_end" }));
        Self::send_line(&json!({ "type": "agent_settled", "aborted": false }));
    }

    fn emit_assistant_reply_with_thoughts(
        &mut self,
        text: &str,
        thoughts: &[String],
        started_ms: u64,
    ) {
        let now_ms = epoch_ms();
        Self::send_line(&json!({
            "type": "message_start",
            "message": {
                "role": "assistant",
                "content": [],
                "timestamp": started_ms
            }
        }));

        let mut content_blocks = Vec::new();
        let mut content_idx = 0usize;

        for thought in thoughts {
            if thought.trim().is_empty() {
                continue;
            }
            Self::send_line(&json!({
                "type": "message_update",
                "assistantMessageEvent": {
                    "type": "thinking_start",
                    "contentIndex": content_idx
                }
            }));
            Self::send_line(&json!({
                "type": "message_update",
                "assistantMessageEvent": {
                    "type": "thinking_end",
                    "contentIndex": content_idx,
                    "content": thought
                }
            }));
            content_blocks.push(json!({
                "type": "thinking",
                "thinking": thought
            }));
            content_idx += 1;
        }

        Self::send_line(&json!({
            "type": "message_update",
            "assistantMessageEvent": {
                "type": "text_start",
                "contentIndex": content_idx
            }
        }));
        Self::send_line(&json!({
            "type": "message_update",
            "assistantMessageEvent": {
                "type": "text_delta",
                "contentIndex": content_idx,
                "delta": text
            }
        }));
        Self::send_line(&json!({
            "type": "message_update",
            "assistantMessageEvent": {
                "type": "text_end",
                "contentIndex": content_idx,
                "content": text
            }
        }));
        content_blocks.push(json!({ "type": "text", "text": text }));

        let msg = json!({
            "role": "assistant",
            "content": content_blocks,
            "stopReason": "stop",
            "timestamp": started_ms,
            "durationMs": now_ms.saturating_sub(started_ms),
        });
        Self::send_line(&json!({ "type": "message_end", "message": msg }));
        self.ui_messages.push(msg.clone());
        self.append_session_entry("message", json!({ "message": msg }));
    }

    async fn execute_single_tool(&self, call: &FunctionCall) -> (Value, String, bool) {
        let Some(tool) = self.tools.get(&call.name) else {
            let msg = format!("Tool '{}' tidak ditemukan", call.name);
            return (json!({ "error": msg }), msg, true);
        };
        match tool.execute(&call.args).await {
            Ok(val) => {
                let (text, is_err) = format_tool_output(&call.name, &val);
                (val, text, is_err)
            }
            Err(err) => (json!({ "error": err }), err, true),
        }
    }

    pub async fn run_loop(&mut self) -> Result<(), BoxError> {
        self.emit_active_widgets();
        let stdin = io::stdin();
        let mut reader = stdin.lock();
        let mut line = String::new();

        loop {
            line.clear();
            let bytes = reader.read_line(&mut line)?;
            if bytes == 0 {
                break;
            }
            let trimmed = line.trim_end_matches(&['\r', '\n'][..]);
            if trimmed.is_empty() {
                continue;
            }
            let Ok(cmd) = serde_json::from_str::<Value>(trimmed) else {
                continue;
            };
            let cmd_type = cmd["type"].as_str().unwrap_or_default().to_string();
            let id = cmd["id"].as_str().unwrap_or_default().to_string();

            match cmd_type.as_str() {
                "get_state" => {
                    self.refresh_credentials();
                    self.maybe_auto_scan().await;
                    Self::send_response(&id, "get_state", self.state_json());
                }
                "get_messages" => {
                    Self::send_response(
                        &id,
                        "get_messages",
                        json!({ "messages": self.ui_messages }),
                    );
                }
                "get_session_stats" => {
                    let window = self
                        .active_model_info()
                        .input_token_limit
                        .unwrap_or(1_048_576)
                        .max(1);
                    let pct = (self.total_tokens as f64 / window as f64) * 100.0;
                    Self::send_response(
                        &id,
                        "get_session_stats",
                        json!({
                            "contextUsage": {
                                "tokens": self.total_tokens,
                                "contextWindow": window,
                                "percent": pct
                            },
                            "cost": self.total_cost
                        }),
                    );
                }
                "get_available_models" => {
                    self.refresh_credentials();
                    self.maybe_auto_scan().await;
                    Self::send_response(
                        &id,
                        "get_available_models",
                        json!({ "models": self.available_models_json() }),
                    );
                }
                "set_model" => {
                    let mut provider_changed = false;
                    if let Some(p) = cmd["provider"].as_str().filter(|s| !s.trim().is_empty()) {
                        if p != self.active_provider {
                            self.active_provider = p.to_string();
                            provider_changed = true;
                        }
                    }
                    if let Some(m) = cmd["modelId"].as_str().filter(|s| !s.trim().is_empty()) {
                        self.active_model = m.to_string();
                    }
                    self.refresh_credentials();
                    if provider_changed {
                        self.maybe_auto_scan().await;
                    }
                    self.client.set_default_model(&self.active_model);
                    self.sync_thinking_level_for_active_model();
                    save_settings_fields(
                        &self.config_dir,
                        Some(&self.active_provider),
                        Some(&self.active_model),
                        Some(&self.thinking_level),
                    );
                    Self::send_response(&id, "set_model", self.current_model_json());
                }
                "get_available_thinking_levels" => {
                    self.refresh_credentials();
                    let levels = self.active_model_info().available_thinking_levels();
                    Self::send_response(
                        &id,
                        "get_available_thinking_levels",
                        json!({ "levels": levels }),
                    );
                }
                "set_thinking_level" => {
                    if let Some(l) = cmd["level"].as_str().filter(|s| !s.trim().is_empty()) {
                        let clamped = self.active_model_info().clamp_thinking_level(l);
                        self.thinking_level = clamped.clone();
                        save_settings_fields(&self.config_dir, None, None, Some(&clamped));
                        Self::send_line(&json!({
                            "type": "thinking_level_changed",
                            "level": clamped,
                        }));
                    }
                    Self::send_response(&id, "set_thinking_level", json!({}));
                }
                "set_session_name" => {
                    let name = cmd["name"]
                        .as_str()
                        .map(|s| s.trim().to_string())
                        .filter(|s| !s.is_empty());
                    self.session_name = name.clone();
                    self.append_session_entry("session_info", json!({ "name": name }));
                    Self::send_response(&id, "set_session_name", json!({}));
                    Self::send_line(&json!({
                        "type": "session_info_changed",
                        "name": self.session_name
                    }));
                }
                "clear_queue" => {
                    Self::send_response(
                        &id,
                        "clear_queue",
                        json!({ "steering": [], "followUp": [] }),
                    );
                }
                "get_commands" => {
                    Self::send_response(
                        &id,
                        "get_commands",
                        json!({
                            "commands": [
                                { "name": "scan", "description": "Scan daftar model langsung dari token provider aktif" },
                                { "name": "token", "description": "Ganti API key & URL provider (/token <KEY> [URL])" },
                                { "name": "models", "description": "Tampilkan semua model yang tersedia di aplikasi" },
                                { "name": "model", "description": "Ganti model aktif (/model <ID> atau /model <provider>/<ID>)" },
                                { "name": "thinking", "description": "Atur level penalaran (/thinking <off|minimal|low|medium|high|xhigh>)" },
                                { "name": "provider", "description": "Lihat atau ganti provider aktif (/provider <ID>)" },
                                { "name": "max-tokens", "description": "Atur batas token output model aktif (/max-tokens <angka>)" },
                                { "name": "clear", "description": "Bersihkan memori sesi saat ini" }
                            ]
                        }),
                    );
                }
                "get_last_assistant_text" => {
                    let last_text = self
                        .ui_messages
                        .iter()
                        .rev()
                        .find(|m| m["role"] == "assistant")
                        .and_then(|m| m["content"].as_array())
                        .and_then(|arr| {
                            arr.iter()
                                .find(|b| b["type"] == "text")
                                .and_then(|b| b["text"].as_str())
                        })
                        .map(str::to_string);
                    Self::send_response(
                        &id,
                        "get_last_assistant_text",
                        json!({ "text": last_text }),
                    );
                }
                "get_fork_messages" => {
                    Self::send_response(&id, "get_fork_messages", json!({ "messages": [] }));
                }
                "compact" => {
                    Self::send_response(&id, "compact", json!({}));
                }
                "abort" => {
                    Self::send_response(&id, "abort", json!({}));
                }
                "prompt" | "steer" => {
                    let message = cmd["message"].as_str().unwrap_or_default().to_string();
                    let images = extract_prompt_images(&cmd["images"]);
                    Self::send_response(
                        &id,
                        &cmd_type,
                        json!({ "disposition": "started" }),
                    );
                    if !message.trim().is_empty() || !images.is_empty() {
                        self.execute_prompt(&message, &images).await;
                    }
                }
                other => {
                    if !id.is_empty() {
                        Self::send_error(&id, other, &format!("Command '{}' tidak dikenal", other));
                    }
                }
            }
        }
        Ok(())
    }
}

fn extract_prompt_images(images_val: &Value) -> Vec<(String, String)> {
    let Some(arr) = images_val.as_array() else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for item in arr {
        let data = item["data"].as_str().unwrap_or_default().trim();
        if data.is_empty() {
            continue;
        }
        let mime = item["mimeType"]
            .as_str()
            .filter(|s| !s.is_empty())
            .unwrap_or("image/jpeg");
        out.push((mime.to_string(), data.to_string()));
    }
    out
}

pub async fn run_rpc_mode(
    session_path: Option<String>,
    system_append: Option<String>,
    extensions: Vec<String>,
) -> Result<(), BoxError> {
    let mut server = RpcServer::new(session_path, system_append, extensions)?;
    server.run_loop().await
}

/// Konversi nama & argumen tool internal `oopbot` ke format tampilan `ChatTimeline.kt` & `StepFigure.kt`.
fn map_tool_for_pipkin_ui(tool_name: &str, args: &Value) -> (String, Value) {
    match tool_name {
        "run_shell" => ("bash".to_string(), args.clone()),
        "read_file" => ("read".to_string(), args.clone()),
        "write_file" => ("write".to_string(), args.clone()),
        "list_dir" => ("ls".to_string(), args.clone()),
        "todo" => ("todo".to_string(), args.clone()),
        "web_search" => ("web_search".to_string(), args.clone()),
        "fetch_page" => ("fetch_page".to_string(), args.clone()),
        "background" => ("background".to_string(), args.clone()),
        other => (other.to_string(), args.clone()),
    }
}

fn format_tool_output(tool_name: &str, val: &Value) -> (String, bool) {
    match tool_name {
        "run_shell" => {
            let stdout = val["stdout"].as_str().unwrap_or_default();
            let stderr = val["stderr"].as_str().unwrap_or_default();
            let code = val["exit_code"].as_i64().unwrap_or(0);
            let mut out = String::new();
            if !stdout.is_empty() {
                out.push_str(stdout);
            }
            if !stderr.is_empty() {
                if !out.is_empty() && !out.ends_with('\n') {
                    out.push('\n');
                }
                out.push_str(stderr);
            }
            if code != 0 {
                if !out.is_empty() && !out.ends_with('\n') {
                    out.push('\n');
                }
                // StepFigure.kt membaca baris terakhir `Command exited with code <N>`
                out.push_str(&format!("Command exited with code {}", code));
            }
            (out, code != 0)
        }
        "read_file" => {
            let content = val["content"].as_str().unwrap_or_default().to_string();
            (content, false)
        }
        "write_file" => {
            let bytes = val["bytes_written"].as_u64().unwrap_or(0);
            (format!("Wrote {} bytes", bytes), false)
        }
        "list_dir" => {
            if let Some(entries) = val["entries"].as_array() {
                let lines: Vec<String> = entries
                    .iter()
                    .map(|e| {
                        let name = e["name"].as_str().unwrap_or_default();
                        let kind = e["type"].as_str().unwrap_or("file");
                        if kind == "dir" {
                            format!("{}/", name)
                        } else {
                            name.to_string()
                        }
                    })
                    .collect();
                (lines.join("\n"), false)
            } else {
                (val.to_string(), false)
            }
        }
        "todo" => {
            if let Some(msg) = val["message"].as_str() {
                (msg.to_string(), val["status"].as_str() == Some("error"))
            } else {
                (serde_json::to_string_pretty(val).unwrap_or_default(), false)
            }
        }
        "web_search" => {
            let count = val["count"].as_u64().unwrap_or(0);
            let mut out = format!("Ditemukan {} hasil pencarian:\n", count);
            if let Some(results) = val["results"].as_array() {
                for (idx, r) in results.iter().enumerate() {
                    let title = r["title"].as_str().unwrap_or("");
                    let url = r["url"].as_str().unwrap_or("");
                    let snippet = r["snippet"].as_str().unwrap_or("");
                    out.push_str(&format!("{}. [{}]({})\n   {}\n", idx + 1, title, url, snippet));
                }
            }
            (out, false)
        }
        "fetch_page" => {
            let content = val["content"].as_str().unwrap_or_default();
            (content.to_string(), false)
        }
        "background" => {
            if let Some(msg) = val["message"].as_str() {
                (msg.to_string(), false)
            } else if let Some(out) = val["output"].as_str() {
                (out.to_string(), false)
            } else {
                (serde_json::to_string_pretty(val).unwrap_or_default(), false)
            }
        }
        _ => (val.to_string(), false),
    }
}

fn load_providers(config_dir: &Path, catalog: &ModelCatalog) -> HashMap<String, ProviderEndpoint> {
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
    if let Some(key) = get_env_or_file("GEMINI_API_KEY") {
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

fn extract_auth_key(auth_json: &Value, provider_id: &str) -> String {
    let entry = &auth_json[provider_id];
    if let Some(s) = entry.as_str() {
        return s.to_string();
    }
    if let Some(s) = entry["apiKey"].as_str() {
        return s.to_string();
    }
    if let Some(s) = entry["key"].as_str() {
        return s.to_string();
    }
    if let Some(s) = entry["auth"]["apiKey"].as_str() {
        return s.to_string();
    }
    String::new()
}

fn save_settings_fields(
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

fn save_scanned_models_to_config(
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

fn save_model_override_to_config(
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

fn save_auth_key(config_dir: &Path, provider_id: &str, api_key: &str, base_url: Option<&str>) {
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

fn read_oopbot_env_map() -> HashMap<String, String> {
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


fn read_json_file(path: &Path) -> Value {
    std::fs::read_to_string(path)
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_else(|| json!({}))
}

fn init_or_load_session(
    config_dir: &Path,
    cwd: &Path,
    session_path: Option<&str>,
) -> (PathBuf, String, Option<String>, Vec<Value>, Vec<Value>, usize) {
    if let Some(path_str) = session_path.filter(|s| !s.trim().is_empty()) {
        let path = PathBuf::from(path_str);
        if path.exists() {
            if let Ok(content) = std::fs::read_to_string(&path) {
                let mut session_id = format!("s{}", epoch_ms());
                let mut session_name = None;
                let mut ui_messages = Vec::new();
                let mut llm_history = Vec::new();
                let mut seq = 0usize;

                for line in content.lines() {
                    let Ok(entry) = serde_json::from_str::<Value>(line) else {
                        continue;
                    };
                    match entry["type"].as_str().unwrap_or_default() {
                        "session" => {
                            if let Some(id) = entry["id"].as_str() {
                                session_id = id.to_string();
                            }
                        }
                        "session_info" => {
                            session_name = entry["name"].as_str().map(str::to_string);
                            seq += 1;
                        }
                        "message" => {
                            seq += 1;
                            let msg = &entry["message"];
                            if !msg.is_null() {
                                ui_messages.push(msg.clone());
                                if let Some(llm_item) = ui_message_to_llm(msg) {
                                    llm_history.push(llm_item);
                                }
                            }
                        }
                        _ => {
                            seq += 1;
                        }
                    }
                }
                return (
                    path,
                    session_id,
                    session_name,
                    ui_messages,
                    llm_history,
                    seq,
                );
            }
        }
    }

    let slug = format!(
        "--{}--",
        cwd.to_string_lossy()
            .trim_start_matches('/')
            .replace('/', "-")
    );
    let session_dir = config_dir.join("sessions").join(slug);
    let _ = std::fs::create_dir_all(&session_dir);
    let now_ms = epoch_ms();
    let session_id = format!("oopbot-{}", now_ms);
    let file_ts = iso_now().replace([':', '.'], "-");
    let file_path = session_dir.join(format!("{}_{}.jsonl", file_ts, session_id));
    (file_path, session_id, None, Vec::new(), Vec::new(), 0)
}

fn ui_message_to_llm(msg: &Value) -> Option<Value> {
    match msg["role"].as_str()? {
        "user" => {
            let mut text = String::new();
            let mut images = Vec::new();
            if let Some(s) = msg["content"].as_str() {
                text = s.to_string();
            } else if let Some(arr) = msg["content"].as_array() {
                let mut texts = Vec::new();
                for b in arr {
                    if let Some(t) = b["text"].as_str() {
                        texts.push(t);
                    } else if b["type"].as_str() == Some("image") {
                        let mime = b["mimeType"].as_str().unwrap_or("image/jpeg").to_string();
                        let data = b["data"].as_str().unwrap_or_default().to_string();
                        if !data.is_empty() {
                            images.push((mime, data));
                        }
                    }
                }
                text = texts.join("\n");
            }
            if text.is_empty() && images.is_empty() {
                None
            } else {
                Some(user_message_with_images(&text, &images))
            }
        }
        "assistant" => {
            let arr = msg["content"].as_array()?;
            let text = arr
                .iter()
                .filter(|b| b["type"] == "text")
                .filter_map(|b| b["text"].as_str())
                .collect::<Vec<_>>()
                .join("\n");
            if text.is_empty() {
                None
            } else {
                Some(json!({ "role": "model", "parts": [{ "text": text }] }))
            }
        }
        _ => None,
    }
}

fn default_rpc_system_instruction(cwd: &Path, tools: &ToolRegistry) -> String {
    format!(
        "Kamu adalah OOPBoT, mesin agen Rust native yang terhubung ke antarmuka Android Pipkin melalui Termux.\n\
         Direktori kerja (cwd): {}.\n\
         Tool yang tersedia: {}.\n\
         Gunakan tool untuk memeriksa isi file atau menjalankan perintah di Termux bila diperlukan.",
        cwd.display(),
        tools.names().join(", ")
    )
}

fn epoch_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

fn iso_now() -> String {
    let ms = epoch_ms();
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

/// Konversi jumlah hari sejak 1970-01-01 ke (year, month, day) (algoritma Howard Hinnant).
fn civil_from_days(z: i64) -> (i64, u32, u32) {
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn maps_tools_for_pipkin_timeline() {
        let (name, args) = map_tool_for_pipkin_ui("run_shell", &json!({ "command": "ls" }));
        assert_eq!(name, "bash");
        assert_eq!(args["command"], "ls");
    }

    #[test]
    fn formats_shell_exit_code_for_step_figure() {
        let val = json!({
            "stdout": "hello",
            "stderr": "err",
            "exit_code": 2
        });
        let (text, is_err) = format_tool_output("run_shell", &val);
        assert!(is_err);
        assert!(text.ends_with("Command exited with code 2"));
    }

    #[test]
    fn load_providers_applies_model_overrides_and_local_providers() {
        let tmp = std::env::temp_dir().join(format!("oopbot_rpc_test_{}", epoch_ms()));
        let _ = std::fs::create_dir_all(&tmp);
        std::fs::write(
            tmp.join("models.json"),
            serde_json::to_string_pretty(&json!({
                "providers": {
                    "google": {
                        "modelOverrides": {
                            "gemini-2.5-flash": {
                                "name": "Flash Override Dari Aplikasi",
                                "maxTokens": 8192,
                                "reasoning": false
                            }
                        }
                    },
                    "pipkin-local": {
                        "baseUrl": "http://127.0.0.1:8080/v1",
                        "api": "openai-completions",
                        "apiKey": "no-api-key",
                        "models": [
                            {
                                "id": "qwen3-4b",
                                "name": "Qwen3 4B Local",
                                "reasoning": true,
                                "contextWindow": 32768,
                                "maxTokens": 4096
                            }
                        ]
                    }
                }
            }))
            .unwrap(),
        )
        .unwrap();

        let cat = ModelCatalog::builtin();
        let providers = load_providers(&tmp, &cat);
        let google = providers.get("google").unwrap();
        let flash = google
            .models
            .iter()
            .find(|m| m.id == "gemini-2.5-flash")
            .unwrap();
        assert_eq!(flash.label, "Flash Override Dari Aplikasi");
        assert_eq!(flash.output_token_limit, Some(8192));
        assert!(!flash.reasoning);

        let local = providers.get("pipkin-local").unwrap();
        assert!(local.has_auth);
        assert_eq!(local.models.len(), 1);
        assert_eq!(local.models[0].id, "qwen3-4b");
        assert_eq!(local.models[0].input_token_limit, Some(32768));

        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn load_providers_uses_scanned_models_without_mixing_hardcoded_models() {
        let tmp = std::env::temp_dir().join(format!("oopbot_scan_test_{}", epoch_ms()));
        let _ = std::fs::create_dir_all(&tmp);

        // Simulasikan hasil scan live dari token pengguna yang hanya mendukung gemini-2.0-flash
        let scanned = vec![ModelInfo {
            id: "gemini-2.0-flash".to_string(),
            label: "Gemini 2.0 Flash (Scanned)".to_string(),
            provider: "google".to_string(),
            supports_tools: true,
            input_token_limit: Some(1_048_576),
            output_token_limit: Some(8192),
            description: "Scanned from token".to_string(),
            api: "google-generative-ai".to_string(),
            reasoning: true,
            thinking_level_map: None,
            input: vec!["text".to_string(), "image".to_string()],
            cost: None,
        }];

        save_scanned_models_to_config(&tmp, "google", &scanned, "test-api-key");

        let cat = ModelCatalog::builtin();
        let providers = load_providers(&tmp, &cat);
        let google = providers.get("google").unwrap();

        // Harus HANYA berisi model hasil scan dari token, TIDAK mencampur model hardcode bawaan!
        assert_eq!(google.models.len(), 1);
        assert_eq!(google.models[0].id, "gemini-2.0-flash");
        assert!(!google
            .models
            .iter()
            .any(|m| m.id == "gemini-3.5-flash-lite"));

        // Verifikasi scannedApiKey tersimpan
        let read_back = read_json_file(&tmp.join("models.json"));
        assert_eq!(
            read_back["providers"]["google"]["scannedApiKey"].as_str(),
            Some("test-api-key")
        );

        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn rpc_server_activates_plugins_from_extensions_arg() {
        let extensions = vec![
            "/data/user/0/io.jeph.pipkin/files/extensions/pipkin-todo/index.js".to_string(),
            "pipkin-web-access".to_string(),
        ];

        let server = RpcServer::new(None, None, extensions).unwrap();
        assert!(server.plugins.is_active("pipkin-todo"));
        assert!(server.plugins.is_active("pipkin-web-access"));
        assert!(!server.plugins.is_active("pipkin-background"));

        assert!(server.tools.get("todo").is_some());
        assert!(server.tools.get("web_search").is_some());
        assert!(server.tools.get("fetch_page").is_some());
        assert!(server.tools.get("background").is_none());
        assert!(server.system_instruction.contains("Todo & Task Management Guidelines"));
        assert!(server.system_instruction.contains("Web Access Guidelines"));
    }
}

