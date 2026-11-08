//! Klien HTTP multi-protokol untuk Gemini API, OpenAI-compatible (OpenRouter, Z.ai, Kimi,
//! Local Server `pipkin-local`), dan Anthropic Messages API dengan:
//! - Function/Tool Calling lintas provider
//! - Kontrol penuh parameter inferensi dari aplikasi Android (`GenerationOptions`:
//!   `api`, `max_tokens`, `thinking_level`, `reasoning`, `thinking_level_map`, `session_id`)
//! - Dukungan input multimodal (`inlineData` gambar base64 dari composer aplikasi)
//! - Ekstraksi `thoughts` (`thinking_start`/`thinking_end`) & penggunaan token (`TokenUsage`)
//! - Pergantian token (`api_key`) & `base_url` secara dinamis tanpa restart
//! - Auto-scan daftar model (`scan_models`) langsung dari token provider

use crate::models::{parse_models_response, ModelInfo};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::sync::RwLock;
use std::time::Duration;

pub type BoxError = Box<dyn std::error::Error + Send + Sync>;

pub const DEFAULT_GEMINI_BASE_URL: &str = "https://generativelanguage.googleapis.com/v1beta";

/// Satu panggilan fungsi yang diminta model.
#[derive(Debug, Clone)]
pub struct FunctionCall {
    pub id: Option<String>,
    pub name: String,
    pub args: Value,
}

/// Statistik penggunaan token aktual dari satu giliran model.
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize)]
pub struct TokenUsage {
    pub input_tokens: u64,
    pub output_tokens: u64,
}

/// Opsi inferensi yang dikontrol langsung dari aplikasi Android / konfigurasi model.
#[derive(Debug, Clone)]
pub struct GenerationOptions {
    /// Protokol API (`google-generative-ai`, `openai-completions`, `openai-responses`, `anthropic-messages`).
    pub api: Option<String>,
    /// Batas token output maksimum (`maxTokens` dari pengaturan model aplikasi).
    pub max_tokens: Option<u64>,
    /// Level penalaran aktif (`off`, `minimal`, `low`, `medium`, `high`, `xhigh`).
    pub thinking_level: Option<String>,
    /// Apakah model mendukung penalaran (`reasoning`).
    pub reasoning: bool,
    /// Pemetaan level penalaran ke string effort provider (`thinkingLevelMap`).
    pub thinking_level_map: Option<BTreeMap<String, Option<String>>>,
    /// ID sesi untuk header `x-session-affinity` (digunakan oleh `LocalModelServer` / `pipkin-local`).
    pub session_id: Option<String>,
}

impl Default for GenerationOptions {
    fn default() -> Self {
        Self {
            api: None,
            max_tokens: None,
            thinking_level: None,
            reasoning: true,
            thinking_level_map: None,
            session_id: None,
        }
    }
}

/// Hasil satu giliran model: konten mentah (untuk disimpan ke memori),
/// teks yang bisa ditampilkan, ringkasan penalaran (`thoughts`), daftar function call,
/// dan penggunaan token (`usage`).
#[derive(Debug, Clone)]
pub struct ModelTurn {
    pub raw_content: Value,
    pub text: String,
    pub thoughts: Vec<String>,
    pub function_calls: Vec<FunctionCall>,
    pub usage: Option<TokenUsage>,
}

#[derive(Debug, Clone)]
struct Credentials {
    api_key: String,
    base_url: String,
}

pub struct GeminiClient {
    creds: RwLock<Credentials>,
    default_model: String,
    http: reqwest::Client,
}

impl GeminiClient {
    pub fn new(api_key: String, default_model: String) -> Result<Self, BoxError> {
        let http = reqwest::Client::builder()
            .timeout(Duration::from_secs(180))
            .user_agent(concat!("oopbot/", env!("CARGO_PKG_VERSION")))
            .build()?;
        let base_url = std::env::var("GEMINI_BASE_URL")
            .unwrap_or_else(|_| DEFAULT_GEMINI_BASE_URL.to_string());
        Ok(Self {
            creds: RwLock::new(Credentials { api_key, base_url }),
            default_model,
            http,
        })
    }

    pub fn model(&self) -> &str {
        &self.default_model
    }

    pub fn base_url(&self) -> String {
        self.creds.read().unwrap().base_url.clone()
    }

    pub fn api_key(&self) -> String {
        self.creds.read().unwrap().api_key.clone()
    }

    /// Ganti API key (dan opsional `base_url`) secara dinamis di memori.
    pub fn update_credentials(&self, api_key: String, base_url: Option<String>) {
        let mut creds = self.creds.write().unwrap();
        creds.api_key = api_key;
        if let Some(url) = base_url.filter(|u| !u.trim().is_empty()) {
            creds.base_url = url.trim_end_matches('/').to_string();
        }
    }

    /// Scan daftar model yang tersedia untuk token saat ini dari `<base_url>/models`.
    /// Mengirimkan `x-goog-api-key` untuk Google Gemini, atau `Authorization: Bearer`
    /// untuk provider OpenAI-compatible (OpenRouter, Groq, DeepSeek, dsb).
    pub async fn scan_models(&self) -> Result<Vec<ModelInfo>, BoxError> {
        let (api_key, base_url) = {
            let c = self.creds.read().unwrap();
            (c.api_key.clone(), c.base_url.trim_end_matches('/').to_string())
        };
        let url = if base_url.contains("googleapis.com") {
            format!("{}/models?pageSize=100", base_url)
        } else {
            format!("{}/models", base_url)
        };
        let provider_hint = if base_url.contains("googleapis.com") {
            "google"
        } else if base_url.contains("openrouter.ai") {
            "openrouter"
        } else if base_url.contains("z.ai") {
            "zai"
        } else if base_url.contains("kimi.com") {
            "kimi-coding"
        } else if base_url.contains("anthropic.com") {
            "anthropic"
        } else {
            "custom"
        };

        let mut req = self.http.get(&url).timeout(Duration::from_secs(20));
        if !api_key.is_empty() {
            if base_url.contains("googleapis.com") {
                req = req.header("x-goog-api-key", &api_key);
            } else {
                req = req.header("Authorization", format!("Bearer {}", api_key));
            }
        }
        let resp = req.send().await?;

        let status = resp.status();
        let res_json: Value = resp.json().await?;
        if !status.is_success() {
            let msg = res_json["error"]["message"]
                .as_str()
                .or_else(|| res_json["message"].as_str())
                .map(str::to_string)
                .unwrap_or_else(|| res_json.to_string());
            return Err(format!("Gagal scan /models ({}): {}", status, msg).into());
        }

        let models = parse_models_response(&res_json, provider_hint);
        if models.is_empty() {
            return Err("Server /models tidak mengembalikan model yang kompatibel".into());
        }
        Ok(models)
    }

    /// Kirim seluruh riwayat + deklarasi tool dengan model default.
    pub async fn generate(
        &self,
        system_instruction: &str,
        contents: &[Value],
        function_declarations: &[Value],
    ) -> Result<ModelTurn, BoxError> {
        self.generate_with_model(
            &self.default_model,
            system_instruction,
            contents,
            function_declarations,
        )
        .await
    }

    /// Kirim seluruh riwayat + deklarasi tool untuk model tertentu dengan opsi default.
    pub async fn generate_with_model(
        &self,
        model: &str,
        system_instruction: &str,
        contents: &[Value],
        function_declarations: &[Value],
    ) -> Result<ModelTurn, BoxError> {
        self.generate_with_options(
            model,
            system_instruction,
            contents,
            function_declarations,
            &GenerationOptions::default(),
        )
        .await
    }

    /// Kirim seluruh riwayat + deklarasi tool dengan opsi inferensi penuh dari aplikasi.
    pub async fn generate_with_options(
        &self,
        model: &str,
        system_instruction: &str,
        contents: &[Value],
        function_declarations: &[Value],
        options: &GenerationOptions,
    ) -> Result<ModelTurn, BoxError> {
        let (api_key, base_url) = {
            let c = self.creds.read().unwrap();
            (c.api_key.clone(), c.base_url.trim_end_matches('/').to_string())
        };

        let api_kind = options.api.as_deref().unwrap_or_else(|| {
            if base_url.contains("googleapis.com") {
                "google-generative-ai"
            } else if base_url.contains("anthropic.com") {
                "anthropic-messages"
            } else {
                "openai-completions"
            }
        });

        match api_kind {
            "anthropic-messages" => {
                return self
                    .generate_anthropic(
                        &api_key,
                        &base_url,
                        model,
                        system_instruction,
                        contents,
                        function_declarations,
                        options,
                    )
                    .await;
            }
            "openai-completions" | "openai-responses" => {
                if !base_url.contains("googleapis.com") {
                    return self
                        .generate_openai_compatible(
                            &api_key,
                            &base_url,
                            model,
                            system_instruction,
                            contents,
                            function_declarations,
                            options,
                        )
                        .await;
                }
            }
            _ => {
                if !base_url.contains("googleapis.com") && options.api.is_none() {
                    return self
                        .generate_openai_compatible(
                            &api_key,
                            &base_url,
                            model,
                            system_instruction,
                            contents,
                            function_declarations,
                            options,
                        )
                        .await;
                }
            }
        }

        let clean_model = model.strip_prefix("models/").unwrap_or(model);
        let url = format!("{}/models/{}:generateContent", base_url, clean_model);

        let mut body = json!({
            "systemInstruction": { "parts": [{ "text": system_instruction }] },
            "contents": contents,
        });
        if !function_declarations.is_empty() {
            body["tools"] = json!([{ "functionDeclarations": function_declarations }]);
        }

        let mut gen_config = serde_json::Map::new();
        if let Some(max_tok) = options.max_tokens.filter(|&n| n > 0) {
            gen_config.insert("maxOutputTokens".to_string(), json!(max_tok));
        }
        if let Some(level) = options.thinking_level.as_deref() {
            let model_lower = clean_model.to_ascii_lowercase();
            let supports_gemini_thinking =
                options.reasoning && (model_lower.contains("gemini-2.5") || model_lower.contains("gemini-3"));
            if supports_gemini_thinking {
                if level == "off" {
                    // Gemini 2.5 Pro tidak mengizinkan budget 0, hanya Flash/Flash-Lite yang mengizinkan 0.
                    if !model_lower.contains("pro") {
                        gen_config.insert(
                            "thinkingConfig".to_string(),
                            json!({ "thinkingBudget": 0 }),
                        );
                    }
                } else if let Some(budget) = gemini_thinking_budget(level) {
                    gen_config.insert(
                        "thinkingConfig".to_string(),
                        json!({
                            "includeThoughts": true,
                            "thinkingBudget": budget
                        }),
                    );
                }
            }
        }
        if !gen_config.is_empty() {
            body["generationConfig"] = Value::Object(gen_config);
        }

        // API key lewat header, bukan query string, supaya tidak bocor di log/error.
        // PENTING: Untuk Google Generative Language API, kirimkan HANYA `x-goog-api-key`.
        // Jangan sertakan `Authorization: Bearer` karena Google API Gateway akan menganggapnya
        // sebagai OAuth2 access token dan memicu error 401 "API keys are not supported by this API".
        let resp = self
            .http
            .post(&url)
            .header("x-goog-api-key", &api_key)
            .json(&body)
            .send()
            .await?;

        let status = resp.status();
        let res_json: Value = resp.json().await?;
        if !status.is_success() {
            let msg = res_json["error"]["message"]
                .as_str()
                .map(str::to_string)
                .unwrap_or_else(|| res_json.to_string());
            return Err(format!("Gemini API {}: {}", status, msg).into());
        }

        parse_model_turn(&res_json)
    }

    #[allow(clippy::too_many_arguments)]
    async fn generate_openai_compatible(
        &self,
        api_key: &str,
        base_url: &str,
        model: &str,
        system_instruction: &str,
        contents: &[Value],
        function_declarations: &[Value],
        options: &GenerationOptions,
    ) -> Result<ModelTurn, BoxError> {
        let url = format!("{}/chat/completions", base_url);
        let mut messages = vec![json!({ "role": "system", "content": system_instruction })];

        for item in contents {
            let role = item["role"].as_str().unwrap_or("user");
            let Some(parts) = item["parts"].as_array() else {
                continue;
            };
            if role == "model" {
                let mut text_buf = String::new();
                let mut tool_calls = Vec::new();
                for (idx, part) in parts.iter().enumerate() {
                    if part["thought"].as_bool() == Some(true) {
                        continue;
                    }
                    if let Some(t) = part["text"].as_str() {
                        text_buf.push_str(t);
                    }
                    if let Some(fc) = part.get("functionCall") {
                        let call_id = fc["id"]
                            .as_str()
                            .map(str::to_string)
                            .unwrap_or_else(|| format!("call_{}", idx));
                        let name = fc["name"].as_str().unwrap_or_default();
                        let args_str = serde_json::to_string(fc.get("args").unwrap_or(&json!({})))
                            .unwrap_or_else(|_| "{}".into());
                        tool_calls.push(json!({
                            "id": call_id,
                            "type": "function",
                            "function": { "name": name, "arguments": args_str }
                        }));
                    }
                }
                let mut msg = json!({ "role": "assistant" });
                if !text_buf.is_empty() || tool_calls.is_empty() {
                    msg["content"] = json!(text_buf);
                }
                if !tool_calls.is_empty() {
                    msg["tool_calls"] = json!(tool_calls);
                }
                messages.push(msg);
            } else {
                let mut user_texts = Vec::new();
                let mut multimodal_parts = Vec::new();
                let mut has_inline_image = false;

                for (idx, part) in parts.iter().enumerate() {
                    if let Some(t) = part["text"].as_str() {
                        user_texts.push(t.to_string());
                        multimodal_parts.push(json!({ "type": "text", "text": t }));
                    }
                    if let Some(inline) = part.get("inlineData") {
                        let mime = inline["mimeType"].as_str().unwrap_or("image/jpeg");
                        let data = inline["data"].as_str().unwrap_or_default();
                        if !data.is_empty() {
                            has_inline_image = true;
                            multimodal_parts.push(json!({
                                "type": "image_url",
                                "image_url": {
                                    "url": format!("data:{};base64,{}", mime, data)
                                }
                            }));
                        }
                    }
                    if let Some(fr) = part.get("functionResponse") {
                        let call_id = fr["id"]
                            .as_str()
                            .map(str::to_string)
                            .unwrap_or_else(|| format!("call_{}", idx));
                        let content_str =
                            serde_json::to_string(fr.get("response").unwrap_or(&json!({})))
                                .unwrap_or_else(|_| "{}".into());
                        messages.push(json!({
                            "role": "tool",
                            "tool_call_id": call_id,
                            "content": content_str
                        }));
                    }
                }
                if has_inline_image {
                    messages.push(json!({
                        "role": "user",
                        "content": multimodal_parts
                    }));
                } else if !user_texts.is_empty() {
                    messages.push(json!({
                        "role": "user",
                        "content": user_texts.join("\n")
                    }));
                }
            }
        }

        let mut body = json!({
            "model": model,
            "messages": messages,
        });
        if let Some(max_tok) = options.max_tokens.filter(|&n| n > 0) {
            body["max_tokens"] = json!(max_tok);
        }
        let is_local = base_url.contains("127.0.0.1") || base_url.contains("localhost");
        if options.reasoning {
            if let Some(level) = options.thinking_level.as_deref() {
                if level != "off" {
                    let mapped = options
                        .thinking_level_map
                        .as_ref()
                        .and_then(|m| m.get(level))
                        .and_then(|v| v.clone())
                        .or_else(|| {
                            if is_local {
                                None
                            } else {
                                Some(
                                    match level {
                                        "minimal" | "low" => "low",
                                        "medium" => "medium",
                                        "high" | "xhigh" => "high",
                                        other => other,
                                    }
                                    .to_string(),
                                )
                            }
                        });
                    if let Some(effort) = mapped {
                        body["reasoning_effort"] = json!(effort);
                    }
                }
            }
        }

        if !function_declarations.is_empty() {
            let tools: Vec<Value> = function_declarations
                .iter()
                .map(|decl| {
                    json!({
                        "type": "function",
                        "function": {
                            "name": decl["name"],
                            "description": decl["description"],
                            "parameters": decl["parameters"]
                        }
                    })
                })
                .collect();
            body["tools"] = json!(tools);
        }

        let mut req = self.http.post(&url);
        if !api_key.is_empty() {
            req = req.header("Authorization", format!("Bearer {}", api_key));
        }
        if let Some(sid) = options.session_id.as_deref().filter(|s| !s.is_empty()) {
            req = req.header("x-session-affinity", sid);
        }
        let resp = req.json(&body).send().await?;

        let status = resp.status();
        let res_json: Value = resp.json().await?;
        if !status.is_success() {
            let msg = res_json["error"]["message"]
                .as_str()
                .or_else(|| res_json["message"].as_str())
                .map(str::to_string)
                .unwrap_or_else(|| res_json.to_string());
            return Err(format!("Provider API {}: {}", status, msg).into());
        }

        let message = &res_json["choices"][0]["message"];
        let text = message["content"].as_str().unwrap_or_default().trim().to_string();
        let mut thoughts = Vec::new();
        if let Some(rc) = message["reasoning_content"]
            .as_str()
            .or_else(|| message["reasoning"].as_str())
            .map(str::trim)
            .filter(|s| !s.is_empty())
        {
            thoughts.push(rc.to_string());
        }

        let mut function_calls = Vec::new();
        let mut raw_parts = Vec::new();
        for t in &thoughts {
            raw_parts.push(json!({ "thought": true, "text": t }));
        }
        if !text.is_empty() {
            raw_parts.push(json!({ "text": text }));
        }
        if let Some(tcs) = message["tool_calls"].as_array() {
            for tc in tcs {
                let id = tc["id"].as_str().map(str::to_string);
                let name = tc["function"]["name"].as_str().unwrap_or_default().to_string();
                let args_str = tc["function"]["arguments"].as_str().unwrap_or("{}");
                let args: Value = serde_json::from_str(args_str).unwrap_or_else(|_| json!({}));
                raw_parts.push(json!({
                    "functionCall": {
                        "id": id,
                        "name": name,
                        "args": args
                    }
                }));
                function_calls.push(FunctionCall { id, name, args });
            }
        }

        let usage = res_json.get("usage").and_then(|u| {
            let inp = u["prompt_tokens"].as_u64().or_else(|| u["input_tokens"].as_u64())?;
            let out = u["completion_tokens"]
                .as_u64()
                .or_else(|| u["output_tokens"].as_u64())
                .unwrap_or(0);
            Some(TokenUsage {
                input_tokens: inp,
                output_tokens: out,
            })
        });

        Ok(ModelTurn {
            raw_content: json!({ "role": "model", "parts": raw_parts }),
            text,
            thoughts,
            function_calls,
            usage,
        })
    }

    #[allow(clippy::too_many_arguments)]
    async fn generate_anthropic(
        &self,
        api_key: &str,
        base_url: &str,
        model: &str,
        system_instruction: &str,
        contents: &[Value],
        function_declarations: &[Value],
        options: &GenerationOptions,
    ) -> Result<ModelTurn, BoxError> {
        let url = format!("{}/messages", base_url.trim_end_matches('/'));
        let mut messages = Vec::new();

        for item in contents {
            let role = item["role"].as_str().unwrap_or("user");
            let Some(parts) = item["parts"].as_array() else {
                continue;
            };
            let anthropic_role = if role == "model" { "assistant" } else { "user" };
            let mut blocks = Vec::new();
            for (idx, part) in parts.iter().enumerate() {
                if part["thought"].as_bool() == Some(true) {
                    continue;
                }
                if let Some(t) = part["text"].as_str().filter(|s| !s.is_empty()) {
                    blocks.push(json!({ "type": "text", "text": t }));
                }
                if let Some(inline) = part.get("inlineData") {
                    let mime = inline["mimeType"].as_str().unwrap_or("image/jpeg");
                    let data = inline["data"].as_str().unwrap_or_default();
                    if !data.is_empty() {
                        blocks.push(json!({
                            "type": "image",
                            "source": {
                                "type": "base64",
                                "media_type": mime,
                                "data": data
                            }
                        }));
                    }
                }
                if let Some(fc) = part.get("functionCall") {
                    let call_id = fc["id"]
                        .as_str()
                        .map(str::to_string)
                        .unwrap_or_else(|| format!("toolu_{}", idx));
                    blocks.push(json!({
                        "type": "tool_use",
                        "id": call_id,
                        "name": fc["name"].as_str().unwrap_or_default(),
                        "input": fc.get("args").cloned().unwrap_or_else(|| json!({}))
                    }));
                }
                if let Some(fr) = part.get("functionResponse") {
                    let call_id = fr["id"]
                        .as_str()
                        .map(str::to_string)
                        .unwrap_or_else(|| format!("toolu_{}", idx));
                    let content_str =
                        serde_json::to_string(fr.get("response").unwrap_or(&json!({})))
                            .unwrap_or_else(|_| "{}".into());
                    blocks.push(json!({
                        "type": "tool_result",
                        "tool_use_id": call_id,
                        "content": content_str
                    }));
                }
            }
            if !blocks.is_empty() {
                messages.push(json!({ "role": anthropic_role, "content": blocks }));
            }
        }

        let max_tokens = options.max_tokens.filter(|&n| n > 0).unwrap_or(8_192);
        let mut body = json!({
            "model": model,
            "max_tokens": max_tokens,
            "system": system_instruction,
            "messages": messages,
        });
        if !function_declarations.is_empty() {
            let tools: Vec<Value> = function_declarations
                .iter()
                .map(|decl| {
                    json!({
                        "name": decl["name"],
                        "description": decl["description"],
                        "input_schema": decl["parameters"]
                    })
                })
                .collect();
            body["tools"] = json!(tools);
        }

        let resp = self
            .http
            .post(&url)
            .header("x-api-key", api_key)
            .header("anthropic-version", "2023-06-01")
            .json(&body)
            .send()
            .await?;

        let status = resp.status();
        let res_json: Value = resp.json().await?;
        if !status.is_success() {
            let msg = res_json["error"]["message"]
                .as_str()
                .map(str::to_string)
                .unwrap_or_else(|| res_json.to_string());
            return Err(format!("Anthropic API {}: {}", status, msg).into());
        }

        let mut text = String::new();
        let mut thoughts = Vec::new();
        let mut function_calls = Vec::new();
        let mut raw_parts = Vec::new();

        if let Some(blocks) = res_json["content"].as_array() {
            for b in blocks {
                match b["type"].as_str().unwrap_or_default() {
                    "thinking" => {
                        if let Some(t) = b["thinking"].as_str().map(str::trim).filter(|s| !s.is_empty()) {
                            thoughts.push(t.to_string());
                            raw_parts.push(json!({ "thought": true, "text": t }));
                        }
                    }
                    "text" => {
                        if let Some(t) = b["text"].as_str() {
                            text.push_str(t);
                            raw_parts.push(json!({ "text": t }));
                        }
                    }
                    "tool_use" => {
                        let id = b["id"].as_str().map(str::to_string);
                        let name = b["name"].as_str().unwrap_or_default().to_string();
                        let args = b.get("input").cloned().unwrap_or_else(|| json!({}));
                        raw_parts.push(json!({
                            "functionCall": { "id": id, "name": name, "args": args }
                        }));
                        function_calls.push(FunctionCall { id, name, args });
                    }
                    _ => {}
                }
            }
        }

        let usage = res_json.get("usage").and_then(|u| {
            let inp = u["input_tokens"].as_u64()?;
            let out = u["output_tokens"].as_u64().unwrap_or(0);
            Some(TokenUsage {
                input_tokens: inp,
                output_tokens: out,
            })
        });

        Ok(ModelTurn {
            raw_content: json!({ "role": "model", "parts": raw_parts }),
            text: text.trim().to_string(),
            thoughts,
            function_calls,
            usage,
        })
    }
}

fn gemini_thinking_budget(level: &str) -> Option<u64> {
    match level {
        "off" => Some(0),
        "minimal" => Some(512),
        "low" => Some(1_024),
        "medium" => Some(8_192),
        "high" => Some(24_576),
        "xhigh" => Some(32_768),
        _ => None,
    }
}

/// Ekstrak giliran model dari respons `generateContent`.
pub fn parse_model_turn(res_json: &Value) -> Result<ModelTurn, BoxError> {
    let candidate = &res_json["candidates"][0];
    let content = &candidate["content"];
    if content.is_null() {
        let reason = candidate["finishReason"]
            .as_str()
            .or_else(|| res_json["promptFeedback"]["blockReason"].as_str())
            .unwrap_or("tidak diketahui");
        return Err(format!("Model tidak mengembalikan konten (alasan: {})", reason).into());
    }

    let mut text = String::new();
    let mut thoughts = Vec::new();
    let mut function_calls = Vec::new();
    if let Some(parts) = content["parts"].as_array() {
        for part in parts {
            // Simpan "thought summary" ke `thoughts` untuk ditampilkan di StepFigure aplikasi.
            if part["thought"].as_bool() == Some(true) {
                if let Some(t) = part["text"].as_str().map(str::trim).filter(|s| !s.is_empty()) {
                    thoughts.push(t.to_string());
                }
                continue;
            }
            if let Some(t) = part["text"].as_str() {
                text.push_str(t);
            }
            if let Some(fc) = part.get("functionCall") {
                function_calls.push(FunctionCall {
                    id: fc["id"].as_str().map(str::to_string),
                    name: fc["name"].as_str().unwrap_or_default().to_string(),
                    args: fc.get("args").cloned().unwrap_or_else(|| json!({})),
                });
            }
        }
    }

    // Pastikan role selalu ada saat dikirim balik sebagai riwayat.
    let mut raw_content = content.clone();
    if raw_content.get("role").is_none() {
        raw_content["role"] = json!("model");
    }

    let usage = res_json.get("usageMetadata").and_then(|u| {
        let inp = u["promptTokenCount"].as_u64()?;
        let out = u["candidatesTokenCount"].as_u64().unwrap_or(0)
            + u["thoughtsTokenCount"].as_u64().unwrap_or(0);
        Some(TokenUsage {
            input_tokens: inp,
            output_tokens: out,
        })
    });

    Ok(ModelTurn {
        raw_content,
        text: text.trim().to_string(),
        thoughts,
        function_calls,
        usage,
    })
}

/// Bangun konten `user` berisi teks biasa.
pub fn user_text(text: &str) -> Value {
    json!({ "role": "user", "parts": [{ "text": text }] })
}

/// Bangun konten `user` berisi teks dan daftar gambar inline (`(mime_type, base64_data)`).
pub fn user_message_with_images(text: &str, images: &[(String, String)]) -> Value {
    let mut parts = Vec::new();
    if !text.is_empty() {
        parts.push(json!({ "text": text }));
    }
    for (mime, data) in images {
        if !data.is_empty() {
            parts.push(json!({
                "inlineData": {
                    "mimeType": mime,
                    "data": data
                }
            }));
        }
    }
    if parts.is_empty() {
        parts.push(json!({ "text": "" }));
    }
    json!({ "role": "user", "parts": parts })
}

/// Bangun satu part `functionResponse`.
pub fn function_response_part(call: &FunctionCall, response: Value) -> Value {
    let mut fr = json!({ "name": call.name, "response": response });
    if let Some(id) = &call.id {
        fr["id"] = json!(id);
    }
    json!({ "functionResponse": fr })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_text_thoughts_usage_and_function_calls() {
        let res = json!({
            "candidates": [{
                "content": {
                    "role": "model",
                    "parts": [
                        { "thought": true, "text": "Menganalisis direktori kerja..." },
                        { "text": "Saya cek dulu." },
                        { "functionCall": { "name": "list_dir", "args": { "path": "." } },
                          "thoughtSignature": "abc" }
                    ]
                }
            }],
            "usageMetadata": {
                "promptTokenCount": 120,
                "candidatesTokenCount": 45,
                "thoughtsTokenCount": 15
            }
        });
        let turn = parse_model_turn(&res).unwrap();
        assert_eq!(turn.text, "Saya cek dulu.");
        assert_eq!(turn.thoughts, vec!["Menganalisis direktori kerja...".to_string()]);
        assert_eq!(turn.function_calls.len(), 1);
        assert_eq!(turn.function_calls[0].name, "list_dir");
        // thoughtSignature harus tetap ada di raw content
        assert_eq!(turn.raw_content["parts"][2]["thoughtSignature"], "abc");
        let usage = turn.usage.unwrap();
        assert_eq!(usage.input_tokens, 120);
        assert_eq!(usage.output_tokens, 60);
    }

    #[test]
    fn reports_blocked_response() {
        let res = json!({ "promptFeedback": { "blockReason": "SAFETY" } });
        let err = parse_model_turn(&res).unwrap_err().to_string();
        assert!(err.contains("SAFETY"));
    }

    #[test]
    fn function_response_keeps_id() {
        let call = FunctionCall {
            id: Some("x1".into()),
            name: "read_file".into(),
            args: json!({}),
        };
        let part = function_response_part(&call, json!({ "ok": true }));
        assert_eq!(part["functionResponse"]["id"], "x1");
        assert_eq!(part["functionResponse"]["name"], "read_file");
    }
}

