//! Bridge penerjemah 2-arah antara protokol JSONL RPC OOPBoT (Android `AgentProcess.kt` / `ChatTimeline.kt`)
//! dan subprocess agen eksternal berbasis Agent Client Protocol (ACP JSON-RPC 2.0).

use super::client::{AcpAgentEvent, AcpClient, AcpLaunchConfig};
use super::protocol::{
    build_initialize_params, build_permission_cancelled_result, build_permission_selected_result,
    build_session_cancel_params, build_session_load_params, build_session_new_params,
    build_session_prompt_params, map_acp_tool_kind_to_oopbot, parse_permission_request,
    parse_session_update, AcpPermissionRequest, AcpPlanEntry, AcpSessionUpdate, JsonRpcError,
};
use crate::common::time::{epoch_ms, iso_now};
use crate::common::BoxError;
use serde_json::{json, Value};
use std::collections::{HashMap, VecDeque};
use std::io::{BufRead, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::sync::{mpsc, oneshot};

static ACP_SESSION_SEQ: AtomicU64 = AtomicU64::new(1);
const ACP_INIT_TIMEOUT_SECS: u64 = 15;

/// Konfigurasi sesi untuk `AcpRpcBridge`.
#[derive(Debug, Clone)]
pub struct AcpBridgeConfig {
    pub engine_id: String,
    pub display_name: String,
    pub config_dir: PathBuf,
    pub cwd: PathBuf,
    pub session_path: Option<PathBuf>,
    pub launch: AcpLaunchConfig,
}

struct PendingPermission {
    rpc_id: Value,
    request: AcpPermissionRequest,
}

struct StreamingTurnState {
    started_ms: u64,
    assistant_msg_started: bool,
    content_blocks: Vec<Value>,
    current_content_idx: usize,
    open_block_kind: Option<&'static str>, // "thinking" | "text"
    open_block_text: String,
    active_tools: HashMap<String, (String, Value)>, // tool_call_id -> (ui_tool_name, args)
    aborted: bool,
}

impl StreamingTurnState {
    fn new(started_ms: u64) -> Self {
        Self {
            started_ms,
            assistant_msg_started: false,
            content_blocks: Vec::new(),
            current_content_idx: 0,
            open_block_kind: None,
            open_block_text: String::new(),
            active_tools: HashMap::new(),
            aborted: false,
        }
    }
}

/// Mirror ringan file sesi JSONL OOPBoT agar chat ACP tetap terbaca offline di Archive Android
/// dan `acpSessionId` dapat dipulihkan lewat `session/load` saat reconnect.
struct AcpSessionMirror {
    session_file: PathBuf,
    session_id: String,
    acp_session_id: Option<String>,
    engine_tag: String,
    cwd: PathBuf,
    ui_messages: Vec<Value>,
    entry_seq: usize,
    leaf_id: Option<String>,
}

impl AcpSessionMirror {
    fn open_or_new(
        config_dir: &Path,
        cwd: &Path,
        engine_id: &str,
        session_path: Option<PathBuf>,
    ) -> Self {
        let engine_tag = format!("acp:{}", engine_id);
        let seq = ACP_SESSION_SEQ.fetch_add(1, Ordering::Relaxed);
        let default_sid = format!("oopbot-acp-{}-{}", epoch_ms(), seq);
        let session_file = session_path.unwrap_or_else(|| {
            let slug = format!(
                "--{}--",
                cwd.to_string_lossy()
                    .trim_start_matches('/')
                    .replace(['/', '\\', ':'], "-")
            );
            let dir = config_dir.join("sessions").join(slug);
            let _ = std::fs::create_dir_all(&dir);
            let ts = iso_now().replace([':', '.'], "-");
            dir.join(format!("{}_{}.jsonl", ts, default_sid))
        });

        let mut session_id = default_sid;
        let mut acp_session_id: Option<String> = None;
        let mut ui_messages = Vec::new();
        let mut entry_seq = 0usize;
        let mut leaf_id: Option<String> = None;

        if let Ok(content) = std::fs::read_to_string(&session_file) {
            for line in content.lines() {
                let trimmed = line.trim();
                if trimmed.is_empty() {
                    continue;
                }
                let Ok(entry) = serde_json::from_str::<Value>(trimmed) else {
                    continue;
                };
                match entry["type"].as_str().unwrap_or_default() {
                    "session" => {
                        if let Some(id) = entry["id"].as_str().filter(|s| !s.is_empty()) {
                            session_id = id.to_string();
                        }
                        if let Some(aid) = entry["acpSessionId"].as_str().filter(|s| !s.is_empty())
                        {
                            acp_session_id = Some(aid.to_string());
                        }
                    }
                    "acp_session" => {
                        if let Some(aid) = entry["acpSessionId"].as_str().filter(|s| !s.is_empty())
                        {
                            acp_session_id = Some(aid.to_string());
                        }
                        if let Some(eid) = entry["id"].as_str().filter(|s| !s.is_empty()) {
                            leaf_id = Some(eid.to_string());
                            entry_seq += 1;
                        }
                    }
                    "message" => {
                        if let Some(msg) = entry.get("message").filter(|m| !m.is_null()) {
                            ui_messages.push(msg.clone());
                        }
                        if let Some(eid) = entry["id"].as_str().filter(|s| !s.is_empty()) {
                            leaf_id = Some(eid.to_string());
                            entry_seq += 1;
                        }
                    }
                    _ => {
                        if let Some(eid) = entry["id"].as_str().filter(|s| !s.is_empty()) {
                            leaf_id = Some(eid.to_string());
                            entry_seq += 1;
                        }
                    }
                }
            }
        }

        Self {
            session_file,
            session_id,
            acp_session_id,
            engine_tag,
            cwd: cwd.to_path_buf(),
            ui_messages,
            entry_seq,
            leaf_id,
        }
    }

    fn ensure_header(&self) {
        if self.session_file.exists() {
            return;
        }
        if let Some(parent) = self.session_file.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        let mut header = json!({
            "type": "session",
            "version": 3,
            "id": self.session_id,
            "engine": self.engine_tag,
            "timestamp": iso_now(),
            "cwd": self.cwd.to_string_lossy(),
        });
        if let Some(ref aid) = self.acp_session_id {
            header["acpSessionId"] = json!(aid);
        }
        if let Ok(mut line) = serde_json::to_string(&header) {
            line.push('\n');
            let _ = std::fs::write(&self.session_file, line);
        }
    }

    fn append_entry(&mut self, entry_type: &str, mut payload: Value) {
        self.ensure_header();
        let eid = format!("e{}", self.entry_seq);
        self.entry_seq += 1;
        payload["type"] = json!(entry_type);
        payload["id"] = json!(eid.clone());
        payload["parentId"] = self
            .leaf_id
            .as_ref()
            .map(|p| json!(p))
            .unwrap_or(Value::Null);
        payload["timestamp"] = json!(iso_now());
        self.leaf_id = Some(eid);

        if let Ok(mut line) = serde_json::to_string(&payload) {
            line.push('\n');
            if let Ok(mut file) = std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(&self.session_file)
            {
                let _ = file.write_all(line.as_bytes());
                let _ = file.flush();
            }
        }
    }

    fn record_acp_session_id(&mut self, acp_sid: String) {
        let changed = self.acp_session_id.as_deref() != Some(acp_sid.as_str());
        self.acp_session_id = Some(acp_sid.clone());
        if changed {
            self.append_entry(
                "acp_session",
                json!({
                    "engine": self.engine_tag,
                    "acpSessionId": acp_sid,
                }),
            );
        }
    }

    fn record_message(&mut self, msg: Value) {
        self.ui_messages.push(msg.clone());
        self.append_entry("message", json!({ "message": msg }));
    }
}

/// Bridge ACP <-> OOPBoT JSONL RPC.
pub struct AcpRpcBridge {
    config: AcpBridgeConfig,
    client: AcpClient,
    acp_events: mpsc::UnboundedReceiver<AcpAgentEvent>,
    mirror: AcpSessionMirror,
    active_model_id: String,
    active_model_name: String,
    acp_session_id: String,
    pending_permissions: HashMap<String, PendingPermission>,
    perm_counter: u64,
    queued_prompts: VecDeque<(String, Vec<(String, String)>)>,
    output: Arc<Mutex<Box<dyn Write + Send + Sync>>>,
}

impl AcpRpcBridge {
    /// Luncurkan subprocess ACP, lakukan handshake `initialize` + `session/new` (atau `session/load`),
    /// dan siapkan bridge siap melayani loop JSONL OOPBoT.
    pub async fn start(
        config: AcpBridgeConfig,
        output: Arc<Mutex<Box<dyn Write + Send + Sync>>>,
    ) -> Result<Self, BoxError> {
        let mut mirror = AcpSessionMirror::open_or_new(
            &config.config_dir,
            &config.cwd,
            &config.engine_id,
            config.session_path.clone(),
        );
        let (client, acp_events) = AcpClient::spawn(config.launch.clone())?;

        // 1. Handshake `initialize`
        let init_res = client
            .request_with_timeout(
                "initialize",
                build_initialize_params("oopbot", env!("CARGO_PKG_VERSION")),
                Duration::from_secs(ACP_INIT_TIMEOUT_SECS),
            )
            .await?;

        let supports_load = init_res["agentCapabilities"]["loadSession"]
            .as_bool()
            .unwrap_or(false);

        let cwd_str = config.cwd.to_string_lossy().to_string();

        // 2. Pulihkan sesi ACP lama bila tersedia dan didukung; jika gagal fallback ke `session/new`.
        let session_res = if supports_load {
            if let Some(prev_sid) = mirror.acp_session_id.clone() {
                match client
                    .request_with_timeout(
                        "session/load",
                        build_session_load_params(&prev_sid, &cwd_str),
                        Duration::from_secs(ACP_INIT_TIMEOUT_SECS),
                    )
                    .await
                {
                    Ok(mut v) => {
                        if v.get("sessionId").and_then(|s| s.as_str()).is_none() {
                            v["sessionId"] = json!(prev_sid);
                        }
                        v
                    }
                    Err(_) => {
                        client
                            .request_with_timeout(
                                "session/new",
                                build_session_new_params(&cwd_str),
                                Duration::from_secs(ACP_INIT_TIMEOUT_SECS),
                            )
                            .await?
                    }
                }
            } else {
                client
                    .request_with_timeout(
                        "session/new",
                        build_session_new_params(&cwd_str),
                        Duration::from_secs(ACP_INIT_TIMEOUT_SECS),
                    )
                    .await?
            }
        } else {
            client
                .request_with_timeout(
                    "session/new",
                    build_session_new_params(&cwd_str),
                    Duration::from_secs(ACP_INIT_TIMEOUT_SECS),
                )
                .await?
        };

        let acp_session_id = session_res["sessionId"]
            .as_str()
            .filter(|s| !s.is_empty())
            .map(str::to_string)
            .unwrap_or_else(|| format!("acp-{}", epoch_ms()));
        mirror.record_acp_session_id(acp_session_id.clone());

        let active_model_id = session_res["models"]["currentModelId"]
            .as_str()
            .or_else(|| session_res["model"].as_str())
            .unwrap_or("default")
            .to_string();
        let active_model_name = session_res["models"]["availableModels"]
            .as_array()
            .and_then(|arr| {
                arr.iter().find(|m| {
                    m["modelId"].as_str() == Some(active_model_id.as_str())
                        || m["id"].as_str() == Some(active_model_id.as_str())
                })
            })
            .and_then(|m| m["name"].as_str())
            .map(str::to_string)
            .unwrap_or_else(|| format!("{} ({})", config.display_name, active_model_id));

        Ok(Self {
            config,
            client,
            acp_events,
            mirror,
            active_model_id,
            active_model_name,
            acp_session_id,
            pending_permissions: HashMap::new(),
            perm_counter: 1,
            queued_prompts: VecDeque::new(),
            output,
        })
    }

    fn send_line(&self, val: &Value) {
        if let Ok(mut w) = self.output.lock() {
            if let Ok(line) = serde_json::to_string(val) {
                let _ = writeln!(w, "{}", line);
                let _ = w.flush();
            }
        }
    }

    fn send_response(&self, id: &str, command: &str, data: Value) {
        self.send_line(&json!({
            "type": "response",
            "id": id,
            "command": command,
            "success": true,
            "data": data,
        }));
    }

    fn send_error(&self, id: &str, command: &str, error: &str) {
        self.send_line(&json!({
            "type": "response",
            "id": id,
            "command": command,
            "success": false,
            "error": error,
        }));
    }

    fn send_channel(&self, topic: &str, data: Value) {
        self.send_line(&json!({
            "type": "oopbot_channel",
            "topic": topic,
            "data": data,
        }));
    }

    fn state_json(&self, is_streaming: bool) -> Value {
        let provider_id = format!("acp:{}", self.config.engine_id);
        json!({
            "model": {
                "id": self.active_model_id,
                "name": self.active_model_name,
                "provider": provider_id,
                "api": "acp",
                "reasoning": true,
                "input": ["text", "image"],
                "contextWindow": 200000,
                "maxTokens": 32768,
            },
            "thinkingLevel": "medium",
            "isStreaming": is_streaming,
            "isCompacting": false,
            "steeringMode": "one-at-a-time",
            "followUpMode": "one-at-a-time",
            "sessionFile": self.mirror.session_file.to_string_lossy(),
            "sessionId": self.mirror.session_id,
            "sessionName": Value::Null,
            "autoCompactionEnabled": false,
            "autoRetryEnabled": false,
            "messageCount": self.mirror.ui_messages.len(),
            "pendingMessageCount": self.queued_prompts.len(),
        })
    }

    fn close_open_block(&self, turn: &mut StreamingTurnState) {
        let Some(kind) = turn.open_block_kind.take() else {
            return;
        };
        let text = std::mem::take(&mut turn.open_block_text);
        let idx = turn.current_content_idx;
        match kind {
            "thinking" => {
                self.send_line(&json!({
                    "type": "message_update",
                    "assistantMessageEvent": {
                        "type": "thinking_end",
                        "contentIndex": idx,
                        "content": text,
                    }
                }));
                if !text.trim().is_empty() {
                    turn.content_blocks.push(json!({
                        "type": "thinking",
                        "thinking": text,
                    }));
                }
            }
            "text" => {
                self.send_line(&json!({
                    "type": "message_update",
                    "assistantMessageEvent": {
                        "type": "text_end",
                        "contentIndex": idx,
                        "content": text,
                    }
                }));
                if !text.is_empty() {
                    turn.content_blocks.push(json!({
                        "type": "text",
                        "text": text,
                    }));
                }
            }
            _ => {}
        }
        turn.current_content_idx += 1;
    }

    fn ensure_assistant_started(&self, turn: &mut StreamingTurnState) {
        if !turn.assistant_msg_started {
            self.send_line(&json!({
                "type": "message_start",
                "message": {
                    "role": "assistant",
                    "content": [],
                    "timestamp": turn.started_ms,
                }
            }));
            turn.assistant_msg_started = true;
        }
    }

    fn flush_assistant_message(&mut self, turn: &mut StreamingTurnState, stop_reason: &str) {
        self.close_open_block(turn);
        if !turn.assistant_msg_started {
            return;
        }
        let assistant_msg = json!({
            "role": "assistant",
            "content": turn.content_blocks.clone(),
            "stopReason": stop_reason,
            "timestamp": turn.started_ms,
            "durationMs": epoch_ms().saturating_sub(turn.started_ms),
        });
        self.send_line(&json!({
            "type": "message_end",
            "message": assistant_msg,
        }));
        self.mirror.record_message(assistant_msg);
        turn.assistant_msg_started = false;
        turn.content_blocks.clear();
        turn.current_content_idx = 0;
    }

    fn emit_plan_widget(&self, entries: &[AcpPlanEntry]) {
        if entries.is_empty() {
            self.send_line(&json!({
                "type": "extension_ui_request",
                "id": "todos-widget",
                "method": "setWidget",
                "widgetKey": "todos",
                "widgetPlacement": "aboveEditor",
            }));
            return;
        }
        let done_count = entries
            .iter()
            .filter(|e| e.status.eq_ignore_ascii_case("completed") || e.status == "done")
            .count();
        let mut lines = vec![format!("● Plan ({}/{})", done_count, entries.len())];
        for (idx, entry) in entries.iter().enumerate() {
            let icon = match entry.status.to_ascii_lowercase().as_str() {
                "completed" | "done" => "✓",
                "in_progress" | "running" => "◐",
                _ => "○",
            };
            let branch = if idx + 1 == entries.len() {
                "└─"
            } else {
                "├─"
            };
            lines.push(format!("{} {} {}", branch, icon, entry.content));
        }
        self.send_line(&json!({
            "type": "extension_ui_request",
            "id": "todos-widget",
            "method": "setWidget",
            "widgetKey": "todos",
            "widgetLines": lines,
            "widgetPlacement": "aboveEditor",
        }));
    }

    fn handle_session_update(&mut self, params: &Value, active_turn: &mut Option<StreamingTurnState>) {
        let update = parse_session_update(params);
        match update {
            AcpSessionUpdate::Plan { entries } => {
                self.emit_plan_widget(&entries);
            }
            AcpSessionUpdate::AgentThoughtChunk { text } => {
                if text.is_empty() {
                    return;
                }
                let Some(turn) = active_turn.as_mut() else {
                    return;
                };
                self.ensure_assistant_started(turn);
                if turn.open_block_kind != Some("thinking") {
                    self.close_open_block(turn);
                    let idx = turn.current_content_idx;
                    self.send_line(&json!({
                        "type": "message_update",
                        "assistantMessageEvent": {
                            "type": "thinking_start",
                            "contentIndex": idx,
                        }
                    }));
                    turn.open_block_kind = Some("thinking");
                }
                let idx = turn.current_content_idx;
                turn.open_block_text.push_str(&text);
                self.send_line(&json!({
                    "type": "message_update",
                    "assistantMessageEvent": {
                        "type": "thinking_delta",
                        "contentIndex": idx,
                        "delta": text,
                    }
                }));
            }
            AcpSessionUpdate::AgentMessageChunk { text } => {
                if text.is_empty() {
                    return;
                }
                let Some(turn) = active_turn.as_mut() else {
                    return;
                };
                self.ensure_assistant_started(turn);
                if turn.open_block_kind != Some("text") {
                    self.close_open_block(turn);
                    let idx = turn.current_content_idx;
                    self.send_line(&json!({
                        "type": "message_update",
                        "assistantMessageEvent": {
                            "type": "text_start",
                            "contentIndex": idx,
                        }
                    }));
                    turn.open_block_kind = Some("text");
                }
                let idx = turn.current_content_idx;
                turn.open_block_text.push_str(&text);
                self.send_line(&json!({
                    "type": "message_update",
                    "assistantMessageEvent": {
                        "type": "text_delta",
                        "contentIndex": idx,
                        "delta": text,
                    }
                }));
            }
            AcpSessionUpdate::ToolCall {
                tool_call_id,
                title,
                kind,
                raw_input,
            } => {
                let Some(turn) = active_turn.as_mut() else {
                    return;
                };
                self.ensure_assistant_started(turn);
                self.close_open_block(turn);

                let ui_tool_name = map_acp_tool_kind_to_oopbot(&kind).to_string();
                let mut ui_args = raw_input.clone();
                if ui_args.is_null() || ui_args.as_object().is_some_and(|m| m.is_empty()) {
                    ui_args = json!({ "title": title });
                }

                let idx = turn.current_content_idx;
                self.send_line(&json!({
                    "type": "message_update",
                    "assistantMessageEvent": {
                        "type": "toolcall_start",
                        "contentIndex": idx,
                        "id": tool_call_id,
                        "toolName": ui_tool_name,
                    }
                }));
                self.send_line(&json!({
                    "type": "message_update",
                    "assistantMessageEvent": {
                        "type": "toolcall_end",
                        "contentIndex": idx,
                        "toolCall": {
                            "id": tool_call_id,
                            "name": ui_tool_name,
                            "arguments": ui_args,
                        }
                    }
                }));
                turn.content_blocks.push(json!({
                    "type": "toolCall",
                    "id": tool_call_id,
                    "name": ui_tool_name,
                    "arguments": ui_args,
                }));
                turn.current_content_idx += 1;
                turn.active_tools
                    .insert(tool_call_id.clone(), (ui_tool_name.clone(), ui_args.clone()));

                self.send_line(&json!({
                    "type": "tool_execution_start",
                    "toolCallId": tool_call_id,
                    "toolName": ui_tool_name,
                    "args": ui_args,
                }));
            }
            AcpSessionUpdate::ToolCallUpdate {
                tool_call_id,
                status,
                title: _,
                output_text,
                is_error,
            } => {
                let is_terminal = matches!(
                    status.to_ascii_lowercase().as_str(),
                    "completed" | "failed" | "error" | "rejected" | "cancelled"
                );
                if !is_terminal {
                    return;
                }
                let Some(turn) = active_turn.as_mut() else {
                    return;
                };
                if turn.assistant_msg_started {
                    self.flush_assistant_message(turn, "toolUse");
                }
                let (ui_tool_name, _) = turn
                    .active_tools
                    .remove(&tool_call_id)
                    .unwrap_or_else(|| ("tool".to_string(), json!({})));

                let result_payload = json!({
                    "content": [{ "type": "text", "text": output_text }]
                });
                self.send_line(&json!({
                    "type": "tool_execution_end",
                    "toolCallId": tool_call_id,
                    "toolName": ui_tool_name,
                    "isError": is_error,
                    "result": result_payload,
                }));

                let tool_result_msg = json!({
                    "role": "toolResult",
                    "toolCallId": tool_call_id,
                    "toolName": ui_tool_name,
                    "isError": is_error,
                    "content": [{ "type": "text", "text": output_text }],
                    "timestamp": epoch_ms(),
                });
                self.send_line(&json!({
                    "type": "message_end",
                    "message": tool_result_msg,
                }));
                self.mirror.record_message(tool_result_msg);
            }
            AcpSessionUpdate::Unknown(_) => {}
        }
    }

    async fn handle_agent_request(&mut self, id: Value, method: &str, params: &Value) {
        if method == "session/request_permission" {
            let perm = parse_permission_request(params);
            let req_id = format!("acp-perm-{}", self.perm_counter);
            self.perm_counter += 1;

            let ui_tool_name = map_acp_tool_kind_to_oopbot(&perm.kind);
            let options_json: Vec<Value> = perm
                .options
                .iter()
                .map(|o| {
                    json!({
                        "optionId": o.option_id,
                        "name": o.name,
                        "kind": o.kind,
                    })
                })
                .collect();

            self.send_channel(
                "tool_approval",
                json!({
                    "requestId": req_id,
                    "toolCallId": perm.tool_call_id,
                    "toolName": ui_tool_name,
                    "preview": perm.title,
                    "args": perm.raw_input,
                    "options": options_json,
                }),
            );

            self.pending_permissions.insert(
                req_id,
                PendingPermission {
                    rpc_id: id,
                    request: perm,
                },
            );
        } else {
            let _ = self
                .client
                .respond_error(id, -32601, &format!("Method '{}' tidak didukung", method))
                .await;
        }
    }

    async fn resolve_permission(&mut self, req_id: &str, approved: bool, explicit_option: Option<&str>) {
        let Some(pending) = self.pending_permissions.remove(req_id) else {
            return;
        };
        let chosen = explicit_option
            .filter(|s| !s.is_empty())
            .or_else(|| pending.request.pick_option_id(approved));
        let result = match chosen {
            Some(opt_id) => build_permission_selected_result(opt_id),
            None => build_permission_cancelled_result(),
        };
        let _ = self.client.respond_result(pending.rpc_id, result).await;
    }

    async fn cancel_all_pending_permissions(&mut self) {
        let drained: Vec<_> = self.pending_permissions.drain().map(|(_, p)| p).collect();
        for pending in drained {
            let _ = self
                .client
                .respond_result(pending.rpc_id, build_permission_cancelled_result())
                .await;
        }
    }

    async fn start_prompt_turn(
        &mut self,
        message: String,
        images: Vec<(String, String)>,
    ) -> Result<
        (
            StreamingTurnState,
            oneshot::Receiver<Result<Value, JsonRpcError>>,
        ),
        BoxError,
    > {
        let started_ms = epoch_ms();
        let mut user_blocks = Vec::new();
        if !message.is_empty() {
            user_blocks.push(json!({ "type": "text", "text": message }));
        }
        for (mime, data) in &images {
            user_blocks.push(json!({
                "type": "image",
                "mimeType": mime,
                "data": data,
            }));
        }
        if user_blocks.is_empty() {
            user_blocks.push(json!({ "type": "text", "text": "" }));
        }
        let user_msg = json!({
            "role": "user",
            "content": user_blocks,
            "timestamp": started_ms,
        });
        self.send_line(&json!({ "type": "message_start", "message": user_msg }));
        self.send_line(&json!({ "type": "message_end", "message": user_msg }));
        self.mirror.record_message(user_msg);

        self.send_line(&json!({ "type": "agent_start" }));

        let rx = self
            .client
            .send_request(
                "session/prompt",
                build_session_prompt_params(&self.acp_session_id, &message, &images),
            )
            .await?;

        Ok((StreamingTurnState::new(started_ms), rx))
    }

    fn finish_turn(
        &mut self,
        mut turn: StreamingTurnState,
        outcome: Result<Value, JsonRpcError>,
    ) {
        match outcome {
            Ok(res) => {
                let stop_reason_raw = res["stopReason"].as_str().unwrap_or("end_turn");
                let aborted = turn.aborted
                    || stop_reason_raw.eq_ignore_ascii_case("cancelled")
                    || stop_reason_raw.eq_ignore_ascii_case("aborted");
                if turn.assistant_msg_started {
                    let sr = if aborted { "aborted" } else { "stop" };
                    self.flush_assistant_message(&mut turn, sr);
                } else if aborted {
                    let abort_msg = json!({
                        "role": "assistant",
                        "content": [],
                        "stopReason": "aborted",
                        "timestamp": turn.started_ms,
                        "durationMs": epoch_ms().saturating_sub(turn.started_ms),
                    });
                    self.send_line(&json!({ "type": "message_start", "message": abort_msg }));
                    self.send_line(&json!({ "type": "message_end", "message": abort_msg }));
                    self.mirror.record_message(abort_msg);
                }
                self.send_line(&json!({ "type": "turn_end" }));
                self.send_line(&json!({ "type": "agent_end" }));
                self.send_line(&json!({ "type": "agent_settled", "aborted": aborted }));
            }
            Err(err) => {
                let aborted = turn.aborted;
                if turn.assistant_msg_started {
                    self.flush_assistant_message(&mut turn, if aborted { "aborted" } else { "error" });
                }
                if !aborted {
                    let err_msg = json!({
                        "role": "assistant",
                        "content": [],
                        "stopReason": "error",
                        "errorMessage": err.to_string(),
                        "timestamp": turn.started_ms,
                        "durationMs": epoch_ms().saturating_sub(turn.started_ms),
                    });
                    self.send_line(&json!({ "type": "message_start", "message": err_msg }));
                    self.send_line(&json!({ "type": "message_end", "message": err_msg }));
                    self.mirror.record_message(err_msg);
                }
                self.send_line(&json!({ "type": "turn_end" }));
                self.send_line(&json!({ "type": "agent_end" }));
                self.send_line(&json!({ "type": "agent_settled", "aborted": aborted }));
            }
        }
    }

    /// Jalankan loop utama yang melayani command JSONL Android secara konkuren dengan event ACP.
    pub async fn run_loop_with_input<R: BufRead + Send + 'static>(
        mut self,
        reader: R,
    ) -> Result<(), BoxError> {
        // Handshake `oopbot:hello` agar Android AgentProcess mengenali engine aktif
        self.send_channel(
            "oopbot:hello",
            json!({
                "engine": format!("acp:{}", self.config.engine_id),
                "oopbotVersion": env!("CARGO_PKG_VERSION"),
                "version": 1,
            }),
        );

        let (cmd_tx, mut cmd_rx) = mpsc::unbounded_channel::<String>();
        std::thread::spawn(move || {
            let mut r = reader;
            let mut line = String::new();
            loop {
                line.clear();
                match r.read_line(&mut line) {
                    Ok(0) | Err(_) => break,
                    Ok(_) => {
                        let trimmed = line.trim().to_string();
                        if !trimmed.is_empty() && cmd_tx.send(trimmed).is_err() {
                            break;
                        }
                    }
                }
            }
        });

        let mut active_turn: Option<StreamingTurnState> = None;
        let mut active_prompt_rx: Option<oneshot::Receiver<Result<Value, JsonRpcError>>> = None;

        loop {
            tokio::select! {
                biased;

                // 1. Event dari agen ACP (`session/update`, `session/request_permission`, `Closed`)
                maybe_ev = self.acp_events.recv() => {
                    match maybe_ev {
                        Some(AcpAgentEvent::Notification { method, params }) => {
                            if method == "session/update" {
                                self.handle_session_update(&params, &mut active_turn);
                            }
                        }
                        Some(AcpAgentEvent::Request { id, method, params }) => {
                            self.handle_agent_request(id, &method, &params).await;
                        }
                        Some(AcpAgentEvent::Closed { stderr_tail }) => {
                            self.cancel_all_pending_permissions().await;
                            if let Some(turn) = active_turn.take() {
                                let msg = if stderr_tail.is_empty() {
                                    "Proses agen ACP berhenti tak terduga".to_string()
                                } else {
                                    format!("Proses agen ACP berhenti: {}", stderr_tail.join(" | "))
                                };
                                self.finish_turn(turn, Err(JsonRpcError { code: -32000, message: msg, data: None }));
                            }
                            break;
                        }
                        None => break,
                    }
                }

                // 2. Penyelesaian `session/prompt` yang sedang berjalan
                prompt_res = async {
                    match active_prompt_rx.as_mut() {
                        Some(rx) => rx.await,
                        None => std::future::pending().await,
                    }
                }, if active_prompt_rx.is_some() => {
                    active_prompt_rx = None;
                    if let Some(turn) = active_turn.take() {
                        let outcome = match prompt_res {
                            Ok(res) => res,
                            Err(_) => Err(JsonRpcError {
                                code: -32000,
                                message: "Channel prompt ACP ditutup".to_string(),
                                data: None,
                            }),
                        };
                        self.finish_turn(turn, outcome);
                    }
                    // Proses antrean prompt berikutnya jika ada
                    if let Some((next_msg, next_imgs)) = self.queued_prompts.pop_front() {
                        match self.start_prompt_turn(next_msg, next_imgs).await {
                            Ok((turn, rx)) => {
                                active_turn = Some(turn);
                                active_prompt_rx = Some(rx);
                            }
                            Err(e) => {
                                let err_turn = StreamingTurnState::new(epoch_ms());
                                self.finish_turn(err_turn, Err(JsonRpcError {
                                    code: -32000,
                                    message: e.to_string(),
                                    data: None,
                                }));
                            }
                        }
                    }
                }

                // 3. Command JSONL masuk dari Android UI
                maybe_line = cmd_rx.recv() => {
                    let Some(line) = maybe_line else {
                        break;
                    };
                    let Ok(cmd) = serde_json::from_str::<Value>(&line) else {
                        continue;
                    };
                    let cmd_type = cmd["type"].as_str().unwrap_or_default();
                    let id = cmd["id"].as_str().unwrap_or_default().to_string();

                    match cmd_type {
                        "get_state" => {
                            let st = self.state_json(active_turn.is_some());
                            self.send_response(&id, "get_state", st);
                        }
                        "get_messages" => {
                            self.send_response(
                                &id,
                                "get_messages",
                                json!({ "messages": self.mirror.ui_messages }),
                            );
                        }
                        "get_commands" => {
                            self.send_response(&id, "get_commands", json!({ "commands": [] }));
                        }
                        "get_session_stats" => {
                            self.send_response(
                                &id,
                                "get_session_stats",
                                json!({
                                    "sessionFile": self.mirror.session_file.to_string_lossy(),
                                    "sessionId": self.mirror.session_id,
                                    "totalMessages": self.mirror.ui_messages.len(),
                                    "tokens": { "input": 0, "output": 0, "cacheRead": 0, "cacheWrite": 0, "total": 0 },
                                    "cost": 0.0,
                                }),
                            );
                        }
                        "prompt" => {
                            let message = cmd["message"].as_str().unwrap_or_default().to_string();
                            let images = extract_prompt_images(&cmd["images"]);
                            if active_turn.is_some() {
                                self.queued_prompts.push_back((message, images));
                                self.send_response(&id, "prompt", json!({ "disposition": "queued" }));
                            } else {
                                self.send_response(&id, "prompt", json!({ "disposition": "started" }));
                                match self.start_prompt_turn(message, images).await {
                                    Ok((turn, rx)) => {
                                        active_turn = Some(turn);
                                        active_prompt_rx = Some(rx);
                                    }
                                    Err(e) => {
                                        let err_turn = StreamingTurnState::new(epoch_ms());
                                        self.finish_turn(err_turn, Err(JsonRpcError {
                                            code: -32000,
                                            message: e.to_string(),
                                            data: None,
                                        }));
                                    }
                                }
                            }
                        }
                        "abort" => {
                            self.queued_prompts.clear();
                            self.cancel_all_pending_permissions().await;
                            if let Some(turn) = active_turn.as_mut() {
                                turn.aborted = true;
                                let _ = self
                                    .client
                                    .notify(
                                        "session/cancel",
                                        build_session_cancel_params(&self.acp_session_id),
                                    )
                                    .await;
                            }
                            self.send_response(&id, "abort", json!({}));
                        }
                        "oopbot_channel" => {
                            let topic = cmd["topic"].as_str().unwrap_or_default();
                            let data = cmd.get("data").unwrap_or(&Value::Null);
                            if topic == "tool_approval" {
                                let req_id = data["request_id"]
                                    .as_str()
                                    .or_else(|| data["requestId"].as_str())
                                    .unwrap_or_default();
                                let approved = data["approved"].as_bool().unwrap_or(false);
                                let option_id = data["optionId"].as_str();
                                if !req_id.is_empty() {
                                    self.resolve_permission(req_id, approved, option_id).await;
                                }
                            }
                        }
                        "clear_queue" => {
                            let drained: Vec<String> = self
                                .queued_prompts
                                .drain(..)
                                .map(|(m, _)| m)
                                .collect();
                            self.send_line(&json!({
                                "type": "queue_update",
                                "steering": [],
                                "followUp": [],
                            }));
                            self.send_response(
                                &id,
                                "clear_queue",
                                json!({ "steering": [], "followUp": drained }),
                            );
                        }
                        other => {
                            if !id.is_empty() {
                                self.send_error(
                                    &id,
                                    other,
                                    &format!("Command '{}' belum didukung pada mode ACP", other),
                                );
                            }
                        }
                    }
                }
            }
        }

        self.client.shutdown().await;
        Ok(())
    }
}

fn extract_prompt_images(val: &Value) -> Vec<(String, String)> {
    let Some(arr) = val.as_array() else {
        return Vec::new();
    };
    arr.iter()
        .filter_map(|item| {
            let mime = item["mimeType"]
                .as_str()
                .unwrap_or("image/jpeg")
                .to_string();
            let data = item["data"].as_str()?.to_string();
            if data.is_empty() {
                None
            } else {
                Some((mime, data))
            }
        })
        .collect()
}
