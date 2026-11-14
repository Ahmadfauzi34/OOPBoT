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
            let m = self.mutable.lock().unwrap();
            let s = self.ui_state.lock().unwrap();
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

        let compaction_prompt = build_compaction_prompt(
            &cut_decision.to_summarize,
            rebuilt_before.previous_compaction_summary.as_deref(),
            custom_instructions,
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
        let (compact_id, rebuilt_after, estimated_after, usage_json) = {
            self.ensure_session_header();
            let mut s = self.ui_state.lock().unwrap();
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
            (cid, rebuilt, est_after, usage_obj)
        };

        self.agent.replace_active_history(
            &session_id,
            session_name.as_deref(),
            rebuilt_after.llm_history,
        );

        let first_kept_id = {
            let s = self.ui_state.lock().unwrap();
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
            let mut m = rt.mutable.lock().unwrap();
            let should_resume = std::mem::take(&mut m.resume_queues_after_compact);
            if !aborted_by_cancel && should_resume {
                let next_prompt = m
                    .steering_queue
                    .pop_front()
                    .or_else(|| m.follow_up_queue.pop_front());
                if let Some(first) = next_prompt {
                    let (cancel_tx, next_cancel_rx) = tokio::sync::watch::channel(false);
                    let seq = m.next_op_seq;
                    m.next_op_seq += 1;
                    m.current_op_seq = seq;
                    m.active_op = ActiveOp::Streaming { cancel_tx };
                    let rs: Vec<String> =
                        m.steering_queue.iter().map(|q| q.message.clone()).collect();
                    let rf: Vec<String> =
                        m.follow_up_queue.iter().map(|q| q.message.clone()).collect();
                    Some((first, next_cancel_rx, seq, rs, rf))
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
        let _ = rt.completed_seq_tx.send(op_seq);

        if let Some((first, next_cancel_rx, seq, rs, rf)) = resumed {
            Self::send_line(&json!({
                "type": "queue_update",
                "steering": rs,
                "followUp": rf,
            }));
            if resume_tx.send((first, next_cancel_rx, seq)).is_err() {
                let mut m = rt.mutable.lock().unwrap();
                if matches!(m.active_op, ActiveOp::Streaming { .. }) && m.current_op_seq == seq {
                    m.active_op = ActiveOp::Idle;
                }
                let _ = rt.completed_seq_tx.send(seq);
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

pub(super) fn is_valid_compaction_cut_entry(entry: &Value) -> bool {
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
            let kept_id = active_entries[cut_idx]["id"].as_str().map(str::to_string);
            return Some(CompactionCut {
                to_summarize: active_entries[..cut_idx].to_vec(),
                first_kept_entry_id: kept_id,
            });
        }
    }

    let latest_user_cut = (1..active_entries.len()).rev().find(|&idx| {
        let e = &active_entries[idx];
        e["type"].as_str() == Some("message")
            && e["message"]["role"].as_str() == Some("user")
    });
    if let Some(cut_idx) = latest_user_cut {
        let kept_id = active_entries[cut_idx]["id"].as_str().map(str::to_string);
        return Some(CompactionCut {
            to_summarize: active_entries[..cut_idx].to_vec(),
            first_kept_entry_id: kept_id,
        });
    }

    // Retain-none fallback ketika sesi hanya memiliki satu giliran awal
    Some(CompactionCut {
        to_summarize: active_entries.to_vec(),
        first_kept_entry_id: None,
    })
}

pub(super) fn format_entry_for_compaction(entry: &Value) -> Option<String> {
    match entry["type"].as_str().unwrap_or_default() {
        "message" => {
            let msg = &entry["message"];
            let role = msg["role"].as_str().unwrap_or_default();
            match role {
                "user" => Some(format!("[User]: {}", extract_user_message_text(msg))),
                "assistant" => {
                    let mut parts = Vec::new();
                    if let Some(arr) = msg["content"].as_array() {
                        for b in arr {
                            if let Some(t) = b["text"].as_str().filter(|s| !s.trim().is_empty()) {
                                parts.push(t.to_string());
                            } else if b["type"].as_str() == Some("toolCall") {
                                let name = b["name"].as_str().unwrap_or("");
                                let args = b
                                    .get("arguments")
                                    .or_else(|| b.get("args"))
                                    .map(|v| v.to_string())
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
                        extract_user_message_text(msg)
                    ))
                }
                _ => None,
            }
        }
        "custom_message" => Some(format!(
            "[Custom]: {}",
            extract_user_message_text(entry)
        )),
        "bashExecution" => Some(format!(
            "[Bash $ {}]: {}",
            entry["command"].as_str().unwrap_or_default(),
            entry["output"].as_str().unwrap_or_default()
        )),
        _ => None,
    }
}

pub(super) fn build_compaction_prompt(
    to_summarize: &[Value],
    previous_summary: Option<&str>,
    custom_instructions: Option<&str>,
) -> String {
    let mut prompt = String::from(
        "Summarize the conversation clearly and preserve key goals, decisions, file paths, and open tasks (Ringkas percakapan sebelumnya secara akurat dan padat).\n",
    );
    if let Some(prev) = previous_summary.filter(|s| !s.trim().is_empty()) {
        prompt.push_str("\n<previous_summary>\n");
        prompt.push_str(prev.trim());
        prompt.push_str("\n</previous_summary>\n");
    }
    prompt.push_str("\n<conversation>\n");
    for entry in to_summarize {
        if let Some(line) = format_entry_for_compaction(entry) {
            prompt.push_str(&line);
            prompt.push('\n');
        }
    }
    prompt.push_str("</conversation>\n");
    if let Some(inst) = custom_instructions.filter(|s| !s.trim().is_empty()) {
        prompt.push_str("\n<custom_instructions>\n");
        prompt.push_str(inst.trim());
        prompt.push_str("\n</custom_instructions>\n");
    }
    prompt
}

