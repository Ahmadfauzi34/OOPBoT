//! State machine runtime RPC (`RpcRuntime`, `ActiveOp`, `QueuedPrompt`, `CoordinatorFinishGuard`),
//! callback plugin (`BackgroundPlugin`, `InteractPlugin`), dialog UI dua arah, dan frame output JSONL.

use super::{
    active_branch_entries, default_rpc_system_instruction, init_or_load_session, load_providers,
    PendingApprovals, ProviderEndpoint, RpcApprover, RpcEventSink, RpcUiState, OUTPUT_SINK,
};
use crate::agent::{AgentHarness, WorkspaceTabs};
use crate::common::fs::read_json_file;
use crate::common::BoxError;
use crate::config::ModelCatalog;
use crate::llm::{LlmClient, DEFAULT_GEMINI_BASE_URL};
use crate::plugins::{
    BackgroundPlugin, InteractPlugin, PluginRegistry, UiDialogAnswer, UiDialogKind,
    UiDialogRequest,
};
use crate::tools::ToolRegistry;
use serde_json::{json, Value};
use std::collections::{HashMap, HashSet, VecDeque};
use std::io::{self, Write};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

pub type PendingUiRequests =
    Arc<Mutex<HashMap<String, tokio::sync::oneshot::Sender<UiDialogAnswer>>>>;
pub(super) type ResumeTx = tokio::sync::mpsc::UnboundedSender<(
    QueuedPrompt,
    tokio::sync::watch::Receiver<bool>,
    u64,
)>;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum QueueMode {
    OneAtATime,
    All,
}

impl QueueMode {
    pub(super) fn as_str(self) -> &'static str {
        match self {
            QueueMode::OneAtATime => "one-at-a-time",
            QueueMode::All => "all",
        }
    }

    pub(super) fn parse(s: &str) -> Option<Self> {
        match s {
            "one-at-a-time" => Some(QueueMode::OneAtATime),
            "all" => Some(QueueMode::All),
            _ => None,
        }
    }
}

#[derive(Debug, Clone)]
pub(super) struct CustomWakeupMeta {
    pub(super) custom_type: String,
    pub(super) summary: String,
    pub(super) background_task_id: Option<u32>,
}

#[derive(Debug, Clone)]
pub(super) struct QueuedPrompt {
    pub(super) message: String,
    pub(super) images: Vec<(String, String)>,
    pub(super) custom_notice: Option<CustomWakeupMeta>,
}

impl QueuedPrompt {
    pub(super) fn user(message: String, images: Vec<(String, String)>) -> Self {
        Self {
            message,
            images,
            custom_notice: None,
        }
    }

    pub(super) fn wakeup(
        message: String,
        custom_type: String,
        summary: String,
        background_task_id: Option<u32>,
    ) -> Self {
        Self {
            message,
            images: Vec::new(),
            custom_notice: Some(CustomWakeupMeta {
                custom_type,
                summary,
                background_task_id,
            }),
        }
    }

    pub(super) fn background_task_id(&self) -> Option<u32> {
        self.custom_notice
            .as_ref()
            .and_then(|meta| meta.background_task_id)
    }

    pub(super) fn queue_display_text(&self) -> String {
        match &self.custom_notice {
            Some(meta) => meta.summary.clone(),
            None => self.message.clone(),
        }
    }
}

struct PendingUiRequestGuard {
    pending: PendingUiRequests,
    req_id: String,
    active: bool,
}

impl Drop for PendingUiRequestGuard {
    fn drop(&mut self) {
        if self.active {
            if let Ok(mut map) = self.pending.lock() {
                map.remove(&self.req_id);
            }
        }
    }
}

pub(super) struct ResumeTxSlotGuard {
    pub(super) slot: Arc<Mutex<Option<ResumeTx>>>,
}

impl Drop for ResumeTxSlotGuard {
    fn drop(&mut self) {
        if let Ok(mut lock) = self.slot.lock() {
            *lock = None;
        }
    }
}

pub(super) enum ActiveOp {
    Idle,
    Streaming {
        cancel_tx: tokio::sync::watch::Sender<bool>,
    },
    Compacting {
        cancel_tx: tokio::sync::watch::Sender<bool>,
    },
}

pub(super) struct RpcMutableState {
    pub(super) session_file: PathBuf,
    pub(super) session_id: String,
    pub(super) session_name: Option<String>,
    pub(super) active_provider: String,
    pub(super) active_model: String,
    pub(super) thinking_level: String,
    pub(super) providers: HashMap<String, ProviderEndpoint>,
    pub(super) catalog: ModelCatalog,
    pub(super) active_op: ActiveOp,
    pub(super) current_op_seq: u64,
    pub(super) next_op_seq: u64,
    pub(super) steering_mode: QueueMode,
    pub(super) follow_up_mode: QueueMode,
    /// Antrean steer yang diketik user (ditampilkan di kartu `STEERING` Android).
    pub(super) steering_queue: VecDeque<QueuedPrompt>,
    /// Antrean follow-up yang diketik user (ditampilkan di kartu `STEERING` Android).
    pub(super) follow_up_queue: VecDeque<QueuedPrompt>,
    /// Antrean internal auto-wakeup sistem (mis. background task selesai).
    /// KONTRAK: TIDAK BOLEH pernah dibocorkan ke `queue_update` atau `clear_queue`!
    pub(super) wakeup_queue: VecDeque<QueuedPrompt>,
    /// ID task background yang wakeup-nya sudah dibatalkan (diobservasi lewat `output`/`wait`/`list`,
    /// atau dibatalkan lewat `kill`/`clear`/`shutdown_all`/`abort`) agar race callback tetap terfilter.
    pub(super) dismissed_bg_wakeups: HashSet<u32>,
    pub(super) resume_queues_after_compact: bool,
    pub(super) auto_compaction_enabled: bool,
    pub(super) auto_retry_enabled: bool,
    pub(super) retry_cancel_tx: Option<tokio::sync::watch::Sender<bool>>,
}

impl RpcMutableState {
    /// Snapshot kanonis antrean user (`steering`, `followUp`) untuk frame `queue_update`.
    /// Gate struktural: antrean user tidak boleh pernah berisi wakeup sistem (`custom_notice.is_some()`).
    pub(super) fn user_queue_snapshot(&self) -> (Vec<String>, Vec<String>) {
        debug_assert!(
            self.steering_queue
                .iter()
                .all(|q| q.custom_notice.is_none()),
            "steering_queue tidak boleh berisi system wakeup"
        );
        debug_assert!(
            self.follow_up_queue
                .iter()
                .all(|q| q.custom_notice.is_none()),
            "follow_up_queue tidak boleh berisi system wakeup"
        );
        let steering = self
            .steering_queue
            .iter()
            .filter(|q| q.custom_notice.is_none())
            .map(QueuedPrompt::queue_display_text)
            .collect();
        let follow_up = self
            .follow_up_queue
            .iter()
            .filter(|q| q.custom_notice.is_none())
            .map(QueuedPrompt::queue_display_text)
            .collect();
        (steering, follow_up)
    }
}

pub(super) struct CoordinatorFinishGuard {
    pub(super) rt: Arc<RpcRuntime>,
    pub(super) op_seq: u64,
    pub(super) armed: bool,
}

impl Drop for CoordinatorFinishGuard {
    fn drop(&mut self) {
        if self.armed {
            {
                let mut m = self.rt.mutable.lock().unwrap_or_else(|e| e.into_inner());
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
            self.rt.publish_completed_seq(self.op_seq);
        }
    }
}

pub struct RpcRuntime {
    pub cwd: PathBuf,
    pub config_dir: PathBuf,
    pub plugins: Arc<PluginRegistry>,
    pub agent: AgentHarness,
    pub ui_state: Arc<Mutex<RpcUiState>>,
    pub pending_approvals: PendingApprovals,
    pub pending_ui_requests: PendingUiRequests,
    pub(super) ui_req_counter: AtomicU64,
    pub(super) resume_tx_slot: Arc<Mutex<Option<ResumeTx>>>,
    pub(super) defer_settled: Arc<AtomicBool>,
    pub(super) mutable: Mutex<RpcMutableState>,
    pub(super) completed_seq_tx: tokio::sync::watch::Sender<u64>,
    pub(super) http_scan_lock: tokio::sync::Mutex<()>,
}

impl RpcRuntime {
    pub(super) fn new_in_dir_with_cwd(
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
        // Selalu aktifkan tool interaksi dua arah (`notify_user` & `ask_user`) pada mode RPC.
        plugins.activate("oopbot-interact");
        plugins.register_active_tools(&mut tools);
        if let Some(plugin) = plugins.get_plugin("oopbot-background") {
            if let Some(bg) = plugin
                .as_any()
                .and_then(|a| a.downcast_ref::<BackgroundPlugin>())
            {
                bg.set_status_notifier(Arc::new(|active| {
                    Self::send_channel("background", json!({ "active": active }));
                }));
            }
        }
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
            if let Some((pid, ep)) = providers
                .iter()
                .find(|(_, p)| p.has_auth && !p.models.is_empty())
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

        let client = LlmClient::new(api_key.clone(), active_model.clone())?;
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
        let pending_ui_requests: PendingUiRequests = Arc::new(Mutex::new(HashMap::new()));
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
        let mut agent = AgentHarness::with_event_sink_and_tabs(
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
            wakeup_queue: VecDeque::new(),
            dismissed_bg_wakeups: HashSet::new(),
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
            pending_ui_requests,
            ui_req_counter: AtomicU64::new(1),
            resume_tx_slot: Arc::new(Mutex::new(None)),
            defer_settled,
            mutable: Mutex::new(mutable),
            completed_seq_tx,
            http_scan_lock: tokio::sync::Mutex::new(()),
        };

        let clamped = rt
            .active_model_info()
            .clamp_thinking_level(&requested_thinking);
        {
            rt.mutable
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .thinking_level = clamped.clone();
        }
        rt.sync_agent_catalog();
        rt.sync_agent_active_model();
        rt.agent.set_thinking_level(&clamped);
        rt.ensure_session_header();
        Ok(rt)
    }

    pub(super) fn install_plugin_callbacks(self: &Arc<Self>) {
        if let Some(plugin) = self.plugins.get_plugin("oopbot-background") {
            if let Some(bg) = plugin
                .as_any()
                .and_then(|a| a.downcast_ref::<BackgroundPlugin>())
            {
                let weak_rt = Arc::downgrade(self);
                bg.set_completion_notifier(Arc::new(move |evt| {
                    if let Some(rt) = weak_rt.upgrade() {
                        rt.trigger_background_wakeup(evt);
                    }
                }));

                let weak_rt_dismiss = Arc::downgrade(self);
                bg.set_wakeup_dismiss_notifier(Arc::new(move |ids| {
                    if let Some(rt) = weak_rt_dismiss.upgrade() {
                        rt.dismiss_background_wakeups(ids);
                    }
                }));
            }
        }

        if let Some(plugin) = self.plugins.get_plugin("oopbot-interact") {
            if let Some(interact) = plugin
                .as_any()
                .and_then(|a| a.downcast_ref::<InteractPlugin>())
            {
                interact.set_notify_handler(Arc::new(|level, message| {
                    Self::send_line(&json!({
                        "type": "extension_ui_request",
                        "method": "notify",
                        "notifyType": level,
                        "message": message,
                    }));
                }));

                let weak_rt = Arc::downgrade(self);
                interact.set_ask_handler(Arc::new(move |req: UiDialogRequest| {
                    let weak_rt = weak_rt.clone();
                    Box::pin(async move {
                        let Some(rt) = weak_rt.upgrade() else {
                            return Err("Runtime RPC sudah dihentikan".to_string());
                        };
                        rt.request_ui_dialog(req).await
                    })
                }));
            }
        }
    }

    pub(super) async fn request_ui_dialog(
        &self,
        req: UiDialogRequest,
    ) -> Result<UiDialogAnswer, String> {
        let id_num = self.ui_req_counter.fetch_add(1, Ordering::SeqCst);
        let req_id = format!("ui-ask-{}", id_num);
        let (tx, rx) = tokio::sync::oneshot::channel::<UiDialogAnswer>();

        {
            let mut pending = self
                .pending_ui_requests
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            pending.insert(req_id.clone(), tx);
        }
        let mut guard = PendingUiRequestGuard {
            pending: Arc::clone(&self.pending_ui_requests),
            req_id: req_id.clone(),
            active: true,
        };

        let timeout_ms = req.timeout_secs.saturating_mul(1000);
        let payload = match &req.kind {
            UiDialogKind::Confirm { message } => {
                let mut obj = json!({
                    "type": "extension_ui_request",
                    "id": req_id,
                    "method": "confirm",
                    "title": req.title,
                    "timeout": timeout_ms,
                });
                if let Some(msg) = message {
                    obj["message"] = json!(msg);
                }
                obj
            }
            UiDialogKind::Select { options } => json!({
                "type": "extension_ui_request",
                "id": req_id,
                "method": "select",
                "title": req.title,
                "options": options,
                "timeout": timeout_ms,
            }),
            UiDialogKind::Input { placeholder } => {
                let mut obj = json!({
                    "type": "extension_ui_request",
                    "id": req_id,
                    "method": "input",
                    "title": req.title,
                    "timeout": timeout_ms,
                });
                if let Some(ph) = placeholder {
                    obj["placeholder"] = json!(ph);
                }
                obj
            }
            UiDialogKind::Editor { prefill } => {
                let mut obj = json!({
                    "type": "extension_ui_request",
                    "id": req_id,
                    "method": "editor",
                    "title": req.title,
                    "timeout": timeout_ms,
                });
                if let Some(pf) = prefill {
                    obj["prefill"] = json!(pf);
                }
                obj
            }
        };
        Self::send_line(&payload);

        let dur = std::time::Duration::from_secs(req.timeout_secs);
        match tokio::time::timeout(dur, rx).await {
            Ok(Ok(answer)) => {
                guard.active = false;
                Ok(answer)
            }
            Ok(Err(_)) => {
                guard.active = false;
                Err("Koneksi UI terputus saat menunggu jawaban user".to_string())
            }
            Err(_) => Ok(UiDialogAnswer::TimedOut),
        }
    }

    /// Publikasikan `op_seq` yang selesai secara monoton agar waiter `abort` tidak melewatkan sequence akibat interleaving.
    pub(super) fn publish_completed_seq(&self, seq: u64) {
        self.completed_seq_tx.send_if_modified(|cur| {
            if seq > *cur {
                *cur = seq;
                true
            } else {
                false
            }
        });
    }

    /// Akses thread-safe ke map approval yang sedang menunggu konfirmasi UI.
    pub fn pending_approvals(&self) -> PendingApprovals {
        Arc::clone(&self.pending_approvals)
    }

    /// Selesaikan permintaan approval tool dengan id tertentu.
    /// Mengembalikan `true` bila request id ditemukan dan notifikasi terkirim, `false` jika tidak.
    pub fn resolve_tool_approval(&self, req_id: &str, approved: bool) -> bool {
        if let Some(sender) = self
            .pending_approvals
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(req_id)
        {
            let _ = sender.send(approved);
            true
        } else {
            false
        }
    }

    /// Pancarkan event widget aktif (misal checklist Todo) dan status channel plugin ke antarmuka aplikasi Android via RPC JSONL.
    pub fn emit_active_widgets(&self) {
        for plugin in self.plugins.active_plugins() {
            if plugin.id() == "oopbot-todo" {
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
        if self.plugins.is_active("oopbot-subagents") {
            Self::send_channel("subagents", json!({ "runs": [], "standingAside": false }));
        }
    }

    /// Kirim pesan dua arah pada OOPBoT channel: `{"type": "oopbot_channel", "topic": ..., "data": ...}`
    pub fn send_channel(topic: &str, data: Value) {
        Self::send_line(&json!({
            "type": "oopbot_channel",
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

    pub(super) fn send_response(id: &str, command: &str, data: Value) {
        Self::send_line(&json!({
            "type": "response",
            "id": id,
            "command": command,
            "success": true,
            "data": data,
        }));
    }

    pub(super) fn send_error(id: &str, command: &str, error: &str) {
        Self::send_line(&json!({
            "type": "response",
            "id": id,
            "command": command,
            "success": false,
            "error": error,
        }));
    }

    pub(super) fn state_json(&self) -> Value {
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
            let m = self.mutable.lock().unwrap_or_else(|e| e.into_inner());
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
                model_info.to_oopbot_json(Some(&m.active_provider)),
            )
        };
        let message_count = self
            .ui_state
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .ui_messages
            .len();

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

    pub(super) fn append_session_entry(&self, entry_type: &str, payload: Value) -> String {
        self.ensure_session_header();
        self.ui_state
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .append_session_entry_returning_id(entry_type, payload)
    }
}
