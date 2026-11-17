//! BackgroundPlugin — Manajemen proses latar belakang OOPBoT.
//!
//! Fitur & organisasi modul:
//! - [`buffer`]: *tail* ring buffer terbatas per task (64 KiB cap) untuk mencegah OOM pada
//!   proses berumur panjang, mendukung cursor inkremental (`since_byte` → `next_byte`),
//!   `tail_lines`, filter baris case-insensitive, dan pencocokan pola UTF-8 aman.
//! - [`process`]: manajemen siklus hidup proses (`start`, `list`, `output`, `wait`, `kill`,
//!   `clear`), isolasi process group Unix (`process_group(0)`), dukungan `cwd`/`env`/`label`,
//!   wait berbasis event (`tokio::sync::watch`), timeout otomatis, notifikasi perubahan status,
//!   dan pembersihan seluruh proses anak saat `Drop`/`shutdown_all`.

pub mod buffer;
pub mod process;

#[cfg(test)]
mod tests;

pub use buffer::{
    DEFAULT_OUTPUT_RETURN_BYTES, MAX_LIST_PREVIEW_BYTES, MAX_OUTPUT_BUFFER_BYTES,
};
pub use process::{
    DEFAULT_WAIT_TIMEOUT_SECS, MAX_RETAINED_TASKS, MAX_RUNNING_TASKS, MAX_TIMEOUT_SECS,
    MAX_WAIT_TIMEOUT_SECS, BgCompletionEvent, CompletionNotifier, StatusNotifier,
};

use super::Plugin;
use crate::tools::{is_dangerous_shell_command, truncate, Tool, ToolRegistry};
use async_trait::async_trait;
use process::BgState;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::any::Any;
use std::sync::Arc;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BgTaskInfo {
    pub id: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
    pub command: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cwd: Option<String>,
    pub status: String,
    pub output: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pid: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub exit_code: Option<i32>,
    pub total_output_bytes: u64,
    pub truncated: bool,
    pub elapsed_secs: u64,
}

pub struct BackgroundPlugin {
    state: Arc<BgState>,
}

impl Default for BackgroundPlugin {
    fn default() -> Self {
        Self::new()
    }
}

impl BackgroundPlugin {
    pub fn new() -> Self {
        Self {
            state: Arc::new(BgState::new()),
        }
    }

    /// Jumlah proses background yang sedang berjalan (`status == "running"`).
    pub fn active_count(&self) -> usize {
        self.state.active_count()
    }

    /// Daftarkan callback untuk menerima jumlah proses aktif saat task background
    /// selesai atau timeout secara mandiri.
    pub fn set_status_notifier(&self, notifier: StatusNotifier) {
        self.state.set_status_notifier(notifier);
    }

    /// Daftarkan callback auto-wakeup untuk membangunkan agen ketika task background
    /// selesai/gagal/timeout secara mandiri di latar belakang.
    pub fn set_completion_notifier(&self, notifier: CompletionNotifier) {
        self.state.set_completion_notifier(notifier);
    }

    /// Hentikan seluruh proses background yang sedang aktif (termasuk process group anaknya).
    pub fn shutdown_all(&self) {
        self.state.shutdown_all();
    }
}

#[async_trait]
impl Plugin for BackgroundPlugin {
    fn id(&self) -> &str {
        "oopbot-background"
    }

    fn aliases(&self) -> Vec<&'static str> {
        vec!["background", "bg"]
    }

    fn name(&self) -> &str {
        "OOPBoT Background"
    }

    fn description(&self) -> &str {
        "Menjalankan proses shell di latar belakang, memantau log (inkremental & filter), menunggu event/pattern (`wait`), dan menghentikan proses"
    }

    fn register_tools(&self, registry: &mut ToolRegistry) {
        registry.register(Box::new(BackgroundTool {
            state: Arc::clone(&self.state),
        }));
    }

    fn system_prompt_append(&self) -> Option<String> {
        Some(
            "## Background Process Guidelines\n\
            - Gunakan tool `background` untuk tugas berdurasi lama (server dev, build panjang, watcher, unduhan besar).\n\
            - Aksi `start`: wajib `command`, opsional `label`, `cwd`, `env`, `timeout_secs`, dan `notify_on_complete` (default `true` untuk auto-wakeup agen saat tugas selesai mandiri).\n\
            - Aksi `wait`: tunggu proses selesai atau kemunculan `pattern` di log secara efisien (event-driven) dengan `wait_timeout_secs` (default 15 dtk, maks 60 dtk) daripada polling berulang.\n\
            - Aksi `output`: baca log inkremental menggunakan cursor `since_byte` -> `next_byte`, atau saring dengan `tail_lines`, `tail_bytes`, dan `filter`.\n\
            - Aksi `list`, `kill`, dan `clear`: pantau daftar task, hentikan proses beserta turunannya, atau bersihkan riwayat task selesai."
                .to_string(),
        )
    }

    fn channel_status(&self) -> Option<(&'static str, Value)> {
        Some(("background", json!({ "active": self.active_count() })))
    }

    fn as_any(&self) -> Option<&dyn Any> {
        Some(self)
    }
}

pub struct BackgroundTool {
    state: Arc<BgState>,
}

#[async_trait]
impl Tool for BackgroundTool {
    fn name(&self) -> &'static str {
        "background"
    }

    fn description(&self) -> &'static str {
        "Jalankan dan kelola proses shell di latar belakang (`start`, `list`, `output`, `wait`, `kill`, `clear`)."
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "action": {
                    "type": "string",
                    "enum": ["start", "list", "output", "wait", "kill", "clear"],
                    "description": "Aksi manajemen proses background."
                },
                "command": {
                    "type": "string",
                    "description": "Perintah shell untuk dijalankan (wajib untuk aksi start)."
                },
                "label": {
                    "type": "string",
                    "description": "Label singkat opsional untuk memudahkan identifikasi task pada `list`."
                },
                "cwd": {
                    "type": "string",
                    "description": "Direktori kerja opsional untuk aksi start (mendukung `~`)."
                },
                "env": {
                    "type": "object",
                    "additionalProperties": { "type": "string" },
                    "description": "Variabel environment tambahan (pasangan key-value string) untuk aksi start."
                },
                "id": {
                    "type": "integer",
                    "description": "ID task background (wajib untuk aksi output, wait, dan kill)."
                },
                "timeout_secs": {
                    "type": "integer",
                    "description": "Batas waktu eksekusi opsional (detik, maks 86400) untuk aksi start; proses otomatis dihentikan bila terlampaui."
                },
                "notify_on_complete": {
                    "type": "boolean",
                    "description": "Opsional (default true): bangunkan agen otomatis lewat follow-up saat proses selesai/gagal/timeout di latar belakang."
                },
                "wait_timeout_secs": {
                    "type": "integer",
                    "description": "Batas waktu tunggu event untuk aksi wait (detik, default 15, maks 60)."
                },
                "pattern": {
                    "type": "string",
                    "description": "Substring/pola log opsional yang ditunggu oleh aksi wait (mis. 'Listening on' atau 'READY')."
                },
                "since_byte": {
                    "type": "integer",
                    "description": "Cursor offset byte (`next_byte` sebelumnya) untuk membaca log baru secara inkremental pada aksi output."
                },
                "tail_bytes": {
                    "type": "integer",
                    "description": "Maksimum byte output terakhir yang dikembalikan untuk aksi output/wait (default 10000, maks 65536)."
                },
                "tail_lines": {
                    "type": "integer",
                    "description": "Ambil hanya N baris terakhir untuk aksi output (maks 2000)."
                },
                "filter": {
                    "type": "string",
                    "description": "Saring hanya baris output yang mengandung substring ini (case-insensitive) untuk aksi output."
                }
            },
            "required": ["action"]
        })
    }

    fn requires_confirmation(&self) -> bool {
        false
    }

    fn requires_confirmation_for_args(&self, args: &Value) -> bool {
        let action = args["action"].as_str().unwrap_or("");
        if action == "start" {
            return args["command"]
                .as_str()
                .map(is_dangerous_shell_command)
                .unwrap_or(false);
        }
        false
    }

    fn preview(&self, args: &Value) -> String {
        let action = args["action"].as_str().unwrap_or("list");
        match action {
            "start" => {
                let cmd = truncate(args["command"].as_str().unwrap_or(""), 60);
                if let Some(label) = args["label"].as_str().filter(|s| !s.trim().is_empty()) {
                    format!("background start [{}]: {}", label.trim(), cmd)
                } else {
                    format!("background start: {}", cmd)
                }
            }
            "output" => format!(
                "background output #{} (since={})",
                args["id"].as_u64().unwrap_or(0),
                args["since_byte"]
                    .as_u64()
                    .map(|n| n.to_string())
                    .unwrap_or_else(|| "tail".to_string())
            ),
            "wait" => {
                let id = args["id"].as_u64().unwrap_or(0);
                if let Some(pat) = args["pattern"].as_str().filter(|s| !s.trim().is_empty()) {
                    format!("background wait #{} pattern='{}'", id, truncate(pat, 30))
                } else {
                    format!("background wait #{}", id)
                }
            }
            "kill" => format!("background kill #{}", args["id"].as_u64().unwrap_or(0)),
            "clear" => "background clear".to_string(),
            _ => format!("background {}", action),
        }
    }

    async fn execute(&self, args: &Value) -> Result<Value, String> {
        let action = args["action"]
            .as_str()
            .filter(|s| !s.trim().is_empty())
            .ok_or_else(|| {
                "argumen 'action' (string) wajib diisi (start, list, output, wait, kill, atau clear)"
                    .to_string()
            })?;
        match action {
            "start" => process::start_task(&self.state, args).await,
            "list" => Ok(process::list_tasks(&self.state)),
            "output" => process::query_output(&self.state, args),
            "wait" => process::wait_for_task(&self.state, args).await,
            "kill" => process::kill_task(&self.state, args),
            "clear" => Ok(process::clear_finished(&self.state)),
            other => Err(format!(
                "Aksi background tidak dikenal: '{}' (gunakan start, list, output, wait, kill, atau clear)",
                other
            )),
        }
    }
}
