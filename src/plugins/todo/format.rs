//! Sanitasi teks (ANSI/OSC/bidi) dan formatter tampilan `todo` (list, get, widget Android, `/todos`).

use super::state::{TaskItem, TaskStatus, TodoState};
use serde_json::{Map, Value};

pub(super) fn format_ids(ids: &[u32], separator: &str) -> String {
    ids.iter()
        .map(|id| format!("#{}", id))
        .collect::<Vec<_>>()
        .join(separator)
}

fn sanitize_json_value(val: &Value) -> Value {
    match val {
        Value::String(s) => Value::String(sanitize_text(s)),
        Value::Array(arr) => Value::Array(arr.iter().map(sanitize_json_value).collect()),
        Value::Object(obj) => {
            let mut out = Map::with_capacity(obj.len());
            for (k, v) in obj {
                out.insert(sanitize_text(k), sanitize_json_value(v));
            }
            Value::Object(out)
        }
        other => other.clone(),
    }
}

pub(super) fn format_list_line(task: &TaskItem) -> String {
    let form = if task.status == TaskStatus::InProgress {
        task.active_form
            .as_deref()
            .map(|af| format!(" ({})", sanitize_text(af)))
            .unwrap_or_default()
    } else {
        String::new()
    };
    let owner = task
        .owner
        .as_deref()
        .filter(|s| !s.trim().is_empty())
        .map(|o| format!(" @{}", sanitize_text(o)))
        .unwrap_or_default();
    let blocked = if task.blocked_by.is_empty() {
        String::new()
    } else {
        format!(" ⛓ {}", format_ids(&task.blocked_by, ","))
    };
    format!(
        "[{}] #{} {}{}{}{}",
        task.status.as_str(),
        task.id,
        sanitize_text(&task.subject),
        form,
        owner,
        blocked
    )
}

pub(super) fn format_get_lines(task: &TaskItem, blocks: &[u32]) -> String {
    let mut lines = vec![format!(
        "#{} [{}] {}",
        task.id,
        task.status.as_str(),
        sanitize_text(&task.subject)
    )];
    if let Some(desc) = &task.description {
        lines.push(format!("  description: {}", sanitize_text(desc)));
    }
    if let Some(af) = &task.active_form {
        lines.push(format!("  activeForm: {}", sanitize_text(af)));
    }
    if !task.blocked_by.is_empty() {
        lines.push(format!("  blockedBy: {}", format_ids(&task.blocked_by, ", ")));
    }
    if !blocks.is_empty() {
        lines.push(format!("  blocks: {}", format_ids(blocks, ", ")));
    }
    if let Some(owner) = &task.owner {
        lines.push(format!("  owner: {}", sanitize_text(owner)));
    }
    if let Some(meta) = &task.metadata {
        if !meta.is_empty() {
            let safe_meta = sanitize_json_value(&Value::Object(meta.clone()));
            lines.push(format!("  metadata: {}", safe_meta));
        }
    }
    lines.join("\n")
}

/// Format baris widget untuk tab UI Android (`todos`).
pub fn widget_lines_for_state(state: &TodoState) -> Option<Vec<String>> {
    let visible = state.visible_tasks();
    if visible.is_empty() {
        return None;
    }

    let total = visible.len();
    let completed = visible
        .iter()
        .filter(|t| t.status == TaskStatus::Completed)
        .count();
    let active = completed < total;

    let mut lines = Vec::with_capacity(total + 1);
    let header_glyph = if active { "●" } else { "○" };
    lines.push(format!("{} Todos ({}/{})", header_glyph, completed, total));

    for (idx, task) in visible.iter().enumerate() {
        let branch = if idx == total - 1 { "└─" } else { "├─" };
        let glyph = match task.status {
            TaskStatus::Pending => "○",
            TaskStatus::InProgress => "◐",
            TaskStatus::Completed => "✓",
            TaskStatus::Deleted => "✕",
        };

        let mut task_line = format!(
            "{} {} #{} {}",
            branch,
            glyph,
            task.id,
            sanitize_text(&task.subject)
        );

        if task.status == TaskStatus::InProgress {
            if let Some(active_form) = &task.active_form {
                task_line.push_str(&format!(" ({})", sanitize_text(active_form)));
            }
        }
        if let Some(owner) = task.owner.as_deref().filter(|s| !s.trim().is_empty()) {
            task_line.push_str(&format!(" @{}", sanitize_text(owner)));
        }
        if !task.blocked_by.is_empty() {
            let deps = format_ids(&task.blocked_by, ",");
            task_line.push_str(&format!(" ⛓ [{}]", deps));
        }
        lines.push(task_line);
    }

    Some(lines)
}

/// Format teks ringkas terkelompok untuk slash command `/todos`.
pub fn command_text_for_state(state: &TodoState) -> Option<String> {
    let tasks = state.visible_tasks();
    if tasks.is_empty() {
        return None;
    }

    let pending: Vec<&&TaskItem> = tasks
        .iter()
        .filter(|t| t.status == TaskStatus::Pending)
        .collect();
    let in_progress: Vec<&&TaskItem> = tasks
        .iter()
        .filter(|t| t.status == TaskStatus::InProgress)
        .collect();
    let completed: Vec<&&TaskItem> = tasks
        .iter()
        .filter(|t| t.status == TaskStatus::Completed)
        .collect();

    let mut counts = Vec::new();
    if !completed.is_empty() {
        counts.push(format!("{}/{} completed", completed.len(), tasks.len()));
    }
    if !in_progress.is_empty() {
        counts.push(format!("{} in progress", in_progress.len()));
    }
    if !pending.is_empty() {
        counts.push(format!("{} pending", pending.len()));
    }

    let mut lines = vec![counts.join(" · ")];
    let push_section = |lines: &mut Vec<String>, title: &str, group: &[&&TaskItem], glyph: &str| {
        if group.is_empty() {
            return;
        }
        lines.push(title.to_string());
        for &&task in group {
            let form = if task.status == TaskStatus::InProgress {
                task.active_form
                    .as_deref()
                    .map(|af| format!(" ({})", sanitize_text(af)))
                    .unwrap_or_default()
            } else {
                String::new()
            };
            let owner = task
                .owner
                .as_deref()
                .filter(|s| !s.trim().is_empty())
                .map(|o| format!(" @{}", sanitize_text(o)))
                .unwrap_or_default();
            let blocked = if task.blocked_by.is_empty() {
                String::new()
            } else {
                format!("    ⛓ {}", format_ids(&task.blocked_by, ","))
            };
            lines.push(format!(
                "  {} #{} {}{}{}{}",
                glyph,
                task.id,
                sanitize_text(&task.subject),
                form,
                owner,
                blocked
            ));
        }
    };

    push_section(&mut lines, "── Pending ──", &pending, "○");
    push_section(&mut lines, "── In Progress ──", &in_progress, "◐");
    push_section(&mut lines, "── Completed ──", &completed, "✓");

    Some(lines.join("\n"))
}

/// Membersihkan teks tugas dari urutan kontrol terminal (ANSI CSI/OSC/ESC),
/// newline/tab, dan karakter pengarah teks dua-arah (bidi overrides).
pub fn sanitize_text(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    let chars: Vec<char> = value.chars().collect();
    let mut i = 0;

    while i < chars.len() {
        let ch = chars[i];
        // Handle ESC sequences
        if ch == '\u{001b}' {
            if i + 1 < chars.len() {
                match chars[i + 1] {
                    '[' => {
                        // CSI: ESC [ ... [@-~]
                        i += 2;
                        while i < chars.len() {
                            let c = chars[i];
                            i += 1;
                            if ('@'..='~').contains(&c) {
                                break;
                            }
                        }
                        continue;
                    }
                    ']' => {
                        // OSC: ESC ] ... (BEL | ST)
                        i += 2;
                        while i < chars.len() {
                            if chars[i] == '\u{0007}' || chars[i] == '\u{009c}' {
                                i += 1;
                                break;
                            }
                            if chars[i] == '\u{001b}'
                                && i + 1 < chars.len()
                                && chars[i + 1] == '\\'
                            {
                                i += 2;
                                break;
                            }
                            i += 1;
                        }
                        continue;
                    }
                    _ => {
                        i += 2;
                        continue;
                    }
                }
            } else {
                i += 1;
                continue;
            }
        }

        // Single-char 8-bit CSI / OSC
        if ch == '\u{009b}' {
            i += 1;
            while i < chars.len() {
                let c = chars[i];
                i += 1;
                if ('@'..='~').contains(&c) {
                    break;
                }
            }
            continue;
        }
        if ch == '\u{009d}' {
            i += 1;
            while i < chars.len() {
                let c = chars[i];
                i += 1;
                if c == '\u{0007}' || c == '\u{009c}' {
                    break;
                }
            }
            continue;
        }

        match ch {
            '\n' | '\r' | '\t' | '\u{2028}' | '\u{2029}' => out.push(' '),
            '\u{0000}'..='\u{001f}' | '\u{007f}'..='\u{009f}' => {}
            '\u{200e}'
            | '\u{200f}'
            | '\u{202a}'..='\u{202e}'
            | '\u{2066}'..='\u{2069}' => {}
            _ => out.push(ch),
        }
        i += 1;
    }

    out
}
