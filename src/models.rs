//! Katalog Model & Provider (`ModelCatalog`) ala fitur Model Picker di Pipkin.
//!
//! Menyediakan daftar model siap pilih, mendukung penambahan model kustom,
//! serta pemilihan model aktif per sesi/tab.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ModelInfo {
    /// ID model API (misal: `gemini-3.5-flash-lite`, `gemini-2.5-pro`).
    pub id: String,
    /// Nama tampilan ramah UI.
    pub label: String,
    /// Nama provider (misal: `gemini`).
    pub provider: String,
    /// Apakah mendukung tool/function calling.
    pub supports_tools: bool,
    /// Deskripsi singkat untuk pemilih model di UI/CLI.
    pub description: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModelCatalog {
    models: Vec<ModelInfo>,
    default_model_id: String,
}

impl Default for ModelCatalog {
    fn default() -> Self {
        Self::builtin()
    }
}

impl ModelCatalog {
    /// Katalog model bawaan Gemini yang bisa langsung dipilih di UI/CLI.
    pub fn builtin() -> Self {
        let models = vec![
            ModelInfo {
                id: "gemini-3.5-flash-lite".into(),
                label: "Gemini 3.5 Flash Lite".into(),
                provider: "gemini".into(),
                supports_tools: true,
                description: "Cepat & hemat kuota, cocok untuk tugas harian di Termux".into(),
            },
            ModelInfo {
                id: "gemini-2.5-flash".into(),
                label: "Gemini 2.5 Flash".into(),
                provider: "gemini".into(),
                supports_tools: true,
                description: "Keseimbangan kecepatan & penalaran multi-langkah".into(),
            },
            ModelInfo {
                id: "gemini-2.5-pro".into(),
                label: "Gemini 2.5 Pro".into(),
                provider: "gemini".into(),
                supports_tools: true,
                description: "Penalaran mendalam untuk coding & debugging kompleks".into(),
            },
        ];
        Self {
            models,
            default_model_id: "gemini-3.5-flash-lite".into(),
        }
    }

    pub fn list(&self) -> &[ModelInfo] {
        &self.models
    }

    pub fn default_model(&self) -> &str {
        &self.default_model_id
    }

    pub fn set_default_model(&mut self, model_id: &str) {
        if self.find(model_id).is_some() {
            self.default_model_id = model_id.to_string();
        } else {
            self.register_custom(model_id, model_id);
            self.default_model_id = model_id.to_string();
        }
    }

    pub fn find(&self, query: &str) -> Option<&ModelInfo> {
        // Bisa pilih berdasarkan nomor urut (1, 2, 3...) atau ID model
        if let Ok(idx) = query.parse::<usize>() {
            if idx >= 1 && idx <= self.models.len() {
                return Some(&self.models[idx - 1]);
            }
        }
        self.models.iter().find(|m| m.id.eq_ignore_ascii_case(query))
    }

    pub fn register_custom(&mut self, id: &str, label: &str) {
        if self.models.iter().any(|m| m.id == id) {
            return;
        }
        self.models.push(ModelInfo {
            id: id.to_string(),
            label: label.to_string(),
            provider: "gemini".into(),
            supports_tools: true,
            description: "Model kustom".into(),
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lookup_by_index_and_id() {
        let mut cat = ModelCatalog::builtin();
        assert_eq!(cat.find("1").unwrap().id, "gemini-3.5-flash-lite");
        assert_eq!(cat.find("gemini-2.5-pro").unwrap().label, "Gemini 2.5 Pro");
        cat.set_default_model("gemini-experimental-custom");
        assert_eq!(cat.default_model(), "gemini-experimental-custom");
        assert!(cat.find("gemini-experimental-custom").is_some());
    }
}
