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
    default_model: RwLock<String>,
    http: reqwest::Client,
}

impl GeminiClient {
    pub fn new(api_key: String, default_model: String) -> Result<Self, BoxError> {
        let base_url = std::env::var("OOPBOT_BASE_URL")
            .or_else(|_| std::env::var("GEMINI_BASE_URL"))
            .unwrap_or_else(|_| DEFAULT_GEMINI_BASE_URL.to_string());
        Self::new_with_base_url(api_key, default_model, Some(base_url))
    }

    pub fn new_with_base_url(
        api_key: String,
        default_model: String,
        base_url: Option<String>,
    ) -> Result<Self, BoxError> {
        let http = reqwest::Client::builder()
            .timeout(Duration::from_secs(180))
            .user_agent(concat!("oopbot/", env!("CARGO_PKG_VERSION")))
            .build()?;
        let base_url = base_url
            .filter(|u| !u.trim().is_empty())
            .unwrap_or_else(|| DEFAULT_GEMINI_BASE_URL.to_string());
        Ok(Self {
            creds: RwLock::new(Credentials { api_key, base_url }),
            default_model: RwLock::new(default_model),
            http,
        })
    }


    pub fn model(&self) -> String {
        self.default_model.read().unwrap().clone()
    }

    /// Ganti model aktif pada klien secara dinamis tanpa membuat ulang HTTP pool.
    pub fn set_default_model(&self, new_model: &str) {
        let mut m = self.default_model.write().unwrap();
        *m = new_model.to_string();
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
        let is_native_google = base_url.contains("googleapis.com") && !base_url.contains("/openai");
        let url = if is_native_google {
            format!("{}/models?pageSize=100", base_url)
        } else {
            format!("{}/models", base_url)
        };
        let provider_hint = if is_native_google {
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
            if is_native_google {
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
        let cur_model = self.model();
        self.generate_with_model(
            &cur_model,
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

        let is_native_google = base_url.contains("googleapis.com") && !base_url.contains("/openai");
        let api_kind = options.api.as_deref().unwrap_or_else(|| {
            if is_native_google {
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
            "openai-completions" => {
                if !is_native_google {
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
            "openai-responses" => {
                if !is_native_google {
                    return self
                        .generate_openai_responses(
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
                if !is_native_google && options.api.is_none() {
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
        let gemini_contents = coalesce_gemini_contents(contents);

        let mut body = json!({
            "systemInstruction": { "parts": [{ "text": system_instruction }] },
            "contents": gemini_contents,
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
            let supports_gemini_thinking = options.reasoning
                || model_lower.contains("thinking")
                || model_lower.contains("gemini-2")
                || model_lower.contains("gemini-3")
                || model_lower.contains("exp");
            if supports_gemini_thinking {
                if level == "off" {
                    // Hanya model yang bukan fixed-thinking yang mengizinkan thinkingBudget: 0.
                    if !model_lower.contains("pro") && !model_lower.contains("thinking-exp") {
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
        let mut fallback_call_seq = 0usize;
        let mut pending_fallback_ids: std::collections::VecDeque<(String, String)> =
            std::collections::VecDeque::new();

        for item in contents {
            let role = item["role"].as_str().unwrap_or("user");
            let Some(parts) = item["parts"].as_array() else {
                continue;
            };
            if role == "model" {
                let mut text_buf = String::new();
                let mut tool_calls = Vec::new();
                for part in parts {
                    if part["thought"].as_bool() == Some(true) {
                        continue;
                    }
                    if let Some(t) = part["text"].as_str() {
                        text_buf.push_str(t);
                    }
                    if let Some(fc) = part.get("functionCall") {
                        let name = fc["name"].as_str().unwrap_or_default().to_string();
                        let call_id = fc["id"]
                            .as_str()
                            .filter(|s| !s.is_empty())
                            .map(str::to_string)
                            .unwrap_or_else(|| {
                                let gen_id = format!("call_{}", fallback_call_seq);
                                fallback_call_seq += 1;
                                pending_fallback_ids.push_back((gen_id.clone(), name.clone()));
                                gen_id
                            });
                        let args_str = serde_json::to_string(fc.get("args").unwrap_or(&json!({})))
                            .unwrap_or_else(|_| "{}".into());
                        tool_calls.push(json!({
                            "id": call_id,
                            "type": "function",
                            "function": { "name": name, "arguments": args_str }
                        }));
                    }
                }
                if !text_buf.is_empty() || !tool_calls.is_empty() {
                    let mut msg = json!({ "role": "assistant" });
                    if !text_buf.is_empty() || tool_calls.is_empty() {
                        msg["content"] = json!(text_buf);
                    }
                    if !tool_calls.is_empty() {
                        msg["tool_calls"] = json!(tool_calls);
                    }
                    messages.push(msg);
                }
            } else {
                let mut user_texts = Vec::new();
                let mut multimodal_parts = Vec::new();
                let mut has_inline_image = false;

                for part in parts {
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
                        let fr_name = fr["name"].as_str().unwrap_or_default();
                        let call_id = fr["id"]
                            .as_str()
                            .filter(|s| !s.is_empty())
                            .map(str::to_string)
                            .or_else(|| take_pending_fallback_id(&mut pending_fallback_ids, fr_name))
                            .unwrap_or_else(|| {
                                let gen_id = format!("call_{}", fallback_call_seq);
                                fallback_call_seq += 1;
                                gen_id
                            });
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
                if has_inline_image || !user_texts.is_empty() {
                    push_openai_user_message(
                        &mut messages,
                        &user_texts,
                        &multimodal_parts,
                        has_inline_image,
                    );
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
    async fn generate_openai_responses(
        &self,
        api_key: &str,
        base_url: &str,
        model: &str,
        system_instruction: &str,
        contents: &[Value],
        function_declarations: &[Value],
        options: &GenerationOptions,
    ) -> Result<ModelTurn, BoxError> {
        let url = format!("{}/responses", base_url);
        let mut input_items: Vec<Value> = Vec::new();
        let mut fallback_call_seq = 0usize;
        let mut pending_fallback_ids: std::collections::VecDeque<(String, String)> =
            std::collections::VecDeque::new();

        for item in contents {
            let role = item["role"].as_str().unwrap_or("user");
            let Some(parts) = item["parts"].as_array() else {
                continue;
            };
            if role == "model" {
                let mut text_buf = String::new();
                let mut pending_calls: Vec<Value> = Vec::new();
                for part in parts {
                    if part["thought"].as_bool() == Some(true) {
                        continue;
                    }
                    if let Some(t) = part["text"].as_str() {
                        text_buf.push_str(t);
                    }
                    if let Some(fc) = part.get("functionCall") {
                        let name = fc["name"].as_str().unwrap_or_default().to_string();
                        let call_id = fc["id"]
                            .as_str()
                            .filter(|s| !s.is_empty())
                            .map(str::to_string)
                            .unwrap_or_else(|| {
                                let gen_id = format!("call_{}", fallback_call_seq);
                                fallback_call_seq += 1;
                                pending_fallback_ids.push_back((gen_id.clone(), name.clone()));
                                gen_id
                            });
                        let args_str = serde_json::to_string(fc.get("args").unwrap_or(&json!({})))
                            .unwrap_or_else(|_| "{}".into());
                        pending_calls.push(json!({
                            "type": "function_call",
                            "call_id": call_id,
                            "name": name,
                            "arguments": args_str,
                        }));
                    }
                }
                if !text_buf.is_empty() {
                    input_items.push(json!({
                        "type": "message",
                        "role": "assistant",
                        "content": [{ "type": "output_text", "text": text_buf }]
                    }));
                }
                input_items.extend(pending_calls);
            } else {
                let mut user_content: Vec<Value> = Vec::new();
                for part in parts {
                    if let Some(t) = part["text"].as_str() {
                        user_content.push(json!({ "type": "input_text", "text": t }));
                    }
                    if let Some(inline) = part.get("inlineData") {
                        let mime = inline["mimeType"].as_str().unwrap_or("image/jpeg");
                        let data = inline["data"].as_str().unwrap_or_default();
                        if !data.is_empty() {
                            user_content.push(json!({
                                "type": "input_image",
                                "image_url": format!("data:{};base64,{}", mime, data)
                            }));
                        }
                    }
                    if let Some(fr) = part.get("functionResponse") {
                        if !user_content.is_empty() {
                            push_responses_user_message(&mut input_items, &mut user_content);
                        }
                        let fr_name = fr["name"].as_str().unwrap_or_default();
                        let call_id = fr["id"]
                            .as_str()
                            .filter(|s| !s.is_empty())
                            .map(str::to_string)
                            .or_else(|| take_pending_fallback_id(&mut pending_fallback_ids, fr_name))
                            .unwrap_or_else(|| {
                                let gen_id = format!("call_{}", fallback_call_seq);
                                fallback_call_seq += 1;
                                gen_id
                            });
                        let output_str =
                            serde_json::to_string(fr.get("response").unwrap_or(&json!({})))
                                .unwrap_or_else(|_| "{}".into());
                        input_items.push(json!({
                            "type": "function_call_output",
                            "call_id": call_id,
                            "output": output_str
                        }));
                    }
                }
                if !user_content.is_empty() {
                    push_responses_user_message(&mut input_items, &mut user_content);
                }
            }
        }

        let mut body = json!({
            "model": model,
            "input": input_items,
        });
        if !system_instruction.trim().is_empty() {
            body["instructions"] = json!(system_instruction);
        }
        if let Some(max_tok) = options.max_tokens.filter(|&n| n > 0) {
            body["max_output_tokens"] = json!(max_tok);
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
                        body["reasoning"] = json!({ "effort": effort });
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
                        "name": decl["name"],
                        "description": decl["description"],
                        "parameters": decl["parameters"],
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
            return Err(format!("Provider Responses API {}: {}", status, msg).into());
        }

        Ok(parse_openai_responses_turn(&res_json))
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
        let mut messages: Vec<Value> = Vec::new();
        let mut fallback_call_seq = 0usize;
        let mut pending_fallback_ids: std::collections::VecDeque<(String, String)> =
            std::collections::VecDeque::new();

        for item in contents {
            let role = item["role"].as_str().unwrap_or("user");
            let Some(parts) = item["parts"].as_array() else {
                continue;
            };
            let anthropic_role = if role == "model" { "assistant" } else { "user" };
            let mut blocks = Vec::new();
            for part in parts {
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
                    let name = fc["name"].as_str().unwrap_or_default().to_string();
                    let call_id = fc["id"]
                        .as_str()
                        .filter(|s| !s.is_empty())
                        .map(str::to_string)
                        .unwrap_or_else(|| {
                            let gen_id = format!("toolu_{}", fallback_call_seq);
                            fallback_call_seq += 1;
                            pending_fallback_ids.push_back((gen_id.clone(), name.clone()));
                            gen_id
                        });
                    blocks.push(json!({
                        "type": "tool_use",
                        "id": call_id,
                        "name": name,
                        "input": fc.get("args").cloned().unwrap_or_else(|| json!({}))
                    }));
                }
                if let Some(fr) = part.get("functionResponse") {
                    let fr_name = fr["name"].as_str().unwrap_or_default();
                    let call_id = fr["id"]
                        .as_str()
                        .filter(|s| !s.is_empty())
                        .map(str::to_string)
                        .or_else(|| take_pending_fallback_id(&mut pending_fallback_ids, fr_name))
                        .unwrap_or_else(|| {
                            let gen_id = format!("toolu_{}", fallback_call_seq);
                            fallback_call_seq += 1;
                            gen_id
                        });
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
                let merge_into_last = messages
                    .last()
                    .and_then(|m| m["role"].as_str())
                    == Some(anthropic_role);
                if merge_into_last {
                    if let Some(last_content) = messages.last_mut().and_then(|m| m["content"].as_array_mut()) {
                        last_content.extend(blocks);
                    }
                } else {
                    messages.push(json!({ "role": anthropic_role, "content": blocks }));
                }
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

fn coalesce_gemini_contents(contents: &[Value]) -> Vec<Value> {
    let mut out: Vec<Value> = Vec::with_capacity(contents.len());
    for item in contents {
        let role = item["role"].as_str().unwrap_or("user");
        let norm_role = if role == "model" || role == "assistant" {
            "model"
        } else {
            "user"
        };
        let parts = item["parts"].as_array().cloned().unwrap_or_default();
        if parts.is_empty() {
            continue;
        }
        let same_as_prev = out
            .last()
            .and_then(|prev| prev["role"].as_str())
            == Some(norm_role);
        if same_as_prev {
            if let Some(prev_parts) = out.last_mut().and_then(|p| p["parts"].as_array_mut()) {
                prev_parts.extend(parts);
            }
        } else {
            out.push(json!({ "role": norm_role, "parts": parts }));
        }
    }
    out
}

fn take_pending_fallback_id(
    queue: &mut std::collections::VecDeque<(String, String)>,
    name: &str,
) -> Option<String> {
    if let Some(pos) = queue.iter().position(|(_, n)| n == name) {
        return queue.remove(pos).map(|(id, _)| id);
    }
    queue.pop_front().map(|(id, _)| id)
}

fn push_openai_user_message(
    messages: &mut Vec<Value>,
    user_texts: &[String],
    multimodal_parts: &[Value],
    has_inline_image: bool,
) {
    let can_merge = messages
        .last()
        .and_then(|m| m["role"].as_str())
        == Some("user");
    if can_merge {
        if let Some(last) = messages.last_mut() {
            if !has_inline_image && last["content"].is_string() {
                let prev = last["content"].as_str().unwrap_or_default();
                let next = user_texts.join("\n");
                let merged = if prev.is_empty() {
                    next
                } else if next.is_empty() {
                    prev.to_string()
                } else {
                    format!("{}\n{}", prev, next)
                };
                last["content"] = json!(merged);
                return;
            }
            let mut merged_parts = if let Some(arr) = last["content"].as_array() {
                arr.clone()
            } else if let Some(s) = last["content"].as_str().filter(|s| !s.is_empty()) {
                vec![json!({ "type": "text", "text": s })]
            } else {
                Vec::new()
            };
            merged_parts.extend(multimodal_parts.iter().cloned());
            last["content"] = json!(merged_parts);
            return;
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

fn push_responses_user_message(input_items: &mut Vec<Value>, user_content: &mut Vec<Value>) {
    if let Some(last) = input_items.last_mut() {
        if last["type"].as_str() == Some("message") && last["role"].as_str() == Some("user") {
            if let Some(arr) = last["content"].as_array_mut() {
                arr.append(user_content);
                return;
            }
        }
    }
    let taken = std::mem::take(user_content);
    input_items.push(json!({
        "type": "message",
        "role": "user",
        "content": taken
    }));
}

pub fn parse_openai_responses_turn(res_json: &Value) -> ModelTurn {
    let mut text_parts: Vec<String> = Vec::new();
    let mut thoughts: Vec<String> = Vec::new();
    let mut function_calls: Vec<FunctionCall> = Vec::new();
    let mut raw_parts: Vec<Value> = Vec::new();

    if let Some(output) = res_json["output"].as_array() {
        for item in output {
            match item["type"].as_str().unwrap_or_default() {
                "reasoning" => {
                    if let Some(summary) = item["summary"].as_array() {
                        for s in summary {
                            if let Some(t) = s["text"].as_str().map(str::trim).filter(|v| !v.is_empty()) {
                                thoughts.push(t.to_string());
                                raw_parts.push(json!({ "thought": true, "text": t }));
                            }
                        }
                    }
                    if let Some(content) = item["content"].as_array() {
                        for c in content {
                            if let Some(t) = c["text"].as_str().map(str::trim).filter(|v| !v.is_empty()) {
                                thoughts.push(t.to_string());
                                raw_parts.push(json!({ "thought": true, "text": t }));
                            }
                        }
                    }
                }
                "message" => {
                    if let Some(content) = item["content"].as_array() {
                        for part in content {
                            let ptype = part["type"].as_str().unwrap_or_default();
                            if ptype == "output_text" || ptype == "text" || ptype.is_empty() {
                                if let Some(t) = part["text"].as_str() {
                                    text_parts.push(t.to_string());
                                }
                            }
                        }
                    }
                }
                "function_call" => {
                    let id = item["call_id"]
                        .as_str()
                        .or_else(|| item["id"].as_str())
                        .map(str::to_string);
                    let name = item["name"].as_str().unwrap_or_default().to_string();
                    let args = match item.get("arguments") {
                        Some(Value::String(s)) => serde_json::from_str(s).unwrap_or_else(|_| json!({})),
                        Some(other) if !other.is_null() => other.clone(),
                        _ => json!({}),
                    };
                    raw_parts.push(json!({
                        "functionCall": {
                            "id": id,
                            "name": name,
                            "args": args
                        }
                    }));
                    function_calls.push(FunctionCall { id, name, args });
                }
                _ => {}
            }
        }
    }

    let text = if !text_parts.is_empty() {
        text_parts.join("").trim().to_string()
    } else {
        res_json["output_text"]
            .as_str()
            .unwrap_or_default()
            .trim()
            .to_string()
    };
    if !text.is_empty() {
        raw_parts.insert(thoughts.len(), json!({ "text": text }));
    }

    let usage = res_json.get("usage").and_then(|u| {
        let inp = u["input_tokens"]
            .as_u64()
            .or_else(|| u["prompt_tokens"].as_u64())?;
        let out = u["output_tokens"]
            .as_u64()
            .or_else(|| u["completion_tokens"].as_u64())
            .unwrap_or(0);
        Some(TokenUsage {
            input_tokens: inp,
            output_tokens: out,
        })
    });

    ModelTurn {
        raw_content: json!({
            "role": "model",
            "parts": raw_parts
        }),
        text,
        thoughts,
        function_calls,
        usage,
    }
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

    #[tokio::test]
    async fn reg006_openai_responses_routes_to_responses_endpoint_and_parses_output() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        use tokio::net::TcpListener;

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();

        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut buf = Vec::new();
            let mut chunk = [0u8; 4096];
            loop {
                let n = stream.read(&mut chunk).await.unwrap();
                if n == 0 {
                    break;
                }
                buf.extend_from_slice(&chunk[..n]);
                if let Some(pos) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
                    let head = String::from_utf8_lossy(&buf[..pos]);
                    let content_len = head
                        .lines()
                        .find_map(|l| {
                            let lower = l.to_ascii_lowercase();
                            lower
                                .strip_prefix("content-length:")
                                .and_then(|v| v.trim().parse::<usize>().ok())
                        })
                        .unwrap_or(0);
                    if buf.len() >= pos + 4 + content_len {
                        let req_body = &buf[pos + 4..pos + 4 + content_len];
                        let captured = (
                            head.to_string(),
                            serde_json::from_slice::<Value>(req_body).unwrap(),
                        );
                        let resp_body = json!({
                            "id": "resp_1",
                            "output": [
                                {
                                    "type": "reasoning",
                                    "summary": [{ "type": "summary_text", "text": "Checking files first" }]
                                },
                                {
                                    "type": "message",
                                    "role": "assistant",
                                    "content": [{ "type": "output_text", "text": "Running tool now." }]
                                },
                                {
                                    "type": "function_call",
                                    "call_id": "fc_99",
                                    "name": "read_file",
                                    "arguments": "{\"path\":\"Cargo.toml\"}"
                                }
                            ],
                            "usage": { "input_tokens": 88, "output_tokens": 24 }
                        })
                        .to_string();
                        let http_resp = format!(
                            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                            resp_body.len(),
                            resp_body
                        );
                        stream.write_all(http_resp.as_bytes()).await.unwrap();
                        return captured;
                    }
                }
            }
            panic!("Incomplete HTTP request received");
        });

        let client = GeminiClient::new_with_base_url(
            "sk-test".into(),
            "gpt-5.3-codex".into(),
            Some(format!("http://{}/v1", addr)),
        )
        .unwrap();

        let contents = vec![
            user_message_with_images("Check image", &[("image/png".into(), "Zm9v".into())]),
            json!({
                "role": "model",
                "parts": [
                    { "thought": true, "text": "old thought" },
                    { "text": "Calling list" },
                    { "functionCall": { "id": "call_prev", "name": "list_dir", "args": { "path": "." } } }
                ]
            }),
            json!({
                "role": "user",
                "parts": [
                    function_response_part(
                        &FunctionCall { id: Some("call_prev".into()), name: "list_dir".into(), args: json!({}) },
                        json!({ "files": ["Cargo.toml"] })
                    )
                ]
            }),
        ];
        let decls = vec![json!({
            "name": "read_file",
            "description": "Read a file",
            "parameters": { "type": "object" }
        })];
        let opts = GenerationOptions {
            api: Some("openai-responses".into()),
            max_tokens: Some(2048),
            thinking_level: Some("high".into()),
            reasoning: true,
            thinking_level_map: None,
            session_id: Some("sess-resp-1".into()),
        };

        let turn = client
            .generate_with_options("gpt-5.3-codex", "System guide", &contents, &decls, &opts)
            .await
            .unwrap();

        let (head, req_json) = server.await.unwrap();
        assert!(head.starts_with("POST /v1/responses HTTP/1.1"), "unexpected request head: {}", head);
        assert!(head.to_ascii_lowercase().contains("authorization: bearer sk-test"));
        assert!(head.to_ascii_lowercase().contains("x-session-affinity: sess-resp-1"));
        assert_eq!(req_json["model"], "gpt-5.3-codex");
        assert_eq!(req_json["instructions"], "System guide");
        assert_eq!(req_json["max_output_tokens"], 2048);
        assert_eq!(req_json["tools"][0]["type"], "function");
        assert_eq!(req_json["tools"][0]["name"], "read_file");
        assert_eq!(req_json["input"][0]["content"][0]["type"], "input_text");
        assert_eq!(req_json["input"][0]["content"][1]["type"], "input_image");
        assert_eq!(req_json["input"][1]["type"], "message");
        assert_eq!(req_json["input"][1]["content"][0]["type"], "output_text");
        assert_eq!(req_json["input"][2]["type"], "function_call");
        assert_eq!(req_json["input"][2]["call_id"], "call_prev");
        assert_eq!(req_json["input"][3]["type"], "function_call_output");
        assert_eq!(req_json["input"][3]["call_id"], "call_prev");

        assert_eq!(turn.thoughts, vec!["Checking files first"]);
        assert_eq!(turn.text, "Running tool now.");
        assert_eq!(turn.function_calls.len(), 1);
        assert_eq!(turn.function_calls[0].id.as_deref(), Some("fc_99"));
        assert_eq!(turn.function_calls[0].name, "read_file");
        assert_eq!(turn.function_calls[0].args["path"], "Cargo.toml");
        assert_eq!(turn.usage.unwrap().input_tokens, 88);
        assert_eq!(turn.usage.unwrap().output_tokens, 24);
    }
}

