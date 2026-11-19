//! Koordinator eksekusi turn (`run_prompt_coordinator`, `execute_single_turn`),
//! auto-wakeup background task (`trigger_background_wakeup`), dan retry transien LLM.

use super::{
    ActiveOp, CoordinatorFinishGuard, CustomWakeupMeta, QueueMode, QueuedPrompt, ResumeTx,
    RpcRuntime,
};
use crate::agent::{ChatInput, SteerHook};
use crate::common::time::epoch_ms;
use crate::plugins::BgCompletionEvent;
use crate::tools::truncate;
use serde_json::{json, Value};
use std::sync::atomic::Ordering;
use std::sync::Arc;

impl RpcRuntime {
    pub(super) fn emit_queue_update(&self) {
        let (steering, follow_up) = {
            let m = self.mutable.lock().unwrap_or_else(|e| e.into_inner());
            m.user_queue_snapshot()
        };
        Self::send_line(&json!({
            "type": "queue_update",
            "steering": steering,
            "followUp": follow_up,
        }));
    }

    /// Batalkan pending auto-wakeup background untuk `task_ids` tertentu dan catat ke
    /// `dismissed_bg_wakeups` agar callback completion yang terlambat tiba (race thread) ikut dibuang.
    pub(super) fn dismiss_background_wakeups(&self, task_ids: &[u32]) {
        if task_ids.is_empty() {
            return;
        }
        let mut m = self.mutable.lock().unwrap_or_else(|e| e.into_inner());
        for &id in task_ids {
            m.dismissed_bg_wakeups.insert(id);
        }
        m.wakeup_queue
            .retain(|q| !q.background_task_id().is_some_and(|id| task_ids.contains(&id)));
    }

    /// Bersihkan seluruh pending auto-wakeup background (mis. saat `background: stop` atau `abort`).
    pub(super) fn clear_all_background_wakeups(&self) {
        let mut guard = self.mutable.lock().unwrap_or_else(|e| e.into_inner());
        let m = &mut *guard;
        for q in &m.wakeup_queue {
            if let Some(id) = q.background_task_id() {
                m.dismissed_bg_wakeups.insert(id);
            }
        }
        m.wakeup_queue
            .retain(|q| q.background_task_id().is_none());
    }

    pub(super) fn emit_and_persist_user_message(
        &self,
        prompt: &str,
        images: &[(String, String)],
    ) -> u64 {
        let started_ms = epoch_ms();
        let mut user_content_blocks = Vec::new();
        if !prompt.is_empty() {
            user_content_blocks.push(json!({ "type": "text", "text": prompt }));
        }
        for (mime, data) in images {
            user_content_blocks.push(json!({
                "type": "image",
                "mimeType": mime,
                "data": data,
            }));
        }
        if user_content_blocks.is_empty() {
            user_content_blocks.push(json!({ "type": "text", "text": "" }));
        }

        let user_msg = json!({
            "role": "user",
            "content": user_content_blocks,
            "timestamp": started_ms,
        });
        Self::send_line(&json!({ "type": "message_start", "message": user_msg }));
        Self::send_line(&json!({ "type": "message_end", "message": user_msg }));
        {
            self.ensure_session_header();
            let mut state = self.ui_state.lock().unwrap_or_else(|e| e.into_inner());
            state.ui_messages.push(user_msg.clone());
            state.append_session_entry("message", json!({ "message": user_msg }));
        }
        started_ms
    }

    pub(super) fn emit_and_persist_wakeup_message(
        &self,
        custom_type: &str,
        summary: &str,
    ) -> u64 {
        let started_ms = epoch_ms();
        let custom_msg = json!({
            "role": "custom",
            "customType": custom_type,
            "display": true,
            "content": [{ "type": "text", "text": summary }],
            "timestamp": started_ms,
        });
        Self::send_line(&json!({ "type": "message_start", "message": custom_msg }));
        Self::send_line(&json!({ "type": "message_end", "message": custom_msg }));
        {
            self.ensure_session_header();
            let mut state = self.ui_state.lock().unwrap_or_else(|e| e.into_inner());
            state.ui_messages.push(custom_msg.clone());
            state.append_session_entry("message", json!({ "message": custom_msg }));
        }
        started_ms
    }

    pub(super) fn emit_and_persist_queued_prompt(&self, q: &QueuedPrompt) -> u64 {
        match &q.custom_notice {
            Some(meta) => self.emit_and_persist_wakeup_message(&meta.custom_type, &meta.summary),
            None => self.emit_and_persist_user_message(&q.message, &q.images),
        }
    }

    /// Bangunkan agen otomatis (jika Idle) atau antrekan ke `wakeup_queue` internal (jika sedang aktif)
    /// ketika proses background selesai, gagal, atau melewati batas waktu secara mandiri.
    /// KONTRAK: tidak pernah masuk `follow_up_queue`/`queue_update` agar tidak muncul sebagai `STEERING` UI.
    pub(super) fn trigger_background_wakeup(&self, evt: BgCompletionEvent) {
        let task_id = evt.id;
        let task_name = evt
            .label
            .as_deref()
            .filter(|s| !s.is_empty())
            .unwrap_or(evt.command.as_str());
        let exit_str = evt
            .exit_code
            .map(|c| format!(" (exit {})", c))
            .unwrap_or_default();
        let summary = format!(
            "Background #{} `{}` {} dalam {} dtk{}",
            evt.id,
            truncate(task_name, 60),
            evt.status,
            evt.elapsed_secs,
            exit_str
        );

        let mut prompt_msg = format!(
            "[System Event - Background Task Finished]\n\
            Task background #{} telah selesai di latar belakang.\n\
            - Status: {}\n",
            evt.id, evt.status
        );
        if let Some(ref lbl) = evt.label {
            prompt_msg.push_str(&format!("- Label: {}\n", lbl));
        }
        prompt_msg.push_str(&format!(
            "- Command: {}\n\
            - Exit code: {}\n\
            - Durasi: {} dtk\n",
            evt.command,
            evt.exit_code
                .map(|c| c.to_string())
                .unwrap_or_else(|| "-".to_string()),
            evt.elapsed_secs
        ));
        if evt.output_tail.trim().is_empty() {
            prompt_msg.push_str("- Output tail: (kosong)\n");
        } else {
            prompt_msg.push_str(&format!("- Output tail:\n```\n{}\n```\n", evt.output_tail));
        }
        prompt_msg.push_str(
            "\nSampaikan hasil/ringkasannya ke user (atau lanjutkan langkah kerja berikutnya bila relevan). Jika butuh log lebih lengkap, gunakan `background` action `output`.",
        );

        let queued = QueuedPrompt::wakeup(
            prompt_msg,
            "oopbot-background-wakeup".to_string(),
            summary,
            Some(task_id),
        );

        let rtx_opt = self
            .resume_tx_slot
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone();

        enum WakeupDispatch {
            Start {
                rtx: ResumeTx,
                cancel_rx: tokio::sync::watch::Receiver<bool>,
                seq: u64,
            },
            Queued,
            Dismissed,
            NoLoop,
        }

        let dispatch = {
            let mut m = self.mutable.lock().unwrap_or_else(|e| e.into_inner());
            if m.dismissed_bg_wakeups.contains(&task_id) {
                WakeupDispatch::Dismissed
            } else {
                match m.active_op {
                    ActiveOp::Idle => {
                        if let Some(rtx) = rtx_opt {
                            let (cancel_tx, cancel_rx) = tokio::sync::watch::channel(false);
                            let seq = m.next_op_seq;
                            m.next_op_seq += 1;
                            m.current_op_seq = seq;
                            m.active_op = ActiveOp::Streaming { cancel_tx };
                            WakeupDispatch::Start { rtx, cancel_rx, seq }
                        } else {
                            WakeupDispatch::NoLoop
                        }
                    }
                    ActiveOp::Streaming { .. } => {
                        m.wakeup_queue
                            .retain(|q| q.background_task_id() != Some(task_id));
                        m.wakeup_queue.push_back(queued.clone());
                        WakeupDispatch::Queued
                    }
                    ActiveOp::Compacting { .. } => {
                        m.wakeup_queue
                            .retain(|q| q.background_task_id() != Some(task_id));
                        m.wakeup_queue.push_back(queued.clone());
                        m.resume_queues_after_compact = true;
                        WakeupDispatch::Queued
                    }
                }
            }
        };

        match dispatch {
            WakeupDispatch::Start { rtx, cancel_rx, seq } => {
                if rtx.send((queued, cancel_rx, seq)).is_err() {
                    let mut m = self.mutable.lock().unwrap_or_else(|e| e.into_inner());
                    if matches!(m.active_op, ActiveOp::Streaming { .. }) && m.current_op_seq == seq
                    {
                        m.active_op = ActiveOp::Idle;
                        self.publish_completed_seq(seq);
                    }
                }
            }
            WakeupDispatch::Queued | WakeupDispatch::Dismissed | WakeupDispatch::NoLoop => {}
        }
    }

    pub(super) fn emit_and_persist_abort_message(&self, started_ms: u64, emit_turn_events: bool) {
        let abort_msg = json!({
            "role": "assistant",
            "content": [],
            "stopReason": "aborted",
            "timestamp": started_ms,
            "durationMs": epoch_ms().saturating_sub(started_ms),
        });
        Self::send_line(&json!({ "type": "message_start", "message": abort_msg }));
        Self::send_line(&json!({ "type": "message_end", "message": abort_msg }));
        {
            self.ensure_session_header();
            let mut state = self.ui_state.lock().unwrap_or_else(|e| e.into_inner());
            state.ui_messages.push(abort_msg.clone());
            state.append_session_entry("message", json!({ "message": abort_msg }));
        }
        if emit_turn_events {
            Self::send_line(&json!({ "type": "turn_end" }));
            Self::send_line(&json!({ "type": "agent_end" }));
        }
        self.emit_active_widgets();
    }

    pub(super) async fn execute_single_turn(
        &self,
        prompt: &str,
        images: &[(String, String)],
        custom_notice: Option<&CustomWakeupMeta>,
        cancel_rx: &mut tokio::sync::watch::Receiver<bool>,
        steer_hook: &SteerHook,
    ) -> bool {
        self.refresh_credentials();
        self.sync_thinking_level_for_active_model();
        let started_ms = match custom_notice {
            Some(meta) => self.emit_and_persist_wakeup_message(&meta.custom_type, &meta.summary),
            None => self.emit_and_persist_user_message(prompt, images),
        };

        if custom_notice.is_none() && prompt.trim_start().starts_with('/') && images.is_empty() {
            if let Some(reply_text) = self.handle_slash_command(prompt.trim()).await {
                Self::send_line(&json!({ "type": "agent_start" }));
                self.emit_assistant_reply_with_thoughts(&reply_text, &[], started_ms, None);
                Self::send_line(&json!({ "type": "turn_end" }));
                Self::send_line(&json!({ "type": "agent_end" }));
                return false;
            }
        }

        let (provider_has_auth, active_provider, active_model, auto_retry_enabled) = {
            let m = self.mutable.lock().unwrap_or_else(|e| e.into_inner());
            let has_auth = m
                .providers
                .get(&m.active_provider)
                .map(|p| p.has_auth)
                .unwrap_or(false);
            (
                has_auth,
                m.active_provider.clone(),
                m.active_model.clone(),
                m.auto_retry_enabled,
            )
        };
        if !provider_has_auth && self.agent.api_key().trim().is_empty() {
            let msg = "API key belum diatur. Silakan buka **Settings › Providers & models** di OOPBoT untuk menempelkan API key (Google Gemini / OpenRouter / Custom), atau ketik langsung di sini:\n\n`/token <API_KEY>`";
            Self::send_line(&json!({ "type": "agent_start" }));
            self.emit_assistant_reply_with_thoughts(msg, &[], started_ms, None);
            Self::send_line(&json!({ "type": "turn_end" }));
            Self::send_line(&json!({ "type": "agent_end" }));
            return false;
        }

        let input = ChatInput::with_images(prompt.to_string(), images.to_vec());
        let max_attempts = if auto_retry_enabled { 3usize } else { 1usize };
        let mut attempt = 0usize;
        let mut retry_started = false;

        loop {
            attempt += 1;
            match self
                .agent
                .chat_turn_cancellable(&input, cancel_rx.clone(), Some(Arc::clone(steer_hook)))
                .await
            {
                Ok(outcome) => {
                    if retry_started {
                        Self::send_line(&json!({
                            "type": "auto_retry_end",
                            "success": true,
                            "attempt": attempt,
                        }));
                    }

                    let active_info = self.active_model_info();
                    let has_real_usage = outcome.usage.is_some();
                    let (turn_in, turn_out) = if let Some(u) = outcome.usage {
                        (u.input_tokens, u.output_tokens)
                    } else if outcome.aborted || outcome.text.trim().is_empty() {
                        (0, 0)
                    } else {
                        (
                            (prompt.len() as u64 / 4) + 60,
                            (outcome.text.len() as u64 / 4) + 60,
                        )
                    };
                    let turn_cost = if let Some(cost) = &active_info.cost {
                        (turn_in as f64 / 1_000_000.0) * cost.input
                            + (turn_out as f64 / 1_000_000.0) * cost.output
                    } else {
                        0.0
                    };
                    let last_ctx = outcome
                        .last_usage
                        .map(|lu| lu.input_tokens + lu.output_tokens)
                        .or_else(|| {
                            if !outcome.aborted && (turn_in + turn_out) > 0 {
                                Some(turn_in + turn_out)
                            } else {
                                None
                            }
                        });

                    if turn_in + turn_out > 0 || has_real_usage {
                        {
                            let mut state = self.ui_state.lock().unwrap_or_else(|e| e.into_inner());
                            state.token_totals.input += turn_in;
                            state.token_totals.output += turn_out;
                            state.total_tokens =
                                state.total_tokens.saturating_add(turn_in + turn_out);
                            state.total_cost += turn_cost;
                            if let Some(ctx) = last_ctx {
                                state.last_context_tokens = Some(ctx);
                                state.context_unknown = false;
                            }
                        }

                        let mut usage_entry = json!({
                            "kind": "turn",
                            "provider": active_provider,
                            "model": active_model,
                            "usage": {
                                "input": turn_in,
                                "output": turn_out,
                                "cacheRead": 0,
                                "cacheWrite": 0,
                                "totalTokens": turn_in + turn_out,
                                "cost": {
                                    "input": 0.0,
                                    "output": 0.0,
                                    "cacheRead": 0.0,
                                    "cacheWrite": 0.0,
                                    "total": turn_cost,
                                }
                            },
                        });
                        if let Some(ctx) = last_ctx {
                            usage_entry["lastContextTokens"] = json!(ctx);
                        }
                        self.append_session_entry("usage", usage_entry);
                    }

                    if outcome.aborted {
                        self.emit_and_persist_abort_message(started_ms, false);
                        return true;
                    }

                    self.emit_active_widgets();
                    return false;
                }
                Err(err) => {
                    if *cancel_rx.borrow() {
                        if retry_started {
                            Self::send_line(&json!({
                                "type": "auto_retry_end",
                                "success": false,
                                "attempt": attempt,
                                "finalError": "Dibatalkan oleh user",
                            }));
                        }
                        self.emit_and_persist_abort_message(started_ms, true);
                        return true;
                    }

                    let err_str = err.to_string();
                    if attempt < max_attempts && is_transient_llm_error(&err_str) {
                        let delay_ms = 500u64 * (1u64 << (attempt - 1));
                        retry_started = true;
                        let (rtx, mut rrx) = tokio::sync::watch::channel(false);
                        {
                            self.mutable
                                .lock()
                                .unwrap_or_else(|e| e.into_inner())
                                .retry_cancel_tx = Some(rtx);
                        }
                        Self::send_line(&json!({
                            "type": "auto_retry_start",
                            "attempt": attempt,
                            "maxAttempts": max_attempts,
                            "delayMs": delay_ms,
                            "errorMessage": err_str,
                        }));

                        enum RetryWaitOutcome {
                            SleepDone,
                            AbortedRun,
                            AbortedRetry,
                        }
                        let wait_out = tokio::select! {
                            biased;
                            _ = wait_watch_true(cancel_rx) => RetryWaitOutcome::AbortedRun,
                            _ = wait_watch_true(&mut rrx) => RetryWaitOutcome::AbortedRetry,
                            _ = tokio::time::sleep(std::time::Duration::from_millis(delay_ms)) => RetryWaitOutcome::SleepDone,
                        };
                        {
                            self.mutable
                                .lock()
                                .unwrap_or_else(|e| e.into_inner())
                                .retry_cancel_tx = None;
                        }
                        match wait_out {
                            RetryWaitOutcome::SleepDone => continue,
                            RetryWaitOutcome::AbortedRun => {
                                Self::send_line(&json!({
                                    "type": "auto_retry_end",
                                    "success": false,
                                    "attempt": attempt,
                                    "finalError": err_str,
                                }));
                                self.emit_and_persist_abort_message(started_ms, true);
                                return true;
                            }
                            RetryWaitOutcome::AbortedRetry => {
                                Self::send_line(&json!({
                                    "type": "auto_retry_end",
                                    "success": false,
                                    "attempt": attempt,
                                    "finalError": err_str,
                                }));
                            }
                        }
                    } else if retry_started {
                        Self::send_line(&json!({
                            "type": "auto_retry_end",
                            "success": false,
                            "attempt": attempt,
                            "finalError": err_str,
                        }));
                    }

                    let err_msg = json!({
                        "role": "assistant",
                        "content": [],
                        "stopReason": "error",
                        "errorMessage": err_str,
                        "timestamp": started_ms,
                        "durationMs": epoch_ms().saturating_sub(started_ms),
                    });
                    Self::send_line(&json!({ "type": "message_start", "message": err_msg }));
                    Self::send_line(&json!({ "type": "message_end", "message": err_msg }));
                    {
                        self.ensure_session_header();
                        let mut state = self.ui_state.lock().unwrap_or_else(|e| e.into_inner());
                        state.ui_messages.push(err_msg.clone());
                        state.append_session_entry("message", json!({ "message": err_msg }));
                    }
                    Self::send_line(&json!({ "type": "turn_end" }));
                    Self::send_line(&json!({ "type": "agent_end" }));
                    self.emit_active_widgets();
                    return false;
                }
            }
        }
    }

    pub(super) async fn run_prompt_coordinator(
        rt: Arc<RpcRuntime>,
        first: QueuedPrompt,
        mut cancel_rx: tokio::sync::watch::Receiver<bool>,
        op_seq: u64,
    ) {
        let mut finish_guard = CoordinatorFinishGuard {
            rt: Arc::clone(&rt),
            op_seq,
            armed: true,
        };
        rt.defer_settled.store(true, Ordering::SeqCst);

        let rt_hook = Arc::clone(&rt);
        let steer_hook: SteerHook = Arc::new(move || {
            let (drained, rem_s, rem_f) = {
                let mut m = rt_hook.mutable.lock().unwrap_or_else(|e| e.into_inner());
                let count = match m.steering_mode {
                    QueueMode::OneAtATime => 1.min(m.steering_queue.len()),
                    QueueMode::All => m.steering_queue.len(),
                };
                let mut out = Vec::with_capacity(count);
                for _ in 0..count {
                    if let Some(q) = m.steering_queue.pop_front() {
                        out.push(q);
                    }
                }
                let (rs, rf) = m.user_queue_snapshot();
                (out, rs, rf)
            };
            if !drained.is_empty() {
                Self::send_line(&json!({
                    "type": "queue_update",
                    "steering": rem_s,
                    "followUp": rem_f,
                }));
                for q in &drained {
                    rt_hook.emit_and_persist_queued_prompt(q);
                }
            }
            drained
                .into_iter()
                .map(|q| ChatInput::with_images(q.message, q.images))
                .collect()
        });

        let mut current_inputs = vec![first];
        let mut final_aborted = false;

        loop {
            if *cancel_rx.borrow() {
                final_aborted = true;
                break;
            }
            let Some(last_input) = current_inputs.pop() else {
                break;
            };
            for extra in &current_inputs {
                rt.emit_and_persist_queued_prompt(extra);
                rt.agent
                    .append_user_message_to_active_history(&ChatInput::with_images(
                        extra.message.clone(),
                        extra.images.clone(),
                    ));
            }
            let turn_aborted = rt
                .execute_single_turn(
                    &last_input.message,
                    &last_input.images,
                    last_input.custom_notice.as_ref(),
                    &mut cancel_rx,
                    &steer_hook,
                )
                .await;
            if turn_aborted || *cancel_rx.borrow() {
                final_aborted = true;
                break;
            }

            // Cek ambang auto-compaction bila diaktifkan & konteks >= 85% window
            let maybe_auto_compact = {
                let m = rt.mutable.lock().unwrap_or_else(|e| e.into_inner());
                if m.auto_compaction_enabled {
                    let window = Self::model_info_from_state(&m)
                        .input_token_limit
                        .unwrap_or(0);
                    let state = rt.ui_state.lock().unwrap_or_else(|e| e.into_inner());
                    if window >= 4096 && !state.context_unknown {
                        let threshold = window.saturating_sub(16_384).max((window * 85) / 100);
                        state
                            .last_context_tokens
                            .map(|toks| toks >= threshold)
                            .unwrap_or(false)
                    } else {
                        false
                    }
                } else {
                    false
                }
            };
            if maybe_auto_compact {
                let _ = rt
                    .perform_compaction(None, "threshold", &mut cancel_rx)
                    .await;
                if *cancel_rx.borrow() {
                    final_aborted = true;
                    break;
                }
            }

            let (next_batch, queue_update_opt) = {
                let mut m = rt.mutable.lock().unwrap_or_else(|e| e.into_inner());
                if !m.steering_queue.is_empty() {
                    let n = match m.steering_mode {
                        QueueMode::OneAtATime => 1,
                        QueueMode::All => m.steering_queue.len(),
                    };
                    let batch: Vec<_> =
                        (0..n).filter_map(|_| m.steering_queue.pop_front()).collect();
                    let snapshot = m.user_queue_snapshot();
                    (Some(batch), Some(snapshot))
                } else if !m.follow_up_queue.is_empty() {
                    let n = match m.follow_up_mode {
                        QueueMode::OneAtATime => 1,
                        QueueMode::All => m.follow_up_queue.len(),
                    };
                    let batch: Vec<_> = (0..n)
                        .filter_map(|_| m.follow_up_queue.pop_front())
                        .collect();
                    let snapshot = m.user_queue_snapshot();
                    (Some(batch), Some(snapshot))
                } else if !m.wakeup_queue.is_empty() {
                    let n = match m.follow_up_mode {
                        QueueMode::OneAtATime => 1,
                        QueueMode::All => m.wakeup_queue.len(),
                    };
                    let batch: Vec<_> = (0..n)
                        .filter_map(|_| m.wakeup_queue.pop_front())
                        .collect();
                    (Some(batch), None)
                } else {
                    (None, None)
                }
            };
            match next_batch {
                Some(batch) if !batch.is_empty() => {
                    if let Some((rem_s, rem_f)) = queue_update_opt {
                        Self::send_line(&json!({
                            "type": "queue_update",
                            "steering": rem_s,
                            "followUp": rem_f,
                        }));
                    }
                    current_inputs = batch;
                }
                _ => break,
            }
        }

        {
            let mut m = rt.mutable.lock().unwrap_or_else(|e| e.into_inner());
            if m.current_op_seq == op_seq {
                m.active_op = ActiveOp::Idle;
                m.retry_cancel_tx = None;
            }
        }
        rt.defer_settled.store(false, Ordering::SeqCst);
        Self::send_line(&json!({ "type": "agent_settled", "aborted": final_aborted }));
        finish_guard.armed = false;
        rt.publish_completed_seq(op_seq);
    }

    pub(super) fn emit_assistant_reply_with_thoughts(
        &self,
        text: &str,
        thoughts: &[String],
        started_ms: u64,
        usage: Option<(u64, u64)>,
    ) {
        let now_ms = epoch_ms();
        Self::send_line(&json!({
            "type": "message_start",
            "message": {
                "role": "assistant",
                "content": [],
                "timestamp": started_ms
            }
        }));

        let mut content_blocks = Vec::new();
        let mut content_idx = 0usize;

        for thought in thoughts {
            if thought.trim().is_empty() {
                continue;
            }
            Self::send_line(&json!({
                "type": "message_update",
                "assistantMessageEvent": {
                    "type": "thinking_start",
                    "contentIndex": content_idx
                }
            }));
            Self::send_line(&json!({
                "type": "message_update",
                "assistantMessageEvent": {
                    "type": "thinking_end",
                    "contentIndex": content_idx,
                    "content": thought
                }
            }));
            content_blocks.push(json!({
                "type": "thinking",
                "thinking": thought
            }));
            content_idx += 1;
        }

        Self::send_line(&json!({
            "type": "message_update",
            "assistantMessageEvent": {
                "type": "text_start",
                "contentIndex": content_idx
            }
        }));
        Self::send_line(&json!({
            "type": "message_update",
            "assistantMessageEvent": {
                "type": "text_delta",
                "contentIndex": content_idx,
                "delta": text
            }
        }));
        Self::send_line(&json!({
            "type": "message_update",
            "assistantMessageEvent": {
                "type": "text_end",
                "contentIndex": content_idx,
                "content": text
            }
        }));
        content_blocks.push(json!({ "type": "text", "text": text }));

        let mut msg = json!({
            "role": "assistant",
            "content": content_blocks,
            "stopReason": "stop",
            "timestamp": started_ms,
            "durationMs": now_ms.saturating_sub(started_ms),
        });
        if let Some((turn_in, turn_out)) = usage {
            msg["usage"] = json!({
                "input": turn_in,
                "output": turn_out,
                "totalTokens": turn_in + turn_out
            });
        }
        Self::send_line(&json!({ "type": "message_end", "message": msg }));
        {
            self.ensure_session_header();
            let mut state = self.ui_state.lock().unwrap_or_else(|e| e.into_inner());
            state.ui_messages.push(msg.clone());
            state.append_session_entry("message", json!({ "message": msg }));
        }
    }
}

pub(super) async fn wait_watch_true(rx: &mut tokio::sync::watch::Receiver<bool>) {
    if *rx.borrow() {
        return;
    }
    while rx.changed().await.is_ok() {
        if *rx.borrow() {
            return;
        }
    }
    std::future::pending::<()>().await;
}

pub(super) fn is_transient_llm_error(err: &str) -> bool {
    let low = err.to_ascii_lowercase();
    low.contains("429")
        || low.contains("500")
        || low.contains("502")
        || low.contains("503")
        || low.contains("504")
        || low.contains("529")
        || low.contains("overloaded")
        || low.contains("rate limit")
        || low.contains("too many requests")
        || low.contains("temporarily unavailable")
        || low.contains("connection reset")
        || low.contains("connection closed")
        || low.contains("broken pipe")
        || low.contains("timed out")
        || low.contains("timeout")
        || low.contains("error sending request")
        || low.contains("empty response")
}

pub(super) fn extract_prompt_images(images_val: &Value) -> Vec<(String, String)> {
    let Some(arr) = images_val.as_array() else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for item in arr {
        let data = item["data"].as_str().unwrap_or_default().trim();
        if data.is_empty() {
            continue;
        }
        let mime = item["mimeType"]
            .as_str()
            .filter(|s| !s.is_empty())
            .unwrap_or("image/jpeg");
        out.push((mime.to_string(), data.to_string()));
    }
    out
}
