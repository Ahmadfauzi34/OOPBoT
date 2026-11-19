//! Tipe data bersama & builder pesan internal untuk percakapan LLM.

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::BTreeMap;

/// Satu panggilan fungsi yang diminta model.
#[derive(Debug, Clone)]
pub struct FunctionCall {
    pub id: Option<String>,
    pub name: String,
    pub args: Value,
    pub thought_signature: Option<String>,
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
    /// ID sesi untuk header `x-session-affinity` (digunakan oleh `LocalModelServer` / `oopbot-local`).
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
/// teks yang bisa ditampilkan, ringkasan penalaran (`thoughts`), tanda tangan penalaran
/// (`thought_signatures`, sejajar indeks dengan `thoughts`), daftar function call,
/// dan penggunaan token (`usage`).
#[derive(Debug, Clone)]
pub struct ModelTurn {
    pub raw_content: Value,
    pub text: String,
    pub thoughts: Vec<String>,
    pub thought_signatures: Vec<Option<String>>,
    pub function_calls: Vec<FunctionCall>,
    pub usage: Option<TokenUsage>,
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
