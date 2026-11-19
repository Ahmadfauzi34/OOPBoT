//! Klien HTTP multi-protokol (`LlmClient`) untuk Gemini API, OpenAI-compatible
//! (OpenRouter, Z.ai, Kimi, Local Server `oopbot-local`), OpenAI Responses API,
//! dan Anthropic Messages API dengan:
//! - Function/Tool Calling lintas provider
//! - Kontrol penuh parameter inferensi dari aplikasi Android ([`GenerationOptions`]:
//!   `api`, `max_tokens`, `thinking_level`, `reasoning`, `thinking_level_map`, `session_id`)
//! - Dukungan input multimodal (`inlineData` gambar base64 dari composer aplikasi)
//! - Ekstraksi `thoughts` (`thinking_start`/`thinking_end`) & penggunaan token ([`TokenUsage`])
//! - Pergantian token (`api_key`) & `base_url` secara dinamis tanpa restart
//! - Auto-scan daftar model (`scan_models`) langsung dari token provider

mod anthropic;
mod gemini;
mod http;
mod openai_chat;
mod openai_responses;
mod types;

#[cfg(test)]
mod tests;

pub use crate::common::BoxError;
pub use gemini::{
    parse_model_turn, sanitize_gemini_function_declarations, sanitize_gemini_schema,
    DEFAULT_GEMINI_BASE_URL,
};
pub use openai_responses::parse_openai_responses_turn;
pub use types::{
    function_response_part, user_message_with_images, user_text, FunctionCall, GenerationOptions,
    ModelTurn, TokenUsage,
};

use crate::config::{parse_models_response, ModelInfo};
use http::parse_api_json_response;
use serde_json::Value;
use std::sync::RwLock;
use std::time::Duration;

#[derive(Debug, Clone)]
struct Credentials {
    api_key: String,
    base_url: String,
}

/// Klien HTTP multi-protokol LLM yang dapat berpindah kredensial, endpoint, dan model secara dinamis.
pub struct LlmClient {
    creds: RwLock<Credentials>,
    default_model: RwLock<String>,
    pub(super) http: reqwest::Client,
}

impl LlmClient {
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
        self.default_model
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    /// Ganti model aktif pada klien secara dinamis tanpa membuat ulang HTTP pool.
    pub fn set_default_model(&self, new_model: &str) {
        let mut m = self
            .default_model
            .write()
            .unwrap_or_else(|e| e.into_inner());
        *m = new_model.to_string();
    }

    pub fn base_url(&self) -> String {
        self.creds
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .base_url
            .clone()
    }

    pub fn api_key(&self) -> String {
        self.creds
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .api_key
            .clone()
    }

    /// Ganti API key (dan opsional `base_url`) secara dinamis di memori.
    pub fn update_credentials(&self, api_key: String, base_url: Option<String>) {
        let mut creds = self.creds.write().unwrap_or_else(|e| e.into_inner());
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
            let c = self.creds.read().unwrap_or_else(|e| e.into_inner());
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
        let res_json = parse_api_json_response("Gagal scan /models", resp).await?;

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
            let c = self.creds.read().unwrap_or_else(|e| e.into_inner());
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

        self.generate_gemini(
            &api_key,
            &base_url,
            model,
            system_instruction,
            contents,
            function_declarations,
            options,
        )
        .await
    }
}
