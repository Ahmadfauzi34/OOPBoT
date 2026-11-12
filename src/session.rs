//! Manajemen Multi-Sesi & Tab (`WorkspaceTabs`) ala Pipkin.
//!
//! Setiap tab/sesi menyimpan:
//! - ID sesi (`tab-1`, `tab-2`, dst. atau nama kustom)
//! - Judul tab (otomatis dari pesan pertama atau diberi nama)
//! - Model yang dipilih khusus untuk tab tersebut
//! - Riwayat `contents` mentah untuk Gemini API
//! - Riwayat `TimelineEntry` terlipat (`StepsFold`) untuk dirender UI

use crate::events::TimelineEntry;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::fs;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChatSession {
    pub id: String,
    pub title: String,
    pub model: String,
    pub history: Vec<Value>,
    pub timeline: Vec<TimelineEntry>,
}

impl ChatSession {
    pub fn new(id: &str, model: &str) -> Self {
        Self {
            id: id.to_string(),
            title: format!("Tab {}", id),
            model: model.to_string(),
            history: Vec::new(),
            timeline: Vec::new(),
        }
    }

    pub fn update_title_from_prompt(&mut self, prompt: &str) {
        if self.history.is_empty() && self.title.starts_with("Tab ") {
            let clean: String = prompt.chars().take(36).collect();
            self.title = if prompt.chars().count() > 36 {
                format!("{}…", clean.trim())
            } else {
                clean.trim().to_string()
            };
        }
    }

    pub fn clear(&mut self) {
        self.history.clear();
        self.timeline.clear();
    }
}

/// Pengelola multi-tab percakapan (di memori + opsional simpan ke disk).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkspaceTabs {
    tabs: Vec<ChatSession>,
    active_index: usize,
    next_id: usize,
}

impl WorkspaceTabs {
    pub fn new(default_model: &str) -> Self {
        let first = ChatSession::new("1", default_model);
        Self {
            tabs: vec![first],
            active_index: 0,
            next_id: 2,
        }
    }

    pub fn from_session(session: ChatSession) -> Self {
        Self {
            tabs: vec![session],
            active_index: 0,
            next_id: 2,
        }
    }

    pub fn active(&self) -> &ChatSession {
        &self.tabs[self.active_index]
    }

    pub fn active_mut(&mut self) -> &mut ChatSession {
        &mut self.tabs[self.active_index]
    }

    pub fn list(&self) -> &[ChatSession] {
        &self.tabs
    }

    pub fn active_index(&self) -> usize {
        self.active_index
    }

    /// Buat tab baru dengan model tertentu dan langsung pindah ke tab tersebut.
    pub fn new_tab(&mut self, model: &str) -> &ChatSession {
        let id = self.next_id.to_string();
        self.next_id += 1;
        self.tabs.push(ChatSession::new(&id, model));
        self.active_index = self.tabs.len() - 1;
        self.active()
    }

    /// Pindah tab berdasarkan nomor urut (1-based) atau ID tab.
    pub fn switch_to(&mut self, selector: &str) -> Option<&ChatSession> {
        if let Ok(num) = selector.parse::<usize>() {
            if num >= 1 && num <= self.tabs.len() {
                self.active_index = num - 1;
                return Some(self.active());
            }
        }
        if let Some(pos) = self.tabs.iter().position(|t| t.id == selector) {
            self.active_index = pos;
            return Some(self.active());
        }
        None
    }

    /// Tutup tab berdasarkan nomor (1-based) atau ID; minimal 1 tab selalu tersisa.
    pub fn close_tab(&mut self, selector: &str, fallback_model: &str) -> bool {
        let pos = if let Ok(num) = selector.parse::<usize>() {
            (num >= 1 && num <= self.tabs.len()).then_some(num - 1)
        } else {
            self.tabs.iter().position(|t| t.id == selector)
        };
        let Some(idx) = pos else { return false };
        self.tabs.remove(idx);
        if self.tabs.is_empty() {
            self.tabs.push(ChatSession::new("1", fallback_model));
            self.active_index = 0;
        } else if self.active_index >= self.tabs.len() {
            self.active_index = self.tabs.len() - 1;
        }
        true
    }

    /// Simpan seluruh state tab ke file JSON.
    pub fn save_to_file(&self, path: &Path) -> Result<(), String> {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).map_err(|e| e.to_string())?;
        }
        let data = serde_json::to_string_pretty(self).map_err(|e| e.to_string())?;
        fs::write(path, data).map_err(|e| e.to_string())
    }

    /// Muat state tab dari file JSON bila ada.
    pub fn load_from_file(path: &Path) -> Option<Self> {
        let data = fs::read_to_string(path).ok()?;
        let loaded: Self = serde_json::from_str(&data).ok()?;
        (!loaded.tabs.is_empty()).then_some(loaded)
    }

    pub fn default_storage_path() -> Option<PathBuf> {
        let home = std::env::var("HOME").ok()?;
        Some(PathBuf::from(home).join(".local/share/oopbot/tabs.json"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tab_lifecycle_create_switch_close() {
        let mut ws = WorkspaceTabs::new("gemini-3.5-flash-lite");
        assert_eq!(ws.list().len(), 1);
        ws.active_mut().update_title_from_prompt("Cek isi folder proyek");
        assert_eq!(ws.active().title, "Cek isi folder proyek");

        ws.new_tab("gemini-2.5-pro");
        assert_eq!(ws.list().len(), 2);
        assert_eq!(ws.active().model, "gemini-2.5-pro");

        ws.switch_to("1").unwrap();
        assert_eq!(ws.active().id, "1");

        assert!(ws.close_tab("2", "gemini-3.5-flash-lite"));
        assert_eq!(ws.list().len(), 1);
    }
}
