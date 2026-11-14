use crate::agent::Approver;
use crate::events::{AgentEvent, EventSink, ToolStatus};
use crate::models::{ModelCatalog, ModelInfo};
use crate::tools::ToolRegistry;
#[allow(unused_imports)]
use crate::util::{epoch_ms, iso_now, parse_iso_to_epoch_ms, read_json_file};
use std::sync::atomic::Ordering;

use super::*;
use std::io::Cursor;

#[test]
fn maps_tools_for_pipkin_timeline() {
    let (name, args) = map_tool_for_pipkin_ui("run_shell", &json!({ "command": "ls" }));
    assert_eq!(name, "bash");
    assert_eq!(args["command"], "ls");
}

#[test]
fn formats_shell_exit_code_for_step_figure() {
    let val = json!({
        "stdout": "hello",
        "stderr": "err",
        "exit_code": 2
    });
    let (text, is_err) = format_tool_output("run_shell", &val);
    assert!(is_err);
    assert!(text.ends_with("Command exited with code 2"));
}

#[test]
fn load_providers_applies_model_overrides_and_local_providers() {
    let tmp = std::env::temp_dir().join(format!("oopbot_rpc_test_{}", epoch_ms()));
    let _ = std::fs::create_dir_all(&tmp);
    std::fs::write(
        tmp.join("models.json"),
        serde_json::to_string_pretty(&json!({
            "providers": {
                "google": {
                    "modelOverrides": {
                        "gemini-2.5-flash": {
                            "name": "Flash Override Dari Aplikasi",
                            "maxTokens": 8192,
                            "reasoning": false
                        }
                    }
                },
                "pipkin-local": {
                    "baseUrl": "http://127.0.0.1:8080/v1",
                    "api": "openai-completions",
                    "apiKey": "no-api-key",
                    "models": [
                        {
                            "id": "qwen3-4b",
                            "name": "Qwen3 4B Local",
                            "reasoning": true,
                            "contextWindow": 32768,
                            "maxTokens": 4096
                        }
                    ]
                }
            }
        }))
        .unwrap(),
    )
    .unwrap();

    let cat = ModelCatalog::builtin();
    let providers = load_providers(&tmp, &cat);
    let google = providers.get("google").unwrap();
    let flash = google
        .models
        .iter()
        .find(|m| m.id == "gemini-2.5-flash")
        .unwrap();
    assert_eq!(flash.label, "Flash Override Dari Aplikasi");
    assert_eq!(flash.output_token_limit, Some(8192));
    assert!(!flash.reasoning);

    let local = providers.get("pipkin-local").unwrap();
    assert!(local.has_auth);
    assert_eq!(local.models.len(), 1);
    assert_eq!(local.models[0].id, "qwen3-4b");
    assert_eq!(local.models[0].input_token_limit, Some(32768));

    let _ = std::fs::remove_dir_all(&tmp);
}

#[test]
fn load_providers_uses_scanned_models_without_mixing_hardcoded_models() {
    let tmp = std::env::temp_dir().join(format!("oopbot_scan_test_{}", epoch_ms()));
    let _ = std::fs::create_dir_all(&tmp);

    // Simulasikan hasil scan live dari token pengguna yang hanya mendukung gemini-2.0-flash
    let scanned = vec![ModelInfo {
        id: "gemini-2.0-flash".to_string(),
        label: "Gemini 2.0 Flash (Scanned)".to_string(),
        provider: "google".to_string(),
        supports_tools: true,
        input_token_limit: Some(1_048_576),
        output_token_limit: Some(8192),
        description: "Scanned from token".to_string(),
        api: "google-generative-ai".to_string(),
        reasoning: true,
        thinking_level_map: None,
        input: vec!["text".to_string(), "image".to_string()],
        cost: None,
    }];

    save_scanned_models_to_config(&tmp, "google", &scanned, "test-api-key");

    let cat = ModelCatalog::builtin();
    let providers = load_providers(&tmp, &cat);
    let google = providers.get("google").unwrap();

    // Harus HANYA berisi model hasil scan dari token, TIDAK mencampur model hardcode bawaan!
    assert_eq!(google.models.len(), 1);
    assert_eq!(google.models[0].id, "gemini-2.0-flash");
    assert!(!google
        .models
        .iter()
        .any(|m| m.id == "gemini-3.5-flash-lite"));

    // Verifikasi scannedApiKey tersimpan
    let read_back = read_json_file(&tmp.join("models.json"));
    assert_eq!(
        read_back["providers"]["google"]["scannedApiKey"].as_str(),
        Some("test-api-key")
    );

    let _ = std::fs::remove_dir_all(&tmp);
}

struct TempConfigGuard(PathBuf);
impl TempConfigGuard {
    fn new(prefix: &str) -> Self {
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
    fn path(&self) -> &Path {
        &self.0
    }
}
impl Drop for TempConfigGuard {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

#[test]
fn rpc_server_activates_plugins_from_extensions_arg() {
    let tmp = TempConfigGuard::new("oopbot_plugins_test");
    let extensions = vec![
        "/data/user/0/io.jeph.pipkin/files/extensions/pipkin-todo/index.js".to_string(),
        "pipkin-web-access".to_string(),
    ];

    let server = RpcServer::new_in_dir(tmp.path().to_path_buf(), None, None, extensions).unwrap();
    assert!(server.plugins.is_active("pipkin-todo"));
    assert!(server.plugins.is_active("pipkin-web-access"));
    assert!(!server.plugins.is_active("pipkin-background"));

    // Tool & system instruction kini milik agen (langkah 4) — intent tes
    // sama: new() mengaktifkan plugin dan merakit instruksi RPC-nya.
    let tool_names = server.agent.tool_names();
    assert!(tool_names.contains(&"todo"));
    assert!(tool_names.contains(&"web_search"));
    assert!(tool_names.contains(&"fetch_page"));
    assert!(!tool_names.contains(&"background"));
    assert!(server
        .agent
        .system_instruction()
        .contains("Todo & Task Management Guidelines"));
    assert!(server
        .agent
        .system_instruction()
        .contains("Web Access Guidelines"));
}

#[test]
fn session_preserves_tokens_and_cost_on_reload() {
    let tmp = TempConfigGuard::new("oopbot_session_stats");
    let session_file = tmp.path().join("test_session.jsonl");

    let lines = [
        json!({
            "type": "session",
            "id": "s-12345",
            "timestamp": "2026-11-09T08:00:00Z",
            "cwd": "/home/user"
        })
        .to_string(),
        json!({
            "type": "message",
            "id": "e0",
            "message": {
                "role": "user",
                "content": "Halo apa kabar?"
            }
        })
        .to_string(),
        json!({
            "type": "message",
            "id": "e1",
            "message": {
                "role": "assistant",
                "content": [{ "type": "text", "text": "Kabar baik!" }],
                "usage": {
                    "input": 1250,
                    "output": 350,
                    "totalTokens": 1600
                },
                "cost": 0.0025
            }
        })
        .to_string(),
        json!({
            "type": "session_stats",
            "id": "e2",
            "total_tokens": 1600,
            "total_cost": 0.0025
        })
        .to_string(),
    ];
    std::fs::write(&session_file, lines.join("\n")).unwrap();

    let server = RpcServer::new_in_dir(
        tmp.path().to_path_buf(),
        Some(session_file.to_string_lossy().to_string()),
        None,
        vec![],
    )
    .unwrap();
    // Total kini hidup di ui_state bersama (langkah 4) — intent tes sama:
    // reload sesi merekonstruksi total token & biaya dari file sesi.
    let (total_tokens, total_cost) = {
        let state = server.ui_state.lock().unwrap();
        (state.total_tokens, state.total_cost)
    };
    assert_eq!(total_tokens, 1600);
    assert!((total_cost - 0.0025).abs() < 1e-6);
}

#[tokio::test]
async fn test_pipkin_channel_handshake_and_incoming_events() {
    use std::io::Cursor;
    let _sink_guard = output_sink_test_mutex().lock().await;
    let tmp = TempConfigGuard::new("oopbot_channel_test");

    let raw = Arc::new(Mutex::new(Vec::new()));
    struct TestBuf(Arc<Mutex<Vec<u8>>>);
    impl io::Write for TestBuf {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            self.0.lock().unwrap().write(buf)
        }
        fn flush(&mut self) -> io::Result<()> {
            self.0.lock().unwrap().flush()
        }
    }

    let output_buf = Arc::new(Mutex::new(Box::new(TestBuf(raw.clone())) as Box<dyn io::Write + Send + Sync>));
    set_output_sink(Some(output_buf));

    let extensions = vec![
        "pipkin-background".to_string(),
        "pipkin-subagents".to_string(),
    ];
    let mut server = RpcServer::new_in_dir(tmp.path().to_path_buf(), None, None, extensions).unwrap();

    let input = b"\
{\"type\":\"pipkin_channel\",\"topic\":\"background\",\"data\":{\"action\":\"stop\"}}\n\
{\"type\":\"pipkin_channel\",\"topic\":\"subagents\",\"data\":{\"action\":\"stopAll\"}}\n\
{\"type\":\"pipkin_channel\",\"topic\":\"remote\",\"data\":{\"action\":\"check\"}}\n\
{\"type\":\"oopbot_channel\",\"topic\":\"remote\",\"data\":{\"action\":\"check\"}}\n\
{\"type\":\"get_state\",\"id\":\"chk_1\"}\n";
    let cursor = Cursor::new(input);

    let res = server.run_loop_with_input(cursor).await;
    assert!(res.is_ok());

    set_output_sink(None);

    let bytes = raw.lock().unwrap().clone();
    let written = String::from_utf8_lossy(&bytes);

    // Verifikasi handshake awal pipkin & pipkin:hello
    assert!(written.contains("\"topic\":\"pipkin\""));
    assert!(written.contains("\"topic\":\"pipkin:hello\""));
    assert!(written.contains("\"engine\":\"oopbot-native\""));

    // Verifikasi channel background, subagents, dan remote
    assert!(written.contains("\"topic\":\"background\""));
    assert!(written.contains("\"active\":0"));
    assert!(written.contains("\"topic\":\"subagents\""));
    assert!(written.contains("\"topic\":\"remote\""));
    assert!(written.contains("\"connected\":true"));

    // Verifikasi respon rpc reguler
    assert!(written.contains("chk_1"));
    assert!(written.contains("\"success\":true"));

    // Pastikan tidak ada error komplain pipkin_channel
    assert!(!written.contains("tidak dikenal"));
}

#[test]
fn test_rpc_event_sink_text_turn() {
    use crate::events::StepsOutcome;

    let ui_state = Arc::new(Mutex::new(RpcUiState::default()));
    let emitted = Arc::new(Mutex::new(Vec::new()));
    let emitted_clone = emitted.clone();
    let sink = RpcEventSink::with_emitter(ui_state.clone(), move |val| {
        emitted_clone.lock().unwrap().push(val.clone());
    });

    sink.on_event(&AgentEvent::TurnStart {
        session_id: "s1".to_string(),
        model: "gemini-2.5-flash".to_string(),
    });
    sink.on_event(&AgentEvent::Thinking {
        step_index: 1,
        text: "Sedang menganalisis pertanyaan".to_string(),
    });
    sink.on_event(&AgentEvent::AssistantText {
        text: "Halo! Ada yang bisa saya bantu?".to_string(),
    });
    sink.on_event(&AgentEvent::TurnEnd {
        steps_count: 0,
        duration_ms: 120,
        outcome: StepsOutcome::Done,
    });

    let lines = emitted.lock().unwrap().clone();
    let types: Vec<&str> = lines.iter().filter_map(|l| l["type"].as_str()).collect();

    assert_eq!(types[0], "agent_start");
    assert_eq!(types[1], "message_start");
    assert_eq!(types[2], "message_update"); // thinking_start
    assert_eq!(types[3], "message_update"); // thinking_end
    assert_eq!(types[4], "message_update"); // text_start
    assert_eq!(types[5], "message_update"); // text_delta
    assert_eq!(types[6], "message_update"); // text_end
    assert_eq!(types[7], "message_end");
    assert_eq!(types[8], "turn_end");
    assert_eq!(types[9], "agent_end");
    assert_eq!(types[10], "agent_settled");

    let state = ui_state.lock().unwrap();
    assert_eq!(state.ui_messages.len(), 1);
    assert_eq!(state.ui_messages[0]["role"], "assistant");
    assert_eq!(state.ui_messages[0]["stopReason"], "stop");
}

#[test]
fn test_rpc_event_sink_tool_turn_and_rejection() {
    use crate::events::StepsOutcome;

    let tmp = std::env::temp_dir().join(format!("oopbot-sink-test-{}", std::process::id()));
    let _ = std::fs::create_dir_all(&tmp);
    let session_file = tmp.join("test_session.jsonl");
    let ui_state = Arc::new(Mutex::new(RpcUiState::new(Some(session_file.clone()), 0)));

    let emitted = Arc::new(Mutex::new(Vec::new()));
    let emitted_clone = emitted.clone();
    let sink = RpcEventSink::with_emitter(ui_state.clone(), move |val| {
        emitted_clone.lock().unwrap().push(val.clone());
    });

    sink.on_event(&AgentEvent::TurnStart {
        session_id: "s2".to_string(),
        model: "gemini-2.5-flash".to_string(),
    });

    // 1. Tool Call biasa (run_shell -> dipetakan ke "bash")
    sink.on_event(&AgentEvent::ToolCallStart {
        step_index: 1,
        call_id: "call_1".to_string(),
        tool_name: "run_shell".to_string(),
        args: json!({ "command": "echo test" }),
        preview: "$ echo test".to_string(),
        requires_confirmation: false,
    });

    sink.on_event(&AgentEvent::ToolCallEnd {
        step_index: 1,
        call_id: "call_1".to_string(),
        tool_name: "run_shell".to_string(),
        status: ToolStatus::Done,
        output: json!({ "exit_code": 0, "stdout": "test\n", "stderr": "" }),
        duration_ms: 10,
    });

    // 2. Tool Call kedua yang ditolak user
    sink.on_event(&AgentEvent::ToolCallStart {
        step_index: 2,
        call_id: "call_2".to_string(),
        tool_name: "run_shell".to_string(),
        args: json!({ "command": "rm -rf /" }),
        preview: "$ rm -rf /".to_string(),
        requires_confirmation: true,
    });

    sink.on_event(&AgentEvent::ToolCallEnd {
        step_index: 2,
        call_id: "call_2".to_string(),
        tool_name: "run_shell".to_string(),
        status: ToolStatus::Rejected,
        output: json!({ "error": "User menolak eksekusi tool ini." }),
        duration_ms: 0,
    });

    sink.on_event(&AgentEvent::AssistantText {
        text: "Saya memahami penolakan Anda.".to_string(),
    });

    sink.on_event(&AgentEvent::TurnEnd {
        steps_count: 2,
        duration_ms: 50,
        outcome: StepsOutcome::Done,
    });

    let lines = emitted.lock().unwrap().clone();
    // Pastikan mapping tool "run_shell" -> "bash"
    let tool_start = lines.iter().find(|l| l["type"] == "tool_execution_start").unwrap();
    assert_eq!(tool_start["toolName"], "bash");

    // Pastikan event rejection memiliki isError: true
    let tool_ends: Vec<&Value> = lines.iter().filter(|l| l["type"] == "tool_execution_end").collect();
    assert_eq!(tool_ends.len(), 2);
    assert_eq!(tool_ends[0]["isError"], false);
    assert_eq!(tool_ends[1]["isError"], true);

    // Pastikan file sesi ditulis dengan format JSONL valid
    let content = std::fs::read_to_string(&session_file).unwrap();
    assert!(content.contains("\"type\":\"message\""));
    assert!(content.contains("\"role\":\"toolResult\""));
    assert!(content.contains("\"isError\":true"));

    let _ = std::fs::remove_dir_all(&tmp);
}

#[tokio::test]
async fn test_rpc_approver_approved() {
    let pending = Arc::new(Mutex::new(HashMap::new()));
    let emitted = Arc::new(Mutex::new(Vec::new()));
    let emitted_clone = Arc::clone(&emitted);

    let approver = RpcApprover::new(Arc::clone(&pending))
        .with_emitter(move |val| {
            emitted_clone.lock().unwrap().push(val);
        });

    let handle = tokio::spawn(async move {
        approver.approve("run_shell", "$ rm -rf /tmp/danger").await
    });

    for _ in 0..50 {
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        if !pending.lock().unwrap().is_empty() {
            break;
        }
    }

    let req_id = {
        let map = pending.lock().unwrap();
        assert_eq!(map.len(), 1);
        map.keys().next().unwrap().clone()
    };

    // Verifikasi event request mengikuti protokol Android extension_ui_request (confirm)
    {
        let events = emitted.lock().unwrap();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0]["type"], "extension_ui_request");
        assert_eq!(events[0]["id"], req_id);
        assert_eq!(events[0]["method"], "confirm");
        assert_eq!(events[0]["title"], "Allow run_shell?");
        assert_eq!(events[0]["message"], "$ rm -rf /tmp/danger");
        assert_eq!(events[0]["timeout"], 300_000);
    }

    // Simulasikan persetujuan dari UI
    if let Some(tx) = pending.lock().unwrap().remove(&req_id) {
        let _ = tx.send(true);
    }

    let approved = handle.await.unwrap();
    assert!(approved);
}

#[tokio::test]
async fn test_rpc_approver_denied() {
    let pending = Arc::new(Mutex::new(HashMap::new()));
    let approver = RpcApprover::new(Arc::clone(&pending))
        .with_emitter(|_| {});

    let handle = tokio::spawn(async move {
        approver.approve("run_shell", "$ sudo rm -rf /").await
    });

    for _ in 0..50 {
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        if !pending.lock().unwrap().is_empty() {
            break;
        }
    }

    let req_id = {
        let map = pending.lock().unwrap();
        map.keys().next().unwrap().clone()
    };

    // Simulasikan penolakan dari UI
    if let Some(tx) = pending.lock().unwrap().remove(&req_id) {
        let _ = tx.send(false);
    }

    let approved = handle.await.unwrap();
    assert!(!approved);
}

#[tokio::test]
async fn test_rpc_approver_timeout_fails_closed() {
    let pending = Arc::new(Mutex::new(HashMap::new()));
    let approver = RpcApprover::new(Arc::clone(&pending))
        .with_timeout(std::time::Duration::from_millis(50))
        .with_emitter(|_| {});

    // Panggil approve tanpa ada yang menyelesaikan respons -> harus timeout dan return false
    let approved = approver.approve("run_shell", "reboot").await;
    assert!(!approved);

    // Pastikan request dibersihkan dari pending map
    assert!(pending.lock().unwrap().is_empty());
}

#[tokio::test]
async fn test_rpc_approver_channel_dropped_fails_closed() {
    let pending = Arc::new(Mutex::new(HashMap::new()));
    let approver = RpcApprover::new(Arc::clone(&pending))
        .with_emitter(|_| {});

    let handle = tokio::spawn(async move {
        approver.approve("run_shell", "ls").await
    });

    for _ in 0..50 {
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        if !pending.lock().unwrap().is_empty() {
            break;
        }
    }

    // Ambil sender dan drop tanpa send -> simulasikan channel putus
    {
        let mut map = pending.lock().unwrap();
        let _sender = map.drain().next();
    }

    let approved = handle.await.unwrap();
    assert!(!approved);
}

#[tokio::test]
async fn test_rpc_server_resolve_tool_approval() {
    let tmp = TempConfigGuard::new("oopbot_resolve_appr");
    let server = RpcServer::new_in_dir_with_cwd(
        tmp.path().to_path_buf(),
        tmp.path().to_path_buf(),
        None,
        None,
        vec![],
    )
    .unwrap();
    let (tx, rx) = tokio::sync::oneshot::channel::<bool>();

    server.pending_approvals().lock().unwrap().insert("appr-99".to_string(), tx);

    // Sukses me-resolve approval yang ada
    assert!(server.resolve_tool_approval("appr-99", true));
    assert!(rx.await.unwrap());

    // Memanggil lagi untuk ID yang sama atau id fiktif mengembalikan false
    assert!(!server.resolve_tool_approval("appr-99", true));
    assert!(!server.resolve_tool_approval("unknown-id", false));
}

#[tokio::test]
async fn test_rpc_loop_tool_approval_pipkin_channel_roundtrip() {
    let tmp = TempConfigGuard::new("oopbot_chan_appr");
    let mut server = RpcServer::new_in_dir_with_cwd(
        tmp.path().to_path_buf(),
        tmp.path().to_path_buf(),
        None,
        None,
        vec![],
    )
    .unwrap();
    let (tx, rx) = tokio::sync::oneshot::channel::<bool>();
    server.pending_approvals().lock().unwrap().insert("appr-42".to_string(), tx);

    let input = b"\
{\"type\":\"pipkin_channel\",\"topic\":\"tool_approval\",\"data\":{\"kind\":\"response\",\"request_id\":\"appr-42\",\"approved\":true}}\n\
{\"type\":\"get_state\",\"id\":\"chk_approval\"}\n";
    let cursor = Cursor::new(input);

    let res = server.run_loop_with_input(cursor).await;
    assert!(res.is_ok());

    // Verifikasi bahwa waiter oneshot menerima approval = true dari stream JSONL
    assert!(rx.await.unwrap());
}

#[tokio::test]
async fn test_rpc_loop_extension_ui_response_roundtrip() {
    let _sink_guard = output_sink_test_mutex().lock().await;
    let tmp = TempConfigGuard::new("oopbot_ext_ui_resp");

    let raw = Arc::new(Mutex::new(Vec::new()));
    struct SharedBuf(Arc<Mutex<Vec<u8>>>);
    impl io::Write for SharedBuf {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            self.0.lock().unwrap().write(buf)
        }
        fn flush(&mut self) -> io::Result<()> {
            self.0.lock().unwrap().flush()
        }
    }
    let output_buf = Arc::new(Mutex::new(Box::new(SharedBuf(raw.clone())) as Box<dyn io::Write + Send + Sync>));
    set_output_sink(Some(output_buf));

    let mut server = RpcServer::new_in_dir_with_cwd(
        tmp.path().to_path_buf(),
        tmp.path().to_path_buf(),
        None,
        None,
        vec![],
    )
    .unwrap();
    let (tx1, rx1) = tokio::sync::oneshot::channel::<bool>();
    let (tx2, rx2) = tokio::sync::oneshot::channel::<bool>();
    let (tx3, rx3) = tokio::sync::oneshot::channel::<bool>();
    let (tx_eof, rx_eof) = tokio::sync::oneshot::channel::<bool>();
    {
        let pending = server.pending_approvals();
        let mut map = pending.lock().unwrap();
        map.insert("appr-1".to_string(), tx1);
        map.insert("appr-2".to_string(), tx2);
        map.insert("appr-3".to_string(), tx3);
        map.insert("appr-eof".to_string(), tx_eof);
    }

    let input = b"\
{\"type\":\"extension_ui_response\",\"id\":\"appr-1\",\"confirmed\":true}\n\
{\"type\":\"extension_ui_response\",\"id\":\"appr-2\",\"confirmed\":false}\n\
{\"type\":\"extension_ui_response\",\"id\":\"appr-3\",\"cancelled\":true}\n\
{\"type\":\"get_state\",\"id\":\"chk_ext\"}\n";
    let cursor = Cursor::new(input);

    let res = server.run_loop_with_input(cursor).await;
    set_output_sink(None);
    assert!(res.is_ok());

    assert!(rx1.await.unwrap());
    assert!(!rx2.await.unwrap());
    assert!(!rx3.await.unwrap());
    assert!(rx_eof.await.is_err());

    let bytes = raw.lock().unwrap().clone();
    let written = String::from_utf8_lossy(&bytes);
    assert!(written.contains("chk_ext"));
    assert!(!written.contains("tidak dikenal"));
    assert!(!written.contains("extension_ui_response"));
}

// -------------------------------------------------------------------------
// Harness & Regression Tests for REG-002 & REG-003 (Regression-Aware.md)
// -------------------------------------------------------------------------

struct OutputSinkResetGuard;
impl Drop for OutputSinkResetGuard {
    fn drop(&mut self) {
        set_output_sink(None);
    }
}

struct ChannelWriter {
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

fn install_channel_output_sink() -> (
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

struct TestLineReader {
    rx: std::sync::mpsc::Receiver<Vec<u8>>,
    cur: Vec<u8>,
    pos: usize,
}

impl TestLineReader {
    fn new() -> (std::sync::mpsc::Sender<Vec<u8>>, Self) {
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

fn send_cmd(tx: &std::sync::mpsc::Sender<Vec<u8>>, cmd: Value) {
    let mut s = serde_json::to_string(&cmd).unwrap();
    s.push('\n');
    tx.send(s.into_bytes()).unwrap();
}

async fn recv_matching<F>(
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

async fn recv_response(
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
struct MockHttpReq {
    path: String,
    body: Value,
}

async fn spawn_mock_llm_server<F, Fut>(
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

fn write_mock_provider_config(
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

fn gemini_text_resp(text: &str, inp: u64, out: u64) -> Value {
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

#[tokio::test]
async fn reg002_get_state_nonblocking_while_streaming() {
    let _sink_guard = output_sink_test_mutex().lock().await;
    let (_reset, mut out_rx) = install_channel_output_sink();
    let tmp = TempConfigGuard::new("reg002_get_state");

    let (inflight_tx, inflight_rx) = tokio::sync::oneshot::channel::<()>();
    let inflight_tx = Arc::new(Mutex::new(Some(inflight_tx)));
    let (base_url, _reqs, mock_handle) = spawn_mock_llm_server(move |_idx, _req| {
        let inflight_tx = Arc::clone(&inflight_tx);
        async move {
            if let Some(tx) = inflight_tx.lock().unwrap().take() {
                let _ = tx.send(());
            }
            tokio::time::sleep(std::time::Duration::from_millis(1200)).await;
            (200, gemini_text_resp("Selesai setelah delay", 100, 20))
        }
    })
    .await;

    write_mock_provider_config(
        tmp.path(),
        "google",
        "google-generative-ai",
        "gemini-2.5-flash",
        &format!("{}/v1beta", base_url),
        "test-google-key",
    );

    let mut server = RpcServer::new_in_dir_with_cwd(
        tmp.path().to_path_buf(),
        tmp.path().to_path_buf(),
        None,
        None,
        vec![],
    )
    .unwrap();

    let (cmd_tx, reader) = TestLineReader::new();
    let loop_task = tokio::spawn(async move {
        server.run_loop_with_input(io::BufReader::new(reader)).await.unwrap();
    });

    // Drain initial hello
    let _ = recv_matching(&mut out_rx, std::time::Duration::from_secs(2), |v| {
        v["type"] == "pipkin_channel" && v["topic"] == "pipkin:hello"
    })
    .await;

    send_cmd(
        &cmd_tx,
        json!({ "id": "p1", "type": "prompt", "message": "jalan lambat" }),
    );
    let p1_resp = recv_response(&mut out_rx, "p1", std::time::Duration::from_millis(400)).await;
    assert_eq!(p1_resp["success"], true);
    assert_eq!(p1_resp["data"]["disposition"], "started");

    // Pastikan request HTTP sudah aktif
    tokio::time::timeout(std::time::Duration::from_secs(2), inflight_rx)
        .await
        .expect("HTTP request never hit mock server")
        .unwrap();

    let started = std::time::Instant::now();
    send_cmd(&cmd_tx, json!({ "id": "gs1", "type": "get_state" }));
    let gs1 = recv_response(&mut out_rx, "gs1", std::time::Duration::from_millis(400)).await;
    assert!(started.elapsed() < std::time::Duration::from_millis(400));
    assert_eq!(gs1["data"]["isStreaming"], true);
    assert_eq!(gs1["data"]["isCompacting"], false);

    // Tunggu turn selesai
    let _settled = recv_matching(&mut out_rx, std::time::Duration::from_secs(3), |v| {
        v["type"] == "agent_settled"
    })
    .await;

    send_cmd(&cmd_tx, json!({ "id": "gs2", "type": "get_state" }));
    let gs2 = recv_response(&mut out_rx, "gs2", std::time::Duration::from_millis(400)).await;
    assert_eq!(gs2["data"]["isStreaming"], false);

    drop(cmd_tx);
    let _ = tokio::time::timeout(std::time::Duration::from_secs(2), loop_task).await;
    mock_handle.abort();
}

#[tokio::test]
async fn reg002_abort_cancels_inflight_http_and_pending_approver() {
    let _sink_guard = output_sink_test_mutex().lock().await;
    let (_reset, mut out_rx) = install_channel_output_sink();
    let tmp = TempConfigGuard::new("reg002_abort");

    let (http_hit_tx, http_hit_rx) = tokio::sync::oneshot::channel::<()>();
    let http_hit_tx = Arc::new(Mutex::new(Some(http_hit_tx)));
    let (base_url, _reqs, mock_handle) = spawn_mock_llm_server(move |idx, _req| {
        let http_hit_tx = Arc::clone(&http_hit_tx);
        async move {
            match idx {
                // Call 0: HTTP hang -> akan di-abort
                0 => {
                    if let Some(tx) = http_hit_tx.lock().unwrap().take() {
                        let _ = tx.send(());
                    }
                    tokio::time::sleep(std::time::Duration::from_secs(10)).await;
                    (200, gemini_text_resp("tidak seharusnya sampai sini", 10, 5))
                }
                // Call 1: return dangerous run_shell tool call -> memicu RpcApprover
                1 => (
                    200,
                    json!({
                        "candidates": [{
                            "content": {
                                "role": "model",
                                "parts": [{
                                    "functionCall": {
                                        "id": "call_danger_1",
                                        "name": "run_shell",
                                        "args": { "command": "rm -rf /tmp/fake_abort_target" }
                                    }
                                }]
                            }
                        }]
                    }),
                ),
                // Call 2+: recovery prompt sesudah abort
                _ => (200, gemini_text_resp("Pemulihan berhasil", 50, 10)),
            }
        }
    })
    .await;

    write_mock_provider_config(
        tmp.path(),
        "google",
        "google-generative-ai",
        "gemini-2.5-flash",
        &format!("{}/v1beta", base_url),
        "test-google-key",
    );

    let mut server = RpcServer::new_in_dir_with_cwd(
        tmp.path().to_path_buf(),
        tmp.path().to_path_buf(),
        None,
        None,
        vec![],
    )
    .unwrap();
    let pending_approvals = server.pending_approvals();

    let (cmd_tx, reader) = TestLineReader::new();
    let loop_task = tokio::spawn(async move {
        server.run_loop_with_input(io::BufReader::new(reader)).await.unwrap();
    });

    let _ = recv_matching(&mut out_rx, std::time::Duration::from_secs(2), |v| {
        v["topic"] == "pipkin:hello"
    })
    .await;

    // Skenario A: Abort saat HTTP in-flight
    send_cmd(&cmd_tx, json!({ "id": "p_http", "type": "prompt", "message": "hang" }));
    let _ = recv_response(&mut out_rx, "p_http", std::time::Duration::from_millis(500)).await;
    tokio::time::timeout(std::time::Duration::from_secs(2), http_hit_rx)
        .await
        .unwrap()
        .unwrap();

    let t0 = std::time::Instant::now();
    send_cmd(&cmd_tx, json!({ "id": "ab1", "type": "abort" }));
    let settled_a = recv_matching(&mut out_rx, std::time::Duration::from_millis(600), |v| {
        v["type"] == "agent_settled"
    })
    .await;
    let ab1 = recv_response(&mut out_rx, "ab1", std::time::Duration::from_millis(600)).await;
    assert!(t0.elapsed() < std::time::Duration::from_millis(600));
    assert_eq!(ab1["success"], true);
    assert_eq!(settled_a["aborted"], true);

    // Skenario B: Abort saat RpcApprover sedang menunggu konfirmasi user
    send_cmd(&cmd_tx, json!({ "id": "p_appr", "type": "prompt", "message": "hapus tmp" }));
    let _ = recv_response(&mut out_rx, "p_appr", std::time::Duration::from_millis(500)).await;
    let confirm_ev = recv_matching(&mut out_rx, std::time::Duration::from_secs(2), |v| {
        v["type"] == "extension_ui_request" && v["method"] == "confirm"
    })
    .await;
    assert_eq!(confirm_ev["title"], "Allow run_shell?");
    assert!(!pending_approvals.lock().unwrap().is_empty());

    send_cmd(&cmd_tx, json!({ "id": "ab2", "type": "abort" }));
    let settled_b = recv_matching(&mut out_rx, std::time::Duration::from_millis(600), |v| {
        v["type"] == "agent_settled"
    })
    .await;
    let ab2 = recv_response(&mut out_rx, "ab2", std::time::Duration::from_millis(600)).await;
    assert_eq!(ab2["success"], true);
    assert_eq!(settled_b["aborted"], true);
    assert!(pending_approvals.lock().unwrap().is_empty());

    // Skenario C: Prompt berikutnya sesudah abort tetap berjalan bersih & histori tool seimbang
    send_cmd(&cmd_tx, json!({ "id": "p_ok", "type": "prompt", "message": "lanjut aman" }));
    let p_ok = recv_response(&mut out_rx, "p_ok", std::time::Duration::from_millis(500)).await;
    assert_eq!(p_ok["data"]["disposition"], "started");
    let reply = recv_matching(&mut out_rx, std::time::Duration::from_secs(2), |v| {
        v["type"] == "message_end"
            && v["message"]["role"] == "assistant"
            && v["message"]["stopReason"] == "stop"
    })
    .await;
    assert!(reply["message"]["content"][0]["text"]
        .as_str()
        .unwrap_or("")
        .contains("Pemulihan berhasil"));
    let _ = recv_matching(&mut out_rx, std::time::Duration::from_secs(2), |v| {
        v["type"] == "agent_settled" && v["aborted"] == false
    })
    .await;

    drop(cmd_tx);
    let _ = tokio::time::timeout(std::time::Duration::from_secs(2), loop_task).await;
    mock_handle.abort();
}

#[tokio::test]
async fn reg002_steer_queue_and_clear_queue_behavior() {
    let _sink_guard = output_sink_test_mutex().lock().await;
    let (_reset, mut out_rx) = install_channel_output_sink();
    let tmp = TempConfigGuard::new("reg002_steer");

    let gate = Arc::new(tokio::sync::Notify::new());
    let gate_srv = Arc::clone(&gate);
    let (call0_hit_tx, call0_hit_rx) = tokio::sync::oneshot::channel::<()>();
    let call0_hit_tx = Arc::new(Mutex::new(Some(call0_hit_tx)));

    let (base_url, reqs, mock_handle) = spawn_mock_llm_server(move |idx, req| {
        let gate_srv = Arc::clone(&gate_srv);
        let call0_hit_tx = Arc::clone(&call0_hit_tx);
        async move {
            match idx {
                0 => {
                    if let Some(tx) = call0_hit_tx.lock().unwrap().take() {
                        let _ = tx.send(());
                    }
                    gate_srv.notified().await;
                    (
                        200,
                        json!({
                            "candidates": [{
                                "content": {
                                    "role": "model",
                                    "parts": [{
                                        "functionCall": {
                                            "id": "call_ls_1",
                                            "name": "list_dir",
                                            "args": { "path": "." }
                                        }
                                    }]
                                }
                            }]
                        }),
                    )
                }
                1 => {
                    let s = req.body.to_string();
                    (
                        200,
                        gemini_text_resp(
                            if s.contains("arah baru") {
                                "Mengikuti arah baru"
                            } else {
                                "Tanpa arah"
                            },
                            60,
                            15,
                        ),
                    )
                }
                2 => (200, gemini_text_resp("Turn utama idle", 40, 10)),
                _ => (200, gemini_text_resp("Turn steer antrean idle", 40, 10)),
            }
        }
    })
    .await;

    write_mock_provider_config(
        tmp.path(),
        "google",
        "google-generative-ai",
        "gemini-2.5-flash",
        &format!("{}/v1beta", base_url),
        "test-google-key",
    );

    let mut server = RpcServer::new_in_dir_with_cwd(
        tmp.path().to_path_buf(),
        tmp.path().to_path_buf(),
        None,
        None,
        vec![],
    )
    .unwrap();

    let (cmd_tx, reader) = TestLineReader::new();
    let loop_task = tokio::spawn(async move {
        server.run_loop_with_input(io::BufReader::new(reader)).await.unwrap();
    });

    let _ = recv_matching(&mut out_rx, std::time::Duration::from_secs(2), |v| {
        v["topic"] == "pipkin:hello"
    })
    .await;

    // 1. Mulai turn streaming
    send_cmd(&cmd_tx, json!({ "id": "p1", "type": "prompt", "message": "mulai" }));
    let _ = recv_response(&mut out_rx, "p1", std::time::Duration::from_millis(500)).await;
    tokio::time::timeout(std::time::Duration::from_secs(2), call0_hit_rx)
        .await
        .unwrap()
        .unwrap();

    // 2. Prompt tanpa streamingBehavior saat streaming harus ditolak
    send_cmd(&cmd_tx, json!({ "id": "p_bad", "type": "prompt", "message": "tanpa behavior" }));
    let p_bad = recv_response(&mut out_rx, "p_bad", std::time::Duration::from_millis(400)).await;
    assert_eq!(p_bad["success"], false);

    // 3. Prompt dg streamingBehavior=steer + steer command harus langsung queued & emit queue_update
    send_cmd(&cmd_tx, json!({ "id": "p_q", "type": "prompt", "message": "antre 1", "streamingBehavior": "steer" }));
    let p_q = recv_response(&mut out_rx, "p_q", std::time::Duration::from_millis(400)).await;
    assert_eq!(p_q["success"], true);
    assert_eq!(p_q["data"]["disposition"], "queued");

    send_cmd(&cmd_tx, json!({ "id": "st1", "type": "steer", "message": "antre 2" }));
    let qu = recv_matching(&mut out_rx, std::time::Duration::from_millis(400), |v| {
        v["type"] == "queue_update" && v["steering"].as_array().map(|a| a.len()) == Some(2)
    })
    .await;
    assert_eq!(qu["steering"], json!(["antre 1", "antre 2"]));
    let st1 = recv_response(&mut out_rx, "st1", std::time::Duration::from_millis(400)).await;
    assert_eq!(st1["data"]["disposition"], "queued");

    // 4. clear_queue mengembalikan teks & mengosongkan antrean
    send_cmd(&cmd_tx, json!({ "id": "cq", "type": "clear_queue" }));
    let qu_empty = recv_matching(&mut out_rx, std::time::Duration::from_millis(400), |v| {
        v["type"] == "queue_update"
    })
    .await;
    assert_eq!(qu_empty["steering"], json!([]));
    assert_eq!(qu_empty["followUp"], json!([]));
    let cq = recv_response(&mut out_rx, "cq", std::time::Duration::from_millis(400)).await;
    assert_eq!(cq["data"]["steering"], json!(["antre 1", "antre 2"]));
    assert_eq!(cq["data"]["followUp"], json!([]));

    // 5. Antre steer real lalu buka gate HTTP -> steer diinjeksikan sebelum call LLM berikutnya
    send_cmd(&cmd_tx, json!({ "id": "st2", "type": "steer", "message": "arah baru" }));
    let _ = recv_response(&mut out_rx, "st2", std::time::Duration::from_millis(400)).await;
    gate.notify_one();

    let _ = recv_matching(&mut out_rx, std::time::Duration::from_secs(3), |v| {
        v["type"] == "agent_settled" && v["aborted"] == false
    })
    .await;
    assert!(reqs.lock().unwrap()[1].body.to_string().contains("arah baru"));

    // 6. ChatController.interruptWith pattern: steer saat idle HARUS tetap queued sampai prompt datang
    let req_count_before = reqs.lock().unwrap().len();
    send_cmd(&cmd_tx, json!({ "id": "st_idle", "type": "steer", "message": "steer belakangan" }));
    let st_idle = recv_response(&mut out_rx, "st_idle", std::time::Duration::from_millis(400)).await;
    assert_eq!(st_idle["data"]["disposition"], "queued");

    send_cmd(&cmd_tx, json!({ "id": "gs_idle", "type": "get_state" }));
    let gs_idle = recv_response(&mut out_rx, "gs_idle", std::time::Duration::from_millis(400)).await;
    assert_eq!(gs_idle["data"]["isStreaming"], false);
    assert_eq!(gs_idle["data"]["pendingMessageCount"], 1);
    assert_eq!(reqs.lock().unwrap().len(), req_count_before);

    send_cmd(&cmd_tx, json!({ "id": "p_first", "type": "prompt", "message": "prompt duluan" }));
    let p_first = recv_response(&mut out_rx, "p_first", std::time::Duration::from_millis(400)).await;
    assert_eq!(p_first["data"]["disposition"], "started");
    let _ = recv_matching(&mut out_rx, std::time::Duration::from_secs(3), |v| {
        v["type"] == "agent_settled" && v["aborted"] == false
    })
    .await;

    let all_reqs = reqs.lock().unwrap().clone();
    assert!(all_reqs.len() >= req_count_before + 2);
    assert!(all_reqs[req_count_before].body.to_string().contains("prompt duluan"));
    assert!(all_reqs[req_count_before + 1].body.to_string().contains("steer belakangan"));

    drop(cmd_tx);
    let _ = tokio::time::timeout(std::time::Duration::from_secs(2), loop_task).await;
    mock_handle.abort();
}

#[tokio::test]
async fn reg003_session_reload_preserves_tool_calls_across_providers() {
    let _sink_guard = output_sink_test_mutex().lock().await;
    let (_reset, mut out_rx) = install_channel_output_sink();
    let tmp = TempConfigGuard::new("reg003_reload");

    let session_file = tmp.path().join("tool_history.jsonl");
    let lines = [
        json!({ "type": "session", "version": 3, "id": "s-tools", "timestamp": "2026-11-12T00:00:00Z", "cwd": tmp.path() }).to_string(),
        json!({ "type": "message", "id": "e0", "parentId": null, "timestamp": "2026-11-12T00:00:01Z",
            "message": { "role": "user", "content": [{ "type": "text", "text": "cek repo" }], "timestamp": 1 } }).to_string(),
        // Assistant turn HANYA berisi thinking + toolCall (tanpa text block)
        json!({ "type": "message", "id": "e1", "parentId": "e0", "timestamp": "2026-11-12T00:00:02Z",
            "message": { "role": "assistant", "stopReason": "toolUse",
                "content": [
                    { "type": "thinking", "thinking": "Checking dir..." },
                    { "type": "toolCall", "id": "c1", "name": "ls", "arguments": { "path": "." } }
                ], "timestamp": 2 } }).to_string(),
        json!({ "type": "message", "id": "e2", "parentId": "e1", "timestamp": "2026-11-12T00:00:03Z",
            "message": { "role": "toolResult", "toolCallId": "c1", "toolName": "ls", "isError": false,
                "content": [{ "type": "text", "text": "Cargo.toml\nsrc/" }], "timestamp": 3 } }).to_string(),
        // Two consecutive toolCalls + two consecutive toolResults (uji merging Anthropic & OpenAI)
        json!({ "type": "message", "id": "e3", "parentId": "e2", "timestamp": "2026-11-12T00:00:04Z",
            "message": { "role": "assistant", "stopReason": "toolUse",
                "content": [
                    { "type": "toolCall", "id": "c2", "name": "read", "arguments": { "path": "Cargo.toml" } },
                    { "type": "toolCall", "id": "c3", "name": "bash", "arguments": { "command": "pwd" } }
                ], "timestamp": 4 } }).to_string(),
        json!({ "type": "message", "id": "e4", "parentId": "e3", "timestamp": "2026-11-12T00:00:05Z",
            "message": { "role": "toolResult", "toolCallId": "c2", "toolName": "read", "isError": false,
                "content": [{ "type": "text", "text": "name = \"oopbot\"" }], "timestamp": 5 } }).to_string(),
        json!({ "type": "message", "id": "e5", "parentId": "e4", "timestamp": "2026-11-12T00:00:06Z",
            "message": { "role": "toolResult", "toolCallId": "c3", "toolName": "bash", "isError": false,
                "content": [{ "type": "text", "text": "/repo" }], "timestamp": 6 } }).to_string(),
        json!({ "type": "message", "id": "e6", "parentId": "e5", "timestamp": "2026-11-12T00:00:07Z",
            "message": { "role": "assistant", "stopReason": "stop",
                "content": [{ "type": "text", "text": "Sudah dibaca." }], "timestamp": 7 } }).to_string(),
    ];
    std::fs::write(&session_file, format!("{}\n", lines.join("\n"))).unwrap();

    let (base_url, reqs, mock_handle) = spawn_mock_llm_server(move |_idx, req| async move {
        if req.path.contains("/chat/completions") {
            (200, json!({
                "choices": [{ "message": { "role": "assistant", "content": "ok openai" } }],
                "usage": { "prompt_tokens": 50, "completion_tokens": 10 }
            }))
        } else if req.path.ends_with("/messages") {
            (200, json!({
                "content": [{ "type": "text", "text": "ok anthropic" }],
                "usage": { "input_tokens": 50, "output_tokens": 10 }
            }))
        } else {
            (200, gemini_text_resp("ok gemini", 50, 10))
        }
    })
    .await;

    for (provider, api, model_id, suffix) in [
        ("google", "google-generative-ai", "gemini-2.5-flash", "/v1beta"),
        ("openrouter", "openai-completions", "qwen/qwen3-coder", "/v1"),
        ("anthropic", "anthropic-messages", "claude-sonnet-4", "/v1"),
    ] {
        let prov_dir = tmp.path().join(provider);
        std::fs::create_dir_all(&prov_dir).unwrap();
        write_mock_provider_config(
            &prov_dir,
            provider,
            api,
            model_id,
            &format!("{}{}", base_url, suffix),
            "k-test",
        );

        let mut server = RpcServer::new_in_dir_with_cwd(
            prov_dir,
            tmp.path().to_path_buf(),
            Some(session_file.to_string_lossy().to_string()),
            None,
            vec![],
        )
        .unwrap();

        let (cmd_tx, reader) = TestLineReader::new();
        let loop_task = tokio::spawn(async move {
            server.run_loop_with_input(io::BufReader::new(reader)).await.unwrap();
        });
        let _ = recv_matching(&mut out_rx, std::time::Duration::from_secs(2), |v| {
            v["topic"] == "pipkin:hello"
        })
        .await;

        send_cmd(&cmd_tx, json!({ "id": "p", "type": "prompt", "message": "tadi pwd apa?" }));
        let _ = recv_response(&mut out_rx, "p", std::time::Duration::from_millis(500)).await;
        let _ = recv_matching(&mut out_rx, std::time::Duration::from_secs(2), |v| {
            v["type"] == "agent_settled"
        })
        .await;

        drop(cmd_tx);
        let _ = tokio::time::timeout(std::time::Duration::from_secs(2), loop_task).await;
    }

    let captured = reqs.lock().unwrap().clone();
    assert_eq!(captured.len(), 3);

    // 1. Gemini payload
    let gem_str = captured[0].body.to_string();
    assert!(gem_str.contains("list_dir"), "Gemini missing reverse-mapped tool ls -> list_dir");
    assert!(gem_str.contains("run_shell"), "Gemini missing reverse-mapped tool bash -> run_shell");
    assert!(gem_str.contains("Cargo.toml\\nsrc/"));
    assert!(!gem_str.contains("Checking dir..."), "Synthetic thought without signature must not be replayed");

    // 2. OpenAI payload
    let oai_msgs = captured[1].body["messages"].as_array().unwrap();
    assert!(oai_msgs.iter().any(|m| m["role"] == "assistant" && m["tool_calls"].is_array()));
    assert!(oai_msgs.iter().any(|m| m["role"] == "tool" && m["tool_call_id"] == "c1"));
    assert!(oai_msgs.iter().any(|m| m["role"] == "tool" && m["tool_call_id"] == "c3"));

    // 3. Anthropic payload: tool_use + merged user tool_result + strict role alternation
    let ant_msgs = captured[2].body["messages"].as_array().unwrap();
    assert!(ant_msgs.iter().any(|m| {
        m["role"] == "user"
            && m["content"]
                .as_array()
                .map(|arr| {
                    arr.iter().filter(|b| b["type"] == "tool_result").count() == 2
                })
                .unwrap_or(false)
    }));
    for pair in ant_msgs.windows(2) {
        assert_ne!(pair[0]["role"], pair[1]["role"], "Anthropic requires strict role alternation");
    }

    mock_handle.abort();
}

#[tokio::test]
async fn reg003_get_session_stats_full_fields() {
    let _sink_guard = output_sink_test_mutex().lock().await;
    let (_reset, mut out_rx) = install_channel_output_sink();
    let tmp = TempConfigGuard::new("reg003_stats");

    let (base_url, _reqs, mock_handle) = spawn_mock_llm_server(|_idx, _req| async move {
        (200, gemini_text_resp("jawaban stats", 420, 80))
    })
    .await;
    write_mock_provider_config(
        tmp.path(),
        "google",
        "google-generative-ai",
        "gemini-2.5-flash",
        &format!("{}/v1beta", base_url),
        "k-stats",
    );

    let session_file = tmp.path().join("stats.jsonl");
    let seed_lines = [
        json!({ "type": "session", "version": 3, "id": "s-stats", "timestamp": "2026-11-12T00:00:00Z", "cwd": tmp.path() }).to_string(),
        json!({ "type": "message", "id": "e0", "parentId": null, "timestamp": "2026-11-12T00:00:01Z",
            "message": { "role": "user", "content": "u1", "timestamp": 1 } }).to_string(),
        json!({ "type": "message", "id": "e1", "parentId": "e0", "timestamp": "2026-11-12T00:00:02Z",
            "message": { "role": "assistant", "stopReason": "toolUse",
                "content": [{ "type": "toolCall", "id": "t1", "name": "read", "arguments": { "path": "a" } }],
                "usage": { "input": 100, "output": 25, "totalTokens": 125 }, "timestamp": 2 } }).to_string(),
        json!({ "type": "message", "id": "e2", "parentId": "e1", "timestamp": "2026-11-12T00:00:03Z",
            "message": { "role": "toolResult", "toolCallId": "t1", "toolName": "read", "isError": false,
                "content": [{ "type": "text", "text": "isi" }], "timestamp": 3 } }).to_string(),
        json!({ "type": "message", "id": "e3", "parentId": "e2", "timestamp": "2026-11-12T00:00:04Z",
            "message": { "role": "assistant", "stopReason": "stop",
                "content": [{ "type": "text", "text": "selesai" }],
                "usage": { "input": 200, "output": 30, "totalTokens": 230 }, "timestamp": 4 } }).to_string(),
    ];
    std::fs::write(&session_file, format!("{}\n", seed_lines.join("\n"))).unwrap();

    let mut server = RpcServer::new_in_dir_with_cwd(
        tmp.path().to_path_buf(),
        tmp.path().to_path_buf(),
        Some(session_file.to_string_lossy().to_string()),
        None,
        vec![],
    )
    .unwrap();

    let (cmd_tx, reader) = TestLineReader::new();
    let loop_task = tokio::spawn(async move {
        server.run_loop_with_input(io::BufReader::new(reader)).await.unwrap();
    });
    let _ = recv_matching(&mut out_rx, std::time::Duration::from_secs(2), |v| {
        v["topic"] == "pipkin:hello"
    })
    .await;

    send_cmd(&cmd_tx, json!({ "id": "p1", "type": "prompt", "message": "halo" }));
    let _ = recv_response(&mut out_rx, "p1", std::time::Duration::from_millis(500)).await;
    let _ = recv_matching(&mut out_rx, std::time::Duration::from_secs(2), |v| {
        v["type"] == "agent_settled"
    })
    .await;

    send_cmd(&cmd_tx, json!({ "id": "st", "type": "get_session_stats" }));
    let st = recv_response(&mut out_rx, "st", std::time::Duration::from_millis(500)).await;
    let data = &st["data"];
    assert_eq!(data["sessionId"], "s-stats");
    assert!(data["sessionFile"].as_str().unwrap_or("").ends_with("stats.jsonl"));
    assert_eq!(data["userMessages"], 2);
    assert_eq!(data["assistantMessages"], 3);
    assert_eq!(data["toolCalls"], 1);
    assert_eq!(data["toolResults"], 1);
    assert_eq!(data["totalMessages"], 6);
    assert_eq!(data["tokens"]["input"], 720); // 100 + 200 + 420
    assert_eq!(data["tokens"]["output"], 135); // 25 + 30 + 80
    assert_eq!(data["tokens"]["total"], 855);
    assert_eq!(data["contextUsage"]["tokens"], 500); // last prompt+out (420+80)
    assert_eq!(data["contextUsage"]["contextWindow"], 128000);

    drop(cmd_tx);
    let _ = tokio::time::timeout(std::time::Duration::from_secs(2), loop_task).await;
    mock_handle.abort();
}

#[tokio::test]
async fn reg003_compact_summarizes_prunes_history_and_reloads() {
    let _sink_guard = output_sink_test_mutex().lock().await;
    let (_reset, mut out_rx) = install_channel_output_sink();
    let tmp = TempConfigGuard::new("reg003_compact");

    let (base_url, reqs, mock_handle) = spawn_mock_llm_server(|_idx, req| async move {
        let s = req.body.to_string();
        if s.contains("Focus on Rust") || s.contains("Ringkas") || s.contains("summarize") {
            (200, gemini_text_resp("## Goal\nRingkasan kompak terfokus Rust", 250, 40))
        } else {
            (200, gemini_text_resp("Pasca kompak oke", 90, 20))
        }
    })
    .await;
    write_mock_provider_config(
        tmp.path(),
        "google",
        "google-generative-ai",
        "gemini-2.5-flash",
        &format!("{}/v1beta", base_url),
        "k-compact",
    );

    let session_file = tmp.path().join("compact.jsonl");
    let lines = [
        json!({ "type": "session", "version": 3, "id": "s-cmp", "timestamp": "2026-11-12T00:00:00Z", "cwd": tmp.path() }).to_string(),
        json!({ "type": "message", "id": "e0", "parentId": null, "timestamp": "2026-11-12T00:00:01Z",
            "message": { "role": "user", "content": "pesan lama pertama yang akan diringkas", "timestamp": 1 } }).to_string(),
        json!({ "type": "message", "id": "e1", "parentId": "e0", "timestamp": "2026-11-12T00:00:02Z",
            "message": { "role": "assistant", "stopReason": "stop",
                "content": [{ "type": "text", "text": "balasan lama pertama" }], "timestamp": 2 } }).to_string(),
        json!({ "type": "message", "id": "e2", "parentId": "e1", "timestamp": "2026-11-12T00:00:03Z",
            "message": { "role": "user", "content": "pesan lama kedua juga diringkas", "timestamp": 3 } }).to_string(),
        json!({ "type": "message", "id": "e3", "parentId": "e2", "timestamp": "2026-11-12T00:00:04Z",
            "message": { "role": "assistant", "stopReason": "stop",
                "content": [{ "type": "text", "text": "balasan lama kedua" }], "timestamp": 4 } }).to_string(),
        json!({ "type": "message", "id": "e4", "parentId": "e3", "timestamp": "2026-11-12T00:00:05Z",
            "message": { "role": "user", "content": "pertanyaan terbaru dipertahankan", "timestamp": 5 } }).to_string(),
        json!({ "type": "message", "id": "e5", "parentId": "e4", "timestamp": "2026-11-12T00:00:06Z",
            "message": { "role": "assistant", "stopReason": "stop",
                "content": [{ "type": "text", "text": "jawaban terbaru dipertahankan" }], "timestamp": 6 } }).to_string(),
    ];
    std::fs::write(&session_file, format!("{}\n", lines.join("\n"))).unwrap();

    let mut server = RpcServer::new_in_dir_with_cwd(
        tmp.path().to_path_buf(),
        tmp.path().to_path_buf(),
        Some(session_file.to_string_lossy().to_string()),
        None,
        vec![],
    )
    .unwrap();

    let (cmd_tx, reader) = TestLineReader::new();
    let loop_task = tokio::spawn(async move {
        server.run_loop_with_input(io::BufReader::new(reader)).await.unwrap();
    });
    let _ = recv_matching(&mut out_rx, std::time::Duration::from_secs(2), |v| {
        v["topic"] == "pipkin:hello"
    })
    .await;

    send_cmd(
        &cmd_tx,
        json!({ "id": "cmp1", "type": "compact", "customInstructions": "Focus on Rust" }),
    );
    let cmp_start = recv_matching(&mut out_rx, std::time::Duration::from_secs(2), |v| {
        v["type"] == "compaction_start"
    })
    .await;
    assert_eq!(cmp_start["reason"], "manual");

    let cmp_end = recv_matching(&mut out_rx, std::time::Duration::from_secs(2), |v| {
        v["type"] == "compaction_end"
    })
    .await;
    assert_eq!(cmp_end["aborted"], false);
    assert!(cmp_end["result"]["summary"]
        .as_str()
        .unwrap_or("")
        .contains("Ringkasan kompak terfokus Rust"));

    let cmp_resp = recv_response(&mut out_rx, "cmp1", std::time::Duration::from_secs(2)).await;
    assert_eq!(cmp_resp["success"], true);
    assert!(cmp_resp["data"]["firstKeptEntryId"].as_str().is_some());
    assert!(cmp_resp["data"]["tokensBefore"].as_u64().unwrap_or(0) > 0);
    assert!(cmp_resp["data"]["estimatedTokensAfter"].as_u64().unwrap_or(0) > 0);

    // Persis setelah compact, contextUsage.tokens & percent HARUS null
    send_cmd(&cmd_tx, json!({ "id": "st_after", "type": "get_session_stats" }));
    let st_after = recv_response(&mut out_rx, "st_after", std::time::Duration::from_millis(500)).await;
    assert!(st_after["data"]["contextUsage"]["tokens"].is_null());
    assert!(st_after["data"]["contextUsage"]["percent"].is_null());

    // Kirim prompt lanjutan -> LLM melihat ringkasan + pesan terbaru, BUKAN pesan awal yg terpotong
    send_cmd(&cmd_tx, json!({ "id": "p2", "type": "prompt", "message": "lanjut" }));
    let _ = recv_response(&mut out_rx, "p2", std::time::Duration::from_millis(500)).await;
    let _ = recv_matching(&mut out_rx, std::time::Duration::from_secs(2), |v| {
        v["type"] == "agent_settled"
    })
    .await;

    let captured = reqs.lock().unwrap().clone();
    assert!(captured.len() >= 2);
    let post_compact_body = captured.last().unwrap().body.to_string();
    assert!(post_compact_body.contains("Ringkasan kompak terfokus Rust"));
    assert!(!post_compact_body.contains("pesan lama pertama yang akan diringkas"));

    drop(cmd_tx);
    let _ = tokio::time::timeout(std::time::Duration::from_secs(2), loop_task).await;

    // Pastikan file JSONL mengandung entry compaction dan reload merekonstruksi konteks terkompresi
    let saved = std::fs::read_to_string(&session_file).unwrap();
    assert!(saved.contains("\"type\":\"compaction\""));
    let reloaded = RpcServer::new_in_dir_with_cwd(
        tmp.path().to_path_buf(),
        tmp.path().to_path_buf(),
        Some(session_file.to_string_lossy().to_string()),
        None,
        vec![],
    )
    .unwrap();
    let hist_str = serde_json::to_string(&reloaded.agent.active_history()).unwrap();
    assert!(hist_str.contains("Ringkasan kompak terfokus Rust"));
    assert!(!hist_str.contains("pesan lama pertama yang akan diringkas"));

    mock_handle.abort();
}

#[tokio::test]
async fn reg003_fork_clone_and_export_html() {
    let _sink_guard = output_sink_test_mutex().lock().await;
    let (_reset, mut out_rx) = install_channel_output_sink();
    let tmp = TempConfigGuard::new("reg003_fork_clone_export");

    let session_file = tmp.path().join("orig.jsonl");
    let lines = [
        json!({ "type": "session", "version": 3, "id": "s-orig", "timestamp": "2026-11-12T00:00:00Z", "cwd": tmp.path() }).to_string(),
        json!({ "type": "message", "id": "e0", "parentId": null, "timestamp": "2026-11-12T00:00:01Z",
            "message": { "role": "user", "content": [{ "type": "text", "text": "prompt satu" }], "timestamp": 100 } }).to_string(),
        json!({ "type": "message", "id": "e1", "parentId": "e0", "timestamp": "2026-11-12T00:00:02Z",
            "message": { "role": "assistant", "stopReason": "stop",
                "content": [{ "type": "text", "text": "jawaban <satu>" }], "timestamp": 200 } }).to_string(),
        json!({ "type": "message", "id": "e2", "parentId": "e1", "timestamp": "2026-11-12T00:00:03Z",
            "message": { "role": "user", "content": [{ "type": "text", "text": "prompt dua" }], "timestamp": 300 } }).to_string(),
        json!({ "type": "message", "id": "e3", "parentId": "e2", "timestamp": "2026-11-12T00:00:04Z",
            "message": { "role": "assistant", "stopReason": "stop",
                "content": [{ "type": "text", "text": "jawaban dua" }], "timestamp": 400 } }).to_string(),
    ];
    std::fs::write(&session_file, format!("{}\n", lines.join("\n"))).unwrap();

    let mut server = RpcServer::new_in_dir_with_cwd(
        tmp.path().to_path_buf(),
        tmp.path().to_path_buf(),
        Some(session_file.to_string_lossy().to_string()),
        None,
        vec![],
    )
    .unwrap();

    let (cmd_tx, reader) = TestLineReader::new();
    let loop_task = tokio::spawn(async move {
        server.run_loop_with_input(io::BufReader::new(reader)).await.unwrap();
    });
    let _ = recv_matching(&mut out_rx, std::time::Duration::from_secs(2), |v| {
        v["topic"] == "pipkin:hello"
    })
    .await;

    // 1. get_fork_messages
    send_cmd(&cmd_tx, json!({ "id": "gfm", "type": "get_fork_messages" }));
    let gfm = recv_response(&mut out_rx, "gfm", std::time::Duration::from_millis(500)).await;
    assert_eq!(
        gfm["data"]["messages"],
        json!([
            { "entryId": "e0", "text": "prompt satu" },
            { "entryId": "e2", "text": "prompt dua" }
        ])
    );

    // 2. fork pada e2 -> memotong tepat sebelum e2, pindah ke file baru dengan parentSession
    send_cmd(&cmd_tx, json!({ "id": "fk", "type": "fork", "entryId": "e2" }));
    let fk = recv_response(&mut out_rx, "fk", std::time::Duration::from_millis(500)).await;
    assert_eq!(fk["success"], true);
    assert_eq!(fk["data"]["text"], "prompt dua");
    assert_eq!(fk["data"]["cancelled"], false);

    send_cmd(&cmd_tx, json!({ "id": "gs_fk", "type": "get_state" }));
    let gs_fk = recv_response(&mut out_rx, "gs_fk", std::time::Duration::from_millis(500)).await;
    let forked_path = gs_fk["data"]["sessionFile"].as_str().unwrap().to_string();
    assert_ne!(forked_path, session_file.to_string_lossy().to_string());
    let forked_contents = std::fs::read_to_string(&forked_path).unwrap();
    assert!(forked_contents.contains("\"parentSession\""));

    send_cmd(&cmd_tx, json!({ "id": "gm_fk", "type": "get_messages" }));
    let gm_fk = recv_response(&mut out_rx, "gm_fk", std::time::Duration::from_millis(500)).await;
    assert_eq!(gm_fk["data"]["messages"].as_array().unwrap().len(), 2);

    // 3. clone -> salin branch aktif saat ini ke file baru lainnya
    send_cmd(&cmd_tx, json!({ "id": "cl", "type": "clone" }));
    let cl = recv_response(&mut out_rx, "cl", std::time::Duration::from_millis(500)).await;
    assert_eq!(cl["success"], true);
    assert_eq!(cl["data"]["cancelled"], false);

    send_cmd(&cmd_tx, json!({ "id": "gs_cl", "type": "get_state" }));
    let gs_cl = recv_response(&mut out_rx, "gs_cl", std::time::Duration::from_millis(500)).await;
    let cloned_path = gs_cl["data"]["sessionFile"].as_str().unwrap().to_string();
    assert_ne!(cloned_path, forked_path);
    assert!(std::fs::read_to_string(&cloned_path).unwrap().contains("\"parentSession\""));

    // 4. export_html (default blank outputPath & relative outputPath)
    send_cmd(&cmd_tx, json!({ "id": "ex1", "type": "export_html" }));
    let ex1 = recv_response(&mut out_rx, "ex1", std::time::Duration::from_millis(500)).await;
    assert_eq!(ex1["success"], true);
    let html_path = ex1["data"]["path"].as_str().unwrap();
    assert!(html_path.ends_with(".html"));
    let html = std::fs::read_to_string(html_path).unwrap();
    assert!(html.contains("prompt satu"));
    assert!(html.contains("jawaban &lt;satu&gt;"));

    drop(cmd_tx);
    let _ = tokio::time::timeout(std::time::Duration::from_secs(2), loop_task).await;
}

#[tokio::test]
async fn reg002_abort_manual_compact_and_prompt_during_compact() {
    let _sink_guard = output_sink_test_mutex().lock().await;
    let (_reset, mut out_rx) = install_channel_output_sink();
    let tmp = TempConfigGuard::new("reg002_abort_compact");

    let gate1 = Arc::new(tokio::sync::Notify::new());
    let gate2 = Arc::new(tokio::sync::Notify::new());
    let g1_srv = Arc::clone(&gate1);
    let g2_srv = Arc::clone(&gate2);
    let (hit0_tx, hit0_rx) = tokio::sync::oneshot::channel::<()>();
    let (hit1_tx, hit1_rx) = tokio::sync::oneshot::channel::<()>();
    let hit0_tx = Arc::new(Mutex::new(Some(hit0_tx)));
    let hit1_tx = Arc::new(Mutex::new(Some(hit1_tx)));

    let (base_url, reqs, mock_handle) = spawn_mock_llm_server(move |idx, _req| {
        let g1_srv = Arc::clone(&g1_srv);
        let g2_srv = Arc::clone(&g2_srv);
        let hit0_tx = Arc::clone(&hit0_tx);
        let hit1_tx = Arc::clone(&hit1_tx);
        async move {
            match idx {
                0 => {
                    if let Some(tx) = hit0_tx.lock().unwrap().take() {
                        let _ = tx.send(());
                    }
                    g1_srv.notified().await;
                    (200, gemini_text_resp("tidak dipakai karena diabort", 20, 5))
                }
                1 => {
                    if let Some(tx) = hit1_tx.lock().unwrap().take() {
                        let _ = tx.send(());
                    }
                    g2_srv.notified().await;
                    (
                        200,
                        gemini_text_resp("Ringkasan kompak sukses sebelum prompt antre", 150, 30),
                    )
                }
                _ => (200, gemini_text_resp("Prompt setelah compact selesai", 80, 15)),
            }
        }
    })
    .await;
    write_mock_provider_config(
        tmp.path(),
        "google",
        "google-generative-ai",
        "gemini-2.5-flash",
        &format!("{}/v1beta", base_url),
        "k-compact-abort",
    );

    let session_file = tmp.path().join("compact_abort.jsonl");
    let seed_lines = [
        json!({ "type": "session", "version": 3, "id": "s-ca", "timestamp": "2026-11-12T00:00:00Z", "cwd": tmp.path() }).to_string(),
        json!({ "type": "message", "id": "e0", "parentId": null, "timestamp": "2026-11-12T00:00:01Z",
            "message": { "role": "user", "content": "pesan lama satu", "timestamp": 10 } }).to_string(),
        json!({ "type": "message", "id": "e1", "parentId": "e0", "timestamp": "2026-11-12T00:00:02Z",
            "message": { "role": "assistant", "stopReason": "stop",
                "content": [{ "type": "text", "text": "jawaban lama satu" }], "timestamp": 20 } }).to_string(),
        json!({ "type": "message", "id": "e2", "parentId": "e1", "timestamp": "2026-11-12T00:00:03Z",
            "message": { "role": "user", "content": "pesan terbaru dua", "timestamp": 30 } }).to_string(),
        json!({ "type": "message", "id": "e3", "parentId": "e2", "timestamp": "2026-11-12T00:00:04Z",
            "message": { "role": "assistant", "stopReason": "stop",
                "content": [{ "type": "text", "text": "jawaban terbaru dua" }], "timestamp": 40 } }).to_string(),
    ];
    std::fs::write(&session_file, format!("{}\n", seed_lines.join("\n"))).unwrap();

    let mut server = RpcServer::new_in_dir_with_cwd(
        tmp.path().to_path_buf(),
        tmp.path().to_path_buf(),
        Some(session_file.to_string_lossy().to_string()),
        None,
        vec![],
    )
    .unwrap();

    let (cmd_tx, reader) = TestLineReader::new();
    let loop_task = tokio::spawn(async move {
        server
            .run_loop_with_input(io::BufReader::new(reader))
            .await
            .unwrap();
    });
    let _ = recv_matching(&mut out_rx, std::time::Duration::from_secs(2), |v| {
        v["topic"] == "pipkin:hello"
    })
    .await;

    // Skenario A: abort di tengah compact manual
    send_cmd(&cmd_tx, json!({ "id": "cmp_ab", "type": "compact" }));
    let _ = recv_matching(&mut out_rx, std::time::Duration::from_secs(2), |v| {
        v["type"] == "compaction_start"
    })
    .await;
    tokio::time::timeout(std::time::Duration::from_secs(2), hit0_rx)
        .await
        .unwrap()
        .unwrap();

    send_cmd(&cmd_tx, json!({ "id": "gs_cmp", "type": "get_state" }));
    let gs_cmp = recv_response(&mut out_rx, "gs_cmp", std::time::Duration::from_millis(400)).await;
    assert_eq!(gs_cmp["data"]["isCompacting"], true);
    assert_eq!(gs_cmp["data"]["isStreaming"], false);

    let t0 = std::time::Instant::now();
    send_cmd(&cmd_tx, json!({ "id": "ab_cmp", "type": "abort" }));
    let cmp_end_ab = recv_matching(&mut out_rx, std::time::Duration::from_millis(600), |v| {
        v["type"] == "compaction_end"
    })
    .await;
    assert_eq!(cmp_end_ab["aborted"], true);
    let cmp_ab_resp =
        recv_response(&mut out_rx, "cmp_ab", std::time::Duration::from_millis(600)).await;
    assert_eq!(cmp_ab_resp["success"], false);
    let ab_cmp_resp =
        recv_response(&mut out_rx, "ab_cmp", std::time::Duration::from_millis(600)).await;
    assert_eq!(ab_cmp_resp["success"], true);
    assert!(t0.elapsed() < std::time::Duration::from_millis(600));

    send_cmd(&cmd_tx, json!({ "id": "gs_after_ab", "type": "get_state" }));
    let gs_after_ab =
        recv_response(&mut out_rx, "gs_after_ab", std::time::Duration::from_millis(400)).await;
    assert_eq!(gs_after_ab["data"]["isCompacting"], false);
    assert_eq!(gs_after_ab["data"]["messageCount"], 4);

    // Skenario B: prompt dg streamingBehavior="steer" diantrekan saat compact berjalan
    // harus otomatis dieksekusi segera sesudah compact selesai.
    send_cmd(&cmd_tx, json!({ "id": "cmp_ok", "type": "compact" }));
    let _ = recv_matching(&mut out_rx, std::time::Duration::from_secs(2), |v| {
        v["type"] == "compaction_start"
    })
    .await;
    tokio::time::timeout(std::time::Duration::from_secs(2), hit1_rx)
        .await
        .unwrap()
        .unwrap();

    send_cmd(
        &cmd_tx,
        json!({
            "id": "p_queued",
            "type": "prompt",
            "message": "prompt setelah compact",
            "streamingBehavior": "steer"
        }),
    );
    let q_ev = recv_matching(&mut out_rx, std::time::Duration::from_millis(400), |v| {
        v["type"] == "queue_update" && v["steering"] == json!(["prompt setelah compact"])
    })
    .await;
    assert_eq!(q_ev["followUp"], json!([]));
    let p_q_resp =
        recv_response(&mut out_rx, "p_queued", std::time::Duration::from_millis(400)).await;
    assert_eq!(p_q_resp["success"], true);
    assert_eq!(p_q_resp["data"]["disposition"], "queued");

    // Lepas compact kedua -> harus selesai lalu auto-start prompt yang diantrekan
    gate2.notify_one();
    let cmp_end_ok = recv_matching(&mut out_rx, std::time::Duration::from_secs(2), |v| {
        v["type"] == "compaction_end"
    })
    .await;
    assert_eq!(cmp_end_ok["aborted"], false);
    let cmp_ok_resp =
        recv_response(&mut out_rx, "cmp_ok", std::time::Duration::from_secs(2)).await;
    assert_eq!(cmp_ok_resp["success"], true);

    let q_cleared = recv_matching(&mut out_rx, std::time::Duration::from_secs(2), |v| {
        v["type"] == "queue_update" && v["steering"] == json!([])
    })
    .await;
    assert_eq!(q_cleared["followUp"], json!([]));

    let settled = recv_matching(&mut out_rx, std::time::Duration::from_secs(3), |v| {
        v["type"] == "agent_settled"
    })
    .await;
    assert_eq!(settled["aborted"], false);

    let captured = reqs.lock().unwrap().clone();
    assert!(captured.len() >= 3);
    let resumed_body = captured.last().unwrap().body.to_string();
    assert!(resumed_body.contains("Ringkasan kompak sukses sebelum prompt antre"));
    assert!(resumed_body.contains("prompt setelah compact"));

    drop(cmd_tx);
    let _ = tokio::time::timeout(std::time::Duration::from_secs(2), loop_task).await;
    mock_handle.abort();
}

#[tokio::test]
async fn reg002_follow_up_modes_and_retry_abort_semantics() {
    let _sink_guard = output_sink_test_mutex().lock().await;
    let (_reset, mut out_rx) = install_channel_output_sink();
    let tmp = TempConfigGuard::new("reg002_retry_followup");

    let gate0 = Arc::new(tokio::sync::Notify::new());
    let gate0_srv = Arc::clone(&gate0);
    let (hit0_tx, hit0_rx) = tokio::sync::oneshot::channel::<()>();
    let hit0_tx = Arc::new(Mutex::new(Some(hit0_tx)));

    let (base_url, reqs, mock_handle) = spawn_mock_llm_server(move |idx, _req| {
        let gate0_srv = Arc::clone(&gate0_srv);
        let hit0_tx = Arc::clone(&hit0_tx);
        async move {
            match idx {
                0 => {
                    if let Some(tx) = hit0_tx.lock().unwrap().take() {
                        let _ = tx.send(());
                    }
                    gate0_srv.notified().await;
                    (200, gemini_text_resp("Turn utama beres", 40, 10))
                }
                1 => (200, gemini_text_resp("Follow up batch beres", 50, 10)),
                2 => (
                    429,
                    json!({ "error": { "message": "429 rate limit sementara" } }),
                ),
                3 => (200, gemini_text_resp("Pulih sesudah retry", 60, 12)),
                4 => (
                    429,
                    json!({ "error": { "message": "429 throttle abort_retry" } }),
                ),
                _ => (
                    429,
                    json!({ "error": { "message": "429 throttle abort_run" } }),
                ),
            }
        }
    })
    .await;
    write_mock_provider_config(
        tmp.path(),
        "google",
        "google-generative-ai",
        "gemini-2.5-flash",
        &format!("{}/v1beta", base_url),
        "k-retry",
    );

    let mut server = RpcServer::new_in_dir_with_cwd(
        tmp.path().to_path_buf(),
        tmp.path().to_path_buf(),
        None,
        None,
        vec![],
    )
    .unwrap();

    let (cmd_tx, reader) = TestLineReader::new();
    let loop_task = tokio::spawn(async move {
        server
            .run_loop_with_input(io::BufReader::new(reader))
            .await
            .unwrap();
    });
    let _ = recv_matching(&mut out_rx, std::time::Duration::from_secs(2), |v| {
        v["topic"] == "pipkin:hello"
    })
    .await;

    // 1. Verifikasi set_steering_mode, set_follow_up_mode, set_auto_compaction, set_auto_retry + validasi error
    send_cmd(
        &cmd_tx,
        json!({ "id": "m_bad", "type": "set_steering_mode", "mode": "random" }),
    );
    let m_bad = recv_response(&mut out_rx, "m_bad", std::time::Duration::from_millis(400)).await;
    assert_eq!(m_bad["success"], false);

    send_cmd(
        &cmd_tx,
        json!({ "id": "sm", "type": "set_steering_mode", "mode": "all" }),
    );
    send_cmd(
        &cmd_tx,
        json!({ "id": "fm", "type": "set_follow_up_mode", "mode": "all" }),
    );
    send_cmd(
        &cmd_tx,
        json!({ "id": "ac", "type": "set_auto_compaction", "enabled": false }),
    );
    send_cmd(
        &cmd_tx,
        json!({ "id": "ar", "type": "set_auto_retry", "enabled": true }),
    );
    for cid in ["sm", "fm", "ac", "ar"] {
        let r = recv_response(&mut out_rx, cid, std::time::Duration::from_millis(400)).await;
        assert_eq!(r["success"], true);
    }

    send_cmd(&cmd_tx, json!({ "id": "gs_cfg", "type": "get_state" }));
    let gs_cfg =
        recv_response(&mut out_rx, "gs_cfg", std::time::Duration::from_millis(400)).await;
    assert_eq!(gs_cfg["data"]["steeringMode"], "all");
    assert_eq!(gs_cfg["data"]["followUpMode"], "all");
    assert_eq!(gs_cfg["data"]["autoCompactionEnabled"], false);
    assert_eq!(gs_cfg["data"]["autoRetryEnabled"], true);

    // 2. Uji follow_up batching dalam followUpMode = "all"
    send_cmd(
        &cmd_tx,
        json!({ "id": "p_main", "type": "prompt", "message": "jalan utama" }),
    );
    let _ = recv_response(&mut out_rx, "p_main", std::time::Duration::from_millis(400)).await;
    tokio::time::timeout(std::time::Duration::from_secs(2), hit0_rx)
        .await
        .unwrap()
        .unwrap();

    send_cmd(
        &cmd_tx,
        json!({ "id": "fu1", "type": "follow_up", "message": "follow satu" }),
    );
    send_cmd(
        &cmd_tx,
        json!({
            "id": "fu2",
            "type": "prompt",
            "message": "follow dua",
            "streamingBehavior": "followUp"
        }),
    );
    let fu1_r = recv_response(&mut out_rx, "fu1", std::time::Duration::from_millis(400)).await;
    let fu2_r = recv_response(&mut out_rx, "fu2", std::time::Duration::from_millis(400)).await;
    assert_eq!(fu1_r["data"]["disposition"], "queued");
    assert_eq!(fu2_r["data"]["disposition"], "queued");

    gate0.notify_one();
    let settled_fu = recv_matching(&mut out_rx, std::time::Duration::from_secs(3), |v| {
        v["type"] == "agent_settled"
    })
    .await;
    assert_eq!(settled_fu["aborted"], false);
    let reqs_snapshot = reqs.lock().unwrap().clone();
    assert_eq!(reqs_snapshot.len(), 2);
    let batch_body = reqs_snapshot[1].body.to_string();
    assert!(batch_body.contains("follow satu"));
    assert!(batch_body.contains("follow dua"));

    // 3. Auto-retry pada 429 lalu sukses pada percobaan kedua
    send_cmd(
        &cmd_tx,
        json!({ "id": "p_retry", "type": "prompt", "message": "uji retry 429" }),
    );
    let _ = recv_response(&mut out_rx, "p_retry", std::time::Duration::from_millis(400)).await;
    let rs_ok = recv_matching(&mut out_rx, std::time::Duration::from_secs(2), |v| {
        v["type"] == "auto_retry_start"
    })
    .await;
    assert_eq!(rs_ok["attempt"], 1);
    let re_ok = recv_matching(&mut out_rx, std::time::Duration::from_secs(3), |v| {
        v["type"] == "auto_retry_end"
    })
    .await;
    assert_eq!(re_ok["success"], true);
    assert_eq!(re_ok["attempt"], 2);
    let settled_r = recv_matching(&mut out_rx, std::time::Duration::from_secs(2), |v| {
        v["type"] == "agent_settled"
    })
    .await;
    assert_eq!(settled_r["aborted"], false);

    // 4. abort_retry membatalkan loop retry dan menghasilkan stopReason="error"
    send_cmd(
        &cmd_tx,
        json!({ "id": "p_abr", "type": "prompt", "message": "uji abort_retry" }),
    );
    let _ = recv_response(&mut out_rx, "p_abr", std::time::Duration::from_millis(400)).await;
    let _ = recv_matching(&mut out_rx, std::time::Duration::from_secs(2), |v| {
        v["type"] == "auto_retry_start"
    })
    .await;
    send_cmd(&cmd_tx, json!({ "id": "abr", "type": "abort_retry" }));
    let abr_r = recv_response(&mut out_rx, "abr", std::time::Duration::from_millis(400)).await;
    assert_eq!(abr_r["success"], true);
    let re_abr = recv_matching(&mut out_rx, std::time::Duration::from_secs(2), |v| {
        v["type"] == "auto_retry_end"
    })
    .await;
    assert_eq!(re_abr["success"], false);
    let err_msg_ev = recv_matching(&mut out_rx, std::time::Duration::from_secs(2), |v| {
        v["type"] == "message_end" && v["message"]["stopReason"] == "error"
    })
    .await;
    assert_eq!(err_msg_ev["message"]["role"], "assistant");
    let _ = recv_matching(&mut out_rx, std::time::Duration::from_secs(2), |v| {
        v["type"] == "agent_settled" && v["aborted"] == false
    })
    .await;

    // 5. abort penuh saat retry sleep -> auto_retry_end + abort_msg (timestamp == user_msg.timestamp) + turn_end + agent_end + agent_settled
    send_cmd(
        &cmd_tx,
        json!({ "id": "p_ab_run", "type": "prompt", "message": "uji abort saat retry" }),
    );
    let _ = recv_response(&mut out_rx, "p_ab_run", std::time::Duration::from_millis(400)).await;
    let user_ev = recv_matching(&mut out_rx, std::time::Duration::from_secs(2), |v| {
        v["type"] == "message_end"
            && v["message"]["role"] == "user"
            && v["message"]["content"][0]["text"] == "uji abort saat retry"
    })
    .await;
    let user_ts = user_ev["message"]["timestamp"].as_u64().unwrap();
    let _ = recv_matching(&mut out_rx, std::time::Duration::from_secs(2), |v| {
        v["type"] == "auto_retry_start"
    })
    .await;

    send_cmd(&cmd_tx, json!({ "id": "ab_run", "type": "abort" }));
    let re_ab_run = recv_matching(&mut out_rx, std::time::Duration::from_millis(600), |v| {
        v["type"] == "auto_retry_end"
    })
    .await;
    assert_eq!(re_ab_run["success"], false);
    let abort_msg_ev = recv_matching(&mut out_rx, std::time::Duration::from_millis(600), |v| {
        v["type"] == "message_end" && v["message"]["stopReason"] == "aborted"
    })
    .await;
    assert_eq!(abort_msg_ev["message"]["timestamp"].as_u64(), Some(user_ts));
    let _ = recv_matching(&mut out_rx, std::time::Duration::from_millis(600), |v| {
        v["type"] == "turn_end"
    })
    .await;
    let _ = recv_matching(&mut out_rx, std::time::Duration::from_millis(600), |v| {
        v["type"] == "agent_end"
    })
    .await;
    let settled_ab = recv_matching(&mut out_rx, std::time::Duration::from_millis(600), |v| {
        v["type"] == "agent_settled"
    })
    .await;
    assert_eq!(settled_ab["aborted"], true);
    let ab_run_r =
        recv_response(&mut out_rx, "ab_run", std::time::Duration::from_millis(600)).await;
    assert_eq!(ab_run_r["success"], true);

    drop(cmd_tx);
    let _ = tokio::time::timeout(std::time::Duration::from_secs(2), loop_task).await;
    mock_handle.abort();
}

#[tokio::test]
async fn reg003_fork_root_new_switch_entries_and_relative_export() {
    let _sink_guard = output_sink_test_mutex().lock().await;
    let (_reset, mut out_rx) = install_channel_output_sink();
    let tmp = TempConfigGuard::new("reg003_nav");

    let session_file = tmp.path().join("orig.jsonl");
    let lines = [
        json!({ "type": "session", "version": 3, "id": "s-nav", "timestamp": "2026-11-12T00:00:00Z", "cwd": tmp.path() }).to_string(),
        json!({ "type": "message", "id": "e0", "parentId": null, "timestamp": "2026-11-12T00:00:01Z",
            "message": { "role": "user", "content": [{ "type": "text", "text": "prompt satu" }], "timestamp": 100 } }).to_string(),
        json!({ "type": "message", "id": "e1", "parentId": "e0", "timestamp": "2026-11-12T00:00:02Z",
            "message": { "role": "assistant", "stopReason": "stop",
                "content": [{ "type": "text", "text": "jawaban satu" }], "timestamp": 200 } }).to_string(),
        json!({ "type": "message", "id": "e2", "parentId": "e1", "timestamp": "2026-11-12T00:00:03Z",
            "message": { "role": "user", "content": [{ "type": "text", "text": "prompt dua" }], "timestamp": 300 } }).to_string(),
        json!({ "type": "message", "id": "e3", "parentId": "e2", "timestamp": "2026-11-12T00:00:04Z",
            "message": { "role": "assistant", "stopReason": "stop",
                "content": [{ "type": "text", "text": "jawaban dua" }], "timestamp": 400 } }).to_string(),
    ];
    std::fs::write(&session_file, format!("{}\n", lines.join("\n"))).unwrap();

    let mut server = RpcServer::new_in_dir_with_cwd(
        tmp.path().to_path_buf(),
        tmp.path().to_path_buf(),
        Some(session_file.to_string_lossy().to_string()),
        None,
        vec![],
    )
    .unwrap();

    let (cmd_tx, reader) = TestLineReader::new();
    let loop_task = tokio::spawn(async move {
        server
            .run_loop_with_input(io::BufReader::new(reader))
            .await
            .unwrap();
    });
    let _ = recv_matching(&mut out_rx, std::time::Duration::from_secs(2), |v| {
        v["topic"] == "pipkin:hello"
    })
    .await;

    // 1. get_entries full & since cursor & unknown cursor
    send_cmd(&cmd_tx, json!({ "id": "ge_all", "type": "get_entries" }));
    let ge_all =
        recv_response(&mut out_rx, "ge_all", std::time::Duration::from_millis(400)).await;
    assert_eq!(ge_all["data"]["entries"].as_array().unwrap().len(), 4);
    assert_eq!(ge_all["data"]["leafId"], "e3");

    send_cmd(
        &cmd_tx,
        json!({ "id": "ge_since", "type": "get_entries", "since": "e1" }),
    );
    let ge_since =
        recv_response(&mut out_rx, "ge_since", std::time::Duration::from_millis(400)).await;
    let slice = ge_since["data"]["entries"].as_array().unwrap();
    assert_eq!(slice.len(), 2);
    assert_eq!(slice[0]["id"], "e2");
    assert_eq!(slice[1]["id"], "e3");

    send_cmd(
        &cmd_tx,
        json!({ "id": "ge_bad", "type": "get_entries", "since": "missing" }),
    );
    let ge_bad =
        recv_response(&mut out_rx, "ge_bad", std::time::Duration::from_millis(400)).await;
    assert_eq!(ge_bad["success"], false);

    // 2. fork pada root "e0" -> menghasilkan sesi kosong dg parentSession menunjuk orig.jsonl
    send_cmd(&cmd_tx, json!({ "id": "fk0", "type": "fork", "entryId": "e0" }));
    let fk0 = recv_response(&mut out_rx, "fk0", std::time::Duration::from_millis(400)).await;
    assert_eq!(fk0["success"], true);
    assert_eq!(fk0["data"]["text"], "prompt satu");

    send_cmd(&cmd_tx, json!({ "id": "gm0", "type": "get_messages" }));
    let gm0 = recv_response(&mut out_rx, "gm0", std::time::Duration::from_millis(400)).await;
    assert_eq!(gm0["data"]["messages"].as_array().unwrap().len(), 0);

    send_cmd(&cmd_tx, json!({ "id": "gs0", "type": "get_state" }));
    let gs0 = recv_response(&mut out_rx, "gs0", std::time::Duration::from_millis(400)).await;
    let forked0 = gs0["data"]["sessionFile"].as_str().unwrap().to_string();
    assert!(std::fs::read_to_string(&forked0)
        .unwrap()
        .contains("\"parentSession\""));

    // 3. new_session (dengan parentSession & tanpa parentSession)
    send_cmd(
        &cmd_tx,
        json!({ "id": "ns", "type": "new_session", "parentSession": forked0 }),
    );
    let ns = recv_response(&mut out_rx, "ns", std::time::Duration::from_millis(400)).await;
    assert_eq!(ns["success"], true);
    assert_eq!(ns["data"]["cancelled"], false);

    send_cmd(&cmd_tx, json!({ "id": "gs_ns", "type": "get_state" }));
    let gs_ns =
        recv_response(&mut out_rx, "gs_ns", std::time::Duration::from_millis(400)).await;
    let ns_file = gs_ns["data"]["sessionFile"].as_str().unwrap().to_string();
    assert_ne!(ns_file, forked0);
    assert!(std::fs::read_to_string(&ns_file)
        .unwrap()
        .contains("\"parentSession\""));

    // 4. switch_session kembali ke orig.jsonl memulihkan seluruh 4 pesan + export_html path relatif nested
    send_cmd(
        &cmd_tx,
        json!({ "id": "sw", "type": "switch_session", "sessionPath": session_file }),
    );
    let sw = recv_response(&mut out_rx, "sw", std::time::Duration::from_millis(400)).await;
    assert_eq!(sw["success"], true);

    send_cmd(&cmd_tx, json!({ "id": "gm_sw", "type": "get_messages" }));
    let gm_sw =
        recv_response(&mut out_rx, "gm_sw", std::time::Duration::from_millis(400)).await;
    assert_eq!(gm_sw["data"]["messages"].as_array().unwrap().len(), 4);

    send_cmd(
        &cmd_tx,
        json!({ "id": "ex_rel", "type": "export_html", "outputPath": "reports/nested.html" }),
    );
    let ex_rel =
        recv_response(&mut out_rx, "ex_rel", std::time::Duration::from_millis(400)).await;
    assert_eq!(ex_rel["success"], true);
    let expected_rel = tmp.path().join("reports/nested.html");
    assert_eq!(
        PathBuf::from(ex_rel["data"]["path"].as_str().unwrap()),
        expected_rel
    );
    assert!(std::fs::read_to_string(&expected_rel)
        .unwrap()
        .contains("prompt dua"));

    if let Ok(home) = std::env::var("HOME") {
        assert_eq!(
            expand_home_or_resolve("~/reports/out.html", tmp.path()),
            PathBuf::from(home).join("reports/out.html")
        );
    }

    drop(cmd_tx);
    let _ = tokio::time::timeout(std::time::Duration::from_secs(2), loop_task).await;
}

#[tokio::test]
async fn reg003_legacy_missing_tool_ids_orphan_abort_and_consecutive_users() {
    let _sink_guard = output_sink_test_mutex().lock().await;
    let (_reset, mut out_rx) = install_channel_output_sink();
    let tmp = TempConfigGuard::new("reg003_legacy_ids");

    let session_file = tmp.path().join("legacy_ids.jsonl");
    let lines = [
        json!({ "type": "session", "version": 3, "id": "s-legacy", "timestamp": "2026-11-12T00:00:00Z", "cwd": tmp.path() }).to_string(),
        // Dua user prompt berturut-turut
        json!({ "type": "message", "id": "e0", "parentId": null, "timestamp": "2026-11-12T00:00:01Z",
            "message": { "role": "user", "content": [{ "type": "text", "text": "pertanyaan awal" }], "timestamp": 1 } }).to_string(),
        json!({ "type": "message", "id": "e1", "parentId": "e0", "timestamp": "2026-11-12T00:00:02Z",
            "message": { "role": "user", "content": [
                { "type": "text", "text": "pertanyaan susulan" },
                { "type": "image", "mimeType": "image/png", "data": "iVBORw0KGgo=" }
            ], "timestamp": 2 } }).to_string(),
        // Assistant memiliki text diikuti toolCall TANPA id + toolCall DENGAN id yang terputus
        json!({ "type": "message", "id": "e2", "parentId": "e1", "timestamp": "2026-11-12T00:00:03Z",
            "message": { "role": "assistant", "stopReason": "toolUse",
                "content": [
                    { "type": "text", "text": "Cek file dulu." },
                    { "type": "toolCall", "name": "read", "arguments": { "path": "Cargo.toml" } },
                    { "type": "toolCall", "id": "c_interrupted", "name": "bash", "arguments": { "command": "sleep 9" } }
                ], "timestamp": 3 } }).to_string(),
        // toolResult TANPA toolCallId untuk toolCall read yang juga tanpa id
        json!({ "type": "message", "id": "e3", "parentId": "e2", "timestamp": "2026-11-12T00:00:04Z",
            "message": { "role": "toolResult", "toolCallId": "", "toolName": "read", "isError": false,
                "content": [{ "type": "text", "text": "isi cargo legacy" }], "timestamp": 4 } }).to_string(),
    ];
    std::fs::write(&session_file, format!("{}\n", lines.join("\n"))).unwrap();

    let (base_url, reqs, mock_handle) = spawn_mock_llm_server(move |_idx, req| async move {
        if req.path.contains("/chat/completions") {
            (200, json!({
                "choices": [{ "message": { "role": "assistant", "content": "ok oai" } }],
                "usage": { "prompt_tokens": 30, "completion_tokens": 5 }
            }))
        } else if req.path.ends_with("/messages") {
            (200, json!({
                "content": [{ "type": "text", "text": "ok ant" }],
                "usage": { "input_tokens": 30, "output_tokens": 5 }
            }))
        } else {
            (200, gemini_text_resp("ok gem", 30, 5))
        }
    })
    .await;

    for (provider, api, model_id, suffix) in [
        ("google", "google-generative-ai", "gemini-2.5-flash", "/v1beta"),
        ("openrouter", "openai-completions", "qwen/qwen3-coder", "/v1"),
        ("anthropic", "anthropic-messages", "claude-sonnet-4", "/v1"),
    ] {
        let prov_dir = tmp.path().join(provider);
        std::fs::create_dir_all(&prov_dir).unwrap();
        write_mock_provider_config(
            &prov_dir,
            provider,
            api,
            model_id,
            &format!("{}{}", base_url, suffix),
            "k-legacy",
        );

        let mut server = RpcServer::new_in_dir_with_cwd(
            prov_dir,
            tmp.path().to_path_buf(),
            Some(session_file.to_string_lossy().to_string()),
            None,
            vec![],
        )
        .unwrap();

        let (cmd_tx, reader) = TestLineReader::new();
        let loop_task = tokio::spawn(async move {
            server
                .run_loop_with_input(io::BufReader::new(reader))
                .await
                .unwrap();
        });
        let _ = recv_matching(&mut out_rx, std::time::Duration::from_secs(2), |v| {
            v["topic"] == "pipkin:hello"
        })
        .await;

        send_cmd(&cmd_tx, json!({ "id": "p", "type": "prompt", "message": "lanjut" }));
        let _ = recv_response(&mut out_rx, "p", std::time::Duration::from_millis(500)).await;
        let _ = recv_matching(&mut out_rx, std::time::Duration::from_secs(2), |v| {
            v["type"] == "agent_settled"
        })
        .await;

        drop(cmd_tx);
        let _ = tokio::time::timeout(std::time::Duration::from_secs(2), loop_task).await;
    }

    let captured = reqs.lock().unwrap().clone();
    assert_eq!(captured.len(), 3);

    // 1. Gemini: 2 user turn awal ter-coalesce + reloaded_call_* berpasangan + interrupted call terisi error
    let gem_contents = captured[0].body["contents"].as_array().unwrap();
    assert_eq!(gem_contents[0]["role"], "user");
    assert!(gem_contents[0]["parts"]
        .as_array()
        .unwrap()
        .iter()
        .any(|p| p["text"] == "pertanyaan awal"));
    assert!(gem_contents[0]["parts"]
        .as_array()
        .unwrap()
        .iter()
        .any(|p| p["text"] == "pertanyaan susulan"));
    let gem_str = captured[0].body.to_string();
    assert!(gem_str.contains("reloaded_call_"));
    assert!(gem_str.contains("isi cargo legacy"));
    assert!(gem_str.contains("Tool call interrupted before completion"));

    // 2. OpenAI: 2 user berturut-turut tergabung (termasuk multimodal), ID tool_calls & tool messages identik
    let oai_msgs = captured[1].body["messages"].as_array().unwrap();
    for pair in oai_msgs.windows(2) {
        assert!(
            !(pair[0]["role"] == "user" && pair[1]["role"] == "user"),
            "OpenAI consecutive user messages must be coalesced"
        );
    }
    let asst_msg = oai_msgs
        .iter()
        .find(|m| m["role"] == "assistant" && m["tool_calls"].is_array())
        .unwrap();
    let tc_ids: Vec<&str> = asst_msg["tool_calls"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|tc| tc["id"].as_str())
        .collect();
    assert_eq!(tc_ids.len(), 2);
    assert!(tc_ids[0].starts_with("reloaded_call_"));
    assert_eq!(tc_ids[1], "c_interrupted");
    assert!(oai_msgs.iter().any(|m| {
        m["role"] == "tool"
            && m["tool_call_id"] == tc_ids[0]
            && m["content"].as_str().unwrap_or("").contains("isi cargo legacy")
    }));
    assert!(oai_msgs.iter().any(|m| {
        m["role"] == "tool"
            && m["tool_call_id"] == "c_interrupted"
            && m["content"]
                .as_str()
                .unwrap_or("")
                .contains("Tool call interrupted before completion")
    }));

    // 3. Anthropic: role strict alternation & matching tool_use.id == tool_result.tool_use_id
    let ant_msgs = captured[2].body["messages"].as_array().unwrap();
    for pair in ant_msgs.windows(2) {
        assert_ne!(
            pair[0]["role"], pair[1]["role"],
            "Anthropic consecutive same-role messages must be merged"
        );
    }
    let ant_str = captured[2].body.to_string();
    assert!(ant_str.contains(tc_ids[0]));
    assert!(ant_str.contains("isi cargo legacy"));
    assert!(ant_str.contains("Tool call interrupted before completion"));

    mock_handle.abort();
}

#[tokio::test]
async fn reg002_stdin_eof_during_active_turn_exits_cleanly() {
    let _sink_guard = output_sink_test_mutex().lock().await;
    let (_reset, mut out_rx) = install_channel_output_sink();
    let tmp = TempConfigGuard::new("reg002_eof");

    let (hit_tx, hit_rx) = tokio::sync::oneshot::channel::<()>();
    let hit_tx = Arc::new(Mutex::new(Some(hit_tx)));
    let (base_url, _reqs, mock_handle) = spawn_mock_llm_server(move |_idx, _req| {
        let hit_tx = Arc::clone(&hit_tx);
        async move {
            if let Some(tx) = hit_tx.lock().unwrap().take() {
                let _ = tx.send(());
            }
            tokio::time::sleep(std::time::Duration::from_secs(15)).await;
            (200, gemini_text_resp("tidak akan selesai", 10, 5))
        }
    })
    .await;
    write_mock_provider_config(
        tmp.path(),
        "google",
        "google-generative-ai",
        "gemini-2.5-flash",
        &format!("{}/v1beta", base_url),
        "k-eof",
    );

    let mut server = RpcServer::new_in_dir_with_cwd(
        tmp.path().to_path_buf(),
        tmp.path().to_path_buf(),
        None,
        None,
        vec![],
    )
    .unwrap();

    let (cmd_tx, reader) = TestLineReader::new();
    let loop_task = tokio::spawn(async move {
        server
            .run_loop_with_input(io::BufReader::new(reader))
            .await
            .unwrap();
    });
    let _ = recv_matching(&mut out_rx, std::time::Duration::from_secs(2), |v| {
        v["topic"] == "pipkin:hello"
    })
    .await;

    send_cmd(
        &cmd_tx,
        json!({ "id": "p_eof", "type": "prompt", "message": "hang forever" }),
    );
    let _ = recv_response(&mut out_rx, "p_eof", std::time::Duration::from_millis(500)).await;
    tokio::time::timeout(std::time::Duration::from_secs(2), hit_rx)
        .await
        .unwrap()
        .unwrap();

    // Tutup stdin secara mendadak saat HTTP in-flight -> run_loop harus segera keluar
    let closed_at = std::time::Instant::now();
    drop(cmd_tx);
    tokio::time::timeout(std::time::Duration::from_secs(1), loop_task)
        .await
        .expect("RPC loop hung on stdin EOF while turn was active")
        .unwrap();
    assert!(closed_at.elapsed() < std::time::Duration::from_secs(1));

    mock_handle.abort();
}

#[tokio::test]
async fn reg011_live_todo_widget_emitted_on_tool_call_end_and_background_active_count() {
    let tmp = TempConfigGuard::new("reg011_live_widget");
    let mut reg = PluginRegistry::with_defaults();
    assert!(reg.activate("pipkin-todo"));
    assert!(reg.activate("pipkin-background"));
    let mut tools = ToolRegistry::new();
    reg.register_active_tools(&mut tools);
    let plugins = Arc::new(reg);

    let emitted = Arc::new(Mutex::new(Vec::<Value>::new()));
    let emitted_clone = Arc::clone(&emitted);
    let ui_state = Arc::new(Mutex::new(RpcUiState::from_loaded_session(
        init_or_load_session(tmp.path(), tmp.path(), None),
    )));
    let sink = RpcEventSink::with_emitter(ui_state, move |val| {
        emitted_clone.lock().unwrap().push(val.clone());
    })
    .with_plugins(Arc::clone(&plugins));

    // 1. Jalankan todo tool -> kirim ToolCallEnd -> setWidget harus terpancar langsung (sebelum TurnEnd)
    let todo_tool = tools.get("todo").unwrap();
    let todo_out = todo_tool
        .execute(&json!({ "action": "create", "subject": "Widget Live" }))
        .await
        .unwrap();
    sink.on_event(&AgentEvent::ToolCallEnd {
        step_index: 0,
        call_id: "call_todo_1".into(),
        tool_name: "todo".into(),
        status: ToolStatus::Done,
        output: todo_out,
        duration_ms: 2,
    });

    let events_after_todo = emitted.lock().unwrap().clone();
    let widget_evt = events_after_todo
        .iter()
        .find(|v| v["type"] == "extension_ui_request" && v["method"] == "setWidget")
        .expect("setWidget must be emitted immediately on todo ToolCallEnd");
    assert_eq!(widget_evt["widgetKey"], "todos");
    assert!(widget_evt["widgetLines"]
        .as_array()
        .unwrap()
        .iter()
        .any(|l| l.as_str().unwrap_or("").contains("Widget Live")));

    // Clear todo -> setWidget tanpa widgetLines harus terpancar untuk menghapus widget UI
    let clear_out = todo_tool
        .execute(&json!({ "action": "clear" }))
        .await
        .unwrap();
    emitted.lock().unwrap().clear();
    sink.on_event(&AgentEvent::ToolCallEnd {
        step_index: 0,
        call_id: "call_todo_2".into(),
        tool_name: "todo".into(),
        status: ToolStatus::Done,
        output: clear_out,
        duration_ms: 1,
    });
    let events_after_clear = emitted.lock().unwrap().clone();
    let clear_evt = events_after_clear
        .iter()
        .find(|v| v["type"] == "extension_ui_request" && v["method"] == "setWidget")
        .expect("setWidget clear must be emitted when todo becomes empty");
    assert!(clear_evt.get("widgetLines").is_none());

    // 2. Jalankan background tool -> ToolCallEnd harus memancarkan pipkin_channel active: 1, lalu kill -> active: 0
    let bg_tool = tools.get("background").unwrap();
    let bg_start = bg_tool
        .execute(&json!({ "action": "start", "command": "sleep 30" }))
        .await
        .unwrap();
    let bg_id = bg_start["id"].as_u64().unwrap();
    emitted.lock().unwrap().clear();
    sink.on_event(&AgentEvent::ToolCallEnd {
        step_index: 0,
        call_id: "call_bg_1".into(),
        tool_name: "background".into(),
        status: ToolStatus::Done,
        output: bg_start,
        duration_ms: 3,
    });
    let bg_events = emitted.lock().unwrap().clone();
    let ch_active = bg_events
        .iter()
        .find(|v| v["type"] == "pipkin_channel" && v["topic"] == "background")
        .expect("background channel must be emitted on ToolCallEnd");
    assert_eq!(ch_active["data"]["active"], 1);

    let bg_kill = bg_tool
        .execute(&json!({ "action": "kill", "id": bg_id }))
        .await
        .unwrap();
    emitted.lock().unwrap().clear();
    sink.on_event(&AgentEvent::ToolCallEnd {
        step_index: 0,
        call_id: "call_bg_2".into(),
        tool_name: "background".into(),
        status: ToolStatus::Done,
        output: bg_kill,
        duration_ms: 1,
    });
    let bg_events_after = emitted.lock().unwrap().clone();
    let ch_killed = bg_events_after
        .iter()
        .find(|v| v["type"] == "pipkin_channel" && v["topic"] == "background")
        .unwrap();
    assert_eq!(ch_killed["data"]["active"], 0);
}

#[test]
fn reg005_available_models_empty_scope_and_no_local_duplicate() {
    let tmp = TempConfigGuard::new("reg005_models_scope");
    std::fs::write(
        tmp.path().join("settings.json"),
        json!({ "enabledModels": [] }).to_string(),
    )
    .unwrap();
    std::fs::write(
        tmp.path().join("models.json"),
        json!({
            "providers": {
                "pipkin-local": {
                    "baseUrl": "http://127.0.0.1:11434/v1",
                    "api": "openai-completions",
                    "models": [{ "id": "llama3.2", "name": "Llama 3.2" }]
                }
            }
        })
        .to_string(),
    )
    .unwrap();

    let server = RpcServer::new_in_dir_with_cwd(
        tmp.path().to_path_buf(),
        tmp.path().to_path_buf(),
        None,
        None,
        vec![],
    )
    .unwrap();
    let models = server.available_models_json();
    let local_matches: Vec<&Value> = models
        .iter()
        .filter(|m| m["id"] == "llama3.2" && m["provider"] == "pipkin-local")
        .collect();
    assert_eq!(
        local_matches.len(),
        1,
        "empty enabledModels must leave scope unrestricted and local model must appear once"
    );
}

#[tokio::test]
async fn todo_plugin_persists_custom_entry_replays_on_resume_and_isolates_sessions() {
    use crate::plugins::{TodoPlugin, TODO_STATE_ENTRY};

    let tmp = TempConfigGuard::new("todo_session_persist");
    let server = RpcServer::new_in_dir_with_cwd(
        tmp.path().to_path_buf(),
        tmp.path().to_path_buf(),
        None,
        None,
        vec!["pipkin-todo".to_string()],
    )
    .unwrap();

    server.runtime.ensure_session_header();
    let session_a_path = server.runtime.mutable.lock().unwrap().session_file.clone();

    let mut tools = ToolRegistry::new();
    server.runtime.plugins.register_active_tools(&mut tools);
    let todo_tool = tools.get("todo").expect("todo tool must be registered");

    let created = todo_tool
        .execute(&json!({
            "action": "create",
            "subject": "Simpan state ke branch",
            "metadata": { "scope": "rpc" }
        }))
        .await
        .unwrap();

    let emitted = Arc::new(Mutex::new(Vec::<Value>::new()));
    let emitted_ref = Arc::clone(&emitted);
    let sink = RpcEventSink::with_emitter(
        Arc::clone(&server.runtime.ui_state),
        move |val| {
            emitted_ref.lock().unwrap().push(val.clone());
        },
    )
    .with_plugins(Arc::clone(&server.runtime.plugins));

    sink.on_event(&AgentEvent::ToolCallEnd {
        step_index: 0,
        call_id: "call_todo_persist_1".into(),
        tool_name: "todo".into(),
        status: ToolStatus::Done,
        output: created,
        duration_ms: 2,
    });

    // Verify tool_execution_end includes details and session JSONL has pipkin-todo-state custom entry
    {
        let evs = emitted.lock().unwrap();
        let exec_end = evs
            .iter()
            .find(|v| v["type"] == "tool_execution_end")
            .unwrap();
        assert_eq!(exec_end["result"]["details"]["tasks"][0]["id"], 1);
    }
    let session_a_raw = std::fs::read_to_string(&session_a_path).unwrap();
    assert!(
        session_a_raw.contains(TODO_STATE_ENTRY),
        "session JSONL must persist pipkin-todo-state custom entry"
    );

    // Resuming session A into a fresh RpcServer instance restores TodoPlugin state
    let resumed = RpcServer::new_in_dir_with_cwd(
        tmp.path().to_path_buf(),
        tmp.path().to_path_buf(),
        Some(session_a_path.to_string_lossy().to_string()),
        None,
        vec!["pipkin-todo".to_string()],
    )
    .unwrap();
    let resumed_plugin = resumed
        .runtime
        .plugins
        .get_plugin("pipkin-todo")
        .unwrap();
    let lines = resumed_plugin
        .render_widget("todos")
        .expect("resumed session must restore todos widget lines");
    assert!(lines[1].contains("Simpan state ke branch"));

    // Starting a new session clears TodoPlugin state so tasks don't leak across sessions
    resumed.runtime.handle_new_session_command("cmd_new", None);
    assert!(
        resumed_plugin.render_widget("todos").is_none(),
        "new_session must reset TodoPlugin state"
    );

    // Switching back to session A restores TodoPlugin state again
    resumed
        .runtime
        .handle_switch_session_command("cmd_sw", &session_a_path.to_string_lossy());
    let restored_lines = resumed_plugin
        .render_widget("todos")
        .expect("switch_session back to session A must restore todos");
    assert!(restored_lines[1].contains("Simpan state ke branch"));

    // Check downcast helper reflects next_id = 2
    let typed = resumed_plugin
        .as_any()
        .and_then(|a| a.downcast_ref::<TodoPlugin>())
        .unwrap();
    assert_eq!(typed.snapshot_state().next_id, 2);
}

#[tokio::test]
async fn todo_slash_command_lists_and_clears_session_todos() {
    use crate::plugins::NO_TODOS_MESSAGE;

    let tmp = TempConfigGuard::new("todo_slash_cmd");
    let server = RpcServer::new_in_dir_with_cwd(
        tmp.path().to_path_buf(),
        tmp.path().to_path_buf(),
        None,
        None,
        vec!["pipkin-todo".to_string()],
    )
    .unwrap();

    let empty_reply = server.runtime.handle_slash_command("/todos").await.unwrap();
    assert_eq!(empty_reply, NO_TODOS_MESSAGE);

    let mut tools = ToolRegistry::new();
    server.runtime.plugins.register_active_tools(&mut tools);
    let todo_tool = tools.get("todo").unwrap();
    todo_tool
        .execute(&json!({ "action": "create", "subject": "Langkah satu" }))
        .await
        .unwrap();
    todo_tool
        .execute(&json!({
            "action": "update",
            "id": 1,
            "status": "in_progress",
            "activeForm": "mengerjakan"
        }))
        .await
        .unwrap();

    let list_reply = server.runtime.handle_slash_command("/todos").await.unwrap();
    assert!(list_reply.contains("1 in progress"));
    assert!(list_reply.contains("◐ #1 Langkah satu (mengerjakan)"));

    let clear_reply = server
        .runtime
        .handle_slash_command("/todos clear")
        .await
        .unwrap();
    assert!(clear_reply.contains("dikosongkan"));
    assert!(server
        .runtime
        .plugins
        .get_plugin("pipkin-todo")
        .unwrap()
        .render_widget("todos")
        .is_none());
}

