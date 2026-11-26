use super::{
    active_branch_entries, extract_user_message_text, rebuild_context_from_entries, ActiveOp,
    CoordinatorFinishGuard, QueuedPrompt, RpcRuntime,
};
use serde_json::{json, Value};
use std::sync::Arc;

#[derive(Debug, Clone)]
pub(super) struct CompactionCut {
    pub(super) to_summarize: Vec<Value>,
    pub(super) first_kept_entry_id: Option<String>,
}


impl RpcRuntime {
    pub(super) async fn perform_compaction(
        &self,
        custom_instructions: Option<&str>,
        reason: &str,
        cancel_rx: &mut tokio::sync::watch::Receiver<bool>,
    ) -> Result<Value, (bool, String)> {
        self.refresh_credentials();

        let (
            all_entries,
            leaf_id,
            session_id,
            session_name,
            active_provider,
            active_model,
            tokens_before,
        ) = {
            let m = self.mutable.lock().unwrap_or_else(|e| e.into_inner());
            let s = self.ui_state.lock().unwrap_or_else(|e| e.into_inner());
            let ctx_toks = s
                .last_context_tokens
                .filter(|&t| t > 0)
                .unwrap_or_else(|| estimate_messages_tokens(&s.ui_messages).max(1));
            (
                s.entries.clone(),
                s.leaf_id.clone(),
                m.session_id.clone(),
                m.session_name.clone(),
                m.active_provider.clone(),
                m.active_model.clone(),
                ctx_toks,
            )
        };

        let path_entries = active_branch_entries(&all_entries, leaf_id.as_deref());
        let rebuilt_before = rebuild_context_from_entries(&path_entries);
        if rebuilt_before.active_entries.is_empty() && path_entries.is_empty() {
            return Err((false, "Tidak ada histori pesan untuk dikompak.".to_string()));
        }

        let Some(cut_decision) = select_compaction_cut_point(&rebuilt_before.active_entries) else {
            return Err((false, "Tidak cukup pesan untuk dikompak.".to_string()));
        };

        let active_info = self.active_model_info();
        let max_prompt_chars = crate::context::ContextPolicy::for_model_limits(
            active_info.input_token_limit,
            active_info.output_token_limit,
        )
        .max_compaction_prompt_chars();
        let compaction_prompt = build_compaction_prompt_with_limit(
            &cut_decision.to_summarize,
            rebuilt_before.previous_compaction_summary.as_deref(),
            custom_instructions,
            max_prompt_chars,
        );

        let emit_started = reason == "threshold";
        if emit_started {
            Self::send_line(&json!({
                "type": "compaction_start",
                "reason": "threshold",
            }));
        }

        let (summary, summary_usage) = match self
            .agent
            .generate_compaction_summary_cancellable(&compaction_prompt, cancel_rx.clone())
            .await
        {
            Ok(pair) => pair,
            Err(e) => {
                let is_aborted = *cancel_rx.borrow();
                if emit_started {
                    let mut end_ev = json!({
                        "type": "compaction_end",
                        "reason": "threshold",
                        "aborted": is_aborted,
                    });
                    if !is_aborted {
                        end_ev["errorMessage"] = json!(e.to_string());
                    }
                    Self::send_line(&end_ev);
                }
                return Err((is_aborted, e.to_string()));
            }
        };

        if *cancel_rx.borrow() {
            if emit_started {
                Self::send_line(&json!({
                    "type": "compaction_end",
                    "reason": "threshold",
                    "aborted": true,
                }));
            }
            return Err((true, "Compaction aborted".to_string()));
        }

        let active_info = self.active_model_info();
        let (compact_id, branch_after, rebuilt_after, estimated_after, usage_json) = {
            self.ensure_session_header();
            let mut s = self.ui_state.lock().unwrap_or_else(|e| e.into_inner());
            let cid = s.next_unique_entry_id();
            let first_kept = cut_decision
                .first_kept_entry_id
                .unwrap_or_else(|| cid.clone());

            let mut usage_obj = Value::Null;
            let mut cost_delta = 0.0f64;
            if let Some(u) = summary_usage {
                let c_in = active_info
                    .cost
                    .as_ref()
                    .map(|c| (u.input_tokens as f64 / 1_000_000.0) * c.input)
                    .unwrap_or(0.0);
                let c_out = active_info
                    .cost
                    .as_ref()
                    .map(|c| (u.output_tokens as f64 / 1_000_000.0) * c.output)
                    .unwrap_or(0.0);
                cost_delta = c_in + c_out;
                usage_obj = json!({
                    "input": u.input_tokens,
                    "output": u.output_tokens,
                    "cacheRead": 0,
                    "cacheWrite": 0,
                    "totalTokens": u.input_tokens + u.output_tokens,
                    "cost": {
                        "input": c_in,
                        "output": c_out,
                        "cacheRead": 0.0,
                        "cacheWrite": 0.0,
                        "total": cost_delta,
                    }
                });
                s.token_totals.input += u.input_tokens;
                s.token_totals.output += u.output_tokens;
                s.total_tokens = s
                    .total_tokens
                    .saturating_add(u.input_tokens + u.output_tokens);
                s.total_cost += cost_delta;
            }
            let _ = (active_provider, active_model, cost_delta);

            let mut payload = json!({
                "summary": summary,
                "firstKeptEntryId": first_kept,
                "tokensBefore": tokens_before,
                "details": {},
            });
            if !usage_obj.is_null() {
                payload["usage"] = usage_obj.clone();
            }
            let _ = s.append_session_entry_with_id(&cid, "compaction", payload);

            let branch = active_branch_entries(&s.entries, s.leaf_id.as_deref());
            let rebuilt = rebuild_context_from_entries(&branch);
            s.ui_messages = rebuilt.ui_messages.clone();
            s.last_context_tokens = None;
            s.context_unknown = true;
            let est_after = estimate_messages_tokens(&rebuilt.ui_messages).max(1);
            (cid, branch, rebuilt, est_after, usage_obj)
        };

        self.agent.replace_active_history(
            &session_id,
            session_name.as_deref(),
            rebuilt_after.llm_history,
        );
        self.plugins.replay_branch(&branch_after);
        self.emit_active_widgets();

        let first_kept_id = {
            let s = self.ui_state.lock().unwrap_or_else(|e| e.into_inner());
            s.entries
                .iter()
                .rev()
                .find(|e| e["id"] == compact_id && e["type"] == "compaction")
                .and_then(|e| e["firstKeptEntryId"].as_str())
                .unwrap_or(&compact_id)
                .to_string()
        };

        let mut result = json!({
            "summary": summary,
            "firstKeptEntryId": first_kept_id,
            "tokensBefore": tokens_before,
            "estimatedTokensAfter": estimated_after,
            "details": {},
        });
        if !usage_json.is_null() {
            result["usage"] = usage_json;
        }
        if emit_started {
            Self::send_line(&json!({
                "type": "compaction_end",
                "reason": "threshold",
                "result": result,
                "aborted": false,
            }));
        }
        Ok(result)
    }

    pub(super) async fn run_compact_task(
        rt: Arc<RpcRuntime>,
        cmd_id: String,
        custom_instructions: Option<String>,
        mut cancel_rx: tokio::sync::watch::Receiver<bool>,
        op_seq: u64,
        resume_tx: tokio::sync::mpsc::UnboundedSender<(
            QueuedPrompt,
            tokio::sync::watch::Receiver<bool>,
            u64,
        )>,
    ) {
        let mut finish_guard = CoordinatorFinishGuard {
            rt: Arc::clone(&rt),
            op_seq,
            armed: true,
        };

        let outcome = rt
            .perform_compaction(custom_instructions.as_deref(), "manual", &mut cancel_rx)
            .await;

        let (was_aborted, payload_res) = match outcome {
            Ok(result) => {
                Self::send_line(&json!({
                    "type": "compaction_end",
                    "reason": "manual",
                    "result": result,
                    "aborted": false,
                }));
                (false, Ok(result))
            }
            Err((aborted, err_msg)) => {
                let mut end_ev = json!({
                    "type": "compaction_end",
                    "reason": "manual",
                    "aborted": aborted,
                });
                if !aborted {
                    end_ev["errorMessage"] = json!(err_msg);
                }
                Self::send_line(&end_ev);
                (aborted, Err(err_msg))
            }
        };

        let aborted_by_cancel = was_aborted || *cancel_rx.borrow();
        let resumed = {
            let mut m = rt.mutable.lock().unwrap_or_else(|e| e.into_inner());
            let should_resume = std::mem::take(&mut m.resume_queues_after_compact);
            if !aborted_by_cancel && should_resume {
                let (next_prompt, from_user_queue) =
                    if let Some(p) = m.steering_queue.pop_front() {
                        (Some(p), true)
                    } else if let Some(p) = m.follow_up_queue.pop_front() {
                        (Some(p), true)
                    } else if let Some(p) = m.wakeup_queue.pop_front() {
                        (Some(p), false)
                    } else {
                        (None, false)
                    };
                if let Some(first) = next_prompt {
                    let (cancel_tx, next_cancel_rx) = tokio::sync::watch::channel(false);
                    let seq = m.next_op_seq;
                    m.next_op_seq += 1;
                    m.current_op_seq = seq;
                    m.active_op = ActiveOp::Streaming { cancel_tx };
                    let queue_update = from_user_queue.then(|| m.user_queue_snapshot());
                    Some((first, next_cancel_rx, seq, queue_update))
                } else {
                    m.active_op = ActiveOp::Idle;
                    None
                }
            } else {
                m.active_op = ActiveOp::Idle;
                None
            }
        };

        match payload_res {
            Ok(result) => Self::send_response(&cmd_id, "compact", result),
            Err(err_msg) => Self::send_error(&cmd_id, "compact", &err_msg),
        }
        finish_guard.armed = false;
        rt.publish_completed_seq(op_seq);

        if let Some((first, next_cancel_rx, seq, queue_update)) = resumed {
            if let Some((rs, rf)) = queue_update {
                Self::send_line(&json!({
                    "type": "queue_update",
                    "steering": rs,
                    "followUp": rf,
                }));
            }
            if resume_tx.send((first, next_cancel_rx, seq)).is_err() {
                let mut m = rt.mutable.lock().unwrap_or_else(|e| e.into_inner());
                if matches!(m.active_op, ActiveOp::Streaming { .. }) && m.current_op_seq == seq {
                    m.active_op = ActiveOp::Idle;
                }
                rt.publish_completed_seq(seq);
            }
        }
    }

}

pub(super) fn estimate_message_tokens(msg: &Value) -> u64 {
    let mut chars = 0usize;
    if let Some(s) = msg["summary"].as_str() {
        chars += s.len();
    }
    if let Some(s) = msg["command"].as_str() {
        chars += s.len();
    }
    if let Some(s) = msg["output"].as_str() {
        chars += s.len();
    }
    if let Some(s) = msg["content"].as_str() {
        chars += s.len();
    } else if let Some(arr) = msg["content"].as_array() {
        for block in arr {
            if let Some(t) = block["text"].as_str() {
                chars += t.len();
            } else if let Some(th) = block["thinking"].as_str() {
                chars += th.len();
            } else if block["type"].as_str() == Some("toolCall") {
                if let Some(args) = block.get("arguments").or_else(|| block.get("args")) {
                    chars += args.to_string().len();
                }
            } else if block["type"].as_str() == Some("image") {
                chars += 4000;
            }
        }
    }
    ((chars as u64) / 4).max(1)
}

pub(super) fn estimate_messages_tokens(messages: &[Value]) -> u64 {
    if messages.is_empty() {
        return 0;
    }
    messages.iter().map(estimate_message_tokens).sum::<u64>().max(1)
}

pub(super) fn estimate_entry_tokens(entry: &Value) -> u64 {
    match entry["type"].as_str().unwrap_or_default() {
        "message" => estimate_message_tokens(&entry["message"]),
        "custom_message" | "bashExecution" => estimate_message_tokens(entry),
        _ => 16,
    }
}

fn entry_non_empty_id(entry: &Value) -> Option<String> {
    entry["id"]
        .as_str()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
}

pub(super) fn is_valid_compaction_cut_entry(entry: &Value) -> bool {
    if entry_non_empty_id(entry).is_none() {
        return false;
    }
    match entry["type"].as_str().unwrap_or_default() {
        "message" => {
            let role = entry["message"]["role"].as_str().unwrap_or_default();
            role != "toolResult" && role != "system"
        }
        "custom_message" | "bashExecution" => true,
        _ => false,
    }
}

pub(super) fn select_compaction_cut_point(active_entries: &[Value]) -> Option<CompactionCut> {
    if active_entries.is_empty() {
        return None;
    }

    const KEEP_RECENT_TOKENS: u64 = 20_000;
    let total_tokens: u64 = active_entries.iter().map(estimate_entry_tokens).sum();

    if total_tokens >= KEEP_RECENT_TOKENS {
        let mut kept_acc = 0u64;
        let mut raw_idx = active_entries.len();
        for idx in (0..active_entries.len()).rev() {
            kept_acc += estimate_entry_tokens(&active_entries[idx]);
            raw_idx = idx;
            if kept_acc >= KEEP_RECENT_TOKENS {
                break;
            }
        }
        let mut chosen: Option<usize> = None;
        for idx in (1..=raw_idx).rev() {
            if is_valid_compaction_cut_entry(&active_entries[idx]) {
                chosen = Some(idx);
                break;
            }
        }
        if chosen.is_none() {
            for (idx, entry) in active_entries.iter().enumerate().skip(raw_idx.max(1)) {
                if is_valid_compaction_cut_entry(entry) {
                    chosen = Some(idx);
                    break;
                }
            }
        }
        if let Some(cut_idx) = chosen {
            if let Some(kept_id) = entry_non_empty_id(&active_entries[cut_idx]) {
                return Some(CompactionCut {
                    to_summarize: active_entries[..cut_idx].to_vec(),
                    first_kept_entry_id: Some(kept_id),
                });
            }
        }
    }

    let latest_user_cut = (1..active_entries.len()).rev().find(|&idx| {
        let e = &active_entries[idx];
        entry_non_empty_id(e).is_some()
            && e["type"].as_str() == Some("message")
            && e["message"]["role"].as_str() == Some("user")
    });
    let under_budget_cut = latest_user_cut.or_else(|| {
        (1..active_entries.len())
            .rev()
            .find(|&idx| is_valid_compaction_cut_entry(&active_entries[idx]))
    });
    if let Some(cut_idx) = under_budget_cut {
        if let Some(kept_id) = entry_non_empty_id(&active_entries[cut_idx]) {
            return Some(CompactionCut {
                to_summarize: active_entries[..cut_idx].to_vec(),
                first_kept_entry_id: Some(kept_id),
            });
        }
    }

    // Retain-none fallback ketika sesi tidak punya kandidat titik potong valid
    Some(CompactionCut {
        to_summarize: active_entries.to_vec(),
        first_kept_entry_id: None,
    })
}

#[cfg(test)]
use crate::context::assemble_compaction_prompt;
use crate::context::{
    assemble_compaction_prompt_with_limit, truncate_head_tail_chars, MAX_COMPACTION_MSG_CHARS,
    MAX_COMPACTION_TOOL_CHARS,
};

pub(super) fn format_entry_for_compaction(entry: &Value) -> Option<String> {
    match entry["type"].as_str().unwrap_or_default() {
        "message" => {
            let msg = &entry["message"];
            let role = msg["role"].as_str().unwrap_or_default();
            match role {
                "user" => Some(format!(
                    "[User]: {}",
                    truncate_head_tail_chars(
                        &extract_user_message_text(msg),
                        MAX_COMPACTION_MSG_CHARS
                    )
                )),
                "custom" => {
                    let body = msg
                        .get("details")
                        .and_then(|d| d["contextText"].as_str())
                        .filter(|s| !s.trim().is_empty())
                        .map(str::to_string)
                        .unwrap_or_else(|| extract_user_message_text(msg));
                    Some(format!(
                        "[Custom]: {}",
                        truncate_head_tail_chars(&body, MAX_COMPACTION_TOOL_CHARS)
                    ))
                }
                "assistant" => {
                    let mut parts = Vec::new();
                    if let Some(arr) = msg["content"].as_array() {
                        for b in arr {
                            if let Some(t) = b["text"].as_str().filter(|s| !s.trim().is_empty()) {
                                parts.push(truncate_head_tail_chars(t, MAX_COMPACTION_MSG_CHARS));
                            } else if b["type"].as_str() == Some("toolCall") {
                                let name = b["name"].as_str().unwrap_or("");
                                let args = b
                                    .get("arguments")
                                    .or_else(|| b.get("args"))
                                    .map(|v| {
                                        truncate_head_tail_chars(
                                            &v.to_string(),
                                            MAX_COMPACTION_TOOL_CHARS,
                                        )
                                    })
                                    .unwrap_or_default();
                                parts.push(format!("[ToolCall {}({})]", name, args));
                            }
                        }
                    }
                    Some(format!("[Assistant]: {}", parts.join("\n")))
                }
                "toolResult" => {
                    let name = msg["toolName"].as_str().unwrap_or("tool");
                    Some(format!(
                        "[ToolResult {}]: {}",
                        name,
                        truncate_head_tail_chars(
                            &extract_user_message_text(msg),
                            MAX_COMPACTION_TOOL_CHARS
                        )
                    ))
                }
                _ => None,
            }
        }
        "custom_message" => Some(format!(
            "[Custom]: {}",
            truncate_head_tail_chars(
                &extract_user_message_text(entry),
                MAX_COMPACTION_TOOL_CHARS
            )
        )),
        "bashExecution" => Some(format!(
            "[Bash $ {}]: {}",
            entry["command"].as_str().unwrap_or_default(),
            truncate_head_tail_chars(
                entry["output"].as_str().unwrap_or_default(),
                MAX_COMPACTION_TOOL_CHARS
            )
        )),
        _ => None,
    }
}

#[cfg(test)]
pub(super) fn build_compaction_prompt(
    to_summarize: &[Value],
    previous_summary: Option<&str>,
    custom_instructions: Option<&str>,
) -> String {
    let formatted: Vec<String> = to_summarize
        .iter()
        .filter_map(format_entry_for_compaction)
        .collect();
    assemble_compaction_prompt(&formatted, previous_summary, custom_instructions)
}

pub(super) fn build_compaction_prompt_with_limit(
    to_summarize: &[Value],
    previous_summary: Option<&str>,
    custom_instructions: Option<&str>,
    max_prompt_chars: usize,
) -> String {
    let formatted: Vec<String> = to_summarize
        .iter()
        .filter_map(format_entry_for_compaction)
        .collect();
    assemble_compaction_prompt_with_limit(
        &formatted,
        previous_summary,
        custom_instructions,
        max_prompt_chars,
    )
}

