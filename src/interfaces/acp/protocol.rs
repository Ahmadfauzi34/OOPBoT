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
        "clientCapabilities": {
            "fs": {
                "readTextFile": true,
                "writeTextFile": true
            },
            "terminal": true
        }
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

/// Parameter `session/set_model` untuk mengganti model aktif pada sesi ACP.
pub fn build_session_set_model_params(session_id: &str, model_id: &str) -> Value {
    json!({
        "sessionId": session_id,
        "modelId": model_id,
    })
}

/// Parameter `session/set_config_option` (mis. `thought_level` atau `model`).
pub fn build_session_set_config_option_params(
    session_id: &str,
    config_id: &str,
    value: &str,
) -> Value {
    json!({
        "sessionId": session_id,
        "configId": config_id,
        "value": value,
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

/// Deskripsi satu model yang diiklankan agen ACP di `models.availableModels`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AcpModelOption {
    pub model_id: String,
    pub name: String,
    pub description: Option<String>,
}

/// Satu pilihan nilai di dalam `SessionConfigOption` (`select`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AcpConfigChoice {
    pub value: String,
    pub name: String,
    pub description: Option<String>,
}

/// Opsi konfigurasi sesi ACP (`session/new`, `session/load`, `config_option_update`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AcpConfigOption {
    pub id: String,
    pub name: String,
    pub category: Option<String>,
    pub current_value: String,
    pub options: Vec<AcpConfigChoice>,
}

fn push_select_choice(out: &mut Vec<AcpConfigChoice>, item: &Value) {
    let Some(val) = item["value"]
        .as_str()
        .or_else(|| item["id"].as_str())
        .map(str::trim)
        .filter(|s| !s.is_empty())
    else {
        return;
    };
    let name = item["name"]
        .as_str()
        .or_else(|| item["label"].as_str())
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .unwrap_or(val)
        .to_string();
    let description = item["description"].as_str().map(str::to_string);
    out.push(AcpConfigChoice {
        value: val.to_string(),
        name,
        description,
    });
}

/// Parse daftar `configOptions` dari hasil `session/new`, `session/load`, atau `session/set_config_option`.
pub fn parse_session_config_options(val: &Value) -> Vec<AcpConfigOption> {
    let Some(arr) = val.get("configOptions").and_then(|v| v.as_array()) else {
        return Vec::new();
    };
    arr.iter()
        .filter_map(|opt| {
            let id = opt["id"].as_str().map(str::trim).filter(|s| !s.is_empty())?;
            let name = opt["name"]
                .as_str()
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .unwrap_or(id)
                .to_string();
            let category = opt["category"].as_str().map(str::to_string);
            let current_value = opt["currentValue"]
                .as_str()
                .unwrap_or_default()
                .to_string();
            let mut choices = Vec::new();
            if let Some(raw_opts) = opt.get("options").and_then(|v| v.as_array()) {
                for entry in raw_opts {
                    if let Some(group_opts) = entry.get("options").and_then(|v| v.as_array()) {
                        for sub in group_opts {
                            push_select_choice(&mut choices, sub);
                        }
                    } else {
                        push_select_choice(&mut choices, entry);
                    }
                }
            }
            Some(AcpConfigOption {
                id: id.to_string(),
                name,
                category,
                current_value,
                options: choices,
            })
        })
        .collect()
}

/// Parse `(current_model_id, available_models)` dari hasil `session/new` atau `session/load`.
pub fn parse_session_models(val: &Value) -> (Option<String>, Vec<AcpModelOption>) {
    let mut current = val["models"]["currentModelId"]
        .as_str()
        .or_else(|| val["model"].as_str())
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string);
    let mut models = Vec::new();
    if let Some(arr) = val["models"]["availableModels"].as_array() {
        for item in arr {
            let Some(mid) = item["modelId"]
                .as_str()
                .or_else(|| item["id"].as_str())
                .map(str::trim)
                .filter(|s| !s.is_empty())
            else {
                continue;
            };
            let name = item["name"]
                .as_str()
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .unwrap_or(mid)
                .to_string();
            let description = item["description"].as_str().map(str::to_string);
            models.push(AcpModelOption {
                model_id: mid.to_string(),
                name,
                description,
            });
        }
    }
    if models.is_empty() {
        let cfgs = parse_session_config_options(val);
        if let Some(model_cfg) = cfgs
            .into_iter()
            .find(|c| c.category.as_deref() == Some("model"))
        {
            if current.is_none() && !model_cfg.current_value.trim().is_empty() {
                current = Some(model_cfg.current_value);
            }
            for ch in model_cfg.options {
                models.push(AcpModelOption {
                    model_id: ch.value,
                    name: ch.name,
                    description: ch.description,
                });
            }
        }
    }
    (current, models)
}

/// Satu slash command yang diiklankan agen ACP via `available_commands_update`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AcpSlashCommand {
    pub name: String,
    pub description: String,
    pub input_hint: Option<String>,
}

/// Satu mode operasi agen ACP (`session/new` `modes.availableModes` / `session/set_mode`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AcpSessionMode {
    pub id: String,
    pub name: String,
    pub description: Option<String>,
}

/// Ringkasan penggunaan token & biaya sesi ACP (`usage_update` atau hasil `session/prompt`).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct AcpUsageStats {
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cache_read_tokens: u64,
    pub cache_write_tokens: u64,
    pub total_tokens: u64,
    pub context_tokens: Option<u64>,
    pub context_window: Option<u64>,
    pub cost_amount: f64,
}

/// Parameter `session/set_mode` untuk mengganti mode kerja agen ACP.
pub fn build_session_set_mode_params(session_id: &str, mode_id: &str) -> Value {
    json!({
        "sessionId": session_id,
        "modeId": mode_id,
    })
}

/// Parse daftar `availableCommands` dari update `available_commands_update`.
pub fn parse_available_commands(val: &Value) -> Vec<AcpSlashCommand> {
    let Some(arr) = val
        .get("availableCommands")
        .or_else(|| val.get("commands"))
        .and_then(|v| v.as_array())
    else {
        return Vec::new();
    };
    arr.iter()
        .filter_map(|cmd| {
            let name = cmd["name"].as_str()?.trim().trim_start_matches('/').to_string();
            if name.is_empty() {
                return None;
            }
            let description = cmd["description"].as_str().unwrap_or_default().to_string();
            let input_hint = cmd["input"]["hint"]
                .as_str()
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(str::to_string);
            Some(AcpSlashCommand {
                name,
                description,
                input_hint,
            })
        })
        .collect()
}

/// Parse `(current_mode_id, available_modes)` dari `session/new` / `session/load`.
pub fn parse_session_modes(val: &Value) -> (Option<String>, Vec<AcpSessionMode>) {
    let current = val["modes"]["currentModeId"]
        .as_str()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string);
    let mut modes = Vec::new();
    if let Some(arr) = val["modes"]["availableModes"].as_array() {
        for item in arr {
            let Some(id) = item["id"].as_str().map(str::trim).filter(|s| !s.is_empty()) else {
                continue;
            };
            let name = item["name"]
                .as_str()
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .unwrap_or(id)
                .to_string();
            let description = item["description"].as_str().map(str::to_string);
            modes.push(AcpSessionMode {
                id: id.to_string(),
                name,
                description,
            });
        }
    }
    (current, modes)
}

/// Sampel delta/snapshot usage dari `session/prompt` (`usage`) atau `usage_update`.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct AcpUsageSample {
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cache_read_tokens: u64,
    pub cache_write_tokens: u64,
    pub used_tokens: Option<u64>,
    pub context_window: Option<u64>,
    pub cost_amount: Option<f64>,
}

/// Ekstrak metadata usage dari objek `session/prompt` result (`usage`) atau `usage_update`.
pub fn extract_usage_stats(val: &Value) -> Option<AcpUsageSample> {
    let u = val.get("usage").unwrap_or(val);
    let input = u["inputTokens"]
        .as_u64()
        .or_else(|| u["input_tokens"].as_u64())
        .or_else(|| u["promptTokens"].as_u64())
        .unwrap_or(0);
    let output = u["outputTokens"]
        .as_u64()
        .or_else(|| u["output_tokens"].as_u64())
        .or_else(|| u["completionTokens"].as_u64())
        .unwrap_or(0);
    let cache_read = u["cachedReadTokens"]
        .as_u64()
        .or_else(|| u["cacheReadTokens"].as_u64())
        .unwrap_or(0);
    let cache_write = u["cachedWriteTokens"]
        .as_u64()
        .or_else(|| u["cacheWriteTokens"].as_u64())
        .unwrap_or(0);
    let used = u["used"].as_u64().or_else(|| u["totalTokens"].as_u64());
    let size = u["size"].as_u64().or_else(|| u["contextWindow"].as_u64());
    let cost = u["cost"]["amount"]
        .as_f64()
        .or_else(|| u["cost"].as_f64());
    if input == 0 && output == 0 && cache_read == 0 && cache_write == 0 && used.is_none() && cost.is_none() {
        None
    } else {
        Some(AcpUsageSample {
            input_tokens: input,
            output_tokens: output,
            cache_read_tokens: cache_read,
            cache_write_tokens: cache_write,
            used_tokens: used,
            context_window: size,
            cost_amount: cost,
        })
    }
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
        raw_input: Option<Value>,
        output_text: String,
        is_error: bool,
    },
    Plan {
        entries: Vec<AcpPlanEntry>,
    },
    ConfigOptionUpdate {
        config_options: Vec<AcpConfigOption>,
    },
    AvailableCommandsUpdate {
        commands: Vec<AcpSlashCommand>,
    },
    SessionInfoUpdate {
        title: Option<String>,
    },
    CurrentModeUpdate {
        current_mode_id: String,
    },
    UsageUpdate {
        used_tokens: Option<u64>,
        context_window: Option<u64>,
        cost_amount: Option<f64>,
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

fn first_nonempty_trimmed<'a>(obj: &'a Value, keys: &[&str]) -> Option<&'a str> {
    for k in keys {
        if let Some(s) = obj.get(*k).and_then(|v| v.as_str()).map(str::trim) {
            if !s.is_empty() {
                return Some(s);
            }
        }
    }
    None
}

fn first_nonempty_raw<'a>(obj: &'a Value, keys: &[&str]) -> Option<&'a str> {
    for k in keys {
        if let Some(s) = obj.get(*k).and_then(|v| v.as_str()) {
            if !s.is_empty() {
                return Some(s);
            }
        }
    }
    None
}

/// Normalisasikan `rawInput` / `arguments` + `locations` ACP ke schema OOPBoT (`path`, `content`, `command`).
pub fn normalize_acp_tool_args(container: &Value) -> Value {
    let raw = container
        .get("rawInput")
        .or_else(|| container.get("arguments"))
        .cloned()
        .unwrap_or_else(|| json!({}));

    let mut out = if raw.is_object() { raw } else { json!({}) };

    if out.get("path").and_then(|v| v.as_str()).is_none() {
        let loc_path = container
            .get("locations")
            .and_then(|v| v.as_array())
            .and_then(|arr| arr.first())
            .and_then(|l| l.get("path").and_then(|p| p.as_str()).or_else(|| l.as_str()))
            .map(str::trim)
            .filter(|s| !s.is_empty());

        let found_path = first_nonempty_trimmed(
            &out,
            &[
                "path",
                "file_path",
                "filePath",
                "target_file",
                "targetFile",
                "TargetFile",
                "FilePath",
                "file",
            ],
        )
        .map(str::to_string)
        .or_else(|| loc_path.map(str::to_string));

        if let Some(p) = found_path {
            out["path"] = json!(p);
        }
    }

    if out.get("content").and_then(|v| v.as_str()).is_none() {
        if let Some(c) = first_nonempty_raw(
            &out,
            &["content", "CodeContent", "codeContent", "file_text", "text"],
        )
        .map(str::to_string)
        {
            out["content"] = json!(c);
        }
    }

    if out.get("command").and_then(|v| v.as_str()).is_none() {
        if let Some(cmd) =
            first_nonempty_trimmed(&out, &["command", "CommandLine", "commandLine", "cmd"])
                .map(str::to_string)
        {
            out["command"] = json!(cmd);
        }
    }

    out
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
            let raw_input = normalize_acp_tool_args(update);
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
            let norm_input = normalize_acp_tool_args(update);
            let raw_input = if norm_input.as_object().is_some_and(|m| !m.is_empty()) {
                Some(norm_input)
            } else {
                None
            };
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
                raw_input,
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
        "config_option_update" | "config_options_update" => AcpSessionUpdate::ConfigOptionUpdate {
            config_options: parse_session_config_options(update),
        },
        "available_commands_update" => AcpSessionUpdate::AvailableCommandsUpdate {
            commands: parse_available_commands(update),
        },
        "session_info_update" => AcpSessionUpdate::SessionInfoUpdate {
            title: update["title"]
                .as_str()
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(str::to_string),
        },
        "current_mode_update" => AcpSessionUpdate::CurrentModeUpdate {
            current_mode_id: update["currentModeId"]
                .as_str()
                .map(str::trim)
                .unwrap_or_default()
                .to_string(),
        },
        "usage_update" => AcpSessionUpdate::UsageUpdate {
            used_tokens: update["used"].as_u64(),
            context_window: update["size"].as_u64(),
            cost_amount: update["cost"]["amount"]
                .as_f64()
                .or_else(|| update["cost"].as_f64()),
        },
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
    let raw_input = normalize_acp_tool_args(tc);

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

/// Petakan kategori tool ACP (`read`, `edit`, `write`, `execute`, `search`, `fetch`, dll.) ke nama kartu tool UI OOPBoT.
pub fn map_acp_tool_kind_to_oopbot(kind: &str) -> &'static str {
    match kind.to_ascii_lowercase().as_str() {
        "read" => "read",
        "write" | "edit" | "delete" | "move" => "write",
        "execute" | "bash" | "shell" => "bash",
        "search" => "web_search",
        "fetch" => "fetch_page",
        _ => "tool",
    }
}
