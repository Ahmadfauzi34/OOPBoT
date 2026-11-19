use super::*;
#[allow(unused_imports)]
use crate::agent::{AgentEvent, Approver, EventSink, StepsOutcome, ToolStatus};
#[allow(unused_imports)]
use crate::common::fs::read_json_file;
#[allow(unused_imports)]
use crate::common::time::{epoch_ms, iso_now, parse_iso_to_epoch_ms};
#[allow(unused_imports)]
use crate::config::{ModelCatalog, ModelInfo};
#[allow(unused_imports)]
use crate::tools::ToolRegistry;
#[allow(unused_imports)]
use serde_json::{json, Value};
#[allow(unused_imports)]
use std::io::{self, Cursor};
#[allow(unused_imports)]
use std::path::Path;
#[allow(unused_imports)]
use std::sync::atomic::{AtomicU64, Ordering};
#[allow(unused_imports)]
use std::sync::{Arc, Mutex};

#[test]
fn maps_tools_for_oopbot_timeline() {
    let (name, args) = map_tool_for_oopbot_ui("run_shell", &json!({ "command": "ls" }));
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
fn formats_web_search_with_engine_note_and_source() {
    let val = json!({
        "engine": "fallback_public_apis",
        "count": 1,
        "note": "DuckDuckGo diblokir; menggunakan API publik.",
        "summary": "Ringkasan hasil pencarian.",
        "results": [
            {
                "title": "Tokio",
                "url": "https://tokio.rs/",
                "snippet": "Async runtime",
                "source": "github"
            }
        ]
    });
    let (text, is_err) = format_tool_output("web_search", &val);
    assert!(!is_err);
    assert!(text.contains("(fallback_public_apis)"));
    assert!(text.contains("Catatan: DuckDuckGo diblokir"));
    assert!(text.contains("Ringkasan: Ringkasan hasil pencarian."));
    assert!(text.contains("[Tokio](https://tokio.rs/) [github]"));
}


#[test]
fn rpc_server_activates_plugins_from_extensions_arg() {
    let tmp = TempConfigGuard::new("oopbot_plugins_test");
    let extensions = vec![
        "/data/user/0/io.jeph.oopbot/files/extensions/oopbot-todo/index.js".to_string(),
        "oopbot-web-access".to_string(),
    ];

    let server = RpcServer::new_in_dir(tmp.path().to_path_buf(), None, None, extensions).unwrap();
    assert!(server.plugins.is_active("oopbot-todo"));
    assert!(server.plugins.is_active("oopbot-web-access"));
    assert!(!server.plugins.is_active("oopbot-background"));

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


#[tokio::test]
async fn test_oopbot_channel_handshake_and_incoming_events() {
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
        "oopbot-background".to_string(),
        "oopbot-subagents".to_string(),
    ];
    let mut server = RpcServer::new_in_dir(tmp.path().to_path_buf(), None, None, extensions).unwrap();

    let input = b"\
{\"type\":\"oopbot_channel\",\"topic\":\"background\",\"data\":{\"action\":\"stop\"}}\n\
{\"type\":\"oopbot_channel\",\"topic\":\"subagents\",\"data\":{\"action\":\"stopAll\"}}\n\
{\"type\":\"oopbot_channel\",\"topic\":\"remote\",\"data\":{\"action\":\"check\"}}\n\
{\"type\":\"get_state\",\"id\":\"chk_1\"}\n";
    let cursor = Cursor::new(input);

    let res = server.run_loop_with_input(cursor).await;
    assert!(res.is_ok());

    set_output_sink(None);

    let bytes = raw.lock().unwrap().clone();
    let written = String::from_utf8_lossy(&bytes);

    // Verifikasi handshake awal oopbot & oopbot:hello
    assert!(written.contains("\"topic\":\"oopbot\""));
    assert!(written.contains("\"topic\":\"oopbot:hello\""));
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

    // Pastikan tidak ada error komplain oopbot_channel
    assert!(!written.contains("tidak dikenal"));
}

#[test]
fn test_rpc_event_sink_text_turn() {
    use crate::agent::StepsOutcome;

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
        thought_signature: None,
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
    use crate::agent::StepsOutcome;

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
        thought_signature: None,
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
        thought_signature: None,
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
        v["type"] == "oopbot_channel" && v["topic"] == "oopbot:hello"
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
        v["topic"] == "oopbot:hello"
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


#[test]
fn reg_err_poisoned_mutex_recovers_instead_of_panicking() {
    let tmp = TempConfigGuard::new("rpc_poison_recover");
    let server = RpcServer::new_in_dir_with_cwd(
        tmp.path().to_path_buf(),
        tmp.path().to_path_buf(),
        None,
        None,
        vec![],
    )
    .unwrap();

    let ui_state = Arc::clone(&server.runtime.ui_state);
    let handle = std::thread::spawn(move || {
        let _guard = ui_state.lock().unwrap();
        panic!("simulated panic holding ui_state lock");
    });
    assert!(handle.join().is_err());
    assert!(server.runtime.ui_state.is_poisoned());

    let state = server.runtime.state_json();
    assert!(state["model"].is_object());
}


#[test]
fn audit_publish_completed_seq_is_monotonic() {
    let tmp = TempConfigGuard::new("audit_seq_monotonic");
    let server = RpcServer::new_in_dir_with_cwd(
        tmp.path().to_path_buf(),
        tmp.path().to_path_buf(),
        None,
        None,
        vec![],
    )
    .unwrap();

    let rx = server.runtime.completed_seq_tx.subscribe();
    server.runtime.publish_completed_seq(2);
    assert_eq!(*rx.borrow(), 2);

    // Out-of-order late publication dari seq lebih kecil tidak boleh menurunkan nilai watch
    server.runtime.publish_completed_seq(1);
    assert_eq!(*rx.borrow(), 2);

    server.runtime.publish_completed_seq(3);
    assert_eq!(*rx.borrow(), 3);
}
