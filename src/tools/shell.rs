//! Built-in tool `run_shell`, resolusi shell bash/sh (Termux & Linux), serta deteksi perintah destruktif.

use super::{str_arg, truncate, Tool};
use async_trait::async_trait;
use serde_json::{json, Value};
use std::process::Stdio;
use std::time::Duration;

/// Batas output yang dikembalikan ke model (byte), supaya konteks tidak meledak.
const MAX_OUTPUT_BYTES: usize = 20_000;

const TERMUX_DEFAULT_PREFIX: &str = "/data/data/com.termux/files/usr";
pub(super) const TERMUX_DEFAULT_BASH: &str = "/data/data/com.termux/files/usr/bin/bash";

/// Tentukan binary shell untuk mengeksekusi perintah agen dengan preferensi `bash`
/// (`/data/data/com.termux/files/usr/bin/bash` atau `$PREFIX/bin/bash` di Termux, `/bin/bash`
/// di Linux standar) agar fitur seperti `time`, `history`, `[[ ... ]]`, `set -o pipefail`,
/// array, dan brace expansion bekerja; fallback ke `sh` jika `bash` tidak tersedia.
pub fn resolve_command_shell_with<F>(
    env_oopbot_shell: Option<&str>,
    env_shell: Option<&str>,
    env_prefix: Option<&str>,
    env_path: Option<&str>,
    exists: F,
) -> String
where
    F: Fn(&str) -> bool,
{
    if let Some(custom) = env_oopbot_shell.map(str::trim).filter(|s| !s.is_empty()) {
        if !custom.contains('/') || exists(custom) {
            return custom.to_string();
        }
    }

    if let Some(sh) = env_shell.map(str::trim).filter(|s| !s.is_empty()) {
        if (sh == "bash" || sh.ends_with("/bash")) && (!sh.contains('/') || exists(sh)) {
            return sh.to_string();
        }
    }

    if let Some(prefix) = env_prefix
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(|s| s.trim_end_matches('/'))
    {
        let bash_in_prefix = format!("{}/bin/bash", prefix);
        if exists(&bash_in_prefix) {
            return bash_in_prefix;
        }
    }

    if exists(TERMUX_DEFAULT_BASH) {
        return TERMUX_DEFAULT_BASH.to_string();
    }

    for candidate in ["/bin/bash", "/usr/bin/bash"] {
        if exists(candidate) {
            return candidate.to_string();
        }
    }

    if let Some(path_var) = env_path {
        for dir in std::env::split_paths(path_var) {
            let candidate = dir.join("bash");
            let cand_str = candidate.to_string_lossy();
            if exists(&cand_str) {
                return cand_str.into_owned();
            }
        }
    }

    if let Some(prefix) = env_prefix
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(|s| s.trim_end_matches('/'))
    {
        let sh_in_prefix = format!("{}/bin/sh", prefix);
        if exists(&sh_in_prefix) {
            return sh_in_prefix;
        }
    }

    let termux_sh = format!("{}/bin/sh", TERMUX_DEFAULT_PREFIX);
    if exists(&termux_sh) {
        return termux_sh;
    }

    if exists("/bin/sh") {
        return "/bin/sh".to_string();
    }

    "sh".to_string()
}

/// Resolusi shell runtime untuk `run_shell` dan `background`.
pub fn resolve_command_shell() -> String {
    let oopbot_shell = std::env::var("OOPBOT_SHELL").ok();
    let env_shell = std::env::var("SHELL").ok();
    let env_prefix = std::env::var("PREFIX").ok();
    let env_path = std::env::var("PATH").ok();
    resolve_command_shell_with(
        oopbot_shell.as_deref(),
        env_shell.as_deref(),
        env_prefix.as_deref(),
        env_path.as_deref(),
        |p| std::path::Path::new(p).is_file(),
    )
}

pub struct RunShell;

#[async_trait]
impl Tool for RunShell {
    fn name(&self) -> &'static str {
        "run_shell"
    }
    fn description(&self) -> &'static str {
        "Jalankan perintah shell (bash -c di Termux/Linux, fallback sh -c) di perangkat user pada direktori kerja saat ini. \
         Mendukung fitur bash (`time`, `[[ ... ]]`, `pipefail`, `history`, array) dan utilitas CLI internet \
         (`curl`, `aria2c -x 8 -s 8` untuk unduhan multi-koneksi cepat, `wget`, `jq`, `dig`/`nslookup`, `nc`, `ssh`/`scp`, `git`). \
         Mengembalikan exit_code, stdout, dan stderr. Paket Termux bisa dipasang dengan `pkg install -y <nama>`."
    }
    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "command": { "type": "string", "description": "Perintah shell (bash) yang akan dijalankan." },
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
        let shell = resolve_command_shell();

        let child = tokio::process::Command::new(&shell)
            .arg("-c")
            .arg(command)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .map_err(|e| format!("gagal menjalankan {}: {}", shell, e))?;

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

        // Lewati command wrapper seperti sudo, doas, busybox, toybox, exec, command, time
        while idx < words.len()
            && ["sudo", "doas", "busybox", "toybox", "exec", "command", "builtin", "time"]
                .contains(&words[idx])
        {
            idx += 1;
        }
        if idx >= words.len() {
            continue;
        }
        let base_cmd = words[idx];

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
