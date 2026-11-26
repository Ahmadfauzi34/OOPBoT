//! Compactor murni untuk `history` kanonik OOPBoT (anti-belah pasangan tool call,
//! batas prompt ketat, head-tail truncation UTF-8, dan chaining `<previous_summary>`).

use super::budget::estimate_value_tokens;
use serde_json::{json, Value};

pub const MAX_COMPACTION_TOOL_CHARS: usize = 4_000;
pub const MAX_COMPACTION_MSG_CHARS: usize = 8_000;
pub const MAX_COMPACTION_INSTRUCTIONS_CHARS: usize = 4_000;
pub const MAX_COMPACTION_PREVIOUS_SUMMARY_CHARS: usize = 16_000;
pub const MAX_COMPACTION_PROMPT_CHARS: usize = 120_000;
pub const MAX_COMPACTION_INLINE_IMAGES: usize = 4;

pub const COMPACTION_SUMMARY_OPEN: &str = "<compaction_summary>";
pub const COMPACTION_SUMMARY_CLOSE: &str = "</compaction_summary>";
const COMPACTION_SUMMARY_ACK_TEXT: &str =
    "Ringkasan konteks sebelumnya dipahami. Saya siap melanjutkan.";

/// Rencana pemadatan deterministik untuk `history` kanonik.
/// `history[..cut_index]` akan diringkas; `history[cut_index..]` dipertahankan utuh.
#[derive(Debug, Clone)]
pub struct CompactionPlan {
    pub cut_index: usize,
    pub summarized_messages: usize,
    pub kept_messages: usize,
    pub prompt: String,
    /// Lampiran `inlineData` terbaru dari pesan yang diringkas agar summarizer multimodal
    /// tetap dapat membaca detail visual sebelum pesan aslinya dipadatkan.
    pub inline_images: Vec<Value>,
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

fn single_text_part(msg: &Value) -> Option<&str> {
    let parts = msg["parts"].as_array()?;
    if parts.len() != 1 {
        return None;
    }
    parts[0]["text"].as_str()
}

/// Ambil isi `<compaction_summary>...</compaction_summary>` hanya dari pasangan prefiks
/// sintetis `[user(<compaction_summary>...), model(ack)]` buatan `rebuild_canonical_history`,
/// sehingga pesan user normal yang kebetulan menyebut marker literal tidak salah dianggap summary.
pub fn extract_existing_compaction_summary(history: &[Value]) -> Option<String> {
    if history.len() < 2 {
        return None;
    }
    let first = &history[0];
    let second = &history[1];
    if first["role"].as_str() != Some("user") || second["role"].as_str() != Some("model") {
        return None;
    }
    if single_text_part(second) != Some(COMPACTION_SUMMARY_ACK_TEXT) {
        return None;
    }
    let text = single_text_part(first)?.trim();
    let inner = text
        .strip_prefix(COMPACTION_SUMMARY_OPEN)?
        .strip_suffix(COMPACTION_SUMMARY_CLOSE)?
        .trim();
    (!inner.is_empty()).then(|| inner.to_string())
}

fn has_valid_inline_image(part: &Value) -> bool {
    part.get("inlineData")
        .and_then(|v| v.as_object())
        .and_then(|obj| obj.get("data"))
        .and_then(|v| v.as_str())
        .is_some_and(|s| !s.is_empty())
}

fn clone_valid_inline_image(part: &Value) -> Option<Value> {
    let inline = part.get("inlineData")?.as_object()?;
    let data = inline.get("data")?.as_str().filter(|s| !s.is_empty())?;
    let mime = inline
        .get("mimeType")
        .and_then(|v| v.as_str())
        .unwrap_or("image/png");
    Some(json!({
        "inlineData": {
            "mimeType": mime,
            "data": data
        }
    }))
}

/// Placeholder dengan panjang tetap (51 karakter) setara `"[gambar inline #1 (terlampir pada input ringkasan)]"`
/// sehingga penggantian pasca-perakitan prompt tidak pernah menambah panjang prompt melampaui budget.
fn inline_image_placeholder(msg_idx: usize, part_idx: usize) -> String {
    let token = format!(
        "\u{1f}IMG:{:06}:{:06}:{:\u{1f}<31}\u{1f}",
        msg_idx, part_idx, ""
    );
    debug_assert_eq!(token.chars().count(), 51);
    token
}

fn format_canonical_message_for_compaction(
    msg_idx: usize,
    msg: &Value,
    discovered_images: &mut Vec<(String, usize, usize)>,
) -> Option<String> {
    let role = msg["role"].as_str().unwrap_or_default();
    let parts = msg["parts"].as_array()?;
    let mut lines = Vec::new();

    match role {
        "user" => {
            for (part_idx, p) in parts.iter().enumerate() {
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
                    lines.push(format!(
                        "[User]: {}",
                        truncate_head_tail_chars(text, MAX_COMPACTION_MSG_CHARS)
                    ));
                } else if p.get("inlineData").is_some() {
                    if has_valid_inline_image(p) {
                        let placeholder = inline_image_placeholder(msg_idx, part_idx);
                        lines.push(format!("[User]: {}", placeholder));
                        discovered_images.push((placeholder, msg_idx, part_idx));
                    } else {
                        lines.push("[User]: [gambar inline sebelumnya]".to_string());
                    }
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

/// Potong teks secara aman di batas UTF-8 sambil menjamin panjang hasil akhir
/// (termasuk teks penanda pemadatan) tidak melebihi `max_chars`.
pub fn truncate_head_tail_within_chars(text: &str, max_chars: usize) -> String {
    let total_chars = text.chars().count();
    if total_chars <= max_chars {
        return text.to_string();
    }
    let marker_probe = format!(
        "\n...[{} karakter dipadatkan untuk compaction]...\n",
        total_chars
    );
    let marker_len = marker_probe.chars().count();
    if max_chars <= marker_len + 4 {
        return text.chars().take(max_chars).collect();
    }
    let content_budget = max_chars.saturating_sub(marker_len);
    let head_len = (content_budget * 2) / 3;
    let tail_len = content_budget.saturating_sub(head_len);
    let head: String = text.chars().take(head_len).collect();
    let tail: String = text
        .chars()
        .skip(total_chars.saturating_sub(tail_len))
        .collect();
    let omitted = total_chars.saturating_sub(head_len + tail_len);
    let result = format!(
        "{}\n...[{} karakter dipadatkan untuk compaction]...\n{}",
        head, omitted, tail
    );
    debug_assert!(result.chars().count() <= max_chars);
    result
}

/// Rakit prompt compaction dengan jaminan ketat total karakter `<= MAX_COMPACTION_PROMPT_CHARS`.
pub fn assemble_compaction_prompt(
    formatted: &[String],
    previous_summary: Option<&str>,
    custom_instructions: Option<&str>,
) -> String {
    assemble_compaction_prompt_with_limit(
        formatted,
        previous_summary,
        custom_instructions,
        MAX_COMPACTION_PROMPT_CHARS,
    )
}

/// Rakit prompt compaction dengan jaminan ketat total karakter `<= max_prompt_chars.min(MAX_COMPACTION_PROMPT_CHARS)`,
/// termasuk skala proporsional untuk `<previous_summary>`, `<custom_instructions>`, dan entri percakapan besar
/// agar model ber-context-window kecil (< 30k token) tidak gagal saat meringkas histori besar.
pub fn assemble_compaction_prompt_with_limit(
    formatted: &[String],
    previous_summary: Option<&str>,
    custom_instructions: Option<&str>,
    max_prompt_chars: usize,
) -> String {
    let effective_limit = max_prompt_chars.min(MAX_COMPACTION_PROMPT_CHARS);
    let mut prompt = String::from(
        "Summarize the conversation clearly and preserve key goals, decisions, file paths, and open tasks (Ringkas percakapan sebelumnya secara akurat dan padat).\n",
    );
    let conv_open = "\n<conversation>\n";
    let conv_close = "</conversation>\n";
    let omitted_probe = format!(
        "[... {} entri pertengahan dipadatkan agar prompt compaction aman di dalam context window ...]\n",
        formatted.len()
    );
    let base_overhead = prompt.chars().count()
        + conv_open.chars().count()
        + conv_close.chars().count()
        + omitted_probe.chars().count();
    let available_body = effective_limit.saturating_sub(base_overhead);

    if let Some(prev) = previous_summary.filter(|s| !s.trim().is_empty()) {
        let prev_tags_len = "\n<previous_summary>\n\n</previous_summary>\n".chars().count();
        let prev_cap = MAX_COMPACTION_PREVIOUS_SUMMARY_CHARS
            .min(available_body / 4)
            .saturating_sub(prev_tags_len);
        if prev_cap > 0 {
            prompt.push_str("\n<previous_summary>\n");
            prompt.push_str(&truncate_head_tail_within_chars(prev.trim(), prev_cap));
            prompt.push_str("\n</previous_summary>\n");
        }
    }

    let instructions_block = custom_instructions
        .filter(|s| !s.trim().is_empty())
        .and_then(|inst| {
            let inst_tags_len = "\n<custom_instructions>\n\n</custom_instructions>\n"
                .chars()
                .count();
            let inst_cap = MAX_COMPACTION_INSTRUCTIONS_CHARS
                .min(available_body / 6)
                .saturating_sub(inst_tags_len);
            (inst_cap > 0).then(|| {
                format!(
                    "\n<custom_instructions>\n{}\n</custom_instructions>\n",
                    truncate_head_tail_within_chars(inst.trim(), inst_cap)
                )
            })
        })
        .unwrap_or_default();

    let fixed_overhead = prompt.chars().count()
        + instructions_block.chars().count()
        + conv_open.chars().count()
        + conv_close.chars().count()
        + omitted_probe.chars().count();
    let conv_budget = effective_limit.saturating_sub(fixed_overhead);
    let total_conv_chars: usize = formatted.iter().map(|l| l.chars().count() + 1).sum();

    prompt.push_str(conv_open);
    if total_conv_chars <= conv_budget {
        for line in formatted {
            prompt.push_str(line);
            prompt.push('\n');
        }
    } else {
        // Pertahankan pesan awal (tujuan/konteks awal) + pesan paling baru dalam batas budget.
        let head_budget = conv_budget / 3;
        let tail_budget = conv_budget.saturating_sub(head_budget);
        let mut head_lines: Vec<String> = Vec::new();
        let mut head_used = 0usize;
        let mut head_count = 0usize;
        for line in formatted {
            let cost = line.chars().count() + 1;
            if head_used + cost > head_budget {
                if head_lines.is_empty() && head_budget > 1 {
                    let clipped = truncate_head_tail_within_chars(line, head_budget - 1);
                    head_lines.push(clipped);
                    head_count = 1;
                }
                break;
            }
            head_used += cost;
            head_lines.push(line.clone());
            head_count += 1;
        }
        let mut tail_lines_rev: Vec<String> = Vec::new();
        let mut tail_used = 0usize;
        let mut tail_count = 0usize;
        for line in formatted[head_count..].iter().rev() {
            let cost = line.chars().count() + 1;
            if tail_used + cost > tail_budget {
                if tail_lines_rev.is_empty() && tail_budget > 1 {
                    let clipped = truncate_head_tail_within_chars(line, tail_budget - 1);
                    tail_lines_rev.push(clipped);
                    tail_count = 1;
                }
                break;
            }
            tail_used += cost;
            tail_lines_rev.push(line.clone());
            tail_count += 1;
        }
        tail_lines_rev.reverse();
        let omitted = formatted.len().saturating_sub(head_count + tail_count);
        for line in &head_lines {
            prompt.push_str(line);
            prompt.push('\n');
        }
        if omitted > 0 {
            prompt.push_str(&format!(
                "[... {} entri pertengahan dipadatkan agar prompt compaction aman di dalam context window ...]\n",
                omitted
            ));
        }
        for line in &tail_lines_rev {
            prompt.push_str(line);
            prompt.push('\n');
        }
    }
    prompt.push_str(conv_close);
    if !instructions_block.is_empty() {
        prompt.push_str(&instructions_block);
    }
    if prompt.chars().count() > effective_limit {
        prompt = truncate_head_tail_within_chars(&prompt, effective_limit);
    }
    debug_assert!(prompt.chars().count() <= effective_limit);
    prompt
}

fn build_canonical_compaction_prompt(
    to_summarize: &[Value],
    previous_summary: Option<&str>,
    custom_instructions: Option<&str>,
    max_prompt_chars: usize,
    max_image_tokens: u64,
) -> (String, Vec<Value>) {
    let mut discovered_images: Vec<(String, usize, usize)> = Vec::new();
    let formatted: Vec<String> = to_summarize
        .iter()
        .enumerate()
        .filter_map(|(msg_idx, msg)| {
            format_canonical_message_for_compaction(msg_idx, msg, &mut discovered_images)
        })
        .collect();
    let mut prompt = assemble_compaction_prompt_with_limit(
        &formatted,
        previous_summary,
        custom_instructions,
        max_prompt_chars,
    );

    // Pilih hanya indeks gambar yang placeholder-nya benar-benar bertahan di dalam prompt akhir
    // (tidak terpotong oleh pemangkasan pertengahan), maksimal MAX_COMPACTION_INLINE_IMAGES
    // dan dalam batas anggaran token gambar `max_image_tokens` (prioritaskan gambar terbaru).
    // Cloning payload base64 ditunda hingga daftar survivor final terpilih.
    let mut selected_indices: Vec<usize> = Vec::new();
    let mut used_image_tokens = 0u64;
    if max_image_tokens > 0 {
        for (idx, (token, msg_idx, part_idx)) in discovered_images.iter().enumerate().rev() {
            if selected_indices.len() >= MAX_COMPACTION_INLINE_IMAGES {
                break;
            }
            if !prompt.contains(token.as_str()) {
                continue;
            }
            let Some(part) = to_summarize
                .get(*msg_idx)
                .and_then(|m| m["parts"].as_array())
                .and_then(|parts| parts.get(*part_idx))
            else {
                continue;
            };
            let cost = estimate_value_tokens(part);
            if used_image_tokens.saturating_add(cost) > max_image_tokens {
                continue;
            }
            used_image_tokens = used_image_tokens.saturating_add(cost);
            selected_indices.push(idx);
        }
        selected_indices.reverse();
    }

    let mut inline_images = Vec::with_capacity(selected_indices.len());
    for (order, &disc_idx) in selected_indices.iter().enumerate() {
        let (token, msg_idx, part_idx) = &discovered_images[disc_idx];
        let Some(img_val) = to_summarize
            .get(*msg_idx)
            .and_then(|m| m["parts"].as_array())
            .and_then(|parts| parts.get(*part_idx))
            .and_then(clone_valid_inline_image)
        else {
            continue;
        };
        let label = format!(
            "[gambar inline #{} (terlampir pada input ringkasan)]",
            order + 1
        );
        prompt = prompt.replace(token.as_str(), &label);
        inline_images.push(img_val);
    }
    for (token, _, _) in &discovered_images {
        if prompt.contains(token.as_str()) {
            prompt = prompt.replace(token.as_str(), "[gambar inline sebelumnya]");
        }
    }
    // Bersihkan kemungkinan fragmen kontrol \u{1f} jika satu baris sangat panjang sempat dipotong di tengah token.
    if prompt.contains('\u{1f}') {
        prompt = prompt.replace('\u{1f}', "");
    }

    debug_assert!(prompt.chars().count() <= max_prompt_chars.min(MAX_COMPACTION_PROMPT_CHARS));
    (prompt, inline_images)
}

/// Rencanakan pemadatan `history` kanonik secara deterministik dengan batas prompt default.
/// Mengembalikan `None` jika `history` terlalu pendek (< 2 pesan) atau tidak ada pesan baru untuk dipadatkan.
pub fn plan_canonical_compaction(
    history: &[Value],
    keep_recent_tokens: u64,
    previous_summary: Option<&str>,
    custom_instructions: Option<&str>,
) -> Option<CompactionPlan> {
    plan_canonical_compaction_with_limits(
        history,
        keep_recent_tokens,
        previous_summary,
        custom_instructions,
        MAX_COMPACTION_PROMPT_CHARS,
        u64::MAX,
    )
}

/// Rencanakan pemadatan `history` kanonik dengan batas panjang prompt compaction yang sesuai
/// dengan context window model aktif (`max_prompt_chars`).
pub fn plan_canonical_compaction_with_prompt_limit(
    history: &[Value],
    keep_recent_tokens: u64,
    previous_summary: Option<&str>,
    custom_instructions: Option<&str>,
    max_prompt_chars: usize,
) -> Option<CompactionPlan> {
    plan_canonical_compaction_with_limits(
        history,
        keep_recent_tokens,
        previous_summary,
        custom_instructions,
        max_prompt_chars,
        u64::MAX,
    )
}

/// Rencanakan pemadatan `history` kanonik dengan batas panjang prompt teks dan anggaran token gambar
/// agar label `[gambar inline #N]` 100% sinkron dengan `CompactionPlan::inline_images`.
pub fn plan_canonical_compaction_with_limits(
    history: &[Value],
    keep_recent_tokens: u64,
    previous_summary: Option<&str>,
    custom_instructions: Option<&str>,
    max_prompt_chars: usize,
    max_image_tokens: u64,
) -> Option<CompactionPlan> {
    if history.len() < 2 {
        return None;
    }

    let existing_summary = extract_existing_compaction_summary(history);
    let synthetic_prefix_len = if existing_summary.is_some() { 2 } else { 0 };
    let has_custom_instructions = custom_instructions.is_some_and(|s| !s.trim().is_empty());
    let inherited_summary = previous_summary
        .map(str::to_string)
        .or(existing_summary);

    // Cut-point setelah prefiks summary sintetis `[0..2]` harus berada di indeks `>= 3`
    // agar selalu ada minimal 1 pesan baru di `to_summarize[2..]`; jika tidak ada cut-point valid
    // (mis. baru 1 giliran user + rantai tool setelah compaction), gunakan retain-none fallback.
    let min_cut_idx = if synthetic_prefix_len > 0 {
        synthetic_prefix_len + 1
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
    // Hanya lewati pasangan sintetis `[user(<compaction_summary>), model(ack)]` di indeks 0..2
    // (yang sudah dibawa ke `<previous_summary>`), bukan pesan user normal mana pun.
    let conversation_slice = &to_summarize[synthetic_prefix_len.min(to_summarize.len())..];

    // Bila tidak ada pesan percakapan baru di luar prefiks summary sintetis dan user tidak memberi
    // instruksi khusus `/compact <instruksi>`, jangan buang request LLM untuk meringkas ulang ringkasan sama.
    if conversation_slice.is_empty() && !has_custom_instructions {
        return None;
    }

    let kept_messages = history.len().saturating_sub(cut_index);
    let (prompt, inline_images) = build_canonical_compaction_prompt(
        conversation_slice,
        inherited_summary.as_deref(),
        custom_instructions,
        max_prompt_chars,
        max_image_tokens,
    );

    Some(CompactionPlan {
        cut_index,
        summarized_messages: to_summarize.len(),
        kept_messages,
        prompt,
        inline_images,
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
            "text": COMPACTION_SUMMARY_ACK_TEXT
        }]
    }));
    rebuilt.extend_from_slice(kept_tail);
    rebuilt
}
