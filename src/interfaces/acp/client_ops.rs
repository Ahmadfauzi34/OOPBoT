//! Handler operasi sisi-client ACP yang diminta Agent -> Client:
//! - `fs/read_text_file`, `fs/write_text_file`
//! - `terminal/create`, `terminal/output`, `terminal/wait_for_exit`, `terminal/kill`, `terminal/release`
//! - Renderer HTML lokal untuk `export_html` sesi ACP.

use serde_json::{json, Value};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::{Arc, Mutex};
use tokio::io::AsyncReadExt;
use tokio::sync::{oneshot, watch};

const DEFAULT_TERMINAL_OUTPUT_BYTES: usize = 256 * 1024;
const MAX_TERMINAL_OUTPUT_BYTES: usize = 2 * 1024 * 1024;

fn resolve_acp_path(default_cwd: &Path, raw: &str) -> Result<PathBuf, (i64, String)> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Err((-32602, "Parameter 'path' wajib diisi".to_string()));
    }
    let p = PathBuf::from(trimmed);
    if p.is_absolute() {
        Ok(p)
    } else {
        Ok(default_cwd.join(p))
    }
}

/// Implementasi `fs/read_text_file` sesuai kontrak ACP (`path`, opsional `line` 1-based, `limit`).
pub(super) fn handle_fs_read_text_file(
    default_cwd: &Path,
    params: &Value,
) -> Result<Value, (i64, String)> {
    let raw_path = params["path"].as_str().unwrap_or_default();
    let path = resolve_acp_path(default_cwd, raw_path)?;
    let content = std::fs::read_to_string(&path).map_err(|e| {
        (
            -32000,
            format!("Gagal membaca file '{}': {}", path.display(), e),
        )
    })?;

    let start_line = params["line"].as_u64().map(|n| n.max(1) as usize);
    let limit = params["limit"].as_u64().map(|n| n as usize);

    let sliced = match (start_line, limit) {
        (None, None) => content,
        _ => {
            let start_idx = start_line.unwrap_or(1).saturating_sub(1);
            let lines: Vec<&str> = content.lines().collect();
            if start_idx >= lines.len() {
                String::new()
            } else {
                let end_idx = match limit {
                    Some(lim) => start_idx.saturating_add(lim).min(lines.len()),
                    None => lines.len(),
                };
                lines[start_idx..end_idx].join("\n")
            }
        }
    };

    Ok(json!({ "content": sliced }))
}

/// Implementasi `fs/write_text_file` sesuai kontrak ACP (`path`, `content`).
pub(super) fn handle_fs_write_text_file(
    default_cwd: &Path,
    params: &Value,
) -> Result<Value, (i64, String)> {
    let raw_path = params["path"].as_str().unwrap_or_default();
    let path = resolve_acp_path(default_cwd, raw_path)?;
    let Some(content) = params["content"].as_str() else {
        return Err((-32602, "Parameter 'content' string wajib diisi".to_string()));
    };

    if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
        std::fs::create_dir_all(parent).map_err(|e| {
            (
                -32000,
                format!("Gagal membuat folder '{}': {}", parent.display(), e),
            )
        })?;
    }
    std::fs::write(&path, content).map_err(|e| {
        (
            -32000,
            format!("Gagal menulis file '{}': {}", path.display(), e),
        )
    })?;

    Ok(json!({}))
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct AcpTerminalExit {
    pub(super) exit_code: Option<i64>,
    pub(super) signal: Option<String>,
}

impl AcpTerminalExit {
    pub(super) fn to_json(&self) -> Value {
        json!({
            "exitCode": self.exit_code,
            "signal": self.signal,
        })
    }
}

struct OutputBuffer {
    bytes: Vec<u8>,
    limit: usize,
    truncated: bool,
}

impl OutputBuffer {
    fn new(limit: usize) -> Self {
        Self {
            bytes: Vec::new(),
            limit: limit.clamp(1024, MAX_TERMINAL_OUTPUT_BYTES),
            truncated: false,
        }
    }

    fn push(&mut self, chunk: &[u8]) {
        if chunk.is_empty() {
            return;
        }
        self.bytes.extend_from_slice(chunk);
        if self.bytes.len() > self.limit {
            self.truncated = true;
            let mut drop_n = self.bytes.len() - self.limit;
            // Hindari memotong di tengah karakter multi-byte UTF-8.
            while drop_n < self.bytes.len() && (self.bytes[drop_n] & 0b1100_0000) == 0b1000_0000 {
                drop_n += 1;
            }
            self.bytes.drain(..drop_n);
        }
    }

    fn text(&self) -> String {
        String::from_utf8_lossy(&self.bytes).to_string()
    }
}

struct AcpTerminalEntry {
    output: Arc<Mutex<OutputBuffer>>,
    exit_rx: watch::Receiver<Option<AcpTerminalExit>>,
    kill_tx: Option<oneshot::Sender<()>>,
    pid: Option<u32>,
}

/// Pengelola subprocess terminal sementara untuk request ACP `terminal/*`.
pub(super) struct AcpTerminalManager {
    next_id: u64,
    terminals: HashMap<String, AcpTerminalEntry>,
}

fn signal_pgroup(pid: u32) {
    #[cfg(unix)]
    {
        if pid == 0 {
            return;
        }
        let target = format!("-{}", pid);
        let _ = std::process::Command::new("kill")
            .args(["-s", "KILL", "--", &target])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
    }
    #[cfg(not(unix))]
    {
        let _ = pid;
    }
}

fn spawn_pipe_collector<R: tokio::io::AsyncRead + Unpin + Send + 'static>(
    mut pipe: R,
    buf: Arc<Mutex<OutputBuffer>>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut chunk = [0u8; 4096];
        loop {
            match pipe.read(&mut chunk).await {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    let mut guard = buf.lock().unwrap_or_else(|e| e.into_inner());
                    guard.push(&chunk[..n]);
                }
            }
        }
    })
}

fn build_terminal_command(command: &str, args: &[String], cwd: &Path) -> tokio::process::Command {
    let mut cmd = if args.is_empty()
        && !Path::new(command).exists()
        && (command.contains(' ')
            || command.contains('|')
            || command.contains('&')
            || command.contains(';')
            || command.contains('$'))
    {
        let mut c = tokio::process::Command::new("sh");
        c.args(["-c", command]);
        c
    } else {
        let mut c = tokio::process::Command::new(command);
        c.args(args);
        c
    };

    cmd.current_dir(cwd)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    #[cfg(unix)]
    {
        cmd.process_group(0);
    }
    cmd
}

impl AcpTerminalManager {
    pub(super) fn new() -> Self {
        Self {
            next_id: 1,
            terminals: HashMap::new(),
        }
    }

    pub(super) fn create(
        &mut self,
        default_cwd: &Path,
        params: &Value,
    ) -> Result<String, (i64, String)> {
        let command = params["command"]
            .as_str()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .ok_or_else(|| (-32602, "Parameter 'command' wajib diisi".to_string()))?;

        let args: Vec<String> = params["args"]
            .as_array()
            .map(|arr| {
                arr.iter()
                    .filter_map(|v| v.as_str().map(str::to_string))
                    .collect()
            })
            .unwrap_or_default();

        let cwd = params["cwd"]
            .as_str()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(|p| resolve_acp_path(default_cwd, p))
            .transpose()?
            .unwrap_or_else(|| default_cwd.to_path_buf());
        if !cwd.is_dir() {
            return Err((-32602, format!("Direktori kerja tidak ada: {}", cwd.display())));
        }

        let limit = params["outputByteLimit"]
            .as_u64()
            .map(|v| v as usize)
            .unwrap_or(DEFAULT_TERMINAL_OUTPUT_BYTES);

        let mut cmd = build_terminal_command(command, &args, &cwd);
        if let Some(env_arr) = params["env"].as_array() {
            for item in env_arr {
                if let (Some(k), Some(v)) = (item["name"].as_str(), item["value"].as_str()) {
                    cmd.env(k, v);
                }
            }
        } else if let Some(env_obj) = params["env"].as_object() {
            for (k, v) in env_obj {
                if let Some(val) = v.as_str() {
                    cmd.env(k, val);
                }
            }
        }

        let mut child = cmd.spawn().map_err(|e| {
            (
                -32000,
                format!("Gagal meluncurkan terminal '{}': {}", command, e),
            )
        })?;
        let pid = child.id();
        let out_buf = Arc::new(Mutex::new(OutputBuffer::new(limit)));
        let stdout_task = child
            .stdout
            .take()
            .map(|p| spawn_pipe_collector(p, Arc::clone(&out_buf)));
        let stderr_task = child
            .stderr
            .take()
            .map(|p| spawn_pipe_collector(p, Arc::clone(&out_buf)));

        let (exit_tx, exit_rx) = watch::channel(None);
        let (kill_tx, mut kill_rx) = oneshot::channel::<()>();

        tokio::spawn(async move {
            let status_res = tokio::select! {
                res = child.wait() => res,
                _ = &mut kill_rx => {
                    if let Some(p) = pid {
                        signal_pgroup(p);
                    }
                    let _ = child.start_kill();
                    child.wait().await
                }
            };
            if let Some(t) = stdout_task {
                let _ = t.await;
            }
            if let Some(t) = stderr_task {
                let _ = t.await;
            }
            let exit_info = match status_res {
                Ok(status) => {
                    #[cfg(unix)]
                    let sig = {
                        use std::os::unix::process::ExitStatusExt;
                        status.signal().map(|s| format!("SIG{}", s))
                    };
                    #[cfg(not(unix))]
                    let sig: Option<String> = None;
                    AcpTerminalExit {
                        exit_code: status.code().map(|c| c as i64),
                        signal: sig,
                    }
                }
                Err(_) => AcpTerminalExit {
                    exit_code: Some(1),
                    signal: None,
                },
            };
            let _ = exit_tx.send(Some(exit_info));
        });

        let term_id = format!("term-{}", self.next_id);
        self.next_id += 1;
        self.terminals.insert(
            term_id.clone(),
            AcpTerminalEntry {
                output: out_buf,
                exit_rx,
                kill_tx: Some(kill_tx),
                pid,
            },
        );
        Ok(term_id)
    }

    pub(super) fn output(&self, params: &Value) -> Result<Value, (i64, String)> {
        let term_id = params["terminalId"]
            .as_str()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .ok_or_else(|| (-32602, "Parameter 'terminalId' wajib diisi".to_string()))?;
        let Some(entry) = self.terminals.get(term_id) else {
            return Err((-32602, format!("Terminal '{}' tidak ditemukan", term_id)));
        };
        let guard = entry.output.lock().unwrap_or_else(|e| e.into_inner());
        let exit_status = entry
            .exit_rx
            .borrow()
            .as_ref()
            .map(AcpTerminalExit::to_json);
        Ok(json!({
            "output": guard.text(),
            "truncated": guard.truncated,
            "exitStatus": exit_status,
        }))
    }

    pub(super) fn exit_watcher(
        &self,
        params: &Value,
    ) -> Result<watch::Receiver<Option<AcpTerminalExit>>, (i64, String)> {
        let term_id = params["terminalId"]
            .as_str()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .ok_or_else(|| (-32602, "Parameter 'terminalId' wajib diisi".to_string()))?;
        let Some(entry) = self.terminals.get(term_id) else {
            return Err((-32602, format!("Terminal '{}' tidak ditemukan", term_id)));
        };
        Ok(entry.exit_rx.clone())
    }

    pub(super) fn kill(&mut self, params: &Value) -> Result<Value, (i64, String)> {
        let term_id = params["terminalId"]
            .as_str()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .ok_or_else(|| (-32602, "Parameter 'terminalId' wajib diisi".to_string()))?;
        let Some(entry) = self.terminals.get_mut(term_id) else {
            return Err((-32602, format!("Terminal '{}' tidak ditemukan", term_id)));
        };
        if let Some(pid) = entry.pid {
            signal_pgroup(pid);
        }
        if let Some(tx) = entry.kill_tx.take() {
            let _ = tx.send(());
        }
        Ok(json!({}))
    }

    pub(super) fn release(&mut self, params: &Value) -> Result<Value, (i64, String)> {
        let term_id = params["terminalId"]
            .as_str()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .ok_or_else(|| (-32602, "Parameter 'terminalId' wajib diisi".to_string()))?;
        let Some(mut entry) = self.terminals.remove(term_id) else {
            return Err((-32602, format!("Terminal '{}' tidak ditemukan", term_id)));
        };
        if let Some(pid) = entry.pid {
            signal_pgroup(pid);
        }
        if let Some(tx) = entry.kill_tx.take() {
            let _ = tx.send(());
        }
        Ok(json!({}))
    }

    pub(super) fn shutdown_all(&mut self) {
        for (_, mut entry) in self.terminals.drain() {
            if let Some(pid) = entry.pid {
                signal_pgroup(pid);
            }
            if let Some(tx) = entry.kill_tx.take() {
                let _ = tx.send(());
            }
        }
    }
}

impl Drop for AcpTerminalManager {
    fn drop(&mut self) {
        self.shutdown_all();
    }
}

fn html_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for ch in s.chars() {
        match ch {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#39;"),
            _ => out.push(ch),
        }
    }
    out
}

/// Render transkrip HTML mandiri untuk command `export_html` di mode ACP.
pub(super) fn render_acp_session_html(
    session_id: &str,
    session_name: Option<&str>,
    cwd: &Path,
    engine_label: &str,
    ui_messages: &[Value],
) -> String {
    let title = session_name
        .filter(|s| !s.trim().is_empty())
        .unwrap_or(session_id);
    let mut body = String::new();
    for msg in ui_messages {
        let role = msg["role"].as_str().unwrap_or("unknown");
        let (role_label, cls) = match role {
            "user" => ("User", "msg user"),
            "assistant" => ("Assistant", "msg assistant"),
            "toolResult" => ("Tool Result", "msg tool"),
            other => (other, "msg"),
        };
        body.push_str(&format!(
            "<section class=\"{}\"><header>{}</header>",
            cls,
            html_escape(role_label)
        ));
        if let Some(blocks) = msg["content"].as_array() {
            for b in blocks {
                match b["type"].as_str().unwrap_or_default() {
                    "text" => {
                        if let Some(t) = b["text"].as_str() {
                            body.push_str(&format!("<pre class=\"text\">{}</pre>", html_escape(t)));
                        }
                    }
                    "thinking" => {
                        if let Some(t) = b["thinking"].as_str() {
                            body.push_str(&format!(
                                "<details class=\"thinking\"><summary>Thinking</summary><pre>{}</pre></details>",
                                html_escape(t)
                            ));
                        }
                    }
                    "toolCall" => {
                        let name = b["name"].as_str().unwrap_or("tool");
                        let args = serde_json::to_string_pretty(&b["arguments"]).unwrap_or_default();
                        body.push_str(&format!(
                            "<details class=\"call\"><summary>Tool: {}</summary><pre>{}</pre></details>",
                            html_escape(name),
                            html_escape(&args)
                        ));
                    }
                    _ => {}
                }
            }
        }
        body.push_str("</section>\n");
    }

    format!(
        "<!DOCTYPE html><html lang=\"en\"><head><meta charset=\"utf-8\">\
        <meta name=\"viewport\" content=\"width=device-width,initial-scale=1\">\
        <title>{}</title><style>\
        body{{font-family:system-ui,-apple-system,sans-serif;background:#111318;color:#e6e9ef;margin:0;padding:24px;line-height:1.5}}\
        .meta{{color:#9aa4b2;font-size:13px;margin-bottom:20px}}\
        .msg{{border:1px solid #262b36;border-radius:12px;padding:12px 16px;margin-bottom:14px;background:#181c24}}\
        .msg.user{{border-color:#3a4b7c}}\
        header{{font-weight:700;font-size:13px;color:#93c5fd;margin-bottom:6px}}\
        pre{{white-space:pre-wrap;word-break:break-word;margin:6px 0;font-family:ui-monospace,monospace;font-size:13px}}\
        details{{margin-top:8px;color:#b4bccb}}\
        </style></head><body><h1>{}</h1>\
        <div class=\"meta\">Engine: {} · CWD: {}</div>{}</body></html>",
        html_escape(title),
        html_escape(title),
        html_escape(engine_label),
        html_escape(&cwd.to_string_lossy()),
        body
    )
}
