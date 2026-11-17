use super::*;
use crate::plugins::background::buffer::{OutputQuery, RingOutputBuffer};
use crate::tools::ToolRegistry;
use serde_json::json;
use std::sync::{Arc, Mutex};
use std::time::Duration;

fn setup_bg_tool() -> (BackgroundPlugin, ToolRegistry) {
    let plugin = BackgroundPlugin::new();
    let mut reg = ToolRegistry::new();
    plugin.register_tools(&mut reg);
    (plugin, reg)
}

fn is_pid_running(pid: &str) -> bool {
    let Ok(out) = std::process::Command::new("ps")
        .args(["-p", pid, "-o", "stat="])
        .stderr(std::process::Stdio::null())
        .output()
    else {
        return false;
    };
    if !out.status.success() {
        return false;
    }
    let stat = String::from_utf8_lossy(&out.stdout);
    let trimmed = stat.trim();
    !trimmed.is_empty() && !trimmed.starts_with('Z')
}

#[test]
fn ring_buffer_caps_memory_keeps_tail_and_respects_utf8_boundaries() {
    let mut buf = RingOutputBuffer::default();
    let chunk = "é".repeat(50_000); // 100.000 byte UTF-8 multibyte (> 64 KiB cap)
    buf.push_chunk(chunk.as_bytes());

    assert!(buf.is_truncated());
    assert_eq!(buf.total_bytes, 100_000);
    assert!(buf.window_start_byte() > 0);

    let rendered = buf.render_tail(512);
    assert!(rendered.contains("byte awal dipangkas"));
    assert!(rendered.contains("é"));
    assert!(!rendered.contains('\u{FFFD}'));
}

#[test]
fn incremental_cursor_since_byte_tail_lines_and_case_insensitive_filter() {
    let mut buf = RingOutputBuffer::default();
    buf.push_chunk(b"INFO boot\nWARN disk low\nerror: failed open\n");

    let first = buf.query(OutputQuery {
        since_byte: Some(0),
        tail_bytes: DEFAULT_OUTPUT_RETURN_BYTES,
        tail_lines: None,
        filter: None,
    });
    assert!(!first.cursor_reset);
    assert_eq!(first.next_byte, buf.total_bytes);
    assert!(first.text.contains("INFO boot"));

    // Polling ulang dengan cursor yang sama saat belum ada data baru -> kosong
    let empty_poll = buf.query(OutputQuery {
        since_byte: Some(first.next_byte),
        tail_bytes: DEFAULT_OUTPUT_RETURN_BYTES,
        tail_lines: None,
        filter: None,
    });
    assert_eq!(empty_poll.text, "");
    assert_eq!(empty_poll.next_byte, first.next_byte);
    assert!(!empty_poll.cursor_reset);

    // Tambah baris baru, baca inkremental -> hanya baris baru yang kembali
    buf.push_chunk(b"ERROR socket closed\nDEBUG ok\n");
    let delta = buf.query(OutputQuery {
        since_byte: Some(first.next_byte),
        tail_bytes: DEFAULT_OUTPUT_RETURN_BYTES,
        tail_lines: None,
        filter: None,
    });
    assert!(!delta.text.contains("INFO boot"));
    assert!(delta.text.contains("ERROR socket closed"));

    // Filter case-insensitive + tail_lines
    let filtered = buf.query(OutputQuery {
        since_byte: None,
        tail_bytes: DEFAULT_OUTPUT_RETURN_BYTES,
        tail_lines: Some(1),
        filter: Some("ERROR"),
    });
    assert!(filtered.text.contains("ERROR socket closed"));
    assert!(!filtered.text.contains("error: failed open"));

    // Simulasi buffer wrap jauh melampaui cursor lama -> cursor_reset = true
    let big = vec![b'x'; MAX_OUTPUT_BUFFER_BYTES + 1024];
    buf.push_chunk(&big);
    let wrapped = buf.query(OutputQuery {
        since_byte: Some(5),
        tail_bytes: 512,
        tail_lines: None,
        filter: None,
    });
    assert!(wrapped.cursor_reset);
    assert!(wrapped.truncated);
    assert!(wrapped.text.contains("cursor since_byte telah terlewati"));
}

#[tokio::test]
async fn reg011_background_kill_terminates_os_child_and_preserves_killed_status() {
    let (_plugin, reg) = setup_bg_tool();
    let tool = reg.get("background").unwrap();

    let start = tool
        .execute(&json!({ "action": "start", "command": "sleep 30" }))
        .await
        .unwrap();
    let id = start["id"].as_u64().unwrap();
    let pid = start["pid"].as_u64().expect("pid must be present") as i32;

    let kill_res = tool
        .execute(&json!({ "action": "kill", "id": id }))
        .await
        .unwrap();
    assert_eq!(kill_res["status"], "killed");

    // Tunggu sebentar agar OS selesai mematikan & menuai child process
    let pid_str = pid.to_string();
    for _ in 0..40 {
        if !is_pid_running(&pid_str) {
            break;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }

    let out = tool
        .execute(&json!({ "action": "output", "id": id }))
        .await
        .unwrap();
    assert_eq!(out["status"], "killed");

    // Pastikan proses OS benar-benar sudah berhenti (bukan masih aktif)
    assert!(
        !is_pid_running(&pid_str),
        "OS child process {} must not remain alive after kill",
        pid
    );
}

#[tokio::test]
async fn live_output_is_readable_while_running_and_kill_cleans_child_group() {
    let (_plugin, reg) = setup_bg_tool();
    let tool = reg.get("background").unwrap();

    let marker = format!(
        "/tmp/oopbot-bg-subproc-{}-{}.pid",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    );
    let cmd = format!(
        "echo LIVE_LINE_1; sleep 30 & echo $! > '{}'; wait",
        marker
    );
    let start = tool
        .execute(&json!({ "action": "start", "command": cmd }))
        .await
        .unwrap();
    let id = start["id"].as_u64().unwrap();

    // Tunggu output live muncul dan file PID subprocess tercipta
    let mut live_seen = false;
    let mut sub_pid: Option<String> = None;
    for _ in 0..30 {
        let out = tool
            .execute(&json!({ "action": "output", "id": id }))
            .await
            .unwrap();
        if out["output"]
            .as_str()
            .unwrap_or("")
            .contains("LIVE_LINE_1")
        {
            live_seen = true;
        }
        if let Ok(content) = std::fs::read_to_string(&marker) {
            let trimmed = content.trim().to_string();
            if !trimmed.is_empty() {
                sub_pid = Some(trimmed);
            }
        }
        if live_seen && sub_pid.is_some() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert!(live_seen, "Output harus bisa dibaca saat proses masih berjalan");
    let sub_pid = sub_pid.expect("subprocess pid marker harus tercipta");

    let list = tool.execute(&json!({ "action": "list" })).await.unwrap();
    assert_eq!(list["active"], 1);

    tool.execute(&json!({ "action": "kill", "id": id }))
        .await
        .unwrap();
    let _ = std::fs::remove_file(&marker);

    for _ in 0..40 {
        if !is_pid_running(&sub_pid) {
            break;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    assert!(
        !is_pid_running(&sub_pid),
        "Subprocess turunan {} dalam process group juga harus ikut mati",
        sub_pid
    );
}

#[tokio::test]
async fn timeout_stops_long_running_process_and_clear_keeps_running_tasks() {
    let (_plugin, reg) = setup_bg_tool();
    let tool = reg.get("background").unwrap();

    let timed = tool
        .execute(&json!({ "action": "start", "command": "sleep 30", "timeout_secs": 1 }))
        .await
        .unwrap();
    let timed_id = timed["id"].as_u64().unwrap();

    let keeper = tool
        .execute(&json!({ "action": "start", "command": "sleep 30" }))
        .await
        .unwrap();
    let keeper_id = keeper["id"].as_u64().unwrap();

    // Gunakan aksi `wait` untuk menunggu task timeout selesai secara event-driven
    let waited = tool
        .execute(&json!({ "action": "wait", "id": timed_id, "wait_timeout_secs": 5 }))
        .await
        .unwrap();
    assert_eq!(waited["status"], "timeout");
    assert_eq!(waited["wait_timed_out"], false);

    let cleared = tool.execute(&json!({ "action": "clear" })).await.unwrap();
    assert_eq!(cleared["removed"], 1);
    assert_eq!(cleared["remaining"], 1);

    let list = tool.execute(&json!({ "action": "list" })).await.unwrap();
    assert_eq!(list["active"], 1);
    assert_eq!(list["tasks"][0]["id"], keeper_id);

    tool.execute(&json!({ "action": "kill", "id": keeper_id }))
        .await
        .unwrap();
}

#[tokio::test]
async fn wait_returns_immediately_on_pattern_and_reports_wait_timed_out_without_killing() {
    let (_plugin, reg) = setup_bg_tool();
    let tool = reg.get("background").unwrap();

    let started = tool
        .execute(&json!({
            "action": "start",
            "label": "server-boot",
            "command": "echo BOOTING; sleep 0.15; echo READY_ON_8080; sleep 30"
        }))
        .await
        .unwrap();
    let id = started["id"].as_u64().unwrap();

    // Tunggu pattern READY_ON_8080 -> kembali saat proses masih running
    let matched_res = tool
        .execute(&json!({
            "action": "wait",
            "id": id,
            "pattern": "READY_ON_8080",
            "wait_timeout_secs": 5
        }))
        .await
        .unwrap();
    assert_eq!(matched_res["matched"], true);
    assert_eq!(matched_res["wait_timed_out"], false);
    assert_eq!(matched_res["status"], "running");
    assert_eq!(matched_res["label"], "server-boot");

    // Tunggu pattern yang tidak pernah muncul -> wait_timed_out: true, tetapi proses tetap hidup
    let timed_out_wait = tool
        .execute(&json!({
            "action": "wait",
            "id": id,
            "pattern": "NEVER_APPEARS",
            "wait_timeout_secs": 1
        }))
        .await
        .unwrap();
    assert_eq!(timed_out_wait["matched"], false);
    assert_eq!(timed_out_wait["wait_timed_out"], true);
    assert_eq!(timed_out_wait["status"], "running");

    tool.execute(&json!({ "action": "kill", "id": id }))
        .await
        .unwrap();
}

#[tokio::test]
async fn start_supports_cwd_env_label_and_validates_inputs() {
    let (_plugin, reg) = setup_bg_tool();
    let tool = reg.get("background").unwrap();

    // Error jika cwd tidak ada
    let bad_cwd = tool
        .execute(&json!({
            "action": "start",
            "command": "pwd",
            "cwd": "/tmp/oopbot-nonexistent-dir-999999"
        }))
        .await;
    assert!(bad_cwd.is_err());

    // Error jika nama env mengandung '='
    let bad_env = tool
        .execute(&json!({
            "action": "start",
            "command": "echo hi",
            "env": { "BAD=KEY": "1" }
        }))
        .await;
    assert!(bad_env.is_err());

    let dir = std::env::temp_dir().join(format!("oopbot-bg-cwd-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let dir_canonical = dir.canonicalize().unwrap();

    let started = tool
        .execute(&json!({
            "action": "start",
            "label": "env-cwd-check",
            "cwd": dir.to_string_lossy(),
            "env": { "OOPBOT_BG_TEST_VAR": "nilai_uji_42" },
            "command": "pwd; printf '%s\\n' \"$OOPBOT_BG_TEST_VAR\""
        }))
        .await
        .unwrap();
    let id = started["id"].as_u64().unwrap();
    assert_eq!(started["label"], "env-cwd-check");

    let waited = tool
        .execute(&json!({ "action": "wait", "id": id, "wait_timeout_secs": 5 }))
        .await
        .unwrap();
    assert_eq!(waited["status"], "completed");
    let out_text = waited["output"].as_str().unwrap_or("");
    assert!(out_text.contains("nilai_uji_42"));
    assert!(out_text.contains(&dir_canonical.to_string_lossy().to_string()));

    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn status_notifier_fires_when_task_finishes_naturally_and_shutdown_all_stops_active() {
    let (plugin, reg) = setup_bg_tool();
    let tool = reg.get("background").unwrap();

    let notifications = Arc::new(Mutex::new(Vec::<usize>::new()));
    let notif_clone = Arc::clone(&notifications);
    plugin.set_status_notifier(Arc::new(move |active| {
        notif_clone.lock().unwrap().push(active);
    }));

    let short = tool
        .execute(&json!({ "action": "start", "command": "echo done" }))
        .await
        .unwrap();
    let short_id = short["id"].as_u64().unwrap();

    let waited = tool
        .execute(&json!({ "action": "wait", "id": short_id, "wait_timeout_secs": 5 }))
        .await
        .unwrap();
    assert_eq!(waited["status"], "completed");

    // Beri sedikit waktu agar pemanggil emit_status_change selesai tepat setelah update_tx
    for _ in 0..20 {
        if !notifications.lock().unwrap().is_empty() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert_eq!(notifications.lock().unwrap().as_slice(), &[0]);

    // Sekarang uji shutdown_all
    tool.execute(&json!({ "action": "start", "command": "sleep 30" }))
        .await
        .unwrap();
    assert_eq!(plugin.active_count(), 1);

    plugin.shutdown_all();
    assert_eq!(plugin.active_count(), 0);
}

#[test]
fn requires_confirmation_flags_dangerous_background_commands_only_on_start() {
    let (_plugin, reg) = setup_bg_tool();
    let tool = reg.get("background").unwrap();

    assert!(!tool.requires_confirmation_for_args(&json!({
        "action": "start",
        "command": "npm run dev"
    })));
    assert!(tool.requires_confirmation_for_args(&json!({
        "action": "start",
        "command": "rm -rf /tmp/something"
    })));
    assert!(!tool.requires_confirmation_for_args(&json!({
        "action": "kill",
        "id": 1
    })));
    assert!(!tool.requires_confirmation_for_args(&json!({
        "action": "wait",
        "id": 1,
        "pattern": "READY"
    })));
}

#[tokio::test]
async fn error_handler_rejects_missing_action_and_invalid_argument_types() {
    let (_plugin, reg) = setup_bg_tool();
    let tool = reg.get("background").unwrap();

    // Missing / wrong-type action must fail instead of falling back to list
    assert!(tool.execute(&json!({})).await.is_err());
    assert!(tool.execute(&json!({ "action": 123 })).await.is_err());
    assert!(tool.execute(&json!({ "action": "unknown" })).await.is_err());

    // Start: mistyped optional fields or timeout_secs == 0 must fail cleanly
    assert!(tool
        .execute(&json!({ "action": "start", "command": "echo hi", "timeout_secs": 0 }))
        .await
        .is_err());
    assert!(tool
        .execute(&json!({ "action": "start", "command": "echo hi", "timeout_secs": -5 }))
        .await
        .is_err());
    assert!(tool
        .execute(&json!({ "action": "start", "command": "echo hi", "timeout_secs": "10" }))
        .await
        .is_err());
    assert!(tool
        .execute(&json!({ "action": "start", "command": "echo hi", "cwd": 123 }))
        .await
        .is_err());
    assert!(tool
        .execute(&json!({ "action": "start", "command": "echo hi", "label": 456 }))
        .await
        .is_err());

    // Output & wait: mistyped optional parameters must return Err, and huge tail_bytes clamps safely
    let started = tool
        .execute(&json!({ "action": "start", "command": "echo ok" }))
        .await
        .unwrap();
    let id = started["id"].as_u64().unwrap();

    assert!(tool
        .execute(&json!({ "action": "output", "id": id, "since_byte": -1 }))
        .await
        .is_err());
    assert!(tool
        .execute(&json!({ "action": "output", "id": id, "tail_lines": 0 }))
        .await
        .is_err());
    assert!(tool
        .execute(&json!({ "action": "output", "id": id, "filter": 999 }))
        .await
        .is_err());
    assert!(tool
        .execute(&json!({ "action": "wait", "id": id, "wait_timeout_secs": -1 }))
        .await
        .is_err());
    assert!(tool
        .execute(&json!({ "action": "wait", "id": id, "pattern": 42 }))
        .await
        .is_err());

    // 2^32 + 1 must clamp to MAX_OUTPUT_BUFFER_BYTES (not truncate to 1 -> 256 on 32-bit)
    let clamped = tool
        .execute(&json!({
            "action": "wait",
            "id": id,
            "tail_bytes": 4_294_967_297u64,
            "wait_timeout_secs": 5
        }))
        .await
        .unwrap();
    assert_eq!(clamped["status"], "completed");
}

#[tokio::test]
async fn completion_notifier_fires_for_autonomous_tasks_and_respects_suppression() {
    let (plugin, reg) = setup_bg_tool();
    let tool = reg.get("background").unwrap();

    // Mistyped notify_on_complete rejected cleanly
    assert!(tool
        .execute(&json!({ "action": "start", "command": "echo hi", "notify_on_complete": "yes" }))
        .await
        .is_err());

    let events = Arc::new(Mutex::new(Vec::<BgCompletionEvent>::new()));
    let ev_clone = Arc::clone(&events);
    plugin.set_completion_notifier(Arc::new(move |evt| {
        ev_clone.lock().unwrap().push(evt);
    }));

    // 1. Default notify_on_complete: true without active wait -> triggers completion event
    tool.execute(&json!({
        "action": "start",
        "label": "auto-task",
        "command": "echo AUTO_RESULT"
    }))
    .await
    .unwrap();

    for _ in 0..40 {
        if !events.lock().unwrap().is_empty() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    {
        let evs = events.lock().unwrap();
        assert_eq!(evs.len(), 1);
        assert_eq!(evs[0].label.as_deref(), Some("auto-task"));
        assert_eq!(evs[0].status, "completed");
        assert!(evs[0].output_tail.contains("AUTO_RESULT"));
    }
    events.lock().unwrap().clear();

    // 2. notify_on_complete: false -> suppressed
    let silent = tool
        .execute(&json!({
            "action": "start",
            "command": "echo SILENT",
            "notify_on_complete": false
        }))
        .await
        .unwrap();
    let silent_id = silent["id"].as_u64().unwrap();
    tool.execute(&json!({ "action": "wait", "id": silent_id, "wait_timeout_secs": 5 }))
        .await
        .unwrap();
    tokio::time::sleep(Duration::from_millis(40)).await;
    assert!(events.lock().unwrap().is_empty());

    // 3. Active waiter via `wait` -> suppressed (caller already receives wait result)
    let waited = tool
        .execute(&json!({
            "action": "start",
            "command": "sleep 0.15; echo WAITED_DONE"
        }))
        .await
        .unwrap();
    let waited_id = waited["id"].as_u64().unwrap();
    let wres = tool
        .execute(&json!({ "action": "wait", "id": waited_id, "wait_timeout_secs": 5 }))
        .await
        .unwrap();
    assert_eq!(wres["status"], "completed");
    tokio::time::sleep(Duration::from_millis(40)).await;
    assert!(events.lock().unwrap().is_empty());

    // 4. Killed task -> suppressed
    let killed = tool
        .execute(&json!({ "action": "start", "command": "sleep 30" }))
        .await
        .unwrap();
    let killed_id = killed["id"].as_u64().unwrap();
    tool.execute(&json!({ "action": "kill", "id": killed_id }))
        .await
        .unwrap();
    tokio::time::sleep(Duration::from_millis(60)).await;
    assert!(events.lock().unwrap().is_empty());
}

#[test]
fn incremental_since_byte_truncated_by_tail_bytes_emits_skip_banner() {
    let mut buf = RingOutputBuffer::default();
    let payload = vec![b'A'; 20_480]; // 20 KiB (< 64 KiB cap, > 10_000 tail_bytes)
    buf.push_chunk(&payload);

    let slice = buf.query(OutputQuery {
        since_byte: Some(0),
        tail_bytes: 10_000,
        tail_lines: None,
        filter: None,
    });
    assert!(!slice.cursor_reset);
    assert!(slice.truncated);
    assert_eq!(slice.next_byte, 20_480);
    assert!(
        slice.text.contains("10480 byte baru dilewati oleh batas tail_bytes; menampilkan 10000 byte terbaru"),
        "unexpected text: {}",
        &slice.text[..slice.text.len().min(200)]
    );
    assert!(slice.text.ends_with(&"A".repeat(10_000)));
}
