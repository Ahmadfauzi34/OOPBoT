//! Manajemen Multi-Sesi & Tab (`WorkspaceTabs`) OOPBoT.
//!
//! Setiap tab/sesi menyimpan:
//! - ID sesi (`tab-1`, `tab-2`, dst. atau nama kustom)
//! - Judul tab (otomatis dari pesan pertama atau diberi nama)
//! - Model yang dipilih khusus untuk tab tersebut
//! - Riwayat `contents` mentah untuk API LLM
//! - Riwayat `TimelineEntry` terlipat (`StepsFold`) untuk dirender UI

use super::events::TimelineEntry;
use crate::common::fs::write_text_atomic;
use crate::common::time::epoch_ms;
use crate::context::ContextStore;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChatSession {
    pub id: String,
    pub title: String,
    pub model: String,
    /// Endpoint/provider `base_url` tempat `model` tab ini terakhir dipilih,
    /// dipakai mendeteksi ketidaksesuaian provider saat tab dipulihkan pada sesi CLI berikutnya.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider_base_url: Option<String>,
    pub history: Vec<Value>,
    pub timeline: Vec<TimelineEntry>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_context_tokens: Option<u64>,
    /// Estimasi token lokal pada saat `last_context_tokens` diobservasi dari provider,
    /// dipakai menghitung delta pesan baru agar `max(estimasi, observasi)` tidak melewatkan pertumbuhan konteks.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_context_estimated_tokens: Option<u64>,
    /// Fingerprint `(model, base_url, system_instruction, tool_declarations)` tempat
    /// `last_context_tokens` diobservasi agar perubahan `--append-system-prompt`, model,
    /// provider, atau set tool tidak memakai angka token basi.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_context_fingerprint: Option<String>,
}

impl ChatSession {
    pub fn new(id: &str, model: &str) -> Self {
        Self {
            id: id.to_string(),
            title: format!("Tab {}", id),
            model: model.to_string(),
            provider_base_url: None,
            history: Vec::new(),
            timeline: Vec::new(),
            last_context_tokens: None,
            last_context_estimated_tokens: None,
            last_context_fingerprint: None,
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

    pub fn clear_observed_context_tokens(&mut self) {
        self.last_context_tokens = None;
        self.last_context_estimated_tokens = None;
        self.last_context_fingerprint = None;
    }

    pub fn set_observed_context_tokens(&mut self, tokens: Option<u64>, fingerprint: Option<String>) {
        self.set_observed_context_tokens_with_baseline(tokens, None, fingerprint);
    }

    pub fn set_observed_context_tokens_with_baseline(
        &mut self,
        tokens: Option<u64>,
        estimated_baseline: Option<u64>,
        fingerprint: Option<String>,
    ) {
        self.last_context_tokens = tokens;
        self.last_context_estimated_tokens = tokens.and(estimated_baseline);
        self.last_context_fingerprint = tokens.and(fingerprint);
    }

    pub fn clear(&mut self) {
        self.history.clear();
        self.timeline.clear();
        self.clear_observed_context_tokens();
    }

    /// Perbarui `history` setelah compaction tanpa menghapus `timeline` visual user.
    pub fn apply_compacted_history(&mut self, new_history: Vec<Value>, notice: String) {
        self.history = new_history;
        self.clear_observed_context_tokens();
        self.timeline.push(TimelineEntry::Notice { text: notice });
    }
}

/// Adapter [`ContextStore`] untuk persistensi `tabs.json` CLI secara atomik (`0o600`).
pub struct TabsFileStore {
    path: PathBuf,
}

impl TabsFileStore {
    pub fn new(path: PathBuf) -> Self {
        Self { path }
    }
}

impl ContextStore for TabsFileStore {
    fn save_serialized(&self, content: &str) -> Result<(), String> {
        write_text_atomic(&self.path, content, Some(0o600)).map_err(|e| e.to_string())
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

    pub fn active(&self) -> &ChatSession {
        &self.tabs[self.active_index]
    }

    pub fn active_mut(&mut self) -> &mut ChatSession {
        &mut self.tabs[self.active_index]
    }

    pub fn list(&self) -> &[ChatSession] {
        &self.tabs
    }

    pub fn list_mut(&mut self) -> &mut [ChatSession] {
        &mut self.tabs
    }

    pub fn active_index(&self) -> usize {
        self.active_index
    }

    /// Buat tab baru dengan model tertentu dan langsung pindah ke tab tersebut.
    pub fn new_tab(&mut self, model: &str) -> &ChatSession {
        let mut candidate = self.next_id.max(1);
        while self.tabs.iter().any(|t| t.id == candidate.to_string()) {
            candidate = if candidate == usize::MAX {
                1
            } else {
                candidate + 1
            };
        }
        let id = candidate.to_string();
        self.next_id = if candidate == usize::MAX {
            1
        } else {
            candidate + 1
        };
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

    /// Simpan seluruh state tab ke file JSON secara atomik (`0o600` di Unix).
    pub fn save_to_file(&self, path: &Path) -> Result<(), String> {
        let data = serde_json::to_string_pretty(self).map_err(|e| e.to_string())?;
        TabsFileStore::new(path.to_path_buf()).save_serialized(&format!("{}\n", data))
    }

    fn sanitize(&mut self) {
        if self.tabs.is_empty() {
            self.active_index = 0;
            self.next_id = self.next_id.max(1);
            return;
        }
        self.active_index = self.active_index.min(self.tabs.len() - 1);

        // Hitung kandidat ID numerik awal yang aman (tidak overflow di `usize::MAX`).
        let max_numeric = self
            .tabs
            .iter()
            .filter_map(|t| t.id.parse::<usize>().ok())
            .filter(|&n| n < usize::MAX)
            .max()
            .unwrap_or(0);
        let mut candidate = self.next_id.max(max_numeric.saturating_add(1)).max(1);
        let mut seen: HashSet<String> = HashSet::with_capacity(self.tabs.len());

        // Kumpulkan terlebih dahulu semua ID unik valid agar tab duplikat/kosong tidak menabrak
        // ID valid yang muncul di indeks lebih akhir.
        let mut needs_reassign = vec![false; self.tabs.len()];
        for (idx, tab) in self.tabs.iter().enumerate() {
            if tab.id.trim().is_empty() || !seen.insert(tab.id.clone()) {
                needs_reassign[idx] = true;
            }
        }
        for (idx, tab) in self.tabs.iter_mut().enumerate() {
            if needs_reassign[idx] {
                while seen.contains(&candidate.to_string()) {
                    candidate = if candidate == usize::MAX {
                        1
                    } else {
                        candidate + 1
                    };
                }
                tab.id = candidate.to_string();
                seen.insert(tab.id.clone());
                candidate = if candidate == usize::MAX {
                    1
                } else {
                    candidate + 1
                };
            }
        }

        let final_max = self
            .tabs
            .iter()
            .filter_map(|t| t.id.parse::<usize>().ok())
            .filter(|&n| n < usize::MAX)
            .max()
            .unwrap_or(0);
        let min_next = final_max.saturating_add(1).max(1);
        if self.next_id < min_next {
            self.next_id = min_next;
        }
    }

    /// Coba parse `tabs.json` secara toleran per-entri saat deserialize penuh gagal
    /// (mis. satu entri tab rusak atau metadata top-level rusak, tetapi tab lain masih utuh).
    /// Mengembalikan `(WorkspaceTabs, jumlah_tab_dilewati)` bila minimal 1 tab sehat berhasil dipulihkan.
    fn parse_tolerant_from_str(data: &str) -> Option<(Self, usize)> {
        let root: Value = serde_json::from_str(data).ok()?;
        let obj = root.as_object()?;
        let tabs_arr = obj.get("tabs")?.as_array()?;

        let raw_active_index = obj
            .get("active_index")
            .and_then(Value::as_u64)
            .and_then(|v| usize::try_from(v).ok());
        let raw_next_id = obj
            .get("next_id")
            .and_then(Value::as_u64)
            .and_then(|v| usize::try_from(v).ok())
            .unwrap_or(1);

        let mut valid_tabs = Vec::with_capacity(tabs_arr.len());
        let mut skipped_tabs = 0usize;
        let mut mapped_active: Option<usize> = None;
        let mut valid_before_active = 0usize;

        for (orig_idx, item) in tabs_arr.iter().enumerate() {
            match serde_json::from_value::<ChatSession>(item.clone()) {
                Ok(tab) => {
                    let new_idx = valid_tabs.len();
                    if raw_active_index == Some(orig_idx) {
                        mapped_active = Some(new_idx);
                    }
                    if raw_active_index.is_some_and(|a| orig_idx < a) {
                        valid_before_active += 1;
                    }
                    valid_tabs.push(tab);
                }
                Err(_) => {
                    skipped_tabs += 1;
                }
            }
        }

        if valid_tabs.is_empty() {
            return None;
        }

        let active_index = match mapped_active {
            Some(idx) => idx,
            None => match raw_active_index {
                Some(_) => valid_before_active.min(valid_tabs.len() - 1),
                None => 0,
            },
        };

        let mut recovered = Self {
            tabs: valid_tabs,
            active_index,
            next_id: raw_next_id,
        };
        recovered.sanitize();
        Some((recovered, skipped_tabs))
    }

    /// Muat state tab dari file JSON dengan sanitasi `active_index`/`next_id`/ID duplikat,
    /// pemulihan toleran per-entri saat korupsi parsial, dan karantina backup `*.corrupt-<ts>.bak`
    /// anti-tabrakan agar data tidak pernah tertimpa diam-diam.
    pub fn load_from_file_checked(path: &Path) -> (Option<Self>, Option<String>) {
        if !path.exists() {
            return (None, None);
        }
        let raw_data = fs::read_to_string(path).ok();
        if let Some(ref data) = raw_data {
            if let Ok(mut loaded) = serde_json::from_str::<Self>(data) {
                if !loaded.tabs.is_empty() {
                    loaded.sanitize();
                    return (Some(loaded), None);
                }
            }

            // Korupsi parsial: pulihkan seluruh tab yang masih sehat, simpan salinan asli
            // ke backup anti-tabrakan, dan tulis ulang `tabs.json` bersih (0o600).
            if let Some((recovered, skipped_tabs)) = Self::parse_tolerant_from_str(data) {
                let recovered_count = recovered.tabs.len();
                let backup_path = allocate_corrupt_backup_path(path);
                let detail = if skipped_tabs > 0 {
                    format!(
                        "{} tab rusak dilewati, {} tab sehat dipulihkan",
                        skipped_tabs, recovered_count
                    )
                } else {
                    format!(
                        "metadata sesi rusak, {} tab sehat dipulihkan",
                        recovered_count
                    )
                };
                let warning = match fs::rename(path, &backup_path) {
                    Ok(()) => match recovered.save_to_file(path) {
                        Ok(()) => format!(
                            "File sesi {} sebagian rusak ({}); salinan asli disimpan di {}.",
                            path.display(),
                            detail,
                            backup_path.display()
                        ),
                        Err(save_err) => format!(
                            "File sesi {} sebagian rusak ({}); salinan asli dipindahkan ke {}, namun gagal menyimpan file hasil pemulihan ({}).",
                            path.display(),
                            detail,
                            backup_path.display(),
                            save_err
                        ),
                    },
                    Err(e) => format!(
                        "File sesi {} sebagian rusak ({}), namun gagal membuat backup ({}).",
                        path.display(),
                        detail,
                        e
                    ),
                };
                return (Some(recovered), Some(warning));
            }
        }

        // File ada tetapi tidak bisa dibaca / tidak valid total / kosong: pindahkan ke backup
        // sebelum sesi baru menimpanya.
        let backup_path = allocate_corrupt_backup_path(path);
        let warning = match fs::rename(path, &backup_path) {
            Ok(()) => format!(
                "File sesi {} rusak; dipindahkan ke {} dan memulai tab baru.",
                path.display(),
                backup_path.display()
            ),
            Err(e) => format!(
                "File sesi {} rusak dan gagal di-backup ({}); memulai tab baru.",
                path.display(),
                e
            ),
        };
        (None, Some(warning))
    }

    /// Muat state tab dari file JSON bila ada.
    pub fn load_from_file(path: &Path) -> Option<Self> {
        Self::load_from_file_checked(path).0
    }

    pub fn default_storage_path() -> Option<PathBuf> {
        let home = std::env::var("HOME").ok()?;
        Some(PathBuf::from(home).join(".local/share/oopbot/tabs.json"))
    }
}

fn allocate_corrupt_backup_path(path: &Path) -> PathBuf {
    let file_name = path
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("tabs.json");
    let ts = epoch_ms();
    let base = path.with_file_name(format!("{}.corrupt-{}.bak", file_name, ts));
    if !base.exists() {
        return base;
    }
    for suffix in 1u32..=10_000 {
        let candidate = path.with_file_name(format!("{}.corrupt-{}-{}.bak", file_name, ts, suffix));
        if !candidate.exists() {
            return candidate;
        }
    }
    base
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

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

    #[test]
    fn partial_corruption_recovers_healthy_tabs_remaps_active_index_and_backs_up_original() {
        let dir = std::env::temp_dir().join(format!(
            "oopbot_partial_tabs_{}_{}",
            std::process::id(),
            epoch_ms()
        ));
        fs::create_dir_all(&dir).unwrap();
        let tabs_path = dir.join("tabs.json");

        // Tab index 0 sehat ("1"), index 1 rusak, index 2 sehat ("3", aktif!) ->
        // pemulihan harus mempertahankan 2 tab sehat dan me-remap active_index 2 -> 1 (tetap tab "3").
        let partial_json = json!({
            "tabs": [
                {
                    "id": "1",
                    "title": "Tab Sehat A",
                    "model": "gpt-4o-mini",
                    "history": [{ "role": "user", "parts": [{ "text": "halo A" }] }],
                    "timeline": []
                },
                {
                    "id": 999,
                    "title": "Tab Rusak",
                    "history": "bukan-array"
                },
                {
                    "id": "3",
                    "title": "Tab Sehat B",
                    "model": "gpt-4.1",
                    "history": [{ "role": "user", "parts": [{ "text": "halo B" }] }],
                    "timeline": []
                }
            ],
            "active_index": 2,
            "next_id": 2
        });
        fs::write(&tabs_path, partial_json.to_string()).unwrap();

        let (loaded, warn) = WorkspaceTabs::load_from_file_checked(&tabs_path);
        let ws = loaded.expect("2 healthy tabs must be recovered from partial corruption");
        let warning = warn.expect("partial corruption must surface a recovery warning");
        assert!(warning.contains("1 tab rusak dilewati, 2 tab sehat dipulihkan"));
        assert!(warning.contains(".corrupt-"));

        assert_eq!(ws.list().len(), 2);
        assert_eq!(ws.list()[0].id, "1");
        assert_eq!(ws.list()[1].id, "3");
        assert_eq!(ws.active_index(), 1, "active tab '3' must remain selected after remap");
        assert_eq!(ws.active().title, "Tab Sehat B");

        // File asli sudah di-backup ke *.corrupt-*.bak dan tabs.json sudah ditulis ulang bersih.
        let backups: Vec<_> = fs::read_dir(&dir)
            .unwrap()
            .filter_map(|e| e.ok())
            .filter(|e| e.file_name().to_string_lossy().contains(".corrupt-"))
            .collect();
        assert_eq!(backups.len(), 1);
        let backup_content = fs::read_to_string(backups[0].path()).unwrap();
        assert!(backup_content.contains("Tab Rusak"));

        // Muat ulang tabs.json hasil pemulihan harus bersih tanpa warning & tanpa backup baru.
        let (reloaded, warn2) = WorkspaceTabs::load_from_file_checked(&tabs_path);
        assert!(warn2.is_none());
        let ws2 = reloaded.expect("rewritten tabs.json must load cleanly");
        assert_eq!(ws2.list().len(), 2);
        assert_eq!(ws2.active().id, "3");

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn partial_corruption_handles_corrupt_active_tab_and_metadata_only_corruption() {
        let dir = std::env::temp_dir().join(format!(
            "oopbot_partial_meta_{}_{}",
            std::process::id(),
            epoch_ms()
        ));
        fs::create_dir_all(&dir).unwrap();
        let tabs_path = dir.join("tabs.json");

        // 1. Tab aktif sendiri yang rusak (index 0 rusak, active_index = 0) + tab 1 & 2 sehat.
        let corrupt_active = json!({
            "tabs": [
                null,
                { "id": "10", "title": "Pertama Sehat", "model": "m1", "history": [], "timeline": [] },
                { "id": "11", "title": "Kedua Sehat", "model": "m2", "history": [], "timeline": [] }
            ],
            "active_index": 0,
            "next_id": 12
        });
        fs::write(&tabs_path, corrupt_active.to_string()).unwrap();
        let (loaded, warn) = WorkspaceTabs::load_from_file_checked(&tabs_path);
        let ws = loaded.expect("healthy tabs recovered when active tab was corrupt");
        assert!(warn.is_some());
        assert_eq!(ws.active_index(), 0);
        assert_eq!(ws.active().id, "10");

        // 2. Hanya metadata top-level (`active_index`) yang rusak, semua tab sehat -> pulihkan semua tab.
        let bad_meta = json!({
            "tabs": [
                { "id": "1", "title": "Utuh A", "model": "m1", "history": [], "timeline": [] },
                { "id": "2", "title": "Utuh B", "model": "m2", "history": [], "timeline": [] }
            ],
            "active_index": "bukan-angka",
            "next_id": -5
        });
        fs::write(&tabs_path, bad_meta.to_string()).unwrap();
        let (loaded2, warn2) = WorkspaceTabs::load_from_file_checked(&tabs_path);
        let ws2 = loaded2.expect("valid tabs recovered when top-level metadata is corrupt");
        let w2 = warn2.expect("metadata corruption must warn");
        assert!(w2.contains("metadata sesi rusak, 2 tab sehat dipulihkan"));
        assert_eq!(ws2.list().len(), 2);
        assert_eq!(ws2.active_index(), 0);

        // 3. Semua tab rusak -> fallback ke karantina penuh `(None, Some(warning))`.
        let all_bad = json!({
            "tabs": [null, { "broken": true }],
            "active_index": 0,
            "next_id": 1
        });
        fs::write(&tabs_path, all_bad.to_string()).unwrap();
        let (loaded3, warn3) = WorkspaceTabs::load_from_file_checked(&tabs_path);
        assert!(loaded3.is_none());
        assert!(warn3.expect("all-bad tabs must warn").contains("memulai tab baru"));

        // Backup anti-tabrakan: ketiga pemulihan menghasilkan 3 file .corrupt-*.bak berbeda.
        let backups: Vec<_> = fs::read_dir(&dir)
            .unwrap()
            .filter_map(|e| e.ok())
            .filter(|e| e.file_name().to_string_lossy().contains(".corrupt-"))
            .collect();
        assert_eq!(backups.len(), 3, "rapid corrupt backups must never overwrite each other");

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn sanitize_deduplicates_tab_ids_and_avoids_usize_max_overflow() {
        let dir = std::env::temp_dir().join(format!(
            "oopbot_dedup_tabs_{}_{}",
            std::process::id(),
            epoch_ms()
        ));
        fs::create_dir_all(&dir).unwrap();
        let tabs_path = dir.join("tabs.json");

        let dup_json = json!({
            "tabs": [
                { "id": "1", "title": "First", "model": "m1", "history": [], "timeline": [] },
                { "id": "1", "title": "Dup", "model": "m2", "history": [], "timeline": [] },
                { "id": "", "title": "EmptyId", "model": "m3", "history": [], "timeline": [] },
                { "id": usize::MAX.to_string(), "title": "MaxId", "model": "m4", "history": [], "timeline": [] }
            ],
            "active_index": 1,
            "next_id": usize::MAX
        });
        fs::write(&tabs_path, dup_json.to_string()).unwrap();

        let (loaded, warn) = WorkspaceTabs::load_from_file_checked(&tabs_path);
        assert!(warn.is_none());
        let mut ws = loaded.expect("valid tabs loaded");
        let ids: HashSet<String> = ws.list().iter().map(|t| t.id.clone()).collect();
        assert_eq!(ids.len(), 4, "all tab IDs must be unique after sanitization");
        assert!(ids.iter().all(|id| !id.trim().is_empty()));

        // new_tab tidak boleh panic meski next_id berada di usize::MAX.
        let created = ws.new_tab("m5");
        assert!(!ids.contains(&created.id));

        let _ = fs::remove_dir_all(&dir);
    }
}
