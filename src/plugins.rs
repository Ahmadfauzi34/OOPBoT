//! Sistem Plugin OOPBoT (`trait Plugin` & `PluginRegistry`).
//!
//! Desain Arsitektur Berorientasi Objek (OOP) & Plugin Pattern:
//! - Mengikuti prinsip Open/Closed & Dependency Inversion.
//! - Engine inti (`oopbot`) bertindak sebagai harness orkestrator yang agnostik.
//! - Setiap fitur tingkat tinggi (Todo, Web Access, Background Process, Subagent)
//!   diimplementasikan sebagai objek independen yang mengimplementasikan [`Plugin`].
//! - Memungkinkan reverse engineering dan integrasi mudah dari ekstensi Pi-Agent
//!   maupun plugin agen AI open source lainnya (AutoGPT, LangChain, CrewAI).

use crate::gemini::BoxError;
use crate::tools::{truncate, Tool, ToolRegistry};
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

// ===========================================================================
// Kontrak & Abstraksi Plugin (OOP Interface)
// ===========================================================================

pub struct PluginContext {
    pub session_id: String,
    pub cwd: PathBuf,
}

#[async_trait]
pub trait Plugin: Send + Sync {
    /// ID pengenal unik plugin (misal: "pipkin-todo", "pipkin-web-access").
    fn id(&self) -> &str;

    /// Alias ID alternatif untuk pencocokan fleksibel.
    fn aliases(&self) -> Vec<&'static str> {
        Vec::new()
    }

    /// Nama tampilan ramah untuk UI/CLI.
    fn name(&self) -> &str;

    /// Penjelasan ringkas fungsionalitas plugin.
    fn description(&self) -> &str;

    /// Mendaftarkan tool-tool plugin ke ToolRegistry engine.
    fn register_tools(&self, registry: &mut ToolRegistry);

    /// Instruksi sistem / guidelines yang diinjeksikan ke prompt LLM.
    fn system_prompt_append(&self) -> Option<String> {
        None
    }

    /// Hook saat sesi dimulai atau diinisialisasi.
    async fn on_session_init(&self, _ctx: &PluginContext) -> Result<(), BoxError> {
        Ok(())
    }

    /// Jika plugin memiliki representasi visual widget di UI Android (misal Todo),
    /// kembalikan baris-baris tampilan teks widget.
    fn render_widget(&self, _widget_key: &str) -> Option<Vec<String>> {
        None
    }

    /// Status channel real-time plugin untuk topik `pipkin_channel` (misal `"background": { "active": N }`).
    fn channel_status(&self) -> Option<(&'static str, Value)> {
        None
    }
}

// ===========================================================================
// PluginRegistry (Manajer Koleksi & Injeksi Dependensi Plugin)
// ===========================================================================

pub struct PluginRegistry {
    plugins: HashMap<String, Arc<dyn Plugin>>,
    active_ids: HashSet<String>,
}

impl Default for PluginRegistry {
    fn default() -> Self {
        Self::new()
    }
}

impl PluginRegistry {
    pub fn new() -> Self {
        Self {
            plugins: HashMap::new(),
            active_ids: HashSet::new(),
        }
    }

    /// Memuat plugin bawaan yang kompatibel dengan ekstensi Android Pipkin & open-source tools.
    pub fn with_defaults() -> Self {
        let mut registry = Self::new();
        registry.register(Arc::new(TodoPlugin::new()));
        registry.register(Arc::new(WebAccessPlugin::new()));
        registry.register(Arc::new(BackgroundPlugin::new()));
        registry.register(Arc::new(SubagentsPlugin::new()));
        registry
    }

    /// Mendaftarkan plugin baru ke katalog.
    pub fn register(&mut self, plugin: Arc<dyn Plugin>) {
        self.plugins.insert(plugin.id().to_string(), plugin);
    }

    /// Mengaktifkan plugin berdasarkan nama ID, alias, atau path ekstensi.
    /// Contoh: "pipkin-todo", "todo", atau "/path/to/extensions/pipkin-todo/index.js".
    pub fn activate(&mut self, identifier: &str) -> bool {
        let clean_id = extract_plugin_id(identifier);
        
        // Cocokkan langsung dengan ID terdaftar
        for (id, plugin) in &self.plugins {
            if id.eq_ignore_ascii_case(&clean_id) {
                self.active_ids.insert(id.clone());
                return true;
            }
            for alias in plugin.aliases() {
                if alias.eq_ignore_ascii_case(&clean_id) {
                    self.active_ids.insert(id.clone());
                    return true;
                }
            }
        }
        false
    }

    /// Periksa apakah plugin sedang aktif (cocokkan via ID maupun alias).
    pub fn is_active(&self, id: &str) -> bool {
        if self.active_ids.contains(id) {
            return true;
        }
        for active_id in &self.active_ids {
            if let Some(plugin) = self.plugins.get(active_id) {
                if plugin.id().eq_ignore_ascii_case(id) {
                    return true;
                }
                for alias in plugin.aliases() {
                    if alias.eq_ignore_ascii_case(id) {
                        return true;
                    }
                }
            }
        }
        false
    }

    /// Ambil daftar semua plugin yang sedang aktif.
    pub fn active_plugins(&self) -> Vec<Arc<dyn Plugin>> {
        self.active_ids
            .iter()
            .filter_map(|id| self.plugins.get(id).cloned())
            .collect()
    }

    /// Ambil plugin tertentu berdasarkan ID.
    pub fn get_plugin(&self, id: &str) -> Option<Arc<dyn Plugin>> {
        self.plugins.get(id).cloned()
    }

    /// Daftarkan semua tool dari plugin aktif ke ToolRegistry.
    pub fn register_active_tools(&self, registry: &mut ToolRegistry) {
        for plugin in self.active_plugins() {
            plugin.register_tools(registry);
        }
    }

    /// Gabungkan semua tambahan system prompt dari plugin aktif.
    pub fn system_prompt_append(&self) -> String {
        let mut appends = Vec::new();
        for plugin in self.active_plugins() {
            if let Some(prompt) = plugin.system_prompt_append() {
                if !prompt.trim().is_empty() {
                    appends.push(prompt);
                }
            }
        }
        appends.join("\n\n")
    }
}

/// Mengekstrak nama plugin dari path file atau identifier.
/// Misal: `/home/.../extensions/pipkin-todo/index.js` -> `pipkin-todo`.
pub fn extract_plugin_id(identifier: &str) -> String {
    let p = Path::new(identifier);
    if let Some(file_name) = p.file_name().and_then(|s| s.to_str()) {
        if file_name.ends_with(".js") || file_name.ends_with(".ts") {
            if let Some(parent) = p.parent().and_then(|p| p.file_name()).and_then(|s| s.to_str()) {
                return parent.to_string();
            }
        }
    }
    p.file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or(identifier)
        .to_string()
}

// ===========================================================================
// 1. TodoPlugin (Reverse Engineered dari Pipkin Todo & rpiv-todo)
// ===========================================================================

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
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
}

#[derive(Debug, Clone, Serialize, Deserialize)]
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
}

#[derive(Debug, Default, Clone, Serialize, Deserialize)]
pub struct TodoState {
    pub tasks: Vec<TaskItem>,
    pub next_id: u32,
}

impl TodoState {
    pub fn new() -> Self {
        Self {
            tasks: Vec::new(),
            next_id: 1,
        }
    }

    pub fn visible_tasks(&self) -> Vec<&TaskItem> {
        self.tasks.iter().filter(|t| t.status != TaskStatus::Deleted).collect()
    }
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

    pub fn get_widget_lines(&self) -> Option<Vec<String>> {
        let state = self.state.lock().ok()?;
        let visible = state.visible_tasks();
        if visible.is_empty() {
            return None;
        }

        let total = visible.len();
        let completed = visible.iter().filter(|t| t.status == TaskStatus::Completed).count();
        let active = completed < total;
        let show_ids = visible.iter().any(|t| !t.blocked_by.is_empty());

        let mut lines = Vec::new();
        // Sesuai format widget Pipkin (`pipkin-todo/todo.ts`)
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

            let mut task_line = format!("{} {} {}", branch, glyph, task.subject);
            if show_ids {
                task_line.push_str(&format!(" #{}", task.id));
            }
            if task.status == TaskStatus::InProgress {
                if let Some(active_form) = &task.active_form {
                    task_line.push_str(&format!(" ({})", active_form));
                }
            }
            if !task.blocked_by.is_empty() {
                let deps: Vec<String> = task.blocked_by.iter().map(|id| format!("#{}", id)).collect();
                task_line.push_str(&format!(" ⛓ [{}]", deps.join(",")));
            }
            lines.push(task_line);
        }

        Some(lines)
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
        "Manajemen checklist task & todo dengan pelacak visual di composer widget Android."
    }

    fn register_tools(&self, registry: &mut ToolRegistry) {
        registry.register(Box::new(TodoTool {
            state: Arc::clone(&self.state),
        }));
    }

    fn system_prompt_append(&self) -> Option<String> {
        Some(
            "## Todo & Task Management Guidelines\n\
            - Gunakan tool `todo` untuk pekerjaan multi-langkah (3+ langkah) atau ketika user meminta rencana tindakan.\n\
            - Status task: pending -> in_progress -> completed (atau deleted).\n\
            - Saat memulai task, ubah status menjadi `in_progress` dengan `activeForm` (misal 'menulis kode').\n\
            - Segera tandai `completed` saat langkah selesai. Hanya boleh satu task yang `in_progress` secara bersamaan.\n\
            - Perubahan pada todo secara otomatis memperbarui panel widget di UI Android pengguna."
                .to_string(),
        )
    }

    fn render_widget(&self, widget_key: &str) -> Option<Vec<String>> {
        if widget_key == "todos" {
            self.get_widget_lines()
        } else {
            None
        }
    }
}

pub struct TodoTool {
    state: Arc<Mutex<TodoState>>,
}

#[async_trait]
impl Tool for TodoTool {
    fn name(&self) -> &'static str {
        "todo"
    }

    fn description(&self) -> &'static str {
        "Kelola daftar tugas (task/todo) untuk melacak kemajuan multi-langkah. \
        Aksi: create (buat tugas baru), update (ubah status/isi tugas), list (lihat tugas), \
        get (lihat detail 1 tugas), delete (hapus tugas), clear (kosongkan semua tugas). \
        Status: pending, in_progress, completed, deleted."
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
                    "description": "Status tugas baru (pada aksi update) atau filter (pada aksi list)"
                },
                "blockedBy": {
                    "type": "array",
                    "items": { "type": "number" },
                    "description": "Daftar ID tugas yang memblokir tugas ini (pada create)"
                },
                "includeDeleted": {
                    "type": "boolean",
                    "description": "Jika true pada aksi list, tugas yang dihapus juga ditampilkan"
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
            ("create", _, s) => format!("todo create: {}", s),
            ("update", Some(i), _) => format!("todo update #{}", i),
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
            "create" => {
                let subject = args["subject"]
                    .as_str()
                    .filter(|s| !s.trim().is_empty())
                    .ok_or_else(|| "argumen 'subject' wajib diisi untuk aksi create".to_string())?;

                let id = state.next_id;
                state.next_id += 1;

                let description = args["description"].as_str().map(|s| s.to_string());
                let active_form = args["activeForm"].as_str().map(|s| s.to_string());
                let blocked_by = args["blockedBy"]
                    .as_array()
                    .map(|arr| arr.iter().filter_map(|v| v.as_u64().map(|n| n as u32)).collect())
                    .unwrap_or_default();

                let task = TaskItem {
                    id,
                    subject: subject.to_string(),
                    description,
                    active_form,
                    status: TaskStatus::Pending,
                    blocked_by,
                    owner: None,
                };

                state.tasks.push(task.clone());

                Ok(json!({
                    "status": "success",
                    "action": "create",
                    "task": task,
                    "message": format!("Tugas #{} '{}' berhasil dibuat", id, subject)
                }))
            }
            "update" => {
                let id = args["id"]
                    .as_u64()
                    .map(|n| n as u32)
                    .ok_or_else(|| "argumen 'id' (angka) wajib diisi untuk aksi update".to_string())?;

                let task = state
                    .tasks
                    .iter_mut()
                    .find(|t| t.id == id)
                    .ok_or_else(|| format!("Tugas dengan ID #{} tidak ditemukan", id))?;

                if let Some(s) = args["subject"].as_str() {
                    task.subject = s.to_string();
                }
                if let Some(d) = args["description"].as_str() {
                    task.description = Some(d.to_string());
                }
                if let Some(af) = args["activeForm"].as_str() {
                    task.active_form = Some(af.to_string());
                }
                if let Some(st_str) = args["status"].as_str() {
                    if let Some(st) = TaskStatus::parse(st_str) {
                        task.status = st;
                    }
                }

                Ok(json!({
                    "status": "success",
                    "action": "update",
                    "task": task.clone(),
                    "message": format!("Tugas #{} berhasil diperbarui ke status '{:?}'", id, task.status)
                }))
            }
            "list" => {
                let include_deleted = args["includeDeleted"].as_bool().unwrap_or(false);
                let status_filter = args["status"].as_str().and_then(TaskStatus::parse);

                let filtered: Vec<&TaskItem> = state
                    .tasks
                    .iter()
                    .filter(|t| {
                        if !include_deleted && t.status == TaskStatus::Deleted {
                            return false;
                        }
                        if let Some(ref st) = status_filter {
                            if &t.status != st {
                                return false;
                            }
                        }
                        true
                    })
                    .collect();

                Ok(json!({
                    "status": "success",
                    "action": "list",
                    "total": filtered.len(),
                    "tasks": filtered
                }))
            }
            "get" => {
                let id = args["id"]
                    .as_u64()
                    .map(|n| n as u32)
                    .ok_or_else(|| "argumen 'id' wajib diisi untuk aksi get".to_string())?;

                let task = state
                    .tasks
                    .iter()
                    .find(|t| t.id == id)
                    .ok_or_else(|| format!("Tugas dengan ID #{} tidak ditemukan", id))?;

                Ok(json!({
                    "status": "success",
                    "action": "get",
                    "task": task
                }))
            }
            "delete" => {
                let id = args["id"]
                    .as_u64()
                    .map(|n| n as u32)
                    .ok_or_else(|| "argumen 'id' wajib diisi untuk aksi delete".to_string())?;

                let task = state
                    .tasks
                    .iter_mut()
                    .find(|t| t.id == id)
                    .ok_or_else(|| format!("Tugas dengan ID #{} tidak ditemukan", id))?;

                task.status = TaskStatus::Deleted;

                Ok(json!({
                    "status": "success",
                    "action": "delete",
                    "id": id,
                    "message": format!("Tugas #{} berhasil dihapus (tombstone)", id)
                }))
            }
            "clear" => {
                let count = state.tasks.len();
                state.tasks.clear();
                state.next_id = 1;
                Ok(json!({
                    "status": "success",
                    "action": "clear",
                    "cleared_count": count,
                    "message": "Seluruh daftar tugas telah dikosongkan"
                }))
            }
            other => Err(format!("Aksi '{}' tidak dikenal untuk tool todo", other)),
        }
    }
}

// ===========================================================================
// 2. WebAccessPlugin (Pencarian Web & Fetch Konten Tanpa Ketergantungan Eksternal)
// ===========================================================================

pub struct WebAccessPlugin {
    client: reqwest::Client,
}

impl Default for WebAccessPlugin {
    fn default() -> Self {
        Self::new()
    }
}

impl WebAccessPlugin {
    pub fn new() -> Self {
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(15))
            .user_agent("OOPBoT-WebAccess/0.5 (Android Termux; Linux)")
            .build()
            .unwrap_or_else(|_| reqwest::Client::new());
        Self { client }
    }
}

#[async_trait]
impl Plugin for WebAccessPlugin {
    fn id(&self) -> &str {
        "pipkin-web-access"
    }

    fn aliases(&self) -> Vec<&'static str> {
        vec!["oopbot-web-access", "web-access", "web_access", "web_search", "browser"]
    }

    fn name(&self) -> &str {
        "Web Access & Search"
    }

    fn description(&self) -> &str {
        "Kemampuan pencarian internet dan pembacaan konten web (DuckDuckGo / scraping teks)."
    }

    fn register_tools(&self, registry: &mut ToolRegistry) {
        registry.register(Box::new(WebSearchTool {
            client: self.client.clone(),
        }));
        registry.register(Box::new(FetchPageTool {
            client: self.client.clone(),
        }));
    }

    fn system_prompt_append(&self) -> Option<String> {
        Some(
            "## Web Access Guidelines\n\
            - Gunakan `web_search` untuk mencari informasi terbaru di internet, dokumentasi library, atau referensi masalah.\n\
            - Gunakan `fetch_page` untuk membaca isi lengkap dari sebuah URL hasil pencarian."
                .to_string(),
        )
    }
}

pub struct WebSearchTool {
    client: reqwest::Client,
}

#[async_trait]
impl Tool for WebSearchTool {
    fn name(&self) -> &'static str {
        "web_search"
    }

    fn description(&self) -> &'static str {
        "Cari informasi terkini di internet (menggunakan DuckDuckGo instant search / HTML)."
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "query": {
                    "type": "string",
                    "description": "Kata kunci pencarian yang ingin dicari di internet"
                }
            },
            "required": ["query"]
        })
    }

    fn preview(&self, args: &Value) -> String {
        let q = args["query"].as_str().unwrap_or("");
        format!("web_search: \"{}\"", q)
    }

    async fn execute(&self, args: &Value) -> Result<Value, String> {
        let query = args["query"]
            .as_str()
            .filter(|s| !s.trim().is_empty())
            .ok_or_else(|| "argumen 'query' (string) wajib diisi".to_string())?;

        // Gunakan DuckDuckGo HTML Lite yang ringan dan stabil tanpa CAPTCHA JS
        let url = format!(
            "https://html.duckduckgo.com/html/?q={}",
            urlencoding(query)
        );

        let res = self
            .client
            .get(&url)
            .header("User-Agent", "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36")
            .send()
            .await
            .map_err(|e| format!("Gagal menghubungi mesin pencari: {}", e))?;

        if !res.status().is_success() {
            return Err(format!("Pencarian mengembalikan status {}", res.status()));
        }

        let body = res
            .text()
            .await
            .map_err(|e| format!("Gagal membaca respons web: {}", e))?;

        let results = parse_ddg_results(&body);
        if results.is_empty() {
            Ok(json!({
                "query": query,
                "count": 0,
                "results": [],
                "note": "Tidak ada hasil ditemukan atau respons terpotong."
            }))
        } else {
            Ok(json!({
                "query": query,
                "count": results.len(),
                "results": results
            }))
        }
    }
}

pub struct FetchPageTool {
    client: reqwest::Client,
}

#[async_trait]
impl Tool for FetchPageTool {
    fn name(&self) -> &'static str {
        "fetch_page"
    }

    fn description(&self) -> &'static str {
        "Ambil dan baca isi teks dari sebuah halaman web (URL). Tag HTML otomatis dibersihkan menjadi teks polos."
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "url": {
                    "type": "string",
                    "description": "URL website lengkap yang ingin dibaca (misal: https://example.com)"
                }
            },
            "required": ["url"]
        })
    }

    fn preview(&self, args: &Value) -> String {
        let url = args["url"].as_str().unwrap_or("");
        format!("fetch_page: {}", url)
    }

    async fn execute(&self, args: &Value) -> Result<Value, String> {
        let url = args["url"]
            .as_str()
            .filter(|s| !s.trim().is_empty())
            .ok_or_else(|| "argumen 'url' wajib diisi".to_string())?;

        if !url.starts_with("http://") && !url.starts_with("https://") {
            return Err("URL harus diawali dengan http:// atau https://".to_string());
        }

        let res = self
            .client
            .get(url)
            .header("User-Agent", "Mozilla/5.0 (compatible; OOPBoT/0.5; +https://github.com/Ahmadfauzi34/OOPBoT)")
            .send()
            .await
            .map_err(|e| format!("Gagal memuat URL '{}': {}", url, e))?;

        let status = res.status();
        if !status.is_success() {
            return Err(format!("Server mengembalikan status HTTP {}", status));
        }

        let html = res.text().await.map_err(|e| e.to_string())?;
        let text = strip_html_tags(&html);
        let clean = truncate(&text, 25_000);

        Ok(json!({
            "url": url,
            "status": status.as_u16(),
            "content": clean
        }))
    }
}

// ===========================================================================
// 3. BackgroundPlugin (Manajemen Proses Latar Belakang di Termux/Linux)
// ===========================================================================

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BgTaskInfo {
    pub id: u32,
    pub command: String,
    pub status: String,
    pub output: String,
}

pub struct BackgroundPlugin {
    tasks: Arc<Mutex<HashMap<u32, BgTaskInfo>>>,
    kill_handles: Arc<Mutex<HashMap<u32, tokio::sync::oneshot::Sender<()>>>>,
    next_id: Arc<AtomicU32>,
}

impl Default for BackgroundPlugin {
    fn default() -> Self {
        Self::new()
    }
}

impl BackgroundPlugin {
    pub fn new() -> Self {
        Self {
            tasks: Arc::new(Mutex::new(HashMap::new())),
            kill_handles: Arc::new(Mutex::new(HashMap::new())),
            next_id: Arc::new(AtomicU32::new(1)),
        }
    }

    pub fn active_count(&self) -> usize {
        self.tasks
            .lock()
            .map(|map| map.values().filter(|t| t.status == "running").count())
            .unwrap_or(0)
    }
}

#[async_trait]
impl Plugin for BackgroundPlugin {
    fn id(&self) -> &str {
        "pipkin-background"
    }

    fn aliases(&self) -> Vec<&'static str> {
        vec!["oopbot-background", "oopbot_background", "background", "pipkin_background", "tasks"]
    }

    fn name(&self) -> &str {
        "Background Task Runner"
    }

    fn description(&self) -> &str {
        "Jalankan perintah shell berdurasi panjang di latar belakang (background) tanpa memblokir sesi chat."
    }

    fn register_tools(&self, registry: &mut ToolRegistry) {
        registry.register(Box::new(BackgroundTool {
            tasks: Arc::clone(&self.tasks),
            kill_handles: Arc::clone(&self.kill_handles),
            next_id: Arc::clone(&self.next_id),
        }));
    }

    fn system_prompt_append(&self) -> Option<String> {
        Some(
            "## Background Tasks Guidelines\n\
            - Gunakan `background` untuk proses berat/panjang (seperti server lokal, build besar, atau watcher).\n\
            - Aksi tersedia: `start` (mulai), `list` (lihat semua proses latar belakang), `output` (baca output terkini), `kill` (hentikan proses)."
                .to_string(),
        )
    }

    fn channel_status(&self) -> Option<(&'static str, Value)> {
        Some(("background", json!({ "active": self.active_count() })))
    }
}

pub struct BackgroundTool {
    tasks: Arc<Mutex<HashMap<u32, BgTaskInfo>>>,
    kill_handles: Arc<Mutex<HashMap<u32, tokio::sync::oneshot::Sender<()>>>>,
    next_id: Arc<AtomicU32>,
}

#[async_trait]
impl Tool for BackgroundTool {
    fn name(&self) -> &'static str {
        "background"
    }

    fn description(&self) -> &'static str {
        "Jalankan atau kelola perintah shell di latar belakang. Aksi: start, list, output, kill."
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "action": {
                    "type": "string",
                    "enum": ["start", "list", "output", "kill"],
                    "description": "Operasi yang ingin dijalankan pada task latar belakang"
                },
                "command": {
                    "type": "string",
                    "description": "Perintah shell yang ingin dijalankan (wajib untuk start)"
                },
                "id": {
                    "type": "number",
                    "description": "ID task latar belakang (wajib untuk output, kill)"
                }
            },
            "required": ["action"]
        })
    }

    fn preview(&self, args: &Value) -> String {
        let action = args["action"].as_str().unwrap_or("unknown");
        let cmd = args["command"].as_str().unwrap_or("");
        let id = args["id"].as_u64();
        match (action, cmd, id) {
            ("start", c, _) => format!("background start: {}", c),
            ("output", _, Some(i)) => format!("background output #{}", i),
            ("kill", _, Some(i)) => format!("background kill #{}", i),
            _ => format!("background {}", action),
        }
    }

    async fn execute(&self, args: &Value) -> Result<Value, String> {
        let action = args["action"]
            .as_str()
            .ok_or_else(|| "argumen 'action' wajib diisi".to_string())?;

        match action {
            "start" => {
                let cmd = args["command"]
                    .as_str()
                    .filter(|s| !s.trim().is_empty())
                    .ok_or_else(|| "argumen 'command' wajib diisi untuk aksi start".to_string())?;

                let id = self.next_id.fetch_add(1, Ordering::SeqCst);
                let tasks_clone = Arc::clone(&self.tasks);
                let kills_clone = Arc::clone(&self.kill_handles);
                let cmd_str = cmd.to_string();

                let mut child = tokio::process::Command::new("sh")
                    .arg("-c")
                    .arg(&cmd_str)
                    .stdout(std::process::Stdio::piped())
                    .stderr(std::process::Stdio::piped())
                    .kill_on_drop(true)
                    .spawn()
                    .map_err(|e| format!("Gagal mengeksekusi '{}': {}", cmd_str, e))?;

                let (kill_tx, mut kill_rx) = tokio::sync::oneshot::channel::<()>();

                {
                    let mut lock = self.tasks.lock().map_err(|e| e.to_string())?;
                    lock.insert(
                        id,
                        BgTaskInfo {
                            id,
                            command: cmd_str.clone(),
                            status: "running".to_string(),
                            output: String::new(),
                        },
                    );
                }
                if let Ok(mut k_lock) = self.kill_handles.lock() {
                    k_lock.insert(id, kill_tx);
                }

                let stdout_pipe = child.stdout.take();
                let stderr_pipe = child.stderr.take();
                let out_task = tokio::spawn(async move {
                    let mut buf = Vec::new();
                    if let Some(mut s) = stdout_pipe {
                        let _ = tokio::io::AsyncReadExt::read_to_end(&mut s, &mut buf).await;
                    }
                    buf
                });
                let err_task = tokio::spawn(async move {
                    let mut buf = Vec::new();
                    if let Some(mut s) = stderr_pipe {
                        let _ = tokio::io::AsyncReadExt::read_to_end(&mut s, &mut buf).await;
                    }
                    buf
                });

                tokio::spawn(async move {
                    let was_killed;
                    let wait_status;
                    tokio::select! {
                        res = child.wait() => {
                            was_killed = false;
                            wait_status = res;
                        }
                        _ = &mut kill_rx => {
                            was_killed = true;
                            let _ = child.kill().await;
                            wait_status = child.wait().await;
                        }
                    }

                    if let Ok(mut k_lock) = kills_clone.lock() {
                        k_lock.remove(&id);
                    }

                    let stdout_bytes = out_task.await.unwrap_or_default();
                    let stderr_bytes = err_task.await.unwrap_or_default();
                    let stdout = String::from_utf8_lossy(&stdout_bytes);
                    let stderr = String::from_utf8_lossy(&stderr_bytes);

                    let (status, text) = if was_killed {
                        ("killed".to_string(), format!("Killed:\n{}{}", stdout, stderr))
                    } else {
                        match wait_status {
                            Ok(st) => {
                                let code = st.code().unwrap_or(-1);
                                let label = if st.success() { "completed" } else { "failed" };
                                (
                                    label.to_string(),
                                    format!("Exit code {}:\n{}{}", code, stdout, stderr),
                                )
                            }
                            Err(e) => ("error".to_string(), format!("Gagal mengeksekusi: {}", e)),
                        }
                    };

                    if let Ok(mut lock) = tasks_clone.lock() {
                        if let Some(item) = lock.get_mut(&id) {
                            if item.status != "killed" {
                                item.status = status;
                            }
                            item.output = text;
                        }
                    }
                });

                Ok(json!({
                    "status": "started",
                    "id": id,
                    "command": cmd,
                    "message": format!("Perintah dijalankan di background dengan ID #{}", id)
                }))
            }
            "list" => {
                let lock = self.tasks.lock().map_err(|e| e.to_string())?;
                let list: Vec<BgTaskInfo> = lock.values().cloned().collect();
                Ok(json!({
                    "count": list.len(),
                    "tasks": list
                }))
            }
            "output" => {
                let id = args["id"]
                    .as_u64()
                    .map(|n| n as u32)
                    .ok_or_else(|| "argumen 'id' wajib diisi untuk aksi output".to_string())?;

                let lock = self.tasks.lock().map_err(|e| e.to_string())?;
                let item = lock
                    .get(&id)
                    .ok_or_else(|| format!("Task background #{} tidak ditemukan", id))?;

                Ok(json!({
                    "id": id,
                    "status": item.status,
                    "output": truncate(&item.output, 10_000)
                }))
            }
            "kill" => {
                let id = args["id"]
                    .as_u64()
                    .map(|n| n as u32)
                    .ok_or_else(|| "argumen 'id' wajib diisi untuk aksi kill".to_string())?;

                {
                    let mut lock = self.tasks.lock().map_err(|e| e.to_string())?;
                    if let Some(item) = lock.get_mut(&id) {
                        item.status = "killed".to_string();
                    } else {
                        return Err(format!("Task background #{} tidak ditemukan", id));
                    }
                }
                if let Ok(mut k_lock) = self.kill_handles.lock() {
                    if let Some(tx) = k_lock.remove(&id) {
                        let _ = tx.send(());
                    }
                }
                Ok(json!({
                    "status": "killed",
                    "id": id,
                    "message": format!("Task background #{} dihentikan", id)
                }))
            }
            other => Err(format!("Aksi '{}' tidak dikenal untuk tool background", other)),
        }
    }
}

// ===========================================================================
// 4. SubagentsPlugin (Fondasi Orkestrasi Multi-Agen)
// ===========================================================================

pub struct SubagentsPlugin;

impl Default for SubagentsPlugin {
    fn default() -> Self {
        Self::new()
    }
}

impl SubagentsPlugin {
    pub fn new() -> Self {
        Self
    }
}

#[async_trait]
impl Plugin for SubagentsPlugin {
    fn id(&self) -> &str {
        "pipkin-subagents"
    }

    fn aliases(&self) -> Vec<&'static str> {
        vec!["oopbot-subagents", "oopbot_subagents", "subagents", "subagent", "multi-agent"]
    }

    fn name(&self) -> &str {
        "Subagents Coordinator"
    }

    fn description(&self) -> &str {
        "Kemampuan pendelegasian sub-tugas ke agen mandiri anak di latar belakang."
    }

    fn register_tools(&self, _registry: &mut ToolRegistry) {
        // Reserved untuk spawn child instance oopbot
    }

    fn system_prompt_append(&self) -> Option<String> {
        // Jangan mengiklankan tool subagent di system prompt sebelum tool benar-benar terdaftar.
        None
    }
}

// ===========================================================================
// Helper String & Web Scraping Ringan
// ===========================================================================

fn urlencoding(s: &str) -> String {
    let mut out = String::new();
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || b == b'-' || b == b'_' || b == b'.' || b == b'~' {
            out.push(b as char);
        } else if b == b' ' {
            out.push('+');
        } else {
            out.push_str(&format!("%{:02X}", b));
        }
    }
    out
}

fn strip_html_tags(html: &str) -> String {
    let mut out = String::with_capacity(html.len());
    let mut inside = false;
    for ch in html.chars() {
        match ch {
            '<' => inside = true,
            '>' => {
                inside = false;
                out.push(' ');
            }
            _ if !inside => out.push(ch),
            _ => {}
        }
    }

    // Rapikan spasi berlebih
    let mut cleaned = String::new();
    let mut last_space = false;
    for line in out.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            if !last_space {
                cleaned.push('\n');
                last_space = true;
            }
        } else {
            cleaned.push_str(trimmed);
            cleaned.push('\n');
            last_space = false;
        }
    }
    cleaned
}

fn parse_ddg_results(html: &str) -> Vec<Value> {
    let mut results = Vec::new();
    // Cari pola `<a class="result__url" href="...">(...)</a>` atau `<a class="result__snippet"`
    let mut remaining = html;
    while let Some(start) = remaining.find("<div class=\"result__body\">") {
        let chunk = &remaining[start..];
        let end = chunk.find("</div>").unwrap_or(chunk.len());
        let block = &chunk[..end];

        let title = extract_tag_inner(block, "result__title");
        let snippet = extract_tag_inner(block, "result__snippet");
        let url = extract_href(block);

        if !title.is_empty() || !snippet.is_empty() {
            results.push(json!({
                "title": strip_html_tags(&title),
                "snippet": strip_html_tags(&snippet),
                "url": url.unwrap_or_default()
            }));
            if results.len() >= 8 {
                break;
            }
        }
        remaining = &chunk[end.min(chunk.len())..];
    }
    results
}

fn extract_tag_inner(chunk: &str, class_marker: &str) -> String {
    if let Some(pos) = chunk.find(class_marker) {
        let sub = &chunk[pos..];
        if let Some(gt) = sub.find('>') {
            let inner_start = &sub[gt + 1..];
            if let Some(lt) = inner_start.find("</") {
                return inner_start[..lt].to_string();
            }
        }
    }
    String::new()
}

fn extract_href(chunk: &str) -> Option<String> {
    if let Some(href_pos) = chunk.find("href=\"") {
        let after = &chunk[href_pos + 6..];
        if let Some(end_quote) = after.find('"') {
            let raw_url = &after[..end_quote];
            // Format DuckDuckGo redirects: `//duckduckgo.com/l/?uddg=https%3A%2F%2Fexample.com`
            if let Some(uddg_pos) = raw_url.find("uddg=") {
                let encoded = &raw_url[uddg_pos + 5..];
                let end = encoded.find('&').unwrap_or(encoded.len());
                let clean = &encoded[..end];
                return Some(clean.replace("%2F", "/").replace("%3A", ":"));
            }
            return Some(raw_url.to_string());
        }
    }
    None
}

// ===========================================================================
// Unit Tests
// ===========================================================================

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_extract_plugin_id() {
        assert_eq!(
            extract_plugin_id("/data/data/com.termux/files/home/.pipkin/extensions/pipkin-todo/index.js"),
            "pipkin-todo"
        );
        assert_eq!(extract_plugin_id("pipkin-web-access"), "pipkin-web-access");
        assert_eq!(extract_plugin_id("todo"), "todo");
    }

    #[test]
    fn test_plugin_registry_activation() {
        let mut registry = PluginRegistry::with_defaults();
        assert!(!registry.is_active("pipkin-todo"));

        // Aktivasi via path ekstensi Android
        assert!(registry.activate("/home/user/.pipkin/extensions/pipkin-todo/index.js"));
        assert!(registry.is_active("pipkin-todo"));
        assert!(registry.is_active("oopbot-todo"));

        // Aktivasi via alias oopbot
        assert!(!registry.is_active("oopbot-subagents"));
        assert!(registry.activate("oopbot-subagents"));
        assert!(registry.is_active("oopbot-subagents"));
        assert!(registry.is_active("pipkin-subagents"));

        // Aktivasi via alias legacy
        assert!(registry.activate("web-access"));
        assert!(registry.is_active("pipkin-web-access"));
        assert!(registry.is_active("oopbot-web-access"));

        let mut tools = ToolRegistry::new();
        registry.register_active_tools(&mut tools);

        assert!(tools.get("todo").is_some());
        assert!(tools.get("web_search").is_some());
        assert!(tools.get("fetch_page").is_some());
        assert!(tools.get("background").is_none()); // Tidak diaktifkan
    }

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
    async fn reg011_background_kill_terminates_os_child_and_preserves_killed_status() {
        let plugin = BackgroundPlugin::new();
        let mut reg = ToolRegistry::new();
        plugin.register_tools(&mut reg);
        let tool = reg.get("background").expect("tool background harus terdaftar");

        assert_eq!(plugin.active_count(), 0);
        assert_eq!(
            plugin.channel_status(),
            Some(("background", json!({ "active": 0 })))
        );

        let started = tool
            .execute(&json!({
                "action": "start",
                "command": "sleep 30"
            }))
            .await
            .unwrap();
        assert_eq!(started["status"], "started");
        let id = started["id"].as_u64().unwrap();
        assert_eq!(plugin.active_count(), 1);
        assert_eq!(
            plugin.channel_status(),
            Some(("background", json!({ "active": 1 })))
        );

        let killed = tool
            .execute(&json!({
                "action": "kill",
                "id": id
            }))
            .await
            .unwrap();
        assert_eq!(killed["status"], "killed");

        tokio::time::sleep(Duration::from_millis(80)).await;

        let out = tool
            .execute(&json!({
                "action": "output",
                "id": id
            }))
            .await
            .unwrap();
        assert_eq!(
            out["status"], "killed",
            "kill status must not be overwritten after child.kill()"
        );
        assert_eq!(plugin.active_count(), 0);
        assert_eq!(
            plugin.channel_status(),
            Some(("background", json!({ "active": 0 })))
        );
    }

    #[test]
    fn reg011_subagents_plugin_does_not_advertise_missing_tools() {
        let plugin = SubagentsPlugin::new();
        let mut reg = ToolRegistry::new();
        plugin.register_tools(&mut reg);
        assert!(reg.names().is_empty());
        assert!(
            plugin.system_prompt_append().is_none(),
            "SubagentsPlugin must not advertise delegation tools while none are registered"
        );
    }
}
