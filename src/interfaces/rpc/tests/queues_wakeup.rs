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
        v["topic"] == "oopbot:hello"
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
        v["topic"] == "oopbot:hello"
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
        v["topic"] == "oopbot:hello"
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
        v["topic"] == "oopbot:hello"
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
async fn rpc_background_channel_stop_terminates_active_tasks_and_notifier_emits_updates() {
    let _sink_guard = output_sink_test_mutex().lock().await;
    let tmp = TempConfigGuard::new("rpc_bg_channel_stop");
    let server = RpcServer::new_in_dir_with_cwd(
        tmp.path().to_path_buf(),
        tmp.path().to_path_buf(),
        None,
        None,
        vec!["oopbot-background".to_string()],
    )
    .unwrap();

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
    set_output_sink(Some(Arc::new(Mutex::new(
        Box::new(TestBuf(raw.clone())) as Box<dyn io::Write + Send + Sync>,
    ))));

    assert!(server.runtime.plugins.is_active("oopbot-background"));

    let bg_plugin = server
        .runtime
        .plugins
        .get_plugin("oopbot-background")
        .unwrap();
    let bg = bg_plugin
        .as_any()
        .and_then(|a| a.downcast_ref::<BackgroundPlugin>())
        .unwrap();

    // 1. Jalankan proses singkat yang selesai alami -> notifier harus memancarkan active: 0
    let mut reg = crate::tools::ToolRegistry::new();
    bg_plugin.register_tools(&mut reg);
    let tool = reg.get("background").unwrap();

    let fast = tool
        .execute(&json!({ "action": "start", "command": "echo ok" }))
        .await
        .unwrap();
    let fast_id = fast["id"].as_u64().unwrap();
    tool.execute(&json!({ "action": "wait", "id": fast_id, "wait_timeout_secs": 5 }))
        .await
        .unwrap();

    let mut notifier_emitted = false;
    for _ in 0..30 {
        let s = {
            let mut guard = raw.lock().unwrap();
            let txt = String::from_utf8_lossy(&guard).into_owned();
            guard.clear();
            txt
        };
        if s.contains("\"oopbot_channel\"") && s.contains("\"active\":0") {
            notifier_emitted = true;
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(15)).await;
    }
    assert!(
        notifier_emitted,
        "Status notifier harus memancarkan active: 0 saat task selesai alami"
    );

    // 2. Jalankan proses sleep panjang lalu kirim channel command stop dari UI
    tool.execute(&json!({ "action": "start", "command": "sleep 30" }))
        .await
        .unwrap();
    assert_eq!(bg.active_count(), 1);

    server
        .runtime
        .handle_channel_message("background", &json!({ "action": "stop" }));
    assert_eq!(bg.active_count(), 0);

    let bytes = raw.lock().unwrap().clone();
    let emitted = String::from_utf8_lossy(&bytes);
    assert!(emitted.contains("\"topic\":\"background\""));
    assert!(emitted.contains("\"active\":0"));

    set_output_sink(None);
}


#[tokio::test]
async fn background_completion_auto_wakeup_when_idle() {
    let _sink_guard = output_sink_test_mutex().lock().await;
    let (_reset, mut out_rx) = install_channel_output_sink();
    let tmp = TempConfigGuard::new("rpc_bg_wakeup_idle");

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
                                    "id": "call_bg_start",
                                    "name": "background",
                                    "args": {
                                        "action": "start",
                                        "label": "quick-job",
                                        "command": "sleep 0.25 && echo DONE_WAKEUP"
                                    }
                                }
                            }]
                        }
                    }]
                }),
            ),
            1 => (
                200,
                gemini_text_resp("Tugas background sudah berjalan, aku akan kabari setelah selesai.", 30, 15),
            ),
            _ => {
                let body_s = req.body.to_string();
                let reply = if body_s.contains("DONE_WAKEUP")
                    && body_s.contains("Background Task Finished")
                {
                    "Laporan otomatis: quick-job selesai dengan DONE_WAKEUP."
                } else {
                    "Wakeup tanpa output tail"
                };
                (200, gemini_text_resp(reply, 50, 15))
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
        vec!["oopbot-background".to_string()],
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
        json!({ "id": "p1", "type": "prompt", "message": "jalankan quick-job" }),
    );
    let _ = recv_response(&mut out_rx, "p1", std::time::Duration::from_millis(500)).await;

    // 1. Tunggu giliran awal selesai & agen kembali Idle
    let _settled1 = recv_matching(&mut out_rx, std::time::Duration::from_secs(3), |v| {
        v["type"] == "agent_settled" && v["aborted"] == false
    })
    .await;

    // 2. Tanpa mengirim perintah baru, agen harus bangun otomatis dengan pesan role=custom
    let custom_ev = recv_matching(&mut out_rx, std::time::Duration::from_secs(4), |v| {
        v["type"] == "message_end" && v["message"]["role"] == "custom"
    })
    .await;
    assert_eq!(custom_ev["message"]["customType"], "oopbot-background-wakeup");
    assert_eq!(custom_ev["message"]["display"], true);
    let custom_txt = custom_ev["message"]["content"][0]["text"]
        .as_str()
        .unwrap_or("");
    assert!(custom_txt.contains("quick-job"), "got: {}", custom_txt);

    let wakeup_reply = recv_matching(&mut out_rx, std::time::Duration::from_secs(4), |v| {
        v["type"] == "message_end"
            && v["message"]["role"] == "assistant"
            && v["message"]["stopReason"] == "stop"
            && v["message"]["content"][0]["text"]
                .as_str()
                .map(|t| t.contains("Laporan otomatis"))
                .unwrap_or(false)
    })
    .await;
    assert_eq!(
        wakeup_reply["message"]["content"][0]["text"],
        "Laporan otomatis: quick-job selesai dengan DONE_WAKEUP."
    );
    let _settled2 = recv_matching(&mut out_rx, std::time::Duration::from_secs(3), |v| {
        v["type"] == "agent_settled" && v["aborted"] == false
    })
    .await;
    assert!(reqs.lock().unwrap().len() >= 3);

    drop(cmd_tx);
    let _ = tokio::time::timeout(std::time::Duration::from_secs(2), loop_task).await;
    mock_handle.abort();
}

#[tokio::test]
async fn background_completion_queues_followup_when_streaming() {
    let _sink_guard = output_sink_test_mutex().lock().await;
    let (_reset, mut out_rx) = install_channel_output_sink();
    let tmp = TempConfigGuard::new("rpc_bg_wakeup_streaming");

    let gate = Arc::new(tokio::sync::Notify::new());
    let gate_srv = Arc::clone(&gate);
    let (http_blocked_tx, http_blocked_rx) = tokio::sync::oneshot::channel::<()>();
    let http_blocked_tx = Arc::new(Mutex::new(Some(http_blocked_tx)));

    let (base_url, _reqs, mock_handle) = spawn_mock_llm_server(move |idx, req| {
        let gate_srv = Arc::clone(&gate_srv);
        let http_blocked_tx = Arc::clone(&http_blocked_tx);
        async move {
            match idx {
                0 => {
                    if let Some(tx) = http_blocked_tx.lock().unwrap().take() {
                        let _ = tx.send(());
                    }
                    gate_srv.notified().await;
                    (200, gemini_text_resp("Turn utama selesai.", 25, 10))
                }
                _ => {
                    let s = req.body.to_string();
                    let reply = if s.contains("QUEUED_WAKEUP_OK") {
                        "Follow-up background diproses otomatis."
                    } else {
                        "Follow-up tidak sesuai"
                    };
                    (200, gemini_text_resp(reply, 40, 10))
                }
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
        vec!["oopbot-background".to_string()],
    )
    .unwrap();

    // Ambil background tool langsung agar kita bisa start task saat LLM sedang Streaming
    let mut reg = crate::tools::ToolRegistry::new();
    server
        .runtime
        .plugins
        .get_plugin("oopbot-background")
        .unwrap()
        .register_tools(&mut reg);
    let bg_tool = reg.get("background").unwrap();

    let (cmd_tx, reader) = TestLineReader::new();
    let loop_task = tokio::spawn(async move {
        server.run_loop_with_input(io::BufReader::new(reader)).await.unwrap();
    });

    let _ = recv_matching(&mut out_rx, std::time::Duration::from_secs(2), |v| {
        v["topic"] == "oopbot:hello"
    })
    .await;

    // 1. Mulai turn streaming yang tertahan di gate mock LLM
    send_cmd(
        &cmd_tx,
        json!({ "id": "p_busy", "type": "prompt", "message": "tunggu bentar" }),
    );
    let _ = recv_response(&mut out_rx, "p_busy", std::time::Duration::from_millis(500)).await;
    tokio::time::timeout(std::time::Duration::from_secs(2), http_blocked_rx)
        .await
        .unwrap()
        .unwrap();

    // 2. Jalankan task background cepat sementara agen masih Streaming
    bg_tool
        .execute(&json!({
            "action": "start",
            "label": "stream-job",
            "command": "echo QUEUED_WAKEUP_OK"
        }))
        .await
        .unwrap();

    // 3. Tunggu queue_update berisi ringkasan wakeup di followUp
    let qu = recv_matching(&mut out_rx, std::time::Duration::from_secs(3), |v| {
        v["type"] == "queue_update"
            && v["followUp"]
                .as_array()
                .map(|a| !a.is_empty())
                .unwrap_or(false)
    })
    .await;
    assert!(qu["followUp"][0].as_str().unwrap_or("").contains("stream-job"));

    // 4. Lepas gate HTTP -> coordinator langsung memproses follow-up wakeup otomatis
    gate.notify_one();

    let custom_ev = recv_matching(&mut out_rx, std::time::Duration::from_secs(3), |v| {
        v["type"] == "message_end" && v["message"]["role"] == "custom"
    })
    .await;
    assert_eq!(custom_ev["message"]["customType"], "oopbot-background-wakeup");

    let followup_reply = recv_matching(&mut out_rx, std::time::Duration::from_secs(3), |v| {
        v["type"] == "message_end"
            && v["message"]["role"] == "assistant"
            && v["message"]["content"][0]["text"]
                .as_str()
                .map(|t| t.contains("Follow-up background diproses otomatis"))
                .unwrap_or(false)
    })
    .await;
    assert_eq!(
        followup_reply["message"]["content"][0]["text"],
        "Follow-up background diproses otomatis."
    );
    let _ = recv_matching(&mut out_rx, std::time::Duration::from_secs(3), |v| {
        v["type"] == "agent_settled" && v["aborted"] == false
    })
    .await;

    drop(cmd_tx);
    let _ = tokio::time::timeout(std::time::Duration::from_secs(2), loop_task).await;
    mock_handle.abort();
}


#[tokio::test]
async fn audit_steer_command_during_compact_resumes_after_compact() {
    let _sink_guard = output_sink_test_mutex().lock().await;
    let (_reset, mut out_rx) = install_channel_output_sink();
    let tmp = TempConfigGuard::new("audit_steer_compact");

    let compact_gate = Arc::new(tokio::sync::Notify::new());
    let compact_gate_srv = Arc::clone(&compact_gate);
    let (compact_entered_tx, compact_entered_rx) = tokio::sync::oneshot::channel::<()>();
    let compact_entered_tx = Arc::new(Mutex::new(Some(compact_entered_tx)));

    let (base_url, reqs, mock_handle) = spawn_mock_llm_server(move |idx, _req| {
        let compact_gate_srv = Arc::clone(&compact_gate_srv);
        let compact_entered_tx = Arc::clone(&compact_entered_tx);
        async move {
            if idx == 0 {
                if let Some(tx) = compact_entered_tx.lock().unwrap().take() {
                    let _ = tx.send(());
                }
                compact_gate_srv.notified().await;
                (200, gemini_text_resp("## Goal\nRingkasan kompak", 120, 30))
            } else {
                (200, gemini_text_resp("Perintah steer dieksekusi pasca compact", 60, 15))
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
        "k-steer-cmp",
    );

    let session_file = tmp.path().join("steer_cmp.jsonl");
    let lines = [
        json!({ "type": "session", "version": 3, "id": "s-steer-cmp", "timestamp": "2026-11-12T00:00:00Z", "cwd": tmp.path() }).to_string(),
        json!({ "type": "message", "id": "e0", "parentId": null, "timestamp": "2026-11-12T00:00:01Z",
            "message": { "role": "user", "content": "turn 1", "timestamp": 1 } }).to_string(),
        json!({ "type": "message", "id": "e1", "parentId": "e0", "timestamp": "2026-11-12T00:00:02Z",
            "message": { "role": "assistant", "stopReason": "stop", "content": [{ "type": "text", "text": "ans 1" }], "timestamp": 2 } }).to_string(),
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

    send_cmd(&cmd_tx, json!({ "id": "cmp1", "type": "compact" }));
    let _ = recv_matching(&mut out_rx, std::time::Duration::from_secs(2), |v| {
        v["type"] == "compaction_start"
    })
    .await;
    tokio::time::timeout(std::time::Duration::from_secs(2), compact_entered_rx)
        .await
        .unwrap()
        .unwrap();

    // Kirim command `"steer"` (bukan `"prompt"`) selagi ActiveOp::Compacting
    send_cmd(&cmd_tx, json!({ "id": "st1", "type": "steer", "message": "lanjutkan steer pasca compact" }));
    let qu = recv_matching(&mut out_rx, std::time::Duration::from_secs(2), |v| {
        v["type"] == "queue_update" && v["steering"] == json!(["lanjutkan steer pasca compact"])
    })
    .await;
    assert_eq!(qu["steering"], json!(["lanjutkan steer pasca compact"]));
    let st_resp = recv_response(&mut out_rx, "st1", std::time::Duration::from_millis(500)).await;
    assert_eq!(st_resp["data"]["disposition"], "queued");

    // Lepas compaction -> steer harus otomatis dijalankan tanpa prompt baru
    compact_gate.notify_one();
    let _ = recv_matching(&mut out_rx, std::time::Duration::from_secs(3), |v| {
        v["type"] == "compaction_end" && v["aborted"] == false
    })
    .await;
    let settled = recv_matching(&mut out_rx, std::time::Duration::from_secs(3), |v| {
        v["type"] == "agent_settled" && v["aborted"] == false
    })
    .await;
    assert_eq!(settled["aborted"], false);

    let captured = reqs.lock().unwrap().clone();
    assert!(
        captured.iter().any(|r| r.body.to_string().contains("lanjutkan steer pasca compact")),
        "command steer selama compact harus dieksekusi otomatis setelah compact selesai"
    );

    drop(cmd_tx);
    let _ = tokio::time::timeout(std::time::Duration::from_secs(2), loop_task).await;
    mock_handle.abort();
}

