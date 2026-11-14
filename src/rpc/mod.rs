//! Mode RPC (`oopbot --mode rpc`) yang kompatibel 100% dengan protokol JSONL
//! aplikasi Android Pipkin (`AgentProcess.kt` & `ChatTimeline.kt`).
//!
//! Menggantikan `@earendil-works/pi-coding-agent` (`pi --mode rpc`) di Termux
//! dengan binary native Rust `oopbot`.


mod approver;
mod commands;
mod compaction;
mod events;
mod providers;
mod session_store;

#[cfg(test)]
mod tests;

pub use approver::{PendingApprovals, RpcApprover};
pub use events::RpcEventSink;
pub use session_store::{RpcUiState, TokenTotals};

use commands::*;
use events::*;
use providers::*;
use session_store::*;

use crate::agent::GeminiAgent;
use crate::gemini::{BoxError, GeminiClient, DEFAULT_GEMINI_BASE_URL};
use crate::models::ModelCatalog;
use crate::plugins::PluginRegistry;
use crate::session::WorkspaceTabs;
use crate::tools::ToolRegistry;
use crate::util::read_json_file;
use serde_json::{json, Value};
use std::collections::{HashMap, VecDeque};
use std::io::{self, BufRead, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, RwLock};

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

        let branch_entries = active_branch_entries(&loaded.entries, loaded.leaf_id.as_deref());
        plugins.replay_branch(&branch_entries);

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
                } else {
                    Self::send_line(&json!({
                        "type": "extension_ui_request",
                        "id": "todos-widget",
                        "method": "setWidget",
                        "widgetKey": "todos",
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
                                { "name": "todos", "description": "Tampilkan semua todo pada branch aktif (/todos [clear])" },
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

}

pub async fn run_rpc_mode(
    session_path: Option<String>,
    system_append: Option<String>,
    extensions: Vec<String>,
) -> Result<(), BoxError> {
    let mut server = RpcServer::new(session_path, system_append, extensions)?;
    server.run_loop().await
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

