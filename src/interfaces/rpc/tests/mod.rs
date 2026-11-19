mod protocol;
mod providers_config;
mod queues_wakeup;
mod sessions_branch;
mod ui_dialogs;

pub(super) use super::*;
#[allow(unused_imports)]
pub(super) use crate::agent::{AgentEvent, Approver, EventSink, StepsOutcome, ToolStatus};
#[allow(unused_imports)]
pub(super) use crate::common::fs::read_json_file;
#[allow(unused_imports)]
pub(super) use crate::common::time::{epoch_ms, iso_now, parse_iso_to_epoch_ms};
#[allow(unused_imports)]
pub(super) use crate::config::{ModelCatalog, ModelInfo};
#[allow(unused_imports)]
pub(super) use crate::plugins::{BackgroundPlugin, PluginRegistry};
#[allow(unused_imports)]
pub(super) use crate::tools::ToolRegistry;
#[allow(unused_imports)]
pub(super) use serde_json::{json, Value};
#[allow(unused_imports)]
pub(super) use std::collections::HashMap;
#[allow(unused_imports)]
pub(super) use std::io::{self, Cursor};
#[allow(unused_imports)]
pub(super) use std::path::{Path, PathBuf};
#[allow(unused_imports)]
pub(super) use std::sync::atomic::{AtomicU64, Ordering};
#[allow(unused_imports)]
pub(super) use std::sync::{Arc, Mutex};

pub(super) struct TempConfigGuard(pub(super) PathBuf);
impl TempConfigGuard {
    pub(super) fn new(prefix: &str) -> Self {
        static SEQ: AtomicU64 = AtomicU64::new(1);
        let dir = std::env::temp_dir().join(format!(
            "{}_{}_{}_{}",
            prefix,
            std::process::id(),
            epoch_ms(),
            SEQ.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = std::fs::create_dir_all(&dir);
        Self(dir)
    }
    pub(super) fn path(&self) -> &Path {
        &self.0
    }
}
impl Drop for TempConfigGuard {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

// -------------------------------------------------------------------------
// Harness & Regression Tests for REG-002 & REG-003 (Regression-Aware.md)
// -------------------------------------------------------------------------

pub(super) struct OutputSinkResetGuard;
impl Drop for OutputSinkResetGuard {
    fn drop(&mut self) {
        set_output_sink(None);
    }
}

pub(super) struct ChannelWriter {
    tx: tokio::sync::mpsc::UnboundedSender<Value>,
    partial: Vec<u8>,
}

impl io::Write for ChannelWriter {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.partial.extend_from_slice(buf);
        while let Some(pos) = self.partial.iter().position(|&b| b == b'\n') {
            let line_bytes: Vec<u8> = self.partial.drain(..=pos).collect();
            let line = String::from_utf8_lossy(&line_bytes);
            let trimmed = line.trim();
            if !trimmed.is_empty() {
                if let Ok(val) = serde_json::from_str::<Value>(trimmed) {
                    let _ = self.tx.send(val);
                }
            }
        }
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

pub(super) fn install_channel_output_sink() -> (
    OutputSinkResetGuard,
    tokio::sync::mpsc::UnboundedReceiver<Value>,
) {
    let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
    let writer = ChannelWriter {
        tx,
        partial: Vec::new(),
    };
    let sink: OutputSink = Arc::new(Mutex::new(Box::new(writer)));
    set_output_sink(Some(sink));
    (OutputSinkResetGuard, rx)
}

pub(super) struct TestLineReader {
    rx: std::sync::mpsc::Receiver<Vec<u8>>,
    cur: Vec<u8>,
    pos: usize,
}

impl TestLineReader {
    pub(super) fn new() -> (std::sync::mpsc::Sender<Vec<u8>>, Self) {
        let (tx, rx) = std::sync::mpsc::channel();
        (
            tx,
            Self {
                rx,
                cur: Vec::new(),
                pos: 0,
            },
        )
    }
}

impl io::Read for TestLineReader {
    fn read(&mut self, out: &mut [u8]) -> io::Result<usize> {
        while self.pos >= self.cur.len() {
            match self.rx.recv() {
                Ok(next) => {
                    self.cur = next;
                    self.pos = 0;
                }
                Err(_) => return Ok(0),
            }
        }
        let avail = self.cur.len() - self.pos;
        let n = avail.min(out.len());
        out[..n].copy_from_slice(&self.cur[self.pos..self.pos + n]);
        self.pos += n;
        Ok(n)
    }
}

pub(super) fn send_cmd(tx: &std::sync::mpsc::Sender<Vec<u8>>, cmd: Value) {
    let mut s = serde_json::to_string(&cmd).unwrap();
    s.push('\n');
    tx.send(s.into_bytes()).unwrap();
}

pub(super) async fn recv_matching<F>(
    rx: &mut tokio::sync::mpsc::UnboundedReceiver<Value>,
    timeout: std::time::Duration,
    pred: F,
) -> Value
where
    F: Fn(&Value) -> bool,
{
    tokio::time::timeout(timeout, async {
        while let Some(v) = rx.recv().await {
            if pred(&v) {
                return v;
            }
        }
        panic!("RPC output channel closed while waiting for matching event");
    })
    .await
    .expect("Timed out waiting for matching RPC event/response")
}

pub(super) async fn recv_response(
    rx: &mut tokio::sync::mpsc::UnboundedReceiver<Value>,
    id: &str,
    timeout: std::time::Duration,
) -> Value {
    recv_matching(rx, timeout, |v| {
        v["type"] == "response" && v["id"].as_str() == Some(id)
    })
    .await
}

#[derive(Clone, Debug)]
pub(super) struct MockHttpReq {

    pub(super) path: String,
    pub(super) body: Value,
}

pub(super) async fn spawn_mock_llm_server<F, Fut>(
    handler: F,
) -> (
    String,
    Arc<Mutex<Vec<MockHttpReq>>>,
    tokio::task::JoinHandle<()>,
)
where
    F: Fn(usize, MockHttpReq) -> Fut + Send + Sync + 'static,
    Fut: std::future::Future<Output = (u16, Value)> + Send + 'static,
{
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let base_url = format!("http://127.0.0.1:{}", addr.port());
    let recorded = Arc::new(Mutex::new(Vec::new()));
    let recorded_bg = Arc::clone(&recorded);
    let handler = Arc::new(handler);
    let seq = Arc::new(AtomicU64::new(0));

    let handle = tokio::spawn(async move {
        loop {
            let Ok((mut socket, _)) = listener.accept().await else {
                break;
            };
            let recorded_req = Arc::clone(&recorded_bg);
            let handler_req = Arc::clone(&handler);
            let seq_req = Arc::clone(&seq);

            tokio::spawn(async move {
                let mut buf = Vec::new();
                let mut tmp = [0u8; 4096];
                let header_end;
                loop {
                    let Ok(n) = socket.read(&mut tmp).await else {
                        return;
                    };
                    if n == 0 {
                        return;
                    }
                    buf.extend_from_slice(&tmp[..n]);
                    if let Some(pos) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
                        header_end = pos + 4;
                        break;
                    }
                }

                let head = String::from_utf8_lossy(&buf[..header_end]);
                let mut lines = head.lines();
                let req_line = lines.next().unwrap_or("");
                let path = req_line.split_whitespace().nth(1).unwrap_or("/").to_string();

                let mut content_len = 0usize;
                for line in lines {
                    if let Some((k, v)) = line.split_once(':') {
                        if k.eq_ignore_ascii_case("content-length") {
                            content_len = v.trim().parse().unwrap_or(0);
                        }
                    }
                }

                while buf.len() < header_end + content_len {
                    let Ok(n) = socket.read(&mut tmp).await else {
                        return;
                    };
                    if n == 0 {
                        break;
                    }
                    buf.extend_from_slice(&tmp[..n]);
                }

                let body_bytes = &buf[header_end..(header_end + content_len).min(buf.len())];
                let body: Value = serde_json::from_slice(body_bytes).unwrap_or(Value::Null);
                let req = MockHttpReq {
                    path: path.clone(),
                    body,
                };
                if !path.ends_with("/models") && !path.contains("/models?") {
                    recorded_req.lock().unwrap().push(req.clone());
                }

                let call_idx = if path.ends_with("/models") || path.contains("/models?") {
                    usize::MAX
                } else {
                    seq_req.fetch_add(1, Ordering::SeqCst) as usize
                };

                let (status, resp_json) = handler_req(call_idx, req).await;
                let resp_body = serde_json::to_string(&resp_json).unwrap();
                let reason = if status == 200 { "OK" } else { "Error" };
                let http = format!(
                    "HTTP/1.1 {} {}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                    status,
                    reason,
                    resp_body.len(),
                    resp_body
                );
                let _ = socket.write_all(http.as_bytes()).await;
                let _ = socket.shutdown().await;
            });
        }
    });

    (base_url, recorded, handle)
}

pub(super) fn write_mock_provider_config(
    dir: &Path,
    provider: &str,
    api: &str,
    model_id: &str,
    base_url: &str,
    api_key: &str,
) {
    std::fs::write(
        dir.join("auth.json"),
        serde_json::to_string_pretty(&json!({
            provider: { "type": "api_key", "apiKey": api_key }
        }))
        .unwrap(),
    )
    .unwrap();
    std::fs::write(
        dir.join("models.json"),
        serde_json::to_string_pretty(&json!({
            "providers": {
                provider: {
                    "baseUrl": base_url,
                    "api": api,
                    "scannedApiKey": api_key,
                    "models": [{
                        "id": model_id,
                        "name": model_id,
                        "reasoning": false,
                        "contextWindow": 128000,
                        "maxTokens": 8192
                    }]
                }
            }
        }))
        .unwrap(),
    )
    .unwrap();
    std::fs::write(
        dir.join("settings.json"),
        serde_json::to_string_pretty(&json!({
            "defaultProvider": provider,
            "defaultModel": model_id,
            "defaultThinkingLevel": "off"
        }))
        .unwrap(),
    )
    .unwrap();
}

pub(super) fn gemini_text_resp(text: &str, inp: u64, out: u64) -> Value {
    json!({
        "candidates": [{
            "content": {
                "role": "model",
                "parts": [{ "text": text }]
            }
        }],
        "usageMetadata": {
            "promptTokenCount": inp,
            "candidatesTokenCount": out,
            "thoughtsTokenCount": 0
        }
    })
}
