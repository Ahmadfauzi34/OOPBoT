//! Mode RPC (`oopbot --mode rpc`) yang kompatibel 100% dengan protokol JSONL
//! aplikasi Android Pipkin (`AgentProcess.kt` & `ChatTimeline.kt`).
//!
//! Menggantikan `@earendil-works/pi-coding-agent` (`pi --mode rpc`) di Termux
//! dengan binary native Rust `oopbot`.

use crate::gemini::{
    function_response_part, user_text, BoxError, FunctionCall, GeminiClient,
    DEFAULT_GEMINI_BASE_URL,
};
use crate::models::{ModelCatalog, ModelInfo};
use crate::tools::ToolRegistry;
use serde_json::{json, Value};
use std::collections::HashMap;
use std::io::{self, BufRead, Write};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

const BUILTIN_PROVIDER_URLS: &[(&str, &str)] = &[
    ("google", DEFAULT_GEMINI_BASE_URL),
    ("openrouter", "https://openrouter.ai/api/v1"),
    ("zai", "https://api.z.ai/api/coding/paas/v4"),
    ("kimi-coding", "https://api.kimi.com/coding/v1"),
    ("openai", "https://api.openai.com/v1"),
];

#[derive(Debug, Clone)]
struct ProviderEndpoint {
    base_url: String,
    api_key: String,
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
    /// Riwayat pesan dalam format Pipkin (`role: "user" | "assistant" | "toolResult"`).
    ui_messages: Vec<Value>,
    /// Riwayat pesan dalam format `GeminiClient` (`role: "user" | "model", parts: [...]`).
    llm_history: Vec<Value>,
    entry_seq: usize,
    total_tokens: u64,
}

impl RpcServer {
    pub fn new(
        session_path: Option<String>,
        system_append: Option<String>,
    ) -> Result<Self, BoxError> {
        let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
        let config_dir = std::env::var("PI_CODING_AGENT_DIR")
            .map(PathBuf::from)
            .unwrap_or_else(|_| {
                let home = std::env::var("HOME").unwrap_or_else(|_| ".".into());
                PathBuf::from(home).join(".pipkin/pi")
            });
        let _ = std::fs::create_dir_all(&config_dir);

        let tools = ToolRegistry::with_defaults();
        let mut system_instruction = default_rpc_system_instruction(&cwd, &tools);
        if let Some(append) = system_append.filter(|s| !s.trim().is_empty()) {
            system_instruction.push_str("\n\n");
            system_instruction.push_str(&append);
        }

        let catalog = ModelCatalog::builtin();
        let mut providers = load_providers(&config_dir, &catalog);

        let settings = read_json_file(&config_dir.join("settings.json"));
        let mut active_provider = settings["defaultProvider"]
            .as_str()
            .unwrap_or("google")
            .to_string();
        let mut active_model = settings["defaultModel"]
            .as_str()
            .unwrap_or_else(|| catalog.default_model())
            .to_string();
        let thinking_level = settings["defaultThinkingLevel"]
            .as_str()
            .unwrap_or("medium")
            .to_string();

        // Jika provider default belum punya key tetapi provider lain punya key, pilih yang aktif.
        if !providers
            .get(&active_provider)
            .map(|p| !p.api_key.is_empty())
            .unwrap_or(false)
        {
            if let Some((pid, ep)) = providers.iter().find(|(_, p)| !p.api_key.is_empty()) {
                active_provider = pid.clone();
                if let Some(first) = ep.models.first() {
                    active_model = first.id.clone();
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
                base_url: DEFAULT_GEMINI_BASE_URL.to_string(),
                api_key: String::new(),
                models: catalog.list().to_vec(),
            });

        let (session_file, session_id, session_name, ui_messages, llm_history, entry_seq) =
            init_or_load_session(&config_dir, &cwd, session_path.as_deref());

        Ok(Self {
            cwd,
            config_dir,
            session_file,
            session_id,
            session_name,
            active_provider,
            active_model,
            thinking_level,
            system_instruction,
            providers,
            catalog,
            client,
            tools,
            ui_messages,
            llm_history,
            entry_seq,
            total_tokens: 0,
        })
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

    fn current_model_json(&self) -> Value {
        let name = self
            .catalog
            .find(&self.active_model)
            .map(|m| m.label.clone())
            .unwrap_or_else(|| self.active_model.clone());
        json!({
            "provider": self.active_provider,
            "id": self.active_model,
            "name": name,
            "input": ["text", "image"],
        })
    }

    fn available_models_json(&self) -> Vec<Value> {
        let mut list = Vec::new();
        for (pid, ep) in &self.providers {
            for m in &ep.models {
                list.push(json!({
                    "provider": pid,
                    "id": m.id,
                    "name": m.label,
                    "input": ["text", "image"],
                    "contextWindow": m.input_token_limit.unwrap_or(1_048_576),
                    "maxTokens": m.output_token_limit.unwrap_or(65_536),
                    "reasoning": m.supports_tools,
                }));
            }
        }
        if list.is_empty() {
            for m in self.catalog.list() {
                list.push(json!({
                    "provider": "google",
                    "id": m.id,
                    "name": m.label,
                    "input": ["text", "image"],
                    "contextWindow": m.input_token_limit.unwrap_or(1_048_576),
                    "maxTokens": m.output_token_limit.unwrap_or(65_536),
                    "reasoning": m.supports_tools,
                }));
            }
        }
        list
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
                            .map(|m| format!("- `{}` ({})", m.id, m.label))
                            .collect();
                        self.catalog.replace_with_scanned(scanned.clone());
                        if let Some(ep) = self.providers.get_mut(&self.active_provider) {
                            ep.models = scanned;
                        }
                        Some(format!(
                            "Berhasil men-scan **{} model** dari `{}`:\n{}\n\nGunakan pemilih model di atas atau ketik `/model <ID>` untuk mengganti model.",
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
                        if let Some(ep) = self.providers.get_mut(&self.active_provider) {
                            ep.api_key = api_key.to_string();
                            if let Some(u) = &base_url {
                                ep.base_url = u.clone();
                            }
                            ep.models = scanned;
                        }
                        Some(format!(
                            "Token berhasil disimpan & **{} model** terdeteksi otomatis! Model aktif sekarang: `{}`.",
                            count, self.active_model
                        ))
                    }
                    Err(e) => Some(format!(
                        "Token disimpan, namun auto-scan `/models` gagal: {}",
                        e
                    )),
                }
            }
            "/models" => {
                let list = self.catalog.list();
                let lines: Vec<String> = list
                    .iter()
                    .enumerate()
                    .map(|(i, m)| {
                        let mark = if m.id == self.active_model {
                            " (aktif)"
                        } else {
                            ""
                        };
                        format!("{}. `{}` — {}{}", i + 1, m.id, m.label, mark)
                    })
                    .collect();
                Some(format!("**Daftar Model ({}):**\n{}", list.len(), lines.join("\n")))
            }
            "/model" => {
                if parts.len() < 2 {
                    return Some(format!(
                        "Model aktif saat ini: `{}` (`{}`). Gunakan `/model <ID>` untuk mengganti.",
                        self.active_model, self.active_provider
                    ));
                }
                let target = parts[1].trim();
                if let Some(info) = self.catalog.find(target) {
                    self.active_model = info.id.clone();
                } else {
                    self.catalog.register_custom(target, target);
                    self.active_model = target.to_string();
                }
                Some(format!("Model aktif diganti ke `{}`.", self.active_model))
            }
            "/clear" => {
                self.ui_messages.clear();
                self.llm_history.clear();
                Some("Memori percakapan sesi ini telah dibersihkan.".to_string())
            }
            _ => None,
        }
    }

    async fn execute_prompt(&mut self, prompt: &str) {
        self.refresh_credentials();
        let started_ms = epoch_ms();

        Self::send_line(&json!({ "type": "agent_start" }));

        let user_msg = json!({
            "role": "user",
            "content": [{ "type": "text", "text": prompt }],
            "timestamp": started_ms,
        });
        Self::send_line(&json!({ "type": "message_start", "message": user_msg }));
        Self::send_line(&json!({ "type": "message_end", "message": user_msg }));
        self.ui_messages.push(user_msg.clone());
        self.append_session_entry("message", json!({ "message": user_msg }));

        // Cek apakah perintah slash internal (/scan, /token, /models, /model, /clear)
        if prompt.trim_start().starts_with('/') {
            if let Some(reply_text) = self.handle_slash_command(prompt.trim()).await {
                self.emit_assistant_reply(&reply_text, started_ms);
                Self::send_line(&json!({ "type": "turn_end" }));
                Self::send_line(&json!({ "type": "agent_end" }));
                Self::send_line(&json!({ "type": "agent_settled", "aborted": false }));
                return;
            }
        }

        if self.client.api_key().trim().is_empty() {
            let msg = "API key belum diatur. Silakan buka **Settings › Providers & models** di Pipkin untuk menempelkan API key (Google Gemini / OpenRouter / Custom), atau ketik langsung di sini:\n\n`/token <API_KEY>`";
            self.emit_assistant_reply(msg, started_ms);
            Self::send_line(&json!({ "type": "turn_end" }));
            Self::send_line(&json!({ "type": "agent_end" }));
            Self::send_line(&json!({ "type": "agent_settled", "aborted": false }));
            return;
        }

        self.llm_history.push(user_text(prompt));
        let decls = self.tools.declarations();
        let max_steps = 25usize;

        for step_idx in 1..=max_steps {
            let step_start_ms = epoch_ms();
            let turn_res = self
                .client
                .generate_with_model(
                    &self.active_model,
                    &self.system_instruction,
                    &self.llm_history,
                    &decls,
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
            self.total_tokens += (prompt.len() as u64 / 4) + (turn.text.len() as u64 / 4) + 120;

            if turn.function_calls.is_empty() {
                self.emit_assistant_reply(&turn.text, step_start_ms);
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

            self.llm_history
                .push(json!({ "role": "user", "parts": llm_response_parts }));
        }

        Self::send_line(&json!({ "type": "turn_end" }));
        Self::send_line(&json!({ "type": "agent_end" }));
        Self::send_line(&json!({ "type": "agent_settled", "aborted": false }));
    }

    fn emit_assistant_reply(&mut self, text: &str, started_ms: u64) {
        let now_ms = epoch_ms();
        Self::send_line(&json!({
            "type": "message_start",
            "message": {
                "role": "assistant",
                "content": [],
                "timestamp": started_ms
            }
        }));
        Self::send_line(&json!({
            "type": "message_update",
            "assistantMessageEvent": {
                "type": "text_start",
                "contentIndex": 0
            }
        }));
        Self::send_line(&json!({
            "type": "message_update",
            "assistantMessageEvent": {
                "type": "text_delta",
                "contentIndex": 0,
                "delta": text
            }
        }));
        Self::send_line(&json!({
            "type": "message_update",
            "assistantMessageEvent": {
                "type": "text_end",
                "contentIndex": 0,
                "content": text
            }
        }));
        let msg = json!({
            "role": "assistant",
            "content": [{ "type": "text", "text": text }],
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
                    let window = 1_048_576u64;
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
                            "cost": 0.0
                        }),
                    );
                }
                "get_available_models" => {
                    self.refresh_credentials();
                    Self::send_response(
                        &id,
                        "get_available_models",
                        json!({ "models": self.available_models_json() }),
                    );
                }
                "set_model" => {
                    if let Some(p) = cmd["provider"].as_str() {
                        self.active_provider = p.to_string();
                    }
                    if let Some(m) = cmd["modelId"].as_str() {
                        self.active_model = m.to_string();
                    }
                    self.refresh_credentials();
                    Self::send_response(&id, "set_model", self.current_model_json());
                }
                "get_available_thinking_levels" => {
                    Self::send_response(
                        &id,
                        "get_available_thinking_levels",
                        json!({ "levels": ["off", "low", "medium", "high"] }),
                    );
                }
                "set_thinking_level" => {
                    if let Some(l) = cmd["level"].as_str() {
                        self.thinking_level = l.to_string();
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
                                { "name": "models", "description": "Tampilkan semua model di katalog" },
                                { "name": "model", "description": "Ganti model aktif (/model <ID>)" },
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
                    Self::send_response(
                        &id,
                        &cmd_type,
                        json!({ "disposition": "started" }),
                    );
                    if !message.trim().is_empty() {
                        self.execute_prompt(&message).await;
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

pub async fn run_rpc_mode(
    session_path: Option<String>,
    system_append: Option<String>,
) -> Result<(), BoxError> {
    let mut server = RpcServer::new(session_path, system_append)?;
    server.run_loop().await
}

/// Konversi nama & argumen tool internal `oopbot` ke format tampilan `ChatTimeline.kt` & `StepFigure.kt`.
fn map_tool_for_pipkin_ui(tool_name: &str, args: &Value) -> (String, Value) {
    match tool_name {
        "run_shell" => ("bash".to_string(), args.clone()),
        "read_file" => ("read".to_string(), args.clone()),
        "write_file" => ("write".to_string(), args.clone()),
        "list_dir" => ("ls".to_string(), args.clone()),
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
        _ => (val.to_string(), false),
    }
}

fn load_providers(config_dir: &Path, catalog: &ModelCatalog) -> HashMap<String, ProviderEndpoint> {
    let mut map = HashMap::new();
    let auth_json = read_json_file(&config_dir.join("auth.json"));
    let models_json = read_json_file(&config_dir.join("models.json"));

    for &(id, default_url) in BUILTIN_PROVIDER_URLS {
        let api_key = extract_auth_key(&auth_json, id);
        let models = if id == "google" {
            catalog.list().to_vec()
        } else {
            Vec::new()
        };
        map.insert(
            id.to_string(),
            ProviderEndpoint {
                base_url: default_url.to_string(),
                api_key,
                models,
            },
        );
    }

    // Baca custom providers & overrides dari models.json
    if let Some(providers_obj) = models_json["providers"].as_object() {
        for (id, pval) in providers_obj {
            let base_url = pval["baseUrl"]
                .as_str()
                .map(str::to_string)
                .or_else(|| map.get(id).map(|p| p.base_url.clone()))
                .unwrap_or_else(|| DEFAULT_GEMINI_BASE_URL.to_string());
            let api_key = pval["apiKey"]
                .as_str()
                .filter(|k| !k.is_empty() && *k != "no-api-key")
                .map(str::to_string)
                .or_else(|| {
                    let k = extract_auth_key(&auth_json, id);
                    if k.is_empty() {
                        None
                    } else {
                        Some(k)
                    }
                })
                .unwrap_or_default();

            let mut parsed_models = Vec::new();
            if let Some(arr) = pval["models"].as_array() {
                for item in arr {
                    if let Some(mid) = item.as_str() {
                        parsed_models.push(ModelInfo {
                            id: mid.to_string(),
                            label: mid.to_string(),
                            provider: id.clone(),
                            supports_tools: true,
                            input_token_limit: Some(128_000),
                            output_token_limit: Some(16_384),
                            description: "Custom provider model".to_string(),
                        });
                    } else if let Some(mid) = item["id"].as_str() {
                        let label = item["name"].as_str().unwrap_or(mid).to_string();
                        parsed_models.push(ModelInfo {
                            id: mid.to_string(),
                            label,
                            provider: id.clone(),
                            supports_tools: true,
                            input_token_limit: item["contextWindow"].as_u64(),
                            output_token_limit: item["maxTokens"].as_u64(),
                            description: "Custom provider model".to_string(),
                        });
                    }
                }
            }
            if parsed_models.is_empty() {
                if let Some(existing) = map.get(id) {
                    parsed_models = existing.models.clone();
                }
            }

            map.insert(
                id.clone(),
                ProviderEndpoint {
                    base_url,
                    api_key,
                    models: parsed_models,
                },
            );
        }
    }

    // Fallback ke ~/.config/oopbot/.env atau env var GEMINI_API_KEY
    let env_key = std::env::var("GEMINI_API_KEY")
        .ok()
        .filter(|k| !k.trim().is_empty())
        .or_else(read_oopbot_env_key)
        .unwrap_or_default();
    if !env_key.is_empty() {
        if let Some(google) = map.get_mut("google") {
            if google.api_key.is_empty() {
                google.api_key = env_key;
            }
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

fn read_oopbot_env_key() -> Option<String> {
    let home = std::env::var("HOME").ok()?;
    let content = std::fs::read_to_string(PathBuf::from(home).join(".config/oopbot/.env")).ok()?;
    for line in content.lines() {
        let trimmed = line.trim();
        if let Some(rest) = trimmed.strip_prefix("GEMINI_API_KEY=") {
            let val = rest.trim().trim_matches('"').trim_matches('\'');
            if !val.is_empty() {
                return Some(val.to_string());
            }
        }
    }
    None
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
            let text = if let Some(s) = msg["content"].as_str() {
                s.to_string()
            } else if let Some(arr) = msg["content"].as_array() {
                arr.iter()
                    .filter_map(|b| b["text"].as_str())
                    .collect::<Vec<_>>()
                    .join("\n")
            } else {
                String::new()
            };
            if text.is_empty() {
                None
            } else {
                Some(user_text(&text))
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
}
