use super::runner::strip_internal_tool_fields_for_llm;
use super::*;
use serde_json::json;

#[test]
fn chat_input_text_only_constructs_correctly() {
    let input = ChatInput::text_only("halo dunia");
    assert_eq!(input.text, "halo dunia");
    assert!(input.images.is_empty());
}

#[test]
fn chat_input_with_images_constructs_correctly() {
    let images = vec![("image/png".to_string(), "base64data".to_string())];
    let input = ChatInput::with_images("analisis gambar ini", images.clone());
    assert_eq!(input.text, "analisis gambar ini");
    assert_eq!(input.images.len(), 1);
    assert_eq!(input.images[0].0, "image/png");
    assert_eq!(input.images[0].1, "base64data");
}

#[test]
fn turn_outcome_aggregates_usage_and_steps() {
    let outcome = TurnOutcome {
        text: "Jawaban asisten".to_string(),
        events: vec![],
        steps: 2,
        outcome: StepsOutcome::Done,
        usage: Some(TokenUsage {
            input_tokens: 150,
            output_tokens: 80,
        }),
        last_usage: None,
        aborted: false,
    };

    assert_eq!(outcome.steps, 2);
    assert_eq!(outcome.outcome, StepsOutcome::Done);
    assert_eq!(outcome.usage.unwrap().input_tokens, 150);
    assert_eq!(outcome.usage.unwrap().output_tokens, 80);
    assert!(!outcome.aborted);
}

#[test]
fn reg_todo_strips_internal_details_from_llm_payload_only() {
    let raw = json!({
        "status": "success",
        "action": "create",
        "changed": true,
        "task": { "id": 1, "subject": "A", "status": "pending" },
        "details": { "action": "create", "tasks": [{ "id": 1 }], "nextId": 2 },
        "message": "ok"
    });
    let for_llm = strip_internal_tool_fields_for_llm("todo", raw.clone());
    assert!(for_llm.get("details").is_none());
    assert_eq!(for_llm["task"]["id"], 1);
    assert_eq!(for_llm["message"], "ok");

    let other = strip_internal_tool_fields_for_llm("read_file", raw.clone());
    assert!(other.get("details").is_some());
}

#[test]
fn reg_edit_file_strips_internal_details_from_llm_payload_only() {
    let raw = json!({
        "path": "src/main.rs",
        "edits_applied": 2,
        "lines_added": 5,
        "lines_removed": 3,
        "first_changed_line": 42,
        "bytes_written": 1840,
        "details": {
            "diff": "  41 fn main() {\n- 42     old();\n+ 42     new();\n  43 }",
            "firstChangedLine": 42
        }
    });
    let for_llm = strip_internal_tool_fields_for_llm("edit_file", raw.clone());
    assert!(for_llm.get("details").is_none());
    assert_eq!(for_llm["path"], "src/main.rs");
    assert_eq!(for_llm["edits_applied"], 2);
    assert_eq!(for_llm["lines_added"], 5);
    assert_eq!(for_llm["lines_removed"], 3);
    assert_eq!(for_llm["first_changed_line"], 42);
    assert_eq!(for_llm["bytes_written"], 1840);

    let sys = default_system_instruction(&crate::tools::ToolRegistry::with_defaults());
    assert!(sys.contains("edit_file"));
}

#[test]
fn workspace_tabs_save_is_atomic_0600_and_harness_surfaces_persist_errors() {
    let dir = std::env::temp_dir().join(format!(
        "oopbot_tabs_atomic_{}_{}",
        std::process::id(),
        crate::common::time::epoch_ms()
    ));
    let _ = std::fs::remove_dir_all(&dir);
    let tabs_file = dir.join("tabs.json");

    let mut ws = WorkspaceTabs::new("gemini-2.5-flash");
    ws.active_mut().history.push(json!({
        "role": "user",
        "parts": [{ "text": "rahasia token" }]
    }));
    ws.save_to_file(&tabs_file).unwrap();

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(&tabs_file).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
    }

    let loaded = WorkspaceTabs::load_from_file(&tabs_file).expect("reload tabs.json");
    assert_eq!(loaded.active().history.len(), 1);

    // Pointing tabs_path at an existing directory must surface error via take_persist_error()
    struct NeverApprove;
    #[async_trait]
    impl Approver for NeverApprove {
        async fn approve(&self, _tool: &str, _preview: &str) -> bool {
            false
        }
    }
    let client = crate::llm::LlmClient::new_with_base_url(
        "k".into(),
        "gemini-2.5-flash".into(),
        Some("http://127.0.0.1:9".into()),
    )
    .unwrap();
    let harness = AgentHarness::with_event_sink_and_tabs(
        "OOPBoT",
        client,
        crate::tools::ToolRegistry::new(),
        Box::new(NeverApprove),
        Box::new(NoopEventSink),
        5,
        WorkspaceTabs::new("gemini-2.5-flash"),
        Some(dir.clone()), // directory path triggers write failure
    );
    harness.clear_memory_silent();
    let err = harness
        .take_persist_error()
        .expect("persist failure must be surfaced instead of silently ignored");
    assert!(!err.is_empty());
    assert!(harness.take_persist_error().is_none());

    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn agent_compact_active_tab_preserves_timeline_and_auto_compacts_in_runner() {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let captured = std::sync::Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
    let captured_srv = std::sync::Arc::clone(&captured);

    let srv = tokio::spawn(async move {
        for _ in 0..3 {
            let Ok((mut sock, _)) = listener.accept().await else {
                break;
            };
            let mut buf = vec![0u8; 65_536];
            let n = sock.read(&mut buf).await.unwrap_or(0);
            let req = String::from_utf8_lossy(&buf[..n]).to_string();
            let is_summary = req.contains("<conversation>");
            captured_srv.lock().unwrap().push(req);

            let body = if is_summary {
                json!({
                    "choices": [{
                        "message": { "role": "assistant", "content": "Ringkasan: user sedang uji compaction." }
                    }],
                    "usage": { "prompt_tokens": 50, "completion_tokens": 12 }
                })
                .to_string()
            } else {
                json!({
                    "choices": [{
                        "message": { "role": "assistant", "content": "Lanjut dari ringkasan berhasil." }
                    }],
                    "usage": { "prompt_tokens": 40, "completion_tokens": 10 }
                })
                .to_string()
            };
            let resp = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                body.len(),
                body
            );
            let _ = sock.write_all(resp.as_bytes()).await;
        }
    });

    struct AllowAll;
    #[async_trait]
    impl Approver for AllowAll {
        async fn approve(&self, _tool: &str, _preview: &str) -> bool {
            true
        }
    }

    let client = crate::llm::LlmClient::new_with_base_url(
        "test-key".into(),
        "tiny-model".into(),
        Some(format!("http://{}/v1", addr)),
    )
    .unwrap();
    let mut tabs = WorkspaceTabs::new("tiny-model");
    tabs.active_mut().history = vec![
        json!({ "role": "user", "parts": [{ "text": "langkah 1 panjang ".repeat(60) }] }),
        json!({ "role": "model", "parts": [{ "text": "hasil 1 panjang ".repeat(60) }] }),
        json!({ "role": "user", "parts": [{ "text": "langkah 2" }] }),
        json!({ "role": "model", "parts": [{ "text": "hasil 2" }] }),
    ];
    tabs.active_mut().timeline.push(TimelineEntry::User {
        text: "langkah 1".into(),
    });

    let mut harness = AgentHarness::with_event_sink_and_tabs(
        "OOPBoT",
        client,
        crate::tools::ToolRegistry::new(),
        Box::new(AllowAll),
        Box::new(NoopEventSink),
        5,
        tabs,
        None,
    );
    // Small context window so auto-compaction triggers deterministically
    harness.configure_model(
        "tiny-model",
        &json!({ "api": "openai-completions", "contextWindow": 500, "maxTokens": 100 }),
    );
    harness.set_auto_compact(true);

    // 1. Manual compact_active_tab preserves prior timeline and appends Notice
    let (_tx, rx) = tokio::sync::watch::channel(false);
    let prep = harness.compact_active_tab(None, rx).await.unwrap();
    assert!(prep.compacted);
    let tl = harness.active_timeline();
    assert_eq!(tl.len(), 2);
    assert!(matches!(&tl[0], TimelineEntry::User { .. }));
    assert!(matches!(&tl[1], TimelineEntry::Notice { text } if text.contains("Konteks dipadatkan")));

    // 2. Seed oversized history & verify runner triggers auto-compaction automatically
    harness.replace_active_history(
        "1",
        Some("Tab 1"),
        vec![
            json!({ "role": "user", "parts": [{ "text": "payload besar ".repeat(120) }] }),
            json!({ "role": "model", "parts": [{ "text": "balasan besar ".repeat(120) }] }),
        ],
    );
    let reply = harness.chat("lanjutkan sekarang").await.unwrap();
    assert_eq!(reply, "Lanjut dari ringkasan berhasil.");

    let reqs = captured.lock().unwrap();
    assert!(
        reqs.len() >= 3,
        "expected manual compact + auto compact + main turn, got {} requests",
        reqs.len()
    );
    assert!(reqs.last().unwrap().contains("<compaction_summary>"));
    srv.abort();
}

#[tokio::test]
async fn agent_runner_gates_llm_requests_when_context_remains_over_limit() {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let captured: Arc<std::sync::Mutex<Vec<String>>> = Arc::new(std::sync::Mutex::new(Vec::new()));
    let captured_srv = Arc::clone(&captured);

    // Server mock mengembalikan ringkasan yang sengaja sangat panjang agar post-compaction tetap OverLimit.
    let srv = tokio::spawn(async move {
        for _ in 0..4 {
            let Ok((mut sock, _)) = listener.accept().await else {
                break;
            };
            let mut buf = vec![0u8; 65_536];
            let n = sock.read(&mut buf).await.unwrap_or(0);
            let req = String::from_utf8_lossy(&buf[..n]).to_string();
            captured_srv.lock().unwrap().push(req);

            let huge_summary = "ringkasan sangat panjang ".repeat(120);
            let body = json!({
                "choices": [{
                    "message": { "role": "assistant", "content": huge_summary }
                }],
                "usage": { "prompt_tokens": 50, "completion_tokens": 600 }
            })
            .to_string();
            let resp = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                body.len(),
                body
            );
            let _ = sock.write_all(resp.as_bytes()).await;
        }
    });

    struct AllowAll;
    #[async_trait]
    impl Approver for AllowAll {
        async fn approve(&self, _tool: &str, _preview: &str) -> bool {
            true
        }
    }

    let client = crate::llm::LlmClient::new_with_base_url(
        "test-key".into(),
        "tiny-model".into(),
        Some(format!("http://{}/v1", addr)),
    )
    .unwrap();
    let mut tabs = WorkspaceTabs::new("tiny-model");
    tabs.active_mut().history = vec![
        json!({ "role": "user", "parts": [{ "text": "lama 1" }] }),
        json!({ "role": "model", "parts": [{ "text": "jawab 1" }] }),
    ];

    let mut harness = AgentHarness::with_event_sink_and_tabs(
        "OOPBoT",
        client,
        crate::tools::ToolRegistry::new(),
        Box::new(AllowAll),
        Box::new(NoopEventSink),
        3,
        tabs,
        None,
    );
    harness.configure_model(
        "tiny-model",
        &json!({ "api": "openai-completions", "contextWindow": 500, "maxTokens": 100 }),
    );
    harness.set_auto_compact(true);

    // 1. Prompt user baru sendirian sudah OverLimit -> summarizer dilewati & LLM utama diblokir (0 HTTP calls).
    let err1 = harness
        .chat(&"prompt terlalu panjang ".repeat(140))
        .await
        .expect_err("oversized single turn must be rejected before LLM request");
    assert!(err1.to_string().contains("melampaui batas model"));
    assert_eq!(
        captured.lock().unwrap().len(),
        0,
        "must not waste summarization or main LLM call when retained tail is already OverLimit"
    );

    // 2. Histori lama besar + prompt baru kecil -> compaction jalan, tetapi summary hasil LLM terlalu besar
    // sehingga post-compaction tetap OverLimit -> request utama tetap diblokir (hanya 1 HTTP call untuk summary).
    harness.replace_active_history(
        "1",
        Some("Tab 1"),
        vec![
            json!({ "role": "user", "parts": [{ "text": "histori lama ".repeat(120) }] }),
            json!({ "role": "model", "parts": [{ "text": "balasan lama ".repeat(120) }] }),
        ],
    );
    let err2 = harness
        .chat("lanjut")
        .await
        .expect_err("post-compaction OverLimit must block main LLM turn");
    assert!(err2.to_string().contains("setelah compaction"));
    assert_eq!(
        captured.lock().unwrap().len(),
        1,
        "only the compaction summary request should have been sent, not the overflowing main turn"
    );

    srv.abort();
}
