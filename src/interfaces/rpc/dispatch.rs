//! Loop pembacaan JSONL stdin (`run_loop_with_input`) dan routing command RPC.

use super::{
    active_branch_entries, extract_prompt_images, extract_user_message_text, save_settings_fields,
    ActiveOp, QueueMode, QueuedPrompt, ResumeTxSlotGuard, RpcRuntime,
};
use crate::common::BoxError;
use crate::plugins::UiDialogAnswer;
use serde_json::{json, Value};
use std::io::BufRead;
use std::sync::Arc;

impl RpcRuntime {
    pub async fn run_loop_with_input<R: BufRead + Send + 'static>(
        self: &Arc<Self>,
        mut reader: R,
    ) -> Result<(), BoxError> {
        Self::send_channel("oopbot", Value::Null);
        Self::send_channel(
            "oopbot:hello",
            json!({
                "version": 1,
                "engine": "oopbot-native",
                "oopbotVersion": env!("CARGO_PKG_VERSION")
            }),
        );

        self.emit_active_widgets();

        let pending = Arc::clone(&self.pending_approvals);
        let pending_ui = Arc::clone(&self.pending_ui_requests);
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<String>();

        let reader_thread = std::thread::spawn(move || {
            let mut line = String::new();
            loop {
                line.clear();
                match reader.read_line(&mut line) {
                    Ok(0) => {
                        pending.lock().unwrap_or_else(|e| e.into_inner()).clear();
                        pending_ui.lock().unwrap_or_else(|e| e.into_inner()).clear();
                        break;
                    }
                    Ok(_) => {
                        let trimmed = line.trim_end_matches(&['\r', '\n'][..]);
                        if trimmed.is_empty() {
                            continue;
                        }
                        if let Ok(val) = serde_json::from_str::<Value>(trimmed) {
                            let msg_type = val["type"].as_str().unwrap_or_default();
                            if msg_type == "extension_ui_response" {
                                let req_id = val["id"].as_str().unwrap_or_default();
                                if !req_id.is_empty() {
                                    let approval_sender = pending
                                        .lock()
                                        .unwrap_or_else(|e| e.into_inner())
                                        .remove(req_id);
                                    if let Some(sender) = approval_sender {
                                        let approved = if val["cancelled"].as_bool() == Some(true) {
                                            false
                                        } else {
                                            val["confirmed"].as_bool().unwrap_or(false)
                                        };
                                        let _ = sender.send(approved);
                                    } else if let Some(ui_sender) = pending_ui
                                        .lock()
                                        .unwrap_or_else(|e| e.into_inner())
                                        .remove(req_id)
                                    {
                                        let answer = if val["cancelled"].as_bool() == Some(true) {
                                            UiDialogAnswer::Cancelled
                                        } else if let Some(confirmed) = val["confirmed"].as_bool() {
                                            UiDialogAnswer::Confirmed(confirmed)
                                        } else if let Some(value) = val["value"].as_str() {
                                            UiDialogAnswer::Value(value.to_string())
                                        } else if !val["value"].is_null() {
                                            UiDialogAnswer::Value(val["value"].to_string())
                                        } else {
                                            UiDialogAnswer::Cancelled
                                        };
                                        let _ = ui_sender.send(answer);
                                    }
                                }
                                continue;
                            }
                            if msg_type == "oopbot_channel" && val["topic"] == "tool_approval" {
                                let data = &val["data"];
                                let req_id = data["request_id"]
                                    .as_str()
                                    .or_else(|| data["requestId"].as_str())
                                    .unwrap_or_default();
                                let approved = data["approved"].as_bool().unwrap_or(false);
                                if !req_id.is_empty() {
                                    if let Some(sender) = pending
                                        .lock()
                                        .unwrap_or_else(|e| e.into_inner())
                                        .remove(req_id)
                                    {
                                        let _ = sender.send(approved);
                                    }
                                }
                                continue;
                            }
                        }
                        if tx.send(line.clone()).is_err() {
                            break;
                        }
                    }
                    Err(_) => {
                        pending.lock().unwrap_or_else(|e| e.into_inner()).clear();
                        pending_ui.lock().unwrap_or_else(|e| e.into_inner()).clear();
                        break;
                    }
                }
            }
        });

        let mut tasks: tokio::task::JoinSet<()> = tokio::task::JoinSet::new();
        let (resume_tx, mut resume_rx) = tokio::sync::mpsc::unbounded_channel::<(
            QueuedPrompt,
            tokio::sync::watch::Receiver<bool>,
            u64,
        )>();
        {
            *self
                .resume_tx_slot
                .lock()
                .unwrap_or_else(|e| e.into_inner()) = Some(resume_tx.clone());
        }
        let _resume_slot_guard = ResumeTxSlotGuard {
            slot: Arc::clone(&self.resume_tx_slot),
        };

        loop {
            while tasks.try_join_next().is_some() {}

            let line = tokio::select! {
                biased;
                maybe_resumed = resume_rx.recv() => {
                    if let Some((first, cancel_rx, seq)) = maybe_resumed {
                        let rt = Arc::clone(self);
                        tasks.spawn(async move {
                            Self::run_prompt_coordinator(rt, first, cancel_rx, seq).await;
                        });
                    }
                    continue;
                }
                maybe_line = rx.recv() => {
                    match maybe_line {
                        Some(line) => line,
                        None => break,
                    }
                }
            };

            let trimmed = line.trim_end_matches(&['\r', '\n'][..]);
            if trimmed.is_empty() {
                continue;
            }
            let cmd = match serde_json::from_str::<Value>(trimmed) {
                Ok(v) => v,
                Err(e) => {
                    Self::send_line(&json!({
                        "type": "response",
                        "command": "parse",
                        "success": false,
                        "error": format!("Failed to parse command: {}", e),
                    }));
                    continue;
                }
            };
            let cmd_type = cmd["type"].as_str().unwrap_or_default().to_string();
            let id = cmd["id"].as_str().unwrap_or_default().to_string();

            match cmd_type.as_str() {
                "oopbot_channel" => {
                    let topic = cmd["topic"].as_str().unwrap_or_default();
                    let data = &cmd["data"];
                    self.handle_channel_message(topic, data);
                }
                "get_state" => {
                    self.refresh_credentials();
                    let is_idle = {
                        let m = self.mutable.lock().unwrap_or_else(|e| e.into_inner());
                        matches!(m.active_op, ActiveOp::Idle)
                    };
                    if !is_idle || self.needs_auto_scan().is_none() {
                        Self::send_response(&id, "get_state", self.state_json());
                    } else {
                        let rt = Arc::clone(self);
                        tasks.spawn(async move {
                            rt.maybe_auto_scan().await;
                            Self::send_response(&id, "get_state", rt.state_json());
                        });
                    }
                }
                "get_messages" => {
                    let messages = self
                        .ui_state
                        .lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .ui_messages
                        .clone();
                    Self::send_response(&id, "get_messages", json!({ "messages": messages }));
                }
                "get_session_stats" => {
                    self.handle_session_stats_command(&id);
                }
                "get_available_models" => {
                    self.refresh_credentials();
                    let is_idle = {
                        let m = self.mutable.lock().unwrap_or_else(|e| e.into_inner());
                        matches!(m.active_op, ActiveOp::Idle)
                    };
                    if !is_idle || self.needs_auto_scan().is_none() {
                        Self::send_response(
                            &id,
                            "get_available_models",
                            json!({ "models": self.available_models_json() }),
                        );
                    } else {
                        let rt = Arc::clone(self);
                        tasks.spawn(async move {
                            rt.maybe_auto_scan().await;
                            Self::send_response(
                                &id,
                                "get_available_models",
                                json!({ "models": rt.available_models_json() }),
                            );
                        });
                    }
                }
                "set_model" => {
                    let mut provider_changed = false;
                    {
                        let mut m = self.mutable.lock().unwrap_or_else(|e| e.into_inner());
                        if let Some(p) = cmd["provider"].as_str().filter(|s| !s.trim().is_empty())
                        {
                            if p != m.active_provider {
                                m.active_provider = p.to_string();
                                provider_changed = true;
                            }
                        }
                        if let Some(mid) = cmd["modelId"].as_str().filter(|s| !s.trim().is_empty())
                        {
                            m.active_model = mid.to_string();
                        }
                    }
                    self.refresh_credentials();
                    if provider_changed && self.needs_auto_scan().is_some() {
                        let rt = Arc::clone(self);
                        tasks.spawn(async move {
                            rt.maybe_auto_scan().await;
                            rt.sync_agent_active_model();
                            rt.sync_thinking_level_for_active_model();
                            let (ap, am, tl) = {
                                let m = rt.mutable.lock().unwrap_or_else(|e| e.into_inner());
                                (
                                    m.active_provider.clone(),
                                    m.active_model.clone(),
                                    m.thinking_level.clone(),
                                )
                            };
                            if let Err(e) = save_settings_fields(
                                &rt.config_dir,
                                Some(&ap),
                                Some(&am),
                                Some(&tl),
                            ) {
                                eprintln!(
                                    "[oopbot-rpc] gagal menyimpan settings.json (set_model): {}",
                                    e
                                );
                            }
                            rt.append_session_entry(
                                "model_change",
                                json!({ "provider": ap, "modelId": am }),
                            );
                            Self::send_response(&id, "set_model", rt.current_model_json());
                        });
                    } else {
                        self.sync_agent_active_model();
                        self.sync_thinking_level_for_active_model();
                        let (ap, am, tl) = {
                            let m = self.mutable.lock().unwrap_or_else(|e| e.into_inner());
                            (
                                m.active_provider.clone(),
                                m.active_model.clone(),
                                m.thinking_level.clone(),
                            )
                        };
                        if let Err(e) =
                            save_settings_fields(&self.config_dir, Some(&ap), Some(&am), Some(&tl))
                        {
                            eprintln!(
                                "[oopbot-rpc] gagal menyimpan settings.json (set_model): {}",
                                e
                            );
                        }
                        self.append_session_entry(
                            "model_change",
                            json!({ "provider": ap, "modelId": am }),
                        );
                        Self::send_response(&id, "set_model", self.current_model_json());
                    }
                }
                "cycle_model" => {
                    self.refresh_credentials();
                    let models = self.available_models_json();
                    if models.len() <= 1 {
                        Self::send_response(&id, "cycle_model", Value::Null);
                    } else {
                        let (cur_p, cur_m) = {
                            let m = self.mutable.lock().unwrap_or_else(|e| e.into_inner());
                            (m.active_provider.clone(), m.active_model.clone())
                        };
                        let cur_idx = models
                            .iter()
                            .position(|item| {
                                item["provider"].as_str() == Some(cur_p.as_str())
                                    && item["id"].as_str() == Some(cur_m.as_str())
                            })
                            .unwrap_or(0);
                        let next = &models[(cur_idx + 1) % models.len()];
                        if let Some(np) = next["provider"].as_str() {
                            self.mutable
                                .lock()
                                .unwrap_or_else(|e| e.into_inner())
                                .active_provider = np.to_string();
                        }
                        if let Some(nm) = next["id"].as_str() {
                            self.mutable
                                .lock()
                                .unwrap_or_else(|e| e.into_inner())
                                .active_model = nm.to_string();
                        }
                        self.refresh_credentials();
                        self.sync_agent_active_model();
                        self.sync_thinking_level_for_active_model();
                        let (ap, am, tl) = {
                            let m = self.mutable.lock().unwrap_or_else(|e| e.into_inner());
                            (
                                m.active_provider.clone(),
                                m.active_model.clone(),
                                m.thinking_level.clone(),
                            )
                        };
                        if let Err(e) =
                            save_settings_fields(&self.config_dir, Some(&ap), Some(&am), Some(&tl))
                        {
                            eprintln!(
                                "[oopbot-rpc] gagal menyimpan settings.json (cycle_model): {}",
                                e
                            );
                        }
                        self.append_session_entry(
                            "model_change",
                            json!({ "provider": ap, "modelId": am }),
                        );
                        Self::send_response(
                            &id,
                            "cycle_model",
                            json!({
                                "model": self.current_model_json(),
                                "thinkingLevel": tl,
                                "isScoped": false,
                            }),
                        );
                    }
                }
                "get_available_thinking_levels" => {
                    self.refresh_credentials();
                    let levels = self.active_model_info().available_thinking_levels();
                    Self::send_response(
                        &id,
                        "get_available_thinking_levels",
                        json!({ "levels": levels }),
                    );
                }
                "set_thinking_level" => {
                    if let Some(l) = cmd["level"].as_str().filter(|s| !s.trim().is_empty()) {
                        let clamped = self.active_model_info().clamp_thinking_level(l);
                        {
                            self.mutable
                                .lock()
                                .unwrap_or_else(|e| e.into_inner())
                                .thinking_level = clamped.clone();
                        }
                        self.agent.set_thinking_level(&clamped);
                        if let Err(e) =
                            save_settings_fields(&self.config_dir, None, None, Some(&clamped))
                        {
                            eprintln!(
                                "[oopbot-rpc] gagal menyimpan settings.json (set_thinking_level): {}",
                                e
                            );
                        }
                        self.append_session_entry(
                            "thinking_level_change",
                            json!({ "thinkingLevel": clamped }),
                        );
                        Self::send_line(&json!({
                            "type": "thinking_level_changed",
                            "level": clamped,
                        }));
                    }
                    Self::send_response(&id, "set_thinking_level", json!({}));
                }
                "cycle_thinking_level" => {
                    let info = self.active_model_info();
                    let levels = info.available_thinking_levels();
                    if levels.len() <= 1 {
                        Self::send_response(&id, "cycle_thinking_level", Value::Null);
                    } else {
                        let cur = self
                            .mutable
                            .lock()
                            .unwrap_or_else(|e| e.into_inner())
                            .thinking_level
                            .clone();
                        let idx = levels.iter().position(|l| l == &cur).unwrap_or(0);
                        let next_lvl = levels[(idx + 1) % levels.len()].clone();
                        {
                            self.mutable
                                .lock()
                                .unwrap_or_else(|e| e.into_inner())
                                .thinking_level = next_lvl.clone();
                        }
                        self.agent.set_thinking_level(&next_lvl);
                        if let Err(e) =
                            save_settings_fields(&self.config_dir, None, None, Some(&next_lvl))
                        {
                            eprintln!(
                                "[oopbot-rpc] gagal menyimpan settings.json (cycle_thinking_level): {}",
                                e
                            );
                        }
                        self.append_session_entry(
                            "thinking_level_change",
                            json!({ "thinkingLevel": next_lvl }),
                        );
                        Self::send_line(&json!({
                            "type": "thinking_level_changed",
                            "level": next_lvl,
                        }));
                        Self::send_response(
                            &id,
                            "cycle_thinking_level",
                            json!({ "level": next_lvl }),
                        );
                    }
                }
                "set_session_name" => {
                    let name = cmd["name"]
                        .as_str()
                        .map(|s| s.trim().to_string())
                        .filter(|s| !s.is_empty());
                    {
                        self.mutable
                            .lock()
                            .unwrap_or_else(|e| e.into_inner())
                            .session_name = name.clone();
                    }
                    self.append_session_entry("session_info", json!({ "name": name }));
                    Self::send_response(&id, "set_session_name", json!({}));
                    Self::send_line(&json!({
                        "type": "session_info_changed",
                        "name": name,
                    }));
                }
                "set_steering_mode" => {
                    let mode_str = cmd["mode"].as_str().unwrap_or_default();
                    if let Some(mode) = QueueMode::parse(mode_str) {
                        self.mutable
                            .lock()
                            .unwrap_or_else(|e| e.into_inner())
                            .steering_mode = mode;
                        Self::send_response(&id, "set_steering_mode", json!({}));
                    } else {
                        Self::send_error(
                            &id,
                            "set_steering_mode",
                            &format!("Mode steering '{}' tidak valid", mode_str),
                        );
                    }
                }
                "set_follow_up_mode" => {
                    let mode_str = cmd["mode"].as_str().unwrap_or_default();
                    if let Some(mode) = QueueMode::parse(mode_str) {
                        self.mutable
                            .lock()
                            .unwrap_or_else(|e| e.into_inner())
                            .follow_up_mode = mode;
                        Self::send_response(&id, "set_follow_up_mode", json!({}));
                    } else {
                        Self::send_error(
                            &id,
                            "set_follow_up_mode",
                            &format!("Mode follow-up '{}' tidak valid", mode_str),
                        );
                    }
                }
                "set_auto_compaction" => {
                    if let Some(enabled) = cmd["enabled"].as_bool() {
                        self.mutable
                            .lock()
                            .unwrap_or_else(|e| e.into_inner())
                            .auto_compaction_enabled = enabled;
                        Self::send_response(&id, "set_auto_compaction", json!({}));
                    } else {
                        Self::send_error(
                            &id,
                            "set_auto_compaction",
                            "Parameter boolean 'enabled' wajib diisi",
                        );
                    }
                }
                "set_auto_retry" => {
                    if let Some(enabled) = cmd["enabled"].as_bool() {
                        self.mutable
                            .lock()
                            .unwrap_or_else(|e| e.into_inner())
                            .auto_retry_enabled = enabled;
                        Self::send_response(&id, "set_auto_retry", json!({}));
                    } else {
                        Self::send_error(
                            &id,
                            "set_auto_retry",
                            "Parameter boolean 'enabled' wajib diisi",
                        );
                    }
                }
                "abort_retry" => {
                    if let Some(ref rtx) = self
                        .mutable
                        .lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .retry_cancel_tx
                    {
                        let _ = rtx.send(true);
                    }
                    Self::send_response(&id, "abort_retry", json!({}));
                }
                "clear_queue" => {
                    let (steering_texts, follow_up_texts) = {
                        let mut m = self.mutable.lock().unwrap_or_else(|e| e.into_inner());
                        let s: Vec<String> =
                            m.steering_queue.drain(..).map(|q| q.message).collect();
                        let f: Vec<String> =
                            m.follow_up_queue.drain(..).map(|q| q.message).collect();
                        m.resume_queues_after_compact = false;
                        (s, f)
                    };
                    // Emit queue_update kosong SEBELUM membalas clear_queue (ChatSteering.kt#L81)
                    Self::send_line(&json!({
                        "type": "queue_update",
                        "steering": [],
                        "followUp": [],
                    }));
                    Self::send_response(
                        &id,
                        "clear_queue",
                        json!({
                            "steering": steering_texts,
                            "followUp": follow_up_texts,
                        }),
                    );
                }
                "get_commands" => {
                    Self::send_response(
                        &id,
                        "get_commands",
                        json!({
                            "commands": [
                                { "name": "scan", "description": "Scan daftar model langsung dari token provider aktif" },
                                { "name": "token", "description": "Ganti API key & URL provider (/token <KEY> [URL])" },
                                { "name": "models", "description": "Tampilkan semua model yang tersedia di aplikasi" },
                                { "name": "model", "description": "Ganti model aktif (/model <ID> atau /model <provider>/<ID>)" },
                                { "name": "thinking", "description": "Atur level penalaran (/thinking <off|minimal|low|medium|high|xhigh>)" },
                                { "name": "provider", "description": "Lihat atau ganti provider aktif (/provider <ID>)" },
                                { "name": "max-tokens", "description": "Atur batas token output model aktif (/max-tokens <angka>)" },
                                { "name": "todos", "description": "Tampilkan semua todo pada branch aktif (/todos [clear])" },
                                { "name": "clear", "description": "Bersihkan memori sesi saat ini" }
                            ]
                        }),
                    );
                }
                "get_last_assistant_text" => {
                    let last_text = self
                        .ui_state
                        .lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .ui_messages
                        .iter()
                        .rev()
                        .find(|m| m["role"] == "assistant")
                        .and_then(|m| m["content"].as_array())
                        .and_then(|arr| {
                            arr.iter()
                                .find(|b| b["type"] == "text")
                                .and_then(|b| b["text"].as_str())
                        })
                        .map(str::to_string);
                    Self::send_response(
                        &id,
                        "get_last_assistant_text",
                        json!({ "text": last_text }),
                    );
                }
                "get_fork_messages" => {
                    let s = self.ui_state.lock().unwrap_or_else(|e| e.into_inner());
                    let branch = active_branch_entries(&s.entries, s.leaf_id.as_deref());
                    let fork_msgs: Vec<Value> = branch
                        .iter()
                        .filter(|e| {
                            e["type"].as_str() == Some("message")
                                && e["message"]["role"].as_str() == Some("user")
                        })
                        .filter_map(|e| {
                            let eid = e["id"].as_str()?;
                            let text = extract_user_message_text(&e["message"]);
                            Some(json!({ "entryId": eid, "text": text }))
                        })
                        .collect();
                    Self::send_response(
                        &id,
                        "get_fork_messages",
                        json!({ "messages": fork_msgs }),
                    );
                }
                "get_entries" => {
                    let since = cmd["since"].as_str();
                    let s = self.ui_state.lock().unwrap_or_else(|e| e.into_inner());
                    if let Some(cursor) = since {
                        if let Some(pos) =
                            s.entries.iter().position(|e| e["id"].as_str() == Some(cursor))
                        {
                            Self::send_response(
                                &id,
                                "get_entries",
                                json!({
                                    "entries": s.entries[pos + 1..],
                                    "leafId": s.leaf_id,
                                }),
                            );
                        } else {
                            Self::send_error(
                                &id,
                                "get_entries",
                                &format!("Entry '{}' tidak ditemukan", cursor),
                            );
                        }
                    } else {
                        Self::send_response(
                            &id,
                            "get_entries",
                            json!({
                                "entries": s.entries,
                                "leafId": s.leaf_id,
                            }),
                        );
                    }
                }
                "fork" => {
                    let entry_id = cmd["entryId"].as_str().unwrap_or_default();
                    self.handle_fork_command(&id, entry_id);
                }
                "clone" => {
                    self.handle_clone_command(&id);
                }
                "new_session" => {
                    self.handle_new_session_command(&id, cmd["parentSession"].as_str());
                }
                "switch_session" => {
                    let session_path = cmd["sessionPath"].as_str().unwrap_or_default();
                    self.handle_switch_session_command(&id, session_path);
                }
                "export_html" => {
                    self.handle_export_html_command(&id, cmd["outputPath"].as_str());
                }
                "compact" => {
                    let custom_instructions = cmd["customInstructions"]
                        .as_str()
                        .map(str::trim)
                        .filter(|s| !s.is_empty())
                        .map(str::to_string);

                    let reserved = {
                        let mut m = self.mutable.lock().unwrap_or_else(|e| e.into_inner());
                        if !matches!(m.active_op, ActiveOp::Idle) {
                            None
                        } else {
                            let (cancel_tx, cancel_rx) = tokio::sync::watch::channel(false);
                            let seq = m.next_op_seq;
                            m.next_op_seq += 1;
                            m.current_op_seq = seq;
                            m.active_op = ActiveOp::Compacting { cancel_tx };
                            Some((cancel_rx, seq))
                        }
                    };
                    let Some((cancel_rx, seq)) = reserved else {
                        Self::send_error(&id, "compact", "Agen sedang memproses operasi lain");
                        continue;
                    };

                    Self::send_line(&json!({
                        "type": "compaction_start",
                        "reason": "manual",
                    }));
                    let rt = Arc::clone(self);
                    let rtx = resume_tx.clone();
                    tasks.spawn(async move {
                        Self::run_compact_task(rt, id, custom_instructions, cancel_rx, seq, rtx)
                            .await;
                    });
                }
                "abort" => {
                    let mut completed_rx = self.completed_seq_tx.subscribe();
                    let wait_target = {
                        let mut guard = self.mutable.lock().unwrap_or_else(|e| e.into_inner());
                        let m = &mut *guard;
                        for q in &m.wakeup_queue {
                            if let Some(task_id) = q.background_task_id() {
                                m.dismissed_bg_wakeups.insert(task_id);
                            }
                        }
                        m.wakeup_queue
                            .retain(|q| q.background_task_id().is_none());
                        if let Some(ref rtx) = m.retry_cancel_tx {
                            let _ = rtx.send(true);
                        }
                        match &m.active_op {
                            ActiveOp::Idle => None,
                            ActiveOp::Streaming { cancel_tx }
                            | ActiveOp::Compacting { cancel_tx } => {
                                let _ = cancel_tx.send(true);
                                Some(m.current_op_seq)
                            }
                        }
                    };
                    if let Some(target_seq) = wait_target {
                        tasks.spawn(async move {
                            while *completed_rx.borrow_and_update() < target_seq {
                                if completed_rx.changed().await.is_err() {
                                    break;
                                }
                            }
                            Self::send_response(&id, "abort", json!({}));
                        });
                    } else {
                        Self::send_response(&id, "abort", json!({}));
                    }
                }
                "prompt" => {
                    let message = cmd["message"].as_str().unwrap_or_default().to_string();
                    let images = extract_prompt_images(&cmd["images"]);
                    let behavior = cmd["streamingBehavior"].as_str();

                    enum PromptDispatch {
                        Start {
                            cancel_rx: tokio::sync::watch::Receiver<bool>,
                            seq: u64,
                        },
                        Queued,
                        HandledEmpty,
                        Rejected(&'static str),
                    }

                    let action = {
                        let mut m = self.mutable.lock().unwrap_or_else(|e| e.into_inner());
                        if !matches!(m.active_op, ActiveOp::Idle) {
                            let is_compacting = matches!(m.active_op, ActiveOp::Compacting { .. });
                            match behavior {
                                Some("steer") => {
                                    m.steering_queue.push_back(QueuedPrompt::user(
                                        message.clone(),
                                        images.clone(),
                                    ));
                                    if is_compacting {
                                        m.resume_queues_after_compact = true;
                                    }
                                    PromptDispatch::Queued
                                }
                                Some("followUp") => {
                                    m.follow_up_queue.push_back(QueuedPrompt::user(
                                        message.clone(),
                                        images.clone(),
                                    ));
                                    if is_compacting {
                                        m.resume_queues_after_compact = true;
                                    }
                                    PromptDispatch::Queued
                                }
                                _ => PromptDispatch::Rejected(
                                    "Agen sedang berjalan; sertakan streamingBehavior ('steer' atau 'followUp')",
                                ),
                            }
                        } else if message.trim().is_empty() && images.is_empty() {
                            PromptDispatch::HandledEmpty
                        } else {
                            let (cancel_tx, cancel_rx) = tokio::sync::watch::channel(false);
                            let seq = m.next_op_seq;
                            m.next_op_seq += 1;
                            m.current_op_seq = seq;
                            m.active_op = ActiveOp::Streaming { cancel_tx };
                            PromptDispatch::Start { cancel_rx, seq }
                        }
                    };

                    match action {
                        PromptDispatch::Rejected(msg) => {
                            Self::send_error(&id, "prompt", msg);
                        }
                        PromptDispatch::HandledEmpty => {
                            Self::send_response(&id, "prompt", json!({ "disposition": "handled" }));
                        }
                        PromptDispatch::Queued => {
                            self.emit_queue_update();
                            Self::send_response(&id, "prompt", json!({ "disposition": "queued" }));
                        }
                        PromptDispatch::Start { cancel_rx, seq } => {
                            Self::send_response(&id, "prompt", json!({ "disposition": "started" }));
                            let rt = Arc::clone(self);
                            let first = QueuedPrompt::user(message, images);
                            tasks.spawn(async move {
                                Self::run_prompt_coordinator(rt, first, cancel_rx, seq).await;
                            });
                        }
                    }
                }
                "steer" => {
                    let message = cmd["message"].as_str().unwrap_or_default().to_string();
                    let images = extract_prompt_images(&cmd["images"]);
                    {
                        let mut m = self.mutable.lock().unwrap_or_else(|e| e.into_inner());
                        m.steering_queue
                            .push_back(QueuedPrompt::user(message, images));
                        if matches!(m.active_op, ActiveOp::Compacting { .. }) {
                            m.resume_queues_after_compact = true;
                        }
                    }
                    self.emit_queue_update();
                    Self::send_response(&id, "steer", json!({ "disposition": "queued" }));
                }
                "follow_up" => {
                    let message = cmd["message"].as_str().unwrap_or_default().to_string();
                    let images = extract_prompt_images(&cmd["images"]);
                    {
                        let mut m = self.mutable.lock().unwrap_or_else(|e| e.into_inner());
                        m.follow_up_queue
                            .push_back(QueuedPrompt::user(message, images));
                        if matches!(m.active_op, ActiveOp::Compacting { .. }) {
                            m.resume_queues_after_compact = true;
                        }
                    }
                    self.emit_queue_update();
                    Self::send_response(&id, "follow_up", json!({ "disposition": "queued" }));
                }
                "extension_ui_response" => {}
                other => {
                    if !id.is_empty() {
                        Self::send_error(
                            &id,
                            other,
                            &format!("Command '{}' tidak dikenal", other),
                        );
                    }
                }
            }
        }

        // EOF / stdin ditutup: batalkan operasi aktif dan tunggu seluruh task selesai dengan rapi.
        {
            *self
                .resume_tx_slot
                .lock()
                .unwrap_or_else(|e| e.into_inner()) = None;
        }
        resume_rx.close();
        {
            let mut m = self.mutable.lock().unwrap_or_else(|e| e.into_inner());
            m.resume_queues_after_compact = false;
            if let Some(ref rtx) = m.retry_cancel_tx {
                let _ = rtx.send(true);
            }
            if let ActiveOp::Streaming { ref cancel_tx }
            | ActiveOp::Compacting { ref cancel_tx } = m.active_op
            {
                let _ = cancel_tx.send(true);
            }
        }
        while let Ok((first, cancel_rx, seq)) = resume_rx.try_recv() {
            let rt = Arc::clone(self);
            tasks.spawn(async move {
                Self::run_prompt_coordinator(rt, first, cancel_rx, seq).await;
            });
        }
        while tasks.join_next().await.is_some() {}
        let _ = reader_thread.join();
        Ok(())
    }
}
