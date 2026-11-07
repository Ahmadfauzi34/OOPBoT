//! Tool yang bisa dipanggil agen. Setiap tool mengimplementasikan trait [`Tool`]
//! (OOP-style), lalu didaftarkan ke [`ToolRegistry`].

use async_trait::async_trait;
use serde_json::{json, Value};
use std::path::PathBuf;
use std::process::Stdio;
use std::time::Duration;

/// Batas output yang dikembalikan ke model (byte), supaya konteks tidak meledak.
const MAX_OUTPUT_BYTES: usize = 20_000;
const MAX_READ_BYTES: usize = 100_000;
const MAX_DIR_ENTRIES: usize = 500;

#[async_trait]
pub trait Tool: Send + Sync {
    fn name(&self) -> &'static str;
    fn description(&self) -> &'static str;
    /// JSON Schema (subset OpenAPI) untuk argumen tool.
    fn parameters(&self) -> Value;
    /// Tool yang mengubah sistem (shell, tulis file) harus dikonfirmasi user.
    fn requires_confirmation(&self) -> bool {
        false
    }
    /// Ringkasan singkat yang ditampilkan ke user sebelum/saat eksekusi.
    fn preview(&self, args: &Value) -> String {
        args.to_string()
    }
    async fn execute(&self, args: &Value) -> Result<Value, String>;

    fn declaration(&self) -> Value {
        json!({
            "name": self.name(),
            "description": self.description(),
            "parameters": self.parameters(),
        })
    }
}

pub struct ToolRegistry {
    tools: Vec<Box<dyn Tool>>,
}

impl ToolRegistry {
    pub fn new() -> Self {
        Self { tools: Vec::new() }
    }

    /// Registry bawaan: shell + operasi file dasar.
    pub fn with_defaults() -> Self {
        let mut r = Self::new();
        r.register(Box::new(RunShell));
        r.register(Box::new(ReadFile));
        r.register(Box::new(WriteFile));
        r.register(Box::new(ListDir));
        r
    }

    pub fn register(&mut self, tool: Box<dyn Tool>) {
        self.tools.push(tool);
    }

    pub fn get(&self, name: &str) -> Option<&dyn Tool> {
        self.tools.iter().find(|t| t.name() == name).map(|t| t.as_ref())
    }

    pub fn declarations(&self) -> Vec<Value> {
        self.tools.iter().map(|t| t.declaration()).collect()
    }

    pub fn names(&self) -> Vec<&'static str> {
        self.tools.iter().map(|t| t.name()).collect()
    }
}

// ---------------------------------------------------------------------------
// Helper
// ---------------------------------------------------------------------------

/// Potong string di batas karakter UTF-8 yang valid.
pub fn truncate(s: &str, max: usize) -> String {
    if s.len() <= max {
        return s.to_string();
    }
    let mut end = max;
    while !s.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}\n...[terpotong, total {} byte]", &s[..end], s.len())
}

/// Ekspansi `~` ke $HOME (berguna di Termux).
pub fn expand_path(p: &str) -> PathBuf {
    if p == "~" || p.starts_with("~/") {
        if let Ok(home) = std::env::var("HOME") {
            return PathBuf::from(home).join(p.trim_start_matches('~').trim_start_matches('/'));
        }
    }
    PathBuf::from(p)
}

fn str_arg<'a>(args: &'a Value, key: &str) -> Result<&'a str, String> {
    args[key]
        .as_str()
        .ok_or_else(|| format!("argumen '{}' (string) wajib diisi", key))
}

// ---------------------------------------------------------------------------
// run_shell
// ---------------------------------------------------------------------------

pub struct RunShell;

#[async_trait]
impl Tool for RunShell {
    fn name(&self) -> &'static str {
        "run_shell"
    }
    fn description(&self) -> &'static str {
        "Jalankan perintah shell (sh -c) di perangkat user (Termux/Android) pada direktori kerja saat ini. \
         Mengembalikan exit_code, stdout, dan stderr. Paket Termux bisa dipasang dengan `pkg install -y <nama>`."
    }
    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "command": { "type": "string", "description": "Perintah shell yang akan dijalankan." },
                "timeout_secs": { "type": "integer", "description": "Batas waktu detik (default 60, maks 600)." }
            },
            "required": ["command"]
        })
    }
    fn requires_confirmation(&self) -> bool {
        true
    }
    fn preview(&self, args: &Value) -> String {
        format!("$ {}", args["command"].as_str().unwrap_or("<kosong>"))
    }

    async fn execute(&self, args: &Value) -> Result<Value, String> {
        let command = str_arg(args, "command")?;
        let timeout = args["timeout_secs"].as_u64().unwrap_or(60).clamp(1, 600);

        // "sh" dicari via PATH: di Termux ini $PREFIX/bin/sh, bukan /bin/sh.
        let child = tokio::process::Command::new("sh")
            .arg("-c")
            .arg(command)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .map_err(|e| format!("gagal menjalankan sh: {}", e))?;

        match tokio::time::timeout(Duration::from_secs(timeout), child.wait_with_output()).await {
            Err(_) => Err(format!("timeout setelah {} detik", timeout)),
            Ok(Err(e)) => Err(format!("gagal menunggu proses: {}", e)),
            Ok(Ok(out)) => Ok(json!({
                "exit_code": out.status.code(),
                "stdout": truncate(&String::from_utf8_lossy(&out.stdout), MAX_OUTPUT_BYTES),
                "stderr": truncate(&String::from_utf8_lossy(&out.stderr), MAX_OUTPUT_BYTES),
            })),
        }
    }
}

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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn truncate_respects_char_boundary() {
        let s = "ééééé"; // 2 byte per char
        let t = truncate(s, 3);
        assert!(t.starts_with("é"));
        assert!(t.contains("terpotong"));
    }

    #[tokio::test]
    async fn shell_write_read_list_roundtrip() {
        let dir = std::env::temp_dir().join(format!("oopbot-test-{}", std::process::id()));
        let file = dir.join("sub/hello.txt");
        let file_s = file.to_string_lossy().to_string();

        let w = WriteFile.execute(&json!({ "path": file_s, "content": "halo" })).await.unwrap();
        assert_eq!(w["bytes_written"], 4);

        let r = ReadFile.execute(&json!({ "path": file_s })).await.unwrap();
        assert_eq!(r["content"], "halo");

        let l = ListDir
            .execute(&json!({ "path": dir.to_string_lossy() }))
            .await
            .unwrap();
        assert_eq!(l["entries"][0]["name"], "sub");
        assert_eq!(l["entries"][0]["type"], "dir");

        let s = RunShell
            .execute(&json!({ "command": format!("cat '{}'; echo err >&2; exit 3", file_s) }))
            .await
            .unwrap();
        assert_eq!(s["exit_code"], 3);
        assert_eq!(s["stdout"], "halo");
        assert_eq!(s["stderr"], "err\n");

        let _ = std::fs::remove_dir_all(dir);
    }

    #[tokio::test]
    async fn shell_timeout() {
        let err = RunShell
            .execute(&json!({ "command": "sleep 5", "timeout_secs": 1 }))
            .await
            .unwrap_err();
        assert!(err.contains("timeout"));
    }
}
