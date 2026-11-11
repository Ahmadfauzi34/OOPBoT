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
        false
    }
    fn requires_confirmation_for_args(&self, args: &Value) -> bool {
        let cmd = args["command"].as_str().unwrap_or("").trim();
        is_dangerous_shell_command(cmd)
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

/// Deteksi apakah suatu perintah shell berpotensi destruktif/berbahaya terhadap sistem
/// atau repositori (misalnya `rm -rf`, format disk, shutdown, git reset --hard, dsb.).
///
/// Perintah aman seperti `ls`, `cat`, `cargo test`, `git status`, `git diff`, `mkdir`
/// akan mengembalikan `false` agar agen dapat bekerja otonom tanpa mengganggu user.
pub fn is_dangerous_shell_command(cmd: &str) -> bool {
    let trimmed = cmd.trim();
    if trimmed.is_empty() {
        return false;
    }

    // Deteksi fork-bomb klasik
    let compact = trimmed.replace(' ', "");
    if compact.contains(":(){:|:&};:") || compact.contains(":(){:|:&}") {
        return true;
    }

    // Pisahkan multi-command pipeline / chaining: ;, &&, ||, |, \n, &
    let normalized = trimmed
        .replace("&&", "\n")
        .replace("||", "\n")
        .replace([';', '|', '&'], "\n");

    for raw_sub in normalized.lines() {
        let sub = raw_sub.trim();
        if sub.is_empty() {
            continue;
        }

        // Cek redirection berbahaya ke block device atau filesystem (/dev/sd*, /dev/block/*, dsb.)
        // Mengecualikan device aman umum: /dev/null, /dev/zero, /dev/full, /dev/random, /dev/urandom, /dev/tty*, /dev/pts/*, /dev/stdout, /dev/stderr
        if let Some(dev_idx) = sub.find("> /dev/") {
            let target = &sub[dev_idx + "> /dev/".len()..];
            let dev_target = target.split_whitespace().next().unwrap_or("");
            let safe_devs = ["null", "zero", "full", "random", "urandom", "stdout", "stderr"];
            let is_safe_dev = safe_devs.contains(&dev_target)
                || dev_target.starts_with("pts/")
                || dev_target.starts_with("tty");
            if !is_safe_dev {
                return true;
            }
        }

        let words: Vec<&str> = sub.split_whitespace().collect();
        if words.is_empty() {
            continue;
        }

        // Lewati env variable bindings di awal (misal FOO=bar BAZ=1 cmd ...)
        let mut idx = 0;
        while idx < words.len() && words[idx].contains('=') && !words[idx].starts_with('-') {
            idx += 1;
        }
        if idx >= words.len() {
            continue;
        }

        // Lewati command wrapper seperti sudo, doas, busybox, toybox, exec, command
        let mut base_cmd = words[idx];
        if ["sudo", "doas", "busybox", "toybox", "exec", "command", "builtin"].contains(&base_cmd) {
            idx += 1;
            if idx < words.len() {
                base_cmd = words[idx];
            } else {
                continue;
            }
        }

        let base_cmd_name = std::path::Path::new(base_cmd)
            .file_name()
            .and_then(|f| f.to_str())
            .unwrap_or(base_cmd);

        let args = &words[idx + 1..];

        match base_cmd_name {
            "rm" => {
                // Berbahaya jika ada flag rekursif: -r, -R, --recursive, -rf, -fr, dsb.
                let has_recursive = args.iter().any(|arg| {
                    if *arg == "--recursive" {
                        return true;
                    }
                    if arg.starts_with('-') && !arg.starts_with("--") {
                        let flags = &arg[1..];
                        return flags.contains('r') || flags.contains('R');
                    }
                    false
                });
                if has_recursive {
                    return true;
                }
                // Berbahaya jika target adalah root, home, atau wildcard berisiko
                let targets_root_or_all = args.iter().any(|arg| {
                    let a = arg.trim_matches(|c| c == '\'' || c == '"');
                    matches!(a, "/" | "/*" | "~" | "~/*" | "*" | ".*")
                });
                if targets_root_or_all {
                    return true;
                }
            }
            "rmdir" => return true,
            "mkfs" | "fdisk" | "parted" | "gdisk" | "sfdisk" | "wipefs" | "shred" => return true,
            cmd if cmd.starts_with("mkfs.") => return true,
            "dd" => {
                // Cek apakah menulis ke block device
                for arg in args {
                    if let Some(target) = arg.strip_prefix("of=/dev/") {
                        let safe_devs = ["null", "zero", "full", "random", "urandom", "stdout", "stderr"];
                        let is_safe = safe_devs.contains(&target)
                            || target.starts_with("pts/")
                            || target.starts_with("tty");
                        if !is_safe {
                            return true;
                        }
                    }
                }
            }
            "reboot" | "shutdown" | "poweroff" | "halt" => return true,
            "init" => {
                if args.iter().any(|&a| a == "0" || a == "6") {
                    return true;
                }
            }
            "git" => {
                // Periksa subcommand git
                if !args.is_empty() {
                    match args[0] {
                        "reset" if args.contains(&"--hard") => return true,
                        "clean" => {
                            let has_force = args.iter().any(|arg| {
                                if *arg == "--force" {
                                    return true;
                                }
                                if arg.starts_with('-') && !arg.starts_with("--") {
                                    return arg[1..].contains('f');
                                }
                                false
                            });
                            if has_force {
                                return true;
                            }
                        }
                        "push" => {
                            let has_force = args.iter().any(|arg| {
                                *arg == "--force"
                                    || *arg == "-f"
                                    || *arg == "--force-with-lease"
                                    || arg.starts_with('+')
                            });
                            if has_force {
                                return true;
                            }
                        }
                        "branch" if args.contains(&"-D") => return true,
                        "checkout" if args.iter().any(|&a| a == "-f" || a == "--force") => {
                            return true;
                        }
                        _ => {}
                    }
                }
            }
            "chmod" | "chown" => {
                let has_recursive = args.iter().any(|arg| *arg == "-R" || *arg == "--recursive");
                if has_recursive {
                    let targets_system = args.iter().any(|arg| {
                        let a = arg.trim_matches(|c| c == '\'' || c == '"');
                        matches!(
                            a,
                            "/" | "/system" | "/data" | "/vendor" | "/etc" | "/bin" | "/usr" | "~" | "$HOME"
                        )
                    });
                    if targets_system {
                        return true;
                    }
                }
            }
            "fastboot" | "flash_image" => return true,
            _ => {}
        }
    }

    false
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

    #[test]
    fn test_is_dangerous_shell_command_safe_cases() {
        let safe_cmds = [
            "ls -la",
            "cat src/main.rs",
            "cargo test --locked",
            "cargo check",
            "git status",
            "git diff",
            "git log -n 5",
            "git branch",
            "git checkout main",
            "git add .",
            "git commit -m 'feat: something'",
            "git push origin main",
            "mkdir -p src/new_dir",
            "echo 'hello world' > file.txt",
            "echo 'log' > /dev/null 2>&1",
            "python3 script.py",
            "npm run build",
            "chmod 755 run.sh",
            "dd if=/dev/zero of=test.img bs=1M count=10",
        ];

        let shell_tool = RunShell;
        for cmd in safe_cmds {
            assert!(
                !is_dangerous_shell_command(cmd),
                "Perintah seharusnya aman (otonom): {}",
                cmd
            );
            assert!(
                !shell_tool.requires_confirmation_for_args(&json!({ "command": cmd })),
                "Tool RunShell seharusnya tidak meminta konfirmasi: {}",
                cmd
            );
        }
    }

    #[test]
    fn test_is_dangerous_shell_command_dangerous_cases() {
        let dangerous_cmds = [
            "rm -rf /tmp/test",
            "rm -r target",
            "rm -rf /",
            "rm -f /",
            "rm *",
            "rmdir old_dir",
            "git reset --hard HEAD~1",
            "git clean -fd",
            "git clean -f",
            "git push --force origin main",
            "git push -f",
            "git push origin +feature",
            "git branch -D old_branch",
            "reboot",
            "shutdown -h now",
            "poweroff",
            "halt",
            "init 0",
            "init 6",
            "mkfs.ext4 /dev/sdb1",
            "fdisk /dev/sda",
            "parted /dev/sda",
            "wipefs -a /dev/sda",
            "shred -u secret.txt",
            "dd if=/dev/zero of=/dev/sda",
            ":(){ :|:& };:",
            "chmod -R 777 /",
            "chmod -R 777 /system",
            "chown -R root /",
            "fastboot flash boot boot.img",
            "echo test > /dev/sda",
            "cargo build && rm -rf /tmp/build",
        ];

        let shell_tool = RunShell;
        for cmd in dangerous_cmds {
            assert!(
                is_dangerous_shell_command(cmd),
                "Perintah seharusnya berbahaya (minta izin): {}",
                cmd
            );
            assert!(
                shell_tool.requires_confirmation_for_args(&json!({ "command": cmd })),
                "Tool RunShell seharusnya meminta konfirmasi: {}",
                cmd
            );
        }
    }
}
