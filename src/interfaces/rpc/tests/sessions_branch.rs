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
            v["topic"] == "oopbot:hello"
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
        v["topic"] == "oopbot:hello"
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
        v["topic"] == "oopbot:hello"
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
        v["topic"] == "oopbot:hello"
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
        v["topic"] == "oopbot:hello"
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
            v["topic"] == "oopbot:hello"
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


#[test]
fn audit_compaction_cut_point_skips_missing_id_and_preserves_assistant_under_budget() {
    use crate::interfaces::rpc::compaction::select_compaction_cut_point;

    // 2b: Under-budget (< 20k tokens) [User, Assistant] harus mempertahankan Assistant terakhir
    let short_entries = vec![
        json!({ "type": "message", "id": "u1", "message": { "role": "user", "content": "hi" } }),
        json!({ "type": "message", "id": "a1", "message": { "role": "assistant", "content": [{ "type": "text", "text": "hello" }] } }),
    ];
    let cut_short = select_compaction_cut_point(&short_entries).unwrap();
    assert_eq!(cut_short.first_kept_entry_id.as_deref(), Some("a1"));
    assert_eq!(cut_short.to_summarize.len(), 1);

    // 2c: Entry tanpa id non-empty tidak boleh dipilih sebagai cut-point agar kept suffix tidak hilang
    let missing_id_entries = vec![
        json!({ "type": "message", "id": "u1", "message": { "role": "user", "content": "old".repeat(30_000) } }),
        json!({ "type": "message", "message": { "role": "assistant", "content": [{ "type": "text", "text": "no-id-entry" }] } }),
        json!({ "type": "message", "id": "   ", "message": { "role": "user", "content": "blank-id-entry" } }),
    ];
    let cut_missing = select_compaction_cut_point(&missing_id_entries).unwrap();
    assert_eq!(cut_missing.first_kept_entry_id, None);
    assert_eq!(
        cut_missing.to_summarize.len(),
        missing_id_entries.len(),
        "retain-none harus merangkum seluruh active_entries ketika tidak ada cut-point ber-id valid"
    );

    // Entry ber-id valid berikutnya tetap bisa dipilih saat entry sebelumnya tanpa id
    let partial_id_entries = vec![
        json!({ "type": "message", "id": "u1", "message": { "role": "user", "content": "first" } }),
        json!({ "type": "message", "message": { "role": "user", "content": "legacy without id" } }),
        json!({ "type": "message", "id": "a2", "message": { "role": "assistant", "content": [{ "type": "text", "text": "kept" }] } }),
    ];
    let cut_partial = select_compaction_cut_point(&partial_id_entries).unwrap();
    assert_eq!(cut_partial.first_kept_entry_id.as_deref(), Some("a2"));
    assert_eq!(cut_partial.to_summarize.len(), 2);
}

