//! Sistem Plugin OOPBoT (`trait Plugin` & `PluginRegistry`).
//!
//! Desain Arsitektur Berorientasi Objek (OOP) & Plugin Pattern:
//! - Mengikuti prinsip Open/Closed & Dependency Inversion.
//! - Engine inti (`oopbot`) bertindak sebagai harness orkestrator yang agnostik.
//! - Setiap fitur tingkat tinggi (Todo, Web Access, Background Process, Subagent)
//!   diimplementasikan sebagai objek independen yang mengimplementasikan [`Plugin`].
//! - Memungkinkan reverse engineering dan integrasi mudah dari ekstensi Pi-Agent
//!   maupun plugin agen AI open source lainnya (AutoGPT, LangChain, CrewAI).

pub mod background;
pub mod interact;
pub mod todo;
pub mod web_access;

pub use background::{BackgroundPlugin, BackgroundTool, BgCompletionEvent, BgTaskInfo};
pub use interact::{
    AskCallback, InteractPlugin, NotifyCallback, UiDialogAnswer, UiDialogKind, UiDialogRequest,
};
pub use todo::{
    NO_TODOS_MESSAGE, TODO_STATE_ENTRY, TODO_WIDGET_KEY, TaskItem, TaskStatus, TodoDetails,
    TodoPlugin, TodoState, TodoTool,
};
pub use web_access::{FetchPageTool, WebAccessPlugin, WebSearchTool};

use crate::gemini::BoxError;
use crate::tools::ToolRegistry;
use async_trait::async_trait;
use serde_json::Value;
use std::any::Any;
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;

// ===========================================================================
// Kontrak & Abstraksi Plugin (OOP Interface)
// ===========================================================================

pub struct PluginContext {
    pub session_id: String,
    pub cwd: PathBuf,
}

#[async_trait]
pub trait Plugin: Send + Sync {
    /// ID pengenal unik plugin (misal: "oopbot-todo", "oopbot-web-access").
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

    /// Replay status internal plugin dari daftar entri branch sesi aktif
    /// (dipanggil saat startup/resume, switch_session, new_session, fork, clone, dan compaction).
    fn on_branch_replay(&self, _branch_entries: &[Value]) {}

    /// Jika pemanggilan tool mengubah state plugin dan perlu disimpan ke branch sesi JSONL
    /// sebagai custom entry, kembalikan `(customType, data)`.
    fn state_entry_for_tool(
        &self,
        _tool_name: &str,
        _tool_output: &Value,
    ) -> Option<(&'static str, Value)> {
        None
    }

    /// Jika plugin memiliki representasi visual widget di UI Android (misal Todo),
    /// kembalikan baris-baris tampilan teks widget.
    fn render_widget(&self, _widget_key: &str) -> Option<Vec<String>> {
        None
    }

    /// Status channel real-time plugin untuk topik `oopbot_channel` (misal `"background": { "active": N }`).
    fn channel_status(&self) -> Option<(&'static str, Value)> {
        None
    }

    /// Downcast opsional untuk pemanggilan khusus dari slash command/runtime.
    fn as_any(&self) -> Option<&dyn Any> {
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

    /// Memuat plugin bawaan yang kompatibel dengan ekstensi Android OOPBoT & open-source tools.
    pub fn with_defaults() -> Self {
        let mut registry = Self::new();
        registry.register(Arc::new(TodoPlugin::new()));
        registry.register(Arc::new(WebAccessPlugin::new()));
        registry.register(Arc::new(BackgroundPlugin::new()));
        registry.register(Arc::new(SubagentsPlugin::new()));
        registry.register(Arc::new(InteractPlugin::new()));
        registry
    }

    /// Mendaftarkan plugin baru ke katalog.
    pub fn register(&mut self, plugin: Arc<dyn Plugin>) {
        self.plugins.insert(plugin.id().to_string(), plugin);
    }

    /// Mengaktifkan plugin berdasarkan nama ID, alias, atau path ekstensi.
    /// Contoh: "oopbot-todo", "todo", atau "/path/to/extensions/oopbot-todo/index.js".
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

    /// Replay seluruh plugin terhadap entri branch sesi yang sedang dibuka.
    pub fn replay_branch(&self, branch_entries: &[Value]) {
        for plugin in self.plugins.values() {
            plugin.on_branch_replay(branch_entries);
        }
    }

    /// Kumpulkan custom state entry yang harus dipersist ke JSONL sesi setelah eksekusi tool.
    pub fn collect_state_entries_for_tool(
        &self,
        tool_name: &str,
        tool_output: &Value,
    ) -> Vec<(&'static str, Value)> {
        self.active_plugins()
            .into_iter()
            .filter_map(|p| p.state_entry_for_tool(tool_name, tool_output))
            .collect()
    }
}

/// Mengekstrak nama plugin dari path file atau identifier.
/// Misal: `/home/.../extensions/oopbot-todo/index.js` -> `oopbot-todo`.
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
// 3. SubagentsPlugin (Fondasi Orkestrasi Multi-Agen)
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
        "oopbot-subagents"
    }

    fn aliases(&self) -> Vec<&'static str> {
        vec!["oopbot_subagents", "subagents", "subagent", "multi-agent"]
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
// Unit Tests
// ===========================================================================

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_extract_plugin_id() {
        assert_eq!(
            extract_plugin_id("/data/data/com.termux/files/home/.oopbot/extensions/oopbot-todo/index.js"),
            "oopbot-todo"
        );
        assert_eq!(extract_plugin_id("oopbot-web-access"), "oopbot-web-access");
        assert_eq!(extract_plugin_id("todo"), "todo");
    }

    #[test]
    fn test_plugin_registry_activation() {
        let mut registry = PluginRegistry::with_defaults();
        assert!(!registry.is_active("oopbot-todo"));

        // Aktivasi via path ekstensi Android
        assert!(registry.activate("/home/user/.oopbot/extensions/oopbot-todo/index.js"));
        assert!(registry.is_active("oopbot-todo"));

        // Aktivasi via nama oopbot
        assert!(!registry.is_active("oopbot-subagents"));
        assert!(registry.activate("oopbot-subagents"));
        assert!(registry.is_active("oopbot-subagents"));

        // Aktivasi via alias pendek
        assert!(registry.activate("web-access"));
        assert!(registry.is_active("oopbot-web-access"));

        let mut tools = ToolRegistry::new();
        registry.register_active_tools(&mut tools);

        assert!(tools.get("todo").is_some());
        assert!(tools.get("web_search").is_some());
        assert!(tools.get("fetch_page").is_some());
        assert!(tools.get("background").is_none()); // Tidak diaktifkan
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
