//! Built-in filesystem tools: `read_file`, `write_file`, `edit_file`, dan `list_dir`.

use super::{expand_path, str_arg, truncate, Tool};
use async_trait::async_trait;
use serde_json::{json, Value};
use std::io::ErrorKind;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

const MAX_READ_BYTES: usize = 100_000;
const MAX_DIR_ENTRIES: usize = 500;
pub(crate) const MAX_EDIT_BYTES: usize = 2 * 1024 * 1024;
const MAX_DIFF_BYTES: usize = 32_768;
const DIFF_CONTEXT_LINES: usize = 2;
const DIFF_LCS_CELL_LIMIT: usize = 25_000;

static EDIT_TMP_SEQ: AtomicU64 = AtomicU64::new(1);

// ---------------------------------------------------------------------------
// read_file
// ---------------------------------------------------------------------------

pub struct ReadFile;

#[async_trait]
impl Tool for ReadFile {
    fn name(&self) -> &'static str {
        "read_file"
    }
    fn description(&self) -> &'static str {
        "Baca isi file teks. Path relatif terhadap direktori kerja; `~` = HOME."
    }
    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": { "path": { "type": "string", "description": "Path file." } },
            "required": ["path"]
        })
    }
    fn preview(&self, args: &Value) -> String {
        args["path"].as_str().unwrap_or("?").to_string()
    }

    async fn execute(&self, args: &Value) -> Result<Value, String> {
        let path = expand_path(str_arg(args, "path")?);
        let bytes = tokio::fs::read(&path)
            .await
            .map_err(|e| format!("gagal membaca {}: {}", path.display(), e))?;
        let text = String::from_utf8_lossy(&bytes);
        Ok(json!({ "path": path.display().to_string(), "content": truncate(&text, MAX_READ_BYTES) }))
    }
}

// ---------------------------------------------------------------------------
// write_file
// ---------------------------------------------------------------------------

pub struct WriteFile;

#[async_trait]
impl Tool for WriteFile {
    fn name(&self) -> &'static str {
        "write_file"
    }
    fn description(&self) -> &'static str {
        "Tulis (buat/timpa) file teks dengan isi lengkap. Direktori induk dibuat otomatis."
    }
    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "path": { "type": "string", "description": "Path file tujuan." },
                "content": { "type": "string", "description": "Isi lengkap file." }
            },
            "required": ["path", "content"]
        })
    }
    fn requires_confirmation(&self) -> bool {
        true
    }
    fn preview(&self, args: &Value) -> String {
        let content = args["content"].as_str().unwrap_or("");
        format!(
            "tulis {} ({} byte, {} baris)",
            args["path"].as_str().unwrap_or("?"),
            content.len(),
            content.lines().count()
        )
    }

    async fn execute(&self, args: &Value) -> Result<Value, String> {
        let path = expand_path(str_arg(args, "path")?);
        let content = str_arg(args, "content")?;
        if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
            tokio::fs::create_dir_all(parent)
                .await
                .map_err(|e| format!("gagal membuat direktori {}: {}", parent.display(), e))?;
        }
        tokio::fs::write(&path, content)
            .await
            .map_err(|e| format!("gagal menulis {}: {}", path.display(), e))?;
        Ok(json!({ "path": path.display().to_string(), "bytes_written": content.len() }))
    }
}

// ---------------------------------------------------------------------------
// edit_file
// ---------------------------------------------------------------------------

pub struct EditFile;

#[derive(Debug, Clone, PartialEq, Eq)]
struct EditOp {
    old_text: String,
    new_text: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ResolvedEdit {
    /// 1-based index of the edit block in user arguments (for error reporting).
    index: usize,
    start: usize,
    end: usize,
    new_text: String,
}

#[derive(Debug, Clone, Copy)]
struct LineSpan<'a> {
    start: usize,
    content_end: usize,
    full_end: usize,
    text: &'a str,
    trimmed_end: &'a str,
}

#[derive(Debug, Clone)]
enum DiffOp<'a> {
    Equal {
        new_no: usize,
        text: &'a str,
    },
    Remove {
        old_no: usize,
        text: &'a str,
    },
    Add {
        new_no: usize,
        text: &'a str,
    },
}

#[derive(Debug, Clone)]
struct DiffReport {
    diff: String,
    lines_added: usize,
    lines_removed: usize,
    first_changed_line: usize,
}

fn normalize_lf(s: &str) -> String {
    if s.contains("\r\n") {
        s.replace("\r\n", "\n")
    } else {
        s.to_string()
    }
}

/// Normalize `\r\n` to `\n` while mapping every byte index in the normalized string
/// (`0..=norm.len()`) back to its exact byte offset in `orig`.
fn normalize_lf_with_map(orig: &str) -> (String, Vec<usize>) {
    let bytes = orig.as_bytes();
    let mut norm = String::with_capacity(bytes.len());
    let mut map = Vec::with_capacity(bytes.len() + 1);
    let mut i = 0usize;
    while i < bytes.len() {
        if bytes[i] == b'\r' && i + 1 < bytes.len() && bytes[i + 1] == b'\n' {
            map.push(i);
            norm.push('\n');
            i += 2;
        } else {
            let ch = orig[i..].chars().next().unwrap();
            let clen = ch.len_utf8();
            for k in 0..clen {
                map.push(i + k);
            }
            norm.push(ch);
            i += clen;
        }
    }
    map.push(bytes.len());
    (norm, map)
}

fn slice_uses_crlf(orig: &str, orig_start: usize, orig_end: usize) -> bool {
    let matched = &orig[orig_start..orig_end];
    if matched.contains("\r\n") {
        return true;
    }
    if matched.contains('\n') {
        return false;
    }
    let bytes = orig.as_bytes();
    if let Some(rel_nl) = bytes[orig_end..].iter().position(|&b| b == b'\n') {
        let nl_idx = orig_end + rel_nl;
        return nl_idx > 0 && bytes[nl_idx - 1] == b'\r';
    }
    if let Some(prev_nl) = bytes[..orig_start].iter().rposition(|&b| b == b'\n') {
        return prev_nl > 0 && bytes[prev_nl - 1] == b'\r';
    }
    false
}

fn count_logical_lines(s: &str) -> usize {
    if s.is_empty() {
        return 0;
    }
    let breaks = s.bytes().filter(|&b| b == b'\n').count();
    if s.ends_with('\n') {
        breaks
    } else {
        breaks + 1
    }
}

fn parse_single_edit_obj(item: &Value, index: usize) -> Result<EditOp, String> {
    let obj = item
        .as_object()
        .ok_or_else(|| format!("blok edit #{} harus berupa objek {{ oldText, newText }}", index))?;
    let old_text = obj
        .get("oldText")
        .and_then(|v| v.as_str())
        .ok_or_else(|| format!("blok edit #{}: field 'oldText' (string) wajib diisi", index))?;
    if old_text.is_empty() {
        return Err(format!("blok edit #{}: 'oldText' tidak boleh kosong", index));
    }
    let new_text = obj
        .get("newText")
        .and_then(|v| v.as_str())
        .ok_or_else(|| format!("blok edit #{}: field 'newText' (string) wajib diisi", index))?;
    if normalize_lf(old_text) == normalize_lf(new_text) {
        return Err(format!(
            "blok edit #{} tidak melakukan perubahan (oldText sama dengan newText)",
            index
        ));
    }
    Ok(EditOp {
        old_text: old_text.to_string(),
        new_text: new_text.to_string(),
    })
}

fn parse_edits(args: &Value) -> Result<Vec<EditOp>, String> {
    let raw_edits = args.get("edits").filter(|v| !v.is_null());
    let has_legacy = args.get("oldText").is_some_and(|v| !v.is_null())
        || args.get("newText").is_some_and(|v| !v.is_null());

    if raw_edits.is_some() && has_legacy {
        return Err(
            "argumen ambigu: jangan kirim 'edits' bersamaan dengan 'oldText'/'newText' di level atas"
                .to_string(),
        );
    }

    let items: Vec<Value> = if let Some(val) = raw_edits {
        match val {
            Value::Array(arr) => arr.clone(),
            Value::Object(_) => vec![val.clone()],
            Value::String(s) => {
                let parsed: Value = serde_json::from_str(s)
                    .map_err(|e| format!("gagal mem-parse string JSON pada 'edits': {}", e))?;
                match parsed {
                    Value::Array(arr) => arr,
                    Value::Object(_) => vec![parsed],
                    _ => {
                        return Err(
                            "string JSON pada 'edits' harus berupa array atau objek { oldText, newText }"
                                .to_string(),
                        );
                    }
                }
            }
            _ => {
                return Err(
                    "argumen 'edits' harus berupa array objek { oldText, newText }".to_string(),
                );
            }
        }
    } else if has_legacy {
        vec![json!({
            "oldText": args.get("oldText").cloned().unwrap_or(Value::Null),
            "newText": args.get("newText").cloned().unwrap_or(Value::Null),
        })]
    } else {
        return Err(
            "argumen 'edits' wajib diisi (minimal 1 blok { oldText, newText })".to_string(),
        );
    };

    if items.is_empty() {
        return Err("daftar 'edits' tidak boleh kosong".to_string());
    }

    let mut ops = Vec::with_capacity(items.len());
    for (idx, item) in items.iter().enumerate() {
        ops.push(parse_single_edit_obj(item, idx + 1)?);
    }
    Ok(ops)
}

fn line_number_at(text: &str, byte_offset: usize) -> usize {
    let clamped = byte_offset.min(text.len());
    text[..clamped].bytes().filter(|&b| b == b'\n').count() + 1
}

fn compute_line_spans(text: &str) -> Vec<LineSpan<'_>> {
    if text.is_empty() {
        return Vec::new();
    }
    let bytes = text.as_bytes();
    let mut spans = Vec::new();
    let mut start = 0usize;
    while start < bytes.len() {
        let rel_nl = bytes[start..].iter().position(|&b| b == b'\n');
        let (content_end, full_end) = match rel_nl {
            Some(off) => (start + off, start + off + 1),
            None => (bytes.len(), bytes.len()),
        };
        let line = &text[start..content_end];
        spans.push(LineSpan {
            start,
            content_end,
            full_end,
            text: line,
            trimmed_end: line.trim_end_matches([' ', '\t']),
        });
        start = full_end;
    }
    spans
}

fn match_trailing_ws_lines(
    spans: &[LineSpan<'_>],
    old_norm: &str,
) -> Vec<(usize, usize)> {
    let had_trailing_nl = old_norm.ends_with('\n');
    let body = old_norm.strip_suffix('\n').unwrap_or(old_norm);
    let old_lines: Vec<&str> = body
        .split('\n')
        .map(|l| l.trim_end_matches([' ', '\t']))
        .collect();

    if old_lines.is_empty() || old_lines.len() > spans.len() {
        return Vec::new();
    }

    let mut found = Vec::new();
    let window = old_lines.len();
    for start_idx in 0..=(spans.len() - window) {
        let mut all_match = true;
        for (k, &expected) in old_lines.iter().enumerate() {
            if spans[start_idx + k].trimmed_end != expected {
                all_match = false;
                break;
            }
        }
        if !all_match {
            continue;
        }
        let last_span = spans[start_idx + window - 1];
        if had_trailing_nl {
            if last_span.full_end == last_span.content_end {
                continue;
            }
            found.push((spans[start_idx].start, last_span.full_end));
        } else {
            found.push((spans[start_idx].start, last_span.content_end));
        }
    }
    found
}

fn resolve_edits(
    working_text: &str,
    ops: &[EditOp],
    display_path: &str,
) -> Result<Vec<ResolvedEdit>, String> {
    let spans = compute_line_spans(working_text);
    let mut resolved = Vec::with_capacity(ops.len());

    for (i, op) in ops.iter().enumerate() {
        let idx = i + 1;
        let old_norm = normalize_lf(&op.old_text);
        let new_norm = normalize_lf(&op.new_text);

        if old_norm == new_norm {
            return Err(format!(
                "blok edit #{} tidak melakukan perubahan (oldText sama dengan newText)",
                idx
            ));
        }

        // Stage 1: Exact match
        let exact_matches: Vec<(usize, usize)> = working_text
            .match_indices(&old_norm)
            .map(|(s, m)| (s, s + m.len()))
            .collect();

        let (start, end) = if exact_matches.len() == 1 {
            exact_matches[0]
        } else if exact_matches.len() > 1 {
            let line_nums: Vec<String> = exact_matches
                .iter()
                .map(|&(s, _)| line_number_at(working_text, s).to_string())
                .collect();
            return Err(format!(
                "blok edit #{} ambigu: ditemukan {} kecocokan pada baris {} di {}. Tambahkan konteks baris di sekitar oldText agar unik.",
                idx,
                exact_matches.len(),
                line_nums.join(", "),
                display_path
            ));
        } else {
            // Stage 2: Safe trailing-whitespace line match (only when Stage 1 yields 0 matches)
            let ws_matches = match_trailing_ws_lines(&spans, &old_norm);
            if ws_matches.len() == 1 {
                ws_matches[0]
            } else if ws_matches.len() > 1 {
                let line_nums: Vec<String> = ws_matches
                    .iter()
                    .map(|&(s, _)| line_number_at(working_text, s).to_string())
                    .collect();
                return Err(format!(
                    "blok edit #{} ambigu: ditemukan {} kecocokan pada baris {} di {}. Tambahkan konteks baris di sekitar oldText agar unik.",
                    idx,
                    ws_matches.len(),
                    line_nums.join(", "),
                    display_path
                ));
            } else {
                return Err(format!(
                    "blok edit #{} tidak ditemukan di {}. Pastikan isi file terbaru sudah dibaca dengan read_file dan oldText cocok persis (termasuk indentasi).",
                    idx, display_path
                ));
            }
        };

        resolved.push(ResolvedEdit {
            index: idx,
            start,
            end,
            new_text: new_norm,
        });
    }

    resolved.sort_by_key(|e| (e.start, e.end));

    for pair in resolved.windows(2) {
        let prev = &pair[0];
        let curr = &pair[1];
        if prev.end > curr.start {
            let start_line = line_number_at(working_text, prev.start.min(curr.start));
            let end_byte = prev.end.max(curr.end).saturating_sub(1).max(prev.start);
            let end_line = line_number_at(working_text, end_byte);
            return Err(format!(
                "blok edit #{} dan #{} saling tumpang tindih pada baris {}–{}",
                prev.index, curr.index, start_line, end_line
            ));
        }
    }

    Ok(resolved)
}

fn apply_edits(
    orig_body: &str,
    norm_to_orig: &[usize],
    sorted_edits: &[ResolvedEdit],
) -> Result<String, String> {
    let mut updated = orig_body.to_string();
    for edit in sorted_edits.iter().rev() {
        let orig_start = norm_to_orig[edit.start];
        let orig_end = norm_to_orig[edit.end];
        let replacement = if slice_uses_crlf(orig_body, orig_start, orig_end) {
            edit.new_text.replace('\n', "\r\n")
        } else {
            edit.new_text.clone()
        };
        updated.replace_range(orig_start..orig_end, &replacement);
    }
    if updated == orig_body {
        return Err("edit tidak menghasilkan perubahan pada file".to_string());
    }
    Ok(updated)
}

fn split_diff_lines(s: &str) -> Vec<&str> {
    if s.is_empty() {
        return Vec::new();
    }
    let body = s.strip_suffix('\n').unwrap_or(s);
    body.split('\n').collect()
}

fn diff_line_slices<'a>(
    old_lines: &[&'a str],
    new_lines: &[&'a str],
    old_base: usize,
    new_base: usize,
) -> Vec<DiffOp<'a>> {
    let mut prefix = 0usize;
    let min_len = old_lines.len().min(new_lines.len());
    while prefix < min_len && old_lines[prefix] == new_lines[prefix] {
        prefix += 1;
    }

    let mut suffix = 0usize;
    while suffix < (old_lines.len() - prefix)
        && suffix < (new_lines.len() - prefix)
        && old_lines[old_lines.len() - 1 - suffix] == new_lines[new_lines.len() - 1 - suffix]
    {
        suffix += 1;
    }

    let mut ops = Vec::new();

    for (i, &line) in old_lines.iter().enumerate().take(prefix) {
        ops.push(DiffOp::Equal {
            new_no: new_base + i,
            text: line,
        });
    }

    let old_core = &old_lines[prefix..old_lines.len() - suffix];
    let new_core = &new_lines[prefix..new_lines.len() - suffix];

    if !old_core.is_empty()
        && !new_core.is_empty()
        && old_core.len().saturating_mul(new_core.len()) <= DIFF_LCS_CELL_LIMIT
    {
        let m = old_core.len();
        let n = new_core.len();
        let mut dp = vec![0u32; (m + 1) * (n + 1)];
        let stride = n + 1;
        for i in (0..m).rev() {
            for j in (0..n).rev() {
                dp[i * stride + j] = if old_core[i] == new_core[j] {
                    dp[(i + 1) * stride + (j + 1)] + 1
                } else {
                    dp[(i + 1) * stride + j].max(dp[i * stride + (j + 1)])
                };
            }
        }
        let mut i = 0usize;
        let mut j = 0usize;
        while i < m && j < n {
            if old_core[i] == new_core[j] {
                ops.push(DiffOp::Equal {
                    new_no: new_base + prefix + j,
                    text: old_core[i],
                });
                i += 1;
                j += 1;
            } else if dp[(i + 1) * stride + j] >= dp[i * stride + (j + 1)] {
                ops.push(DiffOp::Remove {
                    old_no: old_base + prefix + i,
                    text: old_core[i],
                });
                i += 1;
            } else {
                ops.push(DiffOp::Add {
                    new_no: new_base + prefix + j,
                    text: new_core[j],
                });
                j += 1;
            }
        }
        while i < m {
            ops.push(DiffOp::Remove {
                old_no: old_base + prefix + i,
                text: old_core[i],
            });
            i += 1;
        }
        while j < n {
            ops.push(DiffOp::Add {
                new_no: new_base + prefix + j,
                text: new_core[j],
            });
            j += 1;
        }
    } else {
        for (i, &line) in old_core.iter().enumerate() {
            ops.push(DiffOp::Remove {
                old_no: old_base + prefix + i,
                text: line,
            });
        }
        for (j, &line) in new_core.iter().enumerate() {
            ops.push(DiffOp::Add {
                new_no: new_base + prefix + j,
                text: line,
            });
        }
    }

    let old_suf_start = old_lines.len() - suffix;
    let new_suf_start = new_lines.len() - suffix;
    for (k, &line) in old_lines[old_suf_start..].iter().enumerate() {
        ops.push(DiffOp::Equal {
            new_no: new_base + new_suf_start + k,
            text: line,
        });
    }

    ops
}

#[derive(Debug, Clone)]
enum RenderLine {
    Context(usize, String),
    Remove(usize, String),
    Add(usize, String),
    Elision,
}

fn build_numbered_diff(working_text: &str, sorted_edits: &[ResolvedEdit]) -> DiffReport {
    let spans = compute_line_spans(working_text);
    if spans.is_empty() || sorted_edits.is_empty() {
        return DiffReport {
            diff: String::new(),
            lines_added: 0,
            lines_removed: 0,
            first_changed_line: 1,
        };
    }

    // 1. Map each edit to inclusive line indices [first_idx..=last_idx], extending
    //    into the next line if the edit deletes a trailing newline and joins lines.
    struct Region<'a> {
        first_idx: usize,
        last_idx: usize,
        edits: Vec<&'a ResolvedEdit>,
    }

    let mut raw_regions: Vec<Region<'_>> = Vec::with_capacity(sorted_edits.len());
    for edit in sorted_edits {
        let first_idx = spans
            .partition_point(|s| s.start <= edit.start)
            .saturating_sub(1);
        let last_byte = edit.end.saturating_sub(1).max(edit.start);
        let mut last_idx = spans
            .partition_point(|s| s.start <= last_byte)
            .saturating_sub(1);

        // If the edit consumes the line break of `last_idx` and leaves non-empty text
        // without a trailing `\n`, it merges into `last_idx + 1`.
        if last_idx + 1 < spans.len()
            && edit.end == spans[last_idx].full_end
            && spans[last_idx].full_end > spans[last_idx].content_end
        {
            let has_prefix_on_first = edit.start > spans[first_idx].start;
            let leaves_unterminated =
                (!edit.new_text.is_empty() && !edit.new_text.ends_with('\n'))
                    || (edit.new_text.is_empty() && has_prefix_on_first);
            if leaves_unterminated {
                last_idx += 1;
            }
        }

        if let Some(prev) = raw_regions.last_mut() {
            if prev.last_idx >= first_idx {
                prev.last_idx = prev.last_idx.max(last_idx);
                prev.edits.push(edit);
                continue;
            }
        }
        raw_regions.push(Region {
            first_idx,
            last_idx,
            edits: vec![edit],
        });
    }

    // 2. Build diff ops for each merged region + bounded untouched gap context.
    enum StreamItem {
        Context { line_no: usize, text: String },
        Remove { line_no: usize, text: String },
        Add { line_no: usize, text: String },
        GapBreak,
    }

    let mut stream: Vec<StreamItem> = Vec::new();
    let mut line_delta: isize = 0;
    let mut lines_added: usize = 0;
    let mut lines_removed: usize = 0;
    let mut first_changed_line: Option<usize> = None;

    let mut prev_region_end_idx: Option<usize> = None;

    for region in &raw_regions {
        // Emit bounded context from untouched file lines preceding this region.
        match prev_region_end_idx {
            None => {
                let ctx_start = region.first_idx.saturating_sub(DIFF_CONTEXT_LINES);
                for (idx, span) in spans[ctx_start..region.first_idx].iter().enumerate() {
                    let line_no = ctx_start + idx + 1;
                    stream.push(StreamItem::Context {
                        line_no,
                        text: span.text.to_string(),
                    });
                }
            }
            Some(prev_end) => {
                let gap_start = prev_end + 1;
                let gap_end = region.first_idx;
                let gap_len = gap_end.saturating_sub(gap_start);
                if gap_len <= 2 * DIFF_CONTEXT_LINES {
                    for (offset, span) in spans[gap_start..gap_end].iter().enumerate() {
                        let line_no = ((gap_start + offset) as isize + 1 + line_delta) as usize;
                        stream.push(StreamItem::Context {
                            line_no,
                            text: span.text.to_string(),
                        });
                    }
                } else {
                    for (offset, span) in spans[gap_start..gap_start + DIFF_CONTEXT_LINES]
                        .iter()
                        .enumerate()
                    {
                        let line_no = ((gap_start + offset) as isize + 1 + line_delta) as usize;
                        stream.push(StreamItem::Context {
                            line_no,
                            text: span.text.to_string(),
                        });
                    }
                    stream.push(StreamItem::GapBreak);
                    let lead_start = gap_end - DIFF_CONTEXT_LINES;
                    for (offset, span) in spans[lead_start..gap_end].iter().enumerate() {
                        let line_no = ((lead_start + offset) as isize + 1 + line_delta) as usize;
                        stream.push(StreamItem::Context {
                            line_no,
                            text: span.text.to_string(),
                        });
                    }
                }
            }
        }

        let r_start = spans[region.first_idx].start;
        let r_end = spans[region.last_idx].full_end;
        let old_region = &working_text[r_start..r_end];
        let mut new_region = old_region.to_string();
        for edit in region.edits.iter().rev() {
            let rel_s = edit.start - r_start;
            let rel_e = edit.end - r_start;
            new_region.replace_range(rel_s..rel_e, &edit.new_text);
        }

        let old_lines = split_diff_lines(old_region);
        let new_lines = split_diff_lines(&new_region);
        let old_base = region.first_idx + 1;
        let new_base = (region.first_idx as isize + 1 + line_delta) as usize;

        let ops = diff_line_slices(&old_lines, &new_lines, old_base, new_base);
        for op in ops {
            match op {
                DiffOp::Equal { new_no, text, .. } => {
                    stream.push(StreamItem::Context {
                        line_no: new_no,
                        text: text.to_string(),
                    });
                }
                DiffOp::Remove { old_no, text } => {
                    lines_removed += 1;
                    if first_changed_line.is_none() {
                        first_changed_line = Some(old_no);
                    }
                    stream.push(StreamItem::Remove {
                        line_no: old_no,
                        text: text.to_string(),
                    });
                }
                DiffOp::Add { new_no, text } => {
                    lines_added += 1;
                    if first_changed_line.is_none() {
                        first_changed_line = Some(new_no);
                    }
                    stream.push(StreamItem::Add {
                        line_no: new_no,
                        text: text.to_string(),
                    });
                }
            }
        }

        line_delta += new_lines.len() as isize - old_lines.len() as isize;
        prev_region_end_idx = Some(region.last_idx);
    }

    // Trailing context after the last region.
    if let Some(last_end) = prev_region_end_idx {
        let trail_start = last_end + 1;
        let trail_end = (trail_start + DIFF_CONTEXT_LINES).min(spans.len());
        for (offset, span) in spans[trail_start..trail_end].iter().enumerate() {
            let line_no = ((trail_start + offset) as isize + 1 + line_delta) as usize;
            stream.push(StreamItem::Context {
                line_no,
                text: span.text.to_string(),
            });
        }
    }

    // 3. Slice `stream` into hunks keeping at most `DIFF_CONTEXT_LINES` around every change,
    //    inserting `Elision` between separated hunks.
    let change_indices: Vec<usize> = stream
        .iter()
        .enumerate()
        .filter_map(|(idx, item)| match item {
            StreamItem::Remove { .. } | StreamItem::Add { .. } => Some(idx),
            _ => None,
        })
        .collect();

    let mut render_lines: Vec<RenderLine> = Vec::new();
    if !change_indices.is_empty() {
        let mut hunks: Vec<(usize, usize)> = Vec::new();
        for &ci in &change_indices {
            // Walk back up to DIFF_CONTEXT_LINES stopping before any GapBreak.
            let mut h_start = ci;
            let mut back = 0usize;
            while h_start > 0 && back < DIFF_CONTEXT_LINES {
                if matches!(stream[h_start - 1], StreamItem::GapBreak) {
                    break;
                }
                h_start -= 1;
                back += 1;
            }
            // Walk forward up to DIFF_CONTEXT_LINES stopping before any GapBreak.
            let mut h_end = ci;
            let mut fwd = 0usize;
            while h_end + 1 < stream.len() && fwd < DIFF_CONTEXT_LINES {
                if matches!(stream[h_end + 1], StreamItem::GapBreak) {
                    break;
                }
                h_end += 1;
                fwd += 1;
            }

            if let Some(prev) = hunks.last_mut() {
                let no_gap_between = (prev.1 + 1..h_start)
                    .all(|idx| !matches!(stream[idx], StreamItem::GapBreak));
                if h_start <= prev.1 + 1 && no_gap_between {
                    prev.1 = prev.1.max(h_end);
                    continue;
                }
            }
            hunks.push((h_start, h_end));
        }

        for (h_idx, &(start, end)) in hunks.iter().enumerate() {
            if h_idx > 0 {
                render_lines.push(RenderLine::Elision);
            }
            for item in &stream[start..=end] {
                match item {
                    StreamItem::Context { line_no, text } => {
                        render_lines.push(RenderLine::Context(*line_no, text.clone()));
                    }
                    StreamItem::Remove { line_no, text } => {
                        render_lines.push(RenderLine::Remove(*line_no, text.clone()));
                    }
                    StreamItem::Add { line_no, text } => {
                        render_lines.push(RenderLine::Add(*line_no, text.clone()));
                    }
                    StreamItem::GapBreak => {}
                }
            }
        }
    }

    // 4. Format Pi-style lines (`" {no} {text}"`, `"-{no} {text}"`, `"+{no} {text}"`, `"     ..."`)
    //    with safe truncation at MAX_DIFF_BYTES.
    let max_line_no = render_lines
        .iter()
        .filter_map(|l| match l {
            RenderLine::Context(n, _) | RenderLine::Remove(n, _) | RenderLine::Add(n, _) => {
                Some(*n)
            }
            RenderLine::Elision => None,
        })
        .max()
        .unwrap_or(1);
    let width = max_line_no.to_string().len().max(2);

    const TRUNC_FOOTER: &str = "     ... (diff dipotong)";
    let mut diff = String::new();
    for line in &render_lines {
        let formatted = match line {
            RenderLine::Context(n, text) => format!(" {:>width$} {}", n, text, width = width),
            RenderLine::Remove(n, text) => format!("-{:>width$} {}", n, text, width = width),
            RenderLine::Add(n, text) => format!("+{:>width$} {}", n, text, width = width),
            RenderLine::Elision => "     ...".to_string(),
        };

        let sep_len = usize::from(!diff.is_empty());
        if diff.len() + sep_len + formatted.len() + 1 + TRUNC_FOOTER.len() > MAX_DIFF_BYTES {
            if !diff.is_empty() {
                diff.push('\n');
            }
            diff.push_str(TRUNC_FOOTER);
            break;
        }
        if !diff.is_empty() {
            diff.push('\n');
        }
        diff.push_str(&formatted);
    }

    DiffReport {
        diff,
        lines_added,
        lines_removed,
        first_changed_line: first_changed_line.unwrap_or(1),
    }
}

async fn atomic_write_preserving_meta(
    target_path: &Path,
    content: &[u8],
    orig_perms: std::fs::Permissions,
) -> Result<(), String> {
    let parent = target_path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    let seq = EDIT_TMP_SEQ.fetch_add(1, Ordering::Relaxed);
    let tmp_name = format!(
        ".oopbot-edit-{}-{}-{}.tmp",
        std::process::id(),
        nanos,
        seq
    );
    let tmp_path: PathBuf = parent.join(tmp_name);

    let write_res: Result<(), String> = async {
        let mut open_opts = tokio::fs::OpenOptions::new();
        open_opts.write(true).create_new(true);
        #[cfg(unix)]
        open_opts.mode(0o600);

        let mut file = open_opts
            .open(&tmp_path)
            .await
            .map_err(|e| format!("gagal membuat file sementara {}: {}", tmp_path.display(), e))?;
        file.write_all(content)
            .await
            .map_err(|e| format!("gagal menulis file sementara {}: {}", tmp_path.display(), e))?;
        file.flush()
            .await
            .map_err(|e| format!("gagal flush {}: {}", tmp_path.display(), e))?;
        file.sync_all()
            .await
            .map_err(|e| format!("gagal sync {}: {}", tmp_path.display(), e))?;
        tokio::fs::set_permissions(&tmp_path, orig_perms)
            .await
            .map_err(|e| format!("gagal menyalin permission ke {}: {}", tmp_path.display(), e))?;
        drop(file);
        tokio::fs::rename(&tmp_path, target_path)
            .await
            .map_err(|e| format!("gagal mengganti {}: {}", target_path.display(), e))?;
        Ok(())
    }
    .await;

    if write_res.is_err() {
        let _ = tokio::fs::remove_file(&tmp_path).await;
    }
    write_res
}

#[async_trait]
impl Tool for EditFile {
    fn name(&self) -> &'static str {
        "edit_file"
    }

    fn description(&self) -> &'static str {
        "Edit sebagian isi file teks yang sudah ada dengan mengganti blok teks tertentu secara presisi (atomik). Gunakan `edits` berisi daftar `{ oldText, newText }`. Setiap `oldText` harus cocok persis dan unik di dalam file; sertakan 1–3 baris konteks jika perlu. Untuk membuat file baru atau menulis ulang penuh, gunakan `write_file`."
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "path": {
                    "type": "string",
                    "description": "Path file teks yang sudah ada. Relatif terhadap direktori kerja; `~` = HOME."
                },
                "edits": {
                    "type": "array",
                    "description": "Daftar perubahan yang diterapkan secara atomik. Tiap `oldText` harus cocok persis dan unik di file.",
                    "minItems": 1,
                    "items": {
                        "type": "object",
                        "properties": {
                            "oldText": {
                                "type": "string",
                                "description": "Teks lama yang akan diganti (harus persis dan unik dalam file, tidak boleh kosong)."
                            },
                            "newText": {
                                "type": "string",
                                "description": "Teks pengganti (boleh kosong untuk menghapus `oldText`)."
                            }
                        },
                        "required": ["oldText", "newText"]
                    }
                }
            },
            "required": ["path", "edits"]
        })
    }

    fn requires_confirmation(&self) -> bool {
        true
    }

    fn preview(&self, args: &Value) -> String {
        let path = args["path"].as_str().unwrap_or("?");
        match parse_edits(args) {
            Ok(ops) => {
                let removed: usize = ops.iter().map(|o| count_logical_lines(&o.old_text)).sum();
                let added: usize = ops.iter().map(|o| count_logical_lines(&o.new_text)).sum();
                format!(
                    "edit {} ({} blok, -{}/+{} baris)",
                    path,
                    ops.len(),
                    removed,
                    added
                )
            }
            Err(_) => format!("edit {}", path),
        }
    }

    async fn execute(&self, args: &Value) -> Result<Value, String> {
        let raw_path = str_arg(args, "path")?;
        if raw_path.trim().is_empty() {
            return Err("argumen 'path' tidak boleh kosong".to_string());
        }
        let ops = parse_edits(args)?;
        let path = expand_path(raw_path);
        let display_path = path.display().to_string();

        let sym_meta = tokio::fs::symlink_metadata(&path).await.map_err(|e| {
            if e.kind() == ErrorKind::NotFound {
                format!(
                    "file tidak ditemukan: {} (gunakan write_file untuk membuat file baru)",
                    display_path
                )
            } else {
                format!("gagal mengakses {}: {}", display_path, e)
            }
        })?;

        let target_path = if sym_meta.file_type().is_symlink() {
            tokio::fs::canonicalize(&path).await.map_err(|e| {
                if e.kind() == ErrorKind::NotFound {
                    format!(
                        "file tidak ditemukan: {} (gunakan write_file untuk membuat file baru)",
                        display_path
                    )
                } else {
                    format!("gagal menyelesaikan symlink {}: {}", display_path, e)
                }
            })?
        } else {
            path.clone()
        };

        let meta = tokio::fs::metadata(&target_path).await.map_err(|e| {
            if e.kind() == ErrorKind::NotFound {
                format!(
                    "file tidak ditemukan: {} (gunakan write_file untuk membuat file baru)",
                    display_path
                )
            } else {
                format!("gagal membaca metadata {}: {}", display_path, e)
            }
        })?;

        if meta.is_dir() {
            return Err(format!("{} adalah direktori, bukan file", display_path));
        }
        if !meta.is_file() {
            return Err(format!(
                "{} bukan file reguler, tidak bisa diedit dengan edit_file",
                display_path
            ));
        }

        if meta.len() > MAX_EDIT_BYTES as u64 {
            return Err(format!(
                "file {} terlalu besar ({} byte, batas maksimal {} byte)",
                display_path,
                meta.len(),
                MAX_EDIT_BYTES
            ));
        }

        let orig_perms = meta.permissions();
        let file = tokio::fs::File::open(&target_path)
            .await
            .map_err(|e| format!("gagal membaca {}: {}", display_path, e))?;
        let mut bytes = Vec::with_capacity((meta.len() as usize).min(MAX_EDIT_BYTES));
        file.take((MAX_EDIT_BYTES + 1) as u64)
            .read_to_end(&mut bytes)
            .await
            .map_err(|e| format!("gagal membaca {}: {}", display_path, e))?;

        if bytes.len() > MAX_EDIT_BYTES {
            return Err(format!(
                "file {} terlalu besar ({} byte, batas maksimal {} byte)",
                display_path,
                bytes.len(),
                MAX_EDIT_BYTES
            ));
        }

        if bytes.contains(&0) {
            return Err("file terdeteksi biner, tidak bisa diedit dengan edit_file".to_string());
        }

        let content = String::from_utf8(bytes).map_err(|_| {
            format!(
                "file {} bukan UTF-8 valid, tidak bisa diedit dengan edit_file",
                display_path
            )
        })?;

        let (has_bom, body) = match content.strip_prefix('\u{FEFF}') {
            Some(rest) => (true, rest),
            None => (false, content.as_str()),
        };
        let (working_text, norm_to_orig) = normalize_lf_with_map(body);

        let sorted_edits = resolve_edits(&working_text, &ops, &display_path)?;
        let updated_body = apply_edits(body, &norm_to_orig, &sorted_edits)?;
        let report = build_numbered_diff(&working_text, &sorted_edits);

        let mut final_text = String::with_capacity(updated_body.len() + if has_bom { 3 } else { 0 });
        if has_bom {
            final_text.push('\u{FEFF}');
        }
        final_text.push_str(&updated_body);

        let bytes_written = final_text.len();
        atomic_write_preserving_meta(&target_path, final_text.as_bytes(), orig_perms).await?;

        Ok(json!({
            "path": display_path,
            "edits_applied": sorted_edits.len(),
            "lines_added": report.lines_added,
            "lines_removed": report.lines_removed,
            "first_changed_line": report.first_changed_line,
            "bytes_written": bytes_written,
            "details": {
                "diff": report.diff,
                "firstChangedLine": report.first_changed_line,
                "linesAdded": report.lines_added,
                "linesRemoved": report.lines_removed
            }
        }))
    }
}

// ---------------------------------------------------------------------------
// list_dir
// ---------------------------------------------------------------------------

pub struct ListDir;

#[async_trait]
impl Tool for ListDir {
    fn name(&self) -> &'static str {
        "list_dir"
    }
    fn description(&self) -> &'static str {
        "Daftar isi direktori (nama, tipe, ukuran). Default direktori kerja saat ini."
    }
    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": { "path": { "type": "string", "description": "Path direktori (default \".\")." } }
        })
    }
    fn preview(&self, args: &Value) -> String {
        args["path"].as_str().unwrap_or(".").to_string()
    }

    async fn execute(&self, args: &Value) -> Result<Value, String> {
        let path = expand_path(args["path"].as_str().unwrap_or("."));
        let mut rd = tokio::fs::read_dir(&path)
            .await
            .map_err(|e| format!("gagal membuka {}: {}", path.display(), e))?;

        let mut entries = Vec::new();
        let mut truncated = false;
        while let Some(entry) = rd.next_entry().await.map_err(|e| e.to_string())? {
            if entries.len() >= MAX_DIR_ENTRIES {
                truncated = true;
                break;
            }
            let meta = entry.metadata().await.ok();
            let kind = match &meta {
                Some(m) if m.is_dir() => "dir",
                Some(m) if m.is_symlink() => "symlink",
                Some(_) => "file",
                None => "unknown",
            };
            entries.push(json!({
                "name": entry.file_name().to_string_lossy(),
                "type": kind,
                "size": meta.map(|m| m.len()),
            }));
        }
        entries.sort_by(|a, b| a["name"].as_str().cmp(&b["name"].as_str()));
        Ok(json!({ "path": path.display().to_string(), "entries": entries, "truncated": truncated }))
    }
}
