//! Katalog & manifest penyimpanan untuk External CLI Agent Engines berbasis ACP
//! (mis. Antigravity CLI Termux) yang diunduh opsional ke `~/.oopbot/engines/<id>/`.

use crate::common::fs::{read_json_file, write_json_atomic};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

/// Protokol wire yang digunakan oleh external CLI engine.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ExternalEngineProtocol {
    Acp,
}

/// Spesifikasi bawaan untuk satu external CLI engine yang dapat diunduh ke Termux.
#[derive(Debug, Clone, Copy)]
pub struct ExternalEngineSpec {
    pub id: &'static str,
    pub display_name: &'static str,
    pub subtitle: &'static str,
    pub protocol: ExternalEngineProtocol,
    pub github_repo: &'static str,
    pub binary_rel_path: &'static str,
    pub acp_args: &'static [&'static str],
    pub login_args: &'static [&'static str],
    pub requires_termux_glibc: bool,
    pub requires_arm_lse_or_qemu: bool,
}

/// Manifest terpasang yang disimpan di `~/.oopbot/engines/<id>/manifest.json`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ExternalEngineManifest {
    pub id: String,
    pub protocol: ExternalEngineProtocol,
    pub display_name: String,
    pub source_repo: String,
    pub installed_version: String,
    pub command: String,
    pub args: Vec<String>,
    pub installed_at: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sha256: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub authenticated: Option<bool>,
}

const BUILTIN_EXTERNAL_ENGINES: &[ExternalEngineSpec] = &[ExternalEngineSpec {
    id: "antigravity",
    display_name: "Antigravity",
    subtitle: "Antigravity CLI · ACP",
    protocol: ExternalEngineProtocol::Acp,
    github_repo: "wallentx/antigravity-cli-termux",
    binary_rel_path: "agy",
    acp_args: &["acp"],
    login_args: &["acp", "--login"],
    requires_termux_glibc: true,
    requires_arm_lse_or_qemu: true,
}];

/// Daftar seluruh external CLI engine bawaan yang dikenal OOPBoT.
pub fn builtin_external_engines() -> &'static [ExternalEngineSpec] {
    BUILTIN_EXTERNAL_ENGINES
}

/// Cari spesifikasi external engine bawaan berdasarkan ID (case-insensitive).
pub fn find_builtin_external_engine(id: &str) -> Option<&'static ExternalEngineSpec> {
    let needle = id.trim();
    BUILTIN_EXTERNAL_ENGINES
        .iter()
        .find(|spec| spec.id.eq_ignore_ascii_case(needle))
}

/// Parse selector engine seperti `"acp:antigravity"` -> `Some("antigravity")`.
/// Mengembalikan `None` untuk `"native"`, string kosong, atau ID yang tidak aman untuk path.
pub fn parse_acp_engine_selector(raw: &str) -> Option<&str> {
    let trimmed = raw.trim();
    let id = trimmed.strip_prefix("acp:")?;
    if is_safe_engine_id(id) {
        Some(id)
    } else {
        None
    }
}

/// Validasi bahwa ID engine hanya berisi karakter aman untuk nama direktori (`[a-zA-Z0-9_-]`).
pub fn is_safe_engine_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 64
        && id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
}

/// Direktori instalasi engine: `<config_dir>/engines/<id>`.
pub fn engine_dir(config_dir: &Path, engine_id: &str) -> PathBuf {
    config_dir.join("engines").join(engine_id)
}

/// Path file manifest engine: `<config_dir>/engines/<id>/manifest.json`.
pub fn engine_manifest_path(config_dir: &Path, engine_id: &str) -> PathBuf {
    engine_dir(config_dir, engine_id).join("manifest.json")
}

/// Muat `manifest.json` untuk engine tertentu bila ada dan valid.
pub fn load_engine_manifest(config_dir: &Path, engine_id: &str) -> Option<ExternalEngineManifest> {
    if !is_safe_engine_id(engine_id) {
        return None;
    }
    let path = engine_manifest_path(config_dir, engine_id);
    if !path.is_file() {
        return None;
    }
    let val = read_json_file(&path);
    serde_json::from_value(val).ok()
}

/// Simpan `manifest.json` secara atomik ke `<config_dir>/engines/<id>/manifest.json`.
pub fn save_engine_manifest(
    config_dir: &Path,
    manifest: &ExternalEngineManifest,
) -> Result<(), String> {
    if !is_safe_engine_id(&manifest.id) {
        return Err(format!("ID engine tidak valid: '{}'", manifest.id));
    }
    let path = engine_manifest_path(config_dir, &manifest.id);
    let val = serde_json::to_value(manifest)
        .map_err(|e| format!("Gagal serialisasi manifest engine: {}", e))?;
    write_json_atomic(&path, &val, None).map_err(|e| format!("Gagal menyimpan manifest: {}", e))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::common::time::epoch_ms;

    #[test]
    fn builtin_antigravity_engine_spec_and_selector_parsing() {
        let spec = find_builtin_external_engine("antigravity").expect("antigravity spec");
        assert_eq!(spec.protocol, ExternalEngineProtocol::Acp);
        assert_eq!(spec.github_repo, "wallentx/antigravity-cli-termux");
        assert_eq!(spec.acp_args, &["acp"]);
        assert!(spec.requires_termux_glibc);

        assert_eq!(parse_acp_engine_selector("acp:antigravity"), Some("antigravity"));
        assert_eq!(parse_acp_engine_selector("native"), None);
        assert_eq!(parse_acp_engine_selector("acp:../escape"), None);
        assert_eq!(parse_acp_engine_selector("acp:"), None);
    }

    #[test]
    fn manifest_roundtrip_atomic_persistence() {
        let dir = std::env::temp_dir().join(format!(
            "oopbot_engine_manifest_{}_{}",
            std::process::id(),
            epoch_ms()
        ));
        let manifest = ExternalEngineManifest {
            id: "antigravity".to_string(),
            protocol: ExternalEngineProtocol::Acp,
            display_name: "Antigravity".to_string(),
            source_repo: "wallentx/antigravity-cli-termux".to_string(),
            installed_version: "1.4.2".to_string(),
            command: "/data/data/com.termux/files/home/.oopbot/engines/antigravity/bin/agy"
                .to_string(),
            args: vec!["acp".to_string()],
            installed_at: "2026-11-19T00:00:00Z".to_string(),
            sha256: Some("abc123".to_string()),
            authenticated: Some(true),
        };

        save_engine_manifest(&dir, &manifest).unwrap();
        let loaded = load_engine_manifest(&dir, "antigravity").expect("manifest loaded");
        assert_eq!(loaded, manifest);

        let _ = std::fs::remove_dir_all(&dir);
    }
}
