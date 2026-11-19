//! Protokol Anthropic Messages API (`anthropic-messages` / `/messages`).

use super::http::{parse_api_json_response, take_pending_fallback_id};
use super::{FunctionCall, GenerationOptions, LlmClient, ModelTurn, TokenUsage};
use crate::common::BoxError;
use serde_json::{json, Value};
use std::collections::VecDeque;

impl LlmClient {
    #[allow(clippy::too_many_arguments)]
    pub(super) async fn generate_anthropic(
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
        let mut pending_fallback_ids: VecDeque<(String, String)> = VecDeque::new();

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
                    if let Some(last_content) =
                        messages.last_mut().and_then(|m| m["content"].as_array_mut())
                    {
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

        let res_json = parse_api_json_response("Anthropic API", resp).await?;

        let mut text = String::new();
        let mut thoughts = Vec::new();
        let mut function_calls = Vec::new();
        let mut raw_parts = Vec::new();

        let Some(blocks) = res_json["content"].as_array() else {
            return Err("Anthropic API mengembalikan respons tanpa field content".into());
        };
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
                    function_calls.push(FunctionCall {
                        id,
                        name,
                        args,
                        thought_signature: None,
                    });
                }
                _ => {}
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

        let thought_signatures = vec![None; thoughts.len()];
        Ok(ModelTurn {
            raw_content: json!({ "role": "model", "parts": raw_parts }),
            text: text.trim().to_string(),
            thoughts,
            thought_signatures,
            function_calls,
            usage,
        })
    }
}
