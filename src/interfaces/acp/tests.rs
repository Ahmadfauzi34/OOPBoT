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

#[tokio::test]
async fn gate_acp_bridge_model_and_thinking_switching_via_variant_and_config_options() {
    // Kasus 1: Agen ACP hanya mengiklankan `models.availableModels` dengan varian level `(Low)/(Medium)/(High)`
    let dir = std::env::temp_dir().join(format!("oopbot_acp_models_{}", epoch_ms()));
    std::fs::create_dir_all(&dir).unwrap();
    let script1 = write_mock_acp_script(
        &dir,
        "mock_acp_models.py",
        r#"
import sys, json

def send(obj):
    sys.stdout.write(json.dumps(obj) + "\n")
    sys.stdout.flush()

sid = "acp-mod-1"
for line in sys.stdin:
    line = line.strip()
    if not line:
        continue
    msg = json.loads(line)
    method = msg.get("method")
    req_id = msg.get("id")
    params = msg.get("params", {})

    if method == "initialize":
        send({"jsonrpc": "2.0", "id": req_id, "result": {"protocolVersion": 1}})
    elif method == "session/new":
        send({"jsonrpc": "2.0", "id": req_id, "result": {
            "sessionId": sid,
            "models": {
                "currentModelId": "neon-high",
                "availableModels": [
                    {"modelId": "neon-low", "name": "Gemini 4 Neon (Low)"},
                    {"modelId": "neon-medium", "name": "Gemini 4 Neon (Medium)"},
                    {"modelId": "neon-high", "name": "Gemini 4 Neon (High)"},
                    {"modelId": "carbon-medium", "name": "Gemini 4 Carbon (Medium)"}
                ]
            }
        }})
    elif method == "session/set_model":
        send({"jsonrpc": "2.0", "id": req_id, "result": {"modelId": params.get("modelId")}})
"#,
    );

    let (writer, mut out_rx) = ChannelWriter::pair();
    let cfg = AcpBridgeConfig {
        engine_id: "antigravity".to_string(),
        display_name: "Antigravity".to_string(),
        config_dir: dir.clone(),
        cwd: dir.clone(),
        session_path: None,
        launch: AcpLaunchConfig {
            command: PathBuf::from("python3"),
            args: vec![script1.to_string_lossy().to_string()],
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

    // 1. State awal inferred dari `Gemini 4 Neon (High)` -> thinkingLevel == "high"
    cmd_tx
        .send(json!({ "id": "st1", "type": "get_state" }).to_string())
        .unwrap();
    let st1 = recv_until(&mut out_rx, Duration::from_secs(2), |v| {
        v["type"] == "response" && v["id"] == "st1"
    })
    .await;
    assert_eq!(st1["data"]["model"]["id"], "neon-high");
    assert_eq!(st1["data"]["model"]["api"], "acp");
    assert_eq!(st1["data"]["thinkingLevel"], "high");

    // 2. `get_available_models` mengembalikan semua 4 model dari `availableModels`
    cmd_tx
        .send(json!({ "id": "gm1", "type": "get_available_models" }).to_string())
        .unwrap();
    let gm1 = recv_until(&mut out_rx, Duration::from_secs(2), |v| {
        v["type"] == "response" && v["id"] == "gm1"
    })
    .await;
    assert_eq!(gm1["success"], true);
    assert_eq!(gm1["data"]["models"].as_array().unwrap().len(), 4);

    // 3. `get_available_thinking_levels` mendeteksi varian sibling Neon (`low`, `medium`, `high`)
    cmd_tx
        .send(json!({ "id": "gt1", "type": "get_available_thinking_levels" }).to_string())
        .unwrap();
    let gt1 = recv_until(&mut out_rx, Duration::from_secs(2), |v| {
        v["type"] == "response" && v["id"] == "gt1"
    })
    .await;
    assert_eq!(gt1["success"], true);
    assert_eq!(gt1["data"]["levels"], json!(["low", "medium", "high"]));

    // 4. `set_thinking_level("low")` otomatis memanggil `session/set_model("neon-low")`
    cmd_tx
        .send(json!({ "id": "stl1", "type": "set_thinking_level", "level": "low" }).to_string())
        .unwrap();
    let stl1 = recv_until(&mut out_rx, Duration::from_secs(2), |v| {
        v["type"] == "response" && v["id"] == "stl1"
    })
    .await;
    assert_eq!(stl1["success"], true);

    cmd_tx
        .send(json!({ "id": "st2", "type": "get_state" }).to_string())
        .unwrap();
    let st2 = recv_until(&mut out_rx, Duration::from_secs(2), |v| {
        v["type"] == "response" && v["id"] == "st2"
    })
    .await;
    assert_eq!(st2["data"]["model"]["id"], "neon-low");
    assert_eq!(st2["data"]["thinkingLevel"], "low");

    // 5. `set_model("carbon-medium")` mengganti model via `session/set_model` dan sync thinking level
    cmd_tx
        .send(
            json!({
                "id": "sm1",
                "type": "set_model",
                "provider": "acp:antigravity",
                "modelId": "carbon-medium"
            })
            .to_string(),
        )
        .unwrap();
    let sm1 = recv_until(&mut out_rx, Duration::from_secs(2), |v| {
        v["type"] == "response" && v["id"] == "sm1"
    })
    .await;
    assert_eq!(sm1["success"], true);
    assert_eq!(sm1["data"]["id"], "carbon-medium");
    assert_eq!(sm1["data"]["name"], "Gemini 4 Carbon (Medium)");

    drop(cmd_tx);
    let _ = tokio::time::timeout(Duration::from_secs(2), loop_handle).await;

    // Kasus 2: Agen ACP mengekspos `configOptions` dengan category `thought_level` -> `session/set_config_option`
    let script2 = write_mock_acp_script(
        &dir,
        "mock_acp_cfg.py",
        r#"
import sys, json

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
        send({"jsonrpc": "2.0", "id": req_id, "result": {"protocolVersion": 1}})
    elif method == "session/new":
        send({"jsonrpc": "2.0", "id": req_id, "result": {
            "sessionId": "acp-cfg-1",
            "models": {
                "currentModelId": "gemini-4-pro",
                "availableModels": [{"modelId": "gemini-4-pro", "name": "Gemini 4 Pro"}]
            },
            "configOptions": [{
                "id": "thought",
                "name": "Thinking Level",
                "category": "thought_level",
                "type": "select",
                "currentValue": "medium",
                "options": [
                    {"value": "low", "name": "Low"},
                    {"value": "medium", "name": "Medium"},
                    {"value": "high", "name": "High"}
                ]
            }]
        }})
    elif method == "session/set_config_option":
        val = params.get("value")
        send({"jsonrpc": "2.0", "id": req_id, "result": {
            "configOptions": [{
                "id": "thought",
                "name": "Thinking Level",
                "category": "thought_level",
                "type": "select",
                "currentValue": val,
                "options": [
                    {"value": "low", "name": "Low"},
                    {"value": "medium", "name": "Medium"},
                    {"value": "high", "name": "High"}
                ]
            }]
        }})
"#,
    );

    let (writer2, mut out_rx2) = ChannelWriter::pair();
    let cfg2 = AcpBridgeConfig {
        engine_id: "antigravity".to_string(),
        display_name: "Antigravity".to_string(),
        config_dir: dir.clone(),
        cwd: dir.clone(),
        session_path: None,
        launch: AcpLaunchConfig {
            command: PathBuf::from("python3"),
            args: vec![script2.to_string_lossy().to_string()],
            cwd: dir.clone(),
            env: vec![],
        },
    };

    let bridge2 = AcpRpcBridge::start(cfg2, writer2).await.unwrap();
    let (cmd_tx2, reader2) = LinePipeReader::new();
    let loop_handle2 =
        tokio::spawn(async move { bridge2.run_loop_with_input(reader2).await.unwrap() });
    let _ = recv_until(&mut out_rx2, Duration::from_secs(2), |v| {
        v["topic"] == "oopbot:hello"
    })
    .await;

    cmd_tx2
        .send(json!({ "id": "gt2", "type": "get_available_thinking_levels" }).to_string())
        .unwrap();
    let gt2 = recv_until(&mut out_rx2, Duration::from_secs(2), |v| {
        v["type"] == "response" && v["id"] == "gt2"
    })
    .await;
    assert_eq!(gt2["data"]["levels"], json!(["low", "medium", "high"]));

    cmd_tx2
        .send(json!({ "id": "stl2", "type": "set_thinking_level", "level": "high" }).to_string())
        .unwrap();
    let changed = recv_until(&mut out_rx2, Duration::from_secs(2), |v| {
        v["type"] == "thinking_level_changed"
    })
    .await;
    assert_eq!(changed["level"], "high");

    let stl2 = recv_until(&mut out_rx2, Duration::from_secs(2), |v| {
        v["type"] == "response" && v["id"] == "stl2"
    })
    .await;
    assert_eq!(stl2["success"], true);

    drop(cmd_tx2);
    let _ = tokio::time::timeout(Duration::from_secs(2), loop_handle2).await;
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn gate_acp_bridge_fs_and_nonblocking_terminal_callbacks() {
    let dir = std::env::temp_dir().join(format!("oopbot_acp_fsterm_{}", epoch_ms()));
    std::fs::create_dir_all(&dir).unwrap();
    let script = write_mock_acp_script(
        &dir,
        "mock_acp_fsterm.py",
        r#"
import sys, json

def send(obj):
    sys.stdout.write(json.dumps(obj) + "\n")
    sys.stdout.flush()

def recv():
    while True:
        line = sys.stdin.readline()
        if not line:
            return None
        line = line.strip()
        if line:
            return json.loads(line)

rpc_seq = 100
def next_id():
    global rpc_seq
    rpc_seq += 1
    return rpc_seq

def rpc_call(method, params):
    rid = next_id()
    send({"jsonrpc": "2.0", "id": rid, "method": method, "params": params})
    while True:
        msg = recv()
        if msg is None:
            raise RuntimeError("EOF")
        if msg.get("id") == rid:
            if "error" in msg and msg["error"] is not None:
                raise RuntimeError(str(msg["error"]))
            return msg.get("result", {})

sid = "acp-fs-1"
while True:
    msg = recv()
    if msg is None:
        break
    method = msg.get("method")
    req_id = msg.get("id")
    params = msg.get("params", {})

    if method == "initialize":
        caps = params.get("clientCapabilities", {})
        fs_cap = caps.get("fs", {})
        assert fs_cap.get("readTextFile") is True
        assert fs_cap.get("writeTextFile") is True
        assert caps.get("terminal") is True
        send({"jsonrpc": "2.0", "id": req_id, "result": {"protocolVersion": 1}})
    elif method == "session/new":
        send({"jsonrpc": "2.0", "id": req_id, "result": {"sessionId": sid}})
    elif method == "session/prompt":
        rpc_call("fs/write_text_file", {
            "sessionId": sid,
            "path": "sub/hello.txt",
            "content": "baris1\nbaris2\nbaris3\n"
        })
        read_res = rpc_call("fs/read_text_file", {
            "sessionId": sid,
            "path": "sub/hello.txt",
            "line": 2,
            "limit": 1
        })
        t_res = rpc_call("terminal/create", {
            "sessionId": sid,
            "command": "python3",
            "args": ["-c", "import time, sys; print('awal'); sys.stdout.flush(); time.sleep(0.2); print('akhir')"]
        })
        tid = t_res["terminalId"]

        # Kirim wait_for_exit secara non-blocking lalu buktikan bridge tetap melayani fs/read_text_file
        wait_rid = next_id()
        send({"jsonrpc": "2.0", "id": wait_rid, "method": "terminal/wait_for_exit", "params": {"sessionId": sid, "terminalId": tid}})

        # Request sekunder sambil wait_for_exit tertunda
        read2 = rpc_call("fs/read_text_file", {"sessionId": sid, "path": "sub/hello.txt", "line": 3, "limit": 1})

        # Terima hasil wait_for_exit
        exit_res = None
        while exit_res is None:
            m2 = recv()
            if m2.get("id") == wait_rid:
                exit_res = m2.get("result", {})

        out_res = rpc_call("terminal/output", {"sessionId": sid, "terminalId": tid})
        rpc_call("terminal/release", {"sessionId": sid, "terminalId": tid})

        # Uji kill + release pada proses tidur panjang
        t2 = rpc_call("terminal/create", {"sessionId": sid, "command": "python3", "args": ["-c", "import time; time.sleep(30)"]})
        rpc_call("terminal/kill", {"sessionId": sid, "terminalId": t2["terminalId"]})
        rpc_call("terminal/release", {"sessionId": sid, "terminalId": t2["terminalId"]})

        summary = f"{read_res['content']}|{read2['content']}|exit={exit_res.get('exitCode')}|out={out_res.get('output', '').strip()}"
        send({"jsonrpc": "2.0", "method": "session/update", "params": {
            "sessionId": sid,
            "update": {"sessionUpdate": "agent_message_chunk", "content": {"type": "text", "text": summary}}
        }})
        send({"jsonrpc": "2.0", "id": req_id, "result": {"stopReason": "end_turn"}})
"#,
    );

    let (writer, mut out_rx) = ChannelWriter::pair();
    let cfg = AcpBridgeConfig {
        engine_id: "antigravity".to_string(),
        display_name: "Antigravity".to_string(),
        config_dir: dir.clone(),
        cwd: dir.clone(),
        session_path: None,
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
        .send(json!({ "id": "p_fs", "type": "prompt", "message": "uji fs terminal" }).to_string())
        .unwrap();

    let delta = recv_until(&mut out_rx, Duration::from_secs(4), |v| {
        v["type"] == "message_update" && v["assistantMessageEvent"]["type"] == "text_delta"
    })
    .await;
    let text = delta["assistantMessageEvent"]["delta"].as_str().unwrap_or("");
    assert!(text.contains("baris2|baris3|exit=0|"), "delta={text}");
    assert!(text.contains("awal") && text.contains("akhir"), "delta={text}");
    assert_eq!(
        std::fs::read_to_string(dir.join("sub/hello.txt")).unwrap(),
        "baris1\nbaris2\nbaris3\n"
    );

    let _ = recv_until(&mut out_rx, Duration::from_secs(3), |v| {
        v["type"] == "agent_settled"
    })
    .await;

    drop(cmd_tx);
    let _ = tokio::time::timeout(Duration::from_secs(2), loop_handle).await;
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn gate_acp_bridge_extended_rpc_commands_and_updates() {
    let dir = std::env::temp_dir().join(format!("oopbot_acp_ext_{}", epoch_ms()));
    std::fs::create_dir_all(&dir).unwrap();
    let script = write_mock_acp_script(
        &dir,
        "mock_acp_ext.py",
        r#"
import sys, json, time

def send(obj):
    sys.stdout.write(json.dumps(obj) + "\n")
    sys.stdout.flush()

session_counter = 0
active_sid = "acp-s1"

def emit_session_updates(sid):
    send({"jsonrpc": "2.0", "method": "session/update", "params": {
        "sessionId": sid,
        "update": {
            "sessionUpdate": "available_commands_update",
            "availableCommands": [
                {"name": "/review", "description": "Tinjau perubahan", "input": {"hint": "branch"}},
                {"name": "compact", "description": "Ringkas konteks"}
            ]
        }
    }})
    send({"jsonrpc": "2.0", "method": "session/update", "params": {
        "sessionId": sid,
        "update": {"sessionUpdate": "session_info_update", "title": "Sesi Uji ACP"}
    }})
    send({"jsonrpc": "2.0", "method": "session/update", "params": {
        "sessionId": sid,
        "update": {"sessionUpdate": "current_mode_update", "currentModeId": "architect"}
    }})
    send({"jsonrpc": "2.0", "method": "session/update", "params": {
        "sessionId": sid,
        "update": {"sessionUpdate": "usage_update", "used": 1200, "size": 8000, "cost": {"amount": 0.02, "currency": "USD"}}
    }})

def session_payload(sid):
    return {
        "sessionId": sid,
        "models": {
            "currentModelId": "m-alpha",
            "availableModels": [
                {"modelId": "m-alpha", "name": "Model Alpha"},
                {"modelId": "m-beta", "name": "Model Beta"}
            ]
        },
        "modes": {
            "currentModeId": "code",
            "availableModes": [
                {"id": "code", "name": "Code"},
                {"id": "architect", "name": "Architect"}
            ]
        },
        "configOptions": [{
            "id": "thought_level",
            "name": "Thinking",
            "category": "thought_level",
            "type": "select",
            "currentValue": "low",
            "options": [
                {"value": "low", "name": "Low"},
                {"value": "high", "name": "High"}
            ]
        }]
    }

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
    elif method == "session/new":
        session_counter += 1
        active_sid = f"acp-s{session_counter}"
        send({"jsonrpc": "2.0", "id": req_id, "result": session_payload(active_sid)})
        emit_session_updates(active_sid)
    elif method == "session/load":
        active_sid = params.get("sessionId", "acp-s1")
        send({"jsonrpc": "2.0", "id": req_id, "result": session_payload(active_sid)})
    elif method == "session/set_model":
        send({"jsonrpc": "2.0", "id": req_id, "result": {}})
    elif method == "session/set_mode":
        send({"jsonrpc": "2.0", "id": req_id, "result": {}})
    elif method == "session/set_config_option":
        send({"jsonrpc": "2.0", "id": req_id, "result": {
            "configOptions": [{
                "id": "thought_level",
                "name": "Thinking",
                "category": "thought_level",
                "type": "select",
                "currentValue": params.get("value", "low"),
                "options": [
                    {"value": "low", "name": "Low"},
                    {"value": "high", "name": "High"}
                ]
            }]
        }})
    elif method == "session/prompt":
        txt = "".join(b.get("text", "") for b in params.get("prompt", []) if b.get("type") == "text")
        if txt == "slow":
            send({"jsonrpc": "2.0", "method": "session/update", "params": {
                "sessionId": active_sid,
                "update": {"sessionUpdate": "agent_message_chunk", "content": {"type": "text", "text": "working"}}
            }})
            time.sleep(0.25)
            send({"jsonrpc": "2.0", "id": req_id, "result": {
                "stopReason": "end_turn",
                "usage": {"inputTokens": 10, "outputTokens": 20}
            }})
        else:
            send({"jsonrpc": "2.0", "method": "session/update", "params": {
                "sessionId": active_sid,
                "update": {"sessionUpdate": "agent_message_chunk", "content": {"type": "text", "text": f"echo:{txt}"}}
            }})
            send({"jsonrpc": "2.0", "id": req_id, "result": {
                "stopReason": "end_turn",
                "usage": {"inputTokens": 5, "outputTokens": 5}
            }})
"#,
    );

    let (writer, mut out_rx) = ChannelWriter::pair();
    let cfg = AcpBridgeConfig {
        engine_id: "antigravity".to_string(),
        display_name: "Antigravity".to_string(),
        config_dir: dir.clone(),
        cwd: dir.clone(),
        session_path: None,
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

    // 1. Tunggu session_info_update & cek get_commands
    let _ = recv_until(&mut out_rx, Duration::from_secs(2), |v| {
        v["type"] == "session_info_changed" && v["name"] == "Sesi Uji ACP"
    })
    .await;

    cmd_tx
        .send(json!({ "id": "gc", "type": "get_commands" }).to_string())
        .unwrap();
    let gc = recv_until(&mut out_rx, Duration::from_secs(2), |v| {
        v["type"] == "response" && v["id"] == "gc"
    })
    .await;
    assert_eq!(gc["data"]["commands"][0]["name"], "review");
    assert_eq!(gc["data"]["commands"][0]["inputHint"], "branch");
    assert_eq!(gc["data"]["commands"][1]["name"], "compact");

    // 2. set_session_name & catat sessionFile
    cmd_tx
        .send(json!({ "id": "sn", "type": "set_session_name", "name": "Judul Manual" }).to_string())
        .unwrap();
    let _ = recv_until(&mut out_rx, Duration::from_secs(2), |v| {
        v["type"] == "response" && v["id"] == "sn"
    })
    .await;
    cmd_tx
        .send(json!({ "id": "gs1", "type": "get_state" }).to_string())
        .unwrap();
    let gs1 = recv_until(&mut out_rx, Duration::from_secs(2), |v| {
        v["type"] == "response" && v["id"] == "gs1"
    })
    .await;
    assert_eq!(gs1["data"]["sessionName"], "Judul Manual");
    let first_session_file = gs1["data"]["sessionFile"].as_str().unwrap().to_string();

    // 3. steer vs follow_up ordering
    cmd_tx
        .send(json!({ "id": "p_slow", "type": "prompt", "message": "slow" }).to_string())
        .unwrap();
    let _ = recv_until(&mut out_rx, Duration::from_secs(2), |v| {
        v["type"] == "message_update" && v["assistantMessageEvent"]["delta"] == "working"
    })
    .await;
    cmd_tx
        .send(json!({ "id": "fu1", "type": "follow_up", "message": "followup-1" }).to_string())
        .unwrap();
    cmd_tx
        .send(json!({ "id": "st1", "type": "steer", "message": "steer-1" }).to_string())
        .unwrap();
    let fu_resp = recv_until(&mut out_rx, Duration::from_secs(2), |v| {
        v["type"] == "response" && v["id"] == "fu1"
    })
    .await;
    assert_eq!(fu_resp["data"]["disposition"], "queued");
    let st_resp = recv_until(&mut out_rx, Duration::from_secs(2), |v| {
        v["type"] == "response" && v["id"] == "st1"
    })
    .await;
    assert_eq!(st_resp["data"]["disposition"], "queued");

    let d1 = recv_until(&mut out_rx, Duration::from_secs(3), |v| {
        v["type"] == "message_update" && v["assistantMessageEvent"]["delta"] == "echo:steer-1"
    })
    .await;
    assert_eq!(d1["assistantMessageEvent"]["delta"], "echo:steer-1");
    let d2 = recv_until(&mut out_rx, Duration::from_secs(3), |v| {
        v["type"] == "message_update" && v["assistantMessageEvent"]["delta"] == "echo:followup-1"
    })
    .await;
    assert_eq!(d2["assistantMessageEvent"]["delta"], "echo:followup-1");
    let _ = recv_until(&mut out_rx, Duration::from_secs(2), |v| {
        v["type"] == "agent_settled"
    })
    .await;

    // 4. clear_queue & mode toggles
    cmd_tx
        .send(json!({ "id": "p_slow2", "type": "prompt", "message": "slow" }).to_string())
        .unwrap();
    let _ = recv_until(&mut out_rx, Duration::from_secs(2), |v| {
        v["type"] == "message_update" && v["assistantMessageEvent"]["delta"] == "working"
    })
    .await;
    cmd_tx
        .send(json!({ "id": "q_s", "type": "prompt", "message": "q1", "streamingBehavior": "steer" }).to_string())
        .unwrap();
    cmd_tx
        .send(json!({ "id": "q_f", "type": "follow_up", "message": "q2" }).to_string())
        .unwrap();
    let _ = recv_until(&mut out_rx, Duration::from_secs(2), |v| {
        v["type"] == "response" && v["id"] == "q_f"
    })
    .await;
    cmd_tx
        .send(json!({ "id": "cq", "type": "clear_queue" }).to_string())
        .unwrap();
    let cq = recv_until(&mut out_rx, Duration::from_secs(2), |v| {
        v["type"] == "response" && v["id"] == "cq"
    })
    .await;
    assert_eq!(cq["data"]["steering"], json!(["q1"]));
    assert_eq!(cq["data"]["followUp"], json!(["q2"]));
    let _ = recv_until(&mut out_rx, Duration::from_secs(3), |v| {
        v["type"] == "agent_settled"
    })
    .await;

    cmd_tx
        .send(json!({ "id": "ssm", "type": "set_steering_mode", "mode": "all" }).to_string())
        .unwrap();
    cmd_tx
        .send(json!({ "id": "sfm", "type": "set_follow_up_mode", "mode": "all" }).to_string())
        .unwrap();
    cmd_tx
        .send(json!({ "id": "sac", "type": "set_auto_compaction", "enabled": true }).to_string())
        .unwrap();
    cmd_tx
        .send(json!({ "id": "sar", "type": "set_auto_retry", "enabled": true }).to_string())
        .unwrap();
    cmd_tx
        .send(json!({ "id": "eur", "type": "extension_ui_response" }).to_string())
        .unwrap();
    let _ = recv_until(&mut out_rx, Duration::from_secs(2), |v| {
        v["type"] == "response" && v["id"] == "eur"
    })
    .await;

    // 5. get_last_assistant_text, get_fork_messages, get_session_stats, export_html
    cmd_tx
        .send(json!({ "id": "glt", "type": "get_last_assistant_text" }).to_string())
        .unwrap();
    let glt = recv_until(&mut out_rx, Duration::from_secs(2), |v| {
        v["type"] == "response" && v["id"] == "glt"
    })
    .await;
    assert_eq!(glt["data"]["text"], "working");

    cmd_tx
        .send(json!({ "id": "gfm", "type": "get_fork_messages" }).to_string())
        .unwrap();
    let gfm = recv_until(&mut out_rx, Duration::from_secs(2), |v| {
        v["type"] == "response" && v["id"] == "gfm"
    })
    .await;
    assert_eq!(gfm["data"]["messages"].as_array().unwrap().len(), 4);

    cmd_tx
        .send(json!({ "id": "stats", "type": "get_session_stats" }).to_string())
        .unwrap();
    let stats = recv_until(&mut out_rx, Duration::from_secs(2), |v| {
        v["type"] == "response" && v["id"] == "stats"
    })
    .await;
    assert_eq!(stats["data"]["userMessages"], 4);
    assert_eq!(stats["data"]["assistantMessages"], 4);
    assert_eq!(stats["data"]["tokens"]["input"], 30);
    assert_eq!(stats["data"]["tokens"]["output"], 50);
    assert_eq!(stats["data"]["contextUsage"]["tokens"], 1200);
    assert_eq!(stats["data"]["contextUsage"]["contextWindow"], 8000);

    cmd_tx
        .send(json!({ "id": "exp", "type": "export_html", "path": "out/sesi.html" }).to_string())
        .unwrap();
    let exp = recv_until(&mut out_rx, Duration::from_secs(2), |v| {
        v["type"] == "response" && v["id"] == "exp"
    })
    .await;
    let html_path = exp["data"]["path"].as_str().unwrap();
    let html_body = std::fs::read_to_string(html_path).unwrap();
    assert!(html_body.contains("Judul Manual"));
    assert!(html_body.contains("echo:steer-1"));

    // 6. cycle_model, cycle_thinking_level, get_modes, set_mode
    cmd_tx
        .send(json!({ "id": "cm", "type": "cycle_model" }).to_string())
        .unwrap();
    let cm = recv_until(&mut out_rx, Duration::from_secs(2), |v| {
        v["type"] == "response" && v["id"] == "cm"
    })
    .await;
    assert_eq!(cm["data"]["id"], "m-beta");

    cmd_tx
        .send(json!({ "id": "ctl", "type": "cycle_thinking_level" }).to_string())
        .unwrap();
    let ctl = recv_until(&mut out_rx, Duration::from_secs(2), |v| {
        v["type"] == "response" && v["id"] == "ctl"
    })
    .await;
    assert_eq!(ctl["data"]["level"], "high");

    cmd_tx
        .send(json!({ "id": "gm", "type": "get_modes" }).to_string())
        .unwrap();
    let gm = recv_until(&mut out_rx, Duration::from_secs(2), |v| {
        v["type"] == "response" && v["id"] == "gm"
    })
    .await;
    assert_eq!(gm["data"]["currentModeId"], "architect");

    cmd_tx
        .send(json!({ "id": "sm", "type": "set_mode", "modeId": "code" }).to_string())
        .unwrap();
    let sm = recv_until(&mut out_rx, Duration::from_secs(2), |v| {
        v["type"] == "response" && v["id"] == "sm"
    })
    .await;
    assert_eq!(sm["success"], true);

    // 7. new_session & switch_session
    cmd_tx
        .send(json!({ "id": "ns", "type": "new_session" }).to_string())
        .unwrap();
    let ns = recv_until(&mut out_rx, Duration::from_secs(2), |v| {
        v["type"] == "response" && v["id"] == "ns"
    })
    .await;
    assert_eq!(ns["data"]["cancelled"], false);

    cmd_tx
        .send(json!({ "id": "sw", "type": "switch_session", "sessionPath": first_session_file }).to_string())
        .unwrap();
    let sw = recv_until(&mut out_rx, Duration::from_secs(2), |v| {
        v["type"] == "response" && v["id"] == "sw"
    })
    .await;
    assert_eq!(sw["data"]["cancelled"], false);

    cmd_tx
        .send(json!({ "id": "gs2", "type": "get_state" }).to_string())
        .unwrap();
    let gs2 = recv_until(&mut out_rx, Duration::from_secs(2), |v| {
        v["type"] == "response" && v["id"] == "gs2"
    })
    .await;
    assert_eq!(gs2["data"]["sessionName"], "Judul Manual");
    assert_eq!(gs2["data"]["messageCount"], 8);

    drop(cmd_tx);
    let _ = tokio::time::timeout(Duration::from_secs(2), loop_handle).await;
    let _ = std::fs::remove_dir_all(&dir);
}


