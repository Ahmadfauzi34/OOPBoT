//! Mode RPC (`oopbot --mode rpc`) yang kompatibel 100% dengan protokol JSONL
//! aplikasi Android Pipkin (`AgentProcess.kt` & `ChatTimeline.kt`).
//!
//! Menggantikan `@earendil-works/pi-coding-agent` (`pi --mode rpc`) di Termux
//! dengan binary native Rust `oopbot`.

use crate::agent::{Approver, ChatInput, GeminiAgent, SteerHook};
use crate::events::{AgentEvent, EventSink, StepsOutcome, ToolStatus};
use crate::gemini::{
    user_message_with_images, user_text, BoxError, GeminiClient, DEFAULT_GEMINI_BASE_URL,
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
use std::collections::{HashMap, HashSet, VecDeque};
use std::io::{self, BufRead, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use std::time::{SystemTime, UNIX_EPOCH};

type OutputSink = Arc<Mutex<Box<dyn Write + Send + Sync>>>;
static OUTPUT_SINK: RwLock<Option<OutputSink>> = RwLock::new(None);
static SESSION_UNIQUE_SEQ: AtomicU64 = AtomicU64::new(1);

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

struct PendingApprovalGuard {
    pending: PendingApprovals,
    req_id: String,
    active: bool,
}

impl Drop for PendingApprovalGuard {
    fn drop(&mut self) {
        if self.active {
            if let Ok(mut map) = self.pending.lock() {
                map.remove(&self.req_id);
            }
        }
    }
}

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
        let mut _guard = PendingApprovalGuard {
            pending: Arc::clone(&self.pending),
            req_id: req_id.clone(),
            active: true,
        };

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
            Ok(Ok(approved)) => {
                _guard.active = false;
                approved
            }
            Ok(Err(_)) | Err(_) => false,
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum QueueMode {
    OneAtATime,
    All,
}

impl QueueMode {
    fn as_str(self) -> &'static str {
        match self {
            QueueMode::OneAtATime => "one-at-a-time",
            QueueMode::All => "all",
        }
    }

    fn parse(s: &str) -> Option<Self> {
        match s {
            "one-at-a-time" => Some(QueueMode::OneAtATime),
            "all" => Some(QueueMode::All),
            _ => None,
        }
    }
}

#[derive(Debug, Clone)]
struct QueuedPrompt {
    message: String,
    images: Vec<(String, String)>,
}

enum ActiveOp {
    Idle,
    Streaming {
        cancel_tx: tokio::sync::watch::Sender<bool>,
    },
    Compacting {
        cancel_tx: tokio::sync::watch::Sender<bool>,
    },
}

struct RpcMutableState {
    session_file: PathBuf,
    session_id: String,
    session_name: Option<String>,
    active_provider: String,
    active_model: String,
    thinking_level: String,
    providers: HashMap<String, ProviderEndpoint>,
    catalog: ModelCatalog,
    active_op: ActiveOp,
    current_op_seq: u64,
    next_op_seq: u64,
    steering_mode: QueueMode,
    follow_up_mode: QueueMode,
    steering_queue: VecDeque<QueuedPrompt>,
    follow_up_queue: VecDeque<QueuedPrompt>,
    resume_queues_after_compact: bool,
    auto_compaction_enabled: bool,
    auto_retry_enabled: bool,
    retry_cancel_tx: Option<tokio::sync::watch::Sender<bool>>,
}

struct CoordinatorFinishGuard {
    rt: Arc<RpcRuntime>,
    op_seq: u64,
    armed: bool,
}

impl Drop for CoordinatorFinishGuard {
    fn drop(&mut self) {
        if self.armed {
            {
                let mut m = self.rt.mutable.lock().unwrap();
                if matches!(
                    m.active_op,
                    ActiveOp::Streaming { .. } | ActiveOp::Compacting { .. }
                ) && m.current_op_seq == self.op_seq
                {
                    m.active_op = ActiveOp::Idle;
                    m.resume_queues_after_compact = false;
                    m.retry_cancel_tx = None;
                }
            }
            self.rt.defer_settled.store(false, Ordering::SeqCst);
            let _ = self.rt.completed_seq_tx.send(self.op_seq);
        }
    }
}

pub struct RpcRuntime {
    pub cwd: PathBuf,
    pub config_dir: PathBuf,
    pub plugins: Arc<PluginRegistry>,
    pub agent: GeminiAgent,
    pub ui_state: Arc<Mutex<RpcUiState>>,
    pub pending_approvals: PendingApprovals,
    defer_settled: Arc<AtomicBool>,
    mutable: Mutex<RpcMutableState>,
    completed_seq_tx: tokio::sync::watch::Sender<u64>,
    http_scan_lock: tokio::sync::Mutex<()>,
}

pub struct RpcServer {
    runtime: Arc<RpcRuntime>,
}

impl std::ops::Deref for RpcServer {
    type Target = RpcRuntime;
    fn deref(&self) -> &Self::Target {
        &self.runtime
    }
}

impl RpcServer {
    pub fn resolve_default_config_dir() -> PathBuf {
        std::env::var("OOPBOT_CONFIG_DIR")
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
            })
    }

    pub fn new(
        session_path: Option<String>,
        system_append: Option<String>,
        extensions: Vec<String>,
    ) -> Result<Self, BoxError> {
        Self::new_in_dir(
            Self::resolve_default_config_dir(),
            session_path,
            system_append,
            extensions,
        )
    }

    pub fn new_in_dir(
        config_dir: PathBuf,
        session_path: Option<String>,
        system_append: Option<String>,
        extensions: Vec<String>,
    ) -> Result<Self, BoxError> {
        let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
        Self::new_in_dir_with_cwd(config_dir, cwd, session_path, system_append, extensions)
    }

    pub fn new_in_dir_with_cwd(
        config_dir: PathBuf,
        cwd: PathBuf,
        session_path: Option<String>,
        system_append: Option<String>,
        extensions: Vec<String>,
    ) -> Result<Self, BoxError> {
        let runtime = Arc::new(RpcRuntime::new_in_dir_with_cwd(
            config_dir,
            cwd,
            session_path,
            system_append,
            extensions,
        )?);
        Ok(Self { runtime })
    }

    /// Kirim pesan dua arah pada Pipkin/OOPBoT channel: `{"type": "pipkin_channel", "topic": ..., "data": ...}`
    pub fn send_channel(topic: &str, data: Value) {
        RpcRuntime::send_channel(topic, data);
    }

    pub fn send_line(value: &Value) {
        RpcRuntime::send_line(value);
    }

    pub async fn run_loop(&mut self) -> Result<(), BoxError> {
        let stdin = io::stdin();
        let reader = io::BufReader::new(stdin);
        self.run_loop_with_input(reader).await
    }

    pub async fn run_loop_with_input<R: BufRead + Send + 'static>(
        &mut self,
        reader: R,
    ) -> Result<(), BoxError> {
        self.runtime.run_loop_with_input(reader).await
    }
}

impl RpcRuntime {
    fn new_in_dir_with_cwd(
        config_dir: PathBuf,
        cwd: PathBuf,
        session_path: Option<String>,
        system_append: Option<String>,
        extensions: Vec<String>,
    ) -> Result<Self, BoxError> {
        let _ = std::fs::create_dir_all(&config_dir);

        let mut tools = ToolRegistry::with_defaults();
        let mut plugins = PluginRegistry::with_defaults();

        // Aktifkan plugin berdasarkan argumen --extension / --plugin
        for ext in &extensions {
            plugins.activate(ext);
        }
        plugins.register_active_tools(&mut tools);
        let plugins = Arc::new(plugins);

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
        let auto_compaction_enabled = settings["compaction"]["enabled"].as_bool().unwrap_or(true);
        let auto_retry_enabled = settings["retry"]["enabled"].as_bool().unwrap_or(true);

        // Jika provider default belum terkonfigurasi tetapi provider lain sudah punya key/lokal, pilih yang siap pakai.
        if !providers
            .get(&active_provider)
            .map(|p| p.has_auth)
            .unwrap_or(false)
        {
            if let Some((pid, ep)) = providers.iter().find(|(_, p)| p.has_auth && !p.models.is_empty())
            {
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

        let loaded = init_or_load_session(&config_dir, &cwd, session_path.as_deref());
        let session_file = loaded.session_file.clone();
        let session_id = loaded.session_id.clone();
        let session_name = loaded.session_name.clone();
        let llm_history = loaded.llm_history.clone();

        let pending_approvals: PendingApprovals = Arc::new(Mutex::new(HashMap::new()));
        let ui_state = Arc::new(Mutex::new(RpcUiState::from_loaded_session(loaded)));
        let defer_settled = Arc::new(AtomicBool::new(false));

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

        let sink = RpcEventSink::new(Arc::clone(&ui_state))
            .with_defer_settled(Arc::clone(&defer_settled))
            .with_plugins(Arc::clone(&plugins));
        let approver = RpcApprover::new(Arc::clone(&pending_approvals));
        let mut agent = GeminiAgent::with_event_sink_and_tabs(
            "oopbot-rpc",
            client,
            tools,
            Box::new(approver),
            Box::new(sink),
            25,
            tabs,
            None,
        );
        agent.set_system_instruction(system_instruction);

        let (completed_seq_tx, _) = tokio::sync::watch::channel(0u64);
        let mutable = RpcMutableState {
            session_file,
            session_id,
            session_name,
            active_provider,
            active_model,
            thinking_level: requested_thinking.clone(),
            providers,
            catalog,
            active_op: ActiveOp::Idle,
            current_op_seq: 0,
            next_op_seq: 1,
            steering_mode: QueueMode::OneAtATime,
            follow_up_mode: QueueMode::OneAtATime,
            steering_queue: VecDeque::new(),
            follow_up_queue: VecDeque::new(),
            resume_queues_after_compact: false,
            auto_compaction_enabled,
            auto_retry_enabled,
            retry_cancel_tx: None,
        };

        let rt = Self {
            cwd,
            config_dir,
            plugins,
            agent,
            ui_state,
            pending_approvals,
            defer_settled,
            mutable: Mutex::new(mutable),
            completed_seq_tx,
            http_scan_lock: tokio::sync::Mutex::new(()),
        };

        let clamped = rt
            .active_model_info()
            .clamp_thinking_level(&requested_thinking);
        {
            rt.mutable.lock().unwrap().thinking_level = clamped.clone();
        }
        rt.sync_agent_catalog();
        rt.sync_agent_active_model();
        rt.agent.set_thinking_level(&clamped);
        rt.ensure_session_header();
        Ok(rt)
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
            if let Some((topic, data)) = plugin.channel_status() {
                Self::send_channel(topic, data);
            }
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

    fn model_info_from_state(m: &RpcMutableState) -> ModelInfo {
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

    fn sync_agent_catalog(&self) {
        let models = {
            let m = self.mutable.lock().unwrap();
            m.catalog.list().to_vec()
        };
        for model in models {
            self.agent.upsert_model(model);
        }
    }

    fn sync_agent_active_model(&self) {
        let (info, model_id) = {
            let m = self.mutable.lock().unwrap();
            (Self::model_info_from_state(&m), m.active_model.clone())
        };
        self.agent.upsert_model(info);
        self.agent.set_active_model(&model_id);
    }

    fn ensure_session_header(&self) {
        let (session_file, session_id, parent_session) = {
            let m = self.mutable.lock().unwrap();
            let ps = self.ui_state.lock().unwrap().parent_session.clone();
            (m.session_file.clone(), m.session_id.clone(), ps)
        };
        if session_file.exists() {
            return;
        }
        if let Some(parent) = session_file.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        let mut header = json!({
            "type": "session",
            "version": 3,
            "id": session_id,
            "timestamp": iso_now(),
            "cwd": self.cwd.to_string_lossy(),
        });
        if let Some(ps) = parent_session {
            header["parentSession"] = json!(ps);
        }
        if let Ok(line) = serde_json::to_string(&header) {
            let _ = std::fs::write(&session_file, format!("{}\n", line));
        }
    }

    fn current_model_json(&self) -> Value {
        let m = self.mutable.lock().unwrap();
        Self::model_info_from_state(&m).to_pipkin_json(Some(&m.active_provider))
    }

    fn available_models_json(&self) -> Vec<Value> {
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

    fn state_json(&self) -> Value {
        let (
            session_file,
            session_id,
            session_name,
            is_streaming,
            is_compacting,
            steering_mode,
            follow_up_mode,
            thinking_level,
            auto_compaction_enabled,
            auto_retry_enabled,
            pending_message_count,
            model_json,
        ) = {
            let m = self.mutable.lock().unwrap();
            let model_info = Self::model_info_from_state(&m);
            (
                m.session_file.clone(),
                m.session_id.clone(),
                m.session_name.clone(),
                matches!(m.active_op, ActiveOp::Streaming { .. }),
                matches!(m.active_op, ActiveOp::Compacting { .. }),
                m.steering_mode.as_str(),
                m.follow_up_mode.as_str(),
                m.thinking_level.clone(),
                m.auto_compaction_enabled,
                m.auto_retry_enabled,
                m.steering_queue.len() + m.follow_up_queue.len(),
                model_info.to_pipkin_json(Some(&m.active_provider)),
            )
        };
        let message_count = self.ui_state.lock().unwrap().ui_messages.len();

        json!({
            "sessionFile": session_file.to_string_lossy(),
            "sessionId": session_id,
            "sessionName": session_name,
            "isStreaming": is_streaming,
            "isCompacting": is_compacting,
            "steeringMode": steering_mode,
            "followUpMode": follow_up_mode,
            "model": model_json,
            "thinkingLevel": thinking_level,
            "autoCompactionEnabled": auto_compaction_enabled,
            "autoRetryEnabled": auto_retry_enabled,
            "messageCount": message_count,
            "pendingMessageCount": pending_message_count,
        })
    }

    fn append_session_entry(&self, entry_type: &str, payload: Value) -> String {
        self.ensure_session_header();
        self.ui_state
            .lock()
            .unwrap()
            .append_session_entry_returning_id(entry_type, payload)
    }

    fn refresh_credentials(&self) {
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

    fn sync_thinking_level_for_active_model(&self) {
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

    fn needs_auto_scan(&self) -> Option<(String, ProviderEndpoint)> {
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

    async fn maybe_auto_scan(&self) {
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

    fn emit_queue_update(&self) {
        let (steering, follow_up) = {
            let m = self.mutable.lock().unwrap();
            let s: Vec<String> = m.steering_queue.iter().map(|q| q.message.clone()).collect();
            let f: Vec<String> = m
                .follow_up_queue
                .iter()
                .map(|q| q.message.clone())
                .collect();
            (s, f)
        };
        Self::send_line(&json!({
            "type": "queue_update",
            "steering": steering,
            "followUp": follow_up,
        }));
    }

    fn emit_and_persist_user_message(&self, prompt: &str, images: &[(String, String)]) -> u64 {
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
            self.ensure_session_header();
            let mut state = self.ui_state.lock().unwrap();
            state.ui_messages.push(user_msg.clone());
            state.append_session_entry("message", json!({ "message": user_msg }));
        }
        started_ms
    }

    async fn handle_slash_command(&self, cmd_line: &str) -> Option<String> {
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
                        let (active_provider, cur_key) = {
                            let mut m = self.mutable.lock().unwrap();
                            m.catalog.replace_with_scanned(scanned.clone());
                            let key = m
                                .providers
                                .get(&m.active_provider)
                                .map(|p| p.api_key.clone())
                                .unwrap_or_default();
                            (m.active_provider.clone(), key)
                        };
                        self.sync_agent_catalog();
                        save_scanned_models_to_config(
                            &self.config_dir,
                            &active_provider,
                            &scanned,
                            &cur_key,
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
                let active_provider = self.mutable.lock().unwrap().active_provider.clone();
                self.agent
                    .update_credentials(api_key.to_string(), base_url.clone());
                save_auth_key(
                    &self.config_dir,
                    &active_provider,
                    api_key,
                    base_url.as_deref(),
                );
                match self.agent.scan_models().await {
                    Ok(scanned) => {
                        let count = scanned.len();
                        let active_model = {
                            let mut m = self.mutable.lock().unwrap();
                            if !scanned.iter().any(|item| item.id == m.active_model) {
                                if let Some(first) = scanned.first() {
                                    m.active_model = first.id.clone();
                                }
                            }
                            m.catalog.replace_with_scanned(scanned.clone());
                            m.active_model.clone()
                        };
                        self.sync_agent_catalog();
                        self.sync_agent_active_model();
                        save_scanned_models_to_config(
                            &self.config_dir,
                            &active_provider,
                            &scanned,
                            api_key,
                        );
                        self.refresh_credentials();
                        save_settings_fields(
                            &self.config_dir,
                            Some(&active_provider),
                            Some(&active_model),
                            None,
                        );
                        Some(format!(
                            "Token berhasil disimpan & **{} model** terdeteksi otomatis! Model aktif sekarang: `{}` (`{}`).",
                            count, active_model, active_provider
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
                let (active_provider, active_model) = {
                    let m = self.mutable.lock().unwrap();
                    (m.active_provider.clone(), m.active_model.clone())
                };
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
                        let mark = if pid == active_provider && mid == active_model {
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
                    let (active_provider, active_model, thinking_level, info) = {
                        let m = self.mutable.lock().unwrap();
                        (
                            m.active_provider.clone(),
                            m.active_model.clone(),
                            m.thinking_level.clone(),
                            Self::model_info_from_state(&m),
                        )
                    };
                    return Some(format!(
                        "Model aktif saat ini: `{}/{}` (**{}**)\n- API: `{}`\n- Context window: `{}` token\n- Max output: `{}` token\n- Thinking: `{}` (level aktif: `{}`)\n\nGunakan `/model <ID>` atau `/model <provider>/<ID>` untuk mengganti.",
                        active_provider,
                        active_model,
                        info.label,
                        info.api,
                        info.input_token_limit.unwrap_or(1_048_576),
                        info.output_token_limit.unwrap_or(65_536),
                        info.reasoning,
                        thinking_level
                    ));
                }
                let target = parts[1].trim();
                {
                    let mut m = self.mutable.lock().unwrap();
                    if let Some((prov, mid)) = target.split_once('/') {
                        if m.providers.contains_key(prov) {
                            m.active_provider = prov.to_string();
                            m.active_model = mid.to_string();
                        } else {
                            m.active_model = target.to_string();
                        }
                    } else if let Some(info) = m.catalog.find(target) {
                        m.active_model = info.id.clone();
                    } else {
                        let mut found = None;
                        for (pid, ep) in &m.providers {
                            if let Some(model) = ep
                                .models
                                .iter()
                                .find(|item| item.id.eq_ignore_ascii_case(target))
                            {
                                found = Some((pid.clone(), model.id.clone()));
                                break;
                            }
                        }
                        if let Some((pid, mid)) = found {
                            m.active_provider = pid;
                            m.active_model = mid;
                        } else {
                            m.catalog.register_custom(target, target);
                            m.active_model = target.to_string();
                        }
                    }
                }
                self.sync_agent_active_model();
                self.refresh_credentials();
                self.sync_thinking_level_for_active_model();
                let (active_provider, active_model, thinking_level) = {
                    let m = self.mutable.lock().unwrap();
                    (
                        m.active_provider.clone(),
                        m.active_model.clone(),
                        m.thinking_level.clone(),
                    )
                };
                save_settings_fields(
                    &self.config_dir,
                    Some(&active_provider),
                    Some(&active_model),
                    Some(&thinking_level),
                );
                Some(format!(
                    "Model aktif diganti ke `{}/{}` (thinking: `{}`).",
                    active_provider, active_model, thinking_level
                ))
            }
            "/thinking" => {
                let (info, active_model, thinking_level) = {
                    let m = self.mutable.lock().unwrap();
                    (
                        Self::model_info_from_state(&m),
                        m.active_model.clone(),
                        m.thinking_level.clone(),
                    )
                };
                let supported = info.available_thinking_levels();
                if parts.len() < 2 {
                    return Some(format!(
                        "Level thinking saat ini: `{}` (didukung oleh `{}`: `{}`).\nGunakan `/thinking <{}>` untuk mengubah.",
                        thinking_level,
                        active_model,
                        supported.join(", "),
                        supported.join("|")
                    ));
                }
                let requested = parts[1].trim().to_ascii_lowercase();
                let clamped = info.clamp_thinking_level(&requested);
                {
                    self.mutable.lock().unwrap().thinking_level = clamped.clone();
                }
                self.agent.set_thinking_level(&clamped);
                save_settings_fields(&self.config_dir, None, None, Some(&clamped));
                Self::send_line(&json!({
                    "type": "thinking_level_changed",
                    "level": clamped,
                }));
                Some(format!(
                    "Level thinking diatur ke `{}` untuk model `{}`.",
                    clamped, active_model
                ))
            }
            "/provider" => {
                if parts.len() < 2 {
                    let (providers, active_provider) = {
                        let m = self.mutable.lock().unwrap();
                        (m.providers.clone(), m.active_provider.clone())
                    };
                    let mut lines = Vec::new();
                    for (pid, ep) in &providers {
                        let mark = if *pid == active_provider {
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
                let ep_opt = self.mutable.lock().unwrap().providers.get(target_pid).cloned();
                if let Some(ep) = ep_opt {
                    {
                        let mut m = self.mutable.lock().unwrap();
                        m.active_provider = target_pid.to_string();
                        if let Some(first) = ep.models.first() {
                            m.active_model = first.id.clone();
                        }
                    }
                    self.refresh_credentials();
                    self.sync_agent_active_model();
                    self.sync_thinking_level_for_active_model();
                    let (active_provider, active_model, thinking_level) = {
                        let m = self.mutable.lock().unwrap();
                        (
                            m.active_provider.clone(),
                            m.active_model.clone(),
                            m.thinking_level.clone(),
                        )
                    };
                    save_settings_fields(
                        &self.config_dir,
                        Some(&active_provider),
                        Some(&active_model),
                        Some(&thinking_level),
                    );
                    Some(format!(
                        "Provider aktif diganti ke `{}` dengan model `{}`.",
                        active_provider, active_model
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
                    let active_model = self.mutable.lock().unwrap().active_model.clone();
                    return Some(format!(
                        "Batas `maxTokens` model `{}` saat ini: `{}`.\nGunakan `/max-tokens <angka>` untuk mengubah dan menyimpan ke `models.json`.",
                        active_model, cur
                    ));
                }
                let Ok(max_tok) = parts[1].trim().parse::<u64>() else {
                    return Some("Nilai `/max-tokens` harus berupa angka bulat positif.".to_string());
                };
                if max_tok == 0 {
                    return Some("Nilai `/max-tokens` harus lebih besar dari 0.".to_string());
                }
                let (active_provider, active_model) = {
                    let m = self.mutable.lock().unwrap();
                    (m.active_provider.clone(), m.active_model.clone())
                };
                save_model_override_to_config(
                    &self.config_dir,
                    &active_provider,
                    &active_model,
                    &json!({ "maxTokens": max_tok }),
                );
                self.refresh_credentials();
                self.sync_agent_active_model();
                Some(format!(
                    "`maxTokens` untuk `{}/{}` berhasil disimpan ke `{}`.",
                    active_provider, active_model, max_tok
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

    fn emit_and_persist_abort_message(&self, started_ms: u64, emit_turn_events: bool) {
        let abort_msg = json!({
            "role": "assistant",
            "content": [],
            "stopReason": "aborted",
            "timestamp": started_ms,
            "durationMs": epoch_ms().saturating_sub(started_ms),
        });
        Self::send_line(&json!({ "type": "message_start", "message": abort_msg }));
        Self::send_line(&json!({ "type": "message_end", "message": abort_msg }));
        {
            self.ensure_session_header();
            let mut state = self.ui_state.lock().unwrap();
            state.ui_messages.push(abort_msg.clone());
            state.append_session_entry("message", json!({ "message": abort_msg }));
        }
        if emit_turn_events {
            Self::send_line(&json!({ "type": "turn_end" }));
            Self::send_line(&json!({ "type": "agent_end" }));
        }
        self.emit_active_widgets();
    }

    async fn execute_single_turn(
        &self,
        prompt: &str,
        images: &[(String, String)],
        cancel_rx: &mut tokio::sync::watch::Receiver<bool>,
        steer_hook: &SteerHook,
    ) -> bool {
        self.refresh_credentials();
        self.sync_thinking_level_for_active_model();
        let started_ms = self.emit_and_persist_user_message(prompt, images);

        if prompt.trim_start().starts_with('/') && images.is_empty() {
            if let Some(reply_text) = self.handle_slash_command(prompt.trim()).await {
                Self::send_line(&json!({ "type": "agent_start" }));
                self.emit_assistant_reply_with_thoughts(&reply_text, &[], started_ms, None);
                Self::send_line(&json!({ "type": "turn_end" }));
                Self::send_line(&json!({ "type": "agent_end" }));
                return false;
            }
        }

        let (provider_has_auth, active_provider, active_model, auto_retry_enabled) = {
            let m = self.mutable.lock().unwrap();
            let has_auth = m
                .providers
                .get(&m.active_provider)
                .map(|p| p.has_auth)
                .unwrap_or(false);
            (
                has_auth,
                m.active_provider.clone(),
                m.active_model.clone(),
                m.auto_retry_enabled,
            )
        };
        if !provider_has_auth && self.agent.api_key().trim().is_empty() {
            let msg = "API key belum diatur. Silakan buka **Settings › Providers & models** di OOPBoT untuk menempelkan API key (Google Gemini / OpenRouter / Custom), atau ketik langsung di sini:\n\n`/token <API_KEY>`";
            Self::send_line(&json!({ "type": "agent_start" }));
            self.emit_assistant_reply_with_thoughts(msg, &[], started_ms, None);
            Self::send_line(&json!({ "type": "turn_end" }));
            Self::send_line(&json!({ "type": "agent_end" }));
            return false;
        }

        let input = ChatInput::with_images(prompt.to_string(), images.to_vec());
        let max_attempts = if auto_retry_enabled { 3usize } else { 1usize };
        let mut attempt = 0usize;
        let mut retry_started = false;

        loop {
            attempt += 1;
            match self
                .agent
                .chat_turn_cancellable(&input, cancel_rx.clone(), Some(Arc::clone(steer_hook)))
                .await
            {
                Ok(outcome) => {
                    if retry_started {
                        Self::send_line(&json!({
                            "type": "auto_retry_end",
                            "success": true,
                            "attempt": attempt,
                        }));
                    }

                    let active_info = self.active_model_info();
                    let has_real_usage = outcome.usage.is_some();
                    let (turn_in, turn_out) = if let Some(u) = outcome.usage {
                        (u.input_tokens, u.output_tokens)
                    } else if outcome.aborted || outcome.text.trim().is_empty() {
                        (0, 0)
                    } else {
                        (
                            (prompt.len() as u64 / 4) + 60,
                            (outcome.text.len() as u64 / 4) + 60,
                        )
                    };
                    let turn_cost = if let Some(cost) = &active_info.cost {
                        (turn_in as f64 / 1_000_000.0) * cost.input
                            + (turn_out as f64 / 1_000_000.0) * cost.output
                    } else {
                        0.0
                    };
                    let last_ctx = outcome
                        .last_usage
                        .map(|lu| lu.input_tokens + lu.output_tokens)
                        .or_else(|| {
                            if !outcome.aborted && (turn_in + turn_out) > 0 {
                                Some(turn_in + turn_out)
                            } else {
                                None
                            }
                        });

                    if turn_in + turn_out > 0 || has_real_usage {
                        {
                            let mut state = self.ui_state.lock().unwrap();
                            state.token_totals.input += turn_in;
                            state.token_totals.output += turn_out;
                            state.total_tokens = state.total_tokens.saturating_add(turn_in + turn_out);
                            state.total_cost += turn_cost;
                            if let Some(ctx) = last_ctx {
                                state.last_context_tokens = Some(ctx);
                                state.context_unknown = false;
                            }
                        }

                        let mut usage_entry = json!({
                            "kind": "turn",
                            "provider": active_provider,
                            "model": active_model,
                            "usage": {
                                "input": turn_in,
                                "output": turn_out,
                                "cacheRead": 0,
                                "cacheWrite": 0,
                                "totalTokens": turn_in + turn_out,
                                "cost": {
                                    "input": 0.0,
                                    "output": 0.0,
                                    "cacheRead": 0.0,
                                    "cacheWrite": 0.0,
                                    "total": turn_cost,
                                }
                            },
                        });
                        if let Some(ctx) = last_ctx {
                            usage_entry["lastContextTokens"] = json!(ctx);
                        }
                        self.append_session_entry("usage", usage_entry);
                    }

                    if outcome.aborted {
                        self.emit_and_persist_abort_message(started_ms, false);
                        return true;
                    }

                    self.emit_active_widgets();
                    return false;
                }
                Err(err) => {
                    if *cancel_rx.borrow() {
                        if retry_started {
                            Self::send_line(&json!({
                                "type": "auto_retry_end",
                                "success": false,
                                "attempt": attempt,
                                "finalError": "Dibatalkan oleh user",
                            }));
                        }
                        self.emit_and_persist_abort_message(started_ms, true);
                        return true;
                    }

                    let err_str = err.to_string();
                    if attempt < max_attempts && is_transient_llm_error(&err_str) {
                        let delay_ms = 500u64 * (1u64 << (attempt - 1));
                        retry_started = true;
                        let (rtx, mut rrx) = tokio::sync::watch::channel(false);
                        {
                            self.mutable.lock().unwrap().retry_cancel_tx = Some(rtx);
                        }
                        Self::send_line(&json!({
                            "type": "auto_retry_start",
                            "attempt": attempt,
                            "maxAttempts": max_attempts,
                            "delayMs": delay_ms,
                            "errorMessage": err_str,
                        }));

                        enum RetryWaitOutcome {
                            SleepDone,
                            AbortedRun,
                            AbortedRetry,
                        }
                        let wait_out = tokio::select! {
                            biased;
                            _ = wait_watch_true(cancel_rx) => RetryWaitOutcome::AbortedRun,
                            _ = wait_watch_true(&mut rrx) => RetryWaitOutcome::AbortedRetry,
                            _ = tokio::time::sleep(std::time::Duration::from_millis(delay_ms)) => RetryWaitOutcome::SleepDone,
                        };
                        {
                            self.mutable.lock().unwrap().retry_cancel_tx = None;
                        }
                        match wait_out {
                            RetryWaitOutcome::SleepDone => continue,
                            RetryWaitOutcome::AbortedRun => {
                                Self::send_line(&json!({
                                    "type": "auto_retry_end",
                                    "success": false,
                                    "attempt": attempt,
                                    "finalError": err_str,
                                }));
                                self.emit_and_persist_abort_message(started_ms, true);
                                return true;
                            }
                            RetryWaitOutcome::AbortedRetry => {
                                Self::send_line(&json!({
                                    "type": "auto_retry_end",
                                    "success": false,
                                    "attempt": attempt,
                                    "finalError": err_str,
                                }));
                            }
                        }
                    } else if retry_started {
                        Self::send_line(&json!({
                            "type": "auto_retry_end",
                            "success": false,
                            "attempt": attempt,
                            "finalError": err_str,
                        }));
                    }

                    let err_msg = json!({
                        "role": "assistant",
                        "content": [],
                        "stopReason": "error",
                        "errorMessage": err_str,
                        "timestamp": started_ms,
                        "durationMs": epoch_ms().saturating_sub(started_ms),
                    });
                    Self::send_line(&json!({ "type": "message_start", "message": err_msg }));
                    Self::send_line(&json!({ "type": "message_end", "message": err_msg }));
                    {
                        self.ensure_session_header();
                        let mut state = self.ui_state.lock().unwrap();
                        state.ui_messages.push(err_msg.clone());
                        state.append_session_entry("message", json!({ "message": err_msg }));
                    }
                    Self::send_line(&json!({ "type": "turn_end" }));
                    Self::send_line(&json!({ "type": "agent_end" }));
                    self.emit_active_widgets();
                    return false;
                }
            }
        }
    }

    async fn run_prompt_coordinator(
        rt: Arc<RpcRuntime>,
        first: QueuedPrompt,
        mut cancel_rx: tokio::sync::watch::Receiver<bool>,
        op_seq: u64,
    ) {
        let mut finish_guard = CoordinatorFinishGuard {
            rt: Arc::clone(&rt),
            op_seq,
            armed: true,
        };
        rt.defer_settled.store(true, Ordering::SeqCst);

        let rt_hook = Arc::clone(&rt);
        let steer_hook: SteerHook = Arc::new(move || {
            let (drained, rem_s, rem_f) = {
                let mut m = rt_hook.mutable.lock().unwrap();
                let count = match m.steering_mode {
                    QueueMode::OneAtATime => 1.min(m.steering_queue.len()),
                    QueueMode::All => m.steering_queue.len(),
                };
                let mut out = Vec::with_capacity(count);
                for _ in 0..count {
                    if let Some(q) = m.steering_queue.pop_front() {
                        out.push(q);
                    }
                }
                let rs: Vec<String> = m.steering_queue.iter().map(|q| q.message.clone()).collect();
                let rf: Vec<String> = m
                    .follow_up_queue
                    .iter()
                    .map(|q| q.message.clone())
                    .collect();
                (out, rs, rf)
            };
            if !drained.is_empty() {
                Self::send_line(&json!({
                    "type": "queue_update",
                    "steering": rem_s,
                    "followUp": rem_f,
                }));
                for q in &drained {
                    rt_hook.emit_and_persist_user_message(&q.message, &q.images);
                }
            }
            drained
                .into_iter()
                .map(|q| ChatInput::with_images(q.message, q.images))
                .collect()
        });

        let mut current_inputs = vec![first];
        let mut final_aborted = false;

        loop {
            if *cancel_rx.borrow() {
                final_aborted = true;
                break;
            }
            let last_input = current_inputs.pop().unwrap();
            for extra in &current_inputs {
                rt.emit_and_persist_user_message(&extra.message, &extra.images);
                rt.agent
                    .append_user_message_to_active_history(&ChatInput::with_images(
                        extra.message.clone(),
                        extra.images.clone(),
                    ));
            }
            let turn_aborted = rt
                .execute_single_turn(&last_input.message, &last_input.images, &mut cancel_rx, &steer_hook)
                .await;
            if turn_aborted || *cancel_rx.borrow() {
                final_aborted = true;
                break;
            }

            // Cek ambang auto-compaction bila diaktifkan & konteks >= 85% window
            let maybe_auto_compact = {
                let m = rt.mutable.lock().unwrap();
                if m.auto_compaction_enabled {
                    let window = Self::model_info_from_state(&m)
                        .input_token_limit
                        .unwrap_or(0);
                    let state = rt.ui_state.lock().unwrap();
                    if window >= 4096 && !state.context_unknown {
                        let threshold = window.saturating_sub(16_384).max((window * 85) / 100);
                        state
                            .last_context_tokens
                            .map(|toks| toks >= threshold)
                            .unwrap_or(false)
                    } else {
                        false
                    }
                } else {
                    false
                }
            };
            if maybe_auto_compact {
                let _ = rt
                    .perform_compaction(None, "threshold", &mut cancel_rx)
                    .await;
                if *cancel_rx.borrow() {
                    final_aborted = true;
                    break;
                }
            }

            let (next_batch, rem_s, rem_f) = {
                let mut m = rt.mutable.lock().unwrap();
                if !m.steering_queue.is_empty() {
                    let n = match m.steering_mode {
                        QueueMode::OneAtATime => 1,
                        QueueMode::All => m.steering_queue.len(),
                    };
                    let batch: Vec<_> =
                        (0..n).filter_map(|_| m.steering_queue.pop_front()).collect();
                    let rs: Vec<String> =
                        m.steering_queue.iter().map(|q| q.message.clone()).collect();
                    let rf: Vec<String> =
                        m.follow_up_queue.iter().map(|q| q.message.clone()).collect();
                    (Some(batch), rs, rf)
                } else if !m.follow_up_queue.is_empty() {
                    let n = match m.follow_up_mode {
                        QueueMode::OneAtATime => 1,
                        QueueMode::All => m.follow_up_queue.len(),
                    };
                    let batch: Vec<_> = (0..n)
                        .filter_map(|_| m.follow_up_queue.pop_front())
                        .collect();
                    let rs: Vec<String> =
                        m.steering_queue.iter().map(|q| q.message.clone()).collect();
                    let rf: Vec<String> =
                        m.follow_up_queue.iter().map(|q| q.message.clone()).collect();
                    (Some(batch), rs, rf)
                } else {
                    (None, Vec::new(), Vec::new())
                }
            };
            match next_batch {
                Some(batch) if !batch.is_empty() => {
                    Self::send_line(&json!({
                        "type": "queue_update",
                        "steering": rem_s,
                        "followUp": rem_f,
                    }));
                    current_inputs = batch;
                }
                _ => break,
            }
        }

        {
            let mut m = rt.mutable.lock().unwrap();
            m.active_op = ActiveOp::Idle;
            m.retry_cancel_tx = None;
        }
        rt.defer_settled.store(false, Ordering::SeqCst);
        Self::send_line(&json!({ "type": "agent_settled", "aborted": final_aborted }));
        finish_guard.armed = false;
        let _ = rt.completed_seq_tx.send(op_seq);
    }

    async fn perform_compaction(
        &self,
        custom_instructions: Option<&str>,
        reason: &str,
        cancel_rx: &mut tokio::sync::watch::Receiver<bool>,
    ) -> Result<Value, (bool, String)> {
        self.refresh_credentials();

        let (
            all_entries,
            leaf_id,
            session_id,
            session_name,
            active_provider,
            active_model,
            tokens_before,
        ) = {
            let m = self.mutable.lock().unwrap();
            let s = self.ui_state.lock().unwrap();
            let ctx_toks = s
                .last_context_tokens
                .filter(|&t| t > 0)
                .unwrap_or_else(|| estimate_messages_tokens(&s.ui_messages).max(1));
            (
                s.entries.clone(),
                s.leaf_id.clone(),
                m.session_id.clone(),
                m.session_name.clone(),
                m.active_provider.clone(),
                m.active_model.clone(),
                ctx_toks,
            )
        };

        let path_entries = active_branch_entries(&all_entries, leaf_id.as_deref());
        let rebuilt_before = rebuild_context_from_entries(&path_entries);
        if rebuilt_before.active_entries.is_empty() && path_entries.is_empty() {
            return Err((false, "Tidak ada histori pesan untuk dikompak.".to_string()));
        }

        let Some(cut_decision) = select_compaction_cut_point(&rebuilt_before.active_entries) else {
            return Err((false, "Tidak cukup pesan untuk dikompak.".to_string()));
        };

        let compaction_prompt = build_compaction_prompt(
            &cut_decision.to_summarize,
            rebuilt_before.previous_compaction_summary.as_deref(),
            custom_instructions,
        );

        let emit_started = reason == "threshold";
        if emit_started {
            Self::send_line(&json!({
                "type": "compaction_start",
                "reason": "threshold",
            }));
        }

        let (summary, summary_usage) = match self
            .agent
            .generate_compaction_summary_cancellable(&compaction_prompt, cancel_rx.clone())
            .await
        {
            Ok(pair) => pair,
            Err(e) => {
                let is_aborted = *cancel_rx.borrow();
                if emit_started {
                    let mut end_ev = json!({
                        "type": "compaction_end",
                        "reason": "threshold",
                        "aborted": is_aborted,
                    });
                    if !is_aborted {
                        end_ev["errorMessage"] = json!(e.to_string());
                    }
                    Self::send_line(&end_ev);
                }
                return Err((is_aborted, e.to_string()));
            }
        };

        if *cancel_rx.borrow() {
            if emit_started {
                Self::send_line(&json!({
                    "type": "compaction_end",
                    "reason": "threshold",
                    "aborted": true,
                }));
            }
            return Err((true, "Compaction aborted".to_string()));
        }

        let active_info = self.active_model_info();
        let (compact_id, rebuilt_after, estimated_after, usage_json) = {
            self.ensure_session_header();
            let mut s = self.ui_state.lock().unwrap();
            let cid = s.next_unique_entry_id();
            let first_kept = cut_decision
                .first_kept_entry_id
                .unwrap_or_else(|| cid.clone());

            let mut usage_obj = Value::Null;
            let mut cost_delta = 0.0f64;
            if let Some(u) = summary_usage {
                let c_in = active_info
                    .cost
                    .as_ref()
                    .map(|c| (u.input_tokens as f64 / 1_000_000.0) * c.input)
                    .unwrap_or(0.0);
                let c_out = active_info
                    .cost
                    .as_ref()
                    .map(|c| (u.output_tokens as f64 / 1_000_000.0) * c.output)
                    .unwrap_or(0.0);
                cost_delta = c_in + c_out;
                usage_obj = json!({
                    "input": u.input_tokens,
                    "output": u.output_tokens,
                    "cacheRead": 0,
                    "cacheWrite": 0,
                    "totalTokens": u.input_tokens + u.output_tokens,
                    "cost": {
                        "input": c_in,
                        "output": c_out,
                        "cacheRead": 0.0,
                        "cacheWrite": 0.0,
                        "total": cost_delta,
                    }
                });
                s.token_totals.input += u.input_tokens;
                s.token_totals.output += u.output_tokens;
                s.total_tokens = s
                    .total_tokens
                    .saturating_add(u.input_tokens + u.output_tokens);
                s.total_cost += cost_delta;
            }
            let _ = (active_provider, active_model, cost_delta);

            let mut payload = json!({
                "summary": summary,
                "firstKeptEntryId": first_kept,
                "tokensBefore": tokens_before,
                "details": {},
            });
            if !usage_obj.is_null() {
                payload["usage"] = usage_obj.clone();
            }
            let _ = s.append_session_entry_with_id(&cid, "compaction", payload);

            let branch = active_branch_entries(&s.entries, s.leaf_id.as_deref());
            let rebuilt = rebuild_context_from_entries(&branch);
            s.ui_messages = rebuilt.ui_messages.clone();
            s.last_context_tokens = None;
            s.context_unknown = true;
            let est_after = estimate_messages_tokens(&rebuilt.ui_messages).max(1);
            (cid, rebuilt, est_after, usage_obj)
        };

        self.agent.replace_active_history(
            &session_id,
            session_name.as_deref(),
            rebuilt_after.llm_history,
        );

        let first_kept_id = {
            let s = self.ui_state.lock().unwrap();
            s.entries
                .iter()
                .rev()
                .find(|e| e["id"] == compact_id && e["type"] == "compaction")
                .and_then(|e| e["firstKeptEntryId"].as_str())
                .unwrap_or(&compact_id)
                .to_string()
        };

        let mut result = json!({
            "summary": summary,
            "firstKeptEntryId": first_kept_id,
            "tokensBefore": tokens_before,
            "estimatedTokensAfter": estimated_after,
            "details": {},
        });
        if !usage_json.is_null() {
            result["usage"] = usage_json;
        }
        if emit_started {
            Self::send_line(&json!({
                "type": "compaction_end",
                "reason": "threshold",
                "result": result,
                "aborted": false,
            }));
        }
        Ok(result)
    }

    async fn run_compact_task(
        rt: Arc<RpcRuntime>,
        cmd_id: String,
        custom_instructions: Option<String>,
        mut cancel_rx: tokio::sync::watch::Receiver<bool>,
        op_seq: u64,
        resume_tx: tokio::sync::mpsc::UnboundedSender<(
            QueuedPrompt,
            tokio::sync::watch::Receiver<bool>,
            u64,
        )>,
    ) {
        let mut finish_guard = CoordinatorFinishGuard {
            rt: Arc::clone(&rt),
            op_seq,
            armed: true,
        };

        let outcome = rt
            .perform_compaction(custom_instructions.as_deref(), "manual", &mut cancel_rx)
            .await;

        let (was_aborted, payload_res) = match outcome {
            Ok(result) => {
                Self::send_line(&json!({
                    "type": "compaction_end",
                    "reason": "manual",
                    "result": result,
                    "aborted": false,
                }));
                (false, Ok(result))
            }
            Err((aborted, err_msg)) => {
                let mut end_ev = json!({
                    "type": "compaction_end",
                    "reason": "manual",
                    "aborted": aborted,
                });
                if !aborted {
                    end_ev["errorMessage"] = json!(err_msg);
                }
                Self::send_line(&end_ev);
                (aborted, Err(err_msg))
            }
        };

        let aborted_by_cancel = was_aborted || *cancel_rx.borrow();
        let resumed = {
            let mut m = rt.mutable.lock().unwrap();
            let should_resume = std::mem::take(&mut m.resume_queues_after_compact);
            if !aborted_by_cancel && should_resume {
                let next_prompt = m
                    .steering_queue
                    .pop_front()
                    .or_else(|| m.follow_up_queue.pop_front());
                if let Some(first) = next_prompt {
                    let (cancel_tx, next_cancel_rx) = tokio::sync::watch::channel(false);
                    let seq = m.next_op_seq;
                    m.next_op_seq += 1;
                    m.current_op_seq = seq;
                    m.active_op = ActiveOp::Streaming { cancel_tx };
                    let rs: Vec<String> =
                        m.steering_queue.iter().map(|q| q.message.clone()).collect();
                    let rf: Vec<String> =
                        m.follow_up_queue.iter().map(|q| q.message.clone()).collect();
                    Some((first, next_cancel_rx, seq, rs, rf))
                } else {
                    m.active_op = ActiveOp::Idle;
                    None
                }
            } else {
                m.active_op = ActiveOp::Idle;
                None
            }
        };

        match payload_res {
            Ok(result) => Self::send_response(&cmd_id, "compact", result),
            Err(err_msg) => Self::send_error(&cmd_id, "compact", &err_msg),
        }
        finish_guard.armed = false;
        let _ = rt.completed_seq_tx.send(op_seq);

        if let Some((first, next_cancel_rx, seq, rs, rf)) = resumed {
            Self::send_line(&json!({
                "type": "queue_update",
                "steering": rs,
                "followUp": rf,
            }));
            if resume_tx.send((first, next_cancel_rx, seq)).is_err() {
                let mut m = rt.mutable.lock().unwrap();
                if matches!(m.active_op, ActiveOp::Streaming { .. }) && m.current_op_seq == seq {
                    m.active_op = ActiveOp::Idle;
                }
                let _ = rt.completed_seq_tx.send(seq);
            }
        }
    }

    fn emit_assistant_reply_with_thoughts(
        &self,
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
            self.ensure_session_header();
            let mut state = self.ui_state.lock().unwrap();
            state.ui_messages.push(msg.clone());
            state.append_session_entry("message", json!({ "message": msg }));
        }
    }

    fn apply_loaded_session(&self, loaded: LoadedSession) {
        let llm_history = loaded.llm_history.clone();
        let new_id = loaded.session_id.clone();
        let new_name = loaded.session_name.clone();
        let new_file = loaded.session_file.clone();

        {
            let mut m = self.mutable.lock().unwrap();
            m.session_file = new_file;
            m.session_id = new_id.clone();
            m.session_name = new_name.clone();
            m.steering_queue.clear();
            m.follow_up_queue.clear();
            m.resume_queues_after_compact = false;
        }
        {
            let mut s = self.ui_state.lock().unwrap();
            *s = RpcUiState::from_loaded_session(loaded);
        }
        self.agent
            .replace_active_history(&new_id, new_name.as_deref(), llm_history);
    }

    fn handle_session_stats_command(&self, id: &str) {
        let model_info = self.active_model_info();
        let (session_file, session_id) = {
            let m = self.mutable.lock().unwrap();
            (
                m.session_file.to_string_lossy().to_string(),
                m.session_id.clone(),
            )
        };

        let (
            user_messages,
            assistant_messages,
            tool_calls,
            tool_results,
            total_messages,
            tokens,
            total_cost,
            context_usage,
        ) = {
            let s = self.ui_state.lock().unwrap();
            let mut user_cnt = 0u64;
            let mut asst_cnt = 0u64;
            let mut call_cnt = 0u64;
            let mut res_cnt = 0u64;
            for msg in &s.ui_messages {
                match msg["role"].as_str().unwrap_or_default() {
                    "user" => user_cnt += 1,
                    "assistant" => {
                        asst_cnt += 1;
                        if let Some(arr) = msg["content"].as_array() {
                            call_cnt += arr
                                .iter()
                                .filter(|b| b["type"].as_str() == Some("toolCall"))
                                .count() as u64;
                        }
                    }
                    "toolResult" => res_cnt += 1,
                    _ => {}
                }
            }
            let total_tok = s.token_totals.total().max(s.total_tokens);
            let ctx_usage = if let Some(window) = model_info.input_token_limit.filter(|&w| w > 0) {
                if s.context_unknown {
                    Some(json!({
                        "tokens": Value::Null,
                        "contextWindow": window,
                        "percent": Value::Null,
                    }))
                } else {
                    let ctx_tok = s.last_context_tokens.unwrap_or(total_tok);
                    let pct = (ctx_tok as f64 / window as f64) * 100.0;
                    Some(json!({
                        "tokens": ctx_tok,
                        "contextWindow": window,
                        "percent": pct,
                    }))
                }
            } else {
                None
            };

            (
                user_cnt,
                asst_cnt,
                call_cnt,
                res_cnt,
                s.ui_messages.len() as u64,
                json!({
                    "input": s.token_totals.input,
                    "output": s.token_totals.output,
                    "cacheRead": s.token_totals.cache_read,
                    "cacheWrite": s.token_totals.cache_write,
                    "total": total_tok,
                }),
                s.total_cost,
                ctx_usage,
            )
        };

        let mut resp = json!({
            "sessionFile": session_file,
            "sessionId": session_id,
            "userMessages": user_messages,
            "assistantMessages": assistant_messages,
            "toolCalls": tool_calls,
            "toolResults": tool_results,
            "totalMessages": total_messages,
            "tokens": tokens,
            "cost": total_cost,
        });
        if let Some(cu) = context_usage {
            resp["contextUsage"] = cu;
        }
        Self::send_response(id, "get_session_stats", resp);
    }

    fn handle_fork_command(&self, id: &str, entry_id: &str) {
        {
            let m = self.mutable.lock().unwrap();
            if !matches!(m.active_op, ActiveOp::Idle) {
                Self::send_error(id, "fork", "Tidak bisa fork saat agen sedang sibuk");
                return;
            }
        }
        if entry_id.trim().is_empty() {
            Self::send_error(id, "fork", "entryId wajib diisi");
            return;
        }

        let (old_file, path_entries) = {
            let m = self.mutable.lock().unwrap();
            let s = self.ui_state.lock().unwrap();
            (
                m.session_file.clone(),
                active_branch_entries(&s.entries, s.leaf_id.as_deref()),
            )
        };

        let Some(pos) = path_entries
            .iter()
            .position(|e| e["id"].as_str() == Some(entry_id))
        else {
            Self::send_error(
                id,
                "fork",
                &format!("Entry '{}' tidak ditemukan pada branch aktif", entry_id),
            );
            return;
        };

        let target_entry = &path_entries[pos];
        if target_entry["type"].as_str() != Some("message")
            || target_entry["message"]["role"].as_str() != Some("user")
        {
            Self::send_error(id, "fork", "fork hanya mendukung entry pesan user");
            return;
        }
        let target_text = extract_user_message_text(&target_entry["message"]);

        let new_file = unique_session_file_path(
            &self.config_dir,
            &self.cwd,
            old_file.parent(),
        );
        let new_session_id = new_unique_session_id();
        if let Err(e) = write_branched_session_file(
            &new_file,
            &new_session_id,
            &self.cwd,
            Some(&old_file),
            &path_entries[..pos],
        ) {
            Self::send_error(id, "fork", &format!("Gagal membuat sesi fork: {}", e));
            return;
        }

        let loaded = load_session_from_path(&new_file);
        self.apply_loaded_session(loaded);
        Self::send_response(
            id,
            "fork",
            json!({ "text": target_text, "cancelled": false }),
        );
    }

    fn handle_clone_command(&self, id: &str) {
        {
            let m = self.mutable.lock().unwrap();
            if !matches!(m.active_op, ActiveOp::Idle) {
                Self::send_error(id, "clone", "Tidak bisa clone saat agen sedang sibuk");
                return;
            }
        }
        let (old_file, path_entries) = {
            let m = self.mutable.lock().unwrap();
            let s = self.ui_state.lock().unwrap();
            (
                m.session_file.clone(),
                active_branch_entries(&s.entries, s.leaf_id.as_deref()),
            )
        };

        let new_file = unique_session_file_path(
            &self.config_dir,
            &self.cwd,
            old_file.parent(),
        );
        let new_session_id = new_unique_session_id();
        if let Err(e) = write_branched_session_file(
            &new_file,
            &new_session_id,
            &self.cwd,
            Some(&old_file),
            &path_entries,
        ) {
            Self::send_error(id, "clone", &format!("Gagal menyalin sesi: {}", e));
            return;
        }

        let loaded = load_session_from_path(&new_file);
        self.apply_loaded_session(loaded);
        Self::send_response(id, "clone", json!({ "cancelled": false }));
    }

    fn handle_new_session_command(&self, id: &str, parent_session: Option<&str>) {
        {
            let m = self.mutable.lock().unwrap();
            if !matches!(m.active_op, ActiveOp::Idle) {
                Self::send_error(id, "new_session", "Tidak bisa new_session saat agen sedang sibuk");
                return;
            }
        }
        let cur_dir = self.mutable.lock().unwrap().session_file.parent().map(Path::to_path_buf);
        let new_file = unique_session_file_path(
            &self.config_dir,
            &self.cwd,
            cur_dir.as_deref(),
        );
        let new_session_id = new_unique_session_id();
        let parent_path = parent_session.filter(|s| !s.trim().is_empty()).map(Path::new);
        if let Err(e) = write_branched_session_file(
            &new_file,
            &new_session_id,
            &self.cwd,
            parent_path,
            &[],
        ) {
            Self::send_error(id, "new_session", &format!("Gagal membuat sesi baru: {}", e));
            return;
        }

        let loaded = load_session_from_path(&new_file);
        self.apply_loaded_session(loaded);
        Self::send_response(id, "new_session", json!({ "cancelled": false }));
    }

    fn handle_switch_session_command(&self, id: &str, session_path: &str) {
        {
            let m = self.mutable.lock().unwrap();
            if !matches!(m.active_op, ActiveOp::Idle) {
                Self::send_error(id, "switch_session", "Tidak bisa pindah sesi saat agen sedang sibuk");
                return;
            }
        }
        let trimmed = session_path.trim();
        if trimmed.is_empty() {
            Self::send_error(id, "switch_session", "sessionPath wajib diisi");
            return;
        }
        let path = expand_home_or_resolve(trimmed, &self.cwd);
        if !path.exists() {
            Self::send_error(
                id,
                "switch_session",
                &format!("File sesi tidak ditemukan: {}", path.display()),
            );
            return;
        }
        let loaded = load_session_from_path(&path);
        self.apply_loaded_session(loaded);
        Self::send_response(id, "switch_session", json!({ "cancelled": false }));
    }

    fn handle_export_html_command(&self, id: &str, output_path: Option<&str>) {
        let (session_file, session_id, session_name) = {
            let m = self.mutable.lock().unwrap();
            (
                m.session_file.clone(),
                m.session_id.clone(),
                m.session_name.clone(),
            )
        };
        let ui_messages = self.ui_state.lock().unwrap().ui_messages.clone();

        let target_path = match output_path.map(str::trim).filter(|s| !s.is_empty()) {
            Some(raw) => expand_home_or_resolve(raw, &self.cwd),
            None => {
                let stem = session_file
                    .file_stem()
                    .and_then(|s| s.to_str())
                    .filter(|s| !s.is_empty())
                    .unwrap_or(&session_id);
                self.cwd.join(format!("pi-session-{}.html", stem))
            }
        };

        if let Some(parent) = target_path.parent().filter(|p| !p.as_os_str().is_empty()) {
            if let Err(e) = std::fs::create_dir_all(parent) {
                Self::send_error(
                    id,
                    "export_html",
                    &format!("Gagal membuat direktori output HTML: {}", e),
                );
                return;
            }
        }

        let html = render_session_html(
            &session_id,
            session_name.as_deref(),
            &self.cwd,
            &ui_messages,
        );
        if let Err(e) = std::fs::write(&target_path, html) {
            Self::send_error(
                id,
                "export_html",
                &format!("Gagal menulis file HTML '{}': {}", target_path.display(), e),
            );
            return;
        }

        Self::send_response(
            id,
            "export_html",
            json!({ "path": target_path.to_string_lossy() }),
        );
    }

    pub async fn run_loop_with_input<R: BufRead + Send + 'static>(
        self: &Arc<Self>,
        mut reader: R,
    ) -> Result<(), BoxError> {
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

        let reader_thread = std::thread::spawn(move || {
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

        let mut tasks: tokio::task::JoinSet<()> = tokio::task::JoinSet::new();
        let (resume_tx, mut resume_rx) = tokio::sync::mpsc::unbounded_channel::<(
            QueuedPrompt,
            tokio::sync::watch::Receiver<bool>,
            u64,
        )>();

        loop {
            while tasks.try_join_next().is_some() {}

            let line = tokio::select! {
                biased;
                maybe_resumed = resume_rx.recv() => {
                    if let Some((first, cancel_rx, seq)) = maybe_resumed {
                        let rt = Arc::clone(self);
                        tasks.spawn(async move {
                            Self::run_prompt_coordinator(rt, first, cancel_rx, seq).await;
                        });
                    }
                    continue;
                }
                maybe_line = rx.recv() => {
                    match maybe_line {
                        Some(line) => line,
                        None => break,
                    }
                }
            };

            let trimmed = line.trim_end_matches(&['\r', '\n'][..]);
            if trimmed.is_empty() {
                continue;
            }
            let cmd = match serde_json::from_str::<Value>(trimmed) {
                Ok(v) => v,
                Err(e) => {
                    Self::send_line(&json!({
                        "type": "response",
                        "command": "parse",
                        "success": false,
                        "error": format!("Failed to parse command: {}", e),
                    }));
                    continue;
                }
            };
            let cmd_type = cmd["type"].as_str().unwrap_or_default().to_string();
            let id = cmd["id"].as_str().unwrap_or_default().to_string();

            match cmd_type.as_str() {
                "pipkin_channel" | "oopbot_channel" => {
                    let topic = cmd["topic"].as_str().unwrap_or_default();
                    let data = &cmd["data"];
                    self.handle_channel_message(topic, data);
                }
                "get_state" => {
                    self.refresh_credentials();
                    let is_idle = {
                        let m = self.mutable.lock().unwrap();
                        matches!(m.active_op, ActiveOp::Idle)
                    };
                    if !is_idle || self.needs_auto_scan().is_none() {
                        Self::send_response(&id, "get_state", self.state_json());
                    } else {
                        let rt = Arc::clone(self);
                        tasks.spawn(async move {
                            rt.maybe_auto_scan().await;
                            Self::send_response(&id, "get_state", rt.state_json());
                        });
                    }
                }
                "get_messages" => {
                    let messages = self.ui_state.lock().unwrap().ui_messages.clone();
                    Self::send_response(&id, "get_messages", json!({ "messages": messages }));
                }
                "get_session_stats" => {
                    self.handle_session_stats_command(&id);
                }
                "get_available_models" => {
                    self.refresh_credentials();
                    let is_idle = {
                        let m = self.mutable.lock().unwrap();
                        matches!(m.active_op, ActiveOp::Idle)
                    };
                    if !is_idle || self.needs_auto_scan().is_none() {
                        Self::send_response(
                            &id,
                            "get_available_models",
                            json!({ "models": self.available_models_json() }),
                        );
                    } else {
                        let rt = Arc::clone(self);
                        tasks.spawn(async move {
                            rt.maybe_auto_scan().await;
                            Self::send_response(
                                &id,
                                "get_available_models",
                                json!({ "models": rt.available_models_json() }),
                            );
                        });
                    }
                }
                "set_model" => {
                    let mut provider_changed = false;
                    {
                        let mut m = self.mutable.lock().unwrap();
                        if let Some(p) = cmd["provider"].as_str().filter(|s| !s.trim().is_empty())
                        {
                            if p != m.active_provider {
                                m.active_provider = p.to_string();
                                provider_changed = true;
                            }
                        }
                        if let Some(mid) =
                            cmd["modelId"].as_str().filter(|s| !s.trim().is_empty())
                        {
                            m.active_model = mid.to_string();
                        }
                    }
                    self.refresh_credentials();
                    if provider_changed && self.needs_auto_scan().is_some() {
                        let rt = Arc::clone(self);
                        tasks.spawn(async move {
                            rt.maybe_auto_scan().await;
                            rt.sync_agent_active_model();
                            rt.sync_thinking_level_for_active_model();
                            let (ap, am, tl) = {
                                let m = rt.mutable.lock().unwrap();
                                (
                                    m.active_provider.clone(),
                                    m.active_model.clone(),
                                    m.thinking_level.clone(),
                                )
                            };
                            save_settings_fields(&rt.config_dir, Some(&ap), Some(&am), Some(&tl));
                            rt.append_session_entry(
                                "model_change",
                                json!({ "provider": ap, "modelId": am }),
                            );
                            Self::send_response(&id, "set_model", rt.current_model_json());
                        });
                    } else {
                        self.sync_agent_active_model();
                        self.sync_thinking_level_for_active_model();
                        let (ap, am, tl) = {
                            let m = self.mutable.lock().unwrap();
                            (
                                m.active_provider.clone(),
                                m.active_model.clone(),
                                m.thinking_level.clone(),
                            )
                        };
                        save_settings_fields(&self.config_dir, Some(&ap), Some(&am), Some(&tl));
                        self.append_session_entry(
                            "model_change",
                            json!({ "provider": ap, "modelId": am }),
                        );
                        Self::send_response(&id, "set_model", self.current_model_json());
                    }
                }
                "cycle_model" => {
                    self.refresh_credentials();
                    let models = self.available_models_json();
                    if models.len() <= 1 {
                        Self::send_response(&id, "cycle_model", Value::Null);
                    } else {
                        let (cur_p, cur_m) = {
                            let m = self.mutable.lock().unwrap();
                            (m.active_provider.clone(), m.active_model.clone())
                        };
                        let cur_idx = models
                            .iter()
                            .position(|item| {
                                item["provider"].as_str() == Some(cur_p.as_str())
                                    && item["id"].as_str() == Some(cur_m.as_str())
                            })
                            .unwrap_or(0);
                        let next = &models[(cur_idx + 1) % models.len()];
                        if let Some(np) = next["provider"].as_str() {
                            self.mutable.lock().unwrap().active_provider = np.to_string();
                        }
                        if let Some(nm) = next["id"].as_str() {
                            self.mutable.lock().unwrap().active_model = nm.to_string();
                        }
                        self.refresh_credentials();
                        self.sync_agent_active_model();
                        self.sync_thinking_level_for_active_model();
                        let (ap, am, tl) = {
                            let m = self.mutable.lock().unwrap();
                            (
                                m.active_provider.clone(),
                                m.active_model.clone(),
                                m.thinking_level.clone(),
                            )
                        };
                        save_settings_fields(&self.config_dir, Some(&ap), Some(&am), Some(&tl));
                        self.append_session_entry(
                            "model_change",
                            json!({ "provider": ap, "modelId": am }),
                        );
                        Self::send_response(
                            &id,
                            "cycle_model",
                            json!({
                                "model": self.current_model_json(),
                                "thinkingLevel": tl,
                                "isScoped": false,
                            }),
                        );
                    }
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
                        {
                            self.mutable.lock().unwrap().thinking_level = clamped.clone();
                        }
                        self.agent.set_thinking_level(&clamped);
                        save_settings_fields(&self.config_dir, None, None, Some(&clamped));
                        self.append_session_entry(
                            "thinking_level_change",
                            json!({ "thinkingLevel": clamped }),
                        );
                        Self::send_line(&json!({
                            "type": "thinking_level_changed",
                            "level": clamped,
                        }));
                    }
                    Self::send_response(&id, "set_thinking_level", json!({}));
                }
                "cycle_thinking_level" => {
                    let info = self.active_model_info();
                    let levels = info.available_thinking_levels();
                    if levels.len() <= 1 {
                        Self::send_response(&id, "cycle_thinking_level", Value::Null);
                    } else {
                        let cur = self.mutable.lock().unwrap().thinking_level.clone();
                        let idx = levels.iter().position(|l| l == &cur).unwrap_or(0);
                        let next_lvl = levels[(idx + 1) % levels.len()].clone();
                        {
                            self.mutable.lock().unwrap().thinking_level = next_lvl.clone();
                        }
                        self.agent.set_thinking_level(&next_lvl);
                        save_settings_fields(&self.config_dir, None, None, Some(&next_lvl));
                        self.append_session_entry(
                            "thinking_level_change",
                            json!({ "thinkingLevel": next_lvl }),
                        );
                        Self::send_line(&json!({
                            "type": "thinking_level_changed",
                            "level": next_lvl,
                        }));
                        Self::send_response(
                            &id,
                            "cycle_thinking_level",
                            json!({ "level": next_lvl }),
                        );
                    }
                }
                "set_session_name" => {
                    let name = cmd["name"]
                        .as_str()
                        .map(|s| s.trim().to_string())
                        .filter(|s| !s.is_empty());
                    {
                        self.mutable.lock().unwrap().session_name = name.clone();
                    }
                    self.append_session_entry("session_info", json!({ "name": name }));
                    Self::send_response(&id, "set_session_name", json!({}));
                    Self::send_line(&json!({
                        "type": "session_info_changed",
                        "name": name,
                    }));
                }
                "set_steering_mode" => {
                    let mode_str = cmd["mode"].as_str().unwrap_or_default();
                    if let Some(mode) = QueueMode::parse(mode_str) {
                        self.mutable.lock().unwrap().steering_mode = mode;
                        Self::send_response(&id, "set_steering_mode", json!({}));
                    } else {
                        Self::send_error(
                            &id,
                            "set_steering_mode",
                            &format!("Mode steering '{}' tidak valid", mode_str),
                        );
                    }
                }
                "set_follow_up_mode" => {
                    let mode_str = cmd["mode"].as_str().unwrap_or_default();
                    if let Some(mode) = QueueMode::parse(mode_str) {
                        self.mutable.lock().unwrap().follow_up_mode = mode;
                        Self::send_response(&id, "set_follow_up_mode", json!({}));
                    } else {
                        Self::send_error(
                            &id,
                            "set_follow_up_mode",
                            &format!("Mode follow-up '{}' tidak valid", mode_str),
                        );
                    }
                }
                "set_auto_compaction" => {
                    if let Some(enabled) = cmd["enabled"].as_bool() {
                        self.mutable.lock().unwrap().auto_compaction_enabled = enabled;
                        Self::send_response(&id, "set_auto_compaction", json!({}));
                    } else {
                        Self::send_error(
                            &id,
                            "set_auto_compaction",
                            "Parameter boolean 'enabled' wajib diisi",
                        );
                    }
                }
                "set_auto_retry" => {
                    if let Some(enabled) = cmd["enabled"].as_bool() {
                        self.mutable.lock().unwrap().auto_retry_enabled = enabled;
                        Self::send_response(&id, "set_auto_retry", json!({}));
                    } else {
                        Self::send_error(
                            &id,
                            "set_auto_retry",
                            "Parameter boolean 'enabled' wajib diisi",
                        );
                    }
                }
                "abort_retry" => {
                    if let Some(ref rtx) = self.mutable.lock().unwrap().retry_cancel_tx {
                        let _ = rtx.send(true);
                    }
                    Self::send_response(&id, "abort_retry", json!({}));
                }
                "clear_queue" => {
                    let (steering_texts, follow_up_texts) = {
                        let mut m = self.mutable.lock().unwrap();
                        let s: Vec<String> =
                            m.steering_queue.drain(..).map(|q| q.message).collect();
                        let f: Vec<String> =
                            m.follow_up_queue.drain(..).map(|q| q.message).collect();
                        m.resume_queues_after_compact = false;
                        (s, f)
                    };
                    // Emit queue_update kosong SEBELUM membalas clear_queue (ChatSteering.kt#L81)
                    Self::send_line(&json!({
                        "type": "queue_update",
                        "steering": [],
                        "followUp": [],
                    }));
                    Self::send_response(
                        &id,
                        "clear_queue",
                        json!({
                            "steering": steering_texts,
                            "followUp": follow_up_texts,
                        }),
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
                    let s = self.ui_state.lock().unwrap();
                    let branch = active_branch_entries(&s.entries, s.leaf_id.as_deref());
                    let fork_msgs: Vec<Value> = branch
                        .iter()
                        .filter(|e| {
                            e["type"].as_str() == Some("message")
                                && e["message"]["role"].as_str() == Some("user")
                        })
                        .filter_map(|e| {
                            let eid = e["id"].as_str()?;
                            let text = extract_user_message_text(&e["message"]);
                            Some(json!({ "entryId": eid, "text": text }))
                        })
                        .collect();
                    Self::send_response(
                        &id,
                        "get_fork_messages",
                        json!({ "messages": fork_msgs }),
                    );
                }
                "get_entries" => {
                    let since = cmd["since"].as_str();
                    let s = self.ui_state.lock().unwrap();
                    if let Some(cursor) = since {
                        if let Some(pos) =
                            s.entries.iter().position(|e| e["id"].as_str() == Some(cursor))
                        {
                            Self::send_response(
                                &id,
                                "get_entries",
                                json!({
                                    "entries": s.entries[pos + 1..],
                                    "leafId": s.leaf_id,
                                }),
                            );
                        } else {
                            Self::send_error(
                                &id,
                                "get_entries",
                                &format!("Entry '{}' tidak ditemukan", cursor),
                            );
                        }
                    } else {
                        Self::send_response(
                            &id,
                            "get_entries",
                            json!({
                                "entries": s.entries,
                                "leafId": s.leaf_id,
                            }),
                        );
                    }
                }
                "fork" => {
                    let entry_id = cmd["entryId"].as_str().unwrap_or_default();
                    self.handle_fork_command(&id, entry_id);
                }
                "clone" => {
                    self.handle_clone_command(&id);
                }
                "new_session" => {
                    self.handle_new_session_command(&id, cmd["parentSession"].as_str());
                }
                "switch_session" => {
                    let session_path = cmd["sessionPath"].as_str().unwrap_or_default();
                    self.handle_switch_session_command(&id, session_path);
                }
                "export_html" => {
                    self.handle_export_html_command(&id, cmd["outputPath"].as_str());
                }
                "compact" => {
                    let custom_instructions = cmd["customInstructions"]
                        .as_str()
                        .map(str::trim)
                        .filter(|s| !s.is_empty())
                        .map(str::to_string);

                    let reserved = {
                        let mut m = self.mutable.lock().unwrap();
                        if !matches!(m.active_op, ActiveOp::Idle) {
                            None
                        } else {
                            let (cancel_tx, cancel_rx) = tokio::sync::watch::channel(false);
                            let seq = m.next_op_seq;
                            m.next_op_seq += 1;
                            m.current_op_seq = seq;
                            m.active_op = ActiveOp::Compacting { cancel_tx };
                            Some((cancel_rx, seq))
                        }
                    };
                    let Some((cancel_rx, seq)) = reserved else {
                        Self::send_error(
                            &id,
                            "compact",
                            "Agen sedang memproses operasi lain",
                        );
                        continue;
                    };

                    Self::send_line(&json!({
                        "type": "compaction_start",
                        "reason": "manual",
                    }));
                    let rt = Arc::clone(self);
                    let rtx = resume_tx.clone();
                    tasks.spawn(async move {
                        Self::run_compact_task(rt, id, custom_instructions, cancel_rx, seq, rtx)
                            .await;
                    });
                }
                "abort" => {
                    let mut completed_rx = self.completed_seq_tx.subscribe();
                    let wait_target = {
                        let m = self.mutable.lock().unwrap();
                        if let Some(ref rtx) = m.retry_cancel_tx {
                            let _ = rtx.send(true);
                        }
                        match &m.active_op {
                            ActiveOp::Idle => None,
                            ActiveOp::Streaming { cancel_tx }
                            | ActiveOp::Compacting { cancel_tx } => {
                                let _ = cancel_tx.send(true);
                                Some(m.current_op_seq)
                            }
                        }
                    };
                    if let Some(target_seq) = wait_target {
                        tasks.spawn(async move {
                            while *completed_rx.borrow_and_update() < target_seq {
                                if completed_rx.changed().await.is_err() {
                                    break;
                                }
                            }
                            Self::send_response(&id, "abort", json!({}));
                        });
                    } else {
                        Self::send_response(&id, "abort", json!({}));
                    }
                }
                "prompt" => {
                    let message = cmd["message"].as_str().unwrap_or_default().to_string();
                    let images = extract_prompt_images(&cmd["images"]);
                    let behavior = cmd["streamingBehavior"].as_str();

                    enum PromptDispatch {
                        Start {
                            cancel_rx: tokio::sync::watch::Receiver<bool>,
                            seq: u64,
                        },
                        Queued,
                        HandledEmpty,
                        Rejected(&'static str),
                    }

                    let action = {
                        let mut m = self.mutable.lock().unwrap();
                        if !matches!(m.active_op, ActiveOp::Idle) {
                            let is_compacting = matches!(m.active_op, ActiveOp::Compacting { .. });
                            match behavior {
                                Some("steer") => {
                                    m.steering_queue.push_back(QueuedPrompt {
                                        message: message.clone(),
                                        images: images.clone(),
                                    });
                                    if is_compacting {
                                        m.resume_queues_after_compact = true;
                                    }
                                    PromptDispatch::Queued
                                }
                                Some("followUp") => {
                                    m.follow_up_queue.push_back(QueuedPrompt {
                                        message: message.clone(),
                                        images: images.clone(),
                                    });
                                    if is_compacting {
                                        m.resume_queues_after_compact = true;
                                    }
                                    PromptDispatch::Queued
                                }
                                _ => PromptDispatch::Rejected(
                                    "Agen sedang berjalan; sertakan streamingBehavior ('steer' atau 'followUp')",
                                ),
                            }
                        } else if message.trim().is_empty() && images.is_empty() {
                            PromptDispatch::HandledEmpty
                        } else {
                            let (cancel_tx, cancel_rx) = tokio::sync::watch::channel(false);
                            let seq = m.next_op_seq;
                            m.next_op_seq += 1;
                            m.current_op_seq = seq;
                            m.active_op = ActiveOp::Streaming { cancel_tx };
                            PromptDispatch::Start { cancel_rx, seq }
                        }
                    };

                    match action {
                        PromptDispatch::Rejected(msg) => {
                            Self::send_error(&id, "prompt", msg);
                        }
                        PromptDispatch::HandledEmpty => {
                            Self::send_response(&id, "prompt", json!({ "disposition": "handled" }));
                        }
                        PromptDispatch::Queued => {
                            self.emit_queue_update();
                            Self::send_response(&id, "prompt", json!({ "disposition": "queued" }));
                        }
                        PromptDispatch::Start { cancel_rx, seq } => {
                            Self::send_response(&id, "prompt", json!({ "disposition": "started" }));
                            let rt = Arc::clone(self);
                            let first = QueuedPrompt { message, images };
                            tasks.spawn(async move {
                                Self::run_prompt_coordinator(rt, first, cancel_rx, seq).await;
                            });
                        }
                    }
                }
                "steer" => {
                    let message = cmd["message"].as_str().unwrap_or_default().to_string();
                    let images = extract_prompt_images(&cmd["images"]);
                    {
                        self.mutable.lock().unwrap().steering_queue.push_back(
                            QueuedPrompt { message, images },
                        );
                    }
                    self.emit_queue_update();
                    Self::send_response(&id, "steer", json!({ "disposition": "queued" }));
                }
                "follow_up" => {
                    let message = cmd["message"].as_str().unwrap_or_default().to_string();
                    let images = extract_prompt_images(&cmd["images"]);
                    {
                        let mut m = self.mutable.lock().unwrap();
                        m.follow_up_queue
                            .push_back(QueuedPrompt { message, images });
                        if matches!(m.active_op, ActiveOp::Compacting { .. }) {
                            m.resume_queues_after_compact = true;
                        }
                    }
                    self.emit_queue_update();
                    Self::send_response(&id, "follow_up", json!({ "disposition": "queued" }));
                }
                "extension_ui_response" => {}
                other => {
                    if !id.is_empty() {
                        Self::send_error(&id, other, &format!("Command '{}' tidak dikenal", other));
                    }
                }
            }
        }

        // EOF / stdin ditutup: batalkan operasi aktif dan tunggu seluruh task selesai dengan rapi.
        resume_rx.close();
        {
            let mut m = self.mutable.lock().unwrap();
            m.resume_queues_after_compact = false;
            if let Some(ref rtx) = m.retry_cancel_tx {
                let _ = rtx.send(true);
            }
            if let ActiveOp::Streaming { ref cancel_tx }
            | ActiveOp::Compacting { ref cancel_tx } = m.active_op
            {
                let _ = cancel_tx.send(true);
            }
        }
        while let Ok((first, cancel_rx, seq)) = resume_rx.try_recv() {
            let rt = Arc::clone(self);
            tasks.spawn(async move {
                Self::run_prompt_coordinator(rt, first, cancel_rx, seq).await;
            });
        }
        while tasks.join_next().await.is_some() {}
        let _ = reader_thread.join();
        Ok(())
    }

    /// Tangani pesan event masuk dari Android UI atau ekstensi via Pipkin Channel
    fn handle_channel_message(&self, topic: &str, data: &Value) {
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
                            Self::send_channel(
                                "subagents",
                                json!({
                                    "runs": [],
                                    "standingAside": false
                                }),
                            );
                        }
                        "check" => {
                            Self::send_channel(
                                "subagents",
                                json!({
                                    "runs": [],
                                    "standingAside": false
                                }),
                            );
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
            _ => {}
        }
    }
}

async fn wait_watch_true(rx: &mut tokio::sync::watch::Receiver<bool>) {
    if *rx.borrow() {
        return;
    }
    while rx.changed().await.is_ok() {
        if *rx.borrow() {
            return;
        }
    }
    std::future::pending::<()>().await;
}

fn is_transient_llm_error(err: &str) -> bool {
    let low = err.to_ascii_lowercase();
    low.contains("429")
        || low.contains("500")
        || low.contains("502")
        || low.contains("503")
        || low.contains("504")
        || low.contains("overloaded")
        || low.contains("rate limit")
        || low.contains("timed out")
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

#[derive(Debug, Clone, Copy, Default)]
pub struct TokenTotals {
    pub input: u64,
    pub output: u64,
    pub cache_read: u64,
    pub cache_write: u64,
}

impl TokenTotals {
    pub fn total(self) -> u64 {
        self.input + self.output + self.cache_read + self.cache_write
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
    pub token_totals: TokenTotals,
    pub last_context_tokens: Option<u64>,
    pub context_unknown: bool,
    pub parent_session: Option<String>,
    pub entries: Vec<Value>,
    pub leaf_id: Option<String>,
}

impl RpcUiState {
    pub fn new(session_file: Option<PathBuf>, entry_seq: usize) -> Self {
        Self {
            ui_messages: Vec::new(),
            session_file,
            entry_seq,
            total_tokens: 0,
            total_cost: 0.0,
            token_totals: TokenTotals::default(),
            last_context_tokens: None,
            context_unknown: false,
            parent_session: None,
            entries: Vec::new(),
            leaf_id: None,
        }
    }

    fn from_loaded_session(loaded: LoadedSession) -> Self {
        Self {
            ui_messages: loaded.ui_messages,
            session_file: Some(loaded.session_file),
            entry_seq: loaded.entry_seq,
            total_tokens: loaded.total_tokens,
            total_cost: loaded.total_cost,
            token_totals: loaded.token_totals,
            last_context_tokens: loaded.last_context_tokens,
            context_unknown: loaded.context_unknown,
            parent_session: loaded.parent_session,
            entries: loaded.entries,
            leaf_id: loaded.leaf_id,
        }
    }

    pub fn next_unique_entry_id(&mut self) -> String {
        loop {
            let candidate = format!("e{}", self.entry_seq);
            self.entry_seq += 1;
            if !self
                .entries
                .iter()
                .any(|e| e["id"].as_str() == Some(candidate.as_str()))
            {
                return candidate;
            }
        }
    }

    pub fn append_session_entry_with_id(
        &mut self,
        entry_id: &str,
        entry_type: &str,
        mut payload: Value,
    ) -> String {
        let parent_id = self
            .leaf_id
            .as_ref()
            .map(|s| json!(s))
            .unwrap_or(Value::Null);

        payload["type"] = json!(entry_type);
        payload["id"] = json!(entry_id);
        payload["parentId"] = parent_id;
        payload["timestamp"] = json!(iso_now());
        self.entries.push(payload.clone());
        self.leaf_id = Some(entry_id.to_string());

        if let Some(path) = &self.session_file {
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
        entry_id.to_string()
    }

    pub fn append_session_entry_returning_id(
        &mut self,
        entry_type: &str,
        payload: Value,
    ) -> String {
        let id = self.next_unique_entry_id();
        self.append_session_entry_with_id(&id, entry_type, payload)
    }

    pub fn append_session_entry(&mut self, entry_type: &str, payload: Value) {
        let _ = self.append_session_entry_returning_id(entry_type, payload);
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

type EventEmitter = Box<dyn Fn(&Value) + Send + Sync>;

/// Sink event yang menerjemahkan deretan [`AgentEvent`] dari loop agen menjadi
/// baris-baris JSONL protokol Pipkin (`agent_start`, `message_start`, `message_update`,
/// `tool_execution_start/end`, `toolResult`, `turn_end`, dll.).
pub struct RpcEventSink {
    ui_state: Arc<Mutex<RpcUiState>>,
    internal: Mutex<RpcEventSinkInternal>,
    emitter: Option<EventEmitter>,
    defer_settled: Arc<AtomicBool>,
    plugins: Option<Arc<PluginRegistry>>,
}

impl RpcEventSink {
    pub fn new(ui_state: Arc<Mutex<RpcUiState>>) -> Self {
        Self {
            ui_state,
            internal: Mutex::new(RpcEventSinkInternal::default()),
            emitter: None,
            defer_settled: Arc::new(AtomicBool::new(false)),
            plugins: None,
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
            defer_settled: Arc::new(AtomicBool::new(false)),
            plugins: None,
        }
    }

    pub fn with_defer_settled(mut self, defer_settled: Arc<AtomicBool>) -> Self {
        self.defer_settled = defer_settled;
        self
    }

    pub fn with_plugins(mut self, plugins: Arc<PluginRegistry>) -> Self {
        self.plugins = Some(plugins);
        self
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

                if let Some(plugins) = &self.plugins {
                    match tool_name.as_str() {
                        "todo" => {
                            if let Some(plugin) = plugins.get_plugin("pipkin-todo") {
                                if let Some(lines) = plugin.render_widget("todos") {
                                    self.emit(&json!({
                                        "type": "extension_ui_request",
                                        "id": "todos-widget",
                                        "method": "setWidget",
                                        "widgetKey": "todos",
                                        "widgetLines": lines,
                                        "widgetPlacement": "aboveEditor"
                                    }));
                                } else {
                                    self.emit(&json!({
                                        "type": "extension_ui_request",
                                        "id": "todos-widget",
                                        "method": "setWidget",
                                        "widgetKey": "todos",
                                        "widgetPlacement": "aboveEditor"
                                    }));
                                }
                            }
                        }
                        "background" => {
                            if let Some(plugin) = plugins.get_plugin("pipkin-background") {
                                if let Some((topic, data)) = plugin.channel_status() {
                                    self.emit(&json!({
                                        "type": "pipkin_channel",
                                        "topic": topic,
                                        "data": data
                                    }));
                                }
                            }
                        }
                        _ => {}
                    }
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
                outcome,
            } => {
                {
                    let mut internal = self.internal.lock().unwrap();
                    if internal.assistant_msg_started {
                        if !internal.content_blocks.is_empty() {
                            let stop_reason = if *outcome == StepsOutcome::Stopped {
                                "aborted"
                            } else {
                                "stop"
                            };
                            let assistant_msg = json!({
                                "role": "assistant",
                                "content": internal.content_blocks.clone(),
                                "stopReason": stop_reason,
                                "timestamp": internal.step_start_ms,
                                "durationMs": epoch_ms().saturating_sub(internal.step_start_ms),
                            });
                            self.emit(&json!({
                                "type": "message_end",
                                "message": assistant_msg
                            }));
                            let mut state = self.ui_state.lock().unwrap();
                            state.ui_messages.push(assistant_msg.clone());
                            state.append_session_entry("message", json!({ "message": assistant_msg }));
                        }
                        internal.assistant_msg_started = false;
                        internal.content_blocks.clear();
                        internal.current_content_idx = 0;
                    }
                }
                self.emit(&json!({ "type": "turn_end" }));
                self.emit(&json!({ "type": "agent_end" }));
                if !self.defer_settled.load(Ordering::SeqCst) {
                    self.emit(&json!({
                        "type": "agent_settled",
                        "aborted": *outcome == StepsOutcome::Stopped
                    }));
                }
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

fn extract_auth_key(auth_json: &Value, provider_id: &str) -> String {
    crate::models::extract_auth_credentials(auth_json, provider_id)
        .map(|(k, _)| k)
        .unwrap_or_default()
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


#[derive(Debug, Clone)]
struct LoadedSession {
    session_file: PathBuf,
    session_id: String,
    session_name: Option<String>,
    parent_session: Option<String>,
    entries: Vec<Value>,
    leaf_id: Option<String>,
    entry_seq: usize,
    ui_messages: Vec<Value>,
    llm_history: Vec<Value>,
    token_totals: TokenTotals,
    total_tokens: u64,
    total_cost: f64,
    last_context_tokens: Option<u64>,
    context_unknown: bool,
}

#[derive(Debug, Clone)]
struct RebuiltContext {
    ui_messages: Vec<Value>,
    llm_history: Vec<Value>,
    active_entries: Vec<Value>,
    previous_compaction_summary: Option<String>,
    token_totals: TokenTotals,
    total_tokens: u64,
    total_cost: f64,
    last_context_tokens: Option<u64>,
    context_unknown: bool,
}

#[derive(Debug, Clone)]
struct CompactionCut {
    to_summarize: Vec<Value>,
    first_kept_entry_id: Option<String>,
}

fn expand_home_or_resolve(raw: &str, cwd: &Path) -> PathBuf {
    let trimmed = raw.trim();
    if trimmed == "~" || trimmed.starts_with("~/") {
        if let Ok(home) = std::env::var("HOME") {
            let suffix = trimmed.strip_prefix("~/").unwrap_or("");
            if suffix.is_empty() {
                return PathBuf::from(home);
            }
            return PathBuf::from(home).join(suffix);
        }
    }
    let p = PathBuf::from(trimmed);
    if p.is_absolute() {
        p
    } else {
        cwd.join(p)
    }
}

fn default_sessions_dir(config_dir: &Path, cwd: &Path) -> PathBuf {
    let slug = format!(
        "--{}--",
        cwd.to_string_lossy()
            .trim_start_matches('/')
            .replace(['/', '\\', ':'], "-")
    );
    config_dir.join("sessions").join(slug)
}

fn new_unique_session_id() -> String {
    let seq = SESSION_UNIQUE_SEQ.fetch_add(1, Ordering::Relaxed);
    format!("oopbot-{}-{}", epoch_ms(), seq)
}

fn unique_session_file_path(
    config_dir: &Path,
    cwd: &Path,
    preferred_dir: Option<&Path>,
) -> PathBuf {
    let session_dir = preferred_dir
        .filter(|p| !p.as_os_str().is_empty())
        .map(Path::to_path_buf)
        .unwrap_or_else(|| default_sessions_dir(config_dir, cwd));
    let _ = std::fs::create_dir_all(&session_dir);
    let sid = new_unique_session_id();
    let file_ts = iso_now().replace([':', '.'], "-");
    session_dir.join(format!("{}_{}.jsonl", file_ts, sid))
}

fn write_branched_session_file(
    path: &Path,
    session_id: &str,
    cwd: &Path,
    parent_session: Option<&Path>,
    entries: &[Value],
) -> io::Result<()> {
    if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
        std::fs::create_dir_all(parent)?;
    }
    let mut header = json!({
        "type": "session",
        "version": 3,
        "id": session_id,
        "timestamp": iso_now(),
        "cwd": cwd.to_string_lossy(),
    });
    if let Some(ps) = parent_session {
        header["parentSession"] = json!(ps.to_string_lossy());
    }

    let mut lines = vec![serde_json::to_string(&header)?];
    let mut prev_id: Option<String> = None;
    for (idx, raw_entry) in entries.iter().enumerate() {
        let mut entry = raw_entry.clone();
        let eid = entry["id"]
            .as_str()
            .filter(|s| !s.is_empty())
            .map(str::to_string)
            .unwrap_or_else(|| format!("e{}", idx));
        entry["id"] = json!(eid.clone());
        entry["parentId"] = prev_id.as_ref().map(|p| json!(p)).unwrap_or(Value::Null);
        prev_id = Some(eid);
        lines.push(serde_json::to_string(&entry)?);
    }

    std::fs::write(path, format!("{}\n", lines.join("\n")))
}

fn load_session_from_path(path: &Path) -> LoadedSession {
    let mut session_id = new_unique_session_id();
    let mut parent_session: Option<String> = None;
    let mut entries: Vec<Value> = Vec::new();
    let mut leaf_id: Option<String> = None;
    let mut entry_seq: usize = 0;

    if let Ok(content) = std::fs::read_to_string(path) {
        for line in content.lines() {
            let trimmed = line.trim();
            if trimmed.is_empty() {
                continue;
            }
            let Ok(entry) = serde_json::from_str::<Value>(trimmed) else {
                continue;
            };
            if entry["type"].as_str() == Some("session") {
                if let Some(id) = entry["id"].as_str().filter(|s| !s.is_empty()) {
                    session_id = id.to_string();
                }
                if let Some(ps) = entry["parentSession"]
                    .as_str()
                    .filter(|s| !s.trim().is_empty())
                {
                    parent_session = Some(ps.to_string());
                }
                continue;
            }
            if let Some(eid) = entry["id"].as_str().filter(|s| !s.is_empty()) {
                leaf_id = Some(eid.to_string());
                if let Some(num) = eid.strip_prefix('e').and_then(|s| s.parse::<usize>().ok()) {
                    entry_seq = entry_seq.max(num + 1);
                }
            }
            entries.push(entry);
        }
    }
    entry_seq = entry_seq.max(entries.len());

    let path_entries = active_branch_entries(&entries, leaf_id.as_deref());
    let mut session_name: Option<String> = None;
    for e in &path_entries {
        if e["type"].as_str() == Some("session_info") {
            session_name = e["name"]
                .as_str()
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty());
        }
    }

    let rebuilt = rebuild_context_from_entries(&path_entries);
    LoadedSession {
        session_file: path.to_path_buf(),
        session_id,
        session_name,
        parent_session,
        entries,
        leaf_id,
        entry_seq,
        ui_messages: rebuilt.ui_messages,
        llm_history: rebuilt.llm_history,
        token_totals: rebuilt.token_totals,
        total_tokens: rebuilt.total_tokens,
        total_cost: rebuilt.total_cost,
        last_context_tokens: rebuilt.last_context_tokens,
        context_unknown: rebuilt.context_unknown,
    }
}

fn init_or_load_session(
    config_dir: &Path,
    cwd: &Path,
    session_path: Option<&str>,
) -> LoadedSession {
    if let Some(path_str) = session_path.filter(|s| !s.trim().is_empty()) {
        let path = expand_home_or_resolve(path_str, cwd);
        if path.exists() {
            return load_session_from_path(&path);
        }
        return LoadedSession {
            session_file: path,
            session_id: new_unique_session_id(),
            session_name: None,
            parent_session: None,
            entries: Vec::new(),
            leaf_id: None,
            entry_seq: 0,
            ui_messages: Vec::new(),
            llm_history: Vec::new(),
            token_totals: TokenTotals::default(),
            total_tokens: 0,
            total_cost: 0.0,
            last_context_tokens: None,
            context_unknown: false,
        };
    }

    let file_path = unique_session_file_path(config_dir, cwd, None);
    LoadedSession {
        session_file: file_path,
        session_id: new_unique_session_id(),
        session_name: None,
        parent_session: None,
        entries: Vec::new(),
        leaf_id: None,
        entry_seq: 0,
        ui_messages: Vec::new(),
        llm_history: Vec::new(),
        token_totals: TokenTotals::default(),
        total_tokens: 0,
        total_cost: 0.0,
        last_context_tokens: None,
        context_unknown: false,
    }
}

fn active_branch_entries(entries: &[Value], leaf_id: Option<&str>) -> Vec<Value> {
    if entries.is_empty() {
        return Vec::new();
    }
    let Some(target_leaf) = leaf_id else {
        return entries.to_vec();
    };

    let mut by_id: HashMap<&str, usize> = HashMap::with_capacity(entries.len());
    for (idx, e) in entries.iter().enumerate() {
        if let Some(id) = e["id"].as_str().filter(|s| !s.is_empty()) {
            by_id.insert(id, idx);
        }
    }
    if !by_id.contains_key(target_leaf) {
        return entries.to_vec();
    }

    let mut path_rev = Vec::new();
    let mut visited = HashSet::new();
    let mut cur_opt: Option<&str> = Some(target_leaf);

    while let Some(cur_id) = cur_opt {
        if !visited.insert(cur_id) {
            break;
        }
        let Some(&idx) = by_id.get(cur_id) else {
            break;
        };
        let entry = &entries[idx];
        path_rev.push(entry.clone());

        cur_opt = match entry.get("parentId") {
            Some(Value::String(pid)) if !pid.is_empty() => Some(pid.as_str()),
            Some(Value::Null) => None,
            // Legacy v1 tanpa field parentId -> sambungkan ke entry sebelumnya dalam file
            None if idx > 0 => entries[idx - 1]["id"].as_str(),
            _ => None,
        };
    }

    path_rev.reverse();
    path_rev
}

fn parse_usage_fields(usage: &Value) -> (TokenTotals, f64) {
    if !usage.is_object() {
        return (TokenTotals::default(), 0.0);
    }
    let input = usage["input"].as_u64().unwrap_or(0);
    let output = usage["output"].as_u64().unwrap_or(0);
    let cache_read = usage["cacheRead"].as_u64().unwrap_or(0);
    let cache_write = usage["cacheWrite"].as_u64().unwrap_or(0);
    let cost = usage
        .get("cost")
        .and_then(|c| c["total"].as_f64())
        .or_else(|| usage["cost"].as_f64())
        .unwrap_or(0.0);
    (
        TokenTotals {
            input,
            output,
            cache_read,
            cache_write,
        },
        cost,
    )
}

fn apply_context_replacement(msg: &Value, replacement: &Value) -> Value {
    let mut updated = msg.clone();
    let role = msg["role"].as_str().unwrap_or_default();
    if let Some(s) = replacement.as_str() {
        if role == "assistant" || role == "toolResult" {
            updated["content"] = json!([{ "type": "text", "text": s }]);
        } else {
            updated["content"] = json!(s);
        }
    } else {
        updated["content"] = replacement.clone();
    }
    updated
}

fn rebuild_context_from_entries(path_entries: &[Value]) -> RebuiltContext {
    let mut token_totals = TokenTotals::default();
    let mut computed_cost = 0.0f64;
    let mut stats_floor_tokens = 0u64;
    let mut stats_floor_cost = 0.0f64;
    let mut last_context_tokens: Option<u64> = None;
    let mut context_unknown = false;

    for entry in path_entries {
        match entry["type"].as_str().unwrap_or_default() {
            "session_stats" => {
                if let Some(t) = entry["total_tokens"].as_u64() {
                    stats_floor_tokens = stats_floor_tokens.max(t);
                }
                if let Some(c) = entry["total_cost"].as_f64() {
                    stats_floor_cost = stats_floor_cost.max(c);
                }
            }
            "usage" => {
                let (u, c) = parse_usage_fields(&entry["usage"]);
                token_totals.input += u.input;
                token_totals.output += u.output;
                token_totals.cache_read += u.cache_read;
                token_totals.cache_write += u.cache_write;
                computed_cost += c;
                if let Some(ctx) = entry["lastContextTokens"].as_u64().filter(|&n| n > 0) {
                    last_context_tokens = Some(ctx);
                    context_unknown = false;
                }
            }
            "compaction" => {
                let (u, c) = parse_usage_fields(&entry["usage"]);
                token_totals.input += u.input;
                token_totals.output += u.output;
                token_totals.cache_read += u.cache_read;
                token_totals.cache_write += u.cache_write;
                computed_cost += c;
                last_context_tokens = None;
                context_unknown = true;
            }
            "branch_summary" => {
                let (u, c) = parse_usage_fields(&entry["usage"]);
                token_totals.input += u.input;
                token_totals.output += u.output;
                token_totals.cache_read += u.cache_read;
                token_totals.cache_write += u.cache_write;
                computed_cost += c;
            }
            "message" => {
                let msg = &entry["message"];
                if msg["role"].as_str() == Some("assistant") {
                    if let Some(usage) = msg.get("usage") {
                        let (u, u_cost) = parse_usage_fields(usage);
                        token_totals.input += u.input;
                        token_totals.output += u.output;
                        token_totals.cache_read += u.cache_read;
                        token_totals.cache_write += u.cache_write;
                        computed_cost += u_cost;

                        let ctx = usage["totalTokens"]
                            .as_u64()
                            .filter(|&n| n > 0)
                            .unwrap_or_else(|| u.total());
                        if ctx > 0 {
                            last_context_tokens = Some(ctx);
                            context_unknown = false;
                        }
                    }
                    if let Some(top_cost) = msg
                        .get("cost")
                        .and_then(|c| c["total"].as_f64())
                        .or_else(|| msg["cost"].as_f64())
                    {
                        computed_cost += top_cost;
                    }
                }
            }
            _ => {}
        }
    }

    let last_compact_idx = path_entries
        .iter()
        .rposition(|e| e["type"].as_str() == Some("compaction"));
    let previous_compaction_summary = last_compact_idx
        .and_then(|ci| path_entries[ci]["summary"].as_str())
        .map(str::to_string);

    let mut selected_entries: Vec<&Value> = Vec::new();
    if let Some(ci) = last_compact_idx {
        let compact_entry = &path_entries[ci];
        let compact_id = compact_entry["id"].as_str().unwrap_or("");
        let first_kept_id = compact_entry["firstKeptEntryId"].as_str().unwrap_or("");
        selected_entries.push(compact_entry);

        if !first_kept_id.is_empty() && first_kept_id != compact_id {
            if let Some(pos) = path_entries[..ci]
                .iter()
                .position(|e| e["id"].as_str() == Some(first_kept_id))
            {
                for e in &path_entries[pos..ci] {
                    if e["type"].as_str() == Some("message")
                        && e["message"]["role"].as_str() == Some("system")
                    {
                        continue;
                    }
                    selected_entries.push(e);
                }
            }
        }
        for e in &path_entries[ci + 1..] {
            selected_entries.push(e);
        }
    } else {
        for e in path_entries {
            selected_entries.push(e);
        }
    }

    let mut context_edits: HashMap<&str, &Value> = HashMap::new();
    for e in path_entries {
        if e["type"].as_str() == Some("context_edit") {
            if let Some(tid) = e["targetId"].as_str().filter(|s| !s.is_empty()) {
                if let Some(repl) = e.get("replacement") {
                    context_edits.insert(tid, repl);
                }
            }
        }
    }

    let mut ui_messages = Vec::new();
    let mut context_messages = Vec::new();
    let mut active_entries = Vec::new();

    for entry in selected_entries {
        let entry_id = entry["id"].as_str();
        let entry_type = entry["type"].as_str().unwrap_or_default();
        let edit_override = entry_id.and_then(|eid| context_edits.get(eid).copied());

        match entry_type {
            "compaction" => {
                let msg = json!({
                    "role": "compactionSummary",
                    "summary": entry["summary"].as_str().unwrap_or_default(),
                    "tokensBefore": entry["tokensBefore"].as_u64().unwrap_or(0),
                    "timestamp": parse_iso_to_epoch_ms(entry["timestamp"].as_str()),
                });
                ui_messages.push(msg.clone());
                context_messages.push(msg);
            }
            "branch_summary" => {
                let msg = json!({
                    "role": "branchSummary",
                    "summary": entry["summary"].as_str().unwrap_or_default(),
                    "fromId": entry["fromId"],
                    "timestamp": parse_iso_to_epoch_ms(entry["timestamp"].as_str()),
                });
                ui_messages.push(msg.clone());
                context_messages.push(msg);
            }
            "custom_message" => {
                let display = entry["display"].as_bool().unwrap_or(true);
                let msg = json!({
                    "role": "custom",
                    "customType": entry["customType"].as_str().unwrap_or_default(),
                    "content": entry["content"],
                    "display": display,
                    "details": entry["details"],
                    "timestamp": parse_iso_to_epoch_ms(entry["timestamp"].as_str()),
                });
                if display {
                    ui_messages.push(msg.clone());
                }
                match edit_override {
                    Some(v) if v.is_null() => {}
                    Some(repl) => context_messages.push(apply_context_replacement(&msg, repl)),
                    None => context_messages.push(msg),
                }
                active_entries.push(entry.clone());
            }
            "bashExecution" => {
                let mut msg = entry.clone();
                if msg.get("role").and_then(|r| r.as_str()).is_none() {
                    msg["role"] = json!("bashExecution");
                }
                ui_messages.push(msg.clone());
                if msg["excludeFromContext"].as_bool() != Some(true) {
                    match edit_override {
                        Some(v) if v.is_null() => {}
                        Some(repl) => context_messages.push(apply_context_replacement(&msg, repl)),
                        None => context_messages.push(msg),
                    }
                }
                active_entries.push(entry.clone());
            }
            "message" => {
                let msg = &entry["message"];
                if msg.is_null() {
                    continue;
                }
                let role = msg["role"].as_str().unwrap_or_default();
                if role == "system" {
                    continue;
                }
                ui_messages.push(msg.clone());
                match edit_override {
                    Some(v) if v.is_null() => {}
                    Some(repl) => context_messages.push(apply_context_replacement(msg, repl)),
                    None => context_messages.push(msg.clone()),
                }
                active_entries.push(entry.clone());
            }
            _ => {}
        }
    }

    let llm_history = ui_history_to_llm(&context_messages);
    let total_tokens = token_totals.total().max(stats_floor_tokens);
    let total_cost = computed_cost.max(stats_floor_cost);

    RebuiltContext {
        ui_messages,
        llm_history,
        active_entries,
        previous_compaction_summary,
        token_totals,
        total_tokens,
        total_cost,
        last_context_tokens,
        context_unknown,
    }
}

fn reverse_map_tool_name(name: &str) -> &str {
    match name {
        "bash" => "run_shell",
        "read" => "read_file",
        "write" => "write_file",
        "ls" => "list_dir",
        other => other,
    }
}

fn extract_message_text_and_images(msg: &Value) -> (String, Vec<(String, String)>) {
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
    (text, images)
}

fn extract_user_message_text(msg: &Value) -> String {
    extract_message_text_and_images(msg).0
}

fn ui_message_to_llm(msg: &Value) -> Option<Value> {
    match msg["role"].as_str()? {
        "user" | "custom" => {
            let (text, images) = extract_message_text_and_images(msg);
            if text.is_empty() && images.is_empty() {
                None
            } else {
                Some(user_message_with_images(&text, &images))
            }
        }
        "bashExecution" => {
            if msg["excludeFromContext"].as_bool() == Some(true) {
                return None;
            }
            let cmd = msg["command"].as_str().unwrap_or_default();
            let out = msg["output"].as_str().unwrap_or_default();
            let code = msg["exitCode"].as_i64();
            let mut body = format!("$ {}", cmd);
            if !out.is_empty() {
                body.push('\n');
                body.push_str(out);
            }
            if let Some(c) = code {
                if c != 0 {
                    body.push_str(&format!("\n[exit code: {}]", c));
                }
            }
            Some(user_text(&body))
        }
        "compactionSummary" => {
            let summary = msg["summary"].as_str().unwrap_or_default().trim();
            if summary.is_empty() {
                None
            } else {
                Some(user_text(&format!(
                    "[Ringkasan Kompak Konteks Sebelumnya]\n{}",
                    summary
                )))
            }
        }
        "branchSummary" => {
            let summary = msg["summary"].as_str().unwrap_or_default().trim();
            if summary.is_empty() {
                None
            } else {
                Some(user_text(&format!(
                    "[Ringkasan Branch Sebelumnya]\n{}",
                    summary
                )))
            }
        }
        "assistant" => {
            let arr = msg["content"].as_array()?;
            let mut parts = Vec::new();
            for b in arr {
                match b["type"].as_str().unwrap_or_default() {
                    "thinking" => {
                        // Hanya kirim ulang thought bila memiliki thoughtSignature asli.
                        // Thinking sintetis tanpa signature tidak boleh diputar ulang ke LLM.
                        if let Some(sig) = b["thoughtSignature"]
                            .as_str()
                            .filter(|s| !s.trim().is_empty())
                        {
                            if let Some(th) = b["thinking"].as_str().filter(|s| !s.trim().is_empty())
                            {
                                parts.push(json!({
                                    "thought": true,
                                    "text": th,
                                    "thoughtSignature": sig,
                                }));
                            }
                        }
                    }
                    "text" => {
                        if let Some(t) = b["text"].as_str().filter(|s| !s.is_empty()) {
                            parts.push(json!({ "text": t }));
                        }
                    }
                    "toolCall" => {
                        let raw_name = b["name"].as_str().unwrap_or_default();
                        let internal_name = reverse_map_tool_name(raw_name);
                        let args = b
                            .get("arguments")
                            .or_else(|| b.get("args"))
                            .cloned()
                            .unwrap_or_else(|| json!({}));
                        let mut fc = json!({
                            "name": internal_name,
                            "args": args,
                        });
                        if let Some(cid) = b["id"].as_str().filter(|s| !s.is_empty()) {
                            fc["id"] = json!(cid);
                        }
                        let mut part = json!({ "functionCall": fc });
                        if let Some(sig) = b["thoughtSignature"]
                            .as_str()
                            .filter(|s| !s.trim().is_empty())
                        {
                            part["thoughtSignature"] = json!(sig);
                        }
                        parts.push(part);
                    }
                    _ => {}
                }
            }
            if parts.is_empty() {
                None
            } else {
                Some(json!({ "role": "model", "parts": parts }))
            }
        }
        _ => None,
    }
}

fn tool_result_to_response_payload(msg: &Value) -> Value {
    let text = extract_user_message_text(msg);
    if msg["isError"].as_bool() == Some(true) {
        json!({ "error": text })
    } else {
        json!({ "output": text })
    }
}

fn ui_history_to_llm(messages: &[Value]) -> Vec<Value> {
    let mut llm = Vec::new();
    let mut i = 0usize;
    let mut prev_was_summary_user = false;

    while i < messages.len() {
        let msg = &messages[i];
        let role = msg["role"].as_str().unwrap_or_default();

        if role == "toolResult" {
            // toolResult yatim tanpa didahului assistant toolCall diabaikan agar tidak memicu HTTP 400.
            i += 1;
            continue;
        }

        if role == "assistant" {
            let Some(mut llm_item) = ui_message_to_llm(msg) else {
                i += 1;
                continue;
            };
            prev_was_summary_user = false;

            let mut expected_calls: Vec<(String, String, bool)> = Vec::new();
            if let Some(parts) = llm_item.get_mut("parts").and_then(|p| p.as_array_mut()) {
                for (call_idx, p) in parts.iter_mut().enumerate() {
                    let Some(fc) = p.get_mut("functionCall") else {
                        continue;
                    };
                    let name = fc["name"].as_str().unwrap_or_default().to_string();
                    let orig_id = fc["id"]
                        .as_str()
                        .filter(|s| !s.is_empty())
                        .map(str::to_string);
                    let (exp_id, had_orig_id) = match orig_id {
                        Some(id) => (id, true),
                        None => {
                            let gen_id = format!("reloaded_call_{}_{}", i, call_idx);
                            fc["id"] = json!(gen_id);
                            (gen_id, false)
                        }
                    };
                    expected_calls.push((exp_id, name, had_orig_id));
                }
            }

            llm.push(llm_item);
            i += 1;

            if !expected_calls.is_empty() {
                let mut collected_results: Vec<(String, String, Value)> = Vec::new();
                while i < messages.len() && messages[i]["role"].as_str() == Some("toolResult") {
                    let tr = &messages[i];
                    let cid = tr["toolCallId"].as_str().unwrap_or_default().to_string();
                    let tname = reverse_map_tool_name(tr["toolName"].as_str().unwrap_or_default())
                        .to_string();
                    let payload = tool_result_to_response_payload(tr);
                    collected_results.push((cid, tname, payload));
                    i += 1;
                }

                let expected_ids: HashSet<&str> = expected_calls
                    .iter()
                    .map(|(id, _, _)| id.as_str())
                    .collect();
                let exact_reserved: HashSet<usize> = collected_results
                    .iter()
                    .enumerate()
                    .filter(|(_, (cid, _, _))| {
                        !cid.is_empty() && expected_ids.contains(cid.as_str())
                    })
                    .map(|(idx, _)| idx)
                    .collect();

                let mut used_results = HashSet::new();
                let mut assigned: Vec<Option<usize>> = vec![None; expected_calls.len()];

                // Pass 1: exact non-empty toolCallId match
                for (cidx, (exp_id, _, _)) in expected_calls.iter().enumerate() {
                    if let Some((ridx, _)) =
                        collected_results.iter().enumerate().find(|(idx, (cid, _, _))| {
                            !used_results.contains(idx) && !cid.is_empty() && cid == exp_id
                        })
                    {
                        used_results.insert(ridx);
                        assigned[cidx] = Some(ridx);
                    }
                }

                // Pass 2a: calls without original ID -> match unreserved by tool name (or empty name & ID)
                for (cidx, (_, exp_name, had_orig)) in expected_calls.iter().enumerate() {
                    if *had_orig || assigned[cidx].is_some() {
                        continue;
                    }
                    if let Some((ridx, _)) = collected_results.iter().enumerate().find(
                        |(idx, (cid, tname, _))| {
                            !used_results.contains(idx)
                                && !exact_reserved.contains(idx)
                                && (tname == exp_name || (cid.is_empty() && tname.is_empty()))
                        },
                    ) {
                        used_results.insert(ridx);
                        assigned[cidx] = Some(ridx);
                    }
                }

                // Pass 2b: remaining calls with original ID -> match unreserved empty-ID result with same/empty name
                for (cidx, (_, exp_name, _)) in expected_calls.iter().enumerate() {
                    if assigned[cidx].is_some() {
                        continue;
                    }
                    if let Some((ridx, _)) = collected_results.iter().enumerate().find(
                        |(idx, (cid, tname, _))| {
                            !used_results.contains(idx)
                                && !exact_reserved.contains(idx)
                                && cid.is_empty()
                                && (tname == exp_name || tname.is_empty())
                        },
                    ) {
                        used_results.insert(ridx);
                        assigned[cidx] = Some(ridx);
                    }
                }

                // Pass 3: remaining calls without original ID -> first unused & unreserved empty-ID result
                for (cidx, (_, _, had_orig)) in expected_calls.iter().enumerate() {
                    if *had_orig || assigned[cidx].is_some() {
                        continue;
                    }
                    if let Some((ridx, _)) =
                        collected_results.iter().enumerate().find(|(idx, (cid, _, _))| {
                            !used_results.contains(idx)
                                && !exact_reserved.contains(idx)
                                && cid.is_empty()
                        })
                    {
                        used_results.insert(ridx);
                        assigned[cidx] = Some(ridx);
                    }
                }

                let mut response_parts = Vec::with_capacity(expected_calls.len());
                for (cidx, (exp_id, exp_name, _)) in expected_calls.iter().enumerate() {
                    if let Some(ridx) = assigned[cidx] {
                        let (_, tname, resp_val) = &collected_results[ridx];
                        let name = if tname.is_empty() {
                            exp_name.as_str()
                        } else {
                            tname.as_str()
                        };
                        response_parts.push(json!({
                            "functionResponse": {
                                "id": exp_id,
                                "name": name,
                                "response": resp_val,
                            }
                        }));
                    } else {
                        response_parts.push(json!({
                            "functionResponse": {
                                "id": exp_id,
                                "name": exp_name,
                                "response": { "error": "Tool call interrupted before completion" },
                            }
                        }));
                    }
                }

                if !response_parts.is_empty() {
                    llm.push(json!({ "role": "user", "parts": response_parts }));
                }
            }
            continue;
        }

        if let Some(llm_item) = ui_message_to_llm(msg) {
            if prev_was_summary_user {
                llm.push(json!({
                    "role": "model",
                    "parts": [{ "text": "Ringkasan konteks dipahami." }]
                }));
            }
            llm.push(llm_item);
            prev_was_summary_user = matches!(role, "compactionSummary" | "branchSummary");
        }
        i += 1;
    }

    if prev_was_summary_user {
        llm.push(json!({
            "role": "model",
            "parts": [{ "text": "Ringkasan konteks dipahami." }]
        }));
    }

    llm
}

fn estimate_message_tokens(msg: &Value) -> u64 {
    let mut chars = 0usize;
    if let Some(s) = msg["summary"].as_str() {
        chars += s.len();
    }
    if let Some(s) = msg["command"].as_str() {
        chars += s.len();
    }
    if let Some(s) = msg["output"].as_str() {
        chars += s.len();
    }
    if let Some(s) = msg["content"].as_str() {
        chars += s.len();
    } else if let Some(arr) = msg["content"].as_array() {
        for block in arr {
            if let Some(t) = block["text"].as_str() {
                chars += t.len();
            } else if let Some(th) = block["thinking"].as_str() {
                chars += th.len();
            } else if block["type"].as_str() == Some("toolCall") {
                if let Some(args) = block.get("arguments").or_else(|| block.get("args")) {
                    chars += args.to_string().len();
                }
            } else if block["type"].as_str() == Some("image") {
                chars += 4000;
            }
        }
    }
    ((chars as u64) / 4).max(1)
}

fn estimate_messages_tokens(messages: &[Value]) -> u64 {
    if messages.is_empty() {
        return 0;
    }
    messages.iter().map(estimate_message_tokens).sum::<u64>().max(1)
}

fn estimate_entry_tokens(entry: &Value) -> u64 {
    match entry["type"].as_str().unwrap_or_default() {
        "message" => estimate_message_tokens(&entry["message"]),
        "custom_message" | "bashExecution" => estimate_message_tokens(entry),
        _ => 16,
    }
}

fn is_valid_compaction_cut_entry(entry: &Value) -> bool {
    match entry["type"].as_str().unwrap_or_default() {
        "message" => {
            let role = entry["message"]["role"].as_str().unwrap_or_default();
            role != "toolResult" && role != "system"
        }
        "custom_message" | "bashExecution" => true,
        _ => false,
    }
}

fn select_compaction_cut_point(active_entries: &[Value]) -> Option<CompactionCut> {
    if active_entries.is_empty() {
        return None;
    }

    const KEEP_RECENT_TOKENS: u64 = 20_000;
    let total_tokens: u64 = active_entries.iter().map(estimate_entry_tokens).sum();

    if total_tokens >= KEEP_RECENT_TOKENS {
        let mut kept_acc = 0u64;
        let mut raw_idx = active_entries.len();
        for idx in (0..active_entries.len()).rev() {
            kept_acc += estimate_entry_tokens(&active_entries[idx]);
            raw_idx = idx;
            if kept_acc >= KEEP_RECENT_TOKENS {
                break;
            }
        }
        let mut chosen: Option<usize> = None;
        for idx in (1..=raw_idx).rev() {
            if is_valid_compaction_cut_entry(&active_entries[idx]) {
                chosen = Some(idx);
                break;
            }
        }
        if chosen.is_none() {
            for (idx, entry) in active_entries.iter().enumerate().skip(raw_idx.max(1)) {
                if is_valid_compaction_cut_entry(entry) {
                    chosen = Some(idx);
                    break;
                }
            }
        }
        if let Some(cut_idx) = chosen {
            let kept_id = active_entries[cut_idx]["id"].as_str().map(str::to_string);
            return Some(CompactionCut {
                to_summarize: active_entries[..cut_idx].to_vec(),
                first_kept_entry_id: kept_id,
            });
        }
    }

    let latest_user_cut = (1..active_entries.len()).rev().find(|&idx| {
        let e = &active_entries[idx];
        e["type"].as_str() == Some("message")
            && e["message"]["role"].as_str() == Some("user")
    });
    if let Some(cut_idx) = latest_user_cut {
        let kept_id = active_entries[cut_idx]["id"].as_str().map(str::to_string);
        return Some(CompactionCut {
            to_summarize: active_entries[..cut_idx].to_vec(),
            first_kept_entry_id: kept_id,
        });
    }

    // Retain-none fallback ketika sesi hanya memiliki satu giliran awal
    Some(CompactionCut {
        to_summarize: active_entries.to_vec(),
        first_kept_entry_id: None,
    })
}

fn format_entry_for_compaction(entry: &Value) -> Option<String> {
    match entry["type"].as_str().unwrap_or_default() {
        "message" => {
            let msg = &entry["message"];
            let role = msg["role"].as_str().unwrap_or_default();
            match role {
                "user" => Some(format!("[User]: {}", extract_user_message_text(msg))),
                "assistant" => {
                    let mut parts = Vec::new();
                    if let Some(arr) = msg["content"].as_array() {
                        for b in arr {
                            if let Some(t) = b["text"].as_str().filter(|s| !s.trim().is_empty()) {
                                parts.push(t.to_string());
                            } else if b["type"].as_str() == Some("toolCall") {
                                let name = b["name"].as_str().unwrap_or("");
                                let args = b
                                    .get("arguments")
                                    .or_else(|| b.get("args"))
                                    .map(|v| v.to_string())
                                    .unwrap_or_default();
                                parts.push(format!("[ToolCall {}({})]", name, args));
                            }
                        }
                    }
                    Some(format!("[Assistant]: {}", parts.join("\n")))
                }
                "toolResult" => {
                    let name = msg["toolName"].as_str().unwrap_or("tool");
                    Some(format!(
                        "[ToolResult {}]: {}",
                        name,
                        extract_user_message_text(msg)
                    ))
                }
                _ => None,
            }
        }
        "custom_message" => Some(format!(
            "[Custom]: {}",
            extract_user_message_text(entry)
        )),
        "bashExecution" => Some(format!(
            "[Bash $ {}]: {}",
            entry["command"].as_str().unwrap_or_default(),
            entry["output"].as_str().unwrap_or_default()
        )),
        _ => None,
    }
}

fn build_compaction_prompt(
    to_summarize: &[Value],
    previous_summary: Option<&str>,
    custom_instructions: Option<&str>,
) -> String {
    let mut prompt = String::from(
        "Summarize the conversation clearly and preserve key goals, decisions, file paths, and open tasks (Ringkas percakapan sebelumnya secara akurat dan padat).\n",
    );
    if let Some(prev) = previous_summary.filter(|s| !s.trim().is_empty()) {
        prompt.push_str("\n<previous_summary>\n");
        prompt.push_str(prev.trim());
        prompt.push_str("\n</previous_summary>\n");
    }
    prompt.push_str("\n<conversation>\n");
    for entry in to_summarize {
        if let Some(line) = format_entry_for_compaction(entry) {
            prompt.push_str(&line);
            prompt.push('\n');
        }
    }
    prompt.push_str("</conversation>\n");
    if let Some(inst) = custom_instructions.filter(|s| !s.trim().is_empty()) {
        prompt.push_str("\n<custom_instructions>\n");
        prompt.push_str(inst.trim());
        prompt.push_str("\n</custom_instructions>\n");
    }
    prompt
}

fn html_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for ch in s.chars() {
        match ch {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#39;"),
            _ => out.push(ch),
        }
    }
    out
}

fn render_session_html(
    session_id: &str,
    session_name: Option<&str>,
    cwd: &Path,
    ui_messages: &[Value],
) -> String {
    let title = session_name
        .filter(|s| !s.trim().is_empty())
        .unwrap_or(session_id);
    let mut body = String::new();

    for msg in ui_messages {
        let role = msg["role"].as_str().unwrap_or("unknown");
        let (role_label, class_name, text_html) = match role {
            "user" => (
                "User",
                "user",
                format!(
                    "<pre>{}</pre>",
                    html_escape(&extract_user_message_text(msg))
                ),
            ),
            "assistant" => {
                let mut inner = String::new();
                if let Some(arr) = msg["content"].as_array() {
                    for b in arr {
                        match b["type"].as_str().unwrap_or_default() {
                            "thinking" => {
                                if let Some(th) = b["thinking"].as_str() {
                                    inner.push_str(&format!(
                                        "<details><summary>Thinking</summary><pre>{}</pre></details>",
                                        html_escape(th)
                                    ));
                                }
                            }
                            "text" => {
                                if let Some(t) = b["text"].as_str() {
                                    inner.push_str(&format!("<pre>{}</pre>", html_escape(t)));
                                }
                            }
                            "toolCall" => {
                                let name = b["name"].as_str().unwrap_or("tool");
                                let args = b
                                    .get("arguments")
                                    .or_else(|| b.get("args"))
                                    .map(|v| serde_json::to_string_pretty(v).unwrap_or_default())
                                    .unwrap_or_default();
                                inner.push_str(&format!(
                                    "<div class=\"tool\"><strong>Tool: {}</strong><pre>{}</pre></div>",
                                    html_escape(name),
                                    html_escape(&args)
                                ));
                            }
                            _ => {}
                        }
                    }
                }
                if inner.is_empty() {
                    if let Some(err) = msg["errorMessage"].as_str() {
                        inner = format!("<pre class=\"err\">{}</pre>", html_escape(err));
                    }
                }
                ("Assistant", "assistant", inner)
            }
            "toolResult" => {
                let name = msg["toolName"].as_str().unwrap_or("tool");
                (
                    "Tool Result",
                    "tool-result",
                    format!(
                        "<div class=\"tool\"><strong>{}</strong><pre>{}</pre></div>",
                        html_escape(name),
                        html_escape(&extract_user_message_text(msg))
                    ),
                )
            }
            "compactionSummary" => (
                "Compaction Summary",
                "summary",
                format!(
                    "<pre>{}</pre>",
                    html_escape(msg["summary"].as_str().unwrap_or_default())
                ),
            ),
            "branchSummary" => (
                "Branch Summary",
                "summary",
                format!(
                    "<pre>{}</pre>",
                    html_escape(msg["summary"].as_str().unwrap_or_default())
                ),
            ),
            "bashExecution" => (
                "Bash",
                "bash",
                format!(
                    "<pre>$ {}\n{}</pre>",
                    html_escape(msg["command"].as_str().unwrap_or_default()),
                    html_escape(msg["output"].as_str().unwrap_or_default())
                ),
            ),
            _ => (
                role,
                "other",
                format!(
                    "<pre>{}</pre>",
                    html_escape(&extract_user_message_text(msg))
                ),
            ),
        };
        body.push_str(&format!(
            "<section class=\"msg {}\"><h3>{}</h3>{}</section>\n",
            class_name,
            html_escape(role_label),
            text_html
        ));
    }

    format!(
        "<!doctype html>
<html lang=\"en\">
<head>
<meta charset=\"utf-8\">
<meta name=\"viewport\" content=\"width=device-width,initial-scale=1\">
<title>{title}</title>
<style>
body {{ font-family: system-ui, sans-serif; max-width: 860px; margin: 2rem auto; padding: 0 1rem; background: #0f1115; color: #e6edf3; }}
header {{ border-bottom: 1px solid #30363d; margin-bottom: 1.5rem; padding-bottom: 1rem; }}
.msg {{ border: 1px solid #30363d; border-radius: 8px; padding: 0.75rem 1rem; margin-bottom: 1rem; background: #161b22; }}
.msg.user {{ border-left: 4px solid #58a6ff; }}
.msg.assistant {{ border-left: 4px solid #3fb950; }}
.msg.summary {{ border-left: 4px solid #d29922; }}
pre {{ white-space: pre-wrap; word-break: break-word; font-family: ui-monospace, monospace; margin: 0.25rem 0; }}
.tool {{ background: #0d1117; padding: 0.5rem; border-radius: 6px; margin: 0.5rem 0; }}
</style>
</head>
<body>
<header>
<h1>{title}</h1>
<p>Session ID: <code>{sid}</code> · CWD: <code>{cwd}</code></p>
</header>
{body}
</body>
</html>",
        title = html_escape(title),
        sid = html_escape(session_id),
        cwd = html_escape(&cwd.to_string_lossy()),
        body = body,
    )
}

fn days_from_civil(y: i64, m: u32, d: u32) -> i64 {
    let y = y - if m <= 2 { 1 } else { 0 };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = (y - era * 400) as u32;
    let m_adj = if m > 2 { m - 3 } else { m + 9 };
    let doy = (153 * m_adj + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe as i64 - 719_468
}

fn parse_iso_to_epoch_ms(iso: Option<&str>) -> u64 {
    let Some(raw) = iso.map(str::trim) else {
        return 0;
    };
    let Some((date_part, time_part)) = raw.split_once('T') else {
        return 0;
    };
    let date_nums: Vec<i64> = date_part.split('-').filter_map(|p| p.parse().ok()).collect();
    if date_nums.len() != 3 {
        return 0;
    }
    let clean_time = time_part.trim_end_matches('Z');
    let (hms, ms_str) = clean_time.split_once('.').unwrap_or((clean_time, "0"));
    let time_nums: Vec<u64> = hms.split(':').filter_map(|p| p.parse().ok()).collect();
    if time_nums.len() != 3 {
        return 0;
    }
    let millis: u64 = ms_str
        .chars()
        .take(3)
        .collect::<String>()
        .parse()
        .unwrap_or(0);
    let days = days_from_civil(date_nums[0], date_nums[1] as u32, date_nums[2] as u32);
    if days < 0 {
        return 0;
    }
    let secs = (days as u64) * 86_400 + time_nums[0] * 3600 + time_nums[1] * 60 + time_nums[2];
    secs * 1000 + millis
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

    struct TempConfigGuard(PathBuf);
    impl TempConfigGuard {
        fn new(prefix: &str) -> Self {
            static SEQ: AtomicU64 = AtomicU64::new(1);
            let dir = std::env::temp_dir().join(format!(
                "{}_{}_{}_{}",
                prefix,
                std::process::id(),
                epoch_ms(),
                SEQ.fetch_add(1, Ordering::Relaxed)
            ));
            let _ = std::fs::create_dir_all(&dir);
            Self(dir)
        }
        fn path(&self) -> &Path {
            &self.0
        }
    }
    impl Drop for TempConfigGuard {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn rpc_server_activates_plugins_from_extensions_arg() {
        let tmp = TempConfigGuard::new("oopbot_plugins_test");
        let extensions = vec![
            "/data/user/0/io.jeph.pipkin/files/extensions/pipkin-todo/index.js".to_string(),
            "pipkin-web-access".to_string(),
        ];

        let server = RpcServer::new_in_dir(tmp.path().to_path_buf(), None, None, extensions).unwrap();
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
        let tmp = TempConfigGuard::new("oopbot_session_stats");
        let session_file = tmp.path().join("test_session.jsonl");

        let lines = [
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

        let server = RpcServer::new_in_dir(
            tmp.path().to_path_buf(),
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
    }

    #[tokio::test]
    async fn test_pipkin_channel_handshake_and_incoming_events() {
        use std::io::Cursor;
        let _sink_guard = output_sink_test_mutex().lock().await;
        let tmp = TempConfigGuard::new("oopbot_channel_test");

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
        let mut server = RpcServer::new_in_dir(tmp.path().to_path_buf(), None, None, extensions).unwrap();

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
        let tmp = TempConfigGuard::new("oopbot_resolve_appr");
        let server = RpcServer::new_in_dir_with_cwd(
            tmp.path().to_path_buf(),
            tmp.path().to_path_buf(),
            None,
            None,
            vec![],
        )
        .unwrap();
        let (tx, rx) = tokio::sync::oneshot::channel::<bool>();

        server.pending_approvals().lock().unwrap().insert("appr-99".to_string(), tx);

        // Sukses me-resolve approval yang ada
        assert!(server.resolve_tool_approval("appr-99", true));
        assert!(rx.await.unwrap());

        // Memanggil lagi untuk ID yang sama atau id fiktif mengembalikan false
        assert!(!server.resolve_tool_approval("appr-99", true));
        assert!(!server.resolve_tool_approval("unknown-id", false));
    }

    #[tokio::test]
    async fn test_rpc_loop_tool_approval_pipkin_channel_roundtrip() {
        let tmp = TempConfigGuard::new("oopbot_chan_appr");
        let mut server = RpcServer::new_in_dir_with_cwd(
            tmp.path().to_path_buf(),
            tmp.path().to_path_buf(),
            None,
            None,
            vec![],
        )
        .unwrap();
        let (tx, rx) = tokio::sync::oneshot::channel::<bool>();
        server.pending_approvals().lock().unwrap().insert("appr-42".to_string(), tx);

        let input = b"\
{\"type\":\"pipkin_channel\",\"topic\":\"tool_approval\",\"data\":{\"kind\":\"response\",\"request_id\":\"appr-42\",\"approved\":true}}\n\
{\"type\":\"get_state\",\"id\":\"chk_approval\"}\n";
        let cursor = Cursor::new(input);

        let res = server.run_loop_with_input(cursor).await;
        assert!(res.is_ok());

        // Verifikasi bahwa waiter oneshot menerima approval = true dari stream JSONL
        assert!(rx.await.unwrap());
    }

    #[tokio::test]
    async fn test_rpc_loop_extension_ui_response_roundtrip() {
        let _sink_guard = output_sink_test_mutex().lock().await;
        let tmp = TempConfigGuard::new("oopbot_ext_ui_resp");

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

        let mut server = RpcServer::new_in_dir_with_cwd(
            tmp.path().to_path_buf(),
            tmp.path().to_path_buf(),
            None,
            None,
            vec![],
        )
        .unwrap();
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

        assert!(rx1.await.unwrap());
        assert!(!rx2.await.unwrap());
        assert!(!rx3.await.unwrap());
        assert!(rx_eof.await.is_err());

        let bytes = raw.lock().unwrap().clone();
        let written = String::from_utf8_lossy(&bytes);
        assert!(written.contains("chk_ext"));
        assert!(!written.contains("tidak dikenal"));
        assert!(!written.contains("extension_ui_response"));
    }

    // -------------------------------------------------------------------------
    // Harness & Regression Tests for REG-002 & REG-003 (Regression-Aware.md)
    // -------------------------------------------------------------------------

    struct OutputSinkResetGuard;
    impl Drop for OutputSinkResetGuard {
        fn drop(&mut self) {
            set_output_sink(None);
        }
    }

    struct ChannelWriter {
        tx: tokio::sync::mpsc::UnboundedSender<Value>,
        partial: Vec<u8>,
    }

    impl io::Write for ChannelWriter {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            self.partial.extend_from_slice(buf);
            while let Some(pos) = self.partial.iter().position(|&b| b == b'\n') {
                let line_bytes: Vec<u8> = self.partial.drain(..=pos).collect();
                let line = String::from_utf8_lossy(&line_bytes);
                let trimmed = line.trim();
                if !trimmed.is_empty() {
                    if let Ok(val) = serde_json::from_str::<Value>(trimmed) {
                        let _ = self.tx.send(val);
                    }
                }
            }
            Ok(buf.len())
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    fn install_channel_output_sink() -> (
        OutputSinkResetGuard,
        tokio::sync::mpsc::UnboundedReceiver<Value>,
    ) {
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        let writer = ChannelWriter {
            tx,
            partial: Vec::new(),
        };
        let sink: OutputSink = Arc::new(Mutex::new(Box::new(writer)));
        set_output_sink(Some(sink));
        (OutputSinkResetGuard, rx)
    }

    struct TestLineReader {
        rx: std::sync::mpsc::Receiver<Vec<u8>>,
        cur: Vec<u8>,
        pos: usize,
    }

    impl TestLineReader {
        fn new() -> (std::sync::mpsc::Sender<Vec<u8>>, Self) {
            let (tx, rx) = std::sync::mpsc::channel();
            (
                tx,
                Self {
                    rx,
                    cur: Vec::new(),
                    pos: 0,
                },
            )
        }
    }

    impl io::Read for TestLineReader {
        fn read(&mut self, out: &mut [u8]) -> io::Result<usize> {
            while self.pos >= self.cur.len() {
                match self.rx.recv() {
                    Ok(next) => {
                        self.cur = next;
                        self.pos = 0;
                    }
                    Err(_) => return Ok(0),
                }
            }
            let avail = self.cur.len() - self.pos;
            let n = avail.min(out.len());
            out[..n].copy_from_slice(&self.cur[self.pos..self.pos + n]);
            self.pos += n;
            Ok(n)
        }
    }

    fn send_cmd(tx: &std::sync::mpsc::Sender<Vec<u8>>, cmd: Value) {
        let mut s = serde_json::to_string(&cmd).unwrap();
        s.push('\n');
        tx.send(s.into_bytes()).unwrap();
    }

    async fn recv_matching<F>(
        rx: &mut tokio::sync::mpsc::UnboundedReceiver<Value>,
        timeout: std::time::Duration,
        pred: F,
    ) -> Value
    where
        F: Fn(&Value) -> bool,
    {
        tokio::time::timeout(timeout, async {
            while let Some(v) = rx.recv().await {
                if pred(&v) {
                    return v;
                }
            }
            panic!("RPC output channel closed while waiting for matching event");
        })
        .await
        .expect("Timed out waiting for matching RPC event/response")
    }

    async fn recv_response(
        rx: &mut tokio::sync::mpsc::UnboundedReceiver<Value>,
        id: &str,
        timeout: std::time::Duration,
    ) -> Value {
        recv_matching(rx, timeout, |v| {
            v["type"] == "response" && v["id"].as_str() == Some(id)
        })
        .await
    }

    #[derive(Clone, Debug)]
    struct MockHttpReq {
        path: String,
        body: Value,
    }

    async fn spawn_mock_llm_server<F, Fut>(
        handler: F,
    ) -> (
        String,
        Arc<Mutex<Vec<MockHttpReq>>>,
        tokio::task::JoinHandle<()>,
    )
    where
        F: Fn(usize, MockHttpReq) -> Fut + Send + Sync + 'static,
        Fut: std::future::Future<Output = (u16, Value)> + Send + 'static,
    {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let base_url = format!("http://127.0.0.1:{}", addr.port());
        let recorded = Arc::new(Mutex::new(Vec::new()));
        let recorded_bg = Arc::clone(&recorded);
        let handler = Arc::new(handler);
        let seq = Arc::new(AtomicU64::new(0));

        let handle = tokio::spawn(async move {
            loop {
                let Ok((mut socket, _)) = listener.accept().await else {
                    break;
                };
                let recorded_req = Arc::clone(&recorded_bg);
                let handler_req = Arc::clone(&handler);
                let seq_req = Arc::clone(&seq);

                tokio::spawn(async move {
                    let mut buf = Vec::new();
                    let mut tmp = [0u8; 4096];
                    let header_end;
                    loop {
                        let Ok(n) = socket.read(&mut tmp).await else {
                            return;
                        };
                        if n == 0 {
                            return;
                        }
                        buf.extend_from_slice(&tmp[..n]);
                        if let Some(pos) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
                            header_end = pos + 4;
                            break;
                        }
                    }

                    let head = String::from_utf8_lossy(&buf[..header_end]);
                    let mut lines = head.lines();
                    let req_line = lines.next().unwrap_or("");
                    let path = req_line.split_whitespace().nth(1).unwrap_or("/").to_string();

                    let mut content_len = 0usize;
                    for line in lines {
                        if let Some((k, v)) = line.split_once(':') {
                            if k.eq_ignore_ascii_case("content-length") {
                                content_len = v.trim().parse().unwrap_or(0);
                            }
                        }
                    }

                    while buf.len() < header_end + content_len {
                        let Ok(n) = socket.read(&mut tmp).await else {
                            return;
                        };
                        if n == 0 {
                            break;
                        }
                        buf.extend_from_slice(&tmp[..n]);
                    }

                    let body_bytes = &buf[header_end..(header_end + content_len).min(buf.len())];
                    let body: Value = serde_json::from_slice(body_bytes).unwrap_or(Value::Null);
                    let req = MockHttpReq {
                        path: path.clone(),
                        body,
                    };
                    if !path.ends_with("/models") && !path.contains("/models?") {
                        recorded_req.lock().unwrap().push(req.clone());
                    }

                    let call_idx = if path.ends_with("/models") || path.contains("/models?") {
                        usize::MAX
                    } else {
                        seq_req.fetch_add(1, Ordering::SeqCst) as usize
                    };

                    let (status, resp_json) = handler_req(call_idx, req).await;
                    let resp_body = serde_json::to_string(&resp_json).unwrap();
                    let reason = if status == 200 { "OK" } else { "Error" };
                    let http = format!(
                        "HTTP/1.1 {} {}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                        status,
                        reason,
                        resp_body.len(),
                        resp_body
                    );
                    let _ = socket.write_all(http.as_bytes()).await;
                    let _ = socket.shutdown().await;
                });
            }
        });

        (base_url, recorded, handle)
    }

    fn write_mock_provider_config(
        dir: &Path,
        provider: &str,
        api: &str,
        model_id: &str,
        base_url: &str,
        api_key: &str,
    ) {
        std::fs::write(
            dir.join("auth.json"),
            serde_json::to_string_pretty(&json!({
                provider: { "type": "api_key", "apiKey": api_key }
            }))
            .unwrap(),
        )
        .unwrap();
        std::fs::write(
            dir.join("models.json"),
            serde_json::to_string_pretty(&json!({
                "providers": {
                    provider: {
                        "baseUrl": base_url,
                        "api": api,
                        "scannedApiKey": api_key,
                        "models": [{
                            "id": model_id,
                            "name": model_id,
                            "reasoning": false,
                            "contextWindow": 128000,
                            "maxTokens": 8192
                        }]
                    }
                }
            }))
            .unwrap(),
        )
        .unwrap();
        std::fs::write(
            dir.join("settings.json"),
            serde_json::to_string_pretty(&json!({
                "defaultProvider": provider,
                "defaultModel": model_id,
                "defaultThinkingLevel": "off"
            }))
            .unwrap(),
        )
        .unwrap();
    }

    fn gemini_text_resp(text: &str, inp: u64, out: u64) -> Value {
        json!({
            "candidates": [{
                "content": {
                    "role": "model",
                    "parts": [{ "text": text }]
                }
            }],
            "usageMetadata": {
                "promptTokenCount": inp,
                "candidatesTokenCount": out,
                "thoughtsTokenCount": 0
            }
        })
    }

    #[tokio::test]
    async fn reg002_get_state_nonblocking_while_streaming() {
        let _sink_guard = output_sink_test_mutex().lock().await;
        let (_reset, mut out_rx) = install_channel_output_sink();
        let tmp = TempConfigGuard::new("reg002_get_state");

        let (inflight_tx, inflight_rx) = tokio::sync::oneshot::channel::<()>();
        let inflight_tx = Arc::new(Mutex::new(Some(inflight_tx)));
        let (base_url, _reqs, mock_handle) = spawn_mock_llm_server(move |_idx, _req| {
            let inflight_tx = Arc::clone(&inflight_tx);
            async move {
                if let Some(tx) = inflight_tx.lock().unwrap().take() {
                    let _ = tx.send(());
                }
                tokio::time::sleep(std::time::Duration::from_millis(1200)).await;
                (200, gemini_text_resp("Selesai setelah delay", 100, 20))
            }
        })
        .await;

        write_mock_provider_config(
            tmp.path(),
            "google",
            "google-generative-ai",
            "gemini-2.5-flash",
            &format!("{}/v1beta", base_url),
            "test-google-key",
        );

        let mut server = RpcServer::new_in_dir_with_cwd(
            tmp.path().to_path_buf(),
            tmp.path().to_path_buf(),
            None,
            None,
            vec![],
        )
        .unwrap();

        let (cmd_tx, reader) = TestLineReader::new();
        let loop_task = tokio::spawn(async move {
            server.run_loop_with_input(io::BufReader::new(reader)).await.unwrap();
        });

        // Drain initial hello
        let _ = recv_matching(&mut out_rx, std::time::Duration::from_secs(2), |v| {
            v["type"] == "pipkin_channel" && v["topic"] == "pipkin:hello"
        })
        .await;

        send_cmd(
            &cmd_tx,
            json!({ "id": "p1", "type": "prompt", "message": "jalan lambat" }),
        );
        let p1_resp = recv_response(&mut out_rx, "p1", std::time::Duration::from_millis(400)).await;
        assert_eq!(p1_resp["success"], true);
        assert_eq!(p1_resp["data"]["disposition"], "started");

        // Pastikan request HTTP sudah aktif
        tokio::time::timeout(std::time::Duration::from_secs(2), inflight_rx)
            .await
            .expect("HTTP request never hit mock server")
            .unwrap();

        let started = std::time::Instant::now();
        send_cmd(&cmd_tx, json!({ "id": "gs1", "type": "get_state" }));
        let gs1 = recv_response(&mut out_rx, "gs1", std::time::Duration::from_millis(400)).await;
        assert!(started.elapsed() < std::time::Duration::from_millis(400));
        assert_eq!(gs1["data"]["isStreaming"], true);
        assert_eq!(gs1["data"]["isCompacting"], false);

        // Tunggu turn selesai
        let _settled = recv_matching(&mut out_rx, std::time::Duration::from_secs(3), |v| {
            v["type"] == "agent_settled"
        })
        .await;

        send_cmd(&cmd_tx, json!({ "id": "gs2", "type": "get_state" }));
        let gs2 = recv_response(&mut out_rx, "gs2", std::time::Duration::from_millis(400)).await;
        assert_eq!(gs2["data"]["isStreaming"], false);

        drop(cmd_tx);
        let _ = tokio::time::timeout(std::time::Duration::from_secs(2), loop_task).await;
        mock_handle.abort();
    }

    #[tokio::test]
    async fn reg002_abort_cancels_inflight_http_and_pending_approver() {
        let _sink_guard = output_sink_test_mutex().lock().await;
        let (_reset, mut out_rx) = install_channel_output_sink();
        let tmp = TempConfigGuard::new("reg002_abort");

        let (http_hit_tx, http_hit_rx) = tokio::sync::oneshot::channel::<()>();
        let http_hit_tx = Arc::new(Mutex::new(Some(http_hit_tx)));
        let (base_url, _reqs, mock_handle) = spawn_mock_llm_server(move |idx, _req| {
            let http_hit_tx = Arc::clone(&http_hit_tx);
            async move {
                match idx {
                    // Call 0: HTTP hang -> akan di-abort
                    0 => {
                        if let Some(tx) = http_hit_tx.lock().unwrap().take() {
                            let _ = tx.send(());
                        }
                        tokio::time::sleep(std::time::Duration::from_secs(10)).await;
                        (200, gemini_text_resp("tidak seharusnya sampai sini", 10, 5))
                    }
                    // Call 1: return dangerous run_shell tool call -> memicu RpcApprover
                    1 => (
                        200,
                        json!({
                            "candidates": [{
                                "content": {
                                    "role": "model",
                                    "parts": [{
                                        "functionCall": {
                                            "id": "call_danger_1",
                                            "name": "run_shell",
                                            "args": { "command": "rm -rf /tmp/fake_abort_target" }
                                        }
                                    }]
                                }
                            }]
                        }),
                    ),
                    // Call 2+: recovery prompt sesudah abort
                    _ => (200, gemini_text_resp("Pemulihan berhasil", 50, 10)),
                }
            }
        })
        .await;

        write_mock_provider_config(
            tmp.path(),
            "google",
            "google-generative-ai",
            "gemini-2.5-flash",
            &format!("{}/v1beta", base_url),
            "test-google-key",
        );

        let mut server = RpcServer::new_in_dir_with_cwd(
            tmp.path().to_path_buf(),
            tmp.path().to_path_buf(),
            None,
            None,
            vec![],
        )
        .unwrap();
        let pending_approvals = server.pending_approvals();

        let (cmd_tx, reader) = TestLineReader::new();
        let loop_task = tokio::spawn(async move {
            server.run_loop_with_input(io::BufReader::new(reader)).await.unwrap();
        });

        let _ = recv_matching(&mut out_rx, std::time::Duration::from_secs(2), |v| {
            v["topic"] == "pipkin:hello"
        })
        .await;

        // Skenario A: Abort saat HTTP in-flight
        send_cmd(&cmd_tx, json!({ "id": "p_http", "type": "prompt", "message": "hang" }));
        let _ = recv_response(&mut out_rx, "p_http", std::time::Duration::from_millis(500)).await;
        tokio::time::timeout(std::time::Duration::from_secs(2), http_hit_rx)
            .await
            .unwrap()
            .unwrap();

        let t0 = std::time::Instant::now();
        send_cmd(&cmd_tx, json!({ "id": "ab1", "type": "abort" }));
        let settled_a = recv_matching(&mut out_rx, std::time::Duration::from_millis(600), |v| {
            v["type"] == "agent_settled"
        })
        .await;
        let ab1 = recv_response(&mut out_rx, "ab1", std::time::Duration::from_millis(600)).await;
        assert!(t0.elapsed() < std::time::Duration::from_millis(600));
        assert_eq!(ab1["success"], true);
        assert_eq!(settled_a["aborted"], true);

        // Skenario B: Abort saat RpcApprover sedang menunggu konfirmasi user
        send_cmd(&cmd_tx, json!({ "id": "p_appr", "type": "prompt", "message": "hapus tmp" }));
        let _ = recv_response(&mut out_rx, "p_appr", std::time::Duration::from_millis(500)).await;
        let confirm_ev = recv_matching(&mut out_rx, std::time::Duration::from_secs(2), |v| {
            v["type"] == "extension_ui_request" && v["method"] == "confirm"
        })
        .await;
        assert_eq!(confirm_ev["title"], "Allow run_shell?");
        assert!(!pending_approvals.lock().unwrap().is_empty());

        send_cmd(&cmd_tx, json!({ "id": "ab2", "type": "abort" }));
        let settled_b = recv_matching(&mut out_rx, std::time::Duration::from_millis(600), |v| {
            v["type"] == "agent_settled"
        })
        .await;
        let ab2 = recv_response(&mut out_rx, "ab2", std::time::Duration::from_millis(600)).await;
        assert_eq!(ab2["success"], true);
        assert_eq!(settled_b["aborted"], true);
        assert!(pending_approvals.lock().unwrap().is_empty());

        // Skenario C: Prompt berikutnya sesudah abort tetap berjalan bersih & histori tool seimbang
        send_cmd(&cmd_tx, json!({ "id": "p_ok", "type": "prompt", "message": "lanjut aman" }));
        let p_ok = recv_response(&mut out_rx, "p_ok", std::time::Duration::from_millis(500)).await;
        assert_eq!(p_ok["data"]["disposition"], "started");
        let reply = recv_matching(&mut out_rx, std::time::Duration::from_secs(2), |v| {
            v["type"] == "message_end"
                && v["message"]["role"] == "assistant"
                && v["message"]["stopReason"] == "stop"
        })
        .await;
        assert!(reply["message"]["content"][0]["text"]
            .as_str()
            .unwrap_or("")
            .contains("Pemulihan berhasil"));
        let _ = recv_matching(&mut out_rx, std::time::Duration::from_secs(2), |v| {
            v["type"] == "agent_settled" && v["aborted"] == false
        })
        .await;

        drop(cmd_tx);
        let _ = tokio::time::timeout(std::time::Duration::from_secs(2), loop_task).await;
        mock_handle.abort();
    }

    #[tokio::test]
    async fn reg002_steer_queue_and_clear_queue_behavior() {
        let _sink_guard = output_sink_test_mutex().lock().await;
        let (_reset, mut out_rx) = install_channel_output_sink();
        let tmp = TempConfigGuard::new("reg002_steer");

        let gate = Arc::new(tokio::sync::Notify::new());
        let gate_srv = Arc::clone(&gate);
        let (call0_hit_tx, call0_hit_rx) = tokio::sync::oneshot::channel::<()>();
        let call0_hit_tx = Arc::new(Mutex::new(Some(call0_hit_tx)));

        let (base_url, reqs, mock_handle) = spawn_mock_llm_server(move |idx, req| {
            let gate_srv = Arc::clone(&gate_srv);
            let call0_hit_tx = Arc::clone(&call0_hit_tx);
            async move {
                match idx {
                    0 => {
                        if let Some(tx) = call0_hit_tx.lock().unwrap().take() {
                            let _ = tx.send(());
                        }
                        gate_srv.notified().await;
                        (
                            200,
                            json!({
                                "candidates": [{
                                    "content": {
                                        "role": "model",
                                        "parts": [{
                                            "functionCall": {
                                                "id": "call_ls_1",
                                                "name": "list_dir",
                                                "args": { "path": "." }
                                            }
                                        }]
                                    }
                                }]
                            }),
                        )
                    }
                    1 => {
                        let s = req.body.to_string();
                        (
                            200,
                            gemini_text_resp(
                                if s.contains("arah baru") {
                                    "Mengikuti arah baru"
                                } else {
                                    "Tanpa arah"
                                },
                                60,
                                15,
                            ),
                        )
                    }
                    2 => (200, gemini_text_resp("Turn utama idle", 40, 10)),
                    _ => (200, gemini_text_resp("Turn steer antrean idle", 40, 10)),
                }
            }
        })
        .await;

        write_mock_provider_config(
            tmp.path(),
            "google",
            "google-generative-ai",
            "gemini-2.5-flash",
            &format!("{}/v1beta", base_url),
            "test-google-key",
        );

        let mut server = RpcServer::new_in_dir_with_cwd(
            tmp.path().to_path_buf(),
            tmp.path().to_path_buf(),
            None,
            None,
            vec![],
        )
        .unwrap();

        let (cmd_tx, reader) = TestLineReader::new();
        let loop_task = tokio::spawn(async move {
            server.run_loop_with_input(io::BufReader::new(reader)).await.unwrap();
        });

        let _ = recv_matching(&mut out_rx, std::time::Duration::from_secs(2), |v| {
            v["topic"] == "pipkin:hello"
        })
        .await;

        // 1. Mulai turn streaming
        send_cmd(&cmd_tx, json!({ "id": "p1", "type": "prompt", "message": "mulai" }));
        let _ = recv_response(&mut out_rx, "p1", std::time::Duration::from_millis(500)).await;
        tokio::time::timeout(std::time::Duration::from_secs(2), call0_hit_rx)
            .await
            .unwrap()
            .unwrap();

        // 2. Prompt tanpa streamingBehavior saat streaming harus ditolak
        send_cmd(&cmd_tx, json!({ "id": "p_bad", "type": "prompt", "message": "tanpa behavior" }));
        let p_bad = recv_response(&mut out_rx, "p_bad", std::time::Duration::from_millis(400)).await;
        assert_eq!(p_bad["success"], false);

        // 3. Prompt dg streamingBehavior=steer + steer command harus langsung queued & emit queue_update
        send_cmd(&cmd_tx, json!({ "id": "p_q", "type": "prompt", "message": "antre 1", "streamingBehavior": "steer" }));
        let p_q = recv_response(&mut out_rx, "p_q", std::time::Duration::from_millis(400)).await;
        assert_eq!(p_q["success"], true);
        assert_eq!(p_q["data"]["disposition"], "queued");

        send_cmd(&cmd_tx, json!({ "id": "st1", "type": "steer", "message": "antre 2" }));
        let qu = recv_matching(&mut out_rx, std::time::Duration::from_millis(400), |v| {
            v["type"] == "queue_update" && v["steering"].as_array().map(|a| a.len()) == Some(2)
        })
        .await;
        assert_eq!(qu["steering"], json!(["antre 1", "antre 2"]));
        let st1 = recv_response(&mut out_rx, "st1", std::time::Duration::from_millis(400)).await;
        assert_eq!(st1["data"]["disposition"], "queued");

        // 4. clear_queue mengembalikan teks & mengosongkan antrean
        send_cmd(&cmd_tx, json!({ "id": "cq", "type": "clear_queue" }));
        let qu_empty = recv_matching(&mut out_rx, std::time::Duration::from_millis(400), |v| {
            v["type"] == "queue_update"
        })
        .await;
        assert_eq!(qu_empty["steering"], json!([]));
        assert_eq!(qu_empty["followUp"], json!([]));
        let cq = recv_response(&mut out_rx, "cq", std::time::Duration::from_millis(400)).await;
        assert_eq!(cq["data"]["steering"], json!(["antre 1", "antre 2"]));
        assert_eq!(cq["data"]["followUp"], json!([]));

        // 5. Antre steer real lalu buka gate HTTP -> steer diinjeksikan sebelum call LLM berikutnya
        send_cmd(&cmd_tx, json!({ "id": "st2", "type": "steer", "message": "arah baru" }));
        let _ = recv_response(&mut out_rx, "st2", std::time::Duration::from_millis(400)).await;
        gate.notify_one();

        let _ = recv_matching(&mut out_rx, std::time::Duration::from_secs(3), |v| {
            v["type"] == "agent_settled" && v["aborted"] == false
        })
        .await;
        assert!(reqs.lock().unwrap()[1].body.to_string().contains("arah baru"));

        // 6. ChatController.interruptWith pattern: steer saat idle HARUS tetap queued sampai prompt datang
        let req_count_before = reqs.lock().unwrap().len();
        send_cmd(&cmd_tx, json!({ "id": "st_idle", "type": "steer", "message": "steer belakangan" }));
        let st_idle = recv_response(&mut out_rx, "st_idle", std::time::Duration::from_millis(400)).await;
        assert_eq!(st_idle["data"]["disposition"], "queued");

        send_cmd(&cmd_tx, json!({ "id": "gs_idle", "type": "get_state" }));
        let gs_idle = recv_response(&mut out_rx, "gs_idle", std::time::Duration::from_millis(400)).await;
        assert_eq!(gs_idle["data"]["isStreaming"], false);
        assert_eq!(gs_idle["data"]["pendingMessageCount"], 1);
        assert_eq!(reqs.lock().unwrap().len(), req_count_before);

        send_cmd(&cmd_tx, json!({ "id": "p_first", "type": "prompt", "message": "prompt duluan" }));
        let p_first = recv_response(&mut out_rx, "p_first", std::time::Duration::from_millis(400)).await;
        assert_eq!(p_first["data"]["disposition"], "started");
        let _ = recv_matching(&mut out_rx, std::time::Duration::from_secs(3), |v| {
            v["type"] == "agent_settled" && v["aborted"] == false
        })
        .await;

        let all_reqs = reqs.lock().unwrap().clone();
        assert!(all_reqs.len() >= req_count_before + 2);
        assert!(all_reqs[req_count_before].body.to_string().contains("prompt duluan"));
        assert!(all_reqs[req_count_before + 1].body.to_string().contains("steer belakangan"));

        drop(cmd_tx);
        let _ = tokio::time::timeout(std::time::Duration::from_secs(2), loop_task).await;
        mock_handle.abort();
    }

    #[tokio::test]
    async fn reg003_session_reload_preserves_tool_calls_across_providers() {
        let _sink_guard = output_sink_test_mutex().lock().await;
        let (_reset, mut out_rx) = install_channel_output_sink();
        let tmp = TempConfigGuard::new("reg003_reload");

        let session_file = tmp.path().join("tool_history.jsonl");
        let lines = [
            json!({ "type": "session", "version": 3, "id": "s-tools", "timestamp": "2026-11-12T00:00:00Z", "cwd": tmp.path() }).to_string(),
            json!({ "type": "message", "id": "e0", "parentId": null, "timestamp": "2026-11-12T00:00:01Z",
                "message": { "role": "user", "content": [{ "type": "text", "text": "cek repo" }], "timestamp": 1 } }).to_string(),
            // Assistant turn HANYA berisi thinking + toolCall (tanpa text block)
            json!({ "type": "message", "id": "e1", "parentId": "e0", "timestamp": "2026-11-12T00:00:02Z",
                "message": { "role": "assistant", "stopReason": "toolUse",
                    "content": [
                        { "type": "thinking", "thinking": "Checking dir..." },
                        { "type": "toolCall", "id": "c1", "name": "ls", "arguments": { "path": "." } }
                    ], "timestamp": 2 } }).to_string(),
            json!({ "type": "message", "id": "e2", "parentId": "e1", "timestamp": "2026-11-12T00:00:03Z",
                "message": { "role": "toolResult", "toolCallId": "c1", "toolName": "ls", "isError": false,
                    "content": [{ "type": "text", "text": "Cargo.toml\nsrc/" }], "timestamp": 3 } }).to_string(),
            // Two consecutive toolCalls + two consecutive toolResults (uji merging Anthropic & OpenAI)
            json!({ "type": "message", "id": "e3", "parentId": "e2", "timestamp": "2026-11-12T00:00:04Z",
                "message": { "role": "assistant", "stopReason": "toolUse",
                    "content": [
                        { "type": "toolCall", "id": "c2", "name": "read", "arguments": { "path": "Cargo.toml" } },
                        { "type": "toolCall", "id": "c3", "name": "bash", "arguments": { "command": "pwd" } }
                    ], "timestamp": 4 } }).to_string(),
            json!({ "type": "message", "id": "e4", "parentId": "e3", "timestamp": "2026-11-12T00:00:05Z",
                "message": { "role": "toolResult", "toolCallId": "c2", "toolName": "read", "isError": false,
                    "content": [{ "type": "text", "text": "name = \"oopbot\"" }], "timestamp": 5 } }).to_string(),
            json!({ "type": "message", "id": "e5", "parentId": "e4", "timestamp": "2026-11-12T00:00:06Z",
                "message": { "role": "toolResult", "toolCallId": "c3", "toolName": "bash", "isError": false,
                    "content": [{ "type": "text", "text": "/repo" }], "timestamp": 6 } }).to_string(),
            json!({ "type": "message", "id": "e6", "parentId": "e5", "timestamp": "2026-11-12T00:00:07Z",
                "message": { "role": "assistant", "stopReason": "stop",
                    "content": [{ "type": "text", "text": "Sudah dibaca." }], "timestamp": 7 } }).to_string(),
        ];
        std::fs::write(&session_file, format!("{}\n", lines.join("\n"))).unwrap();

        let (base_url, reqs, mock_handle) = spawn_mock_llm_server(move |_idx, req| async move {
            if req.path.contains("/chat/completions") {
                (200, json!({
                    "choices": [{ "message": { "role": "assistant", "content": "ok openai" } }],
                    "usage": { "prompt_tokens": 50, "completion_tokens": 10 }
                }))
            } else if req.path.ends_with("/messages") {
                (200, json!({
                    "content": [{ "type": "text", "text": "ok anthropic" }],
                    "usage": { "input_tokens": 50, "output_tokens": 10 }
                }))
            } else {
                (200, gemini_text_resp("ok gemini", 50, 10))
            }
        })
        .await;

        for (provider, api, model_id, suffix) in [
            ("google", "google-generative-ai", "gemini-2.5-flash", "/v1beta"),
            ("openrouter", "openai-completions", "qwen/qwen3-coder", "/v1"),
            ("anthropic", "anthropic-messages", "claude-sonnet-4", "/v1"),
        ] {
            let prov_dir = tmp.path().join(provider);
            std::fs::create_dir_all(&prov_dir).unwrap();
            write_mock_provider_config(
                &prov_dir,
                provider,
                api,
                model_id,
                &format!("{}{}", base_url, suffix),
                "k-test",
            );

            let mut server = RpcServer::new_in_dir_with_cwd(
                prov_dir,
                tmp.path().to_path_buf(),
                Some(session_file.to_string_lossy().to_string()),
                None,
                vec![],
            )
            .unwrap();

            let (cmd_tx, reader) = TestLineReader::new();
            let loop_task = tokio::spawn(async move {
                server.run_loop_with_input(io::BufReader::new(reader)).await.unwrap();
            });
            let _ = recv_matching(&mut out_rx, std::time::Duration::from_secs(2), |v| {
                v["topic"] == "pipkin:hello"
            })
            .await;

            send_cmd(&cmd_tx, json!({ "id": "p", "type": "prompt", "message": "tadi pwd apa?" }));
            let _ = recv_response(&mut out_rx, "p", std::time::Duration::from_millis(500)).await;
            let _ = recv_matching(&mut out_rx, std::time::Duration::from_secs(2), |v| {
                v["type"] == "agent_settled"
            })
            .await;

            drop(cmd_tx);
            let _ = tokio::time::timeout(std::time::Duration::from_secs(2), loop_task).await;
        }

        let captured = reqs.lock().unwrap().clone();
        assert_eq!(captured.len(), 3);

        // 1. Gemini payload
        let gem_str = captured[0].body.to_string();
        assert!(gem_str.contains("list_dir"), "Gemini missing reverse-mapped tool ls -> list_dir");
        assert!(gem_str.contains("run_shell"), "Gemini missing reverse-mapped tool bash -> run_shell");
        assert!(gem_str.contains("Cargo.toml\\nsrc/"));
        assert!(!gem_str.contains("Checking dir..."), "Synthetic thought without signature must not be replayed");

        // 2. OpenAI payload
        let oai_msgs = captured[1].body["messages"].as_array().unwrap();
        assert!(oai_msgs.iter().any(|m| m["role"] == "assistant" && m["tool_calls"].is_array()));
        assert!(oai_msgs.iter().any(|m| m["role"] == "tool" && m["tool_call_id"] == "c1"));
        assert!(oai_msgs.iter().any(|m| m["role"] == "tool" && m["tool_call_id"] == "c3"));

        // 3. Anthropic payload: tool_use + merged user tool_result + strict role alternation
        let ant_msgs = captured[2].body["messages"].as_array().unwrap();
        assert!(ant_msgs.iter().any(|m| {
            m["role"] == "user"
                && m["content"]
                    .as_array()
                    .map(|arr| {
                        arr.iter().filter(|b| b["type"] == "tool_result").count() == 2
                    })
                    .unwrap_or(false)
        }));
        for pair in ant_msgs.windows(2) {
            assert_ne!(pair[0]["role"], pair[1]["role"], "Anthropic requires strict role alternation");
        }

        mock_handle.abort();
    }

    #[tokio::test]
    async fn reg003_get_session_stats_full_fields() {
        let _sink_guard = output_sink_test_mutex().lock().await;
        let (_reset, mut out_rx) = install_channel_output_sink();
        let tmp = TempConfigGuard::new("reg003_stats");

        let (base_url, _reqs, mock_handle) = spawn_mock_llm_server(|_idx, _req| async move {
            (200, gemini_text_resp("jawaban stats", 420, 80))
        })
        .await;
        write_mock_provider_config(
            tmp.path(),
            "google",
            "google-generative-ai",
            "gemini-2.5-flash",
            &format!("{}/v1beta", base_url),
            "k-stats",
        );

        let session_file = tmp.path().join("stats.jsonl");
        let seed_lines = [
            json!({ "type": "session", "version": 3, "id": "s-stats", "timestamp": "2026-11-12T00:00:00Z", "cwd": tmp.path() }).to_string(),
            json!({ "type": "message", "id": "e0", "parentId": null, "timestamp": "2026-11-12T00:00:01Z",
                "message": { "role": "user", "content": "u1", "timestamp": 1 } }).to_string(),
            json!({ "type": "message", "id": "e1", "parentId": "e0", "timestamp": "2026-11-12T00:00:02Z",
                "message": { "role": "assistant", "stopReason": "toolUse",
                    "content": [{ "type": "toolCall", "id": "t1", "name": "read", "arguments": { "path": "a" } }],
                    "usage": { "input": 100, "output": 25, "totalTokens": 125 }, "timestamp": 2 } }).to_string(),
            json!({ "type": "message", "id": "e2", "parentId": "e1", "timestamp": "2026-11-12T00:00:03Z",
                "message": { "role": "toolResult", "toolCallId": "t1", "toolName": "read", "isError": false,
                    "content": [{ "type": "text", "text": "isi" }], "timestamp": 3 } }).to_string(),
            json!({ "type": "message", "id": "e3", "parentId": "e2", "timestamp": "2026-11-12T00:00:04Z",
                "message": { "role": "assistant", "stopReason": "stop",
                    "content": [{ "type": "text", "text": "selesai" }],
                    "usage": { "input": 200, "output": 30, "totalTokens": 230 }, "timestamp": 4 } }).to_string(),
        ];
        std::fs::write(&session_file, format!("{}\n", seed_lines.join("\n"))).unwrap();

        let mut server = RpcServer::new_in_dir_with_cwd(
            tmp.path().to_path_buf(),
            tmp.path().to_path_buf(),
            Some(session_file.to_string_lossy().to_string()),
            None,
            vec![],
        )
        .unwrap();

        let (cmd_tx, reader) = TestLineReader::new();
        let loop_task = tokio::spawn(async move {
            server.run_loop_with_input(io::BufReader::new(reader)).await.unwrap();
        });
        let _ = recv_matching(&mut out_rx, std::time::Duration::from_secs(2), |v| {
            v["topic"] == "pipkin:hello"
        })
        .await;

        send_cmd(&cmd_tx, json!({ "id": "p1", "type": "prompt", "message": "halo" }));
        let _ = recv_response(&mut out_rx, "p1", std::time::Duration::from_millis(500)).await;
        let _ = recv_matching(&mut out_rx, std::time::Duration::from_secs(2), |v| {
            v["type"] == "agent_settled"
        })
        .await;

        send_cmd(&cmd_tx, json!({ "id": "st", "type": "get_session_stats" }));
        let st = recv_response(&mut out_rx, "st", std::time::Duration::from_millis(500)).await;
        let data = &st["data"];
        assert_eq!(data["sessionId"], "s-stats");
        assert!(data["sessionFile"].as_str().unwrap_or("").ends_with("stats.jsonl"));
        assert_eq!(data["userMessages"], 2);
        assert_eq!(data["assistantMessages"], 3);
        assert_eq!(data["toolCalls"], 1);
        assert_eq!(data["toolResults"], 1);
        assert_eq!(data["totalMessages"], 6);
        assert_eq!(data["tokens"]["input"], 720); // 100 + 200 + 420
        assert_eq!(data["tokens"]["output"], 135); // 25 + 30 + 80
        assert_eq!(data["tokens"]["total"], 855);
        assert_eq!(data["contextUsage"]["tokens"], 500); // last prompt+out (420+80)
        assert_eq!(data["contextUsage"]["contextWindow"], 128000);

        drop(cmd_tx);
        let _ = tokio::time::timeout(std::time::Duration::from_secs(2), loop_task).await;
        mock_handle.abort();
    }

    #[tokio::test]
    async fn reg003_compact_summarizes_prunes_history_and_reloads() {
        let _sink_guard = output_sink_test_mutex().lock().await;
        let (_reset, mut out_rx) = install_channel_output_sink();
        let tmp = TempConfigGuard::new("reg003_compact");

        let (base_url, reqs, mock_handle) = spawn_mock_llm_server(|_idx, req| async move {
            let s = req.body.to_string();
            if s.contains("Focus on Rust") || s.contains("Ringkas") || s.contains("summarize") {
                (200, gemini_text_resp("## Goal\nRingkasan kompak terfokus Rust", 250, 40))
            } else {
                (200, gemini_text_resp("Pasca kompak oke", 90, 20))
            }
        })
        .await;
        write_mock_provider_config(
            tmp.path(),
            "google",
            "google-generative-ai",
            "gemini-2.5-flash",
            &format!("{}/v1beta", base_url),
            "k-compact",
        );

        let session_file = tmp.path().join("compact.jsonl");
        let lines = [
            json!({ "type": "session", "version": 3, "id": "s-cmp", "timestamp": "2026-11-12T00:00:00Z", "cwd": tmp.path() }).to_string(),
            json!({ "type": "message", "id": "e0", "parentId": null, "timestamp": "2026-11-12T00:00:01Z",
                "message": { "role": "user", "content": "pesan lama pertama yang akan diringkas", "timestamp": 1 } }).to_string(),
            json!({ "type": "message", "id": "e1", "parentId": "e0", "timestamp": "2026-11-12T00:00:02Z",
                "message": { "role": "assistant", "stopReason": "stop",
                    "content": [{ "type": "text", "text": "balasan lama pertama" }], "timestamp": 2 } }).to_string(),
            json!({ "type": "message", "id": "e2", "parentId": "e1", "timestamp": "2026-11-12T00:00:03Z",
                "message": { "role": "user", "content": "pesan lama kedua juga diringkas", "timestamp": 3 } }).to_string(),
            json!({ "type": "message", "id": "e3", "parentId": "e2", "timestamp": "2026-11-12T00:00:04Z",
                "message": { "role": "assistant", "stopReason": "stop",
                    "content": [{ "type": "text", "text": "balasan lama kedua" }], "timestamp": 4 } }).to_string(),
            json!({ "type": "message", "id": "e4", "parentId": "e3", "timestamp": "2026-11-12T00:00:05Z",
                "message": { "role": "user", "content": "pertanyaan terbaru dipertahankan", "timestamp": 5 } }).to_string(),
            json!({ "type": "message", "id": "e5", "parentId": "e4", "timestamp": "2026-11-12T00:00:06Z",
                "message": { "role": "assistant", "stopReason": "stop",
                    "content": [{ "type": "text", "text": "jawaban terbaru dipertahankan" }], "timestamp": 6 } }).to_string(),
        ];
        std::fs::write(&session_file, format!("{}\n", lines.join("\n"))).unwrap();

        let mut server = RpcServer::new_in_dir_with_cwd(
            tmp.path().to_path_buf(),
            tmp.path().to_path_buf(),
            Some(session_file.to_string_lossy().to_string()),
            None,
            vec![],
        )
        .unwrap();

        let (cmd_tx, reader) = TestLineReader::new();
        let loop_task = tokio::spawn(async move {
            server.run_loop_with_input(io::BufReader::new(reader)).await.unwrap();
        });
        let _ = recv_matching(&mut out_rx, std::time::Duration::from_secs(2), |v| {
            v["topic"] == "pipkin:hello"
        })
        .await;

        send_cmd(
            &cmd_tx,
            json!({ "id": "cmp1", "type": "compact", "customInstructions": "Focus on Rust" }),
        );
        let cmp_start = recv_matching(&mut out_rx, std::time::Duration::from_secs(2), |v| {
            v["type"] == "compaction_start"
        })
        .await;
        assert_eq!(cmp_start["reason"], "manual");

        let cmp_end = recv_matching(&mut out_rx, std::time::Duration::from_secs(2), |v| {
            v["type"] == "compaction_end"
        })
        .await;
        assert_eq!(cmp_end["aborted"], false);
        assert!(cmp_end["result"]["summary"]
            .as_str()
            .unwrap_or("")
            .contains("Ringkasan kompak terfokus Rust"));

        let cmp_resp = recv_response(&mut out_rx, "cmp1", std::time::Duration::from_secs(2)).await;
        assert_eq!(cmp_resp["success"], true);
        assert!(cmp_resp["data"]["firstKeptEntryId"].as_str().is_some());
        assert!(cmp_resp["data"]["tokensBefore"].as_u64().unwrap_or(0) > 0);
        assert!(cmp_resp["data"]["estimatedTokensAfter"].as_u64().unwrap_or(0) > 0);

        // Persis setelah compact, contextUsage.tokens & percent HARUS null
        send_cmd(&cmd_tx, json!({ "id": "st_after", "type": "get_session_stats" }));
        let st_after = recv_response(&mut out_rx, "st_after", std::time::Duration::from_millis(500)).await;
        assert!(st_after["data"]["contextUsage"]["tokens"].is_null());
        assert!(st_after["data"]["contextUsage"]["percent"].is_null());

        // Kirim prompt lanjutan -> LLM melihat ringkasan + pesan terbaru, BUKAN pesan awal yg terpotong
        send_cmd(&cmd_tx, json!({ "id": "p2", "type": "prompt", "message": "lanjut" }));
        let _ = recv_response(&mut out_rx, "p2", std::time::Duration::from_millis(500)).await;
        let _ = recv_matching(&mut out_rx, std::time::Duration::from_secs(2), |v| {
            v["type"] == "agent_settled"
        })
        .await;

        let captured = reqs.lock().unwrap().clone();
        assert!(captured.len() >= 2);
        let post_compact_body = captured.last().unwrap().body.to_string();
        assert!(post_compact_body.contains("Ringkasan kompak terfokus Rust"));
        assert!(!post_compact_body.contains("pesan lama pertama yang akan diringkas"));

        drop(cmd_tx);
        let _ = tokio::time::timeout(std::time::Duration::from_secs(2), loop_task).await;

        // Pastikan file JSONL mengandung entry compaction dan reload merekonstruksi konteks terkompresi
        let saved = std::fs::read_to_string(&session_file).unwrap();
        assert!(saved.contains("\"type\":\"compaction\""));
        let reloaded = RpcServer::new_in_dir_with_cwd(
            tmp.path().to_path_buf(),
            tmp.path().to_path_buf(),
            Some(session_file.to_string_lossy().to_string()),
            None,
            vec![],
        )
        .unwrap();
        let hist_str = serde_json::to_string(&reloaded.agent.active_history()).unwrap();
        assert!(hist_str.contains("Ringkasan kompak terfokus Rust"));
        assert!(!hist_str.contains("pesan lama pertama yang akan diringkas"));

        mock_handle.abort();
    }

    #[tokio::test]
    async fn reg003_fork_clone_and_export_html() {
        let _sink_guard = output_sink_test_mutex().lock().await;
        let (_reset, mut out_rx) = install_channel_output_sink();
        let tmp = TempConfigGuard::new("reg003_fork_clone_export");

        let session_file = tmp.path().join("orig.jsonl");
        let lines = [
            json!({ "type": "session", "version": 3, "id": "s-orig", "timestamp": "2026-11-12T00:00:00Z", "cwd": tmp.path() }).to_string(),
            json!({ "type": "message", "id": "e0", "parentId": null, "timestamp": "2026-11-12T00:00:01Z",
                "message": { "role": "user", "content": [{ "type": "text", "text": "prompt satu" }], "timestamp": 100 } }).to_string(),
            json!({ "type": "message", "id": "e1", "parentId": "e0", "timestamp": "2026-11-12T00:00:02Z",
                "message": { "role": "assistant", "stopReason": "stop",
                    "content": [{ "type": "text", "text": "jawaban <satu>" }], "timestamp": 200 } }).to_string(),
            json!({ "type": "message", "id": "e2", "parentId": "e1", "timestamp": "2026-11-12T00:00:03Z",
                "message": { "role": "user", "content": [{ "type": "text", "text": "prompt dua" }], "timestamp": 300 } }).to_string(),
            json!({ "type": "message", "id": "e3", "parentId": "e2", "timestamp": "2026-11-12T00:00:04Z",
                "message": { "role": "assistant", "stopReason": "stop",
                    "content": [{ "type": "text", "text": "jawaban dua" }], "timestamp": 400 } }).to_string(),
        ];
        std::fs::write(&session_file, format!("{}\n", lines.join("\n"))).unwrap();

        let mut server = RpcServer::new_in_dir_with_cwd(
            tmp.path().to_path_buf(),
            tmp.path().to_path_buf(),
            Some(session_file.to_string_lossy().to_string()),
            None,
            vec![],
        )
        .unwrap();

        let (cmd_tx, reader) = TestLineReader::new();
        let loop_task = tokio::spawn(async move {
            server.run_loop_with_input(io::BufReader::new(reader)).await.unwrap();
        });
        let _ = recv_matching(&mut out_rx, std::time::Duration::from_secs(2), |v| {
            v["topic"] == "pipkin:hello"
        })
        .await;

        // 1. get_fork_messages
        send_cmd(&cmd_tx, json!({ "id": "gfm", "type": "get_fork_messages" }));
        let gfm = recv_response(&mut out_rx, "gfm", std::time::Duration::from_millis(500)).await;
        assert_eq!(
            gfm["data"]["messages"],
            json!([
                { "entryId": "e0", "text": "prompt satu" },
                { "entryId": "e2", "text": "prompt dua" }
            ])
        );

        // 2. fork pada e2 -> memotong tepat sebelum e2, pindah ke file baru dengan parentSession
        send_cmd(&cmd_tx, json!({ "id": "fk", "type": "fork", "entryId": "e2" }));
        let fk = recv_response(&mut out_rx, "fk", std::time::Duration::from_millis(500)).await;
        assert_eq!(fk["success"], true);
        assert_eq!(fk["data"]["text"], "prompt dua");
        assert_eq!(fk["data"]["cancelled"], false);

        send_cmd(&cmd_tx, json!({ "id": "gs_fk", "type": "get_state" }));
        let gs_fk = recv_response(&mut out_rx, "gs_fk", std::time::Duration::from_millis(500)).await;
        let forked_path = gs_fk["data"]["sessionFile"].as_str().unwrap().to_string();
        assert_ne!(forked_path, session_file.to_string_lossy().to_string());
        let forked_contents = std::fs::read_to_string(&forked_path).unwrap();
        assert!(forked_contents.contains("\"parentSession\""));

        send_cmd(&cmd_tx, json!({ "id": "gm_fk", "type": "get_messages" }));
        let gm_fk = recv_response(&mut out_rx, "gm_fk", std::time::Duration::from_millis(500)).await;
        assert_eq!(gm_fk["data"]["messages"].as_array().unwrap().len(), 2);

        // 3. clone -> salin branch aktif saat ini ke file baru lainnya
        send_cmd(&cmd_tx, json!({ "id": "cl", "type": "clone" }));
        let cl = recv_response(&mut out_rx, "cl", std::time::Duration::from_millis(500)).await;
        assert_eq!(cl["success"], true);
        assert_eq!(cl["data"]["cancelled"], false);

        send_cmd(&cmd_tx, json!({ "id": "gs_cl", "type": "get_state" }));
        let gs_cl = recv_response(&mut out_rx, "gs_cl", std::time::Duration::from_millis(500)).await;
        let cloned_path = gs_cl["data"]["sessionFile"].as_str().unwrap().to_string();
        assert_ne!(cloned_path, forked_path);
        assert!(std::fs::read_to_string(&cloned_path).unwrap().contains("\"parentSession\""));

        // 4. export_html (default blank outputPath & relative outputPath)
        send_cmd(&cmd_tx, json!({ "id": "ex1", "type": "export_html" }));
        let ex1 = recv_response(&mut out_rx, "ex1", std::time::Duration::from_millis(500)).await;
        assert_eq!(ex1["success"], true);
        let html_path = ex1["data"]["path"].as_str().unwrap();
        assert!(html_path.ends_with(".html"));
        let html = std::fs::read_to_string(html_path).unwrap();
        assert!(html.contains("prompt satu"));
        assert!(html.contains("jawaban &lt;satu&gt;"));

        drop(cmd_tx);
        let _ = tokio::time::timeout(std::time::Duration::from_secs(2), loop_task).await;
    }

    #[tokio::test]
    async fn reg002_abort_manual_compact_and_prompt_during_compact() {
        let _sink_guard = output_sink_test_mutex().lock().await;
        let (_reset, mut out_rx) = install_channel_output_sink();
        let tmp = TempConfigGuard::new("reg002_abort_compact");

        let gate1 = Arc::new(tokio::sync::Notify::new());
        let gate2 = Arc::new(tokio::sync::Notify::new());
        let g1_srv = Arc::clone(&gate1);
        let g2_srv = Arc::clone(&gate2);
        let (hit0_tx, hit0_rx) = tokio::sync::oneshot::channel::<()>();
        let (hit1_tx, hit1_rx) = tokio::sync::oneshot::channel::<()>();
        let hit0_tx = Arc::new(Mutex::new(Some(hit0_tx)));
        let hit1_tx = Arc::new(Mutex::new(Some(hit1_tx)));

        let (base_url, reqs, mock_handle) = spawn_mock_llm_server(move |idx, _req| {
            let g1_srv = Arc::clone(&g1_srv);
            let g2_srv = Arc::clone(&g2_srv);
            let hit0_tx = Arc::clone(&hit0_tx);
            let hit1_tx = Arc::clone(&hit1_tx);
            async move {
                match idx {
                    0 => {
                        if let Some(tx) = hit0_tx.lock().unwrap().take() {
                            let _ = tx.send(());
                        }
                        g1_srv.notified().await;
                        (200, gemini_text_resp("tidak dipakai karena diabort", 20, 5))
                    }
                    1 => {
                        if let Some(tx) = hit1_tx.lock().unwrap().take() {
                            let _ = tx.send(());
                        }
                        g2_srv.notified().await;
                        (
                            200,
                            gemini_text_resp("Ringkasan kompak sukses sebelum prompt antre", 150, 30),
                        )
                    }
                    _ => (200, gemini_text_resp("Prompt setelah compact selesai", 80, 15)),
                }
            }
        })
        .await;
        write_mock_provider_config(
            tmp.path(),
            "google",
            "google-generative-ai",
            "gemini-2.5-flash",
            &format!("{}/v1beta", base_url),
            "k-compact-abort",
        );

        let session_file = tmp.path().join("compact_abort.jsonl");
        let seed_lines = [
            json!({ "type": "session", "version": 3, "id": "s-ca", "timestamp": "2026-11-12T00:00:00Z", "cwd": tmp.path() }).to_string(),
            json!({ "type": "message", "id": "e0", "parentId": null, "timestamp": "2026-11-12T00:00:01Z",
                "message": { "role": "user", "content": "pesan lama satu", "timestamp": 10 } }).to_string(),
            json!({ "type": "message", "id": "e1", "parentId": "e0", "timestamp": "2026-11-12T00:00:02Z",
                "message": { "role": "assistant", "stopReason": "stop",
                    "content": [{ "type": "text", "text": "jawaban lama satu" }], "timestamp": 20 } }).to_string(),
            json!({ "type": "message", "id": "e2", "parentId": "e1", "timestamp": "2026-11-12T00:00:03Z",
                "message": { "role": "user", "content": "pesan terbaru dua", "timestamp": 30 } }).to_string(),
            json!({ "type": "message", "id": "e3", "parentId": "e2", "timestamp": "2026-11-12T00:00:04Z",
                "message": { "role": "assistant", "stopReason": "stop",
                    "content": [{ "type": "text", "text": "jawaban terbaru dua" }], "timestamp": 40 } }).to_string(),
        ];
        std::fs::write(&session_file, format!("{}\n", seed_lines.join("\n"))).unwrap();

        let mut server = RpcServer::new_in_dir_with_cwd(
            tmp.path().to_path_buf(),
            tmp.path().to_path_buf(),
            Some(session_file.to_string_lossy().to_string()),
            None,
            vec![],
        )
        .unwrap();

        let (cmd_tx, reader) = TestLineReader::new();
        let loop_task = tokio::spawn(async move {
            server
                .run_loop_with_input(io::BufReader::new(reader))
                .await
                .unwrap();
        });
        let _ = recv_matching(&mut out_rx, std::time::Duration::from_secs(2), |v| {
            v["topic"] == "pipkin:hello"
        })
        .await;

        // Skenario A: abort di tengah compact manual
        send_cmd(&cmd_tx, json!({ "id": "cmp_ab", "type": "compact" }));
        let _ = recv_matching(&mut out_rx, std::time::Duration::from_secs(2), |v| {
            v["type"] == "compaction_start"
        })
        .await;
        tokio::time::timeout(std::time::Duration::from_secs(2), hit0_rx)
            .await
            .unwrap()
            .unwrap();

        send_cmd(&cmd_tx, json!({ "id": "gs_cmp", "type": "get_state" }));
        let gs_cmp = recv_response(&mut out_rx, "gs_cmp", std::time::Duration::from_millis(400)).await;
        assert_eq!(gs_cmp["data"]["isCompacting"], true);
        assert_eq!(gs_cmp["data"]["isStreaming"], false);

        let t0 = std::time::Instant::now();
        send_cmd(&cmd_tx, json!({ "id": "ab_cmp", "type": "abort" }));
        let cmp_end_ab = recv_matching(&mut out_rx, std::time::Duration::from_millis(600), |v| {
            v["type"] == "compaction_end"
        })
        .await;
        assert_eq!(cmp_end_ab["aborted"], true);
        let cmp_ab_resp =
            recv_response(&mut out_rx, "cmp_ab", std::time::Duration::from_millis(600)).await;
        assert_eq!(cmp_ab_resp["success"], false);
        let ab_cmp_resp =
            recv_response(&mut out_rx, "ab_cmp", std::time::Duration::from_millis(600)).await;
        assert_eq!(ab_cmp_resp["success"], true);
        assert!(t0.elapsed() < std::time::Duration::from_millis(600));

        send_cmd(&cmd_tx, json!({ "id": "gs_after_ab", "type": "get_state" }));
        let gs_after_ab =
            recv_response(&mut out_rx, "gs_after_ab", std::time::Duration::from_millis(400)).await;
        assert_eq!(gs_after_ab["data"]["isCompacting"], false);
        assert_eq!(gs_after_ab["data"]["messageCount"], 4);

        // Skenario B: prompt dg streamingBehavior="steer" diantrekan saat compact berjalan
        // harus otomatis dieksekusi segera sesudah compact selesai.
        send_cmd(&cmd_tx, json!({ "id": "cmp_ok", "type": "compact" }));
        let _ = recv_matching(&mut out_rx, std::time::Duration::from_secs(2), |v| {
            v["type"] == "compaction_start"
        })
        .await;
        tokio::time::timeout(std::time::Duration::from_secs(2), hit1_rx)
            .await
            .unwrap()
            .unwrap();

        send_cmd(
            &cmd_tx,
            json!({
                "id": "p_queued",
                "type": "prompt",
                "message": "prompt setelah compact",
                "streamingBehavior": "steer"
            }),
        );
        let q_ev = recv_matching(&mut out_rx, std::time::Duration::from_millis(400), |v| {
            v["type"] == "queue_update" && v["steering"] == json!(["prompt setelah compact"])
        })
        .await;
        assert_eq!(q_ev["followUp"], json!([]));
        let p_q_resp =
            recv_response(&mut out_rx, "p_queued", std::time::Duration::from_millis(400)).await;
        assert_eq!(p_q_resp["success"], true);
        assert_eq!(p_q_resp["data"]["disposition"], "queued");

        // Lepas compact kedua -> harus selesai lalu auto-start prompt yang diantrekan
        gate2.notify_one();
        let cmp_end_ok = recv_matching(&mut out_rx, std::time::Duration::from_secs(2), |v| {
            v["type"] == "compaction_end"
        })
        .await;
        assert_eq!(cmp_end_ok["aborted"], false);
        let cmp_ok_resp =
            recv_response(&mut out_rx, "cmp_ok", std::time::Duration::from_secs(2)).await;
        assert_eq!(cmp_ok_resp["success"], true);

        let q_cleared = recv_matching(&mut out_rx, std::time::Duration::from_secs(2), |v| {
            v["type"] == "queue_update" && v["steering"] == json!([])
        })
        .await;
        assert_eq!(q_cleared["followUp"], json!([]));

        let settled = recv_matching(&mut out_rx, std::time::Duration::from_secs(3), |v| {
            v["type"] == "agent_settled"
        })
        .await;
        assert_eq!(settled["aborted"], false);

        let captured = reqs.lock().unwrap().clone();
        assert!(captured.len() >= 3);
        let resumed_body = captured.last().unwrap().body.to_string();
        assert!(resumed_body.contains("Ringkasan kompak sukses sebelum prompt antre"));
        assert!(resumed_body.contains("prompt setelah compact"));

        drop(cmd_tx);
        let _ = tokio::time::timeout(std::time::Duration::from_secs(2), loop_task).await;
        mock_handle.abort();
    }

    #[tokio::test]
    async fn reg002_follow_up_modes_and_retry_abort_semantics() {
        let _sink_guard = output_sink_test_mutex().lock().await;
        let (_reset, mut out_rx) = install_channel_output_sink();
        let tmp = TempConfigGuard::new("reg002_retry_followup");

        let gate0 = Arc::new(tokio::sync::Notify::new());
        let gate0_srv = Arc::clone(&gate0);
        let (hit0_tx, hit0_rx) = tokio::sync::oneshot::channel::<()>();
        let hit0_tx = Arc::new(Mutex::new(Some(hit0_tx)));

        let (base_url, reqs, mock_handle) = spawn_mock_llm_server(move |idx, _req| {
            let gate0_srv = Arc::clone(&gate0_srv);
            let hit0_tx = Arc::clone(&hit0_tx);
            async move {
                match idx {
                    0 => {
                        if let Some(tx) = hit0_tx.lock().unwrap().take() {
                            let _ = tx.send(());
                        }
                        gate0_srv.notified().await;
                        (200, gemini_text_resp("Turn utama beres", 40, 10))
                    }
                    1 => (200, gemini_text_resp("Follow up batch beres", 50, 10)),
                    2 => (
                        429,
                        json!({ "error": { "message": "429 rate limit sementara" } }),
                    ),
                    3 => (200, gemini_text_resp("Pulih sesudah retry", 60, 12)),
                    4 => (
                        429,
                        json!({ "error": { "message": "429 throttle abort_retry" } }),
                    ),
                    _ => (
                        429,
                        json!({ "error": { "message": "429 throttle abort_run" } }),
                    ),
                }
            }
        })
        .await;
        write_mock_provider_config(
            tmp.path(),
            "google",
            "google-generative-ai",
            "gemini-2.5-flash",
            &format!("{}/v1beta", base_url),
            "k-retry",
        );

        let mut server = RpcServer::new_in_dir_with_cwd(
            tmp.path().to_path_buf(),
            tmp.path().to_path_buf(),
            None,
            None,
            vec![],
        )
        .unwrap();

        let (cmd_tx, reader) = TestLineReader::new();
        let loop_task = tokio::spawn(async move {
            server
                .run_loop_with_input(io::BufReader::new(reader))
                .await
                .unwrap();
        });
        let _ = recv_matching(&mut out_rx, std::time::Duration::from_secs(2), |v| {
            v["topic"] == "pipkin:hello"
        })
        .await;

        // 1. Verifikasi set_steering_mode, set_follow_up_mode, set_auto_compaction, set_auto_retry + validasi error
        send_cmd(
            &cmd_tx,
            json!({ "id": "m_bad", "type": "set_steering_mode", "mode": "random" }),
        );
        let m_bad = recv_response(&mut out_rx, "m_bad", std::time::Duration::from_millis(400)).await;
        assert_eq!(m_bad["success"], false);

        send_cmd(
            &cmd_tx,
            json!({ "id": "sm", "type": "set_steering_mode", "mode": "all" }),
        );
        send_cmd(
            &cmd_tx,
            json!({ "id": "fm", "type": "set_follow_up_mode", "mode": "all" }),
        );
        send_cmd(
            &cmd_tx,
            json!({ "id": "ac", "type": "set_auto_compaction", "enabled": false }),
        );
        send_cmd(
            &cmd_tx,
            json!({ "id": "ar", "type": "set_auto_retry", "enabled": true }),
        );
        for cid in ["sm", "fm", "ac", "ar"] {
            let r = recv_response(&mut out_rx, cid, std::time::Duration::from_millis(400)).await;
            assert_eq!(r["success"], true);
        }

        send_cmd(&cmd_tx, json!({ "id": "gs_cfg", "type": "get_state" }));
        let gs_cfg =
            recv_response(&mut out_rx, "gs_cfg", std::time::Duration::from_millis(400)).await;
        assert_eq!(gs_cfg["data"]["steeringMode"], "all");
        assert_eq!(gs_cfg["data"]["followUpMode"], "all");
        assert_eq!(gs_cfg["data"]["autoCompactionEnabled"], false);
        assert_eq!(gs_cfg["data"]["autoRetryEnabled"], true);

        // 2. Uji follow_up batching dalam followUpMode = "all"
        send_cmd(
            &cmd_tx,
            json!({ "id": "p_main", "type": "prompt", "message": "jalan utama" }),
        );
        let _ = recv_response(&mut out_rx, "p_main", std::time::Duration::from_millis(400)).await;
        tokio::time::timeout(std::time::Duration::from_secs(2), hit0_rx)
            .await
            .unwrap()
            .unwrap();

        send_cmd(
            &cmd_tx,
            json!({ "id": "fu1", "type": "follow_up", "message": "follow satu" }),
        );
        send_cmd(
            &cmd_tx,
            json!({
                "id": "fu2",
                "type": "prompt",
                "message": "follow dua",
                "streamingBehavior": "followUp"
            }),
        );
        let fu1_r = recv_response(&mut out_rx, "fu1", std::time::Duration::from_millis(400)).await;
        let fu2_r = recv_response(&mut out_rx, "fu2", std::time::Duration::from_millis(400)).await;
        assert_eq!(fu1_r["data"]["disposition"], "queued");
        assert_eq!(fu2_r["data"]["disposition"], "queued");

        gate0.notify_one();
        let settled_fu = recv_matching(&mut out_rx, std::time::Duration::from_secs(3), |v| {
            v["type"] == "agent_settled"
        })
        .await;
        assert_eq!(settled_fu["aborted"], false);
        let reqs_snapshot = reqs.lock().unwrap().clone();
        assert_eq!(reqs_snapshot.len(), 2);
        let batch_body = reqs_snapshot[1].body.to_string();
        assert!(batch_body.contains("follow satu"));
        assert!(batch_body.contains("follow dua"));

        // 3. Auto-retry pada 429 lalu sukses pada percobaan kedua
        send_cmd(
            &cmd_tx,
            json!({ "id": "p_retry", "type": "prompt", "message": "uji retry 429" }),
        );
        let _ = recv_response(&mut out_rx, "p_retry", std::time::Duration::from_millis(400)).await;
        let rs_ok = recv_matching(&mut out_rx, std::time::Duration::from_secs(2), |v| {
            v["type"] == "auto_retry_start"
        })
        .await;
        assert_eq!(rs_ok["attempt"], 1);
        let re_ok = recv_matching(&mut out_rx, std::time::Duration::from_secs(3), |v| {
            v["type"] == "auto_retry_end"
        })
        .await;
        assert_eq!(re_ok["success"], true);
        assert_eq!(re_ok["attempt"], 2);
        let settled_r = recv_matching(&mut out_rx, std::time::Duration::from_secs(2), |v| {
            v["type"] == "agent_settled"
        })
        .await;
        assert_eq!(settled_r["aborted"], false);

        // 4. abort_retry membatalkan loop retry dan menghasilkan stopReason="error"
        send_cmd(
            &cmd_tx,
            json!({ "id": "p_abr", "type": "prompt", "message": "uji abort_retry" }),
        );
        let _ = recv_response(&mut out_rx, "p_abr", std::time::Duration::from_millis(400)).await;
        let _ = recv_matching(&mut out_rx, std::time::Duration::from_secs(2), |v| {
            v["type"] == "auto_retry_start"
        })
        .await;
        send_cmd(&cmd_tx, json!({ "id": "abr", "type": "abort_retry" }));
        let abr_r = recv_response(&mut out_rx, "abr", std::time::Duration::from_millis(400)).await;
        assert_eq!(abr_r["success"], true);
        let re_abr = recv_matching(&mut out_rx, std::time::Duration::from_secs(2), |v| {
            v["type"] == "auto_retry_end"
        })
        .await;
        assert_eq!(re_abr["success"], false);
        let err_msg_ev = recv_matching(&mut out_rx, std::time::Duration::from_secs(2), |v| {
            v["type"] == "message_end" && v["message"]["stopReason"] == "error"
        })
        .await;
        assert_eq!(err_msg_ev["message"]["role"], "assistant");
        let _ = recv_matching(&mut out_rx, std::time::Duration::from_secs(2), |v| {
            v["type"] == "agent_settled" && v["aborted"] == false
        })
        .await;

        // 5. abort penuh saat retry sleep -> auto_retry_end + abort_msg (timestamp == user_msg.timestamp) + turn_end + agent_end + agent_settled
        send_cmd(
            &cmd_tx,
            json!({ "id": "p_ab_run", "type": "prompt", "message": "uji abort saat retry" }),
        );
        let _ = recv_response(&mut out_rx, "p_ab_run", std::time::Duration::from_millis(400)).await;
        let user_ev = recv_matching(&mut out_rx, std::time::Duration::from_secs(2), |v| {
            v["type"] == "message_end"
                && v["message"]["role"] == "user"
                && v["message"]["content"][0]["text"] == "uji abort saat retry"
        })
        .await;
        let user_ts = user_ev["message"]["timestamp"].as_u64().unwrap();
        let _ = recv_matching(&mut out_rx, std::time::Duration::from_secs(2), |v| {
            v["type"] == "auto_retry_start"
        })
        .await;

        send_cmd(&cmd_tx, json!({ "id": "ab_run", "type": "abort" }));
        let re_ab_run = recv_matching(&mut out_rx, std::time::Duration::from_millis(600), |v| {
            v["type"] == "auto_retry_end"
        })
        .await;
        assert_eq!(re_ab_run["success"], false);
        let abort_msg_ev = recv_matching(&mut out_rx, std::time::Duration::from_millis(600), |v| {
            v["type"] == "message_end" && v["message"]["stopReason"] == "aborted"
        })
        .await;
        assert_eq!(abort_msg_ev["message"]["timestamp"].as_u64(), Some(user_ts));
        let _ = recv_matching(&mut out_rx, std::time::Duration::from_millis(600), |v| {
            v["type"] == "turn_end"
        })
        .await;
        let _ = recv_matching(&mut out_rx, std::time::Duration::from_millis(600), |v| {
            v["type"] == "agent_end"
        })
        .await;
        let settled_ab = recv_matching(&mut out_rx, std::time::Duration::from_millis(600), |v| {
            v["type"] == "agent_settled"
        })
        .await;
        assert_eq!(settled_ab["aborted"], true);
        let ab_run_r =
            recv_response(&mut out_rx, "ab_run", std::time::Duration::from_millis(600)).await;
        assert_eq!(ab_run_r["success"], true);

        drop(cmd_tx);
        let _ = tokio::time::timeout(std::time::Duration::from_secs(2), loop_task).await;
        mock_handle.abort();
    }

    #[tokio::test]
    async fn reg003_fork_root_new_switch_entries_and_relative_export() {
        let _sink_guard = output_sink_test_mutex().lock().await;
        let (_reset, mut out_rx) = install_channel_output_sink();
        let tmp = TempConfigGuard::new("reg003_nav");

        let session_file = tmp.path().join("orig.jsonl");
        let lines = [
            json!({ "type": "session", "version": 3, "id": "s-nav", "timestamp": "2026-11-12T00:00:00Z", "cwd": tmp.path() }).to_string(),
            json!({ "type": "message", "id": "e0", "parentId": null, "timestamp": "2026-11-12T00:00:01Z",
                "message": { "role": "user", "content": [{ "type": "text", "text": "prompt satu" }], "timestamp": 100 } }).to_string(),
            json!({ "type": "message", "id": "e1", "parentId": "e0", "timestamp": "2026-11-12T00:00:02Z",
                "message": { "role": "assistant", "stopReason": "stop",
                    "content": [{ "type": "text", "text": "jawaban satu" }], "timestamp": 200 } }).to_string(),
            json!({ "type": "message", "id": "e2", "parentId": "e1", "timestamp": "2026-11-12T00:00:03Z",
                "message": { "role": "user", "content": [{ "type": "text", "text": "prompt dua" }], "timestamp": 300 } }).to_string(),
            json!({ "type": "message", "id": "e3", "parentId": "e2", "timestamp": "2026-11-12T00:00:04Z",
                "message": { "role": "assistant", "stopReason": "stop",
                    "content": [{ "type": "text", "text": "jawaban dua" }], "timestamp": 400 } }).to_string(),
        ];
        std::fs::write(&session_file, format!("{}\n", lines.join("\n"))).unwrap();

        let mut server = RpcServer::new_in_dir_with_cwd(
            tmp.path().to_path_buf(),
            tmp.path().to_path_buf(),
            Some(session_file.to_string_lossy().to_string()),
            None,
            vec![],
        )
        .unwrap();

        let (cmd_tx, reader) = TestLineReader::new();
        let loop_task = tokio::spawn(async move {
            server
                .run_loop_with_input(io::BufReader::new(reader))
                .await
                .unwrap();
        });
        let _ = recv_matching(&mut out_rx, std::time::Duration::from_secs(2), |v| {
            v["topic"] == "pipkin:hello"
        })
        .await;

        // 1. get_entries full & since cursor & unknown cursor
        send_cmd(&cmd_tx, json!({ "id": "ge_all", "type": "get_entries" }));
        let ge_all =
            recv_response(&mut out_rx, "ge_all", std::time::Duration::from_millis(400)).await;
        assert_eq!(ge_all["data"]["entries"].as_array().unwrap().len(), 4);
        assert_eq!(ge_all["data"]["leafId"], "e3");

        send_cmd(
            &cmd_tx,
            json!({ "id": "ge_since", "type": "get_entries", "since": "e1" }),
        );
        let ge_since =
            recv_response(&mut out_rx, "ge_since", std::time::Duration::from_millis(400)).await;
        let slice = ge_since["data"]["entries"].as_array().unwrap();
        assert_eq!(slice.len(), 2);
        assert_eq!(slice[0]["id"], "e2");
        assert_eq!(slice[1]["id"], "e3");

        send_cmd(
            &cmd_tx,
            json!({ "id": "ge_bad", "type": "get_entries", "since": "missing" }),
        );
        let ge_bad =
            recv_response(&mut out_rx, "ge_bad", std::time::Duration::from_millis(400)).await;
        assert_eq!(ge_bad["success"], false);

        // 2. fork pada root "e0" -> menghasilkan sesi kosong dg parentSession menunjuk orig.jsonl
        send_cmd(&cmd_tx, json!({ "id": "fk0", "type": "fork", "entryId": "e0" }));
        let fk0 = recv_response(&mut out_rx, "fk0", std::time::Duration::from_millis(400)).await;
        assert_eq!(fk0["success"], true);
        assert_eq!(fk0["data"]["text"], "prompt satu");

        send_cmd(&cmd_tx, json!({ "id": "gm0", "type": "get_messages" }));
        let gm0 = recv_response(&mut out_rx, "gm0", std::time::Duration::from_millis(400)).await;
        assert_eq!(gm0["data"]["messages"].as_array().unwrap().len(), 0);

        send_cmd(&cmd_tx, json!({ "id": "gs0", "type": "get_state" }));
        let gs0 = recv_response(&mut out_rx, "gs0", std::time::Duration::from_millis(400)).await;
        let forked0 = gs0["data"]["sessionFile"].as_str().unwrap().to_string();
        assert!(std::fs::read_to_string(&forked0)
            .unwrap()
            .contains("\"parentSession\""));

        // 3. new_session (dengan parentSession & tanpa parentSession)
        send_cmd(
            &cmd_tx,
            json!({ "id": "ns", "type": "new_session", "parentSession": forked0 }),
        );
        let ns = recv_response(&mut out_rx, "ns", std::time::Duration::from_millis(400)).await;
        assert_eq!(ns["success"], true);
        assert_eq!(ns["data"]["cancelled"], false);

        send_cmd(&cmd_tx, json!({ "id": "gs_ns", "type": "get_state" }));
        let gs_ns =
            recv_response(&mut out_rx, "gs_ns", std::time::Duration::from_millis(400)).await;
        let ns_file = gs_ns["data"]["sessionFile"].as_str().unwrap().to_string();
        assert_ne!(ns_file, forked0);
        assert!(std::fs::read_to_string(&ns_file)
            .unwrap()
            .contains("\"parentSession\""));

        // 4. switch_session kembali ke orig.jsonl memulihkan seluruh 4 pesan + export_html path relatif nested
        send_cmd(
            &cmd_tx,
            json!({ "id": "sw", "type": "switch_session", "sessionPath": session_file }),
        );
        let sw = recv_response(&mut out_rx, "sw", std::time::Duration::from_millis(400)).await;
        assert_eq!(sw["success"], true);

        send_cmd(&cmd_tx, json!({ "id": "gm_sw", "type": "get_messages" }));
        let gm_sw =
            recv_response(&mut out_rx, "gm_sw", std::time::Duration::from_millis(400)).await;
        assert_eq!(gm_sw["data"]["messages"].as_array().unwrap().len(), 4);

        send_cmd(
            &cmd_tx,
            json!({ "id": "ex_rel", "type": "export_html", "outputPath": "reports/nested.html" }),
        );
        let ex_rel =
            recv_response(&mut out_rx, "ex_rel", std::time::Duration::from_millis(400)).await;
        assert_eq!(ex_rel["success"], true);
        let expected_rel = tmp.path().join("reports/nested.html");
        assert_eq!(
            PathBuf::from(ex_rel["data"]["path"].as_str().unwrap()),
            expected_rel
        );
        assert!(std::fs::read_to_string(&expected_rel)
            .unwrap()
            .contains("prompt dua"));

        if let Ok(home) = std::env::var("HOME") {
            assert_eq!(
                expand_home_or_resolve("~/reports/out.html", tmp.path()),
                PathBuf::from(home).join("reports/out.html")
            );
        }

        drop(cmd_tx);
        let _ = tokio::time::timeout(std::time::Duration::from_secs(2), loop_task).await;
    }

    #[tokio::test]
    async fn reg003_legacy_missing_tool_ids_orphan_abort_and_consecutive_users() {
        let _sink_guard = output_sink_test_mutex().lock().await;
        let (_reset, mut out_rx) = install_channel_output_sink();
        let tmp = TempConfigGuard::new("reg003_legacy_ids");

        let session_file = tmp.path().join("legacy_ids.jsonl");
        let lines = [
            json!({ "type": "session", "version": 3, "id": "s-legacy", "timestamp": "2026-11-12T00:00:00Z", "cwd": tmp.path() }).to_string(),
            // Dua user prompt berturut-turut
            json!({ "type": "message", "id": "e0", "parentId": null, "timestamp": "2026-11-12T00:00:01Z",
                "message": { "role": "user", "content": [{ "type": "text", "text": "pertanyaan awal" }], "timestamp": 1 } }).to_string(),
            json!({ "type": "message", "id": "e1", "parentId": "e0", "timestamp": "2026-11-12T00:00:02Z",
                "message": { "role": "user", "content": [
                    { "type": "text", "text": "pertanyaan susulan" },
                    { "type": "image", "mimeType": "image/png", "data": "iVBORw0KGgo=" }
                ], "timestamp": 2 } }).to_string(),
            // Assistant memiliki text diikuti toolCall TANPA id + toolCall DENGAN id yang terputus
            json!({ "type": "message", "id": "e2", "parentId": "e1", "timestamp": "2026-11-12T00:00:03Z",
                "message": { "role": "assistant", "stopReason": "toolUse",
                    "content": [
                        { "type": "text", "text": "Cek file dulu." },
                        { "type": "toolCall", "name": "read", "arguments": { "path": "Cargo.toml" } },
                        { "type": "toolCall", "id": "c_interrupted", "name": "bash", "arguments": { "command": "sleep 9" } }
                    ], "timestamp": 3 } }).to_string(),
            // toolResult TANPA toolCallId untuk toolCall read yang juga tanpa id
            json!({ "type": "message", "id": "e3", "parentId": "e2", "timestamp": "2026-11-12T00:00:04Z",
                "message": { "role": "toolResult", "toolCallId": "", "toolName": "read", "isError": false,
                    "content": [{ "type": "text", "text": "isi cargo legacy" }], "timestamp": 4 } }).to_string(),
        ];
        std::fs::write(&session_file, format!("{}\n", lines.join("\n"))).unwrap();

        let (base_url, reqs, mock_handle) = spawn_mock_llm_server(move |_idx, req| async move {
            if req.path.contains("/chat/completions") {
                (200, json!({
                    "choices": [{ "message": { "role": "assistant", "content": "ok oai" } }],
                    "usage": { "prompt_tokens": 30, "completion_tokens": 5 }
                }))
            } else if req.path.ends_with("/messages") {
                (200, json!({
                    "content": [{ "type": "text", "text": "ok ant" }],
                    "usage": { "input_tokens": 30, "output_tokens": 5 }
                }))
            } else {
                (200, gemini_text_resp("ok gem", 30, 5))
            }
        })
        .await;

        for (provider, api, model_id, suffix) in [
            ("google", "google-generative-ai", "gemini-2.5-flash", "/v1beta"),
            ("openrouter", "openai-completions", "qwen/qwen3-coder", "/v1"),
            ("anthropic", "anthropic-messages", "claude-sonnet-4", "/v1"),
        ] {
            let prov_dir = tmp.path().join(provider);
            std::fs::create_dir_all(&prov_dir).unwrap();
            write_mock_provider_config(
                &prov_dir,
                provider,
                api,
                model_id,
                &format!("{}{}", base_url, suffix),
                "k-legacy",
            );

            let mut server = RpcServer::new_in_dir_with_cwd(
                prov_dir,
                tmp.path().to_path_buf(),
                Some(session_file.to_string_lossy().to_string()),
                None,
                vec![],
            )
            .unwrap();

            let (cmd_tx, reader) = TestLineReader::new();
            let loop_task = tokio::spawn(async move {
                server
                    .run_loop_with_input(io::BufReader::new(reader))
                    .await
                    .unwrap();
            });
            let _ = recv_matching(&mut out_rx, std::time::Duration::from_secs(2), |v| {
                v["topic"] == "pipkin:hello"
            })
            .await;

            send_cmd(&cmd_tx, json!({ "id": "p", "type": "prompt", "message": "lanjut" }));
            let _ = recv_response(&mut out_rx, "p", std::time::Duration::from_millis(500)).await;
            let _ = recv_matching(&mut out_rx, std::time::Duration::from_secs(2), |v| {
                v["type"] == "agent_settled"
            })
            .await;

            drop(cmd_tx);
            let _ = tokio::time::timeout(std::time::Duration::from_secs(2), loop_task).await;
        }

        let captured = reqs.lock().unwrap().clone();
        assert_eq!(captured.len(), 3);

        // 1. Gemini: 2 user turn awal ter-coalesce + reloaded_call_* berpasangan + interrupted call terisi error
        let gem_contents = captured[0].body["contents"].as_array().unwrap();
        assert_eq!(gem_contents[0]["role"], "user");
        assert!(gem_contents[0]["parts"]
            .as_array()
            .unwrap()
            .iter()
            .any(|p| p["text"] == "pertanyaan awal"));
        assert!(gem_contents[0]["parts"]
            .as_array()
            .unwrap()
            .iter()
            .any(|p| p["text"] == "pertanyaan susulan"));
        let gem_str = captured[0].body.to_string();
        assert!(gem_str.contains("reloaded_call_"));
        assert!(gem_str.contains("isi cargo legacy"));
        assert!(gem_str.contains("Tool call interrupted before completion"));

        // 2. OpenAI: 2 user berturut-turut tergabung (termasuk multimodal), ID tool_calls & tool messages identik
        let oai_msgs = captured[1].body["messages"].as_array().unwrap();
        for pair in oai_msgs.windows(2) {
            assert!(
                !(pair[0]["role"] == "user" && pair[1]["role"] == "user"),
                "OpenAI consecutive user messages must be coalesced"
            );
        }
        let asst_msg = oai_msgs
            .iter()
            .find(|m| m["role"] == "assistant" && m["tool_calls"].is_array())
            .unwrap();
        let tc_ids: Vec<&str> = asst_msg["tool_calls"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|tc| tc["id"].as_str())
            .collect();
        assert_eq!(tc_ids.len(), 2);
        assert!(tc_ids[0].starts_with("reloaded_call_"));
        assert_eq!(tc_ids[1], "c_interrupted");
        assert!(oai_msgs.iter().any(|m| {
            m["role"] == "tool"
                && m["tool_call_id"] == tc_ids[0]
                && m["content"].as_str().unwrap_or("").contains("isi cargo legacy")
        }));
        assert!(oai_msgs.iter().any(|m| {
            m["role"] == "tool"
                && m["tool_call_id"] == "c_interrupted"
                && m["content"]
                    .as_str()
                    .unwrap_or("")
                    .contains("Tool call interrupted before completion")
        }));

        // 3. Anthropic: role strict alternation & matching tool_use.id == tool_result.tool_use_id
        let ant_msgs = captured[2].body["messages"].as_array().unwrap();
        for pair in ant_msgs.windows(2) {
            assert_ne!(
                pair[0]["role"], pair[1]["role"],
                "Anthropic consecutive same-role messages must be merged"
            );
        }
        let ant_str = captured[2].body.to_string();
        assert!(ant_str.contains(tc_ids[0]));
        assert!(ant_str.contains("isi cargo legacy"));
        assert!(ant_str.contains("Tool call interrupted before completion"));

        mock_handle.abort();
    }

    #[tokio::test]
    async fn reg002_stdin_eof_during_active_turn_exits_cleanly() {
        let _sink_guard = output_sink_test_mutex().lock().await;
        let (_reset, mut out_rx) = install_channel_output_sink();
        let tmp = TempConfigGuard::new("reg002_eof");

        let (hit_tx, hit_rx) = tokio::sync::oneshot::channel::<()>();
        let hit_tx = Arc::new(Mutex::new(Some(hit_tx)));
        let (base_url, _reqs, mock_handle) = spawn_mock_llm_server(move |_idx, _req| {
            let hit_tx = Arc::clone(&hit_tx);
            async move {
                if let Some(tx) = hit_tx.lock().unwrap().take() {
                    let _ = tx.send(());
                }
                tokio::time::sleep(std::time::Duration::from_secs(15)).await;
                (200, gemini_text_resp("tidak akan selesai", 10, 5))
            }
        })
        .await;
        write_mock_provider_config(
            tmp.path(),
            "google",
            "google-generative-ai",
            "gemini-2.5-flash",
            &format!("{}/v1beta", base_url),
            "k-eof",
        );

        let mut server = RpcServer::new_in_dir_with_cwd(
            tmp.path().to_path_buf(),
            tmp.path().to_path_buf(),
            None,
            None,
            vec![],
        )
        .unwrap();

        let (cmd_tx, reader) = TestLineReader::new();
        let loop_task = tokio::spawn(async move {
            server
                .run_loop_with_input(io::BufReader::new(reader))
                .await
                .unwrap();
        });
        let _ = recv_matching(&mut out_rx, std::time::Duration::from_secs(2), |v| {
            v["topic"] == "pipkin:hello"
        })
        .await;

        send_cmd(
            &cmd_tx,
            json!({ "id": "p_eof", "type": "prompt", "message": "hang forever" }),
        );
        let _ = recv_response(&mut out_rx, "p_eof", std::time::Duration::from_millis(500)).await;
        tokio::time::timeout(std::time::Duration::from_secs(2), hit_rx)
            .await
            .unwrap()
            .unwrap();

        // Tutup stdin secara mendadak saat HTTP in-flight -> run_loop harus segera keluar
        let closed_at = std::time::Instant::now();
        drop(cmd_tx);
        tokio::time::timeout(std::time::Duration::from_secs(1), loop_task)
            .await
            .expect("RPC loop hung on stdin EOF while turn was active")
            .unwrap();
        assert!(closed_at.elapsed() < std::time::Duration::from_secs(1));

        mock_handle.abort();
    }

    #[tokio::test]
    async fn reg011_live_todo_widget_emitted_on_tool_call_end_and_background_active_count() {
        let tmp = TempConfigGuard::new("reg011_live_widget");
        let mut reg = PluginRegistry::with_defaults();
        assert!(reg.activate("pipkin-todo"));
        assert!(reg.activate("pipkin-background"));
        let mut tools = ToolRegistry::new();
        reg.register_active_tools(&mut tools);
        let plugins = Arc::new(reg);

        let emitted = Arc::new(Mutex::new(Vec::<Value>::new()));
        let emitted_clone = Arc::clone(&emitted);
        let ui_state = Arc::new(Mutex::new(RpcUiState::from_loaded_session(
            init_or_load_session(tmp.path(), tmp.path(), None),
        )));
        let sink = RpcEventSink::with_emitter(ui_state, move |val| {
            emitted_clone.lock().unwrap().push(val.clone());
        })
        .with_plugins(Arc::clone(&plugins));

        // 1. Jalankan todo tool -> kirim ToolCallEnd -> setWidget harus terpancar langsung (sebelum TurnEnd)
        let todo_tool = tools.get("todo").unwrap();
        let todo_out = todo_tool
            .execute(&json!({ "action": "create", "subject": "Widget Live" }))
            .await
            .unwrap();
        sink.on_event(&AgentEvent::ToolCallEnd {
            step_index: 0,
            call_id: "call_todo_1".into(),
            tool_name: "todo".into(),
            status: ToolStatus::Done,
            output: todo_out,
            duration_ms: 2,
        });

        let events_after_todo = emitted.lock().unwrap().clone();
        let widget_evt = events_after_todo
            .iter()
            .find(|v| v["type"] == "extension_ui_request" && v["method"] == "setWidget")
            .expect("setWidget must be emitted immediately on todo ToolCallEnd");
        assert_eq!(widget_evt["widgetKey"], "todos");
        assert!(widget_evt["widgetLines"]
            .as_array()
            .unwrap()
            .iter()
            .any(|l| l.as_str().unwrap_or("").contains("Widget Live")));

        // Clear todo -> setWidget tanpa widgetLines harus terpancar untuk menghapus widget UI
        let clear_out = todo_tool
            .execute(&json!({ "action": "clear" }))
            .await
            .unwrap();
        emitted.lock().unwrap().clear();
        sink.on_event(&AgentEvent::ToolCallEnd {
            step_index: 0,
            call_id: "call_todo_2".into(),
            tool_name: "todo".into(),
            status: ToolStatus::Done,
            output: clear_out,
            duration_ms: 1,
        });
        let events_after_clear = emitted.lock().unwrap().clone();
        let clear_evt = events_after_clear
            .iter()
            .find(|v| v["type"] == "extension_ui_request" && v["method"] == "setWidget")
            .expect("setWidget clear must be emitted when todo becomes empty");
        assert!(clear_evt.get("widgetLines").is_none());

        // 2. Jalankan background tool -> ToolCallEnd harus memancarkan pipkin_channel active: 1, lalu kill -> active: 0
        let bg_tool = tools.get("background").unwrap();
        let bg_start = bg_tool
            .execute(&json!({ "action": "start", "command": "sleep 30" }))
            .await
            .unwrap();
        let bg_id = bg_start["id"].as_u64().unwrap();
        emitted.lock().unwrap().clear();
        sink.on_event(&AgentEvent::ToolCallEnd {
            step_index: 0,
            call_id: "call_bg_1".into(),
            tool_name: "background".into(),
            status: ToolStatus::Done,
            output: bg_start,
            duration_ms: 3,
        });
        let bg_events = emitted.lock().unwrap().clone();
        let ch_active = bg_events
            .iter()
            .find(|v| v["type"] == "pipkin_channel" && v["topic"] == "background")
            .expect("background channel must be emitted on ToolCallEnd");
        assert_eq!(ch_active["data"]["active"], 1);

        let bg_kill = bg_tool
            .execute(&json!({ "action": "kill", "id": bg_id }))
            .await
            .unwrap();
        emitted.lock().unwrap().clear();
        sink.on_event(&AgentEvent::ToolCallEnd {
            step_index: 0,
            call_id: "call_bg_2".into(),
            tool_name: "background".into(),
            status: ToolStatus::Done,
            output: bg_kill,
            duration_ms: 1,
        });
        let bg_events_after = emitted.lock().unwrap().clone();
        let ch_killed = bg_events_after
            .iter()
            .find(|v| v["type"] == "pipkin_channel" && v["topic"] == "background")
            .unwrap();
        assert_eq!(ch_killed["data"]["active"], 0);
    }

    #[test]
    fn reg005_available_models_empty_scope_and_no_local_duplicate() {
        let tmp = TempConfigGuard::new("reg005_models_scope");
        std::fs::write(
            tmp.path().join("settings.json"),
            json!({ "enabledModels": [] }).to_string(),
        )
        .unwrap();
        std::fs::write(
            tmp.path().join("models.json"),
            json!({
                "providers": {
                    "pipkin-local": {
                        "baseUrl": "http://127.0.0.1:11434/v1",
                        "api": "openai-completions",
                        "models": [{ "id": "llama3.2", "name": "Llama 3.2" }]
                    }
                }
            })
            .to_string(),
        )
        .unwrap();

        let server = RpcServer::new_in_dir_with_cwd(
            tmp.path().to_path_buf(),
            tmp.path().to_path_buf(),
            None,
            None,
            vec![],
        )
        .unwrap();
        let models = server.available_models_json();
        let local_matches: Vec<&Value> = models
            .iter()
            .filter(|m| m["id"] == "llama3.2" && m["provider"] == "pipkin-local")
            .collect();
        assert_eq!(
            local_matches.len(),
            1,
            "empty enabledModels must leave scope unrestricted and local model must appear once"
        );
    }
}
