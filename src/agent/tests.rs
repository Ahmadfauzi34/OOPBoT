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

#[tokio::test]
async fn agent_resets_observed_tokens_on_model_or_provider_switch_and_bounds_summary_prompt() {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let captured_body: Arc<std::sync::Mutex<Option<String>>> = Arc::new(std::sync::Mutex::new(None));
    let captured_srv = Arc::clone(&captured_body);

    let srv = tokio::spawn(async move {
        if let Ok((mut sock, _)) = listener.accept().await {
            let mut buf = vec![0u8; 262_144];
            let mut full = Vec::new();
            loop {
                let n = sock.read(&mut buf).await.unwrap_or(0);
                if n == 0 {
                    break;
                }
                full.extend_from_slice(&buf[..n]);
                let text = String::from_utf8_lossy(&full);
                if let Some((hdrs, body)) = text.split_once("\r\n\r\n") {
                    let content_len = hdrs
                        .lines()
                        .find_map(|l| {
                            let (k, v) = l.split_once(':')?;
                            k.eq_ignore_ascii_case("content-length")
                                .then(|| v.trim().parse::<usize>().ok())
                                .flatten()
                        })
                        .unwrap_or(0);
                    if body.len() >= content_len {
                        *captured_srv.lock().unwrap() = Some(body.to_string());
                        break;
                    }
                }
            }
            let resp_body = json!({
                "choices": [{
                    "message": { "role": "assistant", "content": "Ringkasan pendek." }
                }],
                "usage": { "prompt_tokens": 120, "completion_tokens": 10 }
            })
            .to_string();
            let resp = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                resp_body.len(),
                resp_body
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
        "model-a".into(),
        Some(format!("http://{}/v1", addr)),
    )
    .unwrap();
    let tabs = WorkspaceTabs::new("model-a");

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
        "model-a",
        &json!({ "api": "openai-completions", "contextWindow": 8000, "maxTokens": 2000 }),
    );
    harness.configure_model(
        "model-b",
        &json!({ "api": "openai-completions", "contextWindow": 1000, "maxTokens": 200 }),
    );

    // 0. Legacy unkeyed last_context_tokens tanpa fingerprint diabaikan agar tidak menyesatkan.
    {
        let mut tabs = harness.tabs.lock().unwrap();
        tabs.active_mut().last_context_tokens = Some(9_999);
        tabs.active_mut().last_context_fingerprint = None;
    }
    assert_eq!(harness.context_budget().last_observed_input_tokens, None);

    // 1. Seed observasi dengan fingerprint cocok -> dipertahankan saat model & system instruction sama.
    let fp_a = harness.context_observation_fingerprint("model-a", &harness.tools.declarations());
    {
        let mut tabs = harness.tabs.lock().unwrap();
        tabs.active_mut()
            .set_observed_context_tokens(Some(9_999), Some(fp_a));
    }
    harness.set_active_model("model-a");
    assert_eq!(harness.context_budget().last_observed_input_tokens, Some(9_999));

    // 2. Ganti system instruction (mis. --append-system-prompt berubah antar sesi) -> observasi di-reset.
    harness.set_system_instruction("OOPBoT dengan prompt tambahan baru".into());
    assert_eq!(harness.context_budget().last_observed_input_tokens, None);

    // 3. Pindah model -> last_context_tokens wajib di-reset ke None.
    let fp_a2 = harness.context_observation_fingerprint("model-a", &harness.tools.declarations());
    {
        let mut tabs = harness.tabs.lock().unwrap();
        tabs.active_mut()
            .set_observed_context_tokens(Some(4_321), Some(fp_a2));
    }
    assert_eq!(harness.context_budget().last_observed_input_tokens, Some(4_321));
    harness.set_active_model("model-b");
    assert_eq!(harness.context_budget().last_observed_input_tokens, None);

    // 4. Pindah base_url/provider -> last_context_tokens juga di-reset.
    let fp_b = harness.context_observation_fingerprint("model-b", &harness.tools.declarations());
    {
        let mut tabs = harness.tabs.lock().unwrap();
        tabs.active_mut()
            .set_observed_context_tokens(Some(777), Some(fp_b));
    }
    assert_eq!(harness.context_budget().last_observed_input_tokens, Some(777));
    harness.update_credentials("test-key".into(), Some("http://127.0.0.1:9/v1".into()));
    assert_eq!(harness.context_budget().last_observed_input_tokens, None);

    // Kembalikan base_url ke mock server untuk uji pembatasan prompt compaction model kecil (model-b: 1000 window).
    harness.update_credentials("test-key".into(), Some(format!("http://{}/v1", addr)));
    let max_cap = harness.context_policy().max_compaction_prompt_chars();
    assert!(max_cap < 3_000, "expected small cap for 1000-window model, got {}", max_cap);

    let huge_prompt = "X".repeat(50_000);
    let (_tx, rx) = tokio::sync::watch::channel(false);
    let (summary, _) = harness
        .generate_compaction_summary_cancellable(&huge_prompt, rx)
        .await
        .unwrap();
    assert_eq!(summary, "Ringkasan pendek.");

    let sent_json: Value =
        serde_json::from_str(captured_body.lock().unwrap().as_deref().unwrap()).unwrap();
    let user_content = sent_json["messages"]
        .as_array()
        .unwrap()
        .iter()
        .find(|m| m["role"] == "user")
        .and_then(|m| m["content"].as_str())
        .unwrap();
    assert!(
        user_content.chars().count() <= max_cap,
        "compaction prompt sent to small-window model exceeded cap: {} > {}",
        user_content.chars().count(),
        max_cap
    );

    srv.abort();
}

#[tokio::test]
async fn agent_compaction_forwards_inline_images_only_to_multimodal_models() {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let captured_bodies: Arc<std::sync::Mutex<Vec<String>>> =
        Arc::new(std::sync::Mutex::new(Vec::new()));
    let captured_srv = Arc::clone(&captured_bodies);

    let srv = tokio::spawn(async move {
        for _ in 0..2 {
            let Ok((mut sock, _)) = listener.accept().await else {
                break;
            };
            let mut buf = vec![0u8; 65_536];
            let mut full = Vec::new();
            loop {
                let n = sock.read(&mut buf).await.unwrap_or(0);
                if n == 0 {
                    break;
                }
                full.extend_from_slice(&buf[..n]);
                let text = String::from_utf8_lossy(&full);
                if let Some((hdrs, body)) = text.split_once("\r\n\r\n") {
                    let content_len = hdrs
                        .lines()
                        .find_map(|l| {
                            let (k, v) = l.split_once(':')?;
                            k.eq_ignore_ascii_case("content-length")
                                .then(|| v.trim().parse::<usize>().ok())
                                .flatten()
                        })
                        .unwrap_or(0);
                    if body.len() >= content_len {
                        captured_srv.lock().unwrap().push(body.to_string());
                        break;
                    }
                }
            }
            let resp_body = json!({
                "choices": [{
                    "message": { "role": "assistant", "content": "Ringkasan visual: tombol biru." }
                }],
                "usage": { "prompt_tokens": 100, "completion_tokens": 10 }
            })
            .to_string();
            let resp = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                resp_body.len(),
                resp_body
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
        "vision-model".into(),
        Some(format!("http://{}/v1", addr)),
    )
    .unwrap();
    let img_history = vec![
        json!({
            "role": "user",
            "parts": [
                { "text": "lihat warna tombol di screenshot ini" },
                { "inlineData": { "mimeType": "image/png", "data": "iVBORw0KGgo=" } }
            ]
        }),
        json!({ "role": "model", "parts": [{ "text": "terlihat." }] }),
        json!({ "role": "user", "parts": [{ "text": "lanjutkan" }] }),
        json!({ "role": "model", "parts": [{ "text": "siap." }] }),
    ];
    let mut tabs = WorkspaceTabs::new("vision-model");
    tabs.active_mut().history = img_history.clone();

    let harness = AgentHarness::with_event_sink_and_tabs(
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
        "vision-model",
        &json!({
            "api": "openai-completions",
            "contextWindow": 16000,
            "maxTokens": 2000,
            "input": ["text", "image"]
        }),
    );
    harness.configure_model(
        "text-only-model",
        &json!({
            "api": "openai-completions",
            "contextWindow": 16000,
            "maxTokens": 2000,
            "input": ["text"]
        }),
    );

    // 1. Model multimodal -> payload compaction wajib membawa image_url (inlineData)
    let (_tx, rx) = tokio::sync::watch::channel(false);
    let prep1 = harness.compact_active_tab(None, rx.clone()).await.unwrap();
    assert!(prep1.compacted);

    // 2. Model text-only -> payload compaction TIDAK boleh menyertakan image_url
    harness.set_active_model("text-only-model");
    harness.replace_active_history("1", Some("Tab 1"), img_history);
    let prep2 = harness.compact_active_tab(None, rx).await.unwrap();
    assert!(prep2.compacted);

    let bodies = captured_bodies.lock().unwrap();
    assert_eq!(bodies.len(), 2);
    assert!(
        bodies[0].contains("data:image/png;base64,iVBORw0KGgo="),
        "multimodal summarizer must receive inline image data"
    );
    assert!(
        !bodies[1].contains("data:image/png;base64,iVBORw0KGgo="),
        "text-only summarizer must omit inline image data"
    );

    srv.abort();
}

#[test]
fn workspace_tabs_clamps_oob_active_index_and_backs_up_corrupt_file() {
    let dir = std::env::temp_dir().join(format!(
        "oopbot_tabs_recovery_{}_{}",
        std::process::id(),
        crate::common::time::epoch_ms()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    let tabs_path = dir.join("tabs.json");

    // 1. active_index out-of-bounds + stale next_id -> harus di-clamp aman tanpa panic.
    let oob_json = json!({
        "tabs": [{
            "id": "5",
            "title": "Tab 5",
            "model": "saved-model",
            "history": [],
            "timeline": []
        }],
        "active_index": 99,
        "next_id": 1
    });
    std::fs::write(&tabs_path, oob_json.to_string()).unwrap();
    let (loaded, warn) = WorkspaceTabs::load_from_file_checked(&tabs_path);
    assert!(warn.is_none());
    let mut ws = loaded.expect("valid tabs loaded");
    assert_eq!(ws.active_index(), 0);
    assert_eq!(ws.active().id, "5");
    assert_eq!(ws.new_tab("saved-model").id, "6");

    // 2. File korup -> harus dipindah ke *.corrupt-*.bak dan mengembalikan warning.
    std::fs::write(&tabs_path, "{broken json").unwrap();
    let (loaded2, warn2) = WorkspaceTabs::load_from_file_checked(&tabs_path);
    assert!(loaded2.is_none());
    let warning = warn2.expect("corrupt warning returned");
    assert!(warning.contains(".corrupt-"));
    assert!(!tabs_path.exists(), "corrupt tabs.json should be moved to backup");
    let backups: Vec<_> = std::fs::read_dir(&dir)
        .unwrap()
        .filter_map(|e| e.ok())
        .filter(|e| e.file_name().to_string_lossy().contains(".corrupt-"))
        .collect();
    assert_eq!(backups.len(), 1);

    // 3. Recovery warning di AgentHarness tidak boleh terhapus oleh persist sukses (mis. --model / set_active_model).
    struct DenyAll;
    #[async_trait]
    impl Approver for DenyAll {
        async fn approve(&self, _tool: &str, _preview: &str) -> bool {
            false
        }
    }
    let client = crate::llm::LlmClient::new("test-key".into(), "model-a".into()).unwrap();
    let harness = AgentHarness::with_event_sink_and_tabs(
        "OOPBoT",
        client,
        crate::tools::ToolRegistry::new(),
        Box::new(DenyAll),
        Box::new(NoopEventSink),
        2,
        WorkspaceTabs::new("model-a"),
        Some(tabs_path.clone()),
    );
    *harness.load_warning.lock().unwrap() = Some(warning.clone());
    harness.set_active_model("model-override");
    assert!(tabs_path.exists(), "set_active_model should persist new tabs.json");
    let surfaced = harness
        .take_persist_error()
        .expect("load_warning must survive successful set_active_model save");
    assert!(surfaced.contains(".corrupt-"));
    assert!(harness.take_persist_error().is_none());

    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn agent_calibrates_token_delta_avoids_per_step_thrash_and_preserves_saved_tab_model() {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let summary_calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let main_calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let summary_calls_srv = Arc::clone(&summary_calls);
    let main_calls_srv = Arc::clone(&main_calls);

    let srv = tokio::spawn(async move {
        for _ in 0..8 {
            let Ok((mut sock, _)) = listener.accept().await else {
                break;
            };
            let mut buf = vec![0u8; 65_536];
            let n = sock.read(&mut buf).await.unwrap_or(0);
            let req = String::from_utf8_lossy(&buf[..n]).to_string();
            let is_summary = req.contains("Summarize the conversation clearly");

            let body = if is_summary {
                summary_calls_srv.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                // Kembalikan ringkasan yang mendarat di zona NeedsCompaction (~625 token dari usable 750),
                // belum OverLimit, untuk menguji anti-thrash antar step tool (min_growth = 75).
                let dense_summary = "ringkasan padat penting ".repeat(90);
                json!({
                    "choices": [{
                        "message": { "role": "assistant", "content": dense_summary }
                    }],
                    "usage": { "prompt_tokens": 50, "completion_tokens": 120 }
                })
                .to_string()
            } else {
                let n_call = main_calls_srv.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                if n_call == 0 {
                    // Step 1 memanggil tool kecil (`echo_tool`) agar loop masuk step 2.
                    json!({
                        "choices": [{
                            "message": {
                                "role": "assistant",
                                "content": null,
                                "tool_calls": [{
                                    "id": "tc_1",
                                    "type": "function",
                                    "function": { "name": "echo_tool", "arguments": "{}" }
                                }]
                            }
                        }],
                        "usage": { "prompt_tokens": 625, "completion_tokens": 10 }
                    })
                    .to_string()
                } else {
                    json!({
                        "choices": [{
                            "message": { "role": "assistant", "content": "Selesai tanpa thrashing." }
                        }],
                        "usage": { "prompt_tokens": 660, "completion_tokens": 10 }
                    })
                    .to_string()
                }
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

    struct EchoTool;
    #[async_trait]
    impl crate::tools::Tool for EchoTool {
        fn name(&self) -> &'static str {
            "echo_tool"
        }
        fn description(&self) -> &'static str {
            "small echo"
        }
        fn parameters(&self) -> Value {
            json!({ "type": "object", "properties": {} })
        }
        async fn execute(&self, _args: &Value) -> Result<Value, String> {
            Ok(json!({ "ok": true }))
        }
    }

    let mut tools = crate::tools::ToolRegistry::new();
    tools.register(Box::new(EchoTool));

    // Client dimulai dengan default env model berbeda ("env-default"), tetapi tab tersimpan memakai "saved-tab-model".
    let client = crate::llm::LlmClient::new_with_base_url(
        "test-key".into(),
        "env-default".into(),
        Some(format!("http://{}/v1", addr)),
    )
    .unwrap();
    let mut tabs = WorkspaceTabs::new("saved-tab-model");
    tabs.active_mut().history = vec![
        json!({ "role": "user", "parts": [{ "text": "pesan lama 1" }] }),
        json!({ "role": "model", "parts": [{ "text": "jawab lama 1" }] }),
    ];

    let mut harness = AgentHarness::with_event_sink_and_tabs(
        "OOPBoT",
        client,
        tools,
        Box::new(AllowAll),
        Box::new(NoopEventSink),
        4,
        tabs,
        None,
    );
    harness.set_system_instruction("sys".into());
    harness.configure_model(
        "saved-tab-model",
        &json!({ "api": "openai-completions", "contextWindow": 1000, "maxTokens": 200 }),
    );
    harness.set_auto_compact(true);

    // #7: sync_client_to_active_tab_model mempertahankan model tab tersimpan & observasinya.
    let fp = harness.context_observation_fingerprint("saved-tab-model", &harness.tools.declarations());
    let base_est = harness.context_budget().total_estimated_tokens;
    {
        let mut t = harness.tabs.lock().unwrap();
        // Provider menokenisasi padat (observed=590 dari usable=750, threshold=600), padahal estimasi lokal kecil.
        t.active_mut()
            .set_observed_context_tokens_with_baseline(Some(590), Some(base_est), Some(fp));
    }
    assert_eq!(harness.sync_client_to_active_tab_model(), "saved-tab-model");
    assert_eq!(harness.model(), "saved-tab-model");
    assert_eq!(harness.context_budget().last_observed_input_tokens, Some(590));

    // #4: Menambahkan pesan baru (~25 token) harus mengkalibrasi observed = 590 + delta -> >= 600 (NeedsCompaction)
    // sehingga memicu compaction di step 1 (meski estimasi lokal saja jauh di bawah 600),
    // lalu #3: step 2 (setelah tool kecil) TIDAK memicu compaction ulang meski masih di NeedsCompaction.
    let reply = harness
        .chat("tolong jalankan echo tool sekarang ya")
        .await
        .unwrap();
    assert_eq!(reply, "Selesai tanpa thrashing.");
    assert_eq!(
        main_calls.load(std::sync::atomic::Ordering::SeqCst),
        2,
        "expected 2 main LLM steps (tool call + final answer)"
    );
    assert_eq!(
        summary_calls.load(std::sync::atomic::Ordering::SeqCst),
        1,
        "expected exactly 1 auto-compaction without thrashing on step 2"
    );

    srv.abort();
}
