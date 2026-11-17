//! InteractPlugin — Tool komunikasi dua arah bertahap (`notify_user` & `ask_user`).
//!
//! Memungkinkan AI tetap terhubung dengan user saat mengerjakan tugas multi-langkah:
//! - [`NotifyUserTool`] (`notify_user`): mengirim pesan progres (`info` yang me-replace
//!   baris info sebelumnya di UI Android, serta `warning` / `error` sebagai kartu tersendiri)
//!   melalui protokol `extension_ui_request` (`method: "notify"`).
//! - [`AskUserTool`] (`ask_user`): mengajukan pertanyaan interaktif (`confirm`, `select`,
//!   `input`, `editor`) ke UI OOPBoT via `extension_ui_request` dan menunggu jawaban user
//!   lewat `extension_ui_response` tanpa kehilangan state eksekusi.

use super::Plugin;
use crate::tools::{truncate, Tool, ToolRegistry};
use async_trait::async_trait;
use serde_json::{json, Value};
use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Mutex};

/// Batas default dan maksimum timeout dialog `ask_user` (detik).
pub const DEFAULT_ASK_TIMEOUT_SECS: u64 = 120;
pub const MAX_ASK_TIMEOUT_SECS: u64 = 600;
const MAX_NOTIFY_MESSAGE_BYTES: usize = 2000;
const MAX_ASK_OPTIONS: usize = 20;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UiDialogKind {
    Confirm { message: Option<String> },
    Select { options: Vec<String> },
    Input { placeholder: Option<String> },
    Editor { prefill: Option<String> },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UiDialogRequest {
    pub title: String,
    pub kind: UiDialogKind,
    pub timeout_secs: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UiDialogAnswer {
    Confirmed(bool),
    Value(String),
    Cancelled,
    TimedOut,
}

pub type NotifyCallback = Arc<dyn Fn(&str, &str) + Send + Sync>;
pub type AskCallback = Arc<
    dyn Fn(UiDialogRequest) -> Pin<Box<dyn Future<Output = Result<UiDialogAnswer, String>> + Send>>
        + Send
        + Sync,
>;

#[derive(Default)]
struct InteractState {
    notify_cb: Mutex<Option<NotifyCallback>>,
    ask_cb: Mutex<Option<AskCallback>>,
}

impl InteractState {
    fn set_notify_handler(&self, cb: NotifyCallback) {
        let mut lock = self.notify_cb.lock().unwrap_or_else(|e| e.into_inner());
        *lock = Some(cb);
    }

    fn set_ask_handler(&self, cb: AskCallback) {
        let mut lock = self.ask_cb.lock().unwrap_or_else(|e| e.into_inner());
        *lock = Some(cb);
    }

    fn emit_notify(&self, level: &str, message: &str) -> bool {
        let cb = self
            .notify_cb
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        if let Some(f) = cb {
            f(level, message);
            true
        } else {
            false
        }
    }

    fn ask_handler(&self) -> Option<AskCallback> {
        self.ask_cb
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }
}

pub struct InteractPlugin {
    state: Arc<InteractState>,
}

impl Default for InteractPlugin {
    fn default() -> Self {
        Self::new()
    }
}

impl InteractPlugin {
    pub fn new() -> Self {
        Self {
            state: Arc::new(InteractState::default()),
        }
    }

    /// Daftarkan callback pengiriman notifikasi UI (mis. `extension_ui_request` `method: "notify"`).
    pub fn set_notify_handler(&self, cb: NotifyCallback) {
        self.state.set_notify_handler(cb);
    }

    /// Daftarkan callback dialog interaktif UI (`confirm`, `select`, `input`, `editor`).
    pub fn set_ask_handler(&self, cb: AskCallback) {
        self.state.set_ask_handler(cb);
    }
}

#[async_trait]
impl Plugin for InteractPlugin {
    fn id(&self) -> &str {
        "oopbot-interact"
    }

    fn aliases(&self) -> Vec<&'static str> {
        vec!["interact", "ui"]
    }

    fn name(&self) -> &str {
        "OOPBoT Interactive Chat"
    }

    fn description(&self) -> &str {
        "Mengirim pembaruan progres bertahap (`notify_user`) dan mengajukan dialog interaktif (`ask_user`) di tengah tugas"
    }

    fn register_tools(&self, registry: &mut ToolRegistry) {
        registry.register(Box::new(NotifyUserTool {
            state: Arc::clone(&self.state),
        }));
        registry.register(Box::new(AskUserTool {
            state: Arc::clone(&self.state),
        }));
    }

    fn system_prompt_append(&self) -> Option<String> {
        Some(
            "## Interactive Progress & User Clarification Guidelines\n\
            - Gunakan `notify_user` untuk memberi kabar progres bertahap ke UI chat saat mengerjakan tugas panjang multi-langkah (`level: \"info\"`, atau `\"warning\"` / `\"error\"` untuk peringatan penting).\n\
            - Gunakan `ask_user` ketika kamu butuh keputusan, konfirmasi, pilihan opsi (`select`), atau masukan teks (`input` / `editor`) dari user di tengah alur kerja tanpa membatalkan konteks tugas."
                .to_string(),
        )
    }

    fn as_any(&self) -> Option<&dyn std::any::Any> {
        Some(self)
    }
}

fn optional_str<'a>(args: &'a Value, key: &str) -> Result<Option<&'a str>, String> {
    if args[key].is_null() {
        return Ok(None);
    }
    args[key]
        .as_str()
        .map(Some)
        .ok_or_else(|| format!("argumen '{}' harus berupa string", key))
}

pub struct NotifyUserTool {
    state: Arc<InteractState>,
}

#[async_trait]
impl Tool for NotifyUserTool {
    fn name(&self) -> &'static str {
        "notify_user"
    }

    fn description(&self) -> &'static str {
        "Kirim notifikasi/kabar progres singkat secara bertahap ke UI chat user selama tugas masih berjalan."
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "message": {
                    "type": "string",
                    "description": "Isi pesan kabar progres atau pemberitahuan untuk ditampilkan ke user."
                },
                "level": {
                    "type": "string",
                    "enum": ["info", "warning", "error"],
                    "description": "Tingkat notifikasi (default 'info'). Notice 'info' berturut-turut memperbarui status progres dengan rapi."
                }
            },
            "required": ["message"]
        })
    }

    fn preview(&self, args: &Value) -> String {
        let level = args["level"].as_str().unwrap_or("info");
        let msg = truncate(args["message"].as_str().unwrap_or(""), 60);
        format!("notify_user [{}]: {}", level, msg)
    }

    async fn execute(&self, args: &Value) -> Result<Value, String> {
        let raw_msg = args["message"]
            .as_str()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .ok_or_else(|| "argumen 'message' (string) wajib diisi untuk notify_user".to_string())?;

        let level = match optional_str(args, "level")? {
            None => "info",
            Some(l) => match l.trim().to_ascii_lowercase().as_str() {
                "info" => "info",
                "warning" => "warning",
                "error" => "error",
                other => {
                    return Err(format!(
                        "Nilai 'level' tidak valid: '{}' (gunakan 'info', 'warning', atau 'error')",
                        other
                    ))
                }
            },
        };

        let message = truncate(raw_msg, MAX_NOTIFY_MESSAGE_BYTES);
        let delivered = self.state.emit_notify(level, &message);

        Ok(json!({
            "status": "sent",
            "delivered": delivered,
            "level": level,
            "message": message
        }))
    }
}

pub struct AskUserTool {
    state: Arc<InteractState>,
}

#[async_trait]
impl Tool for AskUserTool {
    fn name(&self) -> &'static str {
        "ask_user"
    }

    fn description(&self) -> &'static str {
        "Ajukan pertanyaan atau pilihan interaktif ke UI user (`confirm`, `select`, `input`, atau `editor`) dan tunggu jawabannya."
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "title": {
                    "type": "string",
                    "description": "Judul pertanyaan singkat yang ditampilkan di dialog UI user."
                },
                "kind": {
                    "type": "string",
                    "enum": ["confirm", "select", "input", "editor"],
                    "description": "Jenis dialog (default otomatis 'select' jika `options` diberikan, selainnya 'input')."
                },
                "message": {
                    "type": "string",
                    "description": "Penjelasan detail tambahan untuk dialog `confirm`."
                },
                "options": {
                    "type": "array",
                    "items": { "type": "string" },
                    "description": "Daftar pilihan untuk dialog `select` (maksimal 20 opsi)."
                },
                "placeholder": {
                    "type": "string",
                    "description": "Teks placeholder untuk dialog `input`."
                },
                "prefill": {
                    "type": "string",
                    "description": "Teks awal untuk dialog `editor`."
                },
                "timeout_secs": {
                    "type": "integer",
                    "description": "Batas waktu tunggu jawaban user (detik, default 120, maks 600)."
                }
            },
            "required": ["title"]
        })
    }

    fn preview(&self, args: &Value) -> String {
        let kind = args["kind"].as_str().unwrap_or_else(|| {
            if args["options"].is_array() {
                "select"
            } else {
                "input"
            }
        });
        let title = truncate(args["title"].as_str().unwrap_or(""), 60);
        format!("ask_user ({}): {}", kind, title)
    }

    async fn execute(&self, args: &Value) -> Result<Value, String> {
        let title = args["title"]
            .as_str()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .ok_or_else(|| "argumen 'title' (string) wajib diisi untuk ask_user".to_string())?
            .chars()
            .take(240)
            .collect::<String>();

        let options: Option<Vec<String>> = if args["options"].is_null() {
            None
        } else {
            let arr = args["options"]
                .as_array()
                .ok_or_else(|| "argumen 'options' harus berupa array string".to_string())?;
            if arr.len() > MAX_ASK_OPTIONS {
                return Err(format!(
                    "Jumlah 'options' maksimal {} item",
                    MAX_ASK_OPTIONS
                ));
            }
            let mut list = Vec::with_capacity(arr.len());
            for (idx, item) in arr.iter().enumerate() {
                let s = item
                    .as_str()
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                    .ok_or_else(|| {
                        format!("Elemen 'options[{}]' harus berupa string non-kosong", idx)
                    })?;
                list.push(s.to_string());
            }
            Some(list)
        };

        let explicit_kind = optional_str(args, "kind")?.map(|s| s.trim().to_ascii_lowercase());
        let kind_name = match explicit_kind.as_deref() {
            Some("confirm") => "confirm",
            Some("select") => "select",
            Some("input") => "input",
            Some("editor") => "editor",
            Some(other) => {
                return Err(format!(
                    "Nilai 'kind' tidak valid: '{}' (gunakan 'confirm', 'select', 'input', atau 'editor')",
                    other
                ))
            }
            None => {
                if options.is_some() {
                    "select"
                } else {
                    "input"
                }
            }
        };

        let timeout_secs = if args["timeout_secs"].is_null() {
            DEFAULT_ASK_TIMEOUT_SECS
        } else {
            let secs = args["timeout_secs"]
                .as_u64()
                .ok_or_else(|| "argumen 'timeout_secs' harus berupa bilangan bulat non-negatif".to_string())?;
            if secs == 0 {
                return Err("argumen 'timeout_secs' minimal 1 detik".to_string());
            }
            secs.min(MAX_ASK_TIMEOUT_SECS)
        };

        let kind = match kind_name {
            "confirm" => {
                let message = optional_str(args, "message")?
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                    .map(|s| s.to_string());
                UiDialogKind::Confirm { message }
            }
            "select" => {
                let opts = options.ok_or_else(|| {
                    "argumen 'options' wajib diisi untuk dialog kind 'select'".to_string()
                })?;
                if opts.is_empty() {
                    return Err("argumen 'options' minimal berisi 1 pilihan".to_string());
                }
                UiDialogKind::Select { options: opts }
            }
            "input" => {
                let placeholder = optional_str(args, "placeholder")?
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                    .map(|s| s.to_string());
                UiDialogKind::Input { placeholder }
            }
            "editor" => {
                let prefill = optional_str(args, "prefill")?.map(|s| s.to_string());
                UiDialogKind::Editor { prefill }
            }
            _ => unreachable!(),
        };

        let handler = self.state.ask_handler().ok_or_else(|| {
            "Dialog interaktif UI ('ask_user') tidak tersedia pada mode runtime ini".to_string()
        })?;

        let answer = handler(UiDialogRequest {
            title,
            kind,
            timeout_secs,
        })
        .await?;

        Ok(match answer {
            UiDialogAnswer::Confirmed(confirmed) => json!({
                "status": "answered",
                "kind": kind_name,
                "confirmed": confirmed
            }),
            UiDialogAnswer::Value(value) => json!({
                "status": "answered",
                "kind": kind_name,
                "value": value
            }),
            UiDialogAnswer::Cancelled => json!({
                "status": "cancelled",
                "kind": kind_name,
                "cancelled": true
            }),
            UiDialogAnswer::TimedOut => json!({
                "status": "timeout",
                "kind": kind_name,
                "timed_out": true
            }),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex as StdMutex;

    fn setup_interact() -> (InteractPlugin, ToolRegistry) {
        let plugin = InteractPlugin::new();
        let mut reg = ToolRegistry::new();
        plugin.register_tools(&mut reg);
        (plugin, reg)
    }

    #[tokio::test]
    async fn notify_user_validates_inputs_and_invokes_callback() {
        let (plugin, reg) = setup_interact();
        let tool = reg.get("notify_user").unwrap();

        let captured = Arc::new(StdMutex::new(Vec::<(String, String)>::new()));
        let cap_clone = Arc::clone(&captured);
        plugin.set_notify_handler(Arc::new(move |lvl, msg| {
            cap_clone
                .lock()
                .unwrap()
                .push((lvl.to_string(), msg.to_string()));
        }));

        assert!(tool.execute(&json!({})).await.is_err());
        assert!(tool
            .execute(&json!({ "message": "   " }))
            .await
            .is_err());
        assert!(tool
            .execute(&json!({ "message": "ok", "level": "fatal" }))
            .await
            .is_err());
        assert!(tool
            .execute(&json!({ "message": "ok", "level": 123 }))
            .await
            .is_err());

        let res = tool
            .execute(&json!({ "message": "Kompilasi selesai 50%", "level": "warning" }))
            .await
            .unwrap();
        assert_eq!(res["status"], "sent");
        assert_eq!(res["delivered"], true);
        assert_eq!(res["level"], "warning");
        assert_eq!(
            captured.lock().unwrap().as_slice(),
            &[("warning".to_string(), "Kompilasi selesai 50%".to_string())]
        );
    }

    #[tokio::test]
    async fn ask_user_validates_inputs_and_maps_dialog_answers() {
        let (plugin, reg) = setup_interact();
        let tool = reg.get("ask_user").unwrap();

        // Tanpa handler -> error jelas
        assert!(tool.execute(&json!({ "title": "Pilih branch?" })).await.is_err());

        let last_req = Arc::new(StdMutex::new(None::<UiDialogRequest>));
        let req_clone = Arc::clone(&last_req);
        plugin.set_ask_handler(Arc::new(move |req| {
            *req_clone.lock().unwrap() = Some(req.clone());
            Box::pin(async move {
                match req.kind {
                    UiDialogKind::Confirm { .. } => Ok(UiDialogAnswer::Confirmed(true)),
                    UiDialogKind::Select { options } => {
                        Ok(UiDialogAnswer::Value(options[1].clone()))
                    }
                    UiDialogKind::Input { .. } => Ok(UiDialogAnswer::Value("custom-val".into())),
                    UiDialogKind::Editor { .. } => Ok(UiDialogAnswer::Cancelled),
                }
            })
        }));

        // Validasi argumen salah
        assert!(tool.execute(&json!({})).await.is_err());
        assert!(tool
            .execute(&json!({ "title": "Pilih", "kind": "select" }))
            .await
            .is_err());
        assert!(tool
            .execute(&json!({ "title": "Pilih", "options": [] }))
            .await
            .is_err());
        assert!(tool
            .execute(&json!({ "title": "Pilih", "timeout_secs": 0 }))
            .await
            .is_err());
        assert!(tool
            .execute(&json!({ "title": "Pilih", "timeout_secs": -5 }))
            .await
            .is_err());

        // Auto-infer kind = select saat options ada
        let sel = tool
            .execute(&json!({
                "title": "Target deploy?",
                "options": ["staging", "production"]
            }))
            .await
            .unwrap();
        assert_eq!(sel["status"], "answered");
        assert_eq!(sel["kind"], "select");
        assert_eq!(sel["value"], "production");

        let conf = tool
            .execute(&json!({
                "title": "Lanjut migrasi DB?",
                "kind": "confirm",
                "message": "Schema akan diperbarui"
            }))
            .await
            .unwrap();
        assert_eq!(conf["confirmed"], true);

        let inp = tool
            .execute(&json!({ "title": "Nama service?", "placeholder": "oopbot-api" }))
            .await
            .unwrap();
        assert_eq!(inp["kind"], "input");
        assert_eq!(inp["value"], "custom-val");

        let ed = tool
            .execute(&json!({ "title": "Edit pesan commit", "kind": "editor", "prefill": "feat: " }))
            .await
            .unwrap();
        assert_eq!(ed["status"], "cancelled");
        assert_eq!(ed["cancelled"], true);
    }
}
