//! Unit & mock-subprocess integration gate tests untuk ACP Client & AcpRpcBridge.

use super::bridge::{AcpBridgeConfig, AcpRpcBridge};
use super::client::AcpLaunchConfig;
use super::protocol::*;
use crate::common::time::epoch_ms;
use serde_json::{json, Value};
use std::io::{self, BufRead, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::sync::mpsc;

struct ChannelWriter {
    tx: mpsc::UnboundedSender<Value>,
    buf: String,
}

impl ChannelWriter {
    fn pair() -> (Arc<Mutex<Box<dyn Write + Send + Sync>>>, mpsc::UnboundedReceiver<Value>) {
        let (tx, rx) = mpsc::unbounded_channel();
        let w: Box<dyn Write + Send + Sync> = Box::new(Self {
            tx,
            buf: String::new(),
        });
        (Arc::new(Mutex::new(w)), rx)
    }
}

impl Write for ChannelWriter {
    fn write(&mut self, data: &[u8]) -> io::Result<usize> {
        let s = String::from_utf8_lossy(data);
        self.buf.push_str(&s);
        while let Some(pos) = self.buf.find('\n') {
            let line: String = self.buf.drain(..=pos).collect();
            let trimmed = line.trim();
            if !trimmed.is_empty() {
                if let Ok(val) = serde_json::from_str::<Value>(trimmed) {
                    let _ = self.tx.send(val);
                }
            }
        }
        Ok(data.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

struct LinePipeReader {
    rx: std::sync::mpsc::Receiver<String>,
    buf: Vec<u8>,
    pos: usize,
}

impl LinePipeReader {
    fn new() -> (std::sync::mpsc::Sender<String>, Self) {
        let (tx, rx) = std::sync::mpsc::channel();
        (
            tx,
            Self {
                rx,
                buf: Vec::new(),
                pos: 0,
            },
        )
    }
}

impl Read for LinePipeReader {
    fn read(&mut self, out: &mut [u8]) -> io::Result<usize> {
        if self.pos >= self.buf.len() {
            match self.rx.recv() {
                Ok(mut line) => {
                    if !line.ends_with('\n') {
                        line.push('\n');
                    }
                    self.buf = line.into_bytes();
                    self.pos = 0;
                }
                Err(_) => return Ok(0),
            }
        }
        let avail = &self.buf[self.pos..];
        let n = avail.len().min(out.len());
        out[..n].copy_from_slice(&avail[..n]);
        self.pos += n;
        Ok(n)
    }
}

impl BufRead for LinePipeReader {
    fn fill_buf(&mut self) -> io::Result<&[u8]> {
        if self.pos >= self.buf.len() {
            match self.rx.recv() {
                Ok(mut line) => {
                    if !line.ends_with('\n') {
                        line.push('\n');
                    }
                    self.buf = line.into_bytes();
                    self.pos = 0;
                }
                Err(_) => return Ok(&[]),
            }
        }
        Ok(&self.buf[self.pos..])
    }

    fn consume(&mut self, amt: usize) {
        self.pos = (self.pos + amt).min(self.buf.len());
    }
}

async fn recv_until<F>(rx: &mut mpsc::UnboundedReceiver<Value>, timeout: Duration, mut pred: F) -> Value
where
    F: FnMut(&Value) -> bool,
{
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        let rem = deadline.saturating_duration_since(tokio::time::Instant::now());
        if rem.is_zero() {
            panic!("timeout menunggu event ACP bridge yang diharapkan");
        }
        let ev = tokio::time::timeout(rem, rx.recv())
            .await
            .expect("timeout recv")
            .expect("channel ditutup");
        if pred(&ev) {
            return ev;
        }
    }
}

fn write_mock_acp_script(dir: &Path, name: &str, python_body: &str) -> PathBuf {
    let path = dir.join(name);
    std::fs::write(&path, python_body).unwrap();
    path
}

#[test]
fn protocol_parses_all_acp_frames_and_permission_options() {
    let notif = parse_incoming_jsonrpc(
        r#"{"jsonrpc":"2.0","method":"session/update","params":{"sessionId":"s1","update":{"sessionUpdate":"agent_thought_chunk","content":{"type":"text","text":"berpikir"}}}}"#,
    )
    .unwrap();
    match notif {
        IncomingJsonRpc::Notification { method, params } => {
            assert_eq!(method, "session/update");
            assert_eq!(
                parse_session_update(&params),
                AcpSessionUpdate::AgentThoughtChunk {
                    text: "berpikir".to_string()
                }
            );
        }
        other => panic!("expected Notification, got {:?}", other),
    }

    let req = parse_incoming_jsonrpc(
        r#"{"jsonrpc":"2.0","id":"perm-9","method":"session/request_permission","params":{"sessionId":"s1","toolCall":{"toolCallId":"tc1","title":"Run tests","kind":"execute","rawInput":{"command":"cargo test"}},"options":[{"optionId":"opt_allow","name":"Allow once","kind":"allow_once"},{"optionId":"opt_reject","name":"Reject","kind":"reject_once"}]}}"#,
    )
    .unwrap();
    match req {
        IncomingJsonRpc::Request { id, method, params } => {
            assert_eq!(id, json!("perm-9"));
            assert_eq!(method, "session/request_permission");
            let perm = parse_permission_request(&params);
            assert_eq!(perm.tool_call_id, "tc1");
            assert_eq!(perm.pick_option_id(true), Some("opt_allow"));
            assert_eq!(perm.pick_option_id(false), Some("opt_reject"));
        }
        other => panic!("expected Request, got {:?}", other),
    }

    assert_eq!(map_acp_tool_kind_to_oopbot("execute"), "bash");
    assert_eq!(map_acp_tool_kind_to_oopbot("edit"), "write");
    assert_eq!(map_acp_tool_kind_to_oopbot("read"), "read");
}

#[tokio::test]
async fn gate_acp_bridge_streams_thought_text_tool_calls_plan_and_permission_roundtrip() {
    let dir = std::env::temp_dir().join(format!(
        "oopbot_acp_gate_stream_{}_{}",
        std::process::id(),
        epoch_ms()
    ));
    std::fs::create_dir_all(&dir).unwrap();

    let script = write_mock_acp_script(
        &dir,
        "mock_acp_agent.py",
        r#"
import json, sys

def send(obj):
    sys.stdout.write(json.dumps(obj) + "\n")
    sys.stdout.flush()

for line in sys.stdin:
    line = line.strip()
    if not line:
        continue
    msg = json.loads(line)
    method = msg.get("method")
    req_id = msg.get("id")
    params = msg.get("params", {})

    if method == "initialize":
        send({
            "jsonrpc": "2.0",
            "id": req_id,
            "result": {
                "protocolVersion": 1,
                "agentCapabilities": {"loadSession": True}
            }
        })
    elif method == "session/new":
        send({
            "jsonrpc": "2.0",
            "id": req_id,
            "result": {
                "sessionId": "acp-sid-101",
                "models": {
                    "currentModelId": "gemini-3.5-pro",
                    "availableModels": [{"modelId": "gemini-3.5-pro", "name": "Gemini 3.5 Pro"}]
                }
            }
        })
    elif method == "session/prompt":
        sid = params.get("sessionId")
        send({"jsonrpc": "2.0", "method": "session/update", "params": {
            "sessionId": sid,
            "update": {"sessionUpdate": "plan", "entries": [
                {"content": "Periksa file", "status": "completed"},
                {"content": "Jalankan tes", "status": "in_progress"}
            ]}
        }})
        send({"jsonrpc": "2.0", "method": "session/update", "params": {
            "sessionId": sid,
            "update": {"sessionUpdate": "agent_thought_chunk", "content": {"type": "text", "text": "Mengecek perintah dulu."}}
        }})
        send({"jsonrpc": "2.0", "method": "session/update", "params": {
            "sessionId": sid,
            "update": {"sessionUpdate": "tool_call", "toolCallId": "tc-1", "title": "echo ok", "kind": "execute", "rawInput": {"command": "echo ok"}}
        }})
        # Minta izin blocking ke client
        send({"jsonrpc": "2.0", "id": "perm-req-1", "method": "session/request_permission", "params": {
            "sessionId": sid,
            "toolCall": {"toolCallId": "tc-1", "title": "echo ok", "kind": "execute", "rawInput": {"command": "echo ok"}},
            "options": [
                {"optionId": "allow_1", "name": "Allow once", "kind": "allow_once"},
                {"optionId": "reject_1", "name": "Reject", "kind": "reject_once"}
            ]
        }})
        # Tunggu jawaban permission dari client
        perm_resp_line = sys.stdin.readline()
        perm_resp = json.loads(perm_resp_line)
        chosen = perm_resp.get("result", {}).get("outcome", {}).get("optionId", "")
        send({"jsonrpc": "2.0", "method": "session/update", "params": {
            "sessionId": sid,
            "update": {"sessionUpdate": "tool_call_update", "toolCallId": "tc-1", "status": "completed", "content": {"type": "text", "text": "PERMITTED:" + chosen}}
        }})
        send({"jsonrpc": "2.0", "method": "session/update", "params": {
            "sessionId": sid,
            "update": {"sessionUpdate": "agent_message_chunk", "content": {"type": "text", "text": "Halo dari ACP!"}}
        }})
        send({"jsonrpc": "2.0", "id": req_id, "result": {"stopReason": "end_turn"}})
"#,
    );

    let (writer, mut out_rx) = ChannelWriter::pair();
    let session_file = dir.join("acp_sess.jsonl");
    let cfg = AcpBridgeConfig {
        engine_id: "antigravity".to_string(),
        display_name: "Antigravity".to_string(),
        config_dir: dir.clone(),
        cwd: dir.clone(),
        session_path: Some(session_file.clone()),
        launch: AcpLaunchConfig {
            command: PathBuf::from("python3"),
            args: vec![script.to_string_lossy().to_string()],
            cwd: dir.clone(),
            env: vec![],
        },
    };

    let bridge = AcpRpcBridge::start(cfg, writer).await.unwrap();
    let (cmd_tx, reader) = LinePipeReader::new();
    let loop_handle = tokio::spawn(async move { bridge.run_loop_with_input(reader).await.unwrap() });

    let hello = recv_until(&mut out_rx, Duration::from_secs(3), |v| {
        v["type"] == "oopbot_channel" && v["topic"] == "oopbot:hello"
    })
    .await;
    assert_eq!(hello["data"]["engine"], "acp:antigravity");

    cmd_tx
        .send(json!({ "id": "st1", "type": "get_state" }).to_string())
        .unwrap();
    let st = recv_until(&mut out_rx, Duration::from_secs(2), |v| {
        v["type"] == "response" && v["id"] == "st1"
    })
    .await;
    assert_eq!(st["data"]["model"]["provider"], "acp:antigravity");
    assert_eq!(st["data"]["model"]["id"], "gemini-3.5-pro");

    // Kirim prompt -> harus memancarkan plan widget, thinking, tool_execution_start, lalu tool_approval
    cmd_tx
        .send(json!({ "id": "p1", "type": "prompt", "message": "jalankan tes" }).to_string())
        .unwrap();

    let mut saw_plan = false;
    let mut saw_thought = false;
    let mut saw_tool_start = false;
    let approval_req_id: String;
    loop {
        let ev = tokio::time::timeout(Duration::from_secs(4), out_rx.recv())
            .await
            .expect("timeout menunggu tool_approval")
            .expect("channel ditutup");
        if ev["type"] == "extension_ui_request" && ev["method"] == "setWidget" {
            saw_plan = true;
        }
        if ev["type"] == "message_update"
            && ev["assistantMessageEvent"]["type"] == "thinking_delta"
        {
            saw_thought = true;
        }
        if ev["type"] == "tool_execution_start" && ev["toolName"] == "bash" {
            saw_tool_start = true;
        }
        if ev["type"] == "oopbot_channel" && ev["topic"] == "tool_approval" {
            approval_req_id = ev["data"]["requestId"].as_str().unwrap().to_string();
            break;
        }
    }
    assert!(saw_plan, "GATE: plan ACP harus tampil sebagai setWidget todos");
    assert!(saw_thought, "GATE: thought chunk ACP harus dipetakan ke thinking_delta");
    assert!(saw_tool_start, "GATE: tool_call execute harus dipetakan ke bash");

    // Balas tool_approval -> mock ACP menyelesaikan tool + teks akhir
    cmd_tx
        .send(
            json!({
                "type": "oopbot_channel",
                "topic": "tool_approval",
                "data": { "requestId": approval_req_id, "approved": true }
            })
            .to_string(),
        )
        .unwrap();

    let tool_end = recv_until(&mut out_rx, Duration::from_secs(3), |v| {
        v["type"] == "tool_execution_end"
    })
    .await;
    assert_eq!(tool_end["isError"], false);
    assert_eq!(tool_end["result"]["content"][0]["text"], "PERMITTED:allow_1");

    let final_msg = recv_until(&mut out_rx, Duration::from_secs(3), |v| {
        v["type"] == "message_end"
            && v["message"]["role"] == "assistant"
            && v["message"]["stopReason"] == "stop"
    })
    .await;
    assert_eq!(final_msg["message"]["content"][0]["text"], "Halo dari ACP!");

    let settled = recv_until(&mut out_rx, Duration::from_secs(3), |v| {
        v["type"] == "agent_settled"
    })
    .await;
    assert_eq!(settled["aborted"], false);

    drop(cmd_tx);
    let _ = tokio::time::timeout(Duration::from_secs(2), loop_handle).await;

    // Pastikan JSONL mirror menyimpan acpSessionId & riwayat pesan untuk offline/reload
    let raw_jsonl = std::fs::read_to_string(&session_file).unwrap();
    assert!(raw_jsonl.contains("acp-sid-101"));
    assert!(raw_jsonl.contains("Halo dari ACP!"));

    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn gate_acp_bridge_abort_sends_session_cancel_and_session_load_restores_id() {
    let dir = std::env::temp_dir().join(format!(
        "oopbot_acp_gate_cancel_{}_{}",
        std::process::id(),
        epoch_ms()
    ));
    std::fs::create_dir_all(&dir).unwrap();

    let session_file = dir.join("reload_acp.jsonl");
    std::fs::write(
        &session_file,
        format!(
            "{}\n",
            json!({
                "type": "session",
                "version": 3,
                "id": "oopbot-acp-existing",
                "engine": "acp:antigravity",
                "acpSessionId": "acp-prev-777",
                "timestamp": "2026-11-19T00:00:00Z",
                "cwd": dir.to_string_lossy(),
            })
        ),
    )
    .unwrap();

    let script = write_mock_acp_script(
        &dir,
        "mock_acp_cancel.py",
        r#"
import json, sys

def send(obj):
    sys.stdout.write(json.dumps(obj) + "\n")
    sys.stdout.flush()

active_prompt_id = None
loaded_sid = None

for line in sys.stdin:
    line = line.strip()
    if not line:
        continue
    msg = json.loads(line)
    method = msg.get("method")
    req_id = msg.get("id")
    params = msg.get("params", {})

    if method == "initialize":
        send({"jsonrpc": "2.0", "id": req_id, "result": {
            "protocolVersion": 1,
            "agentCapabilities": {"loadSession": True}
        }})
    elif method == "session/load":
        loaded_sid = params.get("sessionId")
        send({"jsonrpc": "2.0", "id": req_id, "result": {"sessionId": loaded_sid}})
    elif method == "session/prompt":
        active_prompt_id = req_id
        send({"jsonrpc": "2.0", "method": "session/update", "params": {
            "sessionId": loaded_sid,
            "update": {"sessionUpdate": "agent_message_chunk", "content": {"type": "text", "text": "LOADED:" + str(loaded_sid)}}
        }})
    elif method == "session/cancel":
        if active_prompt_id is not None:
            send({"jsonrpc": "2.0", "id": active_prompt_id, "result": {"stopReason": "cancelled"}})
            active_prompt_id = None
"#,
    );

    let (writer, mut out_rx) = ChannelWriter::pair();
    let cfg = AcpBridgeConfig {
        engine_id: "antigravity".to_string(),
        display_name: "Antigravity".to_string(),
        config_dir: dir.clone(),
        cwd: dir.clone(),
        session_path: Some(session_file.clone()),
        launch: AcpLaunchConfig {
            command: PathBuf::from("python3"),
            args: vec![script.to_string_lossy().to_string()],
            cwd: dir.clone(),
            env: vec![],
        },
    };

    let bridge = AcpRpcBridge::start(cfg, writer).await.unwrap();
    let (cmd_tx, reader) = LinePipeReader::new();
    let loop_handle = tokio::spawn(async move { bridge.run_loop_with_input(reader).await.unwrap() });

    let _ = recv_until(&mut out_rx, Duration::from_secs(2), |v| {
        v["topic"] == "oopbot:hello"
    })
    .await;

    cmd_tx
        .send(json!({ "id": "p_slow", "type": "prompt", "message": "tunggu cancel" }).to_string())
        .unwrap();

    let delta = recv_until(&mut out_rx, Duration::from_secs(3), |v| {
        v["type"] == "message_update" && v["assistantMessageEvent"]["type"] == "text_delta"
    })
    .await;
    assert_eq!(
        delta["assistantMessageEvent"]["delta"], "LOADED:acp-prev-777",
        "GATE: bridge wajib memulihkan acpSessionId lama via session/load"
    );

    cmd_tx
        .send(json!({ "id": "ab1", "type": "abort" }).to_string())
        .unwrap();
    let _ = recv_until(&mut out_rx, Duration::from_secs(2), |v| {
        v["type"] == "response" && v["id"] == "ab1"
    })
    .await;

    let settled = recv_until(&mut out_rx, Duration::from_secs(3), |v| {
        v["type"] == "agent_settled"
    })
    .await;
    assert_eq!(
        settled["aborted"], true,
        "GATE: abort wajib mengirim session/cancel dan settle dengan aborted=true"
    );

    drop(cmd_tx);
    let _ = tokio::time::timeout(Duration::from_secs(2), loop_handle).await;
    let _ = std::fs::remove_dir_all(&dir);
}
