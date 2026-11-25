//! Compactor murni untuk `history` kanonik OOPBoT (anti-belah pasangan tool call,
//! batas prompt ketat, head-tail truncation UTF-8, dan chaining `<previous_summary>`).

use super::budget::estimate_value_tokens;
use serde_json::{json, Value};

pub const MAX_COMPACTION_TOOL_CHARS: usize = 4_000;
pub const MAX_COMPACTION_MSG_CHARS: usize = 8_000;
pub const MAX_COMPACTION_PROMPT_CHARS: usize = 120_000;

pub const COMPACTION_SUMMARY_OPEN: &str = "<compaction_summary>";
pub const COMPACTION_SUMMARY_CLOSE: &str = "</compaction_summary>";

/// Rencana pemadatan deterministik untuk `history` kanonik.
/// `history[..cut_index]` akan diringkas; `history[cut_index..]` dipertahankan utuh.
#[derive(Debug, Clone)]
pub struct CompactionPlan {
    pub cut_index: usize,
    pub summarized_messages: usize,
    pub kept_messages: usize,
    pub prompt: String,
}

/// Potong teks secara aman di batas UTF-8 dengan mempertahankan 2/3 awal dan 1/3 akhir
/// agar pesan error atau ringkasan di bagian tail tidak hilang saat compaction.
pub fn truncate_head_tail_chars(text: &str, max_chars: usize) -> String {
    let total_chars = text.chars().count();
    if total_chars <= max_chars {
        return text.to_string();
    }
    let head_len = (max_chars * 2) / 3;
    let tail_len = max_chars.saturating_sub(head_len);
    let head: String = text.chars().take(head_len).collect();
    let tail: String = text
        .chars()
        .skip(total_chars.saturating_sub(tail_len))
        .collect();
    let omitted = total_chars.saturating_sub(head_len + tail_len);
    format!(
        "{}\n...[{} karakter dipadatkan untuk compaction]...\n{}",
        head, omitted, tail
    )
}

/// Cek apakah satu pesan kanonik adalah balasan tool (`role == "user"` yang membawa `functionResponse`).
pub fn is_function_response_message(msg: &Value) -> bool {
    msg["role"].as_str() == Some("user")
        && msg["parts"].as_array().is_some_and(|parts| {
            parts
                .iter()
                .any(|p| p.get("functionResponse").is_some_and(|v| !v.is_null()))
        })
}

/// Titik potong valid hanya boleh jatuh di awal pesan `user` non-tool (`idx > 0`),
/// sehingga pasangan `model(functionCall)` + `user(functionResponse)` tidak pernah terbelah.
pub fn is_valid_canonical_cut_point(history: &[Value], idx: usize) -> bool {
    if idx == 0 || idx >= history.len() {
        return false;
    }
    let msg = &history[idx];
    msg["role"].as_str() == Some("user") && !is_function_response_message(msg)
}

/// Ambil isi `<compaction_summary>...</compaction_summary>` dari pesan awal `history`
/// hasil compaction sebelumnya agar ringkasan berikutnya berantai (tidak hilang).
pub fn extract_existing_compaction_summary(history: &[Value]) -> Option<String> {
    let first = history.first()?;
    if first["role"].as_str() != Some("user") {
        return None;
    }
    let text = first["parts"]
        .as_array()?
        .iter()
        .find_map(|p| p["text"].as_str())?;
    let start = text.find(COMPACTION_SUMMARY_OPEN)? + COMPACTION_SUMMARY_OPEN.len();
    let rest = &text[start..];
    let end = rest.find(COMPACTION_SUMMARY_CLOSE)?;
    let inner = rest[..end].trim();
    (!inner.is_empty()).then(|| inner.to_string())
}

fn format_canonical_message_for_compaction(msg: &Value) -> Option<String> {
    let role = msg["role"].as_str().unwrap_or_default();
    let parts = msg["parts"].as_array()?;
    let mut lines = Vec::new();

    match role {
        "user" => {
            for p in parts {
                if let Some(fr) = p.get("functionResponse").and_then(|v| v.as_object()) {
                    let name = fr.get("name").and_then(|v| v.as_str()).unwrap_or("tool");
                    let resp_str = fr
                        .get("response")
                        .map(|v| v.to_string())
                        .unwrap_or_default();
                    lines.push(format!(
                        "[ToolResult {}]: {}",
                        name,
                        truncate_head_tail_chars(&resp_str, MAX_COMPACTION_TOOL_CHARS)
                    ));
                } else if let Some(text) = p["text"].as_str().filter(|s| !s.trim().is_empty()) {
                    // Lewati wrapper summary lama karena sudah dibawa ke <previous_summary>.
                    if text.contains(COMPACTION_SUMMARY_OPEN)
                        && text.contains(COMPACTION_SUMMARY_CLOSE)
                    {
                        continue;
                    }
                    lines.push(format!(
                        "[User]: {}",
                        truncate_head_tail_chars(text, MAX_COMPACTION_MSG_CHARS)
                    ));
                } else if p.get("inlineData").is_some() {
                    lines.push("[User]: [gambar inline]".to_string());
                }
            }
        }
        "model" => {
            let mut model_parts = Vec::new();
            for p in parts {
                if p["thought"].as_bool() == Some(true) {
                    continue;
                }
                if let Some(fc) = p.get("functionCall").and_then(|v| v.as_object()) {
                    let name = fc.get("name").and_then(|v| v.as_str()).unwrap_or("tool");
                    let args_str = fc
                        .get("args")
                        .map(|v| truncate_head_tail_chars(&v.to_string(), MAX_COMPACTION_TOOL_CHARS))
                        .unwrap_or_default();
                    model_parts.push(format!("[ToolCall {}({})]", name, args_str));
                } else if let Some(text) = p["text"].as_str().filter(|s| !s.trim().is_empty()) {
                    model_parts.push(truncate_head_tail_chars(text, MAX_COMPACTION_MSG_CHARS));
                }
            }
            if !model_parts.is_empty() {
                lines.push(format!("[Assistant]: {}", model_parts.join("\n")));
            }
        }
        _ => {}
    }

    (!lines.is_empty()).then(|| lines.join("\n"))
}

fn build_canonical_compaction_prompt(
    to_summarize: &[Value],
    previous_summary: Option<&str>,
    custom_instructions: Option<&str>,
) -> String {
    let mut prompt = String::from(
        "Summarize the conversation clearly and preserve key goals, decisions, file paths, and open tasks (Ringkas percakapan sebelumnya secara akurat dan padat).\n",
    );
    if let Some(prev) = previous_summary.filter(|s| !s.trim().is_empty()) {
        prompt.push_str("\n<previous_summary>\n");
        prompt.push_str(&truncate_head_tail_chars(prev.trim(), 16_000));
        prompt.push_str("\n</previous_summary>\n");
    }

    let formatted: Vec<String> = to_summarize
        .iter()
        .filter_map(format_canonical_message_for_compaction)
        .collect();

    let instructions_block = custom_instructions
        .filter(|s| !s.trim().is_empty())
        .map(|inst| {
            format!(
                "\n<custom_instructions>\n{}\n</custom_instructions>\n",
                inst.trim()
            )
        })
        .unwrap_or_default();

    let fixed_overhead = prompt.chars().count() + instructions_block.chars().count() + 64;
    let conv_budget = MAX_COMPACTION_PROMPT_CHARS.saturating_sub(fixed_overhead);
    let total_conv_chars: usize = formatted.iter().map(|l| l.chars().count() + 1).sum();

    prompt.push_str("\n<conversation>\n");
    if total_conv_chars <= conv_budget {
        for line in &formatted {
            prompt.push_str(line);
            prompt.push('\n');
        }
    } else {
        let head_budget = conv_budget / 3;
        let tail_budget = conv_budget.saturating_sub(head_budget);
        let mut head_lines = Vec::new();
        let mut head_used = 0usize;
        for line in &formatted {
            let cost = line.chars().count() + 1;
            if head_used + cost > head_budget && !head_lines.is_empty() {
                break;
            }
            head_used += cost;
            head_lines.push(line.as_str());
        }
        let mut tail_lines_rev = Vec::new();
        let mut tail_used = 0usize;
        for line in formatted[head_lines.len()..].iter().rev() {
            let cost = line.chars().count() + 1;
            if tail_used + cost > tail_budget && !tail_lines_rev.is_empty() {
                break;
            }
            tail_used += cost;
            tail_lines_rev.push(line.as_str());
        }
        tail_lines_rev.reverse();
        let omitted = formatted
            .len()
            .saturating_sub(head_lines.len() + tail_lines_rev.len());
        for line in head_lines {
            prompt.push_str(line);
            prompt.push('\n');
        }
        if omitted > 0 {
            prompt.push_str(&format!(
                "[... {} entri pertengahan dipadatkan agar prompt compaction aman di dalam context window ...]\n",
                omitted
            ));
        }
        for line in tail_lines_rev {
            prompt.push_str(line);
            prompt.push('\n');
        }
    }
    prompt.push_str("</conversation>\n");
    if !instructions_block.is_empty() {
        prompt.push_str(&instructions_block);
    }
    prompt
}

/// Rencanakan pemadatan `history` kanonik secara deterministik.
/// Mengembalikan `None` jika `history` terlalu pendek (< 2 pesan) untuk dipadatkan.
pub fn plan_canonical_compaction(
    history: &[Value],
    keep_recent_tokens: u64,
    previous_summary: Option<&str>,
    custom_instructions: Option<&str>,
) -> Option<CompactionPlan> {
    if history.len() < 2 {
        return None;
    }

    let inherited_summary = previous_summary
        .map(str::to_string)
        .or_else(|| extract_existing_compaction_summary(history));

    // Hindari menganggap ack summary sebelumnya di indeks 0..=1 sebagai satu-satunya isi yang diringkas.
    let min_cut_idx = if extract_existing_compaction_summary(history).is_some() && history.len() > 2
    {
        2
    } else {
        1
    };

    let total_tokens: u64 = history.iter().map(estimate_value_tokens).sum();
    let mut chosen_cut: Option<usize> = None;

    if total_tokens > keep_recent_tokens {
        let mut kept_acc = 0u64;
        let mut raw_idx = history.len();
        for idx in (0..history.len()).rev() {
            kept_acc += estimate_value_tokens(&history[idx]);
            raw_idx = idx;
            if kept_acc >= keep_recent_tokens {
                break;
            }
        }
        // Cari cut-point valid terdekat di <= raw_idx, lalu >= raw_idx jika tidak ada.
        for idx in (min_cut_idx..=raw_idx.min(history.len() - 1)).rev() {
            if is_valid_canonical_cut_point(history, idx) {
                chosen_cut = Some(idx);
                break;
            }
        }
        if chosen_cut.is_none() {
            for idx in raw_idx.max(min_cut_idx)..history.len() {
                if is_valid_canonical_cut_point(history, idx) {
                    chosen_cut = Some(idx);
                    break;
                }
            }
        }
    }

    // Jika total masih di bawah keep_recent_tokens (mis. manual /compact), pertahankan giliran user terakhir.
    if chosen_cut.is_none() {
        chosen_cut = (min_cut_idx..history.len())
            .rev()
            .find(|&idx| is_valid_canonical_cut_point(history, idx));
    }

    // Retain-none fallback ketika seluruh history hanya terdiri dari 1 giliran user (+ rantai tool-nya).
    let cut_index = chosen_cut.unwrap_or(history.len());
    if cut_index == 0 {
        return None;
    }

    let to_summarize = &history[..cut_index];
    let kept_messages = history.len().saturating_sub(cut_index);
    let prompt = build_canonical_compaction_prompt(
        to_summarize,
        inherited_summary.as_deref(),
        custom_instructions,
    );

    Some(CompactionPlan {
        cut_index,
        summarized_messages: to_summarize.len(),
        kept_messages,
        prompt,
    })
}

/// Rakit ulang `history` kanonik setelah compaction sambil menjaga alternasi `user` -> `model`.
pub fn rebuild_canonical_history(summary: &str, kept_tail: &[Value]) -> Vec<Value> {
    let clean = summary.trim();
    let mut rebuilt = Vec::with_capacity(kept_tail.len() + 2);
    rebuilt.push(json!({
        "role": "user",
        "parts": [{
            "text": format!(
                "{}\n{}\n{}",
                COMPACTION_SUMMARY_OPEN, clean, COMPACTION_SUMMARY_CLOSE
            )
        }]
    }));
    rebuilt.push(json!({
        "role": "model",
        "parts": [{
            "text": "Ringkasan konteks sebelumnya dipahami. Saya siap melanjutkan."
        }]
    }));
    rebuilt.extend_from_slice(kept_tail);
    rebuilt
}
