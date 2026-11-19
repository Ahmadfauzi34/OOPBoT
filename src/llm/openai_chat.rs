//! Protokol OpenAI Chat Completions (`openai-completions` / `/chat/completions`),
//! mencakup OpenRouter, Z.ai, Kimi, LocalModelServer (`oopbot-local`), dan gateway OpenAI-compat lainnya.

use super::http::{
    parse_api_json_response, resolve_openai_reasoning_effort, take_pending_fallback_id,
};
use super::{FunctionCall, GenerationOptions, LlmClient, ModelTurn, TokenUsage};
use crate::common::BoxError;
use serde_json::{json, Value};
use std::collections::VecDeque;

impl LlmClient {
    #[allow(clippy::too_many_arguments)]
    pub(super) async fn generate_openai_compatible(
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
        let mut pending_fallback_ids: VecDeque<(String, String)> = VecDeque::new();

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
        if let Some(effort) = resolve_openai_reasoning_effort(base_url, options) {
            body["reasoning_effort"] = json!(effort);
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

        let res_json = parse_api_json_response("Provider API", resp).await?;
        let Some(choice) = res_json["choices"].as_array().and_then(|c| c.first()) else {
            return Err("Provider API mengembalikan respons tanpa choices".into());
        };
        let message = &choice["message"];
        if !message.is_object() {
            return Err("Provider API mengembalikan choice tanpa objek message".into());
        }
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
                function_calls.push(FunctionCall {
                    id,
                    name,
                    args,
                    thought_signature: None,
                });
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

        let thought_signatures = vec![None; thoughts.len()];
        Ok(ModelTurn {
            raw_content: json!({ "role": "model", "parts": raw_parts }),
            text,
            thoughts,
            thought_signatures,
            function_calls,
            usage,
        })
    }
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
