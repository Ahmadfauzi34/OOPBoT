//! Tipe wire JSON-RPC 2.0 untuk Agent Client Protocol (ACP) di atas newline-delimited stdio.

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

/// Versi protokol ACP utama yang dinegosiasikan pada `initialize`.
pub const ACP_PROTOCOL_VERSION: u64 = 1;

/// Error JSON-RPC 2.0 standar dari agen ACP.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct JsonRpcError {
    pub code: i64,
    pub message: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub data: Option<Value>,
}

impl std::fmt::Display for JsonRpcError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "ACP RPC error ({}): {}", self.code, self.message)
    }
}

impl std::error::Error for JsonRpcError {}

/// Pesan JSON-RPC 2.0 masuk dari stdout subprocess agen ACP.
#[derive(Debug, Clone, PartialEq)]
pub enum IncomingJsonRpc {
    /// Balasan terhadap request Client -> Agent (punya `id`, tanpa `method`).
    Response {
        id: Value,
        result: Option<Value>,
        error: Option<JsonRpcError>,
    },
    /// Request Agent -> Client (punya `id` dan `method`, mis. `session/request_permission`).
    Request {
        id: Value,
        method: String,
        params: Value,
    },
    /// Notification Agent -> Client (punya `method`, tanpa `id`, mis. `session/update`).
    Notification { method: String, params: Value },
}

/// Parse satu baris JSON-RPC 2.0 dari stdout agen ACP.
pub fn parse_incoming_jsonrpc(line: &str) -> Result<IncomingJsonRpc, String> {
    let val: Value =
        serde_json::from_str(line.trim()).map_err(|e| format!("JSON-RPC tidak valid: {}", e))?;
    let Some(obj) = val.as_object() else {
        return Err("Frame JSON-RPC harus berupa objek JSON".to_string());
    };

    let method = obj
        .get("method")
        .and_then(|v| v.as_str())
        .map(str::to_string);
    let id = obj.get("id").filter(|v| !v.is_null()).cloned();

    match (method, id) {
        (Some(method), Some(id)) => Ok(IncomingJsonRpc::Request {
            id,
            method,
            params: obj.get("params").cloned().unwrap_or_else(|| json!({})),
        }),
        (Some(method), None) => Ok(IncomingJsonRpc::Notification {
            method,
            params: obj.get("params").cloned().unwrap_or_else(|| json!({})),
        }),
        (None, Some(id)) => {
            let error = obj
                .get("error")
                .filter(|v| !v.is_null())
                .and_then(|e| serde_json::from_value::<JsonRpcError>(e.clone()).ok());
            let result = obj.get("result").cloned();
            Ok(IncomingJsonRpc::Response { id, result, error })
        }
        (None, None) => Err("Frame JSON-RPC tidak memiliki 'method' maupun 'id'".to_string()),
    }
}

/// Bangun objek JSON-RPC 2.0 Request (Client -> Agent).
pub fn build_request(id: u64, method: &str, params: Value) -> Value {
    json!({
        "jsonrpc": "2.0",
        "id": id,
        "method": method,
        "params": params,
    })
}

/// Bangun objek JSON-RPC 2.0 Notification (Client -> Agent, tanpa `id`).
pub fn build_notification(method: &str, params: Value) -> Value {
    json!({
        "jsonrpc": "2.0",
        "method": method,
        "params": params,
    })
}

/// Bangun objek JSON-RPC 2.0 Response sukses (Client -> Agent untuk `session/request_permission`, dll.).
pub fn build_response_ok(id: Value, result: Value) -> Value {
    json!({
        "jsonrpc": "2.0",
        "id": id,
        "result": result,
    })
}

/// Bangun objek JSON-RPC 2.0 Response error.
pub fn build_response_error(id: Value, code: i64, message: &str) -> Value {
    json!({
        "jsonrpc": "2.0",
        "id": id,
        "error": {
            "code": code,
            "message": message,
        }
    })
}

/// Parameter `initialize` untuk negosiasi kapabilitas Client <-> Agent.
pub fn build_initialize_params(client_name: &str, client_version: &str) -> Value {
    json!({
        "protocolVersion": ACP_PROTOCOL_VERSION,
        "clientInfo": {
            "name": client_name,
            "version": client_version,
        },
        "clientCapabilities": {}
    })
}

/// Parameter `session/new` untuk membuat sesi ACP baru di direktori `cwd`.
pub fn build_session_new_params(cwd: &str) -> Value {
    json!({
        "cwd": cwd,
        "mcpServers": []
    })
}

/// Parameter `session/load` untuk membuka kembali sesi ACP sebelumnya.
pub fn build_session_load_params(session_id: &str, cwd: &str) -> Value {
    json!({
        "sessionId": session_id,
        "cwd": cwd,
        "mcpServers": []
    })
}

/// Parameter `session/prompt` berisi teks dan gambar base64 (jika ada).
pub fn build_session_prompt_params(
    session_id: &str,
    text: &str,
    images: &[(String, String)],
) -> Value {
    let mut prompt_blocks = Vec::new();
    if !text.is_empty() {
        prompt_blocks.push(json!({
            "type": "text",
            "text": text,
        }));
    }
    for (mime, data) in images {
        if !data.is_empty() {
            prompt_blocks.push(json!({
                "type": "image",
                "mimeType": mime,
                "data": data,
            }));
        }
    }
    if prompt_blocks.is_empty() {
        prompt_blocks.push(json!({ "type": "text", "text": "" }));
    }
    json!({
        "sessionId": session_id,
        "prompt": prompt_blocks,
    })
}

/// Parameter notification `session/cancel`.
pub fn build_session_cancel_params(session_id: &str) -> Value {
    json!({
        "sessionId": session_id,
    })
}

/// Hasil `session/request_permission` ketika user memilih salah satu opsi.
pub fn build_permission_selected_result(option_id: &str) -> Value {
    json!({
        "outcome": {
            "outcome": "selected",
            "optionId": option_id,
        }
    })
}

/// Hasil `session/request_permission` ketika permintaan dibatalkan (`abort`).
pub fn build_permission_cancelled_result() -> Value {
    json!({
        "outcome": {
            "outcome": "cancelled",
        }
    })
}

/// Satu entri rencana kerja dalam update ACP `plan`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AcpPlanEntry {
    pub content: String,
    pub status: String,
    pub priority: Option<String>,
}

/// Notifikasi `session/update` yang telah dipetakan ke tipe Rust bersih.
#[derive(Debug, Clone, PartialEq)]
pub enum AcpSessionUpdate {
    AgentThoughtChunk {
        text: String,
    },
    AgentMessageChunk {
        text: String,
    },
    ToolCall {
        tool_call_id: String,
        title: String,
        kind: String,
        raw_input: Value,
    },
    ToolCallUpdate {
        tool_call_id: String,
        status: String,
        title: Option<String>,
        output_text: String,
        is_error: bool,
    },
    Plan {
        entries: Vec<AcpPlanEntry>,
    },
    Unknown(Value),
}

/// Ekstrak teks dari field `content` ACP (bisa berupa objek `{type:"text", text}`, array blok, atau string).
pub fn extract_acp_content_text(val: &Value) -> String {
    if let Some(s) = val.as_str() {
        return s.to_string();
    }
    if let Some(obj) = val.as_object() {
        if let Some(t) = obj.get("text").and_then(|v| v.as_str()) {
            return t.to_string();
        }
        if let Some(inner) = obj.get("content") {
            return extract_acp_content_text(inner);
        }
    }
    if let Some(arr) = val.as_array() {
        let mut parts = Vec::new();
        for item in arr {
            let chunk = extract_acp_content_text(item);
            if !chunk.is_empty() {
                parts.push(chunk);
            }
        }
        return parts.join("\n");
    }
    String::new()
}

/// Parse payload `params` dari notification `session/update`.
pub fn parse_session_update(params: &Value) -> AcpSessionUpdate {
    let update = params.get("update").unwrap_or(params);
    let kind_tag = update["sessionUpdate"]
        .as_str()
        .or_else(|| update["type"].as_str())
        .unwrap_or_default();

    match kind_tag {
        "agent_thought_chunk" | "thought_chunk" => AcpSessionUpdate::AgentThoughtChunk {
            text: extract_acp_content_text(update.get("content").unwrap_or(update)),
        },
        "agent_message_chunk" | "message_chunk" => AcpSessionUpdate::AgentMessageChunk {
            text: extract_acp_content_text(update.get("content").unwrap_or(update)),
        },
        "tool_call" => {
            let tool_call_id = update["toolCallId"]
                .as_str()
                .or_else(|| update["id"].as_str())
                .unwrap_or("acp-tool")
                .to_string();
            let title = update["title"]
                .as_str()
                .or_else(|| update["name"].as_str())
                .unwrap_or("tool")
                .to_string();
            let kind = update["kind"].as_str().unwrap_or("other").to_string();
            let raw_input = update
                .get("rawInput")
                .or_else(|| update.get("arguments"))
                .cloned()
                .unwrap_or_else(|| json!({}));
            AcpSessionUpdate::ToolCall {
                tool_call_id,
                title,
                kind,
                raw_input,
            }
        }
        "tool_call_update" => {
            let tool_call_id = update["toolCallId"]
                .as_str()
                .or_else(|| update["id"].as_str())
                .unwrap_or("acp-tool")
                .to_string();
            let status = update["status"].as_str().unwrap_or("in_progress").to_string();
            let title = update["title"].as_str().map(str::to_string);
            let mut output_text = update
                .get("content")
                .map(extract_acp_content_text)
                .unwrap_or_default();
            if output_text.is_empty() {
                if let Some(raw_out) = update.get("rawOutput") {
                    if let Some(s) = raw_out.as_str() {
                        output_text = s.to_string();
                    } else if !raw_out.is_null() {
                        output_text = raw_out.to_string();
                    }
                }
            }
            let is_error = status.eq_ignore_ascii_case("failed")
                || status.eq_ignore_ascii_case("error")
                || status.eq_ignore_ascii_case("rejected");
            AcpSessionUpdate::ToolCallUpdate {
                tool_call_id,
                status,
                title,
                output_text,
                is_error,
            }
        }
        "plan" => {
            let entries = update
                .get("entries")
                .and_then(|v| v.as_array())
                .map(|arr| {
                    arr.iter()
                        .filter_map(|e| {
                            let content = e["content"].as_str()?.trim().to_string();
                            if content.is_empty() {
                                return None;
                            }
                            let status = e["status"].as_str().unwrap_or("pending").to_string();
                            let priority = e["priority"].as_str().map(str::to_string);
                            Some(AcpPlanEntry {
                                content,
                                status,
                                priority,
                            })
                        })
                        .collect()
                })
                .unwrap_or_default();
            AcpSessionUpdate::Plan { entries }
        }
        _ => AcpSessionUpdate::Unknown(update.clone()),
    }
}

/// Satu opsi izin pada `session/request_permission`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AcpPermissionOption {
    pub option_id: String,
    pub name: String,
    pub kind: String,
}

/// Request `session/request_permission` dari agen ACP ke client.
#[derive(Debug, Clone, PartialEq)]
pub struct AcpPermissionRequest {
    pub session_id: String,
    pub tool_call_id: String,
    pub title: String,
    pub kind: String,
    pub raw_input: Value,
    pub options: Vec<AcpPermissionOption>,
}

impl AcpPermissionRequest {
    /// Pilih `option_id` terbaik untuk jawaban boolean `approved` (allow_once / reject_once).
    pub fn pick_option_id(&self, approved: bool) -> Option<&str> {
        let preferred_kinds: &[&str] = if approved {
            &["allow_once", "allow_always"]
        } else {
            &["reject_once", "reject_always"]
        };
        for target in preferred_kinds {
            if let Some(opt) = self.options.iter().find(|o| o.kind == *target) {
                return Some(opt.option_id.as_str());
            }
        }
        if approved {
            self.options.first().map(|o| o.option_id.as_str())
        } else {
            self.options.last().map(|o| o.option_id.as_str())
        }
    }
}

/// Parse payload `params` dari request `session/request_permission`.
pub fn parse_permission_request(params: &Value) -> AcpPermissionRequest {
    let session_id = params["sessionId"].as_str().unwrap_or_default().to_string();
    let tc = params.get("toolCall").unwrap_or(params);
    let tool_call_id = tc["toolCallId"]
        .as_str()
        .or_else(|| tc["id"].as_str())
        .unwrap_or("acp-perm")
        .to_string();
    let title = tc["title"]
        .as_str()
        .or_else(|| tc["name"].as_str())
        .unwrap_or("Tool action")
        .to_string();
    let kind = tc["kind"].as_str().unwrap_or("execute").to_string();
    let raw_input = tc
        .get("rawInput")
        .or_else(|| tc.get("arguments"))
        .cloned()
        .unwrap_or_else(|| json!({}));

    let options = params
        .get("options")
        .and_then(|v| v.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|o| {
                    let option_id = o["optionId"]
                        .as_str()
                        .or_else(|| o["id"].as_str())?
                        .to_string();
                    let name = o["name"]
                        .as_str()
                        .or_else(|| o["label"].as_str())
                        .unwrap_or(&option_id)
                        .to_string();
                    let kind = o["kind"].as_str().unwrap_or_default().to_string();
                    Some(AcpPermissionOption {
                        option_id,
                        name,
                        kind,
                    })
                })
                .collect()
        })
        .unwrap_or_default();

    AcpPermissionRequest {
        session_id,
        tool_call_id,
        title,
        kind,
        raw_input,
        options,
    }
}

/// Petakan kategori tool ACP (`read`, `edit`, `execute`, `search`, `fetch`, dll.) ke nama kartu tool UI OOPBoT.
pub fn map_acp_tool_kind_to_oopbot(kind: &str) -> &'static str {
    match kind.to_ascii_lowercase().as_str() {
        "read" => "read",
        "edit" | "delete" | "move" => "write",
        "execute" => "bash",
        "search" => "web_search",
        "fetch" => "fetch_page",
        _ => "tool",
    }
}
