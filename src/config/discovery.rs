//! Parser respons endpoint `/models` & inferensi kapabilitas model.
//!
//! Mendukung format Google Gemini (`{"models": [...]}`) serta format
//! OpenAI / OpenRouter / LiteLLM / Ollama / Kimi / Z.ai (`{"data": [...]}`).

use super::catalog::{ModelCost, ModelInfo};
use serde_json::Value;
use std::collections::BTreeMap;

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
