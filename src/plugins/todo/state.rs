//! Model data, mesin status 4-kondisi, validasi graf dependensi (`blockedBy`), dan replay branch sesi.

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use std::collections::{HashMap, HashSet};

/// Key widget yang dikirim lewat `extension_ui_request` (`setWidget`) ke OOPBoT UI.
pub const TODO_WIDGET_KEY: &str = "todos";

/// `customType` entri JSONL sesi untuk snapshot state `TodoPlugin`.
pub const TODO_STATE_ENTRY: &str = "oopbot-todo-state";

/// Pesan default ketika belum ada todo aktif pada branch saat ini.
pub const NO_TODOS_MESSAGE: &str = "No todos yet. Ask the agent to add some!";

/// Batas kapasitas dan panjang input agar state todo, widget UI, dan JSONL sesi tetap aman.
pub(super) const MAX_ACTIVE_TASKS: usize = 200;
pub(super) const MAX_TOTAL_TASKS: usize = 500;
pub(super) const MAX_BATCH_OPS: usize = 50;
pub(super) const MAX_SUBJECT_CHARS: usize = 300;
pub(super) const MAX_DESCRIPTION_CHARS: usize = 8_000;
pub(super) const MAX_LABEL_CHARS: usize = 200;
pub(super) const MAX_METADATA_KEYS: usize = 64;

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub enum TaskStatus {
    #[serde(rename = "pending")]
    Pending,
    #[serde(rename = "in_progress")]
    InProgress,
    #[serde(rename = "completed")]
    Completed,
    #[serde(rename = "deleted")]
    Deleted,
}

impl TaskStatus {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::InProgress => "in_progress",
            Self::Completed => "completed",
            Self::Deleted => "deleted",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "pending" => Some(Self::Pending),
            "in_progress" => Some(Self::InProgress),
            "completed" => Some(Self::Completed),
            "deleted" => Some(Self::Deleted),
            _ => None,
        }
    }

    /// Aturan transisi status mengikuti state machine `oopbot-todo`:
    /// - status yang sama selalu diizinkan (no-op bila field lain tidak berubah)
    /// - `pending` -> `in_progress | completed | deleted`
    /// - `in_progress` -> `pending | completed | deleted`
    /// - `completed` -> `deleted`
    /// - `deleted` -> (tidak bisa keluar dari tombstone)
    pub fn can_transition_to(self, target: Self) -> bool {
        if self == target {
            return true;
        }
        matches!(
            (self, target),
            (Self::Pending, Self::InProgress | Self::Completed | Self::Deleted)
                | (Self::InProgress, Self::Pending | Self::Completed | Self::Deleted)
                | (Self::Completed, Self::Deleted)
        )
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct TaskItem {
    pub id: u32,
    pub subject: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(rename = "activeForm", skip_serializing_if = "Option::is_none")]
    pub active_form: Option<String>,
    pub status: TaskStatus,
    #[serde(rename = "blockedBy", default, skip_serializing_if = "Vec::is_empty")]
    pub blocked_by: Vec<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub owner: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub metadata: Option<Map<String, Value>>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct TodoState {
    pub tasks: Vec<TaskItem>,
    #[serde(rename = "nextId", alias = "next_id")]
    pub next_id: u32,
}

impl Default for TodoState {
    fn default() -> Self {
        Self::new()
    }
}

impl TodoState {
    pub fn new() -> Self {
        Self {
            tasks: Vec::new(),
            next_id: 1,
        }
    }

    pub fn visible_tasks(&self) -> Vec<&TaskItem> {
        self.tasks
            .iter()
            .filter(|t| t.status != TaskStatus::Deleted)
            .collect()
    }
}

/// Snapshot `details` yang disimpan pada `toolResult` dan entri sesi `oopbot-todo-state`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct TodoDetails {
    pub action: String,
    pub params: Value,
    pub tasks: Vec<TaskItem>,
    #[serde(rename = "nextId")]
    pub next_id: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

pub(super) fn ensure_create_capacity(state: &TodoState) -> Result<(), String> {
    let active_count = state
        .tasks
        .iter()
        .filter(|t| t.status != TaskStatus::Deleted)
        .count();
    if active_count >= MAX_ACTIVE_TASKS {
        return Err(format!(
            "jumlah tugas aktif mencapai batas maksimal ({}); selesaikan, hapus, atau clear tugas lama terlebih dahulu",
            MAX_ACTIVE_TASKS
        ));
    }
    if state.tasks.len() >= MAX_TOTAL_TASKS {
        return Err(format!(
            "total riwayat tugas sesi mencapai batas maksimal ({}); jalankan clear untuk membersihkan",
            MAX_TOTAL_TASKS
        ));
    }
    Ok(())
}

pub(super) fn dependency_problem(tasks: &[TaskItem], dep_id: u32) -> Option<String> {
    let Some(task) = tasks.iter().find(|t| t.id == dep_id) else {
        return Some(format!("#{} tidak ditemukan", dep_id));
    };
    if task.status == TaskStatus::Deleted {
        return Some(format!("#{} sudah dihapus (deleted)", dep_id));
    }
    None
}

pub(super) fn unresolved_blockers(tasks: &[TaskItem], blocked_by: &[u32]) -> Vec<String> {
    let mut unresolved = Vec::new();
    for &dep_id in blocked_by {
        if let Some(dep) = tasks.iter().find(|t| t.id == dep_id) {
            if matches!(dep.status, TaskStatus::Pending | TaskStatus::InProgress) {
                unresolved.push(format!("#{} ({})", dep.id, dep.status.as_str()));
            }
        }
    }
    unresolved
}

/// Memeriksa apakah memberi dependensi `blocked_by` pada tugas `id` membentuk siklus graf.
pub fn closes_cycle(tasks: &[TaskItem], id: u32, blocked_by: &[u32]) -> bool {
    let mut edges: HashMap<u32, &[u32]> = HashMap::with_capacity(tasks.len());
    for task in tasks {
        if task.id == id {
            edges.insert(task.id, blocked_by);
        } else {
            edges.insert(task.id, &task.blocked_by);
        }
    }
    let mut seen = HashSet::new();
    let mut pending: Vec<u32> = blocked_by.to_vec();
    while let Some(next) = pending.pop() {
        if next == id {
            return true;
        }
        if !seen.insert(next) {
            continue;
        }
        if let Some(deps) = edges.get(&next) {
            pending.extend_from_slice(deps);
        }
    }
    false
}

pub(super) fn blocks_of(tasks: &[TaskItem]) -> HashMap<u32, Vec<u32>> {
    let mut map: HashMap<u32, Vec<u32>> = HashMap::new();
    for task in tasks {
        if task.status == TaskStatus::Deleted {
            continue;
        }
        for &dep in &task.blocked_by {
            map.entry(dep).or_default().push(task.id);
        }
    }
    map
}

pub(super) fn merge_metadata(
    existing: Option<&Map<String, Value>>,
    changes: &Map<String, Value>,
) -> Result<Option<Map<String, Value>>, String> {
    let mut merged = existing.cloned().unwrap_or_default();
    for (key, value) in changes {
        if value.is_null() {
            merged.remove(key);
        } else {
            merged.insert(key.clone(), value.clone());
        }
    }
    if merged.len() > MAX_METADATA_KEYS {
        return Err(format!(
            "jumlah key metadata melebihi batas maksimal ({})",
            MAX_METADATA_KEYS
        ));
    }
    if merged.is_empty() {
        Ok(None)
    } else {
        Ok(Some(merged))
    }
}

pub(super) fn make_details(action: &str, params: &Value, state: &TodoState) -> TodoDetails {
    TodoDetails {
        action: action.to_string(),
        params: params.clone(),
        tasks: state.tasks.clone(),
        next_id: state.next_id,
        error: None,
    }
}

fn try_parse_todo_state(val: &Value) -> Option<TodoState> {
    let obj = val.as_object()?;
    let tasks_val = obj.get("tasks")?.as_array()?;
    let next_id_val = obj.get("nextId").or_else(|| obj.get("next_id"))?.as_u64()?;

    let mut tasks = Vec::with_capacity(tasks_val.len());
    let mut seen_ids = HashSet::new();
    for item in tasks_val {
        let Ok(task) = serde_json::from_value::<TaskItem>(item.clone()) else {
            continue;
        };
        if task.id == 0 {
            continue;
        }
        if !seen_ids.insert(task.id) {
            tasks.retain(|t: &TaskItem| t.id != task.id);
        }
        tasks.push(task);
    }
    if !tasks_val.is_empty() && tasks.is_empty() {
        return None;
    }

    let active_ids: HashSet<u32> = tasks
        .iter()
        .filter(|t| t.status != TaskStatus::Deleted)
        .map(|t| t.id)
        .collect();
    for task in &mut tasks {
        let own_id = task.id;
        let mut dedup = Vec::with_capacity(task.blocked_by.len());
        for &dep in &task.blocked_by {
            if dep != own_id && active_ids.contains(&dep) && !dedup.contains(&dep) {
                dedup.push(dep);
            }
        }
        task.blocked_by = dedup;
    }

    let max_id = tasks.iter().map(|t| t.id).max().unwrap_or(0);
    let parsed_next = u32::try_from(next_id_val).unwrap_or(u32::MAX);
    let next_id = parsed_next.max(max_id.saturating_add(1)).max(1);

    Some(TodoState { tasks, next_id })
}

/// Replay `TodoState` dari rangkaian entri branch sesi aktif (`active_branch_entries`).
///
/// Sesuai perilaku `oopbot-todo`:
/// - Entri custom `customType: "oopbot-todo-state"` menjadi sumber utama terbaru.
/// - Jika belum ada, fallback ke `details` pada `toolResult` terakhir milik tool `"todo"`.
pub fn replay_branch_entries(entries: &[Value]) -> TodoState {
    let mut from_entries: Option<TodoState> = None;
    let mut from_results: Option<TodoState> = None;

    for entry in entries {
        match entry["type"].as_str().unwrap_or_default() {
            "custom" => {
                if entry["customType"].as_str() == Some(TODO_STATE_ENTRY) {
                    if let Some(parsed) = try_parse_todo_state(&entry["data"]) {
                        from_entries = Some(parsed);
                    }
                }
            }
            "message" => {
                let msg = &entry["message"];
                if msg["role"].as_str() == Some("toolResult")
                    && msg["toolName"].as_str() == Some("todo")
                {
                    if let Some(parsed) = try_parse_todo_state(&msg["details"]) {
                        from_results = Some(parsed);
                    }
                }
            }
            _ => {}
        }
    }

    from_entries.or(from_results).unwrap_or_default()
}
