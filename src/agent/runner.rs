//! Eksekusi giliran agen (`chat_turn_cancellable`), pemanggilan tool dengan pembatalan,
//! dan ringkasan konteks (`generate_compaction_summary_cancellable`).

use super::{
    AgentEvent, AgentHarness, ChatInput, MemoryEventCollector, SteerHook, StepsOutcome, ToolStatus,
    TurnOutcome,
};
use crate::common::BoxError;
use crate::llm::{
    function_response_part, user_message_with_images, user_text, FunctionCall, GenerationOptions,
    TokenUsage,
};
use crate::tools::ToolRegistry;
use serde_json::{json, Value};
use std::time::Instant;

impl AgentHarness {
    /// Buat ringkasan konteks (untuk `compact` RPC/CLI) dengan dukungan pembatalan asinkron.
    pub async fn generate_compaction_summary_cancellable(
        &self,
        prompt: &str,
        cancel_rx: tokio::sync::watch::Receiver<bool>,
    ) -> Result<(String, Option<TokenUsage>), BoxError> {
        self.generate_compaction_summary_with_images_cancellable(prompt, &[], cancel_rx)
            .await
    }

    /// Buat ringkasan konteks sambil meneruskan lampiran gambar `inlineData` terbaru
    /// ke model multimodal (dibatasi agar total teks + gambar tetap aman dalam `usable_limit`).
    pub async fn generate_compaction_summary_with_images_cancellable(
        &self,
        prompt: &str,
        inline_images: &[Value],
        mut cancel_rx: tokio::sync::watch::Receiver<bool>,
    ) -> Result<(String, Option<TokenUsage>), BoxError> {
        let (session_id, active_model) = {
            let tabs = self.tabs.lock().unwrap_or_else(|e| e.into_inner());
            let s = tabs.active();
            (s.id.clone(), s.model.clone())
        };
        let model_info = self.active_model_info();
        let policy = crate::context::ContextPolicy::for_model_limits(
            model_info.input_token_limit,
            model_info.output_token_limit,
        );

        let supports_images = model_info.input.iter().any(|m| m == "image");
        let mut attached_images: Vec<Value> = Vec::new();
        let mut kept_indices: Vec<usize> = Vec::new();
        let mut used_image_tokens = 0u64;
        if supports_images && !inline_images.is_empty() {
            let image_token_budget = policy.max_compaction_image_tokens();
            for (idx, img) in inline_images.iter().enumerate().rev() {
                let cost = crate::context::estimate_value_tokens(img);
                if used_image_tokens.saturating_add(cost) > image_token_budget {
                    continue;
                }
                used_image_tokens = used_image_tokens.saturating_add(cost);
                kept_indices.push(idx);
                attached_images.push(img.clone());
            }
            kept_indices.reverse();
            attached_images.reverse();
        }

        // Bila pemanggil langsung mengirim gambar lebih banyak dari anggaran model aktif (atau model text-only),
        // sinkronkan ulang penomoran `[gambar inline #N (terlampir pada input ringkasan)]` pada teks prompt.
        let mut synced_prompt = prompt.to_string();
        if attached_images.len() != inline_images.len() {
            for idx in 0..inline_images.len() {
                let orig = format!(
                    "[gambar inline #{} (terlampir pada input ringkasan)]",
                    idx + 1
                );
                let tmp = format!("\u{1f}REMAP_IMG_{}\u{1f}", idx);
                synced_prompt = synced_prompt.replace(&orig, &tmp);
            }
            for idx in 0..inline_images.len() {
                let tmp = format!("\u{1f}REMAP_IMG_{}\u{1f}", idx);
                if let Some(new_pos) = kept_indices.iter().position(|&k| k == idx) {
                    let updated = format!(
                        "[gambar inline #{} (terlampir pada input ringkasan)]",
                        new_pos + 1
                    );
                    synced_prompt = synced_prompt.replace(&tmp, &updated);
                } else {
                    synced_prompt = synced_prompt.replace(&tmp, "[gambar inline sebelumnya]");
                }
            }
        }

        let remaining_tokens = policy
            .usable_limit()
            .saturating_sub(used_image_tokens)
            .saturating_sub(64)
            .max(128);
        let prompt_cap = ((remaining_tokens as usize).saturating_mul(3))
            .clamp(512, policy.max_compaction_prompt_chars());
        let bounded_prompt =
            crate::context::truncate_head_tail_within_chars(&synced_prompt, prompt_cap);

        let gen_options = GenerationOptions {
            api: Some(model_info.api.clone()),
            max_tokens: model_info.output_token_limit,
            thinking_level: Some(self.thinking_level()),
            reasoning: model_info.reasoning,
            thinking_level_map: model_info.thinking_level_map.clone(),
            session_id: Some(session_id),
        };
        let mut parts = Vec::with_capacity(1 + attached_images.len());
        parts.push(json!({ "text": bounded_prompt }));
        parts.extend(attached_images);
        let contents = vec![json!({ "role": "user", "parts": parts })];
        let sys = "Kamu adalah asisten peringkas percakapan yang akurat dan ringkas. Bila ada gambar terlampir, rangkum detail visual pentingnya.";
        let turn = tokio::select! {
            biased;
            _ = wait_cancelled(&mut cancel_rx) => {
                return Err("Operasi dibatalkan (aborted)".into());
            }
            res = self.client.generate_with_options(
                &active_model,
                sys,
                &contents,
                &[],
                &gen_options,
            ) => res?,
        };
        Ok((turn.text, turn.usage))
    }

    /// Jalankan satu function call dan pancarkan event `ToolCallStart` + `ToolCallEnd`.
    /// Mengembalikan `None` jika dibatalkan saat menunggu konfirmasi atau eksekusi tool.
    async fn run_tool_cancellable(
        &self,
        step_index: usize,
        call: &FunctionCall,
        collector: &MemoryEventCollector,
        cancel_rx: &mut tokio::sync::watch::Receiver<bool>,
    ) -> Option<Value> {
        let call_id = call
            .id
            .clone()
            .unwrap_or_else(|| format!("step-{}-{}", step_index, call.name));

        let Some(tool) = self.tools.get(&call.name) else {
            let err_val = json!({ "error": format!("tool '{}' tidak dikenal", call.name) });
            // Tetap pancarkan ToolCallStart agar assistant(toolCall) dan toolResult selalu berpasangan di JSONL.
            self.emit(
                AgentEvent::ToolCallStart {
                    step_index,
                    call_id: call_id.clone(),
                    tool_name: call.name.clone(),
                    args: call.args.clone(),
                    preview: call.name.clone(),
                    requires_confirmation: false,
                    thought_signature: call.thought_signature.clone(),
                },
                collector,
            );
            self.emit(
                AgentEvent::ToolCallEnd {
                    step_index,
                    call_id,
                    tool_name: call.name.clone(),
                    status: ToolStatus::Error,
                    output: err_val.clone(),
                    duration_ms: 0,
                },
                collector,
            );
            return Some(err_val);
        };

        let preview = tool.preview(&call.args);
        let requires_confirmation = tool.requires_confirmation_for_args(&call.args);

        self.emit(
            AgentEvent::ToolCallStart {
                step_index,
                call_id: call_id.clone(),
                tool_name: tool.name().to_string(),
                args: call.args.clone(),
                preview: preview.clone(),
                requires_confirmation,
                thought_signature: call.thought_signature.clone(),
            },
            collector,
        );

        if requires_confirmation {
            let approved = tokio::select! {
                biased;
                _ = wait_cancelled(cancel_rx) => {
                    let abort_val = json!({ "error": "Dibatalkan oleh user (aborted)" });
                    self.emit(
                        AgentEvent::ToolCallEnd {
                            step_index,
                            call_id,
                            tool_name: tool.name().to_string(),
                            status: ToolStatus::Error,
                            output: abort_val,
                            duration_ms: 0,
                        },
                        collector,
                    );
                    return None;
                }
                res = self.approver.approve(tool.name(), &preview) => res,
            };
            if !approved {
                let rejected = json!({ "error": "User menolak eksekusi tool ini. Tanyakan alternatif ke user." });
                self.emit(
                    AgentEvent::ToolCallEnd {
                        step_index,
                        call_id,
                        tool_name: tool.name().to_string(),
                        status: ToolStatus::Rejected,
                        output: rejected.clone(),
                        duration_ms: 0,
                    },
                    collector,
                );
                return Some(rejected);
            }
        }

        let started = Instant::now();
        let exec_res = tokio::select! {
            biased;
            _ = wait_cancelled(cancel_rx) => {
                let abort_val = json!({ "error": "Dibatalkan oleh user (aborted)" });
                self.emit(
                    AgentEvent::ToolCallEnd {
                        step_index,
                        call_id,
                        tool_name: tool.name().to_string(),
                        status: ToolStatus::Error,
                        output: abort_val,
                        duration_ms: started.elapsed().as_millis() as u64,
                    },
                    collector,
                );
                return None;
            }
            res = tool.execute(&call.args) => res,
        };

        match exec_res {
            Ok(result) => {
                self.emit(
                    AgentEvent::ToolCallEnd {
                        step_index,
                        call_id,
                        tool_name: tool.name().to_string(),
                        status: ToolStatus::Done,
                        output: result.clone(),
                        duration_ms: started.elapsed().as_millis() as u64,
                    },
                    collector,
                );
                Some(strip_internal_tool_fields_for_llm(tool.name(), result))
            }
            Err(e) => {
                let err_val = json!({ "error": e });
                self.emit(
                    AgentEvent::ToolCallEnd {
                        step_index,
                        call_id,
                        tool_name: tool.name().to_string(),
                        status: ToolStatus::Error,
                        output: err_val.clone(),
                        duration_ms: started.elapsed().as_millis() as u64,
                    },
                    collector,
                );
                Some(err_val)
            }
        }
    }

    /// Jalankan satu giliran obrolan dengan dukungan pembatalan nyata (`watch::Receiver<bool>`)
    /// serta suntikan pesan `steer` di sela langkah tool (`step_idx > 1`).
    pub async fn chat_turn_cancellable(
        &self,
        input: &ChatInput,
        cancel_rx: tokio::sync::watch::Receiver<bool>,
        steer_hook: Option<SteerHook>,
    ) -> Result<TurnOutcome, BoxError> {
        self.run_turn_cancellable(input, true, cancel_rx, steer_hook)
            .await
    }

    /// Lanjutkan giliran yang gagal di tengah langkah tool (saat retry) tanpa menduplikasi
    /// pesan `user` awal yang sudah ter-commit bersama hasil tool & pesan `steer` ke `history`.
    pub async fn continue_turn_cancellable(
        &self,
        input: &ChatInput,
        cancel_rx: tokio::sync::watch::Receiver<bool>,
        steer_hook: Option<SteerHook>,
    ) -> Result<TurnOutcome, BoxError> {
        self.run_turn_cancellable(input, false, cancel_rx, steer_hook)
            .await
    }

    async fn run_turn_cancellable(
        &self,
        input: &ChatInput,
        push_initial_user: bool,
        mut cancel_rx: tokio::sync::watch::Receiver<bool>,
        steer_hook: Option<SteerHook>,
    ) -> Result<TurnOutcome, BoxError> {
        let decls = self.tools.declarations();
        let (
            session_id,
            active_model,
            mut history,
            mut last_observed_input,
            mut last_observed_baseline,
            obs_fingerprint,
        ) = {
            let tabs = self.tabs.lock().unwrap_or_else(|e| e.into_inner());
            let s = tabs.active();
            let fp = self.context_observation_fingerprint(&s.model, &decls);
            let (observed, baseline) = self.observed_context_pair_for(s, &decls);
            (
                s.id.clone(),
                s.model.clone(),
                s.history.clone(),
                observed,
                baseline,
                fp,
            )
        };

        if push_initial_user {
            let user_msg = if input.images.is_empty() {
                user_text(&input.text)
            } else {
                user_message_with_images(&input.text, &input.images)
            };
            history.push(user_msg);
        }

        let collector = MemoryEventCollector::new();
        let turn_started = Instant::now();
        let mut tool_steps = 0usize;
        let mut committed_progress = !push_initial_user;
        let mut total_usage: Option<TokenUsage> = None;
        let mut last_usage: Option<TokenUsage> = None;
        let mut was_aborted = false;

        let model_info = self.active_model_info();
        let gen_options = GenerationOptions {
            api: Some(model_info.api.clone()),
            max_tokens: model_info.output_token_limit,
            thinking_level: Some(self.thinking_level()),
            reasoning: model_info.reasoning,
            thinking_level_map: model_info.thinking_level_map.clone(),
            session_id: Some(session_id.clone()),
        };

        self.emit(
            AgentEvent::TurnStart {
                session_id,
                model: active_model.clone(),
            },
            &collector,
        );

        let turn_policy = self.context_policy();
        let context_mgr = self
            .auto_compact
            .then(|| crate::context::ContextManager::new(turn_policy.clone()));
        let mut last_auto_compact_tokens: Option<u64> = None;

        for step_idx in 1..=self.max_steps {
            if *cancel_rx.borrow() {
                was_aborted = true;
                break;
            }

            if step_idx > 1 {
                if let Some(hook) = &steer_hook {
                    for steer in hook() {
                        let steer_msg = if steer.images.is_empty() {
                            user_text(&steer.text)
                        } else {
                            user_message_with_images(&steer.text, &steer.images)
                        };
                        history.push(steer_msg);
                        committed_progress = true;
                    }
                }
                if *cancel_rx.borrow() {
                    was_aborted = true;
                    break;
                }
            }

            // Pre-turn & mid-turn auto-compaction guard (aktif pada jalur CLI).
            if let Some(mgr) = &context_mgr {
                let effective_observed = Self::calibrated_observed_tokens(
                    mgr.policy(),
                    &self.system_instruction,
                    &decls,
                    &history,
                    last_observed_input,
                    last_observed_baseline,
                );
                let pre_budget = mgr.evaluate(
                    &self.system_instruction,
                    &decls,
                    &history,
                    effective_observed,
                );
                // Cegah thrashing: bila compaction baru saja berjalan di giliran ini dan hasilnya
                // masih di zona `NeedsCompaction` (belum `OverLimit`), tunda compaction ulang sampai
                // konteks benar-benar tumbuh signifikan (>= 10% usable_limit) atau mencapai `OverLimit`.
                let skip_redundant_compact = pre_budget.status
                    == crate::context::BudgetStatus::NeedsCompaction
                    && last_auto_compact_tokens.is_some_and(|prev_tok| {
                        let min_growth = (mgr.policy().usable_limit() / 10).max(1);
                        pre_budget.effective_tokens() < prev_tok.saturating_add(min_growth)
                    });

                if !skip_redundant_compact {
                    let prepared = mgr
                        .prepare_or_compact(
                            &self.system_instruction,
                            &decls,
                            &history,
                            effective_observed,
                            false,
                            None,
                            self,
                            cancel_rx.clone(),
                        )
                        .await;
                    match prepared {
                        Ok(prep) => {
                            let mut post_budget = prep.budget;
                            if prep.compacted {
                                history = prep.history;
                                if history
                                    .last()
                                    .and_then(|m| m["role"].as_str())
                                    == Some("model")
                                {
                                    history.push(user_text(
                                        "Lanjutkan pengerjaan tugas saat ini berdasarkan ringkasan konteks di atas.",
                                    ));
                                    post_budget = mgr.evaluate(
                                        &self.system_instruction,
                                        &decls,
                                        &history,
                                        None,
                                    );
                                }
                                last_observed_input = None;
                                last_observed_baseline = None;
                                last_auto_compact_tokens = Some(post_budget.total_estimated_tokens);
                                self.emit(
                                    AgentEvent::Thinking {
                                        step_index: step_idx,
                                        text: format!(
                                            "🗜️ Auto-compaction konteks ({} pesan diringkas, {} dipertahankan; ~{} token)",
                                            prep.summarized_messages,
                                            prep.kept_messages,
                                            post_budget.total_estimated_tokens
                                        ),
                                        thought_signature: None,
                                    },
                                    &collector,
                                );
                            }
                            if post_budget.status == crate::context::BudgetStatus::OverLimit {
                                if committed_progress {
                                    let events = collector.take_events();
                                    let folded =
                                        MemoryEventCollector::fold_turn(&input.text, &events);
                                    let mut tabs =
                                        self.tabs.lock().unwrap_or_else(|e| e.into_inner());
                                    let active = tabs.active_mut();
                                    active.update_title_from_prompt(&input.text);
                                    active.history = history;
                                    active.set_observed_context_tokens_with_baseline(
                                        last_observed_input,
                                        last_observed_baseline,
                                        Some(obs_fingerprint.clone()),
                                    );
                                    active.timeline.extend(folded);
                                    self.persist_tabs(&tabs);
                                }
                                let stage = if prep.compacted {
                                    " setelah compaction"
                                } else {
                                    ""
                                };
                                return Err(format!(
                                    "Konteks masih melampaui batas model{} (~{} / {} token usable) sehingga request LLM dibatalkan. Gunakan /compact atau perpendek pesan/lampiran.",
                                    stage,
                                    post_budget.effective_tokens(),
                                    post_budget.usable_limit
                                )
                                .into());
                            }
                            if prep.compacted {
                                committed_progress = true;
                            }
                        }
                        Err(_) if *cancel_rx.borrow() => {
                            was_aborted = true;
                            break;
                        }
                        Err(e) => {
                            let current_budget = mgr.evaluate(
                                &self.system_instruction,
                                &decls,
                                &history,
                                effective_observed,
                            );
                            if current_budget.status == crate::context::BudgetStatus::OverLimit {
                                if committed_progress {
                                    let events = collector.take_events();
                                    let folded =
                                        MemoryEventCollector::fold_turn(&input.text, &events);
                                    let mut tabs =
                                        self.tabs.lock().unwrap_or_else(|e| e.into_inner());
                                    let active = tabs.active_mut();
                                    active.update_title_from_prompt(&input.text);
                                    active.history = history;
                                    active.set_observed_context_tokens_with_baseline(
                                        last_observed_input,
                                        last_observed_baseline,
                                        Some(obs_fingerprint.clone()),
                                    );
                                    active.timeline.extend(folded);
                                    self.persist_tabs(&tabs);
                                }
                                return Err(format!(
                                    "Auto-compaction gagal ({}) saat konteks melampaui batas model (~{} / {} token usable). Request LLM dibatalkan.",
                                    e,
                                    current_budget.effective_tokens(),
                                    current_budget.usable_limit
                                )
                                .into());
                            }
                            // Fail-soft: hanya lanjut bila konteks masih di bawah usable_limit
                            // (mis. status NeedsCompaction, belum OverLimit).
                            self.emit(
                                AgentEvent::Thinking {
                                    step_index: step_idx,
                                    text: format!("⚠️ Auto-compaction dilewati: {}", e),
                                    thought_signature: None,
                                },
                                &collector,
                            );
                        }
                    }
                }
            }

            let sent_estimated_tokens = crate::context::evaluate_budget(
                &turn_policy,
                &self.system_instruction,
                &decls,
                &history,
                None,
            )
            .total_estimated_tokens;
            let turn_res = tokio::select! {
                biased;
                _ = wait_cancelled(&mut cancel_rx) => {
                    was_aborted = true;
                    break;
                }
                res = self.client.generate_with_options(
                    &active_model,
                    &self.system_instruction,
                    &history,
                    &decls,
                    &gen_options,
                ) => res,
            };
            let turn = match turn_res {
                Ok(t) => t,
                Err(e) => {
                    // KONTRAK: bila langkah tool atau pesan `steer` sudah dieksekusi di giliran ini,
                    // simpan `history` parsial ke sesi aktif agar konteks tidak hilang saat retry/error.
                    if committed_progress {
                        let events = collector.take_events();
                        let folded = MemoryEventCollector::fold_turn(&input.text, &events);
                        let mut tabs = self.tabs.lock().unwrap_or_else(|e| e.into_inner());
                        let active = tabs.active_mut();
                        active.update_title_from_prompt(&input.text);
                        active.history = history;
                        active.set_observed_context_tokens_with_baseline(
                            last_observed_input,
                            last_observed_baseline,
                            Some(obs_fingerprint.clone()),
                        );
                        active.timeline.extend(folded);
                        self.persist_tabs(&tabs);
                    }
                    return Err(e);
                }
            };
            history.push(turn.raw_content.clone());

            if let Some(u) = turn.usage {
                let cur = total_usage.get_or_insert(TokenUsage::default());
                cur.input_tokens += u.input_tokens;
                cur.output_tokens += u.output_tokens;
                last_usage = Some(u);
                last_observed_input = Some(u.input_tokens);
                last_observed_baseline = Some(sent_estimated_tokens);
            }

            for (idx, thought) in turn.thoughts.iter().enumerate() {
                let thought_signature = turn
                    .thought_signatures
                    .get(idx)
                    .and_then(|s| s.clone());
                self.emit(
                    AgentEvent::Thinking {
                        step_index: step_idx,
                        text: thought.clone(),
                        thought_signature,
                    },
                    &collector,
                );
            }

            if turn.function_calls.is_empty() {
                self.emit(
                    AgentEvent::AssistantText {
                        text: turn.text.clone(),
                    },
                    &collector,
                );
                self.emit(
                    AgentEvent::TurnEnd {
                        steps_count: tool_steps,
                        duration_ms: turn_started.elapsed().as_millis() as u64,
                        outcome: StepsOutcome::Done,
                    },
                    &collector,
                );

                let events = collector.take_events();
                let folded = MemoryEventCollector::fold_turn(&input.text, &events);
                let mut tabs = self.tabs.lock().unwrap_or_else(|e| e.into_inner());
                let active = tabs.active_mut();
                active.update_title_from_prompt(&input.text);
                active.history = history;
                active.set_observed_context_tokens_with_baseline(
                    last_observed_input,
                    last_observed_baseline,
                    Some(obs_fingerprint.clone()),
                );
                active.timeline.extend(folded);
                self.persist_tabs(&tabs);

                return Ok(TurnOutcome {
                    text: turn.text,
                    events,
                    steps: tool_steps,
                    outcome: StepsOutcome::Done,
                    usage: total_usage,
                    last_usage,
                    aborted: false,
                });
            }

            if !turn.text.is_empty() {
                self.emit(
                    AgentEvent::Thinking {
                        step_index: step_idx,
                        text: turn.text.clone(),
                        thought_signature: None,
                    },
                    &collector,
                );
            }

            let mut parts = Vec::with_capacity(turn.function_calls.len());
            for (idx, call) in turn.function_calls.iter().enumerate() {
                tool_steps += 1;
                match self
                    .run_tool_cancellable(step_idx, call, &collector, &mut cancel_rx)
                    .await
                {
                    Some(response) => {
                        parts.push(function_response_part(call, response));
                    }
                    None => {
                        let cancel_err = json!({ "error": "Dibatalkan oleh user (aborted)" });
                        parts.push(function_response_part(call, cancel_err.clone()));
                        for unstarted in &turn.function_calls[idx + 1..] {
                            parts.push(function_response_part(unstarted, cancel_err.clone()));
                        }
                        was_aborted = true;
                        break;
                    }
                }
            }
            history.push(json!({ "role": "user", "parts": parts }));
            committed_progress = true;
            if was_aborted {
                break;
            }
        }

        self.emit(
            AgentEvent::TurnEnd {
                steps_count: tool_steps,
                duration_ms: turn_started.elapsed().as_millis() as u64,
                outcome: StepsOutcome::Stopped,
            },
            &collector,
        );
        let events = collector.take_events();
        let folded = MemoryEventCollector::fold_turn(&input.text, &events);
        let mut tabs = self.tabs.lock().unwrap_or_else(|e| e.into_inner());
        let active = tabs.active_mut();
        active.update_title_from_prompt(&input.text);
        active.history = history;
        active.set_observed_context_tokens_with_baseline(
            last_observed_input,
            last_observed_baseline,
            Some(obs_fingerprint.clone()),
        );
        active.timeline.extend(folded);
        self.persist_tabs(&tabs);

        Ok(TurnOutcome {
            text: String::new(),
            events,
            steps: tool_steps,
            outcome: StepsOutcome::Stopped,
            usage: total_usage,
            last_usage,
            aborted: was_aborted,
        })
    }
}

async fn wait_cancelled(cancel_rx: &mut tokio::sync::watch::Receiver<bool>) {
    if *cancel_rx.borrow() {
        return;
    }
    while cancel_rx.changed().await.is_ok() {
        if *cancel_rx.borrow() {
            return;
        }
    }
    std::future::pending::<()>().await;
}

pub(super) fn default_system_instruction(tools: &ToolRegistry) -> String {
    let cwd = std::env::current_dir()
        .map(|p| p.display().to_string())
        .unwrap_or_else(|_| "?".into());
    let prefix = std::env::var("PREFIX").unwrap_or_default();
    let is_termux = prefix.contains("com.termux");
    format!(
        "Kamu adalah OOPBoT, agen AI yang berjalan langsung di perangkat user.\n\
         Lingkungan: {env}, OS {os}/{arch}, direktori kerja: {cwd}{prefix_info}.\n\
         Tool yang tersedia: {tools}.\n\
         Aturan:\n\
         - Gunakan tool untuk memeriksa fakta (isi file, output perintah) daripada menebak.\n\
         - Untuk perubahan parsial pada file yang sudah ada, prioritaskan `edit_file` dibanding menulis ulang penuh dengan `write_file`.\n\
         - Untuk tugas multi-langkah, kerjakan bertahap dan verifikasi hasilnya.\n\
         - Jangan menjalankan perintah destruktif (rm -rf, format, dsb.) tanpa alasan jelas.\n\
         - Jawab dalam Bahasa Indonesia secara ringkas, gunakan format teks yang enak dibaca.",
        env = if is_termux {
            "Termux di Android"
        } else {
            "terminal Linux/Unix"
        },
        os = std::env::consts::OS,
        arch = std::env::consts::ARCH,
        cwd = cwd,
        prefix_info = if is_termux {
            format!(", $PREFIX={}", prefix)
        } else {
            String::new()
        },
        tools = tools.names().join(", "),
    )
}

pub(super) fn strip_internal_tool_fields_for_llm(tool_name: &str, mut result: Value) -> Value {
    if matches!(tool_name, "todo" | "edit_file") {
        if let Some(obj) = result.as_object_mut() {
            obj.remove("details");
        }
    }
    result
}
