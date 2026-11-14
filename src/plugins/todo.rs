//! TodoPlugin — Manajemen checklist tugas & progress multi-langkah.
//!
//! Kompatibel dengan ekstensi Pi/Pipkin `pipkin-todo` (`todo.ts` & `index.ts`):
//! - Tool `todo` dengan aksi `create`, `update`, `list`, `get`, `delete`, `clear`.
//! - Mesin status 4-kondisi (`pending` → `in_progress` → `completed`, plus `deleted` tombstone).
//! - Pelacakan dependensi `blockedBy` (`addBlockedBy` / `removeBlockedBy`) dengan deteksi siklus graf.
//! - Dukungan `owner` dan `metadata` (merge dengan penghapusan key bernilai `null`).
//! - Persistensi per-branch sesi (`customType: "pipkin-todo-state"` & `toolResult.details`)
//!   serta replay otomatis saat resume, switch session, fork, clone, dan compaction.
//! - Render widget Android (`todos`) dan output slash command `/todos`.

use super::Plugin;
use crate::tools::{Tool, ToolRegistry};
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::{json, Map, Value};
use std::any::Any;
use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};

/// Key widget yang dikirim lewat `extension_ui_request` (`setWidget`) ke Pipkin UI.
pub const TODO_WIDGET_KEY: &str = "todos";

/// `customType` entri JSONL sesi untuk snapshot state `TodoPlugin`.
pub const TODO_STATE_ENTRY: &str = "pipkin-todo-state";

/// Pesan default ketika belum ada todo aktif pada branch saat ini.
pub const NO_TODOS_MESSAGE: &str = "No todos yet. Ask the agent to add some!";

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

    /// Aturan transisi status mengikuti state machine `pipkin-todo`:
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

/// Snapshot `details` yang disimpan pada `toolResult` dan entri sesi `pipkin-todo-state`.
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

pub struct TodoPlugin {
    pub state: Arc<Mutex<TodoState>>,
}

impl Default for TodoPlugin {
    fn default() -> Self {
        Self::new()
    }
}

impl TodoPlugin {
    pub fn new() -> Self {
        Self {
            state: Arc::new(Mutex::new(TodoState::new())),
        }
    }

    pub fn with_state(state: Arc<Mutex<TodoState>>) -> Self {
        Self { state }
    }

    pub fn replace_state(&self, new_state: TodoState) {
        if let Ok(mut guard) = self.state.lock() {
            *guard = new_state;
        }
    }

    pub fn snapshot_state(&self) -> TodoState {
        self.state
            .lock()
            .map(|g| g.clone())
            .unwrap_or_else(|_| TodoState::new())
    }

    pub fn clear_state(&self) -> Option<TodoDetails> {
        let mut guard = self.state.lock().ok()?;
        let had_tasks = !guard.tasks.is_empty() || guard.next_id != 1;
        guard.tasks.clear();
        guard.next_id = 1;
        if had_tasks {
            Some(TodoDetails {
                action: "clear".to_string(),
                params: json!({ "action": "clear" }),
                tasks: Vec::new(),
                next_id: 1,
                error: None,
            })
        } else {
            None
        }
    }

    pub fn get_widget_lines(&self) -> Option<Vec<String>> {
        let state = self.state.lock().ok()?;
        widget_lines_for_state(&state)
    }

    pub fn command_text(&self) -> Option<String> {
        let state = self.state.lock().ok()?;
        command_text_for_state(&state)
    }
}

#[async_trait]
impl Plugin for TodoPlugin {
    fn id(&self) -> &str {
        "pipkin-todo"
    }

    fn aliases(&self) -> Vec<&'static str> {
        vec!["oopbot-todo", "todo", "rpiv-todo"]
    }

    fn name(&self) -> &str {
        "OOPBoT Todo Manager"
    }

    fn description(&self) -> &str {
        "Manajemen checklist task & todo per-sesi dengan dependensi graf, metadata, dan pelacak visual widget Android."
    }

    fn register_tools(&self, registry: &mut ToolRegistry) {
        registry.register(Box::new(TodoTool {
            state: Arc::clone(&self.state),
        }));
    }

    fn system_prompt_append(&self) -> Option<String> {
        Some(
            "## Todo & Task Management Guidelines\n\
            - Gunakan tool `todo` untuk pekerjaan multi-langkah (3+ langkah), saat user memberi daftar tugas, atau untuk melacak rencana tindakan. Lewati untuk pertanyaan percakapan sederhana.\n\
            - Aksi tersedia: `create`, `update`, `list`, `get`, `delete`, `clear`.\n\
            - Mesin status 4-kondisi: `pending` → `in_progress` → `completed`, plus `deleted` (tombstone). Tugas `completed` hanya boleh diubah ke `deleted`.\n\
            - Saat memulai tugas, ubah status ke `in_progress` dengan `activeForm` (misal 'menulis unit test'). Segera tandai `completed` saat selesai dan jangan menandai `completed` jika masih ada error/test gagal. Hanya satu tugas `in_progress` pada satu waktu.\n\
            - Gunakan `blockedBy` saat `create` untuk dependensi awal, dan `addBlockedBy` / `removeBlockedBy` saat `update` (siklus dependensi ditolak).\n\
            - Mendukung `owner` dan `metadata` (kirim nilai `null` pada sebuah key di `metadata` saat `update` untuk menghapus key tersebut).\n\
            - Perubahan pada `todo` tersimpan otomatis di branch sesi dan memperbarui widget di UI Android pengguna."
                .to_string(),
        )
    }

    fn render_widget(&self, widget_key: &str) -> Option<Vec<String>> {
        if widget_key == TODO_WIDGET_KEY {
            self.get_widget_lines()
        } else {
            None
        }
    }

    fn on_branch_replay(&self, branch_entries: &[Value]) {
        self.replace_state(replay_branch_entries(branch_entries));
    }

    fn state_entry_for_tool(
        &self,
        tool_name: &str,
        tool_output: &Value,
    ) -> Option<(&'static str, Value)> {
        if tool_name == "todo" && tool_output["changed"].as_bool() == Some(true) {
            let details = tool_output.get("details")?;
            if !details.is_null() {
                return Some((TODO_STATE_ENTRY, details.clone()));
            }
        }
        None
    }

    fn as_any(&self) -> Option<&dyn Any> {
        Some(self)
    }
}

pub struct TodoTool {
    pub state: Arc<Mutex<TodoState>>,
}

#[async_trait]
impl Tool for TodoTool {
    fn name(&self) -> &'static str {
        "todo"
    }

    fn description(&self) -> &'static str {
        "Kelola daftar tugas (task/todo) untuk melacak kemajuan multi-langkah. \
        Aksi: create (buat tugas baru), update (ubah status/isi/dependensi/owner/metadata), \
        list (lihat tugas, opsional filter status/owner/includeDeleted), \
        get (lihat detail lengkap 1 tugas beserta blocks), delete (tombstone), clear (kosongkan semua tugas). \
        Status: pending → in_progress → completed, plus deleted."
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "action": {
                    "type": "string",
                    "enum": ["create", "update", "list", "get", "delete", "clear"],
                    "description": "Aksi operasi todo yang ingin dijalankan"
                },
                "id": {
                    "type": "number",
                    "description": "ID tugas (wajib untuk aksi update, get, delete)"
                },
                "subject": {
                    "type": "string",
                    "description": "Judul/ringkasan tugas singkat (wajib untuk create)"
                },
                "description": {
                    "type": "string",
                    "description": "Keterangan detail instruksi tugas"
                },
                "activeForm": {
                    "type": "string",
                    "description": "Label aktif bentuk sedang berlangsung (misal: 'menulis unit test')"
                },
                "status": {
                    "type": "string",
                    "enum": ["pending", "in_progress", "completed", "deleted"],
                    "description": "Status tugas baru (pada aksi update) atau filter status (pada aksi list)"
                },
                "blockedBy": {
                    "type": "array",
                    "items": { "type": "number" },
                    "description": "Daftar ID tugas awal yang memblokir tugas ini (khusus create)"
                },
                "addBlockedBy": {
                    "type": "array",
                    "items": { "type": "number" },
                    "description": "Daftar ID tugas untuk ditambahkan ke blockedBy (khusus update, merge aditif)"
                },
                "removeBlockedBy": {
                    "type": "array",
                    "items": { "type": "number" },
                    "description": "Daftar ID tugas untuk dihapus dari blockedBy (khusus update)"
                },
                "owner": {
                    "type": "string",
                    "description": "Agen/pemilik yang ditugaskan pada tugas ini (pada update/create, atau filter pada list)"
                },
                "metadata": {
                    "type": "object",
                    "description": "Metadata arbitrer (objek JSON); kirim nilai null pada suatu key saat update untuk menghapus key tersebut"
                },
                "includeDeleted": {
                    "type": "boolean",
                    "description": "Jika true pada aksi list, tugas yang dihapus (tombstone) juga ditampilkan"
                }
            },
            "required": ["action"]
        })
    }

    fn preview(&self, args: &Value) -> String {
        let action = args["action"].as_str().unwrap_or("unknown");
        let id = args["id"].as_u64();
        let subject = args["subject"].as_str().unwrap_or("");
        match (action, id, subject) {
            ("create", _, s) => format!("todo create: {}", sanitize_text(s)),
            ("update", Some(i), _) => format!("todo update #{}", i),
            ("get", Some(i), _) => format!("todo get #{}", i),
            ("delete", Some(i), _) => format!("todo delete #{}", i),
            ("list", _, _) => "todo list".to_string(),
            _ => format!("todo {}", action),
        }
    }

    async fn execute(&self, args: &Value) -> Result<Value, String> {
        let action = args["action"]
            .as_str()
            .ok_or_else(|| "argumen 'action' wajib diisi".to_string())?;

        let mut state = self.state.lock().map_err(|e| e.to_string())?;

        match action {
            "create" => execute_create(&mut state, args),
            "update" => execute_update(&mut state, args),
            "list" => execute_list(&state, args),
            "get" => execute_get(&state, args),
            "delete" => execute_delete(&mut state, args),
            "clear" => execute_clear(&mut state, args),
            other => Err(format!("Aksi '{}' tidak dikenal untuk tool todo", other)),
        }
    }
}

fn parse_task_id(val: &Value, field: &str) -> Result<u32, String> {
    let n = val
        .as_u64()
        .ok_or_else(|| format!("argumen '{}' (angka positif) wajib diisi", field))?;
    u32::try_from(n).map_err(|_| format!("nilai '{}' di luar jangkauan u32", field))
}

fn parse_id_list(args: &Value, field: &str) -> Result<Option<Vec<u32>>, String> {
    let Some(raw) = args.get(field) else {
        return Ok(None);
    };
    if raw.is_null() {
        return Ok(None);
    }
    let arr = raw
        .as_array()
        .ok_or_else(|| format!("argumen '{}' harus berupa array angka", field))?;
    let mut ids = Vec::with_capacity(arr.len());
    for item in arr {
        let n = item
            .as_u64()
            .ok_or_else(|| format!("setiap elemen pada '{}' harus berupa angka ID tugas", field))?;
        let id = u32::try_from(n)
            .map_err(|_| format!("ID pada '{}' di luar jangkauan u32", field))?;
        if !ids.contains(&id) {
            ids.push(id);
        }
    }
    Ok(Some(ids))
}

fn parse_metadata_arg(args: &Value) -> Result<Option<Map<String, Value>>, String> {
    let Some(raw) = args.get("metadata") else {
        return Ok(None);
    };
    if raw.is_null() {
        return Ok(None);
    }
    let obj = raw
        .as_object()
        .ok_or_else(|| "argumen 'metadata' harus berupa objek JSON".to_string())?;
    Ok(Some(obj.clone()))
}

fn dependency_problem(tasks: &[TaskItem], dep_id: u32) -> Option<String> {
    let Some(task) = tasks.iter().find(|t| t.id == dep_id) else {
        return Some(format!("#{} tidak ditemukan", dep_id));
    };
    if task.status == TaskStatus::Deleted {
        return Some(format!("#{} sudah dihapus (deleted)", dep_id));
    }
    None
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

fn blocks_of(tasks: &[TaskItem]) -> HashMap<u32, Vec<u32>> {
    let mut map: HashMap<u32, Vec<u32>> = HashMap::new();
    for task in tasks {
        for &dep in &task.blocked_by {
            map.entry(dep).or_default().push(task.id);
        }
    }
    map
}

fn merge_metadata(
    existing: Option<&Map<String, Value>>,
    changes: &Map<String, Value>,
) -> Option<Map<String, Value>> {
    let mut merged = existing.cloned().unwrap_or_default();
    for (key, value) in changes {
        if value.is_null() {
            merged.remove(key);
        } else {
            merged.insert(key.clone(), value.clone());
        }
    }
    if merged.is_empty() {
        None
    } else {
        Some(merged)
    }
}

fn make_details(action: &str, params: &Value, state: &TodoState) -> TodoDetails {
    TodoDetails {
        action: action.to_string(),
        params: params.clone(),
        tasks: state.tasks.clone(),
        next_id: state.next_id,
        error: None,
    }
}

fn execute_create(state: &mut TodoState, args: &Value) -> Result<Value, String> {
    let subject = args["subject"]
        .as_str()
        .filter(|s| !s.trim().is_empty())
        .ok_or_else(|| "argumen 'subject' wajib diisi untuk aksi create".to_string())?;

    let blocked_by = parse_id_list(args, "blockedBy")?.unwrap_or_default();
    for &dep in &blocked_by {
        if let Some(problem) = dependency_problem(&state.tasks, dep) {
            return Err(format!("blockedBy: {}", problem));
        }
    }

    let id = state.next_id.max(1);
    state.next_id = id + 1;

    let description = args["description"]
        .as_str()
        .filter(|s| !s.is_empty())
        .map(str::to_string);
    let active_form = args["activeForm"]
        .as_str()
        .filter(|s| !s.is_empty())
        .map(str::to_string);
    let owner = args["owner"]
        .as_str()
        .filter(|s| !s.trim().is_empty())
        .map(str::to_string);
    let metadata = parse_metadata_arg(args)?
        .and_then(|patch| merge_metadata(None, &patch));

    let task = TaskItem {
        id,
        subject: subject.to_string(),
        description,
        active_form,
        status: TaskStatus::Pending,
        blocked_by,
        owner,
        metadata,
    };

    state.tasks.push(task.clone());
    let details = make_details("create", args, state);

    Ok(json!({
        "status": "success",
        "action": "create",
        "changed": true,
        "task": task,
        "details": details,
        "message": format!("Tugas #{} '{}' berhasil dibuat (pending)", id, sanitize_text(subject))
    }))
}

fn execute_update(state: &mut TodoState, args: &Value) -> Result<Value, String> {
    let id = parse_task_id(&args["id"], "id")?;

    let idx = state
        .tasks
        .iter()
        .position(|t| t.id == id)
        .ok_or_else(|| format!("Tugas dengan ID #{} tidak ditemukan", id))?;

    let subject_arg = args.get("subject").and_then(Value::as_str);
    if let Some(s) = subject_arg {
        if s.trim().is_empty() {
            return Err("argumen 'subject' tidak boleh kosong pada aksi update".to_string());
        }
    }
    let description_arg = args.get("description").and_then(Value::as_str);
    let active_form_arg = args.get("activeForm").and_then(Value::as_str);
    let owner_arg = args.get("owner").and_then(Value::as_str);
    let status_arg = match args.get("status").filter(|v| !v.is_null()) {
        Some(v) => {
            let s = v
                .as_str()
                .ok_or_else(|| "argumen 'status' harus berupa string".to_string())?;
            let parsed = TaskStatus::parse(s)
                .ok_or_else(|| format!("status '{}' tidak valid (gunakan pending, in_progress, completed, atau deleted)", s))?;
            Some(parsed)
        }
        None => None,
    };
    let add_blocked_by = parse_id_list(args, "addBlockedBy")?.unwrap_or_default();
    let remove_blocked_by = parse_id_list(args, "removeBlockedBy")?.unwrap_or_default();
    let metadata_patch = parse_metadata_arg(args)?;

    let has_mutable = subject_arg.is_some()
        || description_arg.is_some()
        || active_form_arg.is_some()
        || status_arg.is_some()
        || owner_arg.is_some()
        || metadata_patch.is_some()
        || !add_blocked_by.is_empty()
        || !remove_blocked_by.is_empty();
    if !has_mutable {
        return Err(
            "update memerlukan minimal satu field mutable: subject, description, activeForm, status, owner, metadata, addBlockedBy, atau removeBlockedBy"
                .to_string(),
        );
    }

    let current = state.tasks[idx].clone();
    let target_status = status_arg.unwrap_or(current.status);
    if !current.status.can_transition_to(target_status) {
        return Err(format!(
            "transisi status tidak diizinkan: {} → {}",
            current.status.as_str(),
            target_status.as_str()
        ));
    }

    let remove_set: HashSet<u32> = remove_blocked_by.into_iter().collect();
    let mut next_blocked_by: Vec<u32> = current
        .blocked_by
        .iter()
        .copied()
        .filter(|dep| !remove_set.contains(dep))
        .collect();

    for dep in &add_blocked_by {
        if *dep == current.id {
            return Err(format!(
                "tidak dapat memblokir tugas #{} oleh dirinya sendiri",
                current.id
            ));
        }
        if let Some(problem) = dependency_problem(&state.tasks, *dep) {
            return Err(format!("addBlockedBy: {}", problem));
        }
        if !next_blocked_by.contains(dep) {
            next_blocked_by.push(*dep);
        }
    }

    if !add_blocked_by.is_empty() && closes_cycle(&state.tasks, current.id, &next_blocked_by) {
        return Err("addBlockedBy ditolak karena akan membentuk siklus pada graf blockedBy".to_string());
    }

    let mut updated = current.clone();
    updated.status = target_status;
    if let Some(s) = subject_arg {
        updated.subject = s.to_string();
    }
    if let Some(d) = description_arg {
        updated.description = if d.is_empty() {
            None
        } else {
            Some(d.to_string())
        };
    }
    if let Some(af) = active_form_arg {
        updated.active_form = if af.is_empty() {
            None
        } else {
            Some(af.to_string())
        };
    }
    if let Some(o) = owner_arg {
        updated.owner = if o.trim().is_empty() {
            None
        } else {
            Some(o.to_string())
        };
    }
    updated.blocked_by = next_blocked_by;
    if let Some(patch) = &metadata_patch {
        updated.metadata = merge_metadata(current.metadata.as_ref(), patch);
    }

    let changed = updated != current;
    if changed {
        state.tasks[idx] = updated.clone();
    }

    let message = if !changed {
        format!(
            "No change: tugas #{} sudah sesuai nilai yang diminta (status: {})",
            id,
            target_status.as_str()
        )
    } else if current.status == target_status {
        format!("Tugas #{} berhasil diperbarui", id)
    } else {
        format!(
            "Tugas #{} diperbarui ({} → {})",
            id,
            current.status.as_str(),
            target_status.as_str()
        )
    };

    let details = make_details("update", args, state);
    Ok(json!({
        "status": "success",
        "action": "update",
        "changed": changed,
        "task": updated,
        "details": details,
        "message": message
    }))
}

fn execute_list(state: &TodoState, args: &Value) -> Result<Value, String> {
    let include_deleted = args["includeDeleted"].as_bool().unwrap_or(false);
    let status_filter = match args.get("status").filter(|v| !v.is_null()) {
        Some(v) => {
            let s = v
                .as_str()
                .ok_or_else(|| "argumen 'status' harus berupa string".to_string())?;
            Some(
                TaskStatus::parse(s)
                    .ok_or_else(|| format!("filter status '{}' tidak valid", s))?,
            )
        }
        None => None,
    };
    let owner_filter = args
        .get("owner")
        .and_then(Value::as_str)
        .filter(|s| !s.trim().is_empty());

    let filtered: Vec<&TaskItem> = state
        .tasks
        .iter()
        .filter(|t| {
            if !include_deleted && t.status == TaskStatus::Deleted {
                return false;
            }
            if let Some(st) = status_filter {
                if t.status != st {
                    return false;
                }
            }
            if let Some(owner) = owner_filter {
                if t.owner.as_deref() != Some(owner) {
                    return false;
                }
            }
            true
        })
        .collect();

    let message = if filtered.is_empty() {
        "No tasks".to_string()
    } else {
        filtered
            .iter()
            .map(|t| format_list_line(t))
            .collect::<Vec<_>>()
            .join("\n")
    };

    let details = make_details("list", args, state);
    Ok(json!({
        "status": "success",
        "action": "list",
        "changed": false,
        "total": filtered.len(),
        "tasks": filtered,
        "details": details,
        "message": message
    }))
}

fn execute_get(state: &TodoState, args: &Value) -> Result<Value, String> {
    let id = parse_task_id(&args["id"], "id")?;
    let task = state
        .tasks
        .iter()
        .find(|t| t.id == id)
        .ok_or_else(|| format!("Tugas dengan ID #{} tidak ditemukan", id))?;

    let blocks_map = blocks_of(&state.tasks);
    let blocks = blocks_map.get(&task.id).cloned().unwrap_or_default();
    let message = format_get_lines(task, &blocks);
    let details = make_details("get", args, state);

    Ok(json!({
        "status": "success",
        "action": "get",
        "changed": false,
        "task": task,
        "blocks": blocks,
        "details": details,
        "message": message
    }))
}

fn execute_delete(state: &mut TodoState, args: &Value) -> Result<Value, String> {
    let id = parse_task_id(&args["id"], "id")?;
    let idx = state
        .tasks
        .iter()
        .position(|t| t.id == id)
        .ok_or_else(|| format!("Tugas dengan ID #{} tidak ditemukan", id))?;

    if state.tasks[idx].status == TaskStatus::Deleted {
        return Err(format!("Tugas #{} sudah dihapus (deleted)", id));
    }

    state.tasks[idx].status = TaskStatus::Deleted;
    let deleted_task = state.tasks[idx].clone();
    let details = make_details("delete", args, state);

    Ok(json!({
        "status": "success",
        "action": "delete",
        "changed": true,
        "id": id,
        "task": deleted_task.clone(),
        "details": details,
        "message": format!("Tugas #{} '{}' berhasil dihapus (tombstone)", id, sanitize_text(&deleted_task.subject))
    }))
}

fn execute_clear(state: &mut TodoState, args: &Value) -> Result<Value, String> {
    let count = state.tasks.len();
    let changed = count > 0 || state.next_id != 1;
    state.tasks.clear();
    state.next_id = 1;
    let details = make_details("clear", args, state);

    Ok(json!({
        "status": "success",
        "action": "clear",
        "changed": changed,
        "cleared_count": count,
        "details": details,
        "message": format!("Seluruh daftar tugas ({} item) telah dikosongkan", count)
    }))
}

fn format_ids(ids: &[u32], separator: &str) -> String {
    ids.iter()
        .map(|id| format!("#{}", id))
        .collect::<Vec<_>>()
        .join(separator)
}

fn format_list_line(task: &TaskItem) -> String {
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

fn format_get_lines(task: &TaskItem, blocks: &[u32]) -> String {
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
            lines.push(format!(
                "  metadata: {}",
                Value::Object(meta.clone())
            ));
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
    let show_ids = visible.iter().any(|t| !t.blocked_by.is_empty());

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

        let mut task_line = if show_ids {
            format!("{} {} #{} {}", branch, glyph, task.id, sanitize_text(&task.subject))
        } else {
            format!("{} {} {}", branch, glyph, sanitize_text(&task.subject))
        };

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

fn try_parse_todo_state(val: &Value) -> Option<TodoState> {
    let obj = val.as_object()?;
    let tasks_val = obj.get("tasks")?.as_array()?;
    let next_id_val = obj.get("nextId").or_else(|| obj.get("next_id"))?.as_u64()?;

    let mut tasks = Vec::with_capacity(tasks_val.len());
    for item in tasks_val {
        let task: TaskItem = serde_json::from_value(item.clone()).ok()?;
        tasks.push(task);
    }
    let max_id = tasks.iter().map(|t| t.id).max().unwrap_or(0);
    let parsed_next = u32::try_from(next_id_val).unwrap_or(u32::MAX);
    let next_id = parsed_next.max(max_id.saturating_add(1)).max(1);

    Some(TodoState { tasks, next_id })
}

/// Replay `TodoState` dari rangkaian entri branch sesi aktif (`active_branch_entries`).
///
/// Sesuai perilaku `pipkin-todo`:
/// - Entri custom `customType: "pipkin-todo-state"` menjadi sumber utama terbaru.
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

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_todo_tool_lifecycle() {
        let plugin = TodoPlugin::new();
        let tool = TodoTool {
            state: Arc::clone(&plugin.state),
        };

        // Create
        let res = tool
            .execute(&json!({
                "action": "create",
                "subject": "Desain Arsitektur OOP"
            }))
            .await
            .unwrap();
        assert_eq!(res["status"], "success");
        assert_eq!(res["changed"], true);
        assert_eq!(res["task"]["id"], 1);

        // Update in_progress
        let update_res = tool
            .execute(&json!({
                "action": "update",
                "id": 1,
                "status": "in_progress",
                "activeForm": "mengoding plugin.rs"
            }))
            .await
            .unwrap();
        assert_eq!(update_res["status"], "success");
        assert_eq!(update_res["changed"], true);

        // Periksa widget lines
        let lines = plugin.get_widget_lines().expect("harus ada widget");
        assert_eq!(lines.len(), 2);
        assert!(lines[0].contains("Todos (0/1)"));
        assert!(lines[1].contains("◐ Desain Arsitektur OOP"));
        assert!(lines[1].contains("(mengoding plugin.rs)"));

        // Mark completed
        tool.execute(&json!({
            "action": "update",
            "id": 1,
            "status": "completed"
        }))
        .await
        .unwrap();

        let lines_done = plugin.get_widget_lines().expect("harus ada widget");
        assert!(lines_done[0].contains("Todos (1/1)"));
        assert!(lines_done[1].contains("✓ Desain Arsitektur OOP"));
    }

    #[tokio::test]
    async fn test_todo_dependencies_cycles_transitions_and_metadata() {
        let plugin = TodoPlugin::new();
        let mut reg = ToolRegistry::new();
        plugin.register_tools(&mut reg);
        let tool = reg.get("todo").unwrap();

        // Create #1, #2 blockedBy #1, #3
        let t1 = tool
            .execute(&json!({ "action": "create", "subject": "Riset" }))
            .await
            .unwrap();
        assert_eq!(t1["task"]["id"], 1);

        let t2 = tool
            .execute(&json!({
                "action": "create",
                "subject": "Implementasi",
                "blockedBy": [1],
                "owner": "coder",
                "metadata": { "priority": "high", "track": 1 }
            }))
            .await
            .unwrap();
        assert_eq!(t2["task"]["blockedBy"], json!([1]));
        assert_eq!(t2["task"]["owner"], "coder");

        // Invalid blockedBy at create (#99 missing)
        let bad_dep = tool
            .execute(&json!({
                "action": "create",
                "subject": "Rusak",
                "blockedBy": [99]
            }))
            .await;
        assert!(bad_dep.is_err());

        // Self-block on update must fail
        let self_block = tool
            .execute(&json!({
                "action": "update",
                "id": 1,
                "addBlockedBy": [1]
            }))
            .await;
        assert!(self_block.is_err());

        // Cycle 1 -> 2 -> 1 must fail
        let cycle = tool
            .execute(&json!({
                "action": "update",
                "id": 1,
                "addBlockedBy": [2]
            }))
            .await;
        assert!(cycle.is_err());

        // Update metadata with null deletion + removeBlockedBy
        let upd = tool
            .execute(&json!({
                "action": "update",
                "id": 2,
                "removeBlockedBy": [1],
                "metadata": { "track": null, "stage": "dev" }
            }))
            .await
            .unwrap();
        assert_eq!(upd["changed"], true);
        assert_eq!(upd["task"]["metadata"], json!({ "priority": "high", "stage": "dev" }));
        assert!(upd["task"].get("blockedBy").is_none());

        // Re-sending identical update reports changed = false (no-op)
        let noop = tool
            .execute(&json!({
                "action": "update",
                "id": 2,
                "owner": "coder"
            }))
            .await
            .unwrap();
        assert_eq!(noop["changed"], false);
        assert!(noop["message"].as_str().unwrap().starts_with("No change"));

        // Update without any mutable field fails
        let empty_upd = tool.execute(&json!({ "action": "update", "id": 1 })).await;
        assert!(empty_upd.is_err());

        // Completed -> pending transition is illegal
        tool.execute(&json!({ "action": "update", "id": 1, "status": "completed" }))
            .await
            .unwrap();
        let illegal = tool
            .execute(&json!({ "action": "update", "id": 1, "status": "pending" }))
            .await;
        assert!(illegal.is_err());

        // Get includes reverse `blocks` after adding dep back
        tool.execute(&json!({ "action": "update", "id": 2, "addBlockedBy": [1] }))
            .await
            .unwrap();
        let get1 = tool.execute(&json!({ "action": "get", "id": 1 })).await.unwrap();
        assert_eq!(get1["blocks"], json!([2]));
        assert!(get1["message"].as_str().unwrap().contains("blocks: #2"));

        // Delete #1 -> cannot delete twice, cannot block on deleted #1
        tool.execute(&json!({ "action": "delete", "id": 1 })).await.unwrap();
        assert!(tool.execute(&json!({ "action": "delete", "id": 1 })).await.is_err());
        assert!(tool
            .execute(&json!({ "action": "create", "subject": "X", "blockedBy": [1] }))
            .await
            .is_err());
    }

    #[test]
    fn test_replay_branch_custom_wins_over_tool_result_and_handles_empty() {
        let result_entry = json!({
            "type": "message",
            "message": {
                "role": "toolResult",
                "toolName": "todo",
                "details": {
                    "tasks": [{ "id": 1, "subject": "From result", "status": "pending" }],
                    "nextId": 2
                }
            }
        });
        let custom_entry = json!({
            "type": "custom",
            "customType": TODO_STATE_ENTRY,
            "data": {
                "tasks": [
                    { "id": 1, "subject": "From result", "status": "completed" },
                    { "id": 2, "subject": "From custom", "status": "in_progress", "activeForm": "working" }
                ],
                "nextId": 3
            }
        });

        let only_result = replay_branch_entries(std::slice::from_ref(&result_entry));
        assert_eq!(only_result.tasks.len(), 1);
        assert_eq!(only_result.tasks[0].subject, "From result");

        let both = replay_branch_entries(&[custom_entry.clone(), result_entry]);
        assert_eq!(both.tasks.len(), 2);
        assert_eq!(both.next_id, 3);
        assert_eq!(both.tasks[1].subject, "From custom");

        let plugin = TodoPlugin::new();
        plugin.on_branch_replay(&[custom_entry]);
        assert_eq!(plugin.snapshot_state().tasks.len(), 2);

        let cmd_out = plugin.command_text().expect("command_text should exist");
        assert!(cmd_out.contains("1/2 completed · 1 in progress"));
        assert!(cmd_out.contains("◐ #2 From custom (working)"));

        // Empty branch resets state cleanly
        plugin.on_branch_replay(&[]);
        assert!(plugin.snapshot_state().tasks.is_empty());
        assert!(plugin.get_widget_lines().is_none());
        assert!(plugin.command_text().is_none());
    }

    #[test]
    fn test_sanitize_strips_ansi_and_bidi_controls() {
        let dirty = "\u{001b}[31mMerah\u{001b}[0m\nBaris\t\u{202e}bidi\u{2066}!";
        assert_eq!(sanitize_text(dirty), "Merah Baris bidi!");
    }
}
