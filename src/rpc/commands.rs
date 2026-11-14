use super::{
    active_branch_entries, expand_home_or_resolve, extract_user_message_text,
    load_session_from_path, new_unique_session_id, save_auth_key,
    save_model_override_to_config, save_scanned_models_to_config, save_settings_fields,
    unique_session_file_path, write_branched_session_file, ActiveOp, CoordinatorFinishGuard,
    LoadedSession, QueueMode, QueuedPrompt, RpcRuntime, RpcUiState,
};
use crate::agent::{ChatInput, SteerHook};
use crate::util::epoch_ms;
use serde_json::{json, Value};
use std::path::Path;
use std::sync::atomic::Ordering;
use std::sync::Arc;

impl RpcRuntime {
    pub(super) fn emit_queue_update(&self) {
        let (steering, follow_up) = {
            let m = self.mutable.lock().unwrap();
            let s: Vec<String> = m.steering_queue.iter().map(|q| q.message.clone()).collect();
            let f: Vec<String> = m
                .follow_up_queue
                .iter()
                .map(|q| q.message.clone())
                .collect();
            (s, f)
        };
        Self::send_line(&json!({
            "type": "queue_update",
            "steering": steering,
            "followUp": follow_up,
        }));
    }

    pub(super) fn emit_and_persist_user_message(&self, prompt: &str, images: &[(String, String)]) -> u64 {
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
            let mut state = self.ui_state.lock().unwrap();
            state.ui_messages.push(user_msg.clone());
            state.append_session_entry("message", json!({ "message": user_msg }));
        }
        started_ms
    }

    pub(super) async fn handle_slash_command(&self, cmd_line: &str) -> Option<String> {
        let parts: Vec<&str> = cmd_line.split_whitespace().collect();
        let cmd = parts.first().copied()?;
        match cmd {
            "/scan" => {
                self.refresh_credentials();
                match self.agent.scan_models().await {
                    Ok(scanned) => {
                        let count = scanned.len();
                        let preview: Vec<String> = scanned
                            .iter()
                            .take(15)
                            .map(|m| {
                                let think_badge = if m.reasoning { " 🧠" } else { "" };
                                format!("- `{}` ({}){}", m.id, m.label, think_badge)
                            })
                            .collect();
                        let (active_provider, cur_key) = {
                            let mut m = self.mutable.lock().unwrap();
                            m.catalog.replace_with_scanned(scanned.clone());
                            let key = m
                                .providers
                                .get(&m.active_provider)
                                .map(|p| p.api_key.clone())
                                .unwrap_or_default();
                            (m.active_provider.clone(), key)
                        };
                        self.sync_agent_catalog();
                        save_scanned_models_to_config(
                            &self.config_dir,
                            &active_provider,
                            &scanned,
                            &cur_key,
                        );
                        self.sync_agent_active_model();
                        self.refresh_credentials();
                        Some(format!(
                            "Berhasil men-scan **{} model** dari `{}` dan menyimpannya ke `models.json`:\n{}\n\nGunakan pemilih model di atas atau ketik `/model <ID>` untuk mengganti model.",
                            count,
                            self.agent.base_url(),
                            preview.join("\n")
                        ))
                    }
                    Err(e) => Some(format!("Gagal scan model: {}", e)),
                }
            }
            "/token" => {
                if parts.len() < 2 {
                    return Some(
                        "Format: `/token <API_KEY> [BASE_URL]`\nContoh:\n- `/token AIzaSy...`\n- `/token sk-or-v1-... https://openrouter.ai/api/v1`"
                            .to_string(),
                    );
                }
                let api_key = parts[1].trim();
                let base_url = parts.get(2).map(|s| s.trim().to_string());
                let active_provider = self.mutable.lock().unwrap().active_provider.clone();
                self.agent
                    .update_credentials(api_key.to_string(), base_url.clone());
                save_auth_key(
                    &self.config_dir,
                    &active_provider,
                    api_key,
                    base_url.as_deref(),
                );
                match self.agent.scan_models().await {
                    Ok(scanned) => {
                        let count = scanned.len();
                        let active_model = {
                            let mut m = self.mutable.lock().unwrap();
                            if !scanned.iter().any(|item| item.id == m.active_model) {
                                if let Some(first) = scanned.first() {
                                    m.active_model = first.id.clone();
                                }
                            }
                            m.catalog.replace_with_scanned(scanned.clone());
                            m.active_model.clone()
                        };
                        self.sync_agent_catalog();
                        self.sync_agent_active_model();
                        save_scanned_models_to_config(
                            &self.config_dir,
                            &active_provider,
                            &scanned,
                            api_key,
                        );
                        self.refresh_credentials();
                        save_settings_fields(
                            &self.config_dir,
                            Some(&active_provider),
                            Some(&active_model),
                            None,
                        );
                        Some(format!(
                            "Token berhasil disimpan & **{} model** terdeteksi otomatis! Model aktif sekarang: `{}` (`{}`).",
                            count, active_model, active_provider
                        ))
                    }
                    Err(e) => {
                        self.refresh_credentials();
                        Some(format!(
                            "Token disimpan, namun auto-scan `/models` gagal: {}",
                            e
                        ))
                    }
                }
            }
            "/models" => {
                self.refresh_credentials();
                let (active_provider, active_model) = {
                    let m = self.mutable.lock().unwrap();
                    (m.active_provider.clone(), m.active_model.clone())
                };
                let avail = self.available_models_json();
                let lines: Vec<String> = avail
                    .iter()
                    .enumerate()
                    .map(|(i, m)| {
                        let pid = m["provider"].as_str().unwrap_or("");
                        let mid = m["id"].as_str().unwrap_or("");
                        let name = m["name"].as_str().unwrap_or(mid);
                        let cw = m["contextWindow"].as_u64().unwrap_or(0) / 1000;
                        let think = if m["reasoning"].as_bool() == Some(true) {
                            " · 🧠 thinking"
                        } else {
                            ""
                        };
                        let mark = if pid == active_provider && mid == active_model {
                            " **(aktif)**"
                        } else {
                            ""
                        };
                        format!(
                            "{}. `{}/{}` — {} ({}k ctx{}){}",
                            i + 1,
                            pid,
                            mid,
                            name,
                            cw,
                            think,
                            mark
                        )
                    })
                    .collect();
                Some(format!(
                    "**Daftar Model Tersedia ({}):**\n{}",
                    avail.len(),
                    lines.join("\n")
                ))
            }
            "/model" => {
                if parts.len() < 2 {
                    let (active_provider, active_model, thinking_level, info) = {
                        let m = self.mutable.lock().unwrap();
                        (
                            m.active_provider.clone(),
                            m.active_model.clone(),
                            m.thinking_level.clone(),
                            Self::model_info_from_state(&m),
                        )
                    };
                    return Some(format!(
                        "Model aktif saat ini: `{}/{}` (**{}**)\n- API: `{}`\n- Context window: `{}` token\n- Max output: `{}` token\n- Thinking: `{}` (level aktif: `{}`)\n\nGunakan `/model <ID>` atau `/model <provider>/<ID>` untuk mengganti.",
                        active_provider,
                        active_model,
                        info.label,
                        info.api,
                        info.input_token_limit.unwrap_or(1_048_576),
                        info.output_token_limit.unwrap_or(65_536),
                        info.reasoning,
                        thinking_level
                    ));
                }
                let target = parts[1].trim();
                {
                    let mut m = self.mutable.lock().unwrap();
                    if let Some((prov, mid)) = target.split_once('/') {
                        if m.providers.contains_key(prov) {
                            m.active_provider = prov.to_string();
                            m.active_model = mid.to_string();
                        } else {
                            m.active_model = target.to_string();
                        }
                    } else if let Some(info) = m.catalog.find(target) {
                        m.active_model = info.id.clone();
                    } else {
                        let mut found = None;
                        for (pid, ep) in &m.providers {
                            if let Some(model) = ep
                                .models
                                .iter()
                                .find(|item| item.id.eq_ignore_ascii_case(target))
                            {
                                found = Some((pid.clone(), model.id.clone()));
                                break;
                            }
                        }
                        if let Some((pid, mid)) = found {
                            m.active_provider = pid;
                            m.active_model = mid;
                        } else {
                            m.catalog.register_custom(target, target);
                            m.active_model = target.to_string();
                        }
                    }
                }
                self.sync_agent_active_model();
                self.refresh_credentials();
                self.sync_thinking_level_for_active_model();
                let (active_provider, active_model, thinking_level) = {
                    let m = self.mutable.lock().unwrap();
                    (
                        m.active_provider.clone(),
                        m.active_model.clone(),
                        m.thinking_level.clone(),
                    )
                };
                save_settings_fields(
                    &self.config_dir,
                    Some(&active_provider),
                    Some(&active_model),
                    Some(&thinking_level),
                );
                Some(format!(
                    "Model aktif diganti ke `{}/{}` (thinking: `{}`).",
                    active_provider, active_model, thinking_level
                ))
            }
            "/thinking" => {
                let (info, active_model, thinking_level) = {
                    let m = self.mutable.lock().unwrap();
                    (
                        Self::model_info_from_state(&m),
                        m.active_model.clone(),
                        m.thinking_level.clone(),
                    )
                };
                let supported = info.available_thinking_levels();
                if parts.len() < 2 {
                    return Some(format!(
                        "Level thinking saat ini: `{}` (didukung oleh `{}`: `{}`).\nGunakan `/thinking <{}>` untuk mengubah.",
                        thinking_level,
                        active_model,
                        supported.join(", "),
                        supported.join("|")
                    ));
                }
                let requested = parts[1].trim().to_ascii_lowercase();
                let clamped = info.clamp_thinking_level(&requested);
                {
                    self.mutable.lock().unwrap().thinking_level = clamped.clone();
                }
                self.agent.set_thinking_level(&clamped);
                save_settings_fields(&self.config_dir, None, None, Some(&clamped));
                Self::send_line(&json!({
                    "type": "thinking_level_changed",
                    "level": clamped,
                }));
                Some(format!(
                    "Level thinking diatur ke `{}` untuk model `{}`.",
                    clamped, active_model
                ))
            }
            "/provider" => {
                if parts.len() < 2 {
                    let (providers, active_provider) = {
                        let m = self.mutable.lock().unwrap();
                        (m.providers.clone(), m.active_provider.clone())
                    };
                    let mut lines = Vec::new();
                    for (pid, ep) in &providers {
                        let mark = if *pid == active_provider {
                            " **(aktif)**"
                        } else {
                            ""
                        };
                        let auth_badge = if ep.has_auth { "✅ siap" } else { "⚪ belum ada key" };
                        lines.push(format!(
                            "- `{}` ({}) — {} · {} model{}",
                            pid,
                            ep.base_url,
                            auth_badge,
                            ep.models.len(),
                            mark
                        ));
                    }
                    lines.sort();
                    return Some(format!(
                        "**Daftar Provider:**\n{}\n\nGunakan `/provider <ID>` untuk berpindah provider.",
                        lines.join("\n")
                    ));
                }
                let target_pid = parts[1].trim();
                let ep_opt = self.mutable.lock().unwrap().providers.get(target_pid).cloned();
                if let Some(ep) = ep_opt {
                    {
                        let mut m = self.mutable.lock().unwrap();
                        m.active_provider = target_pid.to_string();
                        if let Some(first) = ep.models.first() {
                            m.active_model = first.id.clone();
                        }
                    }
                    self.refresh_credentials();
                    self.sync_agent_active_model();
                    self.sync_thinking_level_for_active_model();
                    let (active_provider, active_model, thinking_level) = {
                        let m = self.mutable.lock().unwrap();
                        (
                            m.active_provider.clone(),
                            m.active_model.clone(),
                            m.thinking_level.clone(),
                        )
                    };
                    save_settings_fields(
                        &self.config_dir,
                        Some(&active_provider),
                        Some(&active_model),
                        Some(&thinking_level),
                    );
                    Some(format!(
                        "Provider aktif diganti ke `{}` dengan model `{}`.",
                        active_provider, active_model
                    ))
                } else {
                    Some(format!("Provider `{}` tidak ditemukan.", target_pid))
                }
            }
            "/max-tokens" => {
                if parts.len() < 2 {
                    let cur = self
                        .active_model_info()
                        .output_token_limit
                        .unwrap_or(65_536);
                    let active_model = self.mutable.lock().unwrap().active_model.clone();
                    return Some(format!(
                        "Batas `maxTokens` model `{}` saat ini: `{}`.\nGunakan `/max-tokens <angka>` untuk mengubah dan menyimpan ke `models.json`.",
                        active_model, cur
                    ));
                }
                let Ok(max_tok) = parts[1].trim().parse::<u64>() else {
                    return Some("Nilai `/max-tokens` harus berupa angka bulat positif.".to_string());
                };
                if max_tok == 0 {
                    return Some("Nilai `/max-tokens` harus lebih besar dari 0.".to_string());
                }
                let (active_provider, active_model) = {
                    let m = self.mutable.lock().unwrap();
                    (m.active_provider.clone(), m.active_model.clone())
                };
                save_model_override_to_config(
                    &self.config_dir,
                    &active_provider,
                    &active_model,
                    &json!({ "maxTokens": max_tok }),
                );
                self.refresh_credentials();
                self.sync_agent_active_model();
                Some(format!(
                    "`maxTokens` untuk `{}/{}` berhasil disimpan ke `{}`.",
                    active_provider, active_model, max_tok
                ))
            }
            "/clear" => {
                {
                    let mut state = self.ui_state.lock().unwrap();
                    state.ui_messages.clear();
                }
                self.agent.clear_memory_silent();
                Some("Memori percakapan sesi ini telah dibersihkan.".to_string())
            }
            _ => None,
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
            let mut state = self.ui_state.lock().unwrap();
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
        cancel_rx: &mut tokio::sync::watch::Receiver<bool>,
        steer_hook: &SteerHook,
    ) -> bool {
        self.refresh_credentials();
        self.sync_thinking_level_for_active_model();
        let started_ms = self.emit_and_persist_user_message(prompt, images);

        if prompt.trim_start().starts_with('/') && images.is_empty() {
            if let Some(reply_text) = self.handle_slash_command(prompt.trim()).await {
                Self::send_line(&json!({ "type": "agent_start" }));
                self.emit_assistant_reply_with_thoughts(&reply_text, &[], started_ms, None);
                Self::send_line(&json!({ "type": "turn_end" }));
                Self::send_line(&json!({ "type": "agent_end" }));
                return false;
            }
        }

        let (provider_has_auth, active_provider, active_model, auto_retry_enabled) = {
            let m = self.mutable.lock().unwrap();
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
                            let mut state = self.ui_state.lock().unwrap();
                            state.token_totals.input += turn_in;
                            state.token_totals.output += turn_out;
                            state.total_tokens = state.total_tokens.saturating_add(turn_in + turn_out);
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
                            self.mutable.lock().unwrap().retry_cancel_tx = Some(rtx);
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
                            self.mutable.lock().unwrap().retry_cancel_tx = None;
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
                        let mut state = self.ui_state.lock().unwrap();
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
                let mut m = rt_hook.mutable.lock().unwrap();
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
                let rs: Vec<String> = m.steering_queue.iter().map(|q| q.message.clone()).collect();
                let rf: Vec<String> = m
                    .follow_up_queue
                    .iter()
                    .map(|q| q.message.clone())
                    .collect();
                (out, rs, rf)
            };
            if !drained.is_empty() {
                Self::send_line(&json!({
                    "type": "queue_update",
                    "steering": rem_s,
                    "followUp": rem_f,
                }));
                for q in &drained {
                    rt_hook.emit_and_persist_user_message(&q.message, &q.images);
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
            let last_input = current_inputs.pop().unwrap();
            for extra in &current_inputs {
                rt.emit_and_persist_user_message(&extra.message, &extra.images);
                rt.agent
                    .append_user_message_to_active_history(&ChatInput::with_images(
                        extra.message.clone(),
                        extra.images.clone(),
                    ));
            }
            let turn_aborted = rt
                .execute_single_turn(&last_input.message, &last_input.images, &mut cancel_rx, &steer_hook)
                .await;
            if turn_aborted || *cancel_rx.borrow() {
                final_aborted = true;
                break;
            }

            // Cek ambang auto-compaction bila diaktifkan & konteks >= 85% window
            let maybe_auto_compact = {
                let m = rt.mutable.lock().unwrap();
                if m.auto_compaction_enabled {
                    let window = Self::model_info_from_state(&m)
                        .input_token_limit
                        .unwrap_or(0);
                    let state = rt.ui_state.lock().unwrap();
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

            let (next_batch, rem_s, rem_f) = {
                let mut m = rt.mutable.lock().unwrap();
                if !m.steering_queue.is_empty() {
                    let n = match m.steering_mode {
                        QueueMode::OneAtATime => 1,
                        QueueMode::All => m.steering_queue.len(),
                    };
                    let batch: Vec<_> =
                        (0..n).filter_map(|_| m.steering_queue.pop_front()).collect();
                    let rs: Vec<String> =
                        m.steering_queue.iter().map(|q| q.message.clone()).collect();
                    let rf: Vec<String> =
                        m.follow_up_queue.iter().map(|q| q.message.clone()).collect();
                    (Some(batch), rs, rf)
                } else if !m.follow_up_queue.is_empty() {
                    let n = match m.follow_up_mode {
                        QueueMode::OneAtATime => 1,
                        QueueMode::All => m.follow_up_queue.len(),
                    };
                    let batch: Vec<_> = (0..n)
                        .filter_map(|_| m.follow_up_queue.pop_front())
                        .collect();
                    let rs: Vec<String> =
                        m.steering_queue.iter().map(|q| q.message.clone()).collect();
                    let rf: Vec<String> =
                        m.follow_up_queue.iter().map(|q| q.message.clone()).collect();
                    (Some(batch), rs, rf)
                } else {
                    (None, Vec::new(), Vec::new())
                }
            };
            match next_batch {
                Some(batch) if !batch.is_empty() => {
                    Self::send_line(&json!({
                        "type": "queue_update",
                        "steering": rem_s,
                        "followUp": rem_f,
                    }));
                    current_inputs = batch;
                }
                _ => break,
            }
        }

        {
            let mut m = rt.mutable.lock().unwrap();
            m.active_op = ActiveOp::Idle;
            m.retry_cancel_tx = None;
        }
        rt.defer_settled.store(false, Ordering::SeqCst);
        Self::send_line(&json!({ "type": "agent_settled", "aborted": final_aborted }));
        finish_guard.armed = false;
        let _ = rt.completed_seq_tx.send(op_seq);
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
            let mut state = self.ui_state.lock().unwrap();
            state.ui_messages.push(msg.clone());
            state.append_session_entry("message", json!({ "message": msg }));
        }
    }

    pub(super) fn apply_loaded_session(&self, loaded: LoadedSession) {
        let llm_history = loaded.llm_history.clone();
        let new_id = loaded.session_id.clone();
        let new_name = loaded.session_name.clone();
        let new_file = loaded.session_file.clone();

        {
            let mut m = self.mutable.lock().unwrap();
            m.session_file = new_file;
            m.session_id = new_id.clone();
            m.session_name = new_name.clone();
            m.steering_queue.clear();
            m.follow_up_queue.clear();
            m.resume_queues_after_compact = false;
        }
        {
            let mut s = self.ui_state.lock().unwrap();
            *s = RpcUiState::from_loaded_session(loaded);
        }
        self.agent
            .replace_active_history(&new_id, new_name.as_deref(), llm_history);
    }

    pub(super) fn handle_session_stats_command(&self, id: &str) {
        let model_info = self.active_model_info();
        let (session_file, session_id) = {
            let m = self.mutable.lock().unwrap();
            (
                m.session_file.to_string_lossy().to_string(),
                m.session_id.clone(),
            )
        };

        let (
            user_messages,
            assistant_messages,
            tool_calls,
            tool_results,
            total_messages,
            tokens,
            total_cost,
            context_usage,
        ) = {
            let s = self.ui_state.lock().unwrap();
            let mut user_cnt = 0u64;
            let mut asst_cnt = 0u64;
            let mut call_cnt = 0u64;
            let mut res_cnt = 0u64;
            for msg in &s.ui_messages {
                match msg["role"].as_str().unwrap_or_default() {
                    "user" => user_cnt += 1,
                    "assistant" => {
                        asst_cnt += 1;
                        if let Some(arr) = msg["content"].as_array() {
                            call_cnt += arr
                                .iter()
                                .filter(|b| b["type"].as_str() == Some("toolCall"))
                                .count() as u64;
                        }
                    }
                    "toolResult" => res_cnt += 1,
                    _ => {}
                }
            }
            let total_tok = s.token_totals.total().max(s.total_tokens);
            let ctx_usage = if let Some(window) = model_info.input_token_limit.filter(|&w| w > 0) {
                if s.context_unknown {
                    Some(json!({
                        "tokens": Value::Null,
                        "contextWindow": window,
                        "percent": Value::Null,
                    }))
                } else {
                    let ctx_tok = s.last_context_tokens.unwrap_or(total_tok);
                    let pct = (ctx_tok as f64 / window as f64) * 100.0;
                    Some(json!({
                        "tokens": ctx_tok,
                        "contextWindow": window,
                        "percent": pct,
                    }))
                }
            } else {
                None
            };

            (
                user_cnt,
                asst_cnt,
                call_cnt,
                res_cnt,
                s.ui_messages.len() as u64,
                json!({
                    "input": s.token_totals.input,
                    "output": s.token_totals.output,
                    "cacheRead": s.token_totals.cache_read,
                    "cacheWrite": s.token_totals.cache_write,
                    "total": total_tok,
                }),
                s.total_cost,
                ctx_usage,
            )
        };

        let mut resp = json!({
            "sessionFile": session_file,
            "sessionId": session_id,
            "userMessages": user_messages,
            "assistantMessages": assistant_messages,
            "toolCalls": tool_calls,
            "toolResults": tool_results,
            "totalMessages": total_messages,
            "tokens": tokens,
            "cost": total_cost,
        });
        if let Some(cu) = context_usage {
            resp["contextUsage"] = cu;
        }
        Self::send_response(id, "get_session_stats", resp);
    }

    pub(super) fn handle_fork_command(&self, id: &str, entry_id: &str) {
        {
            let m = self.mutable.lock().unwrap();
            if !matches!(m.active_op, ActiveOp::Idle) {
                Self::send_error(id, "fork", "Tidak bisa fork saat agen sedang sibuk");
                return;
            }
        }
        if entry_id.trim().is_empty() {
            Self::send_error(id, "fork", "entryId wajib diisi");
            return;
        }

        let (old_file, path_entries) = {
            let m = self.mutable.lock().unwrap();
            let s = self.ui_state.lock().unwrap();
            (
                m.session_file.clone(),
                active_branch_entries(&s.entries, s.leaf_id.as_deref()),
            )
        };

        let Some(pos) = path_entries
            .iter()
            .position(|e| e["id"].as_str() == Some(entry_id))
        else {
            Self::send_error(
                id,
                "fork",
                &format!("Entry '{}' tidak ditemukan pada branch aktif", entry_id),
            );
            return;
        };

        let target_entry = &path_entries[pos];
        if target_entry["type"].as_str() != Some("message")
            || target_entry["message"]["role"].as_str() != Some("user")
        {
            Self::send_error(id, "fork", "fork hanya mendukung entry pesan user");
            return;
        }
        let target_text = extract_user_message_text(&target_entry["message"]);

        let new_file = unique_session_file_path(
            &self.config_dir,
            &self.cwd,
            old_file.parent(),
        );
        let new_session_id = new_unique_session_id();
        if let Err(e) = write_branched_session_file(
            &new_file,
            &new_session_id,
            &self.cwd,
            Some(&old_file),
            &path_entries[..pos],
        ) {
            Self::send_error(id, "fork", &format!("Gagal membuat sesi fork: {}", e));
            return;
        }

        let loaded = load_session_from_path(&new_file);
        self.apply_loaded_session(loaded);
        Self::send_response(
            id,
            "fork",
            json!({ "text": target_text, "cancelled": false }),
        );
    }

    pub(super) fn handle_clone_command(&self, id: &str) {
        {
            let m = self.mutable.lock().unwrap();
            if !matches!(m.active_op, ActiveOp::Idle) {
                Self::send_error(id, "clone", "Tidak bisa clone saat agen sedang sibuk");
                return;
            }
        }
        let (old_file, path_entries) = {
            let m = self.mutable.lock().unwrap();
            let s = self.ui_state.lock().unwrap();
            (
                m.session_file.clone(),
                active_branch_entries(&s.entries, s.leaf_id.as_deref()),
            )
        };

        let new_file = unique_session_file_path(
            &self.config_dir,
            &self.cwd,
            old_file.parent(),
        );
        let new_session_id = new_unique_session_id();
        if let Err(e) = write_branched_session_file(
            &new_file,
            &new_session_id,
            &self.cwd,
            Some(&old_file),
            &path_entries,
        ) {
            Self::send_error(id, "clone", &format!("Gagal menyalin sesi: {}", e));
            return;
        }

        let loaded = load_session_from_path(&new_file);
        self.apply_loaded_session(loaded);
        Self::send_response(id, "clone", json!({ "cancelled": false }));
    }

    pub(super) fn handle_new_session_command(&self, id: &str, parent_session: Option<&str>) {
        {
            let m = self.mutable.lock().unwrap();
            if !matches!(m.active_op, ActiveOp::Idle) {
                Self::send_error(id, "new_session", "Tidak bisa new_session saat agen sedang sibuk");
                return;
            }
        }
        let cur_dir = self.mutable.lock().unwrap().session_file.parent().map(Path::to_path_buf);
        let new_file = unique_session_file_path(
            &self.config_dir,
            &self.cwd,
            cur_dir.as_deref(),
        );
        let new_session_id = new_unique_session_id();
        let parent_path = parent_session.filter(|s| !s.trim().is_empty()).map(Path::new);
        if let Err(e) = write_branched_session_file(
            &new_file,
            &new_session_id,
            &self.cwd,
            parent_path,
            &[],
        ) {
            Self::send_error(id, "new_session", &format!("Gagal membuat sesi baru: {}", e));
            return;
        }

        let loaded = load_session_from_path(&new_file);
        self.apply_loaded_session(loaded);
        Self::send_response(id, "new_session", json!({ "cancelled": false }));
    }

    pub(super) fn handle_switch_session_command(&self, id: &str, session_path: &str) {
        {
            let m = self.mutable.lock().unwrap();
            if !matches!(m.active_op, ActiveOp::Idle) {
                Self::send_error(id, "switch_session", "Tidak bisa pindah sesi saat agen sedang sibuk");
                return;
            }
        }
        let trimmed = session_path.trim();
        if trimmed.is_empty() {
            Self::send_error(id, "switch_session", "sessionPath wajib diisi");
            return;
        }
        let path = expand_home_or_resolve(trimmed, &self.cwd);
        if !path.exists() {
            Self::send_error(
                id,
                "switch_session",
                &format!("File sesi tidak ditemukan: {}", path.display()),
            );
            return;
        }
        let loaded = load_session_from_path(&path);
        self.apply_loaded_session(loaded);
        Self::send_response(id, "switch_session", json!({ "cancelled": false }));
    }

    pub(super) fn handle_export_html_command(&self, id: &str, output_path: Option<&str>) {
        let (session_file, session_id, session_name) = {
            let m = self.mutable.lock().unwrap();
            (
                m.session_file.clone(),
                m.session_id.clone(),
                m.session_name.clone(),
            )
        };
        let ui_messages = self.ui_state.lock().unwrap().ui_messages.clone();

        let target_path = match output_path.map(str::trim).filter(|s| !s.is_empty()) {
            Some(raw) => expand_home_or_resolve(raw, &self.cwd),
            None => {
                let stem = session_file
                    .file_stem()
                    .and_then(|s| s.to_str())
                    .filter(|s| !s.is_empty())
                    .unwrap_or(&session_id);
                self.cwd.join(format!("pi-session-{}.html", stem))
            }
        };

        if let Some(parent) = target_path.parent().filter(|p| !p.as_os_str().is_empty()) {
            if let Err(e) = std::fs::create_dir_all(parent) {
                Self::send_error(
                    id,
                    "export_html",
                    &format!("Gagal membuat direktori output HTML: {}", e),
                );
                return;
            }
        }

        let html = render_session_html(
            &session_id,
            session_name.as_deref(),
            &self.cwd,
            &ui_messages,
        );
        if let Err(e) = std::fs::write(&target_path, html) {
            Self::send_error(
                id,
                "export_html",
                &format!("Gagal menulis file HTML '{}': {}", target_path.display(), e),
            );
            return;
        }

        Self::send_response(
            id,
            "export_html",
            json!({ "path": target_path.to_string_lossy() }),
        );
    }


    /// Tangani pesan event masuk dari Android UI atau ekstensi via Pipkin Channel
    pub(super) fn handle_channel_message(&self, topic: &str, data: &Value) {
        match topic {
            "tool_approval" => {
                let req_id = data["request_id"]
                    .as_str()
                    .or_else(|| data["requestId"].as_str())
                    .unwrap_or_default();
                let approved = data["approved"].as_bool().unwrap_or(false);
                if !req_id.is_empty() {
                    self.resolve_tool_approval(req_id, approved);
                }
            }
            "background" => {
                if let Some(action) = data["action"].as_str() {
                    if action == "stop" {
                        Self::send_channel("background", json!({ "active": 0 }));
                    }
                }
            }
            "subagents" => {
                if let Some(action) = data["action"].as_str() {
                    match action {
                        "stop" | "stopAll" => {
                            Self::send_channel(
                                "subagents",
                                json!({
                                    "runs": [],
                                    "standingAside": false
                                }),
                            );
                        }
                        "check" => {
                            Self::send_channel(
                                "subagents",
                                json!({
                                    "runs": [],
                                    "standingAside": false
                                }),
                            );
                        }
                        _ => {}
                    }
                }
            }
            "remote" => {
                if let Some(action) = data["action"].as_str() {
                    if action == "check" {
                        Self::send_channel("remote", json!({ "connected": true }));
                    }
                }
            }
            _ => {}
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
        || low.contains("overloaded")
        || low.contains("rate limit")
        || low.contains("timed out")
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


pub(super) fn html_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for ch in s.chars() {
        match ch {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#39;"),
            _ => out.push(ch),
        }
    }
    out
}

pub(super) fn render_session_html(
    session_id: &str,
    session_name: Option<&str>,
    cwd: &Path,
    ui_messages: &[Value],
) -> String {
    let title = session_name
        .filter(|s| !s.trim().is_empty())
        .unwrap_or(session_id);
    let mut body = String::new();

    for msg in ui_messages {
        let role = msg["role"].as_str().unwrap_or("unknown");
        let (role_label, class_name, text_html) = match role {
            "user" => (
                "User",
                "user",
                format!(
                    "<pre>{}</pre>",
                    html_escape(&extract_user_message_text(msg))
                ),
            ),
            "assistant" => {
                let mut inner = String::new();
                if let Some(arr) = msg["content"].as_array() {
                    for b in arr {
                        match b["type"].as_str().unwrap_or_default() {
                            "thinking" => {
                                if let Some(th) = b["thinking"].as_str() {
                                    inner.push_str(&format!(
                                        "<details><summary>Thinking</summary><pre>{}</pre></details>",
                                        html_escape(th)
                                    ));
                                }
                            }
                            "text" => {
                                if let Some(t) = b["text"].as_str() {
                                    inner.push_str(&format!("<pre>{}</pre>", html_escape(t)));
                                }
                            }
                            "toolCall" => {
                                let name = b["name"].as_str().unwrap_or("tool");
                                let args = b
                                    .get("arguments")
                                    .or_else(|| b.get("args"))
                                    .map(|v| serde_json::to_string_pretty(v).unwrap_or_default())
                                    .unwrap_or_default();
                                inner.push_str(&format!(
                                    "<div class=\"tool\"><strong>Tool: {}</strong><pre>{}</pre></div>",
                                    html_escape(name),
                                    html_escape(&args)
                                ));
                            }
                            _ => {}
                        }
                    }
                }
                if inner.is_empty() {
                    if let Some(err) = msg["errorMessage"].as_str() {
                        inner = format!("<pre class=\"err\">{}</pre>", html_escape(err));
                    }
                }
                ("Assistant", "assistant", inner)
            }
            "toolResult" => {
                let name = msg["toolName"].as_str().unwrap_or("tool");
                (
                    "Tool Result",
                    "tool-result",
                    format!(
                        "<div class=\"tool\"><strong>{}</strong><pre>{}</pre></div>",
                        html_escape(name),
                        html_escape(&extract_user_message_text(msg))
                    ),
                )
            }
            "compactionSummary" => (
                "Compaction Summary",
                "summary",
                format!(
                    "<pre>{}</pre>",
                    html_escape(msg["summary"].as_str().unwrap_or_default())
                ),
            ),
            "branchSummary" => (
                "Branch Summary",
                "summary",
                format!(
                    "<pre>{}</pre>",
                    html_escape(msg["summary"].as_str().unwrap_or_default())
                ),
            ),
            "bashExecution" => (
                "Bash",
                "bash",
                format!(
                    "<pre>$ {}\n{}</pre>",
                    html_escape(msg["command"].as_str().unwrap_or_default()),
                    html_escape(msg["output"].as_str().unwrap_or_default())
                ),
            ),
            _ => (
                role,
                "other",
                format!(
                    "<pre>{}</pre>",
                    html_escape(&extract_user_message_text(msg))
                ),
            ),
        };
        body.push_str(&format!(
            "<section class=\"msg {}\"><h3>{}</h3>{}</section>\n",
            class_name,
            html_escape(role_label),
            text_html
        ));
    }

    format!(
        "<!doctype html>
<html lang=\"en\">
<head>
<meta charset=\"utf-8\">
<meta name=\"viewport\" content=\"width=device-width,initial-scale=1\">
<title>{title}</title>
<style>
body {{ font-family: system-ui, sans-serif; max-width: 860px; margin: 2rem auto; padding: 0 1rem; background: #0f1115; color: #e6edf3; }}
header {{ border-bottom: 1px solid #30363d; margin-bottom: 1.5rem; padding-bottom: 1rem; }}
.msg {{ border: 1px solid #30363d; border-radius: 8px; padding: 0.75rem 1rem; margin-bottom: 1rem; background: #161b22; }}
.msg.user {{ border-left: 4px solid #58a6ff; }}
.msg.assistant {{ border-left: 4px solid #3fb950; }}
.msg.summary {{ border-left: 4px solid #d29922; }}
pre {{ white-space: pre-wrap; word-break: break-word; font-family: ui-monospace, monospace; margin: 0.25rem 0; }}
.tool {{ background: #0d1117; padding: 0.5rem; border-radius: 6px; margin: 0.5rem 0; }}
</style>
</head>
<body>
<header>
<h1>{title}</h1>
<p>Session ID: <code>{sid}</code> · CWD: <code>{cwd}</code></p>
</header>
{body}
</body>
</html>",
        title = html_escape(title),
        sid = html_escape(session_id),
        cwd = html_escape(&cwd.to_string_lossy()),
        body = body,
    )
}

