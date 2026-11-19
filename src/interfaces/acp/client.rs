//! Async JSON-RPC 2.0 stdio client untuk subprocess agen ACP (`agy acp`, dll.).

use super::protocol::{
    build_notification, build_request, build_response_error, build_response_ok,
    parse_incoming_jsonrpc, IncomingJsonRpc, JsonRpcError,
};
use crate::common::BoxError;
use serde_json::Value;
use std::collections::{HashMap, VecDeque};
use std::path::PathBuf;
use std::process::Stdio;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, Command};
use tokio::sync::{mpsc, oneshot, Mutex as AsyncMutex};

const MAX_STDERR_LINES: usize = 64;

/// Konfigurasi peluncuran subprocess agen ACP.
#[derive(Debug, Clone)]
pub struct AcpLaunchConfig {
    pub command: PathBuf,
    pub args: Vec<String>,
    pub cwd: PathBuf,
    pub env: Vec<(String, String)>,
}

/// Event asinkron dari agen ACP ke layer bridge (`session/update`, `session/request_permission`, atau `Closed`).
#[derive(Debug, Clone)]
pub enum AcpAgentEvent {
    Notification {
        method: String,
        params: Value,
    },
    Request {
        id: Value,
        method: String,
        params: Value,
    },
    Closed {
        stderr_tail: Vec<String>,
    },
}

type PendingMap = Arc<Mutex<HashMap<String, oneshot::Sender<Result<Value, JsonRpcError>>>>>;

fn rpc_id_key(id: &Value) -> String {
    match id {
        Value::Number(n) => format!("n:{}", n),
        Value::String(s) => format!("s:{}", s),
        other => other.to_string(),
    }
}

/// Client JSON-RPC 2.0 2-arah untuk subprocess ACP.
pub struct AcpClient {
    stdin: Arc<AsyncMutex<tokio::process::ChildStdin>>,
    child: Arc<AsyncMutex<Child>>,
    pending: PendingMap,
    next_id: AtomicU64,
    stderr_lines: Arc<Mutex<VecDeque<String>>>,
}

impl AcpClient {
    /// Luncurkan subprocess agen ACP dan mulai reader task untuk `stdout` (JSON-RPC) serta `stderr`.
    pub fn spawn(
        config: AcpLaunchConfig,
    ) -> Result<(Self, mpsc::UnboundedReceiver<AcpAgentEvent>), BoxError> {
        let mut cmd = Command::new(&config.command);
        cmd.args(&config.args)
            .current_dir(&config.cwd)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        for (k, v) in &config.env {
            cmd.env(k, v);
        }

        let mut child = cmd.spawn().map_err(|e| {
            format!(
                "Gagal meluncurkan agen ACP '{}': {}",
                config.command.display(),
                e
            )
        })?;

        let stdin = child
            .stdin
            .take()
            .ok_or("Gagal membuka stdin subprocess ACP")?;
        let stdout = child
            .stdout
            .take()
            .ok_or("Gagal membuka stdout subprocess ACP")?;
        let stderr = child.stderr.take();

        let pending: PendingMap = Arc::new(Mutex::new(HashMap::new()));
        let stderr_lines: Arc<Mutex<VecDeque<String>>> =
            Arc::new(Mutex::new(VecDeque::with_capacity(MAX_STDERR_LINES)));
        let (event_tx, event_rx) = mpsc::unbounded_channel();

        if let Some(err_pipe) = stderr {
            let err_buf = Arc::clone(&stderr_lines);
            tokio::spawn(async move {
                let mut reader = BufReader::new(err_pipe).lines();
                while let Ok(Some(line)) = reader.next_line().await {
                    let mut q = err_buf.lock().unwrap_or_else(|e| e.into_inner());
                    if q.len() >= MAX_STDERR_LINES {
                        q.pop_front();
                    }
                    q.push_back(line);
                }
            });
        }

        let pending_reader = Arc::clone(&pending);
        let stderr_reader = Arc::clone(&stderr_lines);
        tokio::spawn(async move {
            let mut reader = BufReader::new(stdout).lines();
            while let Ok(Some(line)) = reader.next_line().await {
                let trimmed = line.trim();
                if trimmed.is_empty() {
                    continue;
                }
                match parse_incoming_jsonrpc(trimmed) {
                    Ok(IncomingJsonRpc::Response { id, result, error }) => {
                        let key = rpc_id_key(&id);
                        let tx_opt = pending_reader
                            .lock()
                            .unwrap_or_else(|e| e.into_inner())
                            .remove(&key);
                        if let Some(tx) = tx_opt {
                            let payload = match error {
                                Some(err) => Err(err),
                                None => Ok(result.unwrap_or(Value::Null)),
                            };
                            let _ = tx.send(payload);
                        }
                    }
                    Ok(IncomingJsonRpc::Notification { method, params }) => {
                        let _ = event_tx.send(AcpAgentEvent::Notification { method, params });
                    }
                    Ok(IncomingJsonRpc::Request { id, method, params }) => {
                        let _ = event_tx.send(AcpAgentEvent::Request { id, method, params });
                    }
                    Err(_) => {
                        // Abaikan baris non-JSON yang tak sengaja tercetak oleh subprocess
                    }
                }
            }

            // Subprocess stdout EOF -> gagalkan seluruh pending request agar tidak hang
            let drained: Vec<_> = pending_reader
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .drain()
                .map(|(_, tx)| tx)
                .collect();
            for tx in drained {
                let _ = tx.send(Err(JsonRpcError {
                    code: -32000,
                    message: "Proses agen ACP berhenti sebelum membalas request".to_string(),
                    data: None,
                }));
            }
            let tail: Vec<String> = stderr_reader
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .iter()
                .cloned()
                .collect();
            let _ = event_tx.send(AcpAgentEvent::Closed { stderr_tail: tail });
        });

        Ok((
            Self {
                stdin: Arc::new(AsyncMutex::new(stdin)),
                child: Arc::new(AsyncMutex::new(child)),
                pending,
                next_id: AtomicU64::new(1),
                stderr_lines,
            },
            event_rx,
        ))
    }

    async fn write_frame(&self, frame: &Value) -> Result<(), BoxError> {
        let mut line = serde_json::to_string(frame)?;
        line.push('\n');
        let mut stdin = self.stdin.lock().await;
        stdin.write_all(line.as_bytes()).await?;
        stdin.flush().await?;
        Ok(())
    }

    /// Kirim JSON-RPC Request (Client -> Agent) dan kembalikan `oneshot::Receiver` untuk balasannya.
    pub async fn send_request(
        &self,
        method: &str,
        params: Value,
    ) -> Result<oneshot::Receiver<Result<Value, JsonRpcError>>, BoxError> {
        let id = self.next_id.fetch_add(1, Ordering::SeqCst);
        let key = format!("n:{}", id);
        let (tx, rx) = oneshot::channel();
        {
            self.pending
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .insert(key.clone(), tx);
        }
        let frame = build_request(id, method, params);
        if let Err(e) = self.write_frame(&frame).await {
            self.pending
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .remove(&key);
            return Err(e);
        }
        Ok(rx)
    }

    /// Kirim JSON-RPC Request dan tunggu balasannya dengan batas waktu `timeout`.
    pub async fn request_with_timeout(
        &self,
        method: &str,
        params: Value,
        timeout: Duration,
    ) -> Result<Value, BoxError> {
        let rx = self.send_request(method, params).await?;
        let res = tokio::time::timeout(timeout, rx)
            .await
            .map_err(|_| format!("Timeout menunggu balasan ACP '{}'", method))?
            .map_err(|_| format!("Channel balasan ACP '{}' ditutup", method))?;
        res.map_err(|e| Box::new(e) as BoxError)
    }

    /// Kirim JSON-RPC Notification (Client -> Agent, tanpa menunggu balasan).
    pub async fn notify(&self, method: &str, params: Value) -> Result<(), BoxError> {
        let frame = build_notification(method, params);
        self.write_frame(&frame).await
    }

    /// Balas JSON-RPC Request dari Agent (mis. `session/request_permission`) dengan hasil sukses.
    pub async fn respond_result(&self, id: Value, result: Value) -> Result<(), BoxError> {
        let frame = build_response_ok(id, result);
        self.write_frame(&frame).await
    }

    /// Balas JSON-RPC Request dari Agent dengan error.
    pub async fn respond_error(
        &self,
        id: Value,
        code: i64,
        message: &str,
    ) -> Result<(), BoxError> {
        let frame = build_response_error(id, code, message);
        self.write_frame(&frame).await
    }

    /// Ambil ringkasan baris `stderr` terbaru untuk diagnosa error.
    pub fn recent_stderr(&self) -> Vec<String> {
        self.stderr_lines
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .iter()
            .cloned()
            .collect()
    }

    /// Hentikan subprocess ACP secara bersih.
    pub async fn shutdown(&self) {
        let mut child = self.child.lock().await;
        let _ = child.start_kill();
        let _ = tokio::time::timeout(Duration::from_millis(500), child.wait()).await;
    }
}
