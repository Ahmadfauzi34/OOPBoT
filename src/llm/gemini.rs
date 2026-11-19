//! Protokol native Google Gemini (`google-generative-ai` / `:generateContent`),
//! termasuk sanitasi schema `functionDeclarations`, budget thinking, dan parser respons.

use super::http::parse_api_json_response;
use super::{FunctionCall, GenerationOptions, LlmClient, ModelTurn, TokenUsage};
use crate::common::BoxError;
use serde_json::{json, Value};

pub const DEFAULT_GEMINI_BASE_URL: &str = "https://generativelanguage.googleapis.com/v1beta";

impl LlmClient {
    #[allow(clippy::too_many_arguments)]
    pub(super) async fn generate_gemini(
        &self,
        api_key: &str,
        base_url: &str,
        model: &str,
        system_instruction: &str,
        contents: &[Value],
        function_declarations: &[Value],
        options: &GenerationOptions,
    ) -> Result<ModelTurn, BoxError> {
        let clean_model = model.strip_prefix("models/").unwrap_or(model);
        let url = format!("{}/models/{}:generateContent", base_url, clean_model);
        let gemini_contents = coalesce_gemini_contents(contents);

        let mut body = json!({
            "systemInstruction": { "parts": [{ "text": system_instruction }] },
            "contents": gemini_contents,
        });
        if !function_declarations.is_empty() {
            let sanitized = sanitize_gemini_function_declarations(function_declarations);
            body["tools"] = json!([{ "functionDeclarations": sanitized }]);
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
            .header("x-goog-api-key", api_key)
            .json(&body)
            .send()
            .await?;

        let res_json = parse_api_json_response("Gemini API", resp).await?;
        parse_model_turn(&res_json)
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

/// Bersihkan daftar `functionDeclarations` agar sesuai dengan subset `Schema` protobuf
/// milik Google Gemini `generateContent` API (menolak keyword seperti `additionalProperties`,
/// `$schema`, `default`, `oneOf`, dll. dengan HTTP 400).
pub fn sanitize_gemini_function_declarations(declarations: &[Value]) -> Vec<Value> {
    declarations
        .iter()
        .map(|decl| {
            let Some(obj) = decl.as_object() else {
                return decl.clone();
            };
            let mut out = obj.clone();
            if let Some(params) = obj.get("parameters") {
                out.insert("parameters".to_string(), sanitize_gemini_schema(params));
            }
            if let Some(resp) = obj.get("response") {
                out.insert("response".to_string(), sanitize_gemini_schema(resp));
            }
            Value::Object(out)
        })
        .collect()
}

/// Sanitasi rekursif satu node JSON Schema ke subset field yang diterima oleh
/// `google.ai.generativelanguage.v1beta.Schema`.
pub fn sanitize_gemini_schema(schema: &Value) -> Value {
    let Some(obj) = schema.as_object() else {
        return schema.clone();
    };

    const ALLOWED_KEYS: &[&str] = &[
        "format",
        "title",
        "description",
        "nullable",
        "enum",
        "maxItems",
        "minItems",
        "required",
        "propertyOrdering",
    ];

    let mut out = serde_json::Map::new();

    for &key in ALLOWED_KEYS {
        if let Some(val) = obj.get(key) {
            out.insert(key.to_string(), val.clone());
        }
    }

    if let Some(type_val) = obj.get("type") {
        match type_val {
            Value::String(s) => {
                out.insert("type".to_string(), Value::String(s.clone()));
            }
            Value::Array(arr) => {
                let mut has_null = false;
                let mut first_type: Option<String> = None;
                for item in arr {
                    if let Some(s) = item.as_str() {
                        if s == "null" {
                            has_null = true;
                        } else if first_type.is_none() {
                            first_type = Some(s.to_string());
                        }
                    }
                }
                if let Some(t) = first_type {
                    out.insert("type".to_string(), Value::String(t));
                }
                if has_null && !out.contains_key("nullable") {
                    out.insert("nullable".to_string(), Value::Bool(true));
                }
            }
            _ => {}
        }
    }

    // Jika schema memakai `anyOf`/`oneOf` tanpa `type` (mis. pola nullable Pydantic/Zod),
    // ambil varian non-null pertama agar tidak menjadi schema kosong.
    if !out.contains_key("type") && !out.contains_key("properties") {
        if let Some(variants) = obj
            .get("anyOf")
            .or_else(|| obj.get("oneOf"))
            .and_then(Value::as_array)
        {
            let has_null = variants
                .iter()
                .any(|v| v.get("type").and_then(Value::as_str) == Some("null"));
            if let Some(non_null) = variants
                .iter()
                .find(|v| v.get("type").and_then(Value::as_str) != Some("null"))
            {
                if let Value::Object(sanitized_variant) = sanitize_gemini_schema(non_null) {
                    for (k, v) in sanitized_variant {
                        out.entry(k).or_insert(v);
                    }
                }
            }
            if has_null && !out.contains_key("nullable") {
                out.insert("nullable".to_string(), Value::Bool(true));
            }
        }
    }

    // `properties` adalah map `<nama_parameter, Schema>`: pertahankan semua nama parameter
    // (termasuk bila ada parameter bernama `default`, `const`, dsb.) sambil mensanitasi nilainya.
    if let Some(Value::Object(props)) = obj.get("properties") {
        let mut sanitized_props = serde_json::Map::with_capacity(props.len());
        for (prop_name, prop_schema) in props {
            sanitized_props.insert(prop_name.clone(), sanitize_gemini_schema(prop_schema));
        }
        out.insert("properties".to_string(), Value::Object(sanitized_props));
    }

    if let Some(items) = obj.get("items") {
        match items {
            Value::Object(_) => {
                out.insert("items".to_string(), sanitize_gemini_schema(items));
            }
            Value::Array(arr) => {
                if let Some(first) = arr.first() {
                    out.insert("items".to_string(), sanitize_gemini_schema(first));
                }
            }
            _ => {}
        }
    }

    Value::Object(out)
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
    let mut thought_signatures = Vec::new();
    let mut function_calls = Vec::new();
    if let Some(parts) = content["parts"].as_array() {
        for part in parts {
            let part_sig = part["thoughtSignature"]
                .as_str()
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(str::to_string);
            // Simpan "thought summary" ke `thoughts` untuk ditampilkan di StepFigure aplikasi.
            if part["thought"].as_bool() == Some(true) {
                if let Some(t) = part["text"].as_str().map(str::trim).filter(|s| !s.is_empty()) {
                    thoughts.push(t.to_string());
                    thought_signatures.push(part_sig);
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
                    thought_signature: part_sig,
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
        thought_signatures,
        function_calls,
        usage,
    })
}
