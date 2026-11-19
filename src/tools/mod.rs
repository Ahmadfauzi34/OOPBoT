//! Tool yang bisa dipanggil agen. Setiap tool mengimplementasikan trait [`Tool`]
//! (OOP-style), lalu didaftarkan ke [`ToolRegistry`].
//!
//! - [`shell`]: [`RunShell`], [`resolve_command_shell`], [`is_dangerous_shell_command`].
//! - [`fs`]: [`ReadFile`], [`WriteFile`], [`ListDir`].

mod fs;
mod shell;

#[cfg(test)]
mod tests;

pub use fs::{ListDir, ReadFile, WriteFile};
pub use shell::{
    is_dangerous_shell_command, resolve_command_shell, resolve_command_shell_with, RunShell,
};

use async_trait::async_trait;
use serde_json::{json, Value};
use std::path::PathBuf;

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
    /// Apakah pemanggilan dengan argumen spesifik ini membutuhkan konfirmasi user.
    /// Default mendelegasikan ke `requires_confirmation()`.
    fn requires_confirmation_for_args(&self, _args: &Value) -> bool {
        self.requires_confirmation()
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

impl Default for ToolRegistry {
    fn default() -> Self {
        Self::new()
    }
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
// Helper bersama untuk implementasi tool
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

pub(super) fn str_arg<'a>(args: &'a Value, key: &str) -> Result<&'a str, String> {
    args[key]
        .as_str()
        .ok_or_else(|| format!("argumen '{}' (string) wajib diisi", key))
}
