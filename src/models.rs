//! Katalog Model & Provider (`ModelCatalog`) + Auto-Scanner ala Pipkin (`model-list.ts` & `detect.ts`).
//!
//! Mendukung:
//! 1. **Katalog Bawaan (Built-in Fallback)**: tetap ada saat offline.
//! 2. **Live Scan dari Token (`parse_models_response`)**:
//!    - Format **Gemini API** (`GET /v1beta/models` → `{"models": [{"name": "models/...", ...}]}`)
//!    - Format **OpenAI / OpenRouter / LiteLLM / Ollama** (`GET /v1/models` → `{"data": [{"id": "...", ...}]}`)

use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ModelInfo {
    /// ID model API (misal: `gemini-3.5-flash-lite`, `gemini-2.5-pro`).
    pub id: String,
    /// Nama tampilan ramah UI.
    pub label: String,
    /// Nama provider (misal: `gemini`, `openai-compatible`).
    pub provider: String,
    /// Apakah mendukung tool/function calling.
    pub supports_tools: bool,
    /// Batas token input (context window) bila dilaporkan server.
    pub input_token_limit: Option<u64>,
    /// Batas token output bila dilaporkan server.
    pub output_token_limit: Option<u64>,
    /// Deskripsi singkat untuk pemilih model di UI/CLI.
    pub description: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModelCatalog {
    models: Vec<ModelInfo>,
    default_model_id: String,
}

impl Default for ModelCatalog {
    fn default() -> Self {
        Self::builtin()
    }
}

impl ModelCatalog {
    /// Katalog model bawaan Gemini yang bisa langsung dipilih di UI/CLI sebelum scan.
    pub fn builtin() -> Self {
        let models = vec![
            ModelInfo {
                id: "gemini-3.5-flash-lite".into(),
                label: "Gemini 3.5 Flash Lite".into(),
                provider: "gemini".into(),
                supports_tools: true,
                input_token_limit: Some(1_048_576),
                output_token_limit: Some(65_536),
                description: "Cepat & hemat kuota, cocok untuk tugas harian di Termux".into(),
            },
            ModelInfo {
                id: "gemini-2.5-flash".into(),
                label: "Gemini 2.5 Flash".into(),
                provider: "gemini".into(),
                supports_tools: true,
                input_token_limit: Some(1_048_576),
                output_token_limit: Some(65_536),
                description: "Keseimbangan kecepatan & penalaran multi-langkah".into(),
            },
            ModelInfo {
                id: "gemini-2.5-pro".into(),
                label: "Gemini 2.5 Pro".into(),
                provider: "gemini".into(),
                supports_tools: true,
                input_token_limit: Some(1_048_576),
                output_token_limit: Some(65_536),
                description: "Penalaran mendalam untuk coding & debugging kompleks".into(),
            },
        ];
        Self {
            models,
            default_model_id: "gemini-3.5-flash-lite".into(),
        }
    }

    pub fn list(&self) -> &[ModelInfo] {
        &self.models
    }

    pub fn default_model(&self) -> &str {
        &self.default_model_id
    }

    pub fn set_default_model(&mut self, model_id: &str) {
        if self.find(model_id).is_none() {
            self.register_custom(model_id, model_id);
        }
        self.default_model_id = model_id.to_string();
    }

    pub fn find(&self, query: &str) -> Option<&ModelInfo> {
        if let Ok(idx) = query.parse::<usize>() {
            if idx >= 1 && idx <= self.models.len() {
                return Some(&self.models[idx - 1]);
            }
        }
        self.models
            .iter()
            .find(|m| m.id.eq_ignore_ascii_case(query))
    }

    pub fn register_custom(&mut self, id: &str, label: &str) {
        if self.models.iter().any(|m| m.id == id) {
            return;
        }
        self.models.push(ModelInfo {
            id: id.to_string(),
            label: label.to_string(),
            provider: "custom".into(),
            supports_tools: true,
            input_token_limit: None,
            output_token_limit: None,
            description: "Model kustom".into(),
        });
    }

    /// Perbarui katalog dengan hasil scan live dari endpoint `/models`.
    /// Mengembalikan jumlah model hasil scan.
    pub fn replace_with_scanned(&mut self, scanned: Vec<ModelInfo>) -> usize {
        if scanned.is_empty() {
            return 0;
        }
        let count = scanned.len();
        self.models = scanned;
        if self.find(&self.default_model_id).is_none() {
            if let Some(first) = self.models.first() {
                self.default_model_id = first.id.clone();
            }
        }
        count
    }
}

/// Ekstrak daftar `ModelInfo` dari JSON balasan endpoint `/models`:
/// - Mendukung format Google Gemini (`{"models": [...]}`)
/// - Mendukung format OpenAI / OpenRouter / LiteLLM (`{"data": [...]}`)
pub fn parse_models_response(res_json: &Value, provider_hint: &str) -> Vec<ModelInfo> {
    let mut out = Vec::new();

    // 1. Format Google Gemini API: { "models": [ { "name": "models/gemini-2.5-flash", ... } ] }
    if let Some(models) = res_json["models"].as_array() {
        for entry in models {
            let raw_name = entry["name"].as_str().unwrap_or("");
            let id = raw_name.strip_prefix("models/").unwrap_or(raw_name);
            if id.is_empty() {
                continue;
            }
            // Hanya ambil model yang mendukung generateContent (chat/agent), lewati model embedding murni.
            if let Some(methods) = entry["supportedGenerationMethods"].as_array() {
                let can_generate = methods
                    .iter()
                    .any(|m| m.as_str() == Some("generateContent"));
                if !can_generate {
                    continue;
                }
            }
            let label = entry["displayName"]
                .as_str()
                .filter(|s| !s.is_empty())
                .unwrap_or(id)
                .to_string();
            let input_limit = entry["inputTokenLimit"].as_u64();
            let output_limit = entry["outputTokenLimit"].as_u64();
            let desc = entry["description"]
                .as_str()
                .map(|d| d.chars().take(90).collect::<String>())
                .unwrap_or_default();

            out.push(ModelInfo {
                id: id.to_string(),
                label,
                provider: provider_hint.to_string(),
                supports_tools: true,
                input_token_limit: input_limit,
                output_token_limit: output_limit,
                description: desc,
            });
        }
    }
    // 2. Format OpenAI / OpenRouter / Ollama: { "data": [ { "id": "...", ... } ] }
    else if let Some(data) = res_json["data"].as_array() {
        for entry in data {
            let Some(id) = entry["id"].as_str().filter(|s| !s.is_empty()) else {
                continue;
            };
            let label = entry["name"]
                .as_str()
                .or_else(|| entry["display_name"].as_str())
                .filter(|s| !s.is_empty())
                .unwrap_or(id)
                .to_string();
            let input_limit = entry["context_length"]
                .as_u64()
                .or_else(|| entry["max_input_tokens"].as_u64());
            let output_limit = entry["max_completion_tokens"]
                .as_u64()
                .or_else(|| entry["max_output_tokens"].as_u64());
            let desc = entry["description"]
                .as_str()
                .map(|d| d.chars().take(90).collect::<String>())
                .unwrap_or_else(|| format!("Provider: {}", provider_hint));

            out.push(ModelInfo {
                id: id.to_string(),
                label,
                provider: provider_hint.to_string(),
                supports_tools: true,
                input_token_limit: input_limit,
                output_token_limit: output_limit,
                description: desc,
            });
        }
    }

    out.sort_by(|a, b| a.id.cmp(&b.id));
    out.dedup_by(|a, b| a.id == b.id);
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn lookup_by_index_and_id() {
        let mut cat = ModelCatalog::builtin();
        assert_eq!(cat.find("1").unwrap().id, "gemini-3.5-flash-lite");
        assert_eq!(cat.find("gemini-2.5-pro").unwrap().label, "Gemini 2.5 Pro");
        cat.set_default_model("gemini-experimental-custom");
        assert_eq!(cat.default_model(), "gemini-experimental-custom");
        assert!(cat.find("gemini-experimental-custom").is_some());
    }

    #[test]
    fn parses_gemini_and_openai_model_lists() {
        let gemini_json = json!({
            "models": [
                {
                    "name": "models/gemini-2.5-flash",
                    "displayName": "Gemini 2.5 Flash",
                    "inputTokenLimit": 1048576,
                    "outputTokenLimit": 65536,
                    "supportedGenerationMethods": ["generateContent", "countTokens"]
                },
                {
                    "name": "models/text-embedding-004",
                    "supportedGenerationMethods": ["embedContent"]
                }
            ]
        });
        let parsed = parse_models_response(&gemini_json, "gemini");
        assert_eq!(parsed.len(), 1);
        assert_eq!(parsed[0].id, "gemini-2.5-flash");
        assert_eq!(parsed[0].input_token_limit, Some(1_048_576));

        let openai_json = json!({
            "object": "list",
            "data": [
                { "id": "deepseek-chat", "name": "DeepSeek V3", "context_length": 64000 }
            ]
        });
        let parsed_oa = parse_models_response(&openai_json, "openai");
        assert_eq!(parsed_oa.len(), 1);
        assert_eq!(parsed_oa[0].id, "deepseek-chat");
        assert_eq!(parsed_oa[0].label, "DeepSeek V3");
        assert_eq!(parsed_oa[0].input_token_limit, Some(64_000));
    }
}
