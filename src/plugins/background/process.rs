//! Manajemen siklus hidup proses background, process group Unix, timeout, dan `wait` berbasis event.

use super::buffer::{
    OutputQuery, RingOutputBuffer, DEFAULT_OUTPUT_RETURN_BYTES, MAX_LIST_PREVIEW_BYTES,
    MAX_OUTPUT_BUFFER_BYTES,
};
use super::BgTaskInfo;
use crate::tools::expand_path;
use serde_json::{json, Value};
use std::collections::HashMap;
use std::path::PathBuf;
use std::process::Stdio;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::io::AsyncReadExt;

/// Batas maksimal proses background yang berjalan bersamaan.
pub const MAX_RUNNING_TASKS: usize = 16;
/// Batas maksimal total task (aktif + selesai) yang dipertahankan di memori.
pub const MAX_RETAINED_TASKS: usize = 50;
/// Batas timeout eksekusi opsional maksimal (24 jam).
pub const MAX_TIMEOUT_SECS: u64 = 86_400;
/// Default timeout tunggu untuk aksi `wait`.
pub const DEFAULT_WAIT_TIMEOUT_SECS: u64 = 15;
/// Batas maksimal timeout untuk aksi `wait`.
pub const MAX_WAIT_TIMEOUT_SECS: u64 = 60;

const READ_CHUNK_BYTES: usize = 4096;
const SIGTERM_GRACE_MS: u64 = 150;
const PIPE_DRAIN_AFTER_KILL_MS: u64 = 250;
const COMPLETION_TAIL_BYTES: usize = 2000;

/// Event ringkasan saat proses background selesai/timeout secara mandiri di latar belakang.
#[derive(Debug, Clone)]
pub struct BgCompletionEvent {
    pub id: u32,
    pub label: Option<String>,
    pub command: String,
    pub status: String,
    pub exit_code: Option<i32>,
    pub elapsed_secs: u64,
    pub output_tail: String,
}

pub type StatusNotifier = Arc<dyn Fn(usize) + Send + Sync>;
pub type CompletionNotifier = Arc<dyn Fn(BgCompletionEvent) + Send + Sync>;

pub(super) struct TaskRecord {
    pub(super) id: u32,
    pub(super) label: Option<String>,
    pub(super) command: String,
    pub(super) cwd: Option<String>,
    pub(super) status: String,
    pub(super) pid: Option<u32>,
    pub(super) exit_code: Option<i32>,
    pub(super) started_at: Instant,
    pub(super) finished_at: Option<Instant>,
    pub(super) buffer: RingOutputBuffer,
    pub(super) status_note: Option<String>,
    pub(super) notify_on_complete: bool,
    pub(super) active_waiters: usize,
    pub(super) update_tx: tokio::sync::watch::Sender<u64>,
    pub(super) update_rx: tokio::sync::watch::Receiver<u64>,
}

impl TaskRecord {
    pub(super) fn is_running(&self) -> bool {
        self.status == "running"
    }

    pub(super) fn elapsed_secs(&self) -> u64 {
        let end = self.finished_at.unwrap_or_else(Instant::now);
        end.saturating_duration_since(self.started_at).as_secs()
    }

    pub(super) fn formatted_output(&self, max_bytes: usize) -> String {
        let body = self.buffer.render_tail(max_bytes);
        attach_status_note(body, self.status_note.as_deref())
    }

    pub(super) fn to_info(&self, preview_bytes: usize) -> BgTaskInfo {
        BgTaskInfo {
            id: self.id,
            label: self.label.clone(),
            command: self.command.clone(),
            cwd: self.cwd.clone(),
            status: self.status.clone(),
            output: self.formatted_output(preview_bytes),
            pid: self.pid,
            exit_code: self.exit_code,
            total_output_bytes: self.buffer.total_bytes,
            truncated: self.buffer.is_truncated(),
            elapsed_secs: self.elapsed_secs(),
        }
    }
}

pub(super) fn attach_status_note(body: String, note: Option<&str>) -> String {
    match note {
        Some(n) if body.is_empty() => n.to_string(),
        Some(n) => format!("{}\n{}", body, n),
        None => body,
    }
}

pub(super) struct BgState {
    pub(super) tasks: Mutex<HashMap<u32, TaskRecord>>,
    pub(super) kill_handles: Mutex<HashMap<u32, tokio::sync::oneshot::Sender<()>>>,
    pub(super) next_id: AtomicU32,
    pub(super) status_notifier: Mutex<Option<StatusNotifier>>,
    pub(super) completion_notifier: Mutex<Option<CompletionNotifier>>,
}

impl BgState {
    pub(super) fn new() -> Self {
        Self {
            tasks: Mutex::new(HashMap::new()),
            kill_handles: Mutex::new(HashMap::new()),
            next_id: AtomicU32::new(1),
            status_notifier: Mutex::new(None),
            completion_notifier: Mutex::new(None),
        }
    }

    pub(super) fn active_count(&self) -> usize {
        let lock = self.tasks.lock().unwrap_or_else(|e| e.into_inner());
        lock.values().filter(|t| t.is_running()).count()
    }

    pub(super) fn set_status_notifier(&self, notifier: StatusNotifier) {
        let mut lock = self
            .status_notifier
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        *lock = Some(notifier);
    }

    pub(super) fn set_completion_notifier(&self, notifier: CompletionNotifier) {
        let mut lock = self
            .completion_notifier
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        *lock = Some(notifier);
    }

    pub(super) fn emit_status_change(&self, active: usize) {
        let cb = self
            .status_notifier
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        if let Some(f) = cb {
            f(active);
        }
    }

    pub(super) fn emit_completion(&self, event: BgCompletionEvent) {
        let cb = self
            .completion_notifier
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        if let Some(f) = cb {
            f(event);
        }
    }

    pub(super) fn shutdown_all(&self) {
        let senders: Vec<tokio::sync::oneshot::Sender<()>> = {
            let mut k_lock = self.kill_handles.lock().unwrap_or_else(|e| e.into_inner());
            k_lock.drain().map(|(_, tx)| tx).collect()
        };
        for tx in senders {
            let _ = tx.send(());
        }

        let (pids, waiters): (Vec<u32>, Vec<tokio::sync::watch::Sender<u64>>) = {
            let mut lock = self.tasks.lock().unwrap_or_else(|e| e.into_inner());
            let mut active_pids = Vec::new();
            let mut senders = Vec::new();
            for item in lock.values_mut() {
                if item.is_running() {
                    item.status = "killed".to_string();
                    item.finished_at = Some(Instant::now());
                    item.status_note = Some("[Proses dihentikan]".to_string());
                    if let Some(pid) = item.pid {
                        active_pids.push(pid);
                    }
                    senders.push(item.update_tx.clone());
                }
            }
            (active_pids, senders)
        };

        for tx in waiters {
            tx.send_modify(|n| *n = n.wrapping_add(1));
        }
        for pid in pids {
            signal_process_group(pid, true);
        }
    }
}

impl Drop for BgState {
    fn drop(&mut self) {
        self.shutdown_all();
    }
}

pub(super) fn optional_str_arg<'a>(args: &'a Value, key: &str) -> Result<Option<&'a str>, String> {
    if args[key].is_null() {
        return Ok(None);
    }
    args[key]
        .as_str()
        .map(Some)
        .ok_or_else(|| format!("argumen '{}' harus berupa string", key))
}

pub(super) fn optional_u64_arg(args: &Value, key: &str) -> Result<Option<u64>, String> {
    if args[key].is_null() {
        return Ok(None);
    }
    args[key]
        .as_u64()
        .map(Some)
        .ok_or_else(|| format!("argumen '{}' harus berupa bilangan bulat non-negatif", key))
}

pub(super) fn optional_bool_arg(args: &Value, key: &str) -> Result<Option<bool>, String> {
    if args[key].is_null() {
        return Ok(None);
    }
    args[key]
        .as_bool()
        .map(Some)
        .ok_or_else(|| format!("argumen '{}' harus berupa boolean", key))
}

pub(super) fn parse_tail_bytes(args: &Value) -> Result<usize, String> {
    Ok(optional_u64_arg(args, "tail_bytes")?
        .map(|n| {
            usize::try_from(n)
                .unwrap_or(MAX_OUTPUT_BUFFER_BYTES)
                .clamp(256, MAX_OUTPUT_BUFFER_BYTES)
        })
        .unwrap_or(DEFAULT_OUTPUT_RETURN_BYTES))
}

pub(super) fn parse_tail_lines(args: &Value) -> Result<Option<usize>, String> {
    match optional_u64_arg(args, "tail_lines")? {
        None => Ok(None),
        Some(0) => Err("argumen 'tail_lines' minimal 1".to_string()),
        Some(n) => Ok(Some(usize::try_from(n).unwrap_or(2000).clamp(1, 2000))),
    }
}

pub(super) fn parse_task_id(args: &Value, action: &str) -> Result<u32, String> {
    if args["id"].is_null() {
        return Err(format!("argumen 'id' wajib diisi untuk aksi {}", action));
    }
    let raw = args["id"].as_u64().ok_or_else(|| {
        format!(
            "argumen 'id' harus berupa bilangan bulat non-negatif untuk aksi {}",
            action
        )
    })?;
    u32::try_from(raw).map_err(|_| format!("ID task background '{}' tidak valid", raw))
}

pub(super) fn resolve_cwd(raw: Option<&str>) -> Result<Option<PathBuf>, String> {
    let Some(trimmed) = raw.map(str::trim).filter(|s| !s.is_empty()) else {
        return Ok(None);
    };
    let expanded = expand_path(trimmed);
    if !expanded.exists() {
        return Err(format!(
            "Direktori kerja '{}' tidak ditemukan",
            expanded.display()
        ));
    }
    if !expanded.is_dir() {
        return Err(format!(
            "Path '{}' bukan direktori yang valid",
            expanded.display()
        ));
    }
    Ok(Some(expanded))
}

pub(super) fn parse_env_map(val: &Value) -> Result<Vec<(String, String)>, String> {
    if val.is_null() {
        return Ok(Vec::new());
    }
    let obj = val
        .as_object()
        .ok_or_else(|| "argumen 'env' harus berupa objek JSON string-ke-string".to_string())?;
    let mut pairs = Vec::with_capacity(obj.len());
    for (k, v) in obj {
        if k.trim().is_empty() || k.contains('=') || k.contains('\0') {
            return Err(format!("Nama variabel env '{}' tidak valid", k));
        }
        let s = v
            .as_str()
            .ok_or_else(|| format!("Nilai env '{}' harus berupa string", k))?;
        if s.contains('\0') {
            return Err(format!("Nilai env '{}' mengandung karakter NUL", k));
        }
        pairs.push((k.clone(), s.to_string()));
    }
    Ok(pairs)
}

pub(super) fn prune_finished_tasks(tasks: &mut HashMap<u32, TaskRecord>, target_max: usize) {
    if tasks.len() <= target_max {
        return;
    }
    let mut finished_ids: Vec<u32> = tasks
        .iter()
        .filter(|(_, r)| !r.is_running())
        .map(|(&id, _)| id)
        .collect();
    finished_ids.sort_unstable();
    let to_remove = tasks.len().saturating_sub(target_max);
    for id in finished_ids.into_iter().take(to_remove) {
        tasks.remove(&id);
    }
}

pub(super) async fn start_task(state: &Arc<BgState>, args: &Value) -> Result<Value, String> {
    let cmd = args["command"]
        .as_str()
        .filter(|s| !s.trim().is_empty())
        .ok_or_else(|| "argumen 'command' (string) wajib diisi untuk aksi start".to_string())?;

    let label = optional_str_arg(args, "label")?
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(|s| s.chars().take(80).collect::<String>());
    let cwd = resolve_cwd(optional_str_arg(args, "cwd")?)?;
    let envs = parse_env_map(&args["env"])?;
    let timeout_secs = match optional_u64_arg(args, "timeout_secs")? {
        None => None,
        Some(0) => return Err("argumen 'timeout_secs' minimal 1 detik".to_string()),
        Some(secs) => Some(secs.min(MAX_TIMEOUT_SECS)),
    };
    let notify_on_complete = optional_bool_arg(args, "notify_on_complete")?.unwrap_or(true);

    {
        let mut lock = state.tasks.lock().unwrap_or_else(|e| e.into_inner());
        let running_count = lock.values().filter(|t| t.is_running()).count();
        if running_count >= MAX_RUNNING_TASKS {
            return Err(format!(
                "Batas proses background aktif tercapai ({}/{}). Hentikan task yang tidak dipakai dengan `kill` terlebih dahulu.",
                running_count, MAX_RUNNING_TASKS
            ));
        }
        prune_finished_tasks(&mut lock, MAX_RETAINED_TASKS.saturating_sub(1));
    }

    let cmd_str = cmd.to_string();
    let mut command = tokio::process::Command::new("sh");
    command
        .arg("-c")
        .arg(&cmd_str)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);

    if let Some(dir) = &cwd {
        command.current_dir(dir);
    }
    for (k, v) in &envs {
        command.env(k, v);
    }

    #[cfg(unix)]
    {
        // Jalankan di process group tersendiri agar `kill`/timeout juga mematikan seluruh
        // subprocess turunan yang di-fork oleh `sh -c`.
        command.process_group(0);
    }

    let mut child = command
        .spawn()
        .map_err(|e| format!("Gagal mengeksekusi '{}': {}", cmd_str, e))?;

    let pid = child.id();
    let id = state.next_id.fetch_add(1, Ordering::SeqCst);
    let (kill_tx, mut kill_rx) = tokio::sync::oneshot::channel::<()>();
    let (update_tx, update_rx) = tokio::sync::watch::channel(0u64);
    let cwd_str = cwd.as_ref().map(|p| p.to_string_lossy().to_string());

    {
        let mut lock = state.tasks.lock().unwrap_or_else(|e| e.into_inner());
        lock.insert(
            id,
            TaskRecord {
                id,
                label: label.clone(),
                command: cmd_str.clone(),
                cwd: cwd_str.clone(),
                status: "running".to_string(),
                pid,
                exit_code: None,
                started_at: Instant::now(),
                finished_at: None,
                buffer: RingOutputBuffer::default(),
                status_note: None,
                notify_on_complete,
                active_waiters: 0,
                update_tx: update_tx.clone(),
                update_rx,
            },
        );
    }
    {
        let mut k_lock = state.kill_handles.lock().unwrap_or_else(|e| e.into_inner());
        k_lock.insert(id, kill_tx);
    }

    let stdout_pipe = child.stdout.take();
    let stderr_pipe = child.stderr.take();
    let state_for_io = Arc::clone(state);
    let io_task = tokio::spawn(async move {
        stream_pipes_into_buffer(id, stdout_pipe, stderr_pipe, state_for_io).await;
    });

    let state_for_wait = Arc::clone(state);
    tokio::spawn(async move {
        let mut io_task = io_task;
        let mut was_killed = false;
        let mut timed_out = false;

        let timeout_fut = async {
            match timeout_secs {
                Some(secs) => tokio::time::sleep(Duration::from_secs(secs)).await,
                None => std::future::pending::<()>().await,
            }
        };
        tokio::pin!(timeout_fut);

        let wait_status = tokio::select! {
            res = child.wait() => res,
            _ = &mut kill_rx => {
                was_killed = true;
                terminate_child_tree(&mut child, pid).await
            }
            _ = &mut timeout_fut => {
                timed_out = true;
                terminate_child_tree(&mut child, pid).await
            }
        };

        {
            let mut k_lock = state_for_wait
                .kill_handles
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            k_lock.remove(&id);
        }

        // Tunggu sebentar agar pipe yang tersisa selesai terbaca; jika ada proses cucu bandel
        // yang masih memegang descriptor pipe, batalkan reader agar tidak hang selamanya.
        if tokio::time::timeout(
            Duration::from_millis(PIPE_DRAIN_AFTER_KILL_MS),
            &mut io_task,
        )
        .await
        .is_err()
        {
            io_task.abort();
        }

        let (status, exit_code, note) = if was_killed {
            (
                "killed".to_string(),
                wait_status.ok().and_then(|s| s.code()),
                Some("[Proses dihentikan oleh pengguna]".to_string()),
            )
        } else if timed_out {
            (
                "timeout".to_string(),
                wait_status.ok().and_then(|s| s.code()),
                Some(format!(
                    "[Proses melebihi batas timeout {} detik dan dihentikan]",
                    timeout_secs.unwrap_or(0)
                )),
            )
        } else {
            match wait_status {
                Ok(st) => {
                    let code = st.code();
                    let label = if st.success() { "completed" } else { "failed" };
                    (
                        label.to_string(),
                        code,
                        Some(format!(
                            "[Proses selesai dengan exit code {}]",
                            code.unwrap_or(-1)
                        )),
                    )
                }
                Err(e) => (
                    "error".to_string(),
                    None,
                    Some(format!("[Gagal menunggu proses: {}]", e)),
                ),
            }
        };

        let (active_after, transitioned, completion_evt) = {
            let mut lock = state_for_wait
                .tasks
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            let mut changed = false;
            let mut evt = None;
            if let Some(item) = lock.get_mut(&id) {
                if item.status == "running" {
                    item.status = status;
                    item.status_note = note;
                    changed = true;
                }
                item.exit_code = exit_code;
                if item.finished_at.is_none() {
                    item.finished_at = Some(Instant::now());
                }
                if changed && !was_killed && item.notify_on_complete && item.active_waiters == 0 {
                    evt = Some(BgCompletionEvent {
                        id: item.id,
                        label: item.label.clone(),
                        command: item.command.clone(),
                        status: item.status.clone(),
                        exit_code: item.exit_code,
                        elapsed_secs: item.elapsed_secs(),
                        output_tail: item.formatted_output(COMPLETION_TAIL_BYTES),
                    });
                }
            }
            let active = lock.values().filter(|t| t.is_running()).count();
            (active, changed, evt)
        };

        update_tx.send_modify(|n| *n = n.wrapping_add(1));

        // Pancarkan update oopbot_channel & auto-wakeup agen saat proses selesai/timeout secara mandiri.
        if transitioned && !was_killed {
            state_for_wait.emit_status_change(active_after);
            if let Some(evt) = completion_evt {
                state_for_wait.emit_completion(evt);
            }
        }
    });

    Ok(json!({
        "status": "started",
        "id": id,
        "label": label,
        "pid": pid,
        "cwd": cwd_str,
        "command": cmd,
        "timeout_secs": timeout_secs,
        "notify_on_complete": notify_on_complete,
        "message": format!("Perintah dijalankan di background dengan ID #{}", id)
    }))
}

pub(super) fn list_tasks(state: &BgState) -> Value {
    let lock = state.tasks.lock().unwrap_or_else(|e| e.into_inner());
    let mut list: Vec<BgTaskInfo> = lock
        .values()
        .map(|r| r.to_info(MAX_LIST_PREVIEW_BYTES))
        .collect();
    list.sort_by_key(|t| t.id);
    let active = list.iter().filter(|t| t.status == "running").count();
    json!({
        "count": list.len(),
        "active": active,
        "tasks": list
    })
}

pub(super) fn query_output(state: &BgState, args: &Value) -> Result<Value, String> {
    let id = parse_task_id(args, "output")?;
    let since_byte = optional_u64_arg(args, "since_byte")?;
    let tail_bytes = parse_tail_bytes(args)?;
    let tail_lines = parse_tail_lines(args)?;
    let filter = optional_str_arg(args, "filter")?;

    let lock = state.tasks.lock().unwrap_or_else(|e| e.into_inner());
    let item = lock
        .get(&id)
        .ok_or_else(|| format!("Task background #{} tidak ditemukan", id))?;

    let slice = item.buffer.query(OutputQuery {
        since_byte,
        tail_bytes,
        tail_lines,
        filter,
    });
    let output = if item.is_running() {
        slice.text
    } else {
        attach_status_note(slice.text, item.status_note.as_deref())
    };

    Ok(json!({
        "id": id,
        "label": item.label,
        "status": item.status,
        "pid": item.pid,
        "exit_code": item.exit_code,
        "elapsed_secs": item.elapsed_secs(),
        "total_output_bytes": item.buffer.total_bytes,
        "window_start_byte": slice.window_start_byte,
        "next_byte": slice.next_byte,
        "cursor_reset": slice.cursor_reset,
        "truncated": slice.truncated,
        "output": output
    }))
}

struct WaiterGuard {
    state: Arc<BgState>,
    id: u32,
}

impl Drop for WaiterGuard {
    fn drop(&mut self) {
        let mut lock = self.state.tasks.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(item) = lock.get_mut(&self.id) {
            item.active_waiters = item.active_waiters.saturating_sub(1);
        }
    }
}

pub(super) async fn wait_for_task(state: &Arc<BgState>, args: &Value) -> Result<Value, String> {
    let id = parse_task_id(args, "wait")?;
    let raw_wait = match optional_u64_arg(args, "wait_timeout_secs")? {
        Some(n) => Some(n),
        None => optional_u64_arg(args, "timeout_secs")?,
    };
    let wait_secs = match raw_wait {
        None => DEFAULT_WAIT_TIMEOUT_SECS,
        Some(0) => return Err("argumen 'wait_timeout_secs' minimal 1 detik".to_string()),
        Some(n) => n.min(MAX_WAIT_TIMEOUT_SECS),
    };
    let pattern = optional_str_arg(args, "pattern")?
        .map(str::trim)
        .filter(|s| !s.is_empty());
    let tail_bytes = parse_tail_bytes(args)?;

    let mut rx = {
        let mut lock = state.tasks.lock().unwrap_or_else(|e| e.into_inner());
        let item = lock
            .get_mut(&id)
            .ok_or_else(|| format!("Task background #{} tidak ditemukan", id))?;
        item.active_waiters = item.active_waiters.saturating_add(1);
        item.update_rx.clone()
    };
    let _waiter_guard = WaiterGuard {
        state: Arc::clone(state),
        id,
    };

    let deadline = tokio::time::Instant::now() + Duration::from_secs(wait_secs);

    loop {
        // Tandai snapshot watch SEBELUM memeriksa tasks lock agar perubahan antara cek & await tidak hilang.
        let _ = rx.borrow_and_update();

        {
            let lock = state.tasks.lock().unwrap_or_else(|e| e.into_inner());
            let item = lock
                .get(&id)
                .ok_or_else(|| format!("Task background #{} tidak ditemukan", id))?;

            let matched = pattern
                .map(|pat| item.buffer.contains_pattern(pat))
                .unwrap_or(false);

            if !item.is_running() || matched {
                return Ok(build_wait_payload(item, tail_bytes, matched, false));
            }
        }

        match tokio::time::timeout_at(deadline, rx.changed()).await {
            Err(_) => {
                let lock = state.tasks.lock().unwrap_or_else(|e| e.into_inner());
                let item = lock
                    .get(&id)
                    .ok_or_else(|| format!("Task background #{} tidak ditemukan", id))?;
                let matched = pattern
                    .map(|pat| item.buffer.contains_pattern(pat))
                    .unwrap_or(false);
                return Ok(build_wait_payload(
                    item,
                    tail_bytes,
                    matched,
                    item.is_running() && !matched,
                ));
            }
            Ok(Err(_)) => {
                // Sender selesai/dropped; loop sekali lagi untuk membaca status akhir.
                let lock = state.tasks.lock().unwrap_or_else(|e| e.into_inner());
                let item = lock
                    .get(&id)
                    .ok_or_else(|| format!("Task background #{} tidak ditemukan", id))?;
                let matched = pattern
                    .map(|pat| item.buffer.contains_pattern(pat))
                    .unwrap_or(false);
                return Ok(build_wait_payload(item, tail_bytes, matched, false));
            }
            Ok(Ok(())) => continue,
        }
    }
}

fn build_wait_payload(
    item: &TaskRecord,
    tail_bytes: usize,
    matched: bool,
    wait_timed_out: bool,
) -> Value {
    let slice = item.buffer.query(OutputQuery {
        since_byte: None,
        tail_bytes,
        tail_lines: None,
        filter: None,
    });
    let output = if item.is_running() {
        slice.text
    } else {
        attach_status_note(slice.text, item.status_note.as_deref())
    };
    json!({
        "id": item.id,
        "label": item.label,
        "status": item.status,
        "pid": item.pid,
        "exit_code": item.exit_code,
        "elapsed_secs": item.elapsed_secs(),
        "matched": matched,
        "wait_timed_out": wait_timed_out,
        "next_byte": slice.next_byte,
        "truncated": slice.truncated,
        "output": output
    })
}

pub(super) fn kill_task(state: &BgState, args: &Value) -> Result<Value, String> {
    let id = parse_task_id(args, "kill")?;
    let (already_finished, current_status, pid) = {
        let mut lock = state.tasks.lock().unwrap_or_else(|e| e.into_inner());
        let item = lock
            .get_mut(&id)
            .ok_or_else(|| format!("Task background #{} tidak ditemukan", id))?;

        if !item.is_running() {
            (true, item.status.clone(), item.pid)
        } else {
            item.status = "killed".to_string();
            item.finished_at = Some(Instant::now());
            item.status_note = Some("[Proses dihentikan oleh pengguna]".to_string());
            item.update_tx.send_modify(|n| *n = n.wrapping_add(1));
            (false, "killed".to_string(), item.pid)
        }
    };

    if already_finished {
        return Ok(json!({
            "status": current_status,
            "id": id,
            "already_finished": true,
            "message": format!("Task background #{} sudah selesai sebelumnya (status: {})", id, current_status)
        }));
    }

    let tx = {
        let mut k_lock = state.kill_handles.lock().unwrap_or_else(|e| e.into_inner());
        k_lock.remove(&id)
    };
    if let Some(tx) = tx {
        let _ = tx.send(());
    } else if let Some(pid) = pid {
        signal_process_group(pid, true);
    }

    Ok(json!({
        "status": "killed",
        "id": id,
        "message": format!("Task background #{} dihentikan", id)
    }))
}

pub(super) fn clear_finished(state: &BgState) -> Value {
    let mut lock = state.tasks.lock().unwrap_or_else(|e| e.into_inner());
    let before = lock.len();
    lock.retain(|_, item| item.is_running());
    let removed = before.saturating_sub(lock.len());
    json!({
        "removed": removed,
        "remaining": lock.len()
    })
}

async fn stream_pipes_into_buffer(
    id: u32,
    mut stdout: Option<tokio::process::ChildStdout>,
    mut stderr: Option<tokio::process::ChildStderr>,
    state: Arc<BgState>,
) {
    let mut out_buf = [0u8; READ_CHUNK_BYTES];
    let mut err_buf = [0u8; READ_CHUNK_BYTES];

    loop {
        if stdout.is_none() && stderr.is_none() {
            break;
        }
        tokio::select! {
            res = async {
                match stdout.as_mut() {
                    Some(s) => s.read(&mut out_buf).await,
                    None => std::future::pending().await,
                }
            }, if stdout.is_some() => {
                match res {
                    Ok(0) | Err(_) => stdout = None,
                    Ok(n) => append_task_bytes(&state, id, &out_buf[..n]),
                }
            }
            res = async {
                match stderr.as_mut() {
                    Some(s) => s.read(&mut err_buf).await,
                    None => std::future::pending().await,
                }
            }, if stderr.is_some() => {
                match res {
                    Ok(0) | Err(_) => stderr = None,
                    Ok(n) => append_task_bytes(&state, id, &err_buf[..n]),
                }
            }
        }
    }
}

fn append_task_bytes(state: &BgState, id: u32, chunk: &[u8]) {
    let tx = {
        let mut lock = state.tasks.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(item) = lock.get_mut(&id) {
            item.buffer.push_chunk(chunk);
            Some(item.update_tx.clone())
        } else {
            None
        }
    };
    if let Some(tx) = tx {
        tx.send_modify(|n| *n = n.wrapping_add(1));
    }
}

async fn terminate_child_tree(
    child: &mut tokio::process::Child,
    pid: Option<u32>,
) -> std::io::Result<std::process::ExitStatus> {
    if let Some(p) = pid {
        signal_process_group(p, false);
    }
    let res = match tokio::time::timeout(Duration::from_millis(SIGTERM_GRACE_MS), child.wait()).await
    {
        Ok(res) => res,
        Err(_) => {
            if let Some(p) = pid {
                signal_process_group(p, true);
            }
            let _ = child.start_kill();
            child.wait().await
        }
    };
    if let Some(p) = pid {
        signal_process_group(p, true);
    }
    res
}

pub(super) fn signal_process_group(pgid: u32, force_kill: bool) {
    #[cfg(unix)]
    {
        if pgid == 0 {
            return;
        }
        let sig_name = if force_kill { "KILL" } else { "TERM" };
        let target = format!("-{}", pgid);
        let ok = std::process::Command::new("kill")
            .args(["-s", sig_name, "--", &target])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .map(|s| s.success())
            .unwrap_or(false);

        if !ok {
            let _ = std::process::Command::new("sh")
                .args([
                    "-c",
                    &format!("kill -{} -- -{} 2>/dev/null || kill -{} -{} 2>/dev/null || true", sig_name, pgid, sig_name, pgid),
                ])
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status();
        }
    }
    #[cfg(not(unix))]
    {
        let _ = (pgid, force_kill);
    }
}
