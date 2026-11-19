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
async fn test_rpc_loop_tool_approval_oopbot_channel_roundtrip() {
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
{\"type\":\"oopbot_channel\",\"topic\":\"tool_approval\",\"data\":{\"kind\":\"response\",\"request_id\":\"appr-42\",\"approved\":true}}\n\
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


#[tokio::test]
async fn reg011_live_todo_widget_emitted_on_tool_call_end_and_background_active_count() {
    let tmp = TempConfigGuard::new("reg011_live_widget");
    let mut reg = PluginRegistry::with_defaults();
    assert!(reg.activate("oopbot-todo"));
    assert!(reg.activate("oopbot-background"));
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

    // 2. Jalankan background tool -> ToolCallEnd harus memancarkan oopbot_channel active: 1, lalu kill -> active: 0
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
        .find(|v| v["type"] == "oopbot_channel" && v["topic"] == "background")
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
        .find(|v| v["type"] == "oopbot_channel" && v["topic"] == "background")
        .unwrap();
    assert_eq!(ch_killed["data"]["active"], 0);
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
        vec!["oopbot-todo".to_string()],
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

    // Verify tool_execution_end includes details and session JSONL has oopbot-todo-state custom entry
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
        "session JSONL must persist oopbot-todo-state custom entry"
    );

    // Resuming session A into a fresh RpcServer instance restores TodoPlugin state
    let resumed = RpcServer::new_in_dir_with_cwd(
        tmp.path().to_path_buf(),
        tmp.path().to_path_buf(),
        Some(session_a_path.to_string_lossy().to_string()),
        None,
        vec!["oopbot-todo".to_string()],
    )
    .unwrap();
    let resumed_plugin = resumed
        .runtime
        .plugins
        .get_plugin("oopbot-todo")
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
        vec!["oopbot-todo".to_string()],
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
        .get_plugin("oopbot-todo")
        .unwrap()
        .render_widget("todos")
        .is_none());
}


#[tokio::test]
async fn interact_notify_and_ask_user_roundtrip_and_abort_over_rpc() {
    let _sink_guard = output_sink_test_mutex().lock().await;
    let (_reset, mut out_rx) = install_channel_output_sink();
    let tmp = TempConfigGuard::new("rpc_interact_roundtrip");

    let (base_url, reqs, mock_handle) = spawn_mock_llm_server(move |idx, req| async move {
        match idx {
            0 => (
                200,
                json!({
                    "candidates": [{
                        "content": {
                            "role": "model",
                            "parts": [{
                                "functionCall": {
                                    "id": "call_notify_1",
                                    "name": "notify_user",
                                    "args": {
                                        "message": "Tahap 1: analisa proyek",
                                        "level": "info"
                                    }
                                }
                            }]
                        }
                    }]
                }),
            ),
            1 => (
                200,
                json!({
                    "candidates": [{
                        "content": {
                            "role": "model",
                            "parts": [{
                                "functionCall": {
                                    "id": "call_ask_1",
                                    "name": "ask_user",
                                    "args": {
                                        "title": "Pilih target deploy?",
                                        "kind": "select",
                                        "options": ["staging", "prod"]
                                    }
                                }
                            }]
                        }
                    }]
                }),
            ),
            2 => {
                let s = req.body.to_string();
                let reply = if s.contains("prod") && s.contains("answered") {
                    "Siap deploy ke prod."
                } else {
                    "Jawaban tidak terbaca"
                };
                (200, gemini_text_resp(reply, 40, 12))
            }
            // Prompt ke-2 memicu ask_user confirm untuk menguji abort membatalkan pending_ui_requests
            _ => (
                200,
                json!({
                    "candidates": [{
                        "content": {
                            "role": "model",
                            "parts": [{
                                "functionCall": {
                                    "id": "call_ask_abort",
                                    "name": "ask_user",
                                    "args": {
                                        "title": "Konfirmasi hapus?",
                                        "kind": "confirm",
                                        "message": "Aksi ini permanen"
                                    }
                                }
                            }]
                        }
                    }]
                }),
            ),
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
    assert!(server.plugins.is_active("oopbot-interact"));
    let tool_names = server.agent.tool_names();
    assert!(tool_names.contains(&"notify_user"));
    assert!(tool_names.contains(&"ask_user"));
    let pending_ui = Arc::clone(&server.pending_ui_requests);

    let (cmd_tx, reader) = TestLineReader::new();
    let loop_task = tokio::spawn(async move {
        server.run_loop_with_input(io::BufReader::new(reader)).await.unwrap();
    });

    let _ = recv_matching(&mut out_rx, std::time::Duration::from_secs(2), |v| {
        v["topic"] == "oopbot:hello"
    })
    .await;

    send_cmd(
        &cmd_tx,
        json!({ "id": "p1", "type": "prompt", "message": "bantu deploy" }),
    );
    let p1 = recv_response(&mut out_rx, "p1", std::time::Duration::from_millis(500)).await;
    assert_eq!(p1["data"]["disposition"], "started");

    // 1. Terima notice notify_user
    let notify_ev = recv_matching(&mut out_rx, std::time::Duration::from_secs(2), |v| {
        v["type"] == "extension_ui_request" && v["method"] == "notify"
    })
    .await;
    assert_eq!(notify_ev["notifyType"], "info");
    assert_eq!(notify_ev["message"], "Tahap 1: analisa proyek");

    // 2. Terima dialog select ask_user & kirim jawaban user via extension_ui_response
    let select_ev = recv_matching(&mut out_rx, std::time::Duration::from_secs(2), |v| {
        v["type"] == "extension_ui_request" && v["method"] == "select"
    })
    .await;
    assert_eq!(select_ev["title"], "Pilih target deploy?");
    assert_eq!(select_ev["options"], json!(["staging", "prod"]));
    let req_id = select_ev["id"].as_str().unwrap().to_string();

    send_cmd(
        &cmd_tx,
        json!({ "type": "extension_ui_response", "id": req_id, "value": "prod" }),
    );

    let reply = recv_matching(&mut out_rx, std::time::Duration::from_secs(2), |v| {
        v["type"] == "message_end"
            && v["message"]["role"] == "assistant"
            && v["message"]["stopReason"] == "stop"
    })
    .await;
    assert_eq!(reply["message"]["content"][0]["text"], "Siap deploy ke prod.");
    let _ = recv_matching(&mut out_rx, std::time::Duration::from_secs(2), |v| {
        v["type"] == "agent_settled" && v["aborted"] == false
    })
    .await;
    assert!(pending_ui.lock().unwrap().is_empty());
    assert!(reqs.lock().unwrap().len() >= 3);

    // 3. Abort saat ask_user confirm sedang menunggu -> guard harus membersihkan pending_ui_requests
    send_cmd(
        &cmd_tx,
        json!({ "id": "p2", "type": "prompt", "message": "hapus data" }),
    );
    let _ = recv_response(&mut out_rx, "p2", std::time::Duration::from_millis(500)).await;
    let confirm_ev = recv_matching(&mut out_rx, std::time::Duration::from_secs(2), |v| {
        v["type"] == "extension_ui_request" && v["method"] == "confirm"
    })
    .await;
    assert_eq!(confirm_ev["title"], "Konfirmasi hapus?");
    assert!(!pending_ui.lock().unwrap().is_empty());

    send_cmd(&cmd_tx, json!({ "id": "ab_ui", "type": "abort" }));
    let settled_ab = recv_matching(&mut out_rx, std::time::Duration::from_secs(2), |v| {
        v["type"] == "agent_settled" && v["aborted"] == true
    })
    .await;
    assert_eq!(settled_ab["aborted"], true);
    let _ = recv_response(&mut out_rx, "ab_ui", std::time::Duration::from_secs(2)).await;
    assert!(pending_ui.lock().unwrap().is_empty());

    drop(cmd_tx);
    let _ = tokio::time::timeout(std::time::Duration::from_secs(2), loop_task).await;
    mock_handle.abort();
}

