//! Katalog Referensi Model & Provider (`ModelCatalog`) + Auto-Scanner & Capability Detector
//! ala Pipkin (`catalogue.ts`, `model-list.ts`, `detect.ts`, `scope.ts`).
//!
//! Menjadikan Rust (`oopbot`) sebagai mesin referensi tunggal (single source of truth) untuk:
//! 1. **Katalog Provider & Model Bawaan (`builtin_providers`)**: mencakup Google Gemini,
//!    OpenRouter, Z.ai, Kimi Code, Anthropic, dan OpenAI beserta metadata lengkap (`reasoning`,
//!    `thinkingLevelMap`, `input`, `contextWindow`, `maxTokens`, `cost`).
//! 2. **Resolusi Override & Konfigurasi Aplikasi (`apply_override`, `from_pipkin_json`)**:
//!    agar perubahan dari UI Android (`ModelEditorScreen`, `AddModelsScreen`, `SubscriptionScreen`,
//!    `ModelSheet`) langsung mengendalikan parameter model di engine Rust.
//! 3. **Live Scan & Deteksi Kapabilitas (`parse_models_response`)**:
//!    - Format **Gemini API** (`GET /v1beta/models`)
//!    - Format **OpenAI / OpenRouter / LiteLLM / Kimi / Z.ai / Ollama** (`GET /v1/models`)
//! 4. **Filter Scope Model (`resolve_model_scope`)**: mendukung pola glob & `:thinkingLevel`
//!    dari `settings.json` (`enabledModels`).

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::{BTreeMap, HashSet};

fn default_true() -> bool {
    true
}

fn default_api() -> String {
    "google-generative-ai".to_string()
}

fn default_input_modalities() -> Vec<String> {
    vec!["text".to_string(), "image".to_string()]
}

pub const CANONICAL_THINKING_LEVELS: &[&str] = &["off", "minimal", "low", "medium", "high", "xhigh"];

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ModelCost {
    #[serde(default)]
    pub input: f64,
    #[serde(default)]
    pub output: f64,
    #[serde(rename = "cacheRead", default)]
    pub cache_read: f64,
    #[serde(rename = "cacheWrite", default)]
    pub cache_write: f64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ModelInfo {
    /// ID model API (misal: `gemini-3.5-flash-lite`, `gemini-2.5-pro`).
    pub id: String,
    /// Nama tampilan ramah UI.
    pub label: String,
    /// Nama provider (misal: `google`, `openrouter`, `pipkin-local`).
    pub provider: String,
    /// Apakah mendukung tool/function calling.
    #[serde(default = "default_true")]
    pub supports_tools: bool,
    /// Batas token input (context window) bila dilaporkan server.
    pub input_token_limit: Option<u64>,
    /// Batas token output (`maxTokens`) bila dilaporkan server atau diatur aplikasi.
    pub output_token_limit: Option<u64>,
    /// Deskripsi singkat untuk pemilih model di UI/CLI.
    #[serde(default)]
    pub description: String,
    /// Jenis protokol API (`google-generative-ai`, `openai-completions`, `openai-responses`, `anthropic-messages`).
    #[serde(default = "default_api")]
    pub api: String,
    /// Apakah model mendukung mode penalaran (`thinking` / `reasoning`).
    #[serde(default = "default_true")]
    pub reasoning: bool,
    /// Pemetaan level thinking aplikasi (`minimal`..`xhigh`) ke nilai effort provider.
    #[serde(
        rename = "thinkingLevelMap",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub thinking_level_map: Option<BTreeMap<String, Option<String>>>,
    /// Modalitas input yang didukung (`["text", "image"]` atau `["text"]`).
    #[serde(default = "default_input_modalities")]
    pub input: Vec<String>,
    /// Informasi biaya per 1 juta token (`input`, `output`, `cacheRead`, `cacheWrite`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cost: Option<ModelCost>,
}

impl ModelInfo {
    /// Alias ramah OOPBoT untuk `to_pipkin_json`.
    pub fn to_oopbot_json(&self, provider_override: Option<&str>) -> Value {
        self.to_pipkin_json(provider_override)
    }

    /// Konversi ke objek JSON standar protokol Pipkin (`AgentProcess.kt` & `oopbot-sdk.ts`).
    pub fn to_pipkin_json(&self, provider_override: Option<&str>) -> Value {
        let provider = provider_override.unwrap_or(&self.provider);
        let input = if self.input.is_empty() {
            vec!["text".to_string(), "image".to_string()]
        } else {
            self.input.clone()
        };
        let mut obj = json!({
            "provider": provider,
            "id": self.id,
            "name": self.label,
            "api": self.api,
            "reasoning": self.reasoning,
            "input": input,
            "contextWindow": self.input_token_limit.unwrap_or(1_048_576),
            "maxTokens": self.output_token_limit.unwrap_or(65_536),
        });
        if let Some(map) = &self.thinking_level_map {
            obj["thinkingLevelMap"] = json!(map);
        }
        if let Some(cost) = &self.cost {
            obj["cost"] = json!({
                "input": cost.input,
                "output": cost.output,
                "cacheRead": cost.cache_read,
                "cacheWrite": cost.cache_write,
            });
        }
        obj
    }

    /// Terapkan override dari `models.json` (`modelOverrides.<id>` atau definisi model kustom)
    /// sehingga seluruh pengaturan dari aplikasi Android langsung berlaku di Rust.
    pub fn apply_override(&mut self, ov: &Value) {
        if !ov.is_object() {
            return;
        }
        if let Some(name) = ov["name"]
            .as_str()
            .or_else(|| ov["label"].as_str())
            .filter(|s| !s.trim().is_empty())
        {
            self.label = name.to_string();
        }
        if let Some(api) = ov["api"].as_str().filter(|s| !s.trim().is_empty()) {
            self.api = api.to_string();
        }
        if let Some(reasoning) = ov["reasoning"].as_bool() {
            self.reasoning = reasoning;
            if !reasoning {
                self.thinking_level_map = None;
            }
        }
        if let Some(cw) = ov["contextWindow"]
            .as_u64()
            .or_else(|| ov["input_token_limit"].as_u64())
            .filter(|&n| n > 0)
        {
            self.input_token_limit = Some(cw);
        }
        if let Some(mt) = ov["maxTokens"]
            .as_u64()
            .or_else(|| ov["output_token_limit"].as_u64())
            .filter(|&n| n > 0)
        {
            self.output_token_limit = Some(mt);
        }
        if let Some(inp_arr) = ov["input"].as_array() {
            let parsed: Vec<String> = inp_arr
                .iter()
                .filter_map(|v| v.as_str().map(str::to_string))
                .collect();
            if !parsed.is_empty() {
                self.input = parsed;
            }
        }
        if let Some(tlm_val) = ov.get("thinkingLevelMap") {
            if tlm_val.is_null() {
                self.thinking_level_map = None;
            } else if let Some(tlm_obj) = tlm_val.as_object() {
                let mut map = BTreeMap::new();
                for (k, v) in tlm_obj {
                    let mapped = v.as_str().map(str::to_string);
                    map.insert(k.clone(), mapped);
                }
                self.thinking_level_map = Some(map);
            }
        }
        if let Some(cost_obj) = ov.get("cost").and_then(|v| v.as_object()) {
            let prev = self.cost.clone().unwrap_or(ModelCost {
                input: 0.0,
                output: 0.0,
                cache_read: 0.0,
                cache_write: 0.0,
            });
            self.cost = Some(ModelCost {
                input: cost_obj
                    .get("input")
                    .and_then(|v| v.as_f64())
                    .unwrap_or(prev.input),
                output: cost_obj
                    .get("output")
                    .and_then(|v| v.as_f64())
                    .unwrap_or(prev.output),
                cache_read: cost_obj
                    .get("cacheRead")
                    .or_else(|| cost_obj.get("cache_read"))
                    .and_then(|v| v.as_f64())
                    .unwrap_or(prev.cache_read),
                cache_write: cost_obj
                    .get("cacheWrite")
                    .or_else(|| cost_obj.get("cache_write"))
                    .and_then(|v| v.as_f64())
                    .unwrap_or(prev.cache_write),
            });
        }
    }

    /// Alias ramah OOPBoT untuk `from_pipkin_json`.
    pub fn from_oopbot_json(item: &Value, default_provider: &str, default_api: &str) -> Option<Self> {
        Self::from_pipkin_json(item, default_provider, default_api)
    }

    /// Bangun `ModelInfo` dari item `models` di `models.json` (baik string ID maupun objek penuh).
    pub fn from_pipkin_json(item: &Value, default_provider: &str, default_api: &str) -> Option<Self> {
        if let Some(id) = item.as_str().filter(|s| !s.trim().is_empty()) {
            return Some(Self {
                id: id.to_string(),
                label: id.to_string(),
                provider: default_provider.to_string(),
                supports_tools: true,
                input_token_limit: Some(128_000),
                output_token_limit: Some(16_384),
                description: format!("Provider: {}", default_provider),
                api: default_api.to_string(),
                reasoning: infer_reasoning_from_id(id),
                thinking_level_map: None,
                input: infer_input_from_id(id),
                cost: None,
            });
        }
        let id = item["id"].as_str().filter(|s| !s.trim().is_empty())?;
        let provider = item["provider"]
            .as_str()
            .filter(|s| !s.trim().is_empty())
            .unwrap_or(default_provider);
        let mut info = Self {
            id: id.to_string(),
            label: item["name"]
                .as_str()
                .or_else(|| item["label"].as_str())
                .filter(|s| !s.trim().is_empty())
                .unwrap_or(id)
                .to_string(),
            provider: provider.to_string(),
            supports_tools: item["supports_tools"].as_bool().unwrap_or(true),
            input_token_limit: item["contextWindow"]
                .as_u64()
                .or_else(|| item["input_token_limit"].as_u64())
                .or(Some(128_000)),
            output_token_limit: item["maxTokens"]
                .as_u64()
                .or_else(|| item["output_token_limit"].as_u64())
                .or(Some(16_384)),
            description: item["description"]
                .as_str()
                .unwrap_or("Configured model")
                .to_string(),
            api: item["api"]
                .as_str()
                .filter(|s| !s.trim().is_empty())
                .unwrap_or(default_api)
                .to_string(),
            reasoning: item["reasoning"]
                .as_bool()
                .unwrap_or_else(|| infer_reasoning_from_id(id)),
            thinking_level_map: None,
            input: infer_input_from_id(id),
            cost: None,
        };
        info.apply_override(item);
        Some(info)
    }

    /// Daftar level thinking yang tersedia untuk model ini sesuai pengaturan aplikasi.
    pub fn available_thinking_levels(&self) -> Vec<String> {
        if !self.reasoning {
            return vec!["off".to_string()];
        }
        let mut levels = vec!["off".to_string()];
        if let Some(map) = &self.thinking_level_map {
            for &lvl in &CANONICAL_THINKING_LEVELS[1..] {
                if let Some(Some(mapped)) = map.get(lvl) {
                    if !mapped.is_empty() && mapped != "none" && mapped != "off" {
                        levels.push(lvl.to_string());
                    }
                }
            }
            if levels.len() == 1 {
                for (k, v) in map {
                    if k != "off"
                        && v.as_deref()
                            .map(|s| !s.is_empty() && s != "none" && s != "off")
                            .unwrap_or(false)
                        && !levels.contains(k)
                    {
                        levels.push(k.clone());
                    }
                }
            }
            return levels;
        }
        for &lvl in &CANONICAL_THINKING_LEVELS[1..] {
            levels.push(lvl.to_string());
        }
        levels
    }

    /// Pastikan level thinking valid untuk model ini; bila tidak didukung, kembalikan fallback terbaik.
    pub fn clamp_thinking_level(&self, requested: &str) -> String {
        let supported = self.available_thinking_levels();
        if supported.iter().any(|l| l == requested) {
            return requested.to_string();
        }
        if !self.reasoning || supported.len() <= 1 {
            return "off".to_string();
        }
        if supported.iter().any(|l| l == "medium") {
            return "medium".to_string();
        }
        supported
            .get(1)
            .cloned()
            .unwrap_or_else(|| "off".to_string())
    }
}

/// Definisi satu provider di katalog referensi bawaan Rust.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProviderCatalogEntry {
    pub id: String,
    pub name: String,
    #[serde(rename = "baseUrl")]
    pub base_url: String,
    pub api: String,
    pub models: Vec<ModelInfo>,
}

impl ProviderCatalogEntry {
    /// Alias ramah OOPBoT untuk `to_pipkin_json`.
    pub fn to_oopbot_json(&self) -> Value {
        self.to_pipkin_json()
    }

    pub fn to_pipkin_json(&self) -> Value {
        let models: Vec<Value> = self
            .models
            .iter()
            .map(|m| m.to_pipkin_json(Some(&self.id)))
            .collect();
        json!({
            "id": self.id,
            "name": self.name,
            "baseUrl": self.base_url,
            "api": self.api,
            "models": models,
        })
    }
}

/// Katalog referensi seluruh provider bawaan di mesin Rust `oopbot`.
pub fn builtin_providers() -> Vec<ProviderCatalogEntry> {
    vec![
        ProviderCatalogEntry {
            id: "google".into(),
            name: "Google Gemini (OOPBoT)".into(),
            base_url: "https://generativelanguage.googleapis.com/v1beta".into(),
            api: "google-generative-ai".into(),
            models: vec![
                ModelInfo {
                    id: "gemini-3.5-flash-lite".into(),
                    label: "Gemini 3.5 Flash Lite".into(),
                    provider: "google".into(),
                    supports_tools: true,
                    input_token_limit: Some(1_048_576),
                    output_token_limit: Some(65_536),
                    description: "Cepat & hemat kuota, cocok untuk tugas harian di Termux".into(),
                    api: "google-generative-ai".into(),
                    reasoning: true,
                    thinking_level_map: None,
                    input: vec!["text".into(), "image".into()],
                    cost: Some(ModelCost {
                        input: 0.10,
                        output: 0.40,
                        cache_read: 0.025,
                        cache_write: 0.10,
                    }),
                },
                ModelInfo {
                    id: "gemini-2.5-flash".into(),
                    label: "Gemini 2.5 Flash".into(),
                    provider: "google".into(),
                    supports_tools: true,
                    input_token_limit: Some(1_048_576),
                    output_token_limit: Some(65_536),
                    description: "Keseimbangan kecepatan & penalaran multi-langkah".into(),
                    api: "google-generative-ai".into(),
                    reasoning: true,
                    thinking_level_map: None,
                    input: vec!["text".into(), "image".into()],
                    cost: Some(ModelCost {
                        input: 0.30,
                        output: 2.50,
                        cache_read: 0.075,
                        cache_write: 0.30,
                    }),
                },
                ModelInfo {
                    id: "gemini-2.5-pro".into(),
                    label: "Gemini 2.5 Pro".into(),
                    provider: "google".into(),
                    supports_tools: true,
                    input_token_limit: Some(1_048_576),
                    output_token_limit: Some(65_536),
                    description: "Penalaran mendalam untuk coding & debugging kompleks".into(),
                    api: "google-generative-ai".into(),
                    reasoning: true,
                    thinking_level_map: None,
                    input: vec!["text".into(), "image".into()],
                    cost: Some(ModelCost {
                        input: 1.25,
                        output: 10.0,
                        cache_read: 0.31,
                        cache_write: 1.25,
                    }),
                },
                ModelInfo {
                    id: "gemini-2.5-flash-lite".into(),
                    label: "Gemini 2.5 Flash-Lite".into(),
                    provider: "google".into(),
                    supports_tools: true,
                    input_token_limit: Some(1_048_576),
                    output_token_limit: Some(65_536),
                    description: "Varian ringan Gemini 2.5 dengan latensi sangat rendah".into(),
                    api: "google-generative-ai".into(),
                    reasoning: true,
                    thinking_level_map: None,
                    input: vec!["text".into(), "image".into()],
                    cost: Some(ModelCost {
                        input: 0.10,
                        output: 0.40,
                        cache_read: 0.025,
                        cache_write: 0.10,
                    }),
                },
                ModelInfo {
                    id: "gemma-4-31b-it".into(),
                    label: "Gemma 4 31B IT".into(),
                    provider: "google".into(),
                    supports_tools: true,
                    input_token_limit: Some(131_072),
                    output_token_limit: Some(8_192),
                    description: "Model open-weights Gemma 4 31B Instruct di Google AI".into(),
                    api: "google-generative-ai".into(),
                    reasoning: true,
                    thinking_level_map: None,
                    input: vec!["text".into(), "image".into()],
                    cost: None,
                },
            ],
        },
        ProviderCatalogEntry {
            id: "openrouter".into(),
            name: "OpenRouter".into(),
            base_url: "https://openrouter.ai/api/v1".into(),
            api: "openai-completions".into(),
            models: vec![
                ModelInfo {
                    id: "google/gemini-2.5-flash".into(),
                    label: "Gemini 2.5 Flash".into(),
                    provider: "openrouter".into(),
                    supports_tools: true,
                    input_token_limit: Some(1_048_576),
                    output_token_limit: Some(65_536),
                    description: "Gemini 2.5 Flash via OpenRouter".into(),
                    api: "openai-completions".into(),
                    reasoning: true,
                    thinking_level_map: None,
                    input: vec!["text".into(), "image".into()],
                    cost: None,
                },
                ModelInfo {
                    id: "google/gemini-2.5-pro".into(),
                    label: "Gemini 2.5 Pro".into(),
                    provider: "openrouter".into(),
                    supports_tools: true,
                    input_token_limit: Some(1_048_576),
                    output_token_limit: Some(65_536),
                    description: "Gemini 2.5 Pro via OpenRouter".into(),
                    api: "openai-completions".into(),
                    reasoning: true,
                    thinking_level_map: None,
                    input: vec!["text".into(), "image".into()],
                    cost: None,
                },
                ModelInfo {
                    id: "anthropic/claude-sonnet-4".into(),
                    label: "Claude Sonnet 4".into(),
                    provider: "openrouter".into(),
                    supports_tools: true,
                    input_token_limit: Some(200_000),
                    output_token_limit: Some(64_000),
                    description: "Claude Sonnet 4 via OpenRouter".into(),
                    api: "openai-completions".into(),
                    reasoning: true,
                    thinking_level_map: None,
                    input: vec!["text".into(), "image".into()],
                    cost: None,
                },
                ModelInfo {
                    id: "deepseek/deepseek-r1".into(),
                    label: "DeepSeek R1".into(),
                    provider: "openrouter".into(),
                    supports_tools: true,
                    input_token_limit: Some(128_000),
                    output_token_limit: Some(32_768),
                    description: "DeepSeek R1 via OpenRouter".into(),
                    api: "openai-completions".into(),
                    reasoning: true,
                    thinking_level_map: None,
                    input: vec!["text".into()],
                    cost: None,
                },
            ],
        },
        ProviderCatalogEntry {
            id: "zai".into(),
            name: "Z.ai Coding Plan".into(),
            base_url: "https://api.z.ai/api/coding/paas/v4".into(),
            api: "openai-completions".into(),
            models: vec![
                ModelInfo {
                    id: "glm-4.5".into(),
                    label: "GLM-4.5".into(),
                    provider: "zai".into(),
                    supports_tools: true,
                    input_token_limit: Some(128_000),
                    output_token_limit: Some(16_384),
                    description: "Z.ai GLM-4.5 Coding Plan".into(),
                    api: "openai-completions".into(),
                    reasoning: true,
                    thinking_level_map: None,
                    input: vec!["text".into(), "image".into()],
                    cost: None,
                },
                ModelInfo {
                    id: "glm-4.5-air".into(),
                    label: "GLM-4.5 Air".into(),
                    provider: "zai".into(),
                    supports_tools: true,
                    input_token_limit: Some(128_000),
                    output_token_limit: Some(16_384),
                    description: "Z.ai GLM-4.5 Air".into(),
                    api: "openai-completions".into(),
                    reasoning: true,
                    thinking_level_map: None,
                    input: vec!["text".into()],
                    cost: None,
                },
            ],
        },
        ProviderCatalogEntry {
            id: "kimi-coding".into(),
            name: "Kimi Code".into(),
            base_url: "https://api.kimi.com/coding/v1".into(),
            api: "openai-completions".into(),
            models: vec![
                ModelInfo {
                    id: "kimi-k2-0711-preview".into(),
                    label: "Kimi K2".into(),
                    provider: "kimi-coding".into(),
                    supports_tools: true,
                    input_token_limit: Some(131_072),
                    output_token_limit: Some(32_768),
                    description: "Kimi K2 Coding Preview".into(),
                    api: "openai-completions".into(),
                    reasoning: true,
                    thinking_level_map: None,
                    input: vec!["text".into(), "image".into()],
                    cost: None,
                },
                ModelInfo {
                    id: "kimi-thinking-preview".into(),
                    label: "Kimi Thinking".into(),
                    provider: "kimi-coding".into(),
                    supports_tools: true,
                    input_token_limit: Some(131_072),
                    output_token_limit: Some(32_768),
                    description: "Kimi Thinking Preview".into(),
                    api: "openai-completions".into(),
                    reasoning: true,
                    thinking_level_map: None,
                    input: vec!["text".into(), "image".into()],
                    cost: None,
                },
            ],
        },
        ProviderCatalogEntry {
            id: "anthropic".into(),
            name: "Anthropic".into(),
            base_url: "https://api.anthropic.com/v1".into(),
            api: "anthropic-messages".into(),
            models: vec![
                ModelInfo {
                    id: "claude-sonnet-4-20250514".into(),
                    label: "Claude Sonnet 4".into(),
                    provider: "anthropic".into(),
                    supports_tools: true,
                    input_token_limit: Some(200_000),
                    output_token_limit: Some(64_000),
                    description: "Anthropic Claude Sonnet 4".into(),
                    api: "anthropic-messages".into(),
                    reasoning: true,
                    thinking_level_map: None,
                    input: vec!["text".into(), "image".into()],
                    cost: Some(ModelCost {
                        input: 3.0,
                        output: 15.0,
                        cache_read: 0.30,
                        cache_write: 3.75,
                    }),
                },
                ModelInfo {
                    id: "claude-opus-4-20250514".into(),
                    label: "Claude Opus 4".into(),
                    provider: "anthropic".into(),
                    supports_tools: true,
                    input_token_limit: Some(200_000),
                    output_token_limit: Some(32_000),
                    description: "Anthropic Claude Opus 4".into(),
                    api: "anthropic-messages".into(),
                    reasoning: true,
                    thinking_level_map: None,
                    input: vec!["text".into(), "image".into()],
                    cost: Some(ModelCost {
                        input: 15.0,
                        output: 75.0,
                        cache_read: 1.50,
                        cache_write: 18.75,
                    }),
                },
                ModelInfo {
                    id: "claude-3-5-haiku-latest".into(),
                    label: "Claude 3.5 Haiku".into(),
                    provider: "anthropic".into(),
                    supports_tools: true,
                    input_token_limit: Some(200_000),
                    output_token_limit: Some(8_192),
                    description: "Anthropic Claude 3.5 Haiku".into(),
                    api: "anthropic-messages".into(),
                    reasoning: false,
                    thinking_level_map: None,
                    input: vec!["text".into(), "image".into()],
                    cost: Some(ModelCost {
                        input: 0.80,
                        output: 4.0,
                        cache_read: 0.08,
                        cache_write: 1.0,
                    }),
                },
            ],
        },
        ProviderCatalogEntry {
            id: "openai-codex".into(),
            name: "OpenAI".into(),
            base_url: "https://api.openai.com/v1".into(),
            api: "openai-responses".into(),
            models: vec![
                ModelInfo {
                    id: "gpt-5".into(),
                    label: "GPT-5".into(),
                    provider: "openai-codex".into(),
                    supports_tools: true,
                    input_token_limit: Some(400_000),
                    output_token_limit: Some(128_000),
                    description: "OpenAI GPT-5".into(),
                    api: "openai-responses".into(),
                    reasoning: true,
                    thinking_level_map: None,
                    input: vec!["text".into(), "image".into()],
                    cost: None,
                },
                ModelInfo {
                    id: "gpt-5-mini".into(),
                    label: "GPT-5 Mini".into(),
                    provider: "openai-codex".into(),
                    supports_tools: true,
                    input_token_limit: Some(400_000),
                    output_token_limit: Some(128_000),
                    description: "OpenAI GPT-5 Mini".into(),
                    api: "openai-responses".into(),
                    reasoning: true,
                    thinking_level_map: None,
                    input: vec!["text".into(), "image".into()],
                    cost: None,
                },
                ModelInfo {
                    id: "o4-mini".into(),
                    label: "o4-mini".into(),
                    provider: "openai-codex".into(),
                    supports_tools: true,
                    input_token_limit: Some(200_000),
                    output_token_limit: Some(100_000),
                    description: "OpenAI o4-mini reasoning model".into(),
                    api: "openai-responses".into(),
                    reasoning: true,
                    thinking_level_map: None,
                    input: vec!["text".into(), "image".into()],
                    cost: None,
                },
            ],
        },
    ]
}

/// Ekspor katalog referensi provider & model bawaan dalam format JSON Pipkin.
pub fn builtin_catalog_json() -> Value {
    let providers: Vec<Value> = builtin_providers()
        .iter()
        .map(ProviderCatalogEntry::to_pipkin_json)
        .collect();
    json!({
        "version": env!("CARGO_PKG_VERSION"),
        "defaultProvider": "google",
        "defaultModel": "gemini-3.5-flash-lite",
        "providers": providers,
    })
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
    /// Katalog model bawaan dari provider referensi utama (`google`).
    pub fn builtin() -> Self {
        let google_models = builtin_providers()
            .into_iter()
            .find(|p| p.id == "google")
            .map(|p| {
                p.models
                    .into_iter()
                    .map(|mut m| {
                        m.provider = "gemini".to_string();
                        m
                    })
                    .collect()
            })
            .unwrap_or_default();
        Self {
            models: google_models,
            default_model_id: "gemini-3.5-flash-lite".into(),
        }
    }

    /// Katalog gabungan seluruh provider bawaan.
    pub fn all_builtin() -> Self {
        let mut models = Vec::new();
        for p in builtin_providers() {
            models.extend(p.models);
        }
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
            .or_else(|| {
                let stripped = query.split('/').next_back().unwrap_or(query);
                self.models
                    .iter()
                    .find(|m| m.id.eq_ignore_ascii_case(stripped))
            })
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
            api: infer_api_from_provider("custom"),
            reasoning: infer_reasoning_from_id(id),
            thinking_level_map: None,
            input: infer_input_from_id(id),
            cost: None,
        });
    }

    pub fn upsert_model(&mut self, info: ModelInfo) {
        if let Some(existing) = self.models.iter_mut().find(|m| m.id == info.id) {
            *existing = info;
        } else {
            self.models.push(info);
        }
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

pub fn infer_api_from_provider(provider_id: &str) -> String {
    match provider_id {
        "google" | "gemini" => "google-generative-ai".to_string(),
        "anthropic" => "anthropic-messages".to_string(),
        "openai-codex" => "openai-responses".to_string(),
        _ => "openai-completions".to_string(),
    }
}

pub fn infer_reasoning_from_id(id: &str) -> bool {
    let lower = id.to_ascii_lowercase();
    if lower.contains("haiku") || lower.contains("gemini-1.5") || lower.contains("gpt-4o-mini") {
        return false;
    }
    lower.contains("gemini-2.5")
        || lower.contains("gemini-3")
        || lower.contains("gemma-4")
        || lower.contains("claude-sonnet-4")
        || lower.contains("claude-opus-4")
        || lower.contains("claude-3-7")
        || lower.contains("gpt-5")
        || lower.contains("o1")
        || lower.contains("o3")
        || lower.contains("o4")
        || lower.contains("deepseek-r1")
        || lower.contains("reasoner")
        || lower.contains("thinking")
        || lower.contains("qwq")
        || lower.contains("glm-4.5")
        || lower.contains("kimi-k2")
        || lower.contains("qwen3")
}

pub fn infer_input_from_id(id: &str) -> Vec<String> {
    let lower = id.to_ascii_lowercase();
    if lower.contains("deepseek-r1")
        || lower.contains("deepseek-chat")
        || lower.contains("glm-4.5-air")
        || lower.contains("qwq")
        || lower.contains("coder")
    {
        vec!["text".to_string()]
    } else {
        vec!["text".to_string(), "image".to_string()]
    }
}

fn normalize_effort_name(raw: &str) -> Option<&'static str> {
    match raw.trim().to_ascii_lowercase().as_str() {
        "minimal" => Some("minimal"),
        "low" => Some("low"),
        "medium" | "med" => Some("medium"),
        "high" => Some("high"),
        "xhigh" | "max" | "very_high" => Some("xhigh"),
        _ => None,
    }
}

fn build_effort_map(levels: &[String]) -> BTreeMap<String, Option<String>> {
    let all = ["minimal", "low", "medium", "high", "xhigh"];
    let mut supported: Vec<(&str, &String)> = Vec::new();
    for raw in levels {
        if let Some(norm) = normalize_effort_name(raw) {
            if !supported.iter().any(|(n, _)| *n == norm) {
                supported.push((norm, raw));
            }
        }
    }
    supported.sort_by_key(|(norm, _)| all.iter().position(|&x| x == *norm).unwrap_or(99));

    let mut map = BTreeMap::new();
    for (idx, &slot) in all.iter().enumerate() {
        if let Some((_, raw)) = supported.iter().find(|(n, _)| *n == slot) {
            map.insert(slot.to_string(), Some((*raw).clone()));
        } else {
            // Pilih tetangga terdekat yang lebih rendah bila ada, atau yang lebih tinggi
            let lower = supported
                .iter()
                .rev()
                .find(|(n, _)| all.iter().position(|&x| x == *n).unwrap_or(99) < idx);
            let higher = supported
                .iter()
                .find(|(n, _)| all.iter().position(|&x| x == *n).unwrap_or(0) > idx);
            let chosen = lower.or(higher).map(|(_, raw)| (*raw).clone());
            map.insert(slot.to_string(), chosen);
        }
    }
    map
}

fn parse_price_value(v: &Value) -> Option<f64> {
    let raw = v
        .as_f64()
        .or_else(|| v.as_str().and_then(|s| s.trim().parse::<f64>().ok()))?;
    if !raw.is_finite() || raw < 0.0 {
        return None;
    }
    // OpenRouter/LiteLLM sering mengirim harga per-token (mis. 0.000003), konversi ke per-1M token.
    let per_m = if raw > 0.0 && raw < 0.01 {
        raw * 1_000_000.0
    } else {
        raw
    };
    Some((per_m * 1e6).round() / 1e6)
}

/// Ekstrak daftar `ModelInfo` dari JSON balasan endpoint `/models` beserta deteksi kapabilitas:
/// - Mendukung format Google Gemini (`{"models": [...]}`)
/// - Mendukung format OpenAI / OpenRouter / LiteLLM / Kimi / Z.ai (`{"data": [...]}`)
pub fn parse_models_response(res_json: &Value, provider_hint: &str) -> Vec<ModelInfo> {
    let mut out = Vec::new();
    let default_api = infer_api_from_provider(provider_hint);

    // 1. Format Google Gemini API: { "models": [ { "name": "models/gemini-2.5-flash", ... } ] }
    if let Some(models) = res_json["models"].as_array() {
        for entry in models {
            let raw_name = entry["name"].as_str().unwrap_or("");
            let id = raw_name.strip_prefix("models/").unwrap_or(raw_name);
            if id.is_empty()
                || id.contains("embedding")
                || id.contains("aqa")
                || id.contains("imagen")
            {
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
            let reasoning = entry["thinking"]
                .as_bool()
                .unwrap_or_else(|| infer_reasoning_from_id(id));

            out.push(ModelInfo {
                id: id.to_string(),
                label,
                provider: provider_hint.to_string(),
                supports_tools: true,
                input_token_limit: input_limit,
                output_token_limit: output_limit,
                description: desc,
                api: "google-generative-ai".to_string(),
                reasoning,
                thinking_level_map: None,
                input: vec!["text".to_string(), "image".to_string()],
                cost: None,
            });
        }
    }
    // 2. Format OpenAI / OpenRouter / Ollama / Kimi / Z.ai: { "data": [ { "id": "...", ... } ] }
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
                .or_else(|| entry["max_input_tokens"].as_u64())
                .or_else(|| entry["input_token_limit"].as_u64())
                .or_else(|| entry["context_window"].as_u64())
                .or_else(|| entry["top_provider"]["context_length"].as_u64());
            let output_limit = entry["max_completion_tokens"]
                .as_u64()
                .or_else(|| entry["max_output_tokens"].as_u64())
                .or_else(|| entry["output_token_limit"].as_u64())
                .or_else(|| entry["max_tokens"].as_u64())
                .or_else(|| entry["top_provider"]["max_completion_tokens"].as_u64());
            let desc = entry["description"]
                .as_str()
                .map(|d| d.chars().take(90).collect::<String>())
                .unwrap_or_else(|| format!("Provider: {}", provider_hint));

            // Deteksi reasoning & thinkingLevelMap ala detect.ts
            let mut reasoning = entry["reasoning"]
                .as_bool()
                .or_else(|| entry["supports_reasoning"].as_bool());
            let mut thinking_level_map = None;

            if let Some(efforts) = entry["reasoning_effort"]
                .as_array()
                .or_else(|| entry["supported_reasoning_efforts"].as_array())
            {
                let active: Vec<String> = efforts
                    .iter()
                    .filter_map(|v| v.as_str())
                    .filter(|s| *s != "none" && *s != "off")
                    .map(str::to_string)
                    .collect();
                if active.is_empty() {
                    reasoning = Some(false);
                } else {
                    reasoning = Some(true);
                    thinking_level_map = Some(build_effort_map(&active));
                }
            }
            if reasoning.is_none() {
                if let Some(params) = entry["supported_parameters"].as_array() {
                    if params.iter().any(|p| {
                        matches!(
                            p.as_str(),
                            Some("reasoning")
                                | Some("include_reasoning")
                                | Some("reasoning_effort")
                        )
                    }) {
                        reasoning = Some(true);
                    }
                }
            }
            if reasoning.is_none() {
                if let Some(r_str) = entry["architecture"]["Reasoning"].as_str() {
                    reasoning = Some(!r_str.is_empty() && r_str != "none" && r_str != "false");
                }
            }
            let reasoning = reasoning.unwrap_or_else(|| infer_reasoning_from_id(id));

            // Deteksi modalitas input (text vs text+image)
            let mut has_image = None;
            if let Some(mods) = entry["architecture"]["input_modalities"]
                .as_array()
                .or_else(|| entry["input_modalities"].as_array())
                .or_else(|| entry["modalities"].as_array())
            {
                has_image = Some(mods.iter().any(|m| {
                    matches!(
                        m.as_str(),
                        Some("image") | Some("vision") | Some("multimodal")
                    )
                }));
            } else if let Some(mod_str) = entry["architecture"]["modality"].as_str() {
                let left = mod_str.split("->").next().unwrap_or(mod_str);
                has_image = Some(left.contains("image") || left.contains("vision"));
            } else if let Some(v) = entry["vision"]
                .as_bool()
                .or_else(|| entry["supports_vision"].as_bool())
            {
                has_image = Some(v);
            }
            let input = match has_image {
                Some(true) => vec!["text".to_string(), "image".to_string()],
                Some(false) => vec!["text".to_string()],
                None => infer_input_from_id(id),
            };

            // Deteksi cost bila ada objek pricing
            let cost = entry
                .get("pricing")
                .or_else(|| entry.get("cost"))
                .and_then(|p| p.as_object())
                .and_then(|p| {
                    let inp = p
                        .get("prompt")
                        .or_else(|| p.get("input"))
                        .and_then(parse_price_value);
                    let out_p = p
                        .get("completion")
                        .or_else(|| p.get("output"))
                        .and_then(parse_price_value);
                    if inp.is_none() && out_p.is_none() {
                        return None;
                    }
                    let cr = p
                        .get("input_cache_read")
                        .or_else(|| p.get("cacheRead"))
                        .or_else(|| p.get("cache_read"))
                        .and_then(parse_price_value)
                        .unwrap_or(0.0);
                    let cw = p
                        .get("input_cache_write")
                        .or_else(|| p.get("cacheWrite"))
                        .or_else(|| p.get("cache_write"))
                        .and_then(parse_price_value)
                        .unwrap_or(0.0);
                    Some(ModelCost {
                        input: inp.unwrap_or(0.0),
                        output: out_p.unwrap_or(0.0),
                        cache_read: cr,
                        cache_write: cw,
                    })
                });

            out.push(ModelInfo {
                id: id.to_string(),
                label,
                provider: provider_hint.to_string(),
                supports_tools: true,
                input_token_limit: input_limit,
                output_token_limit: output_limit,
                description: desc,
                api: default_api.clone(),
                reasoning,
                thinking_level_map,
                input,
                cost,
            });
        }
    }

    out.sort_by(|a, b| a.id.cmp(&b.id));
    out.dedup_by(|a, b| a.id == b.id);
    out
}

/// Cocokkan teks dengan pola glob sederhana (`*` dan `?`, case-insensitive).
pub fn glob_match(pattern: &str, text: &str) -> bool {
    let p: Vec<char> = pattern.to_ascii_lowercase().chars().collect();
    let t: Vec<char> = text.to_ascii_lowercase().chars().collect();
    let (mut pi, mut ti) = (0usize, 0usize);
    let (mut star_pi, mut star_ti) = (None::<usize>, 0usize);

    while ti < t.len() {
        if pi < p.len() && (p[pi] == '?' || p[pi] == t[ti]) {
            pi += 1;
            ti += 1;
        } else if pi < p.len() && p[pi] == '*' {
            star_pi = Some(pi);
            pi += 1;
            star_ti = ti;
        } else if let Some(sp) = star_pi {
            pi = sp + 1;
            star_ti += 1;
            ti = star_ti;
        } else {
            return false;
        }
    }
    while pi < p.len() && p[pi] == '*' {
        pi += 1;
    }
    pi == p.len()
}

/// Filter dan urutkan daftar model yang tersedia berdasarkan `enabledModels` di `settings.json`
/// (mendukung format `<provider>/<modelId>`, `<modelId>`, wildcard `*`, serta akhiran `:thinkingLevel`).
pub fn resolve_model_scope(
    patterns: &[String],
    available: &[ModelInfo],
) -> Vec<(ModelInfo, Option<String>)> {
    let mut scoped = Vec::new();
    let mut seen = HashSet::new();

    for raw_pattern in patterns {
        let trimmed = raw_pattern.trim();
        if trimmed.is_empty() {
            continue;
        }
        let (core, thinking_level) = if let Some(colon_idx) = trimmed.rfind(':') {
            let suffix = &trimmed[colon_idx + 1..];
            if colon_idx > 0 && CANONICAL_THINKING_LEVELS.contains(&suffix) {
                (&trimmed[..colon_idx], Some(suffix.to_string()))
            } else {
                (trimmed, None)
            }
        } else {
            (trimmed, None)
        };

        for model in available {
            let full_ref = format!("{}/{}", model.provider, model.id);
            let matched = if core.contains('/') {
                glob_match(core, &full_ref)
            } else {
                glob_match(core, &model.id) || glob_match(core, &model.label)
            };
            if matched && seen.insert(full_ref) {
                scoped.push((model.clone(), thinking_level.clone()));
            }
        }
    }
    scoped
}

/// Ekstrak kredensial (`(token, "oauth" | "api_key")`) dari `auth.json` untuk provider tertentu.
pub fn extract_auth_credentials(auth_json: &Value, provider_id: &str) -> Option<(String, &'static str)> {
    let entry = auth_json.get(provider_id)?;
    if let Some(s) = entry.as_str().map(str::trim).filter(|s| !s.is_empty() && *s != "no-api-key") {
        return Some((s.to_string(), "api_key"));
    }

    let obj = entry.as_object()?;
    let declared_type = obj.get("type").and_then(|v| v.as_str()).unwrap_or_default();

    let oauth_candidates = [
        obj.get("access").and_then(|v| v.as_str()),
        obj.get("access_token").and_then(|v| v.as_str()),
        obj.get("accessToken").and_then(|v| v.as_str()),
        obj.get("auth").and_then(|a| a.get("access")).and_then(|v| v.as_str()),
        obj.get("auth").and_then(|a| a.get("access_token")).and_then(|v| v.as_str()),
        obj.get("auth").and_then(|a| a.get("accessToken")).and_then(|v| v.as_str()),
    ];
    let api_candidates = [
        obj.get("apiKey").and_then(|v| v.as_str()),
        obj.get("key").and_then(|v| v.as_str()),
        obj.get("auth").and_then(|a| a.get("apiKey")).and_then(|v| v.as_str()),
        obj.get("auth").and_then(|a| a.get("key")).and_then(|v| v.as_str()),
    ];
    let token_fallback = obj
        .get("token")
        .and_then(|v| v.as_str())
        .or_else(|| obj.get("auth").and_then(|a| a.get("token")).and_then(|v| v.as_str()));

    if declared_type == "oauth" {
        if let Some(tok) = oauth_candidates
            .into_iter()
            .flatten()
            .chain(token_fallback)
            .map(str::trim)
            .find(|s| !s.is_empty() && *s != "no-api-key")
        {
            return Some((tok.to_string(), "oauth"));
        }
    }

    if let Some(key) = api_candidates
        .into_iter()
        .flatten()
        .map(str::trim)
        .find(|s| !s.is_empty() && *s != "no-api-key")
    {
        return Some((key.to_string(), "api_key"));
    }

    if let Some(tok) = oauth_candidates
        .into_iter()
        .flatten()
        .map(str::trim)
        .find(|s| !s.is_empty() && *s != "no-api-key")
    {
        return Some((tok.to_string(), "oauth"));
    }

    if let Some(tok) = token_fallback.map(str::trim).filter(|s| !s.is_empty() && *s != "no-api-key") {
        let kind = if declared_type == "oauth" { "oauth" } else { "api_key" };
        return Some((tok.to_string(), kind));
    }

    None
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
    fn builtin_providers_cover_all_six_reference_providers() {
        let providers = builtin_providers();
        let ids: Vec<&str> = providers.iter().map(|p| p.id.as_str()).collect();
        for expected in [
            "google",
            "openrouter",
            "zai",
            "kimi-coding",
            "anthropic",
            "openai-codex",
        ] {
            assert!(ids.contains(&expected), "missing provider {}", expected);
            let p = providers.iter().find(|p| p.id == expected).unwrap();
            assert!(!p.models.is_empty(), "provider {} has no models", expected);
        }
    }

    #[test]
    fn model_overrides_and_thinking_levels_work() {
        let mut m = ModelCatalog::builtin().find("gemini-2.5-flash").unwrap().clone();
        assert!(m.reasoning);
        assert!(m.available_thinking_levels().contains(&"high".to_string()));

        m.apply_override(&json!({
            "name": "Flash Kustom App",
            "contextWindow": 500_000,
            "maxTokens": 16_000,
            "thinkingLevelMap": {
                "minimal": "low",
                "low": "low",
                "medium": "medium",
                "high": null,
                "xhigh": null
            },
            "input": ["text"],
            "cost": { "input": 0.5, "output": 1.5 }
        }));
        assert_eq!(m.label, "Flash Kustom App");
        assert_eq!(m.input_token_limit, Some(500_000));
        assert_eq!(m.output_token_limit, Some(16_000));
        assert_eq!(m.input, vec!["text".to_string()]);
        assert_eq!(
            m.available_thinking_levels(),
            vec!["off", "minimal", "low", "medium"]
        );
        assert_eq!(m.clamp_thinking_level("high"), "medium");
    }

    #[test]
    fn parses_gemini_and_openai_model_lists_with_capabilities() {
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
        let parsed = parse_models_response(&gemini_json, "google");
        assert_eq!(parsed.len(), 1);
        assert_eq!(parsed[0].id, "gemini-2.5-flash");
        assert_eq!(parsed[0].input_token_limit, Some(1_048_576));
        assert!(parsed[0].reasoning);

        let openai_json = json!({
            "object": "list",
            "data": [
                {
                    "id": "deepseek-chat",
                    "name": "DeepSeek V3",
                    "context_length": 64000,
                    "reasoning_effort": ["low", "high"],
                    "architecture": { "input_modalities": ["text"] },
                    "pricing": { "prompt": "0.00000027", "completion": "0.0000011" }
                }
            ]
        });
        let parsed_oa = parse_models_response(&openai_json, "openrouter");
        assert_eq!(parsed_oa.len(), 1);
        assert_eq!(parsed_oa[0].id, "deepseek-chat");
        assert_eq!(parsed_oa[0].label, "DeepSeek V3");
        assert_eq!(parsed_oa[0].input_token_limit, Some(64_000));
        assert!(parsed_oa[0].reasoning);
        assert_eq!(parsed_oa[0].input, vec!["text".to_string()]);
        assert!(parsed_oa[0].thinking_level_map.is_some());
        assert!(parsed_oa[0].cost.is_some());
    }

    #[test]
    fn resolves_model_scope_patterns() {
        let available = vec![
            ModelCatalog::builtin().find("gemini-2.5-flash").unwrap().clone(),
            ModelCatalog::builtin().find("gemini-2.5-pro").unwrap().clone(),
        ];
        let patterns = vec!["gemini/gemini-2.5-pro:high".to_string(), "*flash*".to_string()];
        let scoped = resolve_model_scope(&patterns, &available);
        assert_eq!(scoped.len(), 2);
        assert_eq!(scoped[0].0.id, "gemini-2.5-pro");
        assert_eq!(scoped[0].1.as_deref(), Some("high"));
        assert_eq!(scoped[1].0.id, "gemini-2.5-flash");
        assert_eq!(scoped[1].1, None);
    }

    #[test]
    fn reg005_extract_auth_credentials_supports_oauth_and_api_key() {
        let doc = json!({
            "anthropic": { "type": "oauth", "access": "oa-tok-1", "refresh": "ref-1" },
            "openai-codex": { "access_token": "oa-tok-2" },
            "openrouter": { "type": "api_key", "key": "sk-or-123" },
            "google": "AIza-direct-str",
            "zai": { "apiKey": "no-api-key" }
        });
        assert_eq!(
            extract_auth_credentials(&doc, "anthropic"),
            Some(("oa-tok-1".to_string(), "oauth"))
        );
        assert_eq!(
            extract_auth_credentials(&doc, "openai-codex"),
            Some(("oa-tok-2".to_string(), "oauth"))
        );
        assert_eq!(
            extract_auth_credentials(&doc, "openrouter"),
            Some(("sk-or-123".to_string(), "api_key"))
        );
        assert_eq!(
            extract_auth_credentials(&doc, "google"),
            Some(("AIza-direct-str".to_string(), "api_key"))
        );
        assert_eq!(extract_auth_credentials(&doc, "zai"), None);
    }
}
