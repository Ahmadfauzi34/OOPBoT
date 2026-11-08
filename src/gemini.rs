//! Klien HTTP untuk Gemini API & OpenAI-compatible Provider dengan:
//! - Function/Tool Calling
//! - Pergantian token (`api_key`) & `base_url` secara dinamis tanpa restart
//! - Auto-scan daftar model (`scan_models`) langsung dari token provider

use crate::models::{parse_models_response, ModelInfo};
use serde_json::{json, Value};
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

/// Hasil satu giliran model: konten mentah (untuk disimpan ke memori),
/// teks yang bisa ditampilkan, dan daftar function call.
#[derive(Debug, Clone)]
pub struct ModelTurn {
    pub raw_content: Value,
    pub text: String,
    pub function_calls: Vec<FunctionCall>,
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
    /// Mengirimkan `x-goog-api-key` (untuk Gemini) sekaligus `Authorization: Bearer`
    /// (untuk provider OpenAI-compatible seperti OpenRouter/Groq/DeepSeek).
    pub async fn scan_models(&self) -> Result<Vec<ModelInfo>, BoxError> {
        let (api_key, base_url) = {
            let c = self.creds.read().unwrap();
            (c.api_key.clone(), c.base_url.trim_end_matches('/').to_string())
        };
        let url = format!("{}/models?pageSize=100", base_url);
        let provider_hint = if base_url.contains("googleapis.com") {
            "gemini"
        } else {
            "custom"
        };

        let resp = self
            .http
            .get(&url)
            .header("x-goog-api-key", &api_key)
            .header("Authorization", format!("Bearer {}", api_key))
            .timeout(Duration::from_secs(20))
            .send()
            .await?;

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

    /// Kirim seluruh riwayat + deklarasi tool untuk model tertentu (mendukung Gemini & OpenAI-compatible).
    pub async fn generate_with_model(
        &self,
        model: &str,
        system_instruction: &str,
        contents: &[Value],
        function_declarations: &[Value],
    ) -> Result<ModelTurn, BoxError> {
        let (api_key, base_url) = {
            let c = self.creds.read().unwrap();
            (c.api_key.clone(), c.base_url.trim_end_matches('/').to_string())
        };
        if !base_url.contains("googleapis.com") {
            return self
                .generate_openai_compatible(
                    &api_key,
                    &base_url,
                    model,
                    system_instruction,
                    contents,
                    function_declarations,
                )
                .await;
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

        // API key lewat header, bukan query string, supaya tidak bocor di log/error.
        let resp = self
            .http
            .post(&url)
            .header("x-goog-api-key", &api_key)
            .header("Authorization", format!("Bearer {}", api_key))
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

    async fn generate_openai_compatible(
        &self,
        api_key: &str,
        base_url: &str,
        model: &str,
        system_instruction: &str,
        contents: &[Value],
        function_declarations: &[Value],
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
                for (idx, part) in parts.iter().enumerate() {
                    if let Some(t) = part["text"].as_str() {
                        user_texts.push(t.to_string());
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
                if !user_texts.is_empty() {
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

        let resp = self
            .http
            .post(&url)
            .header("Authorization", format!("Bearer {}", api_key))
            .json(&body)
            .send()
            .await?;

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
        let mut function_calls = Vec::new();
        let mut raw_parts = Vec::new();
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

        Ok(ModelTurn {
            raw_content: json!({ "role": "model", "parts": raw_parts }),
            text,
            function_calls,
        })
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
    let mut function_calls = Vec::new();
    if let Some(parts) = content["parts"].as_array() {
        for part in parts {
            // Lewati "thought summary" bila ada.
            if part["thought"].as_bool() == Some(true) {
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

    Ok(ModelTurn {
        raw_content,
        text: text.trim().to_string(),
        function_calls,
    })
}

/// Bangun konten `user` berisi teks biasa.
pub fn user_text(text: &str) -> Value {
    json!({ "role": "user", "parts": [{ "text": text }] })
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
    fn parses_text_and_function_calls() {
        let res = json!({
            "candidates": [{
                "content": {
                    "role": "model",
                    "parts": [
                        { "text": "Saya cek dulu." },
                        { "functionCall": { "name": "list_dir", "args": { "path": "." } },
                          "thoughtSignature": "abc" }
                    ]
                }
            }]
        });
        let turn = parse_model_turn(&res).unwrap();
        assert_eq!(turn.text, "Saya cek dulu.");
        assert_eq!(turn.function_calls.len(), 1);
        assert_eq!(turn.function_calls[0].name, "list_dir");
        // thoughtSignature harus tetap ada di raw content
        assert_eq!(turn.raw_content["parts"][1]["thoughtSignature"], "abc");
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
