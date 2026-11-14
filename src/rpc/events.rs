use super::{RpcServer, RpcUiState};
use crate::events::{AgentEvent, EventSink, StepsOutcome, ToolStatus};
use crate::plugins::PluginRegistry;
use crate::util::epoch_ms;
use serde_json::{json, Value};
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

/// Konversi nama & argumen tool internal `oopbot` ke format tampilan `ChatTimeline.kt` & `StepFigure.kt`.
pub(super) fn map_tool_for_pipkin_ui(tool_name: &str, args: &Value) -> (String, Value) {
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

pub(super) fn format_tool_output(tool_name: &str, val: &Value) -> (String, bool) {
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

                let details_opt = output.get("details").filter(|v| !v.is_null()).cloned();

                let mut result_payload = json!({
                    "content": [{ "type": "text", "text": display_text }]
                });
                if let Some(ref details) = details_opt {
                    result_payload["details"] = details.clone();
                }

                self.emit(&json!({
                    "type": "tool_execution_end",
                    "toolCallId": call_id,
                    "toolName": ui_tool_name,
                    "isError": is_error,
                    "result": result_payload
                }));

                let mut tool_result_msg = json!({
                    "role": "toolResult",
                    "toolCallId": call_id,
                    "toolName": ui_tool_name,
                    "isError": is_error,
                    "content": [{ "type": "text", "text": display_text }],
                    "timestamp": epoch_ms()
                });
                if let Some(details) = details_opt {
                    tool_result_msg["details"] = details;
                }

                self.emit(&json!({
                    "type": "message_end",
                    "message": tool_result_msg
                }));

                {
                    let mut state = self.ui_state.lock().unwrap();
                    state.ui_messages.push(tool_result_msg.clone());
                    state.append_session_entry("message", json!({ "message": tool_result_msg }));
                    if let Some(plugins) = &self.plugins {
                        for (custom_type, data) in
                            plugins.collect_state_entries_for_tool(tool_name, output)
                        {
                            state.append_session_entry(
                                "custom",
                                json!({ "customType": custom_type, "data": data }),
                            );
                        }
                    }
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


pub(super) fn reverse_map_tool_name(name: &str) -> &str {
    match name {
        "bash" => "run_shell",
        "read" => "read_file",
        "write" => "write_file",
        "ls" => "list_dir",
        other => other,
    }
}

