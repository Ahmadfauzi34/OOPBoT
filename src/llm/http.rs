//! Helper HTTP & protokol bersama untuk seluruh transport LLM (`src/llm/`).

use super::GenerationOptions;
use crate::common::error::{extract_api_error_message, extract_error_from_value};
use crate::common::BoxError;
use serde_json::Value;
use std::collections::VecDeque;

/// Membaca respons HTTP, memvalidasi status sukses, mem-parse JSON, serta
/// mendeteksi objek `"error"` yang disematkan pada respons HTTP 200.
pub(super) async fn parse_api_json_response(
    provider_label: &str,
    resp: reqwest::Response,
) -> Result<Value, BoxError> {
    let status = resp.status();
    let body_text = resp.text().await.map_err(|e| {
        format!(
            "{} ({}): gagal membaca respons body: {}",
            provider_label, status, e
        )
    })?;

    if !status.is_success() {
        let msg = extract_api_error_message(&body_text);
        return Err(format!("{} {}: {}", provider_label, status, msg).into());
    }

    let res_json: Value = serde_json::from_str(&body_text).map_err(|e| {
        let preview = extract_api_error_message(&body_text);
        format!(
            "{} {} mengembalikan respons bukan JSON ({}): {}",
            provider_label, status, e, preview
        )
    })?;

    if let Some(err_val) = res_json.get("error").filter(|v| {
        v.is_object() || v.as_str().is_some_and(|s| !s.trim().is_empty())
    }) {
        let msg = extract_error_from_value(&res_json, 500)
            .unwrap_or_else(|| err_val.to_string().chars().take(400).collect());
        return Err(format!("{} ({}): {}", provider_label, status, msg).into());
    }

    Ok(res_json)
}

/// Mengambil ID fallback tertua yang cocok dengan nama tool (atau entri terdepan)
/// untuk mencocokkan `functionCall` tanpa ID dengan `functionResponse` pasangannya.
pub(super) fn take_pending_fallback_id(
    queue: &mut VecDeque<(String, String)>,
    name: &str,
) -> Option<String> {
    if let Some(pos) = queue.iter().position(|(_, n)| n == name) {
        return queue.remove(pos).map(|(id, _)| id);
    }
    queue.pop_front().map(|(id, _)| id)
}

/// Memetakan `thinking_level` dan `thinking_level_map` ke nilai effort OpenAI
/// (`reasoning_effort` untuk Chat Completions / `reasoning.effort` untuk Responses API).
pub(super) fn resolve_openai_reasoning_effort(
    base_url: &str,
    options: &GenerationOptions,
) -> Option<String> {
    if !options.reasoning {
        return None;
    }
    let level = options.thinking_level.as_deref()?;
    if level == "off" {
        return None;
    }
    let is_local = base_url.contains("127.0.0.1") || base_url.contains("localhost");
    options
        .thinking_level_map
        .as_ref()
        .and_then(|m| m.get(level))
        .and_then(|v| v.clone())
        .or_else(|| {
            if is_local {
                None
            } else {
                Some(
                    match level {
                        "minimal" | "low" => "low",
                        "medium" => "medium",
                        "high" | "xhigh" => "high",
                        other => other,
                    }
                    .to_string(),
                )
            }
        })
}
