//! TodoPlugin — Manajemen checklist tugas & progress multi-langkah.
//!
//! Kompatibel dengan ekstensi OOPBoT `oopbot-todo`:
//! - Tool `todo` dengan aksi `create`, `update`, `batch`, `list`, `get`, `delete`, `clear`.
//! - Mesin status 4-kondisi (`pending` → `in_progress` → `completed`, plus `deleted` tombstone).
//! - Pelacakan dependensi `blockedBy` (`addBlockedBy` / `removeBlockedBy`) dengan deteksi siklus graf.
//! - Dukungan `owner` dan `metadata` (merge dengan penghapusan key bernilai `null`).
//! - Persistensi per-branch sesi (`customType: "oopbot-todo-state"` & `toolResult.details`)
//!   serta replay otomatis saat resume, switch session, fork, clone, dan compaction.
//! - Render widget Android (`todos`) dan output slash command `/todos`.
//!
//! Struktur modul:
//! - [`state`]: model data (`TaskStatus`, `TaskItem`, `TodoState`, `TodoDetails`), graf dependensi, replay branch.
//! - [`actions`]: validasi argumen & eksekusi aksi `create`/`update`/`batch`/`list`/`get`/`delete`/`clear`.
//! - [`format`]: sanitasi teks (ANSI/bidi), formatter `list`/`get`, widget Android, dan `/todos`.

mod actions;
mod format;
mod state;

#[cfg(test)]
mod tests;

pub use format::{command_text_for_state, sanitize_text, widget_lines_for_state};
pub use state::{
    closes_cycle, replay_branch_entries, TaskItem, TaskStatus, TodoDetails, TodoState,
    NO_TODOS_MESSAGE, TODO_STATE_ENTRY, TODO_WIDGET_KEY,
};

use super::Plugin;
use crate::tools::{Tool, ToolRegistry};
use actions::{
    execute_batch, execute_clear, execute_create, execute_delete, execute_get, execute_list,
    execute_update, parse_task_id,
};
use async_trait::async_trait;
use serde_json::{json, Value};
use std::any::Any;
use std::sync::{Arc, Mutex};

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
        let mut guard = self.state.lock().unwrap_or_else(|e| e.into_inner());
        *guard = new_state;
    }

    pub fn snapshot_state(&self) -> TodoState {
        self.state.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }

    pub fn clear_state(&self) -> Option<TodoDetails> {
        let mut guard = self.state.lock().unwrap_or_else(|e| e.into_inner());
        let had_tasks = !guard.tasks.is_empty();
        guard.tasks.clear();
        // Pertahankan next_id tetap naik (monoton) dalam sesi yang sama agar ID lama tidak terpakai ulang.
        let preserved_next_id = guard.next_id.max(1);
        guard.next_id = preserved_next_id;
        if had_tasks {
            Some(TodoDetails {
                action: "clear".to_string(),
                params: json!({ "action": "clear" }),
                tasks: Vec::new(),
                next_id: preserved_next_id,
                error: None,
            })
        } else {
            None
        }
    }

    pub fn get_widget_lines(&self) -> Option<Vec<String>> {
        let state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        widget_lines_for_state(&state)
    }

    pub fn command_text(&self) -> Option<String> {
        let state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        command_text_for_state(&state)
    }
}

#[async_trait]
impl Plugin for TodoPlugin {
    fn id(&self) -> &str {
        "oopbot-todo"
    }

    fn aliases(&self) -> Vec<&'static str> {
        vec!["todo", "rpiv-todo"]
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
            - Aksi tersedia: `create` (tunggal via `subject` atau batch via `tasks`), `update`, `batch` (`operations` atomik `create`/`update`/`delete`), `list`, `get`, `delete`, `clear`.\n\
            - Mesin status 4-kondisi: `pending` → `in_progress` → `completed`, plus `deleted` (tombstone). Tugas `completed` hanya boleh diubah ke `deleted`.\n\
            - Tugas yang masih memiliki `blockedBy` berstatus `pending`/`in_progress` akan ditolak bila langsung diubah ke `in_progress` atau `completed` sebelum penghambatnya selesai atau dihapus.\n\
            - Saat memulai tugas, ubah status ke `in_progress` dengan `activeForm` (misal 'menulis unit test'). Segera tandai `completed` saat selesai dan jangan menandai `completed` jika masih ada error/test gagal. Utamakan hanya satu tugas `in_progress` pada satu waktu.\n\
            - Gunakan `blockedBy` (atau `dependsOnBatchIndex` di mode batch `create.tasks`) untuk dependensi awal, dan `addBlockedBy` / `removeBlockedBy` saat `update` (siklus dependensi ditolak).\n\
            - Mendukung `owner` dan `metadata` (kirim nilai `null` pada sebuah key di `metadata` saat `update` untuk menghapus key tersebut; kirim `null` atau `\"\"` pada `description`/`activeForm`/`owner` untuk mengosongkannya).\n\
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
        Aksi: create (buat 1 tugas via subject atau banyak via tasks), update (ubah status/isi/dependensi/owner/metadata), \
        batch (eksekusi atomik beberapa create/update/delete), list (lihat tugas, opsional filter status/owner/includeDeleted), \
        get (lihat detail lengkap 1 tugas beserta blocks), delete (tombstone & bersihkan blockedBy), clear (kosongkan semua tugas tanpa reset nextId). \
        Status: pending → in_progress → completed, plus deleted."
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "action": {
                    "type": "string",
                    "enum": ["create", "update", "batch", "list", "get", "delete", "clear"],
                    "description": "Aksi operasi todo yang ingin dijalankan"
                },
                "id": {
                    "type": "number",
                    "description": "ID tugas positif >= 1 (wajib untuk aksi update, get, delete)"
                },
                "subject": {
                    "type": "string",
                    "description": "Judul/ringkasan tugas singkat (wajib untuk create tunggal)"
                },
                "tasks": {
                    "type": "array",
                    "description": "Daftar tugas untuk create batch atomik (pengganti subject tunggal); tiap item mendukung subject, description, activeForm, blockedBy, dependsOnBatchIndex, owner, metadata",
                    "items": {
                        "type": "object",
                        "properties": {
                            "subject": { "type": "string" },
                            "description": { "type": "string" },
                            "activeForm": { "type": "string" },
                            "blockedBy": { "type": "array", "items": { "type": "number" } },
                            "dependsOnBatchIndex": { "type": "array", "items": { "type": "number" } },
                            "owner": { "type": "string" },
                            "metadata": { "type": "object" }
                        },
                        "required": ["subject"]
                    }
                },
                "operations": {
                    "type": "array",
                    "description": "Daftar operasi atomik untuk aksi batch (tiap item wajib memiliki field action: create|update|delete beserta argumennya)",
                    "items": { "type": "object" }
                },
                "description": {
                    "type": "string",
                    "description": "Keterangan detail instruksi tugas (kirim null atau string kosong saat update untuk menghapus)"
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
        let id = parse_task_id(&args["id"], "id").ok();
        let subject = args["subject"].as_str().unwrap_or("");
        match (action, id, subject) {
            ("create", _, _) if args.get("tasks").and_then(Value::as_array).is_some() => {
                let n = args["tasks"].as_array().map(|a| a.len()).unwrap_or(0);
                format!("todo create batch ({} tasks)", n)
            }
            ("create", _, s) => format!("todo create: {}", sanitize_text(s)),
            ("batch", _, _) => {
                let n = args["operations"].as_array().map(|a| a.len()).unwrap_or(0);
                format!("todo batch ({} ops)", n)
            }
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

        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());

        match action {
            "create" => execute_create(&mut state, args),
            "update" => execute_update(&mut state, args),
            "batch" => execute_batch(&mut state, args),
            "list" => execute_list(&state, args),
            "get" => execute_get(&state, args),
            "delete" => execute_delete(&mut state, args),
            "clear" => execute_clear(&mut state, args),
            other => Err(format!("Aksi '{}' tidak dikenal untuk tool todo", other)),
        }
    }
}
