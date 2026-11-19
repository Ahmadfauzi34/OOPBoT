//! Operasi manajemen sesi (`fork`, `clone`, `new_session`, `switch_session`,
//! `get_session_stats`, `export_html`, `apply_loaded_session`) dan penanganan `oopbot_channel`.

use super::{
    active_branch_entries, expand_home_or_resolve, extract_user_message_text,
    load_session_from_path, new_unique_session_id, render_session_html, unique_session_file_path,
    write_branched_session_file, ActiveOp, LoadedSession, RpcRuntime, RpcUiState,
};
use crate::plugins::BackgroundPlugin;
use serde_json::{json, Value};
use std::path::Path;

impl RpcRuntime {
    pub(super) fn apply_loaded_session(&self, loaded: LoadedSession) {
        let llm_history = loaded.llm_history.clone();
        let new_id = loaded.session_id.clone();
        let new_name = loaded.session_name.clone();
        let new_file = loaded.session_file.clone();
        let branch_entries = active_branch_entries(&loaded.entries, loaded.leaf_id.as_deref());

        {
            let mut m = self.mutable.lock().unwrap_or_else(|e| e.into_inner());
            m.session_file = new_file;
            m.session_id = new_id.clone();
            m.session_name = new_name.clone();
            m.steering_queue.clear();
            m.follow_up_queue.clear();
            m.wakeup_queue.clear();
            m.resume_queues_after_compact = false;
        }
        {
            let mut s = self.ui_state.lock().unwrap_or_else(|e| e.into_inner());
            *s = RpcUiState::from_loaded_session(loaded);
        }
        self.plugins.replay_branch(&branch_entries);
        self.agent
            .replace_active_history(&new_id, new_name.as_deref(), llm_history);
        self.emit_active_widgets();
    }

    pub(super) fn handle_session_stats_command(&self, id: &str) {
        let model_info = self.active_model_info();
        let (session_file, session_id) = {
            let m = self.mutable.lock().unwrap_or_else(|e| e.into_inner());
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
            let s = self.ui_state.lock().unwrap_or_else(|e| e.into_inner());
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
            let m = self.mutable.lock().unwrap_or_else(|e| e.into_inner());
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
            let m = self.mutable.lock().unwrap_or_else(|e| e.into_inner());
            let s = self.ui_state.lock().unwrap_or_else(|e| e.into_inner());
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

        let new_file =
            unique_session_file_path(&self.config_dir, &self.cwd, old_file.parent());
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
            let m = self.mutable.lock().unwrap_or_else(|e| e.into_inner());
            if !matches!(m.active_op, ActiveOp::Idle) {
                Self::send_error(id, "clone", "Tidak bisa clone saat agen sedang sibuk");
                return;
            }
        }
        let (old_file, path_entries) = {
            let m = self.mutable.lock().unwrap_or_else(|e| e.into_inner());
            let s = self.ui_state.lock().unwrap_or_else(|e| e.into_inner());
            (
                m.session_file.clone(),
                active_branch_entries(&s.entries, s.leaf_id.as_deref()),
            )
        };

        let new_file =
            unique_session_file_path(&self.config_dir, &self.cwd, old_file.parent());
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
            let m = self.mutable.lock().unwrap_or_else(|e| e.into_inner());
            if !matches!(m.active_op, ActiveOp::Idle) {
                Self::send_error(
                    id,
                    "new_session",
                    "Tidak bisa new_session saat agen sedang sibuk",
                );
                return;
            }
        }
        let cur_dir = self
            .mutable
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .session_file
            .parent()
            .map(Path::to_path_buf);
        let new_file =
            unique_session_file_path(&self.config_dir, &self.cwd, cur_dir.as_deref());
        let new_session_id = new_unique_session_id();
        let parent_path = parent_session
            .filter(|s| !s.trim().is_empty())
            .map(Path::new);
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
            let m = self.mutable.lock().unwrap_or_else(|e| e.into_inner());
            if !matches!(m.active_op, ActiveOp::Idle) {
                Self::send_error(
                    id,
                    "switch_session",
                    "Tidak bisa pindah sesi saat agen sedang sibuk",
                );
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
            let m = self.mutable.lock().unwrap_or_else(|e| e.into_inner());
            (
                m.session_file.clone(),
                m.session_id.clone(),
                m.session_name.clone(),
            )
        };
        let ui_messages = self
            .ui_state
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .ui_messages
            .clone();

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

    /// Tangani pesan event masuk dari Android UI atau ekstensi via OOPBoT Channel
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
                        if let Some(plugin) = self.plugins.get_plugin("oopbot-background") {
                            if let Some(bg) = plugin
                                .as_any()
                                .and_then(|a| a.downcast_ref::<BackgroundPlugin>())
                            {
                                bg.shutdown_all();
                            }
                        }
                        self.clear_all_background_wakeups();
                        Self::send_channel("background", json!({ "active": 0 }));
                    }
                }
            }
            "subagents" => {
                if let Some("stop" | "stopAll" | "check") = data["action"].as_str() {
                    Self::send_channel(
                        "subagents",
                        json!({
                            "runs": [],
                            "standingAside": false
                        }),
                    );
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
