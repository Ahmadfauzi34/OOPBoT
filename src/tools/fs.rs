//! Built-in filesystem tools: `read_file`, `write_file`, dan `list_dir`.

use super::{expand_path, str_arg, truncate, Tool};
use async_trait::async_trait;
use serde_json::{json, Value};

const MAX_READ_BYTES: usize = 100_000;
const MAX_DIR_ENTRIES: usize = 500;

// ---------------------------------------------------------------------------
// read_file
// ---------------------------------------------------------------------------

pub struct ReadFile;

#[async_trait]
impl Tool for ReadFile {
    fn name(&self) -> &'static str {
        "read_file"
    }
    fn description(&self) -> &'static str {
        "Baca isi file teks. Path relatif terhadap direktori kerja; `~` = HOME."
    }
    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": { "path": { "type": "string", "description": "Path file." } },
            "required": ["path"]
        })
    }
    fn preview(&self, args: &Value) -> String {
        args["path"].as_str().unwrap_or("?").to_string()
    }

    async fn execute(&self, args: &Value) -> Result<Value, String> {
        let path = expand_path(str_arg(args, "path")?);
        let bytes = tokio::fs::read(&path)
            .await
            .map_err(|e| format!("gagal membaca {}: {}", path.display(), e))?;
        let text = String::from_utf8_lossy(&bytes);
        Ok(json!({ "path": path.display().to_string(), "content": truncate(&text, MAX_READ_BYTES) }))
    }
}

// ---------------------------------------------------------------------------
// write_file
// ---------------------------------------------------------------------------

pub struct WriteFile;

#[async_trait]
impl Tool for WriteFile {
    fn name(&self) -> &'static str {
        "write_file"
    }
    fn description(&self) -> &'static str {
        "Tulis (buat/timpa) file teks dengan isi lengkap. Direktori induk dibuat otomatis."
    }
    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "path": { "type": "string", "description": "Path file tujuan." },
                "content": { "type": "string", "description": "Isi lengkap file." }
            },
            "required": ["path", "content"]
        })
    }
    fn requires_confirmation(&self) -> bool {
        true
    }
    fn preview(&self, args: &Value) -> String {
        let content = args["content"].as_str().unwrap_or("");
        format!(
            "tulis {} ({} byte, {} baris)",
            args["path"].as_str().unwrap_or("?"),
            content.len(),
            content.lines().count()
        )
    }

    async fn execute(&self, args: &Value) -> Result<Value, String> {
        let path = expand_path(str_arg(args, "path")?);
        let content = str_arg(args, "content")?;
        if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
            tokio::fs::create_dir_all(parent)
                .await
                .map_err(|e| format!("gagal membuat direktori {}: {}", parent.display(), e))?;
        }
        tokio::fs::write(&path, content)
            .await
            .map_err(|e| format!("gagal menulis {}: {}", path.display(), e))?;
        Ok(json!({ "path": path.display().to_string(), "bytes_written": content.len() }))
    }
}

// ---------------------------------------------------------------------------
// list_dir
// ---------------------------------------------------------------------------

pub struct ListDir;

#[async_trait]
impl Tool for ListDir {
    fn name(&self) -> &'static str {
        "list_dir"
    }
    fn description(&self) -> &'static str {
        "Daftar isi direktori (nama, tipe, ukuran). Default direktori kerja saat ini."
    }
    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": { "path": { "type": "string", "description": "Path direktori (default \".\")." } }
        })
    }
    fn preview(&self, args: &Value) -> String {
        args["path"].as_str().unwrap_or(".").to_string()
    }

    async fn execute(&self, args: &Value) -> Result<Value, String> {
        let path = expand_path(args["path"].as_str().unwrap_or("."));
        let mut rd = tokio::fs::read_dir(&path)
            .await
            .map_err(|e| format!("gagal membuka {}: {}", path.display(), e))?;

        let mut entries = Vec::new();
        let mut truncated = false;
        while let Some(entry) = rd.next_entry().await.map_err(|e| e.to_string())? {
            if entries.len() >= MAX_DIR_ENTRIES {
                truncated = true;
                break;
            }
            let meta = entry.metadata().await.ok();
            let kind = match &meta {
                Some(m) if m.is_dir() => "dir",
                Some(m) if m.is_symlink() => "symlink",
                Some(_) => "file",
                None => "unknown",
            };
            entries.push(json!({
                "name": entry.file_name().to_string_lossy(),
                "type": kind,
                "size": meta.map(|m| m.len()),
            }));
        }
        entries.sort_by(|a, b| a["name"].as_str().cmp(&b["name"].as_str()));
        Ok(json!({ "path": path.display().to_string(), "entries": entries, "truncated": truncated }))
    }
}
