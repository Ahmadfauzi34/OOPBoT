//! Klien minimal untuk Gemini API (`generateContent`) dengan dukungan function calling.
//!
//! Riwayat percakapan disimpan sebagai `serde_json::Value` mentah agar field
//! seperti `thoughtSignature` (wajib dikirim balik oleh model Gemini 3 saat
//! function calling) tidak hilang ketika kita meneruskan riwayat ke request berikutnya.

use serde_json::{json, Value};
use std::time::Duration;

pub type BoxError = Box<dyn std::error::Error + Send + Sync>;

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

pub struct GeminiClient {
    api_key: String,
    model: String,
    base_url: String,
    http: reqwest::Client,
}

impl GeminiClient {
    pub fn new(api_key: String, model: String) -> Result<Self, BoxError> {
        let http = reqwest::Client::builder()
            .timeout(Duration::from_secs(180))
            .user_agent(concat!("oopbot/", env!("CARGO_PKG_VERSION")))
            .build()?;
        let base_url = std::env::var("GEMINI_BASE_URL")
            .unwrap_or_else(|_| "https://generativelanguage.googleapis.com/v1beta".to_string());
        Ok(Self {
            api_key,
            model,
            base_url,
            http,
        })
    }

    pub fn model(&self) -> &str {
        &self.model
    }

    /// Kirim seluruh riwayat + deklarasi tool, kembalikan giliran model.
    pub async fn generate(
        &self,
        system_instruction: &str,
        contents: &[Value],
        function_declarations: &[Value],
    ) -> Result<ModelTurn, BoxError> {
        let url = format!("{}/models/{}:generateContent", self.base_url, self.model);

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
            .header("x-goog-api-key", &self.api_key)
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
        let call = FunctionCall { id: Some("x1".into()), name: "read_file".into(), args: json!({}) };
        let part = function_response_part(&call, json!({ "ok": true }));
        assert_eq!(part["functionResponse"]["id"], "x1");
        assert_eq!(part["functionResponse"]["name"], "read_file");
    }
}
