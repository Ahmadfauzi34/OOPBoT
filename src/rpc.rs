//! Mode RPC (`oopbot --mode rpc`) yang kompatibel 100% dengan protokol JSONL
//! aplikasi Android Pipkin (`AgentProcess.kt` & `ChatTimeline.kt`).
//!
//! Menggantikan `@earendil-works/pi-coding-agent` (`pi --mode rpc`) di Termux
//! dengan binary native Rust `oopbot`.

use crate::agent::{Agent, Approver, ChatInput, GeminiAgent};
use crate::events::{AgentEvent, EventSink, ToolStatus};
use crate::gemini::{
    user_message_with_images, BoxError, GeminiClient, DEFAULT_GEMINI_BASE_URL,
};
use crate::models::{
    builtin_providers, infer_api_from_provider, infer_input_from_id, infer_reasoning_from_id,
    resolve_model_scope, ModelCatalog, ModelInfo,
};
use crate::plugins::PluginRegistry;
use crate::session::WorkspaceTabs;
use crate::tools::ToolRegistry;
use async_trait::async_trait;
use serde_json::{json, Value};
use std::collections::HashMap;
use std::io::{self, BufRead, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use std::time::{SystemTime, UNIX_EPOCH};

type OutputSink = Arc<Mutex<Box<dyn Write + Send + Sync>>>;
static OUTPUT_SINK: RwLock<Option<OutputSink>> = RwLock::new(None);

pub fn set_output_sink(sink: Option<OutputSink>) {
    if let Ok(mut lock) = OUTPUT_SINK.write() {
        *lock = sink;
    }
}

#[cfg(test)]
pub(crate) fn output_sink_test_mutex() -> &'static tokio::sync::Mutex<()> {
    static LOCK: std::sync::OnceLock<tokio::sync::Mutex<()>> = std::sync::OnceLock::new();
    LOCK.get_or_init(|| tokio::sync::Mutex::new(()))
}

pub type PendingApprovals = Arc<Mutex<HashMap<String, tokio::sync::oneshot::Sender<bool>>>>;

/// Approver berbasis RPC yang berinteraksi secara asinkron dengan UI Pipkin Android
/// melalui protokol standar Pi `extension_ui_request` (`method: "confirm"`) dan `extension_ui_response`.
#[derive(Clone)]
pub struct RpcApprover {
    pending: PendingApprovals,
    counter: Arc<AtomicU64>,
    timeout: std::time::Duration,
    emitter: Option<Arc<dyn Fn(Value) + Send + Sync>>,
}

impl RpcApprover {
    pub fn new(pending: PendingApprovals) -> Self {
        Self {
            pending,
            counter: Arc::new(AtomicU64::new(1)),
            timeout: std::time::Duration::from_secs(300), // Default fail-closed setelah 5 menit
            emitter: None,
        }
    }

    pub fn with_timeout(mut self, timeout: std::time::Duration) -> Self {
        self.timeout = timeout;
        self
    }

    pub fn with_emitter<F>(mut self, emitter: F) -> Self
    where
        F: Fn(Value) + Send + Sync + 'static,
    {
        self.emitter = Some(Arc::new(emitter));
        self
    }
}

#[async_trait]
impl Approver for RpcApprover {
    async fn approve(&self, tool_name: &str, preview: &str) -> bool {
        let id_num = self.counter.fetch_add(1, Ordering::SeqCst);
        let req_id = format!("appr-{}", id_num);
        let (tx, rx) = tokio::sync::oneshot::channel::<bool>();

        {
            let mut pending = self.pending.lock().unwrap();
            pending.insert(req_id.clone(), tx);
        }

        let timeout_ms = self.timeout.as_millis() as u64;
        let event = json!({
            "type": "extension_ui_request",
            "id": req_id,
            "method": "confirm",
            "title": format!("Allow {}?", tool_name),
            "message": preview,
            "timeout": timeout_ms,
        });

        if let Some(ref em) = self.emitter {
            em(event);
        } else {
            RpcServer::send_line(&event);
        }

        match tokio::time::timeout(self.timeout, rx).await {
            Ok(Ok(approved)) => approved,
            Ok(Err(_)) => {
                // Sender dropped / disconnected -> fail-closed
                self.pending.lock().unwrap().remove(&req_id);
                false
            }
            Err(_) => {
                // Timeout -> fail-closed
                self.pending.lock().unwrap().remove(&req_id);
                false
            }
        }
    }
}

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
    providers: HashMap<String, ProviderEndpoint>,
    catalog: ModelCatalog,
    plugins: PluginRegistry,
    /// Loop agen tunggal (langkah 4 penyatuan loop): RpcServer tidak lagi
    /// memegang client/tools/llm_history sendiri — semuanya milik agen.
    agent: GeminiAgent,
    /// Status UI yang dibagikan dengan RpcEventSink: ui_messages, entry_seq,
    /// total_tokens, total_cost. Satu-satunya sumber kebenaran untuk ketiganya.
    ui_state: Arc<Mutex<RpcUiState>>,
    pub pending_approvals: PendingApprovals,
}

impl RpcServer {
    pub fn new(
        session_path: Option<String>,
        system_append: Option<String>,
        extensions: Vec<String>,
    ) -> Result<Self, BoxError> {
        let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
        let config_dir = std::env::var("OOPBOT_CONFIG_DIR")
            .or_else(|_| std::env::var("PI_CODING_AGENT_DIR"))
            .map(PathBuf::from)
            .unwrap_or_else(|_| {
                let home = std::env::var("HOME").unwrap_or_else(|_| ".".into());
                let oopbot_dir = PathBuf::from(&home).join(".oopbot");
                if oopbot_dir.exists() {
                    oopbot_dir
                } else {
                    PathBuf::from(home).join(".pipkin/pi")
                }
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

        let (session_file, session_id, session_name, ui_messages, llm_history, entry_seq, total_tokens, total_cost) =
            init_or_load_session(&config_dir, &cwd, session_path.as_deref());

        let pending_approvals: PendingApprovals = Arc::new(Mutex::new(HashMap::new()));

        // Status UI bersama dengan RpcEventSink (langkah 2), diisi dari sesi
        // yang dimuat — sink menulis ke sini, RpcServer membaca dari sini.
        let mut initial_state = RpcUiState::new(Some(session_file.clone()), entry_seq);
        initial_state.ui_messages = ui_messages;
        initial_state.total_tokens = total_tokens;
        initial_state.total_cost = total_cost;
        let ui_state = Arc::new(Mutex::new(initial_state));

        // Tab agen di-seed dari histori sesi (id tab = id sesi agar
        // GenerationOptions.session_id sama seperti sebelum langkah 4).
        // Tab TIDAK dipersist ke tabs.json CLI (tabs_path=None): store RPC
        // tetap file sesi JSONL.
        let mut tabs = WorkspaceTabs::new(&active_model);
        {
            let tab = tabs.active_mut();
            tab.id = session_id.clone();
            tab.model = active_model.clone();
            tab.history = llm_history;
            if let Some(name) = &session_name {
                tab.title = name.clone();
            }
        }

        let sink = RpcEventSink::new(Arc::clone(&ui_state));
        let approver = RpcApprover::new(Arc::clone(&pending_approvals));
        let mut agent = GeminiAgent::with_event_sink_and_tabs(
            "oopbot-rpc",
            client,
            tools,
            Box::new(approver),
            Box::new(sink),
            25, // sama dengan DEFAULT_MAX_STEPS (main.rs) — dulu literal di execute_prompt
            tabs,
            None,
        );
        agent.set_system_instruction(system_instruction);

        let mut server = Self {
            cwd,
            config_dir,
            session_file,
            session_id,
            session_name,
            active_provider,
            active_model,
            thinking_level: requested_thinking.clone(),
            providers,
            catalog,
            plugins,
            agent,
            ui_state,
            pending_approvals,
        };
        server.thinking_level = server
            .active_model_info()
            .clamp_thinking_level(&requested_thinking);
        // Selaraskan katalog + model aktif agen (api/limit/cost dari providers,
        // termasuk override models.json) dan level thinking.
        server.sync_agent_catalog();
        server.sync_agent_active_model();
        server.agent.set_thinking_level(&server.thinking_level);
        // Header file sesi ditulis di sini (dulu malas di append pertama).
        server.ensure_session_header();
        Ok(server)
    }

    /// Akses thread-safe ke map approval yang sedang menunggu konfirmasi UI.
    pub fn pending_approvals(&self) -> PendingApprovals {
        Arc::clone(&self.pending_approvals)
    }

    /// Selesaikan permintaan approval tool dengan id tertentu.
    /// Mengembalikan `true` bila request id ditemukan dan notifikasi terkirim, `false` jika tidak.
    pub fn resolve_tool_approval(&self, req_id: &str, approved: bool) -> bool {
        if let Some(sender) = self.pending_approvals.lock().unwrap().remove(req_id) {
            let _ = sender.send(approved);
            true
        } else {
            false
        }
    }

    /// Pancarkan event widget aktif (misal checklist Todo) dan status channel plugin ke antarmuka aplikasi Android via RPC JSONL.
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
        if self.plugins.is_active("pipkin-background") {
            Self::send_channel("background", json!({ "active": 0 }));
        }
        if self.plugins.is_active("pipkin-subagents") {
            Self::send_channel("subagents", json!({ "runs": [], "standingAside": false }));
        }
    }

    /// Kirim pesan dua arah pada Pipkin/OOPBoT channel: `{"type": "pipkin_channel", "topic": ..., "data": ...}`
    pub fn send_channel(topic: &str, data: Value) {
        Self::send_line(&json!({
            "type": "pipkin_channel",
            "topic": topic,
            "data": data,
        }));
    }

    pub fn send_line(value: &Value) {
        if let Ok(line) = serde_json::to_string(value) {
            if let Ok(lock) = OUTPUT_SINK.read() {
                if let Some(sink) = lock.as_ref() {
                    if let Ok(mut writer) = sink.lock() {
                        let _ = writeln!(writer, "{}", line);
                        let _ = writer.flush();
                        return;
                    }
                }
            }
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

    /// Salin seluruh entri katalog RPC ke katalog agen agar info model yang
    /// dipakai `chat_turn` (api, limit, thinking map, cost) sama dengan info
    /// RPC — termasuk override `models.json` dan model hasil scan provider.
    fn sync_agent_catalog(&self) {
        for m in self.catalog.list() {
            self.agent.upsert_model(m.clone());
        }
    }

    /// Selaraskan model aktif agen dengan `self.active_model`: upsert info
    /// lengkapnya (dari providers — membawa override `models.json`) lalu set
    /// model tab aktif agen (yang juga menyetel default model client).
    fn sync_agent_active_model(&self) {
        let info = self.active_model_info();
        self.agent.upsert_model(info);
        self.agent.set_active_model(&self.active_model);
    }

    /// Tulis header file sesi (`type: "session"`) bila file belum ada.
    /// Dulu header ditulis malas di append pertama; kini juga di new() agar
    /// file sesi baru langsung terbentuk lengkap.
    fn ensure_session_header(&self) {
        if self.session_file.exists() {
            return;
        }
        if let Some(parent) = self.session_file.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
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

    /// Tiga baris penutup giliran (dipakai jalur slash / tanpa-key / error —
    /// jalur model normal sudah dipancarkan sink dari TurnEnd).
    fn send_turn_end_lines() {
        Self::send_line(&json!({ "type": "turn_end" }));
        Self::send_line(&json!({ "type": "agent_end" }));
        Self::send_line(&json!({ "type": "agent_settled", "aborted": false }));
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

    fn append_session_entry(&mut self, entry_type: &str, payload: Value) {
        // Header sesi ditulis malas bila file sempat terhapus di tengah jalan.
        self.ensure_session_header();
        self.ui_state
            .lock()
            .unwrap()
            .append_session_entry(entry_type, payload);
    }

    fn refresh_credentials(&mut self) {
        self.providers = load_providers(&self.config_dir, &self.catalog);
        if let Some(ep) = self.providers.get(&self.active_provider) {
            self.agent
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
        self.agent.set_thinking_level(&self.thinking_level);
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
        if let Ok(scanned) = self.agent.scan_models().await {
            if !scanned.is_empty() {
                save_scanned_models_to_config(
                    &self.config_dir,
                    &self.active_provider,
                    &scanned,
                    &ep.api_key,
                );
                self.catalog.replace_with_scanned(scanned.clone());
                self.sync_agent_catalog();
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
                self.sync_agent_active_model();
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
                match self.agent.scan_models().await {
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
                        self.sync_agent_catalog();
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
                        self.sync_agent_active_model();
                        self.refresh_credentials();
                        Some(format!(
                            "Berhasil men-scan **{} model** dari `{}` dan menyimpannya ke `models.json`:\n{}\n\nGunakan pemilih model di atas atau ketik `/model <ID>` untuk mengganti model.",
                            count,
                            self.agent.base_url(),
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
                self.agent
                    .update_credentials(api_key.to_string(), base_url.clone());
                save_auth_key(
                    &self.config_dir,
                    &self.active_provider,
                    api_key,
                    base_url.as_deref(),
                );
                match self.agent.scan_models().await {
                    Ok(scanned) => {
                        let count = scanned.len();
                        if !scanned.iter().any(|m| m.id == self.active_model) {
                            if let Some(first) = scanned.first() {
                                self.active_model = first.id.clone();
                            }
                        }
                        self.catalog.replace_with_scanned(scanned.clone());
                        self.sync_agent_catalog();
                        self.sync_agent_active_model();
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
                self.sync_agent_active_model();
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
                self.agent.set_thinking_level(&clamped);
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
                    self.sync_agent_active_model();
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
                // Override maxTokens kini tercermin di active_model_info (via
                // providers) — selaraskan katalog agen agar chat_turn memakainya.
                self.sync_agent_active_model();
                Some(format!(
                    "`maxTokens` untuk `{}/{}` berhasil disimpan ke `{}`.",
                    self.active_provider, self.active_model, max_tok
                ))
            }
            "/clear" => {
                {
                    let mut state = self.ui_state.lock().unwrap();
                    state.ui_messages.clear();
                }
                self.agent.clear_memory_silent();
                Some("Memori percakapan sesi ini telah dibersihkan.".to_string())
            }
            _ => None,
        }
    }

    /// Adapter tipis di atas `GeminiAgent::chat_turn` (langkah 4 penyatuan loop).
    ///
    /// Tanggung jawab yang TINGGAL di sini: pesan user, slash command, cek auth,
    /// akuntansi token/biaya, session_stats. Loop model → tool → model, pemancaran
    /// event JSONL, dan histori model sepenuhnya milik agen (via RpcEventSink).
    /// `agent_start` dipancarkan sink dari TurnStart — urutannya sesudah pesan
    /// user (dulu sebelum); ChatTimeline hanya memakai agent_start untuk
    /// membuka footer run, jadi aman.
    async fn execute_prompt(&mut self, prompt: &str, images: &[(String, String)]) {
        self.refresh_credentials();
        self.sync_thinking_level_for_active_model();
        let started_ms = epoch_ms();

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
        {
            let mut state = self.ui_state.lock().unwrap();
            state.ui_messages.push(user_msg.clone());
        }
        self.append_session_entry("message", json!({ "message": user_msg }));

        // Cek apakah perintah slash internal (/scan, /token, /models, /model, /thinking, /provider, /max-tokens, /clear)
        if prompt.trim_start().starts_with('/') && images.is_empty() {
            if let Some(reply_text) = self.handle_slash_command(prompt.trim()).await {
                self.emit_assistant_reply_with_thoughts(&reply_text, &[], started_ms, None);
                Self::send_turn_end_lines();
                return;
            }
        }

        let provider_has_auth = self
            .providers
            .get(&self.active_provider)
            .map(|p| p.has_auth)
            .unwrap_or(false);
        if !provider_has_auth && self.agent.api_key().trim().is_empty() {
            let msg = "API key belum diatur. Silakan buka **Settings › Providers & models** di OOPBoT untuk menempelkan API key (Google Gemini / OpenRouter / Custom), atau ketik langsung di sini:\n\n`/token <API_KEY>`";
            self.emit_assistant_reply_with_thoughts(msg, &[], started_ms, None);
            Self::send_turn_end_lines();
            return;
        }

        let input = ChatInput::with_images(prompt.to_string(), images.to_vec());
        match self.agent.chat_turn(&input).await {
            Ok(outcome) => {
                // Akuntansi dari agregat turn. PERUBAHAN vs loop lama: total_tokens
                // dulu DITIMPA per langkah generate (hanya langkah terakhir yang
                // tersisa); kini diakumulasi seperti cabang estimasi dan seperti
                // arti field-nya (contextUsage di get_session_stats).
                let active_info = self.active_model_info();
                let (turn_in, turn_out) = if let Some(u) = outcome.usage {
                    (u.input_tokens, u.output_tokens)
                } else {
                    (
                        (prompt.len() as u64 / 4) + 60,
                        (outcome.text.len() as u64 / 4) + 60,
                    )
                };
                {
                    let mut state = self.ui_state.lock().unwrap();
                    state.total_tokens += turn_in + turn_out;
                    if let Some(cost) = &active_info.cost {
                        state.total_cost += (turn_in as f64 / 1_000_000.0) * cost.input
                            + (turn_out as f64 / 1_000_000.0) * cost.output;
                    }
                }

                self.emit_active_widgets();

                // Simpan total token & cost sesi secara permanen ke file sesi JSONL.
                // turn_end/agent_end/agent_settled SUDAH dipancarkan sink dari
                // TurnEnd — jangan kirim ulang.
                let (total_tokens, total_cost) = {
                    let state = self.ui_state.lock().unwrap();
                    (state.total_tokens, state.total_cost)
                };
                self.append_session_entry(
                    "session_stats",
                    json!({
                        "total_tokens": total_tokens,
                        "total_cost": total_cost
                    }),
                );
            }
            Err(err) => {
                // chat_turn mengembalikan Err bila generate gagal — sink tidak
                // pernah melihat TurnEnd, jadi epilog ditangani di sini persis
                // seperti jalur error loop lama.
                let err_msg = json!({
                    "role": "assistant",
                    "content": [],
                    "stopReason": "error",
                    "errorMessage": err.to_string(),
                    "timestamp": started_ms,
                    "durationMs": epoch_ms().saturating_sub(started_ms),
                });
                Self::send_line(&json!({ "type": "message_start", "message": err_msg }));
                Self::send_line(&json!({ "type": "message_end", "message": err_msg }));
                {
                    let mut state = self.ui_state.lock().unwrap();
                    state.ui_messages.push(err_msg.clone());
                }
                self.append_session_entry("message", json!({ "message": err_msg }));

                let (total_tokens, total_cost) = {
                    let state = self.ui_state.lock().unwrap();
                    (state.total_tokens, state.total_cost)
                };
                self.append_session_entry(
                    "session_stats",
                    json!({
                        "total_tokens": total_tokens,
                        "total_cost": total_cost
                    }),
                );
                Self::send_turn_end_lines();
            }
        }
    }

    fn emit_assistant_reply_with_thoughts(
        &mut self,
        text: &str,
        thoughts: &[String],
        started_ms: u64,
        usage: Option<(u64, u64)>,
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

        let mut msg = json!({
            "role": "assistant",
            "content": content_blocks,
            "stopReason": "stop",
            "timestamp": started_ms,
            "durationMs": now_ms.saturating_sub(started_ms),
        });
        if let Some((turn_in, turn_out)) = usage {
            msg["usage"] = json!({
                "input": turn_in,
                "output": turn_out,
                "totalTokens": turn_in + turn_out
            });
        }
        Self::send_line(&json!({ "type": "message_end", "message": msg }));
        {
            let mut state = self.ui_state.lock().unwrap();
            state.ui_messages.push(msg.clone());
        }
        self.append_session_entry("message", json!({ "message": msg }));
    }

    pub async fn run_loop(&mut self) -> Result<(), BoxError> {
        let stdin = io::stdin();
        let reader = io::BufReader::new(stdin);
        self.run_loop_with_input(reader).await
    }

    pub async fn run_loop_with_input<R: BufRead + Send + 'static>(
        &mut self,
        mut reader: R,
    ) -> Result<(), BoxError> {
        // Handshake awal Pipkin Channel (mengumumkan koneksi host siap ke ekstensi/UI)
        Self::send_channel("pipkin", Value::Null);
        Self::send_channel(
            "pipkin:hello",
            json!({
                "version": 1,
                "engine": "oopbot-native",
                "oopbotVersion": env!("CARGO_PKG_VERSION")
            }),
        );

        self.emit_active_widgets();

        let pending = Arc::clone(&self.pending_approvals);
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<String>();

        let _reader_thread = std::thread::spawn(move || {
            let mut line = String::new();
            loop {
                line.clear();
                match reader.read_line(&mut line) {
                    Ok(0) => {
                        pending.lock().unwrap().clear();
                        break;
                    }
                    Ok(_) => {
                        let trimmed = line.trim_end_matches(&['\r', '\n'][..]);
                        if trimmed.is_empty() {
                            continue;
                        }
                        // Rutekan respons konfirmasi (`extension_ui_response` & legacy `tool_approval`)
                        // langsung ke waiter oneshot untuk mencegah deadlock saat loop menunggu prompt / tool.
                        if let Ok(val) = serde_json::from_str::<Value>(trimmed) {
                            let msg_type = val["type"].as_str().unwrap_or_default();
                            if msg_type == "extension_ui_response" {
                                let req_id = val["id"].as_str().unwrap_or_default();
                                let approved = if val["cancelled"].as_bool() == Some(true) {
                                    false
                                } else {
                                    val["confirmed"].as_bool().unwrap_or(false)
                                };
                                if !req_id.is_empty() {
                                    if let Some(sender) = pending.lock().unwrap().remove(req_id) {
                                        let _ = sender.send(approved);
                                    }
                                }
                                continue;
                            }
                            let is_channel =
                                msg_type == "pipkin_channel" || msg_type == "oopbot_channel";
                            if is_channel && val["topic"] == "tool_approval" {
                                let data = &val["data"];
                                let req_id = data["request_id"]
                                    .as_str()
                                    .or_else(|| data["requestId"].as_str())
                                    .unwrap_or_default();
                                let approved = data["approved"].as_bool().unwrap_or(false);
                                if !req_id.is_empty() {
                                    if let Some(sender) = pending.lock().unwrap().remove(req_id) {
                                        let _ = sender.send(approved);
                                    }
                                }
                                continue;
                            }
                        }
                        if tx.send(line.clone()).is_err() {
                            break;
                        }
                    }
                    Err(_) => {
                        pending.lock().unwrap().clear();
                        break;
                    }
                }
            }
        });

        while let Some(line) = rx.recv().await {
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
                "pipkin_channel" | "oopbot_channel" => {
                    let topic = cmd["topic"].as_str().unwrap_or_default();
                    let data = &cmd["data"];
                    self.handle_channel_message(topic, data).await;
                }
                "get_state" => {
                    self.refresh_credentials();
                    self.maybe_auto_scan().await;
                    Self::send_response(&id, "get_state", self.state_json());
                }
                "get_messages" => {
                    let messages = self.ui_state.lock().unwrap().ui_messages.clone();
                    Self::send_response(
                        &id,
                        "get_messages",
                        json!({ "messages": messages }),
                    );
                }
                "get_session_stats" => {
                    let window = self
                        .active_model_info()
                        .input_token_limit
                        .unwrap_or(1_048_576)
                        .max(1);
                    let (total_tokens, total_cost) = {
                        let state = self.ui_state.lock().unwrap();
                        (state.total_tokens, state.total_cost)
                    };
                    let pct = (total_tokens as f64 / window as f64) * 100.0;
                    Self::send_response(
                        &id,
                        "get_session_stats",
                        json!({
                            "contextUsage": {
                                "tokens": total_tokens,
                                "contextWindow": window,
                                "percent": pct
                            },
                            "cost": total_cost
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
                    self.sync_agent_active_model();
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
                        self.agent.set_thinking_level(&clamped);
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
                        .ui_state
                        .lock()
                        .unwrap()
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
                "extension_ui_response" => {}
                other => {
                    if !id.is_empty() {
                        Self::send_error(&id, other, &format!("Command '{}' tidak dikenal", other));
                    }
                }
            }
        }
        Ok(())
    }

    /// Tangani pesan event masuk dari Android UI atau ekstensi via Pipkin Channel
    async fn handle_channel_message(&mut self, topic: &str, data: &Value) {
        match topic {
            "tool_approval" => {
                let req_id = data["request_id"]
                    .as_str()
                    .or_else(|| data["requestId"].as_str())
                    .unwrap_or_default();
                let approved = data["approved"].as_bool().unwrap_or(false);
                if !req_id.is_empty() {
                    self.resolve_tool_approval(req_id, approved);
                }
            }
            "background" => {
                if let Some(action) = data["action"].as_str() {
                    if action == "stop" {
                        Self::send_channel("background", json!({ "active": 0 }));
                    }
                }
            }
            "subagents" => {
                if let Some(action) = data["action"].as_str() {
                    match action {
                        "stop" | "stopAll" => {
                            Self::send_channel("subagents", json!({
                                "runs": [],
                                "standingAside": false
                            }));
                        }
                        "check" => {
                            Self::send_channel("subagents", json!({
                                "runs": [],
                                "standingAside": false
                            }));
                        }
                        _ => {}
                    }
                }
            }
            "remote" => {
                if let Some(action) = data["action"].as_str() {
                    if action == "check" {
                        Self::send_channel("remote", json!({ "connected": true }));
                    }
                }
            }
            _ => {
                // Topic channel lainnya ditangani dengan aman tanpa menimbulkan error
            }
        }
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

/// Status UI RPC yang dibagikan antara RpcServer dan RpcEventSink.
#[derive(Debug, Clone, Default)]
pub struct RpcUiState {
    pub ui_messages: Vec<Value>,
    pub session_file: Option<PathBuf>,
    pub entry_seq: usize,
    pub total_tokens: u64,
    pub total_cost: f64,
}

impl RpcUiState {
    pub fn new(session_file: Option<PathBuf>, entry_seq: usize) -> Self {
        Self {
            ui_messages: Vec::new(),
            session_file,
            entry_seq,
            total_tokens: 0,
            total_cost: 0.0,
        }
    }

    pub fn append_session_entry(&mut self, entry_type: &str, mut payload: Value) {
        let Some(path) = &self.session_file else {
            return;
        };
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
            if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
                let _ = std::fs::create_dir_all(parent);
            }
            if let Ok(mut file) = std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(path)
            {
                let _ = writeln!(file, "{}", line);
            }
        }
    }
}

/// Status internal pelacakan langkah pesan asisten untuk satu giliran obrolan.
#[derive(Debug, Default)]
struct RpcEventSinkInternal {
    step_start_ms: u64,
    current_content_idx: usize,
    content_blocks: Vec<Value>,
    assistant_msg_started: bool,
    pending_tool_calls: HashMap<String, (String, Value)>,
}

/// Sink event yang menerjemahkan deretan [`AgentEvent`] dari loop agen menjadi
/// baris-baris JSONL protokol Pipkin (`agent_start`, `message_start`, `message_update`,
/// `tool_execution_start/end`, `toolResult`, `turn_end`, dll.).
pub struct RpcEventSink {
    ui_state: Arc<Mutex<RpcUiState>>,
    internal: Mutex<RpcEventSinkInternal>,
    emitter: Option<Box<dyn Fn(&Value) + Send + Sync>>,
}

impl RpcEventSink {
    pub fn new(ui_state: Arc<Mutex<RpcUiState>>) -> Self {
        Self {
            ui_state,
            internal: Mutex::new(RpcEventSinkInternal::default()),
            emitter: None,
        }
    }

    pub fn with_emitter(
        ui_state: Arc<Mutex<RpcUiState>>,
        emitter: impl Fn(&Value) + Send + Sync + 'static,
    ) -> Self {
        Self {
            ui_state,
            internal: Mutex::new(RpcEventSinkInternal::default()),
            emitter: Some(Box::new(emitter)),
        }
    }

    fn emit(&self, val: &Value) {
        if let Some(emitter) = &self.emitter {
            emitter(val);
        } else {
            RpcServer::send_line(val);
        }
    }
}

impl EventSink for RpcEventSink {
    fn on_event(&self, event: &AgentEvent) {
        match event {
            AgentEvent::TurnStart {
                session_id: _,
                model: _,
            } => {
                let mut internal = self.internal.lock().unwrap();
                internal.step_start_ms = epoch_ms();
                internal.current_content_idx = 0;
                internal.content_blocks.clear();
                internal.assistant_msg_started = false;
                internal.pending_tool_calls.clear();

                self.emit(&json!({ "type": "agent_start" }));
            }
            AgentEvent::Thinking {
                step_index: _,
                text,
            } => {
                if text.trim().is_empty() {
                    return;
                }
                let mut internal = self.internal.lock().unwrap();
                if !internal.assistant_msg_started {
                    self.emit(&json!({
                        "type": "message_start",
                        "message": {
                            "role": "assistant",
                            "content": [],
                            "timestamp": internal.step_start_ms
                        }
                    }));
                    internal.assistant_msg_started = true;
                }
                let idx = internal.current_content_idx;
                self.emit(&json!({
                    "type": "message_update",
                    "assistantMessageEvent": {
                        "type": "thinking_start",
                        "contentIndex": idx
                    }
                }));
                self.emit(&json!({
                    "type": "message_update",
                    "assistantMessageEvent": {
                        "type": "thinking_end",
                        "contentIndex": idx,
                        "content": text
                    }
                }));
                internal.content_blocks.push(json!({
                    "type": "thinking",
                    "thinking": text
                }));
                internal.current_content_idx += 1;
            }
            AgentEvent::ToolCallStart {
                step_index: _,
                call_id,
                tool_name,
                args,
                preview: _,
                requires_confirmation: _,
            } => {
                let (ui_tool_name, ui_args) = map_tool_for_pipkin_ui(tool_name, args);
                let mut internal = self.internal.lock().unwrap();
                if !internal.assistant_msg_started {
                    self.emit(&json!({
                        "type": "message_start",
                        "message": {
                            "role": "assistant",
                            "content": [],
                            "timestamp": internal.step_start_ms
                        }
                    }));
                    internal.assistant_msg_started = true;
                }
                let idx = internal.current_content_idx;
                self.emit(&json!({
                    "type": "message_update",
                    "assistantMessageEvent": {
                        "type": "toolcall_start",
                        "contentIndex": idx,
                        "id": call_id,
                        "toolName": ui_tool_name
                    }
                }));
                self.emit(&json!({
                    "type": "message_update",
                    "assistantMessageEvent": {
                        "type": "toolcall_end",
                        "contentIndex": idx,
                        "toolCall": {
                            "id": call_id,
                            "name": ui_tool_name,
                            "arguments": ui_args
                        }
                    }
                }));
                internal.content_blocks.push(json!({
                    "type": "toolCall",
                    "id": call_id,
                    "name": ui_tool_name,
                    "arguments": ui_args
                }));
                internal.current_content_idx += 1;
                internal
                    .pending_tool_calls
                    .insert(call_id.clone(), (ui_tool_name.clone(), ui_args.clone()));

                self.emit(&json!({
                    "type": "tool_execution_start",
                    "toolCallId": call_id,
                    "toolName": ui_tool_name,
                    "args": ui_args
                }));
            }
            AgentEvent::ToolCallEnd {
                step_index: _,
                call_id,
                tool_name,
                status,
                output,
                duration_ms: _,
            } => {
                let mut internal = self.internal.lock().unwrap();
                if internal.assistant_msg_started {
                    let assistant_msg = json!({
                        "role": "assistant",
                        "content": internal.content_blocks.clone(),
                        "stopReason": "toolUse",
                        "timestamp": internal.step_start_ms,
                        "durationMs": epoch_ms().saturating_sub(internal.step_start_ms),
                    });
                    self.emit(&json!({
                        "type": "message_end",
                        "message": assistant_msg
                    }));
                    {
                        let mut state = self.ui_state.lock().unwrap();
                        state.ui_messages.push(assistant_msg.clone());
                        state.append_session_entry("message", json!({ "message": assistant_msg }));
                    }
                    internal.assistant_msg_started = false;
                    internal.content_blocks.clear();
                    internal.current_content_idx = 0;
                }

                let (ui_tool_name, _) = internal
                    .pending_tool_calls
                    .remove(call_id)
                    .unwrap_or_else(|| map_tool_for_pipkin_ui(tool_name, &Value::Null));

                let (display_text, is_error) = match status {
                    ToolStatus::Rejected => {
                        let msg = output["error"]
                            .as_str()
                            .unwrap_or("User menolak eksekusi tool ini. Tanyakan alternatif ke user.")
                            .to_string();
                        (msg, true)
                    }
                    ToolStatus::Error => {
                        let msg = output["error"].as_str().unwrap_or("error").to_string();
                        (msg, true)
                    }
                    ToolStatus::Done => {
                        let (text, is_err) = format_tool_output(tool_name, output);
                        (text, is_err)
                    }
                    _ => {
                        let (text, is_err) = format_tool_output(tool_name, output);
                        (text, is_err)
                    }
                };

                self.emit(&json!({
                    "type": "tool_execution_end",
                    "toolCallId": call_id,
                    "toolName": ui_tool_name,
                    "isError": is_error,
                    "result": {
                        "content": [{ "type": "text", "text": display_text }]
                    }
                }));

                let tool_result_msg = json!({
                    "role": "toolResult",
                    "toolCallId": call_id,
                    "toolName": ui_tool_name,
                    "isError": is_error,
                    "content": [{ "type": "text", "text": display_text }],
                    "timestamp": epoch_ms()
                });

                self.emit(&json!({
                    "type": "message_end",
                    "message": tool_result_msg
                }));

                {
                    let mut state = self.ui_state.lock().unwrap();
                    state.ui_messages.push(tool_result_msg.clone());
                    state.append_session_entry("message", json!({ "message": tool_result_msg }));
                }
            }
            AgentEvent::AssistantText { text } => {
                let mut internal = self.internal.lock().unwrap();
                if !internal.assistant_msg_started {
                    self.emit(&json!({
                        "type": "message_start",
                        "message": {
                            "role": "assistant",
                            "content": [],
                            "timestamp": internal.step_start_ms
                        }
                    }));
                    internal.assistant_msg_started = true;
                }

                let idx = internal.current_content_idx;
                self.emit(&json!({
                    "type": "message_update",
                    "assistantMessageEvent": {
                        "type": "text_start",
                        "contentIndex": idx
                    }
                }));
                self.emit(&json!({
                    "type": "message_update",
                    "assistantMessageEvent": {
                        "type": "text_delta",
                        "contentIndex": idx,
                        "delta": text
                    }
                }));
                self.emit(&json!({
                    "type": "message_update",
                    "assistantMessageEvent": {
                        "type": "text_end",
                        "contentIndex": idx,
                        "content": text
                    }
                }));

                internal.content_blocks.push(json!({
                    "type": "text",
                    "text": text
                }));
                internal.current_content_idx += 1;

                let assistant_msg = json!({
                    "role": "assistant",
                    "content": internal.content_blocks.clone(),
                    "stopReason": "stop",
                    "timestamp": internal.step_start_ms,
                    "durationMs": epoch_ms().saturating_sub(internal.step_start_ms),
                });

                self.emit(&json!({
                    "type": "message_end",
                    "message": assistant_msg
                }));

                {
                    let mut state = self.ui_state.lock().unwrap();
                    state.ui_messages.push(assistant_msg.clone());
                    state.append_session_entry("message", json!({ "message": assistant_msg }));
                }

                internal.assistant_msg_started = false;
                internal.content_blocks.clear();
                internal.current_content_idx = 0;
            }
            AgentEvent::TurnEnd {
                steps_count: _,
                duration_ms: _,
                outcome: _,
            } => {
                self.emit(&json!({ "type": "turn_end" }));
                self.emit(&json!({ "type": "agent_end" }));
                self.emit(&json!({ "type": "agent_settled", "aborted": false }));
            }
        }
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
    if let Ok(content) = std::fs::read_to_string(path) {
        if let Ok(val) = serde_json::from_str(&content) {
            return val;
        }
    }
    json!({})
}


fn init_or_load_session(
    config_dir: &Path,
    cwd: &Path,
    session_path: Option<&str>,
) -> (PathBuf, String, Option<String>, Vec<Value>, Vec<Value>, usize, u64, f64) {
    if let Some(path_str) = session_path.filter(|s| !s.trim().is_empty()) {
        let path = PathBuf::from(path_str);
        if path.exists() {
            if let Ok(content) = std::fs::read_to_string(&path) {
                let mut session_id = format!("s{}", epoch_ms());
                let mut session_name = None;
                let mut ui_messages = Vec::new();
                let mut llm_history = Vec::new();
                let mut seq = 0usize;
                let mut total_tokens = 0u64;
                let mut total_cost = 0.0f64;

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
                        "session_stats" => {
                            seq += 1;
                            if let Some(t) = entry["total_tokens"].as_u64() {
                                total_tokens = t;
                            }
                            if let Some(c) = entry["total_cost"].as_f64() {
                                total_cost = c;
                            }
                        }
                        "message" => {
                            seq += 1;
                            let msg = &entry["message"];
                            if !msg.is_null() {
                                ui_messages.push(msg.clone());
                                if let Some(llm_item) = ui_message_to_llm(msg) {
                                    llm_history.push(llm_item);
                                }

                                if msg["role"].as_str() == Some("assistant") {
                                    if let Some(usage) = msg.get("usage") {
                                        if let Some(t) = usage["totalTokens"].as_u64().or_else(|| {
                                            let in_t = usage["input"].as_u64().unwrap_or(0);
                                            let out_t = usage["output"].as_u64().unwrap_or(0);
                                            if in_t + out_t > 0 {
                                                Some(in_t + out_t)
                                            } else {
                                                None
                                            }
                                        }) {
                                            total_tokens = t;
                                        }
                                    }
                                    if let Some(cost) = msg
                                        .get("cost")
                                        .and_then(|c| c["total"].as_f64())
                                        .or_else(|| msg["cost"].as_f64())
                                    {
                                        total_cost += cost;
                                    }
                                }
                            }
                        }
                        _ => {
                            seq += 1;
                        }
                    }
                }

                // Fallback estimasi cerdas jika session lama belum menyimpan field usage/stats
                if total_tokens == 0 && !ui_messages.is_empty() {
                    let mut total_chars = 0usize;
                    for msg in &ui_messages {
                        if let Some(content) = msg["content"].as_str() {
                            total_chars += content.len();
                        } else if let Some(arr) = msg["content"].as_array() {
                            for block in arr {
                                if let Some(t) = block["text"].as_str() {
                                    total_chars += t.len();
                                } else if let Some(th) = block["thinking"].as_str() {
                                    total_chars += th.len();
                                }
                            }
                        }
                    }
                    total_tokens = (total_chars as u64 / 4) + 500;
                }

                return (
                    path,
                    session_id,
                    session_name,
                    ui_messages,
                    llm_history,
                    seq,
                    total_tokens,
                    total_cost,
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
    (file_path, session_id, None, Vec::new(), Vec::new(), 0, 0, 0.0)
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
    use std::io::Cursor;

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

        // Tool & system instruction kini milik agen (langkah 4) — intent tes
        // sama: new() mengaktifkan plugin dan merakit instruksi RPC-nya.
        let tool_names = server.agent.tool_names();
        assert!(tool_names.contains(&"todo"));
        assert!(tool_names.contains(&"web_search"));
        assert!(tool_names.contains(&"fetch_page"));
        assert!(!tool_names.contains(&"background"));
        assert!(server
            .agent
            .system_instruction()
            .contains("Todo & Task Management Guidelines"));
        assert!(server
            .agent
            .system_instruction()
            .contains("Web Access Guidelines"));
    }

    #[test]
    fn session_preserves_tokens_and_cost_on_reload() {
        let tmp = std::env::temp_dir().join(format!("oopbot_session_stats_{}", epoch_ms()));
        let _ = std::fs::create_dir_all(&tmp);
        let session_file = tmp.join("test_session.jsonl");

        let lines = vec![
            json!({
                "type": "session",
                "id": "s-12345",
                "timestamp": "2026-11-09T08:00:00Z",
                "cwd": "/home/user"
            })
            .to_string(),
            json!({
                "type": "message",
                "id": "e0",
                "message": {
                    "role": "user",
                    "content": "Halo apa kabar?"
                }
            })
            .to_string(),
            json!({
                "type": "message",
                "id": "e1",
                "message": {
                    "role": "assistant",
                    "content": [{ "type": "text", "text": "Kabar baik!" }],
                    "usage": {
                        "input": 1250,
                        "output": 350,
                        "totalTokens": 1600
                    },
                    "cost": 0.0025
                }
            })
            .to_string(),
            json!({
                "type": "session_stats",
                "id": "e2",
                "total_tokens": 1600,
                "total_cost": 0.0025
            })
            .to_string(),
        ];
        std::fs::write(&session_file, lines.join("\n")).unwrap();

        let server = RpcServer::new(
            Some(session_file.to_string_lossy().to_string()),
            None,
            vec![],
        )
        .unwrap();
        // Total kini hidup di ui_state bersama (langkah 4) — intent tes sama:
        // reload sesi merekonstruksi total token & biaya dari file sesi.
        let (total_tokens, total_cost) = {
            let state = server.ui_state.lock().unwrap();
            (state.total_tokens, state.total_cost)
        };
        assert_eq!(total_tokens, 1600);
        assert!((total_cost - 0.0025).abs() < 1e-6);

        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[tokio::test]
    async fn test_pipkin_channel_handshake_and_incoming_events() {
        use std::io::Cursor;
        let _sink_guard = output_sink_test_mutex().lock().await;

        let raw = Arc::new(Mutex::new(Vec::new()));
        struct TestBuf(Arc<Mutex<Vec<u8>>>);
        impl io::Write for TestBuf {
            fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
                self.0.lock().unwrap().write(buf)
            }
            fn flush(&mut self) -> io::Result<()> {
                self.0.lock().unwrap().flush()
            }
        }

        let output_buf = Arc::new(Mutex::new(Box::new(TestBuf(raw.clone())) as Box<dyn io::Write + Send + Sync>));
        set_output_sink(Some(output_buf));

        let extensions = vec![
            "pipkin-background".to_string(),
            "pipkin-subagents".to_string(),
        ];
        let mut server = RpcServer::new(None, None, extensions).unwrap();

        let input = b"\
{\"type\":\"pipkin_channel\",\"topic\":\"background\",\"data\":{\"action\":\"stop\"}}\n\
{\"type\":\"pipkin_channel\",\"topic\":\"subagents\",\"data\":{\"action\":\"stopAll\"}}\n\
{\"type\":\"pipkin_channel\",\"topic\":\"remote\",\"data\":{\"action\":\"check\"}}\n\
{\"type\":\"oopbot_channel\",\"topic\":\"remote\",\"data\":{\"action\":\"check\"}}\n\
{\"type\":\"get_state\",\"id\":\"chk_1\"}\n";
        let cursor = Cursor::new(input);

        let res = server.run_loop_with_input(cursor).await;
        assert!(res.is_ok());

        set_output_sink(None);

        let bytes = raw.lock().unwrap().clone();
        let written = String::from_utf8_lossy(&bytes);

        // Verifikasi handshake awal pipkin & pipkin:hello
        assert!(written.contains("\"topic\":\"pipkin\""));
        assert!(written.contains("\"topic\":\"pipkin:hello\""));
        assert!(written.contains("\"engine\":\"oopbot-native\""));

        // Verifikasi channel background, subagents, dan remote
        assert!(written.contains("\"topic\":\"background\""));
        assert!(written.contains("\"active\":0"));
        assert!(written.contains("\"topic\":\"subagents\""));
        assert!(written.contains("\"topic\":\"remote\""));
        assert!(written.contains("\"connected\":true"));

        // Verifikasi respon rpc reguler
        assert!(written.contains("chk_1"));
        assert!(written.contains("\"success\":true"));

        // Pastikan tidak ada error komplain pipkin_channel
        assert!(!written.contains("tidak dikenal"));
    }

    #[test]
    fn test_rpc_event_sink_text_turn() {
        use crate::events::StepsOutcome;

        let ui_state = Arc::new(Mutex::new(RpcUiState::default()));
        let emitted = Arc::new(Mutex::new(Vec::new()));
        let emitted_clone = emitted.clone();
        let sink = RpcEventSink::with_emitter(ui_state.clone(), move |val| {
            emitted_clone.lock().unwrap().push(val.clone());
        });

        sink.on_event(&AgentEvent::TurnStart {
            session_id: "s1".to_string(),
            model: "gemini-2.5-flash".to_string(),
        });
        sink.on_event(&AgentEvent::Thinking {
            step_index: 1,
            text: "Sedang menganalisis pertanyaan".to_string(),
        });
        sink.on_event(&AgentEvent::AssistantText {
            text: "Halo! Ada yang bisa saya bantu?".to_string(),
        });
        sink.on_event(&AgentEvent::TurnEnd {
            steps_count: 0,
            duration_ms: 120,
            outcome: StepsOutcome::Done,
        });

        let lines = emitted.lock().unwrap().clone();
        let types: Vec<&str> = lines.iter().filter_map(|l| l["type"].as_str()).collect();

        assert_eq!(types[0], "agent_start");
        assert_eq!(types[1], "message_start");
        assert_eq!(types[2], "message_update"); // thinking_start
        assert_eq!(types[3], "message_update"); // thinking_end
        assert_eq!(types[4], "message_update"); // text_start
        assert_eq!(types[5], "message_update"); // text_delta
        assert_eq!(types[6], "message_update"); // text_end
        assert_eq!(types[7], "message_end");
        assert_eq!(types[8], "turn_end");
        assert_eq!(types[9], "agent_end");
        assert_eq!(types[10], "agent_settled");

        let state = ui_state.lock().unwrap();
        assert_eq!(state.ui_messages.len(), 1);
        assert_eq!(state.ui_messages[0]["role"], "assistant");
        assert_eq!(state.ui_messages[0]["stopReason"], "stop");
    }

    #[test]
    fn test_rpc_event_sink_tool_turn_and_rejection() {
        use crate::events::StepsOutcome;

        let tmp = std::env::temp_dir().join(format!("oopbot-sink-test-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&tmp);
        let session_file = tmp.join("test_session.jsonl");
        let ui_state = Arc::new(Mutex::new(RpcUiState::new(Some(session_file.clone()), 0)));

        let emitted = Arc::new(Mutex::new(Vec::new()));
        let emitted_clone = emitted.clone();
        let sink = RpcEventSink::with_emitter(ui_state.clone(), move |val| {
            emitted_clone.lock().unwrap().push(val.clone());
        });

        sink.on_event(&AgentEvent::TurnStart {
            session_id: "s2".to_string(),
            model: "gemini-2.5-flash".to_string(),
        });

        // 1. Tool Call biasa (run_shell -> dipetakan ke "bash")
        sink.on_event(&AgentEvent::ToolCallStart {
            step_index: 1,
            call_id: "call_1".to_string(),
            tool_name: "run_shell".to_string(),
            args: json!({ "command": "echo test" }),
            preview: "$ echo test".to_string(),
            requires_confirmation: false,
        });

        sink.on_event(&AgentEvent::ToolCallEnd {
            step_index: 1,
            call_id: "call_1".to_string(),
            tool_name: "run_shell".to_string(),
            status: ToolStatus::Done,
            output: json!({ "exit_code": 0, "stdout": "test\n", "stderr": "" }),
            duration_ms: 10,
        });

        // 2. Tool Call kedua yang ditolak user
        sink.on_event(&AgentEvent::ToolCallStart {
            step_index: 2,
            call_id: "call_2".to_string(),
            tool_name: "run_shell".to_string(),
            args: json!({ "command": "rm -rf /" }),
            preview: "$ rm -rf /".to_string(),
            requires_confirmation: true,
        });

        sink.on_event(&AgentEvent::ToolCallEnd {
            step_index: 2,
            call_id: "call_2".to_string(),
            tool_name: "run_shell".to_string(),
            status: ToolStatus::Rejected,
            output: json!({ "error": "User menolak eksekusi tool ini." }),
            duration_ms: 0,
        });

        sink.on_event(&AgentEvent::AssistantText {
            text: "Saya memahami penolakan Anda.".to_string(),
        });

        sink.on_event(&AgentEvent::TurnEnd {
            steps_count: 2,
            duration_ms: 50,
            outcome: StepsOutcome::Done,
        });

        let lines = emitted.lock().unwrap().clone();
        // Pastikan mapping tool "run_shell" -> "bash"
        let tool_start = lines.iter().find(|l| l["type"] == "tool_execution_start").unwrap();
        assert_eq!(tool_start["toolName"], "bash");

        // Pastikan event rejection memiliki isError: true
        let tool_ends: Vec<&Value> = lines.iter().filter(|l| l["type"] == "tool_execution_end").collect();
        assert_eq!(tool_ends.len(), 2);
        assert_eq!(tool_ends[0]["isError"], false);
        assert_eq!(tool_ends[1]["isError"], true);

        // Pastikan file sesi ditulis dengan format JSONL valid
        let content = std::fs::read_to_string(&session_file).unwrap();
        assert!(content.contains("\"type\":\"message\""));
        assert!(content.contains("\"role\":\"toolResult\""));
        assert!(content.contains("\"isError\":true"));

        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[tokio::test]
    async fn test_rpc_approver_approved() {
        let pending = Arc::new(Mutex::new(HashMap::new()));
        let emitted = Arc::new(Mutex::new(Vec::new()));
        let emitted_clone = Arc::clone(&emitted);

        let approver = RpcApprover::new(Arc::clone(&pending))
            .with_emitter(move |val| {
                emitted_clone.lock().unwrap().push(val);
            });

        let handle = tokio::spawn(async move {
            approver.approve("run_shell", "$ rm -rf /tmp/danger").await
        });

        for _ in 0..50 {
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
            if !pending.lock().unwrap().is_empty() {
                break;
            }
        }

        let req_id = {
            let map = pending.lock().unwrap();
            assert_eq!(map.len(), 1);
            map.keys().next().unwrap().clone()
        };

        // Verifikasi event request mengikuti protokol Android extension_ui_request (confirm)
        {
            let events = emitted.lock().unwrap();
            assert_eq!(events.len(), 1);
            assert_eq!(events[0]["type"], "extension_ui_request");
            assert_eq!(events[0]["id"], req_id);
            assert_eq!(events[0]["method"], "confirm");
            assert_eq!(events[0]["title"], "Allow run_shell?");
            assert_eq!(events[0]["message"], "$ rm -rf /tmp/danger");
            assert_eq!(events[0]["timeout"], 300_000);
        }

        // Simulasikan persetujuan dari UI
        if let Some(tx) = pending.lock().unwrap().remove(&req_id) {
            let _ = tx.send(true);
        }

        let approved = handle.await.unwrap();
        assert!(approved);
    }

    #[tokio::test]
    async fn test_rpc_approver_denied() {
        let pending = Arc::new(Mutex::new(HashMap::new()));
        let approver = RpcApprover::new(Arc::clone(&pending))
            .with_emitter(|_| {});

        let handle = tokio::spawn(async move {
            approver.approve("run_shell", "$ sudo rm -rf /").await
        });

        for _ in 0..50 {
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
            if !pending.lock().unwrap().is_empty() {
                break;
            }
        }

        let req_id = {
            let map = pending.lock().unwrap();
            map.keys().next().unwrap().clone()
        };

        // Simulasikan penolakan dari UI
        if let Some(tx) = pending.lock().unwrap().remove(&req_id) {
            let _ = tx.send(false);
        }

        let approved = handle.await.unwrap();
        assert!(!approved);
    }

    #[tokio::test]
    async fn test_rpc_approver_timeout_fails_closed() {
        let pending = Arc::new(Mutex::new(HashMap::new()));
        let approver = RpcApprover::new(Arc::clone(&pending))
            .with_timeout(std::time::Duration::from_millis(50))
            .with_emitter(|_| {});

        // Panggil approve tanpa ada yang menyelesaikan respons -> harus timeout dan return false
        let approved = approver.approve("run_shell", "reboot").await;
        assert!(!approved);

        // Pastikan request dibersihkan dari pending map
        assert!(pending.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn test_rpc_approver_channel_dropped_fails_closed() {
        let pending = Arc::new(Mutex::new(HashMap::new()));
        let approver = RpcApprover::new(Arc::clone(&pending))
            .with_emitter(|_| {});

        let handle = tokio::spawn(async move {
            approver.approve("run_shell", "ls").await
        });

        for _ in 0..50 {
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
            if !pending.lock().unwrap().is_empty() {
                break;
            }
        }

        // Ambil sender dan drop tanpa send -> simulasikan channel putus
        {
            let mut map = pending.lock().unwrap();
            let _sender = map.drain().next();
        }

        let approved = handle.await.unwrap();
        assert!(!approved);
    }

    #[tokio::test]
    async fn test_rpc_server_resolve_tool_approval() {
        let server = RpcServer::new(None, None, vec![]).unwrap();
        let (tx, rx) = tokio::sync::oneshot::channel::<bool>();

        server.pending_approvals().lock().unwrap().insert("appr-99".to_string(), tx);

        // Sukses me-resolve approval yang ada
        assert!(server.resolve_tool_approval("appr-99", true));
        assert_eq!(rx.await.unwrap(), true);

        // Memanggil lagi untuk ID yang sama atau id fiktif mengembalikan false
        assert!(!server.resolve_tool_approval("appr-99", true));
        assert!(!server.resolve_tool_approval("unknown-id", false));
    }

    #[tokio::test]
    async fn test_rpc_loop_tool_approval_pipkin_channel_roundtrip() {
        let mut server = RpcServer::new(None, None, vec![]).unwrap();
        let (tx, rx) = tokio::sync::oneshot::channel::<bool>();
        server.pending_approvals().lock().unwrap().insert("appr-42".to_string(), tx);

        let input = b"\
{\"type\":\"pipkin_channel\",\"topic\":\"tool_approval\",\"data\":{\"kind\":\"response\",\"request_id\":\"appr-42\",\"approved\":true}}\n\
{\"type\":\"get_state\",\"id\":\"chk_approval\"}\n";
        let cursor = Cursor::new(input);

        let res = server.run_loop_with_input(cursor).await;
        assert!(res.is_ok());

        // Verifikasi bahwa waiter oneshot menerima approval = true dari stream JSONL
        assert_eq!(rx.await.unwrap(), true);
    }

    #[tokio::test]
    async fn test_rpc_loop_extension_ui_response_roundtrip() {
        let _sink_guard = output_sink_test_mutex().lock().await;

        let raw = Arc::new(Mutex::new(Vec::new()));
        struct SharedBuf(Arc<Mutex<Vec<u8>>>);
        impl io::Write for SharedBuf {
            fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
                self.0.lock().unwrap().write(buf)
            }
            fn flush(&mut self) -> io::Result<()> {
                self.0.lock().unwrap().flush()
            }
        }
        let output_buf = Arc::new(Mutex::new(Box::new(SharedBuf(raw.clone())) as Box<dyn io::Write + Send + Sync>));
        set_output_sink(Some(output_buf));

        let mut server = RpcServer::new(None, None, vec![]).unwrap();
        let (tx1, rx1) = tokio::sync::oneshot::channel::<bool>();
        let (tx2, rx2) = tokio::sync::oneshot::channel::<bool>();
        let (tx3, rx3) = tokio::sync::oneshot::channel::<bool>();
        let (tx_eof, rx_eof) = tokio::sync::oneshot::channel::<bool>();
        {
            let pending = server.pending_approvals();
            let mut map = pending.lock().unwrap();
            map.insert("appr-1".to_string(), tx1);
            map.insert("appr-2".to_string(), tx2);
            map.insert("appr-3".to_string(), tx3);
            map.insert("appr-eof".to_string(), tx_eof);
        }

        let input = b"\
{\"type\":\"extension_ui_response\",\"id\":\"appr-1\",\"confirmed\":true}\n\
{\"type\":\"extension_ui_response\",\"id\":\"appr-2\",\"confirmed\":false}\n\
{\"type\":\"extension_ui_response\",\"id\":\"appr-3\",\"cancelled\":true}\n\
{\"type\":\"get_state\",\"id\":\"chk_ext\"}\n";
        let cursor = Cursor::new(input);

        let res = server.run_loop_with_input(cursor).await;
        set_output_sink(None);
        assert!(res.is_ok());

        assert_eq!(rx1.await.unwrap(), true);
        assert_eq!(rx2.await.unwrap(), false);
        assert_eq!(rx3.await.unwrap(), false);
        assert!(rx_eof.await.is_err());

        let bytes = raw.lock().unwrap().clone();
        let written = String::from_utf8_lossy(&bytes);
        assert!(written.contains("chk_ext"));
        assert!(!written.contains("tidak dikenal"));
        assert!(!written.contains("extension_ui_response"));
    }
}

