//! Protokol OpenAI Responses API (`openai-responses` / `/responses`) dan parser responsnya.

use super::http::{
    parse_api_json_response, resolve_openai_reasoning_effort, take_pending_fallback_id,
};
use super::{FunctionCall, GenerationOptions, LlmClient, ModelTurn, TokenUsage};
use crate::common::BoxError;
use serde_json::{json, Value};
use std::collections::VecDeque;

impl LlmClient {
    #[allow(clippy::too_many_arguments)]
    pub(super) async fn generate_openai_responses(
        &self,
        api_key: &str,
        base_url: &str,
        model: &str,
        system_instruction: &str,
        contents: &[Value],
        function_declarations: &[Value],
        options: &GenerationOptions,
    ) -> Result<ModelTurn, BoxError> {
        let url = format!("{}/responses", base_url);
        let mut input_items: Vec<Value> = Vec::new();
        let mut fallback_call_seq = 0usize;
        let mut pending_fallback_ids: VecDeque<(String, String)> = VecDeque::new();

        for item in contents {
            let role = item["role"].as_str().unwrap_or("user");
            let Some(parts) = item["parts"].as_array() else {
                continue;
            };
            if role == "model" {
                let mut text_buf = String::new();
                let mut pending_calls: Vec<Value> = Vec::new();
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
                        pending_calls.push(json!({
                            "type": "function_call",
                            "call_id": call_id,
                            "name": name,
                            "arguments": args_str,
                        }));
                    }
                }
                if !text_buf.is_empty() {
                    input_items.push(json!({
                        "type": "message",
                        "role": "assistant",
                        "content": [{ "type": "output_text", "text": text_buf }]
                    }));
                }
                input_items.extend(pending_calls);
            } else {
                let mut user_content: Vec<Value> = Vec::new();
                for part in parts {
                    if let Some(t) = part["text"].as_str() {
                        user_content.push(json!({ "type": "input_text", "text": t }));
                    }
                    if let Some(inline) = part.get("inlineData") {
                        let mime = inline["mimeType"].as_str().unwrap_or("image/jpeg");
                        let data = inline["data"].as_str().unwrap_or_default();
                        if !data.is_empty() {
                            user_content.push(json!({
                                "type": "input_image",
                                "image_url": format!("data:{};base64,{}", mime, data)
                            }));
                        }
                    }
                    if let Some(fr) = part.get("functionResponse") {
                        if !user_content.is_empty() {
                            push_responses_user_message(&mut input_items, &mut user_content);
                        }
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
                        let output_str =
                            serde_json::to_string(fr.get("response").unwrap_or(&json!({})))
                                .unwrap_or_else(|_| "{}".into());
                        input_items.push(json!({
                            "type": "function_call_output",
                            "call_id": call_id,
                            "output": output_str
                        }));
                    }
                }
                if !user_content.is_empty() {
                    push_responses_user_message(&mut input_items, &mut user_content);
                }
            }
        }

        let mut body = json!({
            "model": model,
            "input": input_items,
        });
        if !system_instruction.trim().is_empty() {
            body["instructions"] = json!(system_instruction);
        }
        if let Some(max_tok) = options.max_tokens.filter(|&n| n > 0) {
            body["max_output_tokens"] = json!(max_tok);
        }
        if let Some(effort) = resolve_openai_reasoning_effort(base_url, options) {
            body["reasoning"] = json!({ "effort": effort });
        }
        if !function_declarations.is_empty() {
            let tools: Vec<Value> = function_declarations
                .iter()
                .map(|decl| {
                    json!({
                        "type": "function",
                        "name": decl["name"],
                        "description": decl["description"],
                        "parameters": decl["parameters"],
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

        let res_json = parse_api_json_response("Provider Responses API", resp).await?;
        if res_json["status"].as_str() == Some("failed") {
            let reason = res_json["error"]["message"]
                .as_str()
                .or_else(|| res_json["incomplete_details"]["reason"].as_str())
                .unwrap_or("status failed");
            return Err(format!("Provider Responses API gagal: {}", reason).into());
        }

        Ok(parse_openai_responses_turn(&res_json))
    }
}

fn push_responses_user_message(input_items: &mut Vec<Value>, user_content: &mut Vec<Value>) {
    if let Some(last) = input_items.last_mut() {
        if last["type"].as_str() == Some("message") && last["role"].as_str() == Some("user") {
            if let Some(arr) = last["content"].as_array_mut() {
                arr.append(user_content);
                return;
            }
        }
    }
    let taken = std::mem::take(user_content);
    input_items.push(json!({
        "type": "message",
        "role": "user",
        "content": taken
    }));
}

pub fn parse_openai_responses_turn(res_json: &Value) -> ModelTurn {
    let mut text_parts: Vec<String> = Vec::new();
    let mut thoughts: Vec<String> = Vec::new();
    let mut function_calls: Vec<FunctionCall> = Vec::new();
    let mut raw_parts: Vec<Value> = Vec::new();

    if let Some(output) = res_json["output"].as_array() {
        for item in output {
            match item["type"].as_str().unwrap_or_default() {
                "reasoning" => {
                    if let Some(summary) = item["summary"].as_array() {
                        for s in summary {
                            if let Some(t) = s["text"].as_str().map(str::trim).filter(|v| !v.is_empty()) {
                                thoughts.push(t.to_string());
                                raw_parts.push(json!({ "thought": true, "text": t }));
                            }
                        }
                    }
                    if let Some(content) = item["content"].as_array() {
                        for c in content {
                            if let Some(t) = c["text"].as_str().map(str::trim).filter(|v| !v.is_empty()) {
                                thoughts.push(t.to_string());
                                raw_parts.push(json!({ "thought": true, "text": t }));
                            }
                        }
                    }
                }
                "message" => {
                    if let Some(content) = item["content"].as_array() {
                        for part in content {
                            let ptype = part["type"].as_str().unwrap_or_default();
                            if ptype == "output_text" || ptype == "text" || ptype.is_empty() {
                                if let Some(t) = part["text"].as_str() {
                                    text_parts.push(t.to_string());
                                }
                            }
                        }
                    }
                }
                "function_call" => {
                    let id = item["call_id"]
                        .as_str()
                        .or_else(|| item["id"].as_str())
                        .map(str::to_string);
                    let name = item["name"].as_str().unwrap_or_default().to_string();
                    let args = match item.get("arguments") {
                        Some(Value::String(s)) => serde_json::from_str(s).unwrap_or_else(|_| json!({})),
                        Some(other) if !other.is_null() => other.clone(),
                        _ => json!({}),
                    };
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
                _ => {}
            }
        }
    }

    let text = if !text_parts.is_empty() {
        text_parts.join("").trim().to_string()
    } else {
        res_json["output_text"]
            .as_str()
            .unwrap_or_default()
            .trim()
            .to_string()
    };
    if !text.is_empty() {
        raw_parts.insert(thoughts.len(), json!({ "text": text }));
    }

    let usage = res_json.get("usage").and_then(|u| {
        let inp = u["input_tokens"]
            .as_u64()
            .or_else(|| u["prompt_tokens"].as_u64())?;
        let out = u["output_tokens"]
            .as_u64()
            .or_else(|| u["completion_tokens"].as_u64())
            .unwrap_or(0);
        Some(TokenUsage {
            input_tokens: inp,
            output_tokens: out,
        })
    });

    let thought_signatures = vec![None; thoughts.len()];
    ModelTurn {
        raw_content: json!({
            "role": "model",
            "parts": raw_parts
        }),
        text,
        thoughts,
        thought_signatures,
        function_calls,
        usage,
    }
}
