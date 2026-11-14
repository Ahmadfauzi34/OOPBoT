use super::{reverse_map_tool_name, RpcRuntime, SESSION_UNIQUE_SEQ};
use crate::gemini::{user_message_with_images, user_text};
use crate::util::{epoch_ms, iso_now, parse_iso_to_epoch_ms};
use serde_json::{json, Value};
use std::collections::{HashMap, HashSet};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::Ordering;

impl RpcRuntime {
    pub(super) fn ensure_session_header(&self) {
        let (session_file, session_id, parent_session) = {
            let m = self.mutable.lock().unwrap();
            let ps = self.ui_state.lock().unwrap().parent_session.clone();
            (m.session_file.clone(), m.session_id.clone(), ps)
        };
        if session_file.exists() {
            return;
        }
        if let Some(parent) = session_file.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        let mut header = json!({
            "type": "session",
            "version": 3,
            "id": session_id,
            "timestamp": iso_now(),
            "cwd": self.cwd.to_string_lossy(),
        });
        if let Some(ps) = parent_session {
            header["parentSession"] = json!(ps);
        }
        if let Ok(line) = serde_json::to_string(&header) {
            let _ = std::fs::write(&session_file, format!("{}\n", line));
        }
    }

}

#[derive(Debug, Clone, Copy, Default)]
pub struct TokenTotals {
    pub input: u64,
    pub output: u64,
    pub cache_read: u64,
    pub cache_write: u64,
}

impl TokenTotals {
    pub fn total(self) -> u64 {
        self.input + self.output + self.cache_read + self.cache_write
    }
}

/// Status UI RPC yang dibagikan antara RpcServer dan RpcEventSink.
#[derive(Debug, Clone, Default)]
pub struct RpcUiState {
    pub ui_messages: Vec<Value>,
    pub session_file: Option<PathBuf>,
    pub entry_seq: usize,
    pub total_tokens: u64,
    pub total_cost: f64,
    pub token_totals: TokenTotals,
    pub last_context_tokens: Option<u64>,
    pub context_unknown: bool,
    pub parent_session: Option<String>,
    pub entries: Vec<Value>,
    pub leaf_id: Option<String>,
}

impl RpcUiState {
    pub fn new(session_file: Option<PathBuf>, entry_seq: usize) -> Self {
        Self {
            ui_messages: Vec::new(),
            session_file,
            entry_seq,
            total_tokens: 0,
            total_cost: 0.0,
            token_totals: TokenTotals::default(),
            last_context_tokens: None,
            context_unknown: false,
            parent_session: None,
            entries: Vec::new(),
            leaf_id: None,
        }
    }

    pub(super) fn from_loaded_session(loaded: LoadedSession) -> Self {
        Self {
            ui_messages: loaded.ui_messages,
            session_file: Some(loaded.session_file),
            entry_seq: loaded.entry_seq,
            total_tokens: loaded.total_tokens,
            total_cost: loaded.total_cost,
            token_totals: loaded.token_totals,
            last_context_tokens: loaded.last_context_tokens,
            context_unknown: loaded.context_unknown,
            parent_session: loaded.parent_session,
            entries: loaded.entries,
            leaf_id: loaded.leaf_id,
        }
    }

    pub fn next_unique_entry_id(&mut self) -> String {
        loop {
            let candidate = format!("e{}", self.entry_seq);
            self.entry_seq += 1;
            if !self
                .entries
                .iter()
                .any(|e| e["id"].as_str() == Some(candidate.as_str()))
            {
                return candidate;
            }
        }
    }

    pub fn append_session_entry_with_id(
        &mut self,
        entry_id: &str,
        entry_type: &str,
        mut payload: Value,
    ) -> String {
        let parent_id = self
            .leaf_id
            .as_ref()
            .map(|s| json!(s))
            .unwrap_or(Value::Null);

        payload["type"] = json!(entry_type);
        payload["id"] = json!(entry_id);
        payload["parentId"] = parent_id;
        payload["timestamp"] = json!(iso_now());
        self.entries.push(payload.clone());
        self.leaf_id = Some(entry_id.to_string());

        if let Some(path) = &self.session_file {
            if let Ok(line) = serde_json::to_string(&payload) {
                if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
                    let _ = std::fs::create_dir_all(parent);
                }
                if let Ok(mut file) = std::fs::OpenOptions::new()
                    .create(true)
                    .append(true)
                    .open(path)
                {
                    let _ = writeln!(file, "{}", line);
                }
            }
        }
        entry_id.to_string()
    }

    pub fn append_session_entry_returning_id(
        &mut self,
        entry_type: &str,
        payload: Value,
    ) -> String {
        let id = self.next_unique_entry_id();
        self.append_session_entry_with_id(&id, entry_type, payload)
    }

    pub fn append_session_entry(&mut self, entry_type: &str, payload: Value) {
        let _ = self.append_session_entry_returning_id(entry_type, payload);
    }
}


#[derive(Debug, Clone)]
pub(super) struct LoadedSession {
    pub(super) session_file: PathBuf,
    pub(super) session_id: String,
    pub(super) session_name: Option<String>,
    pub(super) parent_session: Option<String>,
    pub(super) entries: Vec<Value>,
    pub(super) leaf_id: Option<String>,
    pub(super) entry_seq: usize,
    pub(super) ui_messages: Vec<Value>,
    pub(super) llm_history: Vec<Value>,
    pub(super) token_totals: TokenTotals,
    pub(super) total_tokens: u64,
    pub(super) total_cost: f64,
    pub(super) last_context_tokens: Option<u64>,
    pub(super) context_unknown: bool,
}

#[derive(Debug, Clone)]
pub(super) struct RebuiltContext {
    pub(super) ui_messages: Vec<Value>,
    pub(super) llm_history: Vec<Value>,
    pub(super) active_entries: Vec<Value>,
    pub(super) previous_compaction_summary: Option<String>,
    pub(super) token_totals: TokenTotals,
    pub(super) total_tokens: u64,
    pub(super) total_cost: f64,
    pub(super) last_context_tokens: Option<u64>,
    pub(super) context_unknown: bool,
}


pub(super) fn expand_home_or_resolve(raw: &str, cwd: &Path) -> PathBuf {
    let trimmed = raw.trim();
    if trimmed == "~" || trimmed.starts_with("~/") {
        if let Ok(home) = std::env::var("HOME") {
            let suffix = trimmed.strip_prefix("~/").unwrap_or("");
            if suffix.is_empty() {
                return PathBuf::from(home);
            }
            return PathBuf::from(home).join(suffix);
        }
    }
    let p = PathBuf::from(trimmed);
    if p.is_absolute() {
        p
    } else {
        cwd.join(p)
    }
}

pub(super) fn default_sessions_dir(config_dir: &Path, cwd: &Path) -> PathBuf {
    let slug = format!(
        "--{}--",
        cwd.to_string_lossy()
            .trim_start_matches('/')
            .replace(['/', '\\', ':'], "-")
    );
    config_dir.join("sessions").join(slug)
}

pub(super) fn new_unique_session_id() -> String {
    let seq = SESSION_UNIQUE_SEQ.fetch_add(1, Ordering::Relaxed);
    format!("oopbot-{}-{}", epoch_ms(), seq)
}

pub(super) fn unique_session_file_path(
    config_dir: &Path,
    cwd: &Path,
    preferred_dir: Option<&Path>,
) -> PathBuf {
    let session_dir = preferred_dir
        .filter(|p| !p.as_os_str().is_empty())
        .map(Path::to_path_buf)
        .unwrap_or_else(|| default_sessions_dir(config_dir, cwd));
    let _ = std::fs::create_dir_all(&session_dir);
    let sid = new_unique_session_id();
    let file_ts = iso_now().replace([':', '.'], "-");
    session_dir.join(format!("{}_{}.jsonl", file_ts, sid))
}

pub(super) fn write_branched_session_file(
    path: &Path,
    session_id: &str,
    cwd: &Path,
    parent_session: Option<&Path>,
    entries: &[Value],
) -> io::Result<()> {
    if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
        std::fs::create_dir_all(parent)?;
    }
    let mut header = json!({
        "type": "session",
        "version": 3,
        "id": session_id,
        "timestamp": iso_now(),
        "cwd": cwd.to_string_lossy(),
    });
    if let Some(ps) = parent_session {
        header["parentSession"] = json!(ps.to_string_lossy());
    }

    let mut lines = vec![serde_json::to_string(&header)?];
    let mut prev_id: Option<String> = None;
    for (idx, raw_entry) in entries.iter().enumerate() {
        let mut entry = raw_entry.clone();
        let eid = entry["id"]
            .as_str()
            .filter(|s| !s.is_empty())
            .map(str::to_string)
            .unwrap_or_else(|| format!("e{}", idx));
        entry["id"] = json!(eid.clone());
        entry["parentId"] = prev_id.as_ref().map(|p| json!(p)).unwrap_or(Value::Null);
        prev_id = Some(eid);
        lines.push(serde_json::to_string(&entry)?);
    }

    std::fs::write(path, format!("{}\n", lines.join("\n")))
}

pub(super) fn load_session_from_path(path: &Path) -> LoadedSession {
    let mut session_id = new_unique_session_id();
    let mut parent_session: Option<String> = None;
    let mut entries: Vec<Value> = Vec::new();
    let mut leaf_id: Option<String> = None;
    let mut entry_seq: usize = 0;

    if let Ok(content) = std::fs::read_to_string(path) {
        for line in content.lines() {
            let trimmed = line.trim();
            if trimmed.is_empty() {
                continue;
            }
            let Ok(entry) = serde_json::from_str::<Value>(trimmed) else {
                continue;
            };
            if entry["type"].as_str() == Some("session") {
                if let Some(id) = entry["id"].as_str().filter(|s| !s.is_empty()) {
                    session_id = id.to_string();
                }
                if let Some(ps) = entry["parentSession"]
                    .as_str()
                    .filter(|s| !s.trim().is_empty())
                {
                    parent_session = Some(ps.to_string());
                }
                continue;
            }
            if let Some(eid) = entry["id"].as_str().filter(|s| !s.is_empty()) {
                leaf_id = Some(eid.to_string());
                if let Some(num) = eid.strip_prefix('e').and_then(|s| s.parse::<usize>().ok()) {
                    entry_seq = entry_seq.max(num + 1);
                }
            }
            entries.push(entry);
        }
    }
    entry_seq = entry_seq.max(entries.len());

    let path_entries = active_branch_entries(&entries, leaf_id.as_deref());
    let mut session_name: Option<String> = None;
    for e in &path_entries {
        if e["type"].as_str() == Some("session_info") {
            session_name = e["name"]
                .as_str()
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty());
        }
    }

    let rebuilt = rebuild_context_from_entries(&path_entries);
    LoadedSession {
        session_file: path.to_path_buf(),
        session_id,
        session_name,
        parent_session,
        entries,
        leaf_id,
        entry_seq,
        ui_messages: rebuilt.ui_messages,
        llm_history: rebuilt.llm_history,
        token_totals: rebuilt.token_totals,
        total_tokens: rebuilt.total_tokens,
        total_cost: rebuilt.total_cost,
        last_context_tokens: rebuilt.last_context_tokens,
        context_unknown: rebuilt.context_unknown,
    }
}

pub(super) fn init_or_load_session(
    config_dir: &Path,
    cwd: &Path,
    session_path: Option<&str>,
) -> LoadedSession {
    if let Some(path_str) = session_path.filter(|s| !s.trim().is_empty()) {
        let path = expand_home_or_resolve(path_str, cwd);
        if path.exists() {
            return load_session_from_path(&path);
        }
        return LoadedSession {
            session_file: path,
            session_id: new_unique_session_id(),
            session_name: None,
            parent_session: None,
            entries: Vec::new(),
            leaf_id: None,
            entry_seq: 0,
            ui_messages: Vec::new(),
            llm_history: Vec::new(),
            token_totals: TokenTotals::default(),
            total_tokens: 0,
            total_cost: 0.0,
            last_context_tokens: None,
            context_unknown: false,
        };
    }

    let file_path = unique_session_file_path(config_dir, cwd, None);
    LoadedSession {
        session_file: file_path,
        session_id: new_unique_session_id(),
        session_name: None,
        parent_session: None,
        entries: Vec::new(),
        leaf_id: None,
        entry_seq: 0,
        ui_messages: Vec::new(),
        llm_history: Vec::new(),
        token_totals: TokenTotals::default(),
        total_tokens: 0,
        total_cost: 0.0,
        last_context_tokens: None,
        context_unknown: false,
    }
}

pub(super) fn active_branch_entries(entries: &[Value], leaf_id: Option<&str>) -> Vec<Value> {
    if entries.is_empty() {
        return Vec::new();
    }
    let Some(target_leaf) = leaf_id else {
        return entries.to_vec();
    };

    let mut by_id: HashMap<&str, usize> = HashMap::with_capacity(entries.len());
    for (idx, e) in entries.iter().enumerate() {
        if let Some(id) = e["id"].as_str().filter(|s| !s.is_empty()) {
            by_id.insert(id, idx);
        }
    }
    if !by_id.contains_key(target_leaf) {
        return entries.to_vec();
    }

    let mut path_rev = Vec::new();
    let mut visited = HashSet::new();
    let mut cur_opt: Option<&str> = Some(target_leaf);

    while let Some(cur_id) = cur_opt {
        if !visited.insert(cur_id) {
            break;
        }
        let Some(&idx) = by_id.get(cur_id) else {
            break;
        };
        let entry = &entries[idx];
        path_rev.push(entry.clone());

        cur_opt = match entry.get("parentId") {
            Some(Value::String(pid)) if !pid.is_empty() => Some(pid.as_str()),
            Some(Value::Null) => None,
            // Legacy v1 tanpa field parentId -> sambungkan ke entry sebelumnya dalam file
            None if idx > 0 => entries[idx - 1]["id"].as_str(),
            _ => None,
        };
    }

    path_rev.reverse();
    path_rev
}

pub(super) fn parse_usage_fields(usage: &Value) -> (TokenTotals, f64) {
    if !usage.is_object() {
        return (TokenTotals::default(), 0.0);
    }
    let input = usage["input"].as_u64().unwrap_or(0);
    let output = usage["output"].as_u64().unwrap_or(0);
    let cache_read = usage["cacheRead"].as_u64().unwrap_or(0);
    let cache_write = usage["cacheWrite"].as_u64().unwrap_or(0);
    let cost = usage
        .get("cost")
        .and_then(|c| c["total"].as_f64())
        .or_else(|| usage["cost"].as_f64())
        .unwrap_or(0.0);
    (
        TokenTotals {
            input,
            output,
            cache_read,
            cache_write,
        },
        cost,
    )
}

pub(super) fn apply_context_replacement(msg: &Value, replacement: &Value) -> Value {
    let mut updated = msg.clone();
    let role = msg["role"].as_str().unwrap_or_default();
    if let Some(s) = replacement.as_str() {
        if role == "assistant" || role == "toolResult" {
            updated["content"] = json!([{ "type": "text", "text": s }]);
        } else {
            updated["content"] = json!(s);
        }
    } else {
        updated["content"] = replacement.clone();
    }
    updated
}

pub(super) fn rebuild_context_from_entries(path_entries: &[Value]) -> RebuiltContext {
    let mut token_totals = TokenTotals::default();
    let mut computed_cost = 0.0f64;
    let mut stats_floor_tokens = 0u64;
    let mut stats_floor_cost = 0.0f64;
    let mut last_context_tokens: Option<u64> = None;
    let mut context_unknown = false;

    for entry in path_entries {
        match entry["type"].as_str().unwrap_or_default() {
            "session_stats" => {
                if let Some(t) = entry["total_tokens"].as_u64() {
                    stats_floor_tokens = stats_floor_tokens.max(t);
                }
                if let Some(c) = entry["total_cost"].as_f64() {
                    stats_floor_cost = stats_floor_cost.max(c);
                }
            }
            "usage" => {
                let (u, c) = parse_usage_fields(&entry["usage"]);
                token_totals.input += u.input;
                token_totals.output += u.output;
                token_totals.cache_read += u.cache_read;
                token_totals.cache_write += u.cache_write;
                computed_cost += c;
                if let Some(ctx) = entry["lastContextTokens"].as_u64().filter(|&n| n > 0) {
                    last_context_tokens = Some(ctx);
                    context_unknown = false;
                }
            }
            "compaction" => {
                let (u, c) = parse_usage_fields(&entry["usage"]);
                token_totals.input += u.input;
                token_totals.output += u.output;
                token_totals.cache_read += u.cache_read;
                token_totals.cache_write += u.cache_write;
                computed_cost += c;
                last_context_tokens = None;
                context_unknown = true;
            }
            "branch_summary" => {
                let (u, c) = parse_usage_fields(&entry["usage"]);
                token_totals.input += u.input;
                token_totals.output += u.output;
                token_totals.cache_read += u.cache_read;
                token_totals.cache_write += u.cache_write;
                computed_cost += c;
            }
            "message" => {
                let msg = &entry["message"];
                if msg["role"].as_str() == Some("assistant") {
                    if let Some(usage) = msg.get("usage") {
                        let (u, u_cost) = parse_usage_fields(usage);
                        token_totals.input += u.input;
                        token_totals.output += u.output;
                        token_totals.cache_read += u.cache_read;
                        token_totals.cache_write += u.cache_write;
                        computed_cost += u_cost;

                        let ctx = usage["totalTokens"]
                            .as_u64()
                            .filter(|&n| n > 0)
                            .unwrap_or_else(|| u.total());
                        if ctx > 0 {
                            last_context_tokens = Some(ctx);
                            context_unknown = false;
                        }
                    }
                    if let Some(top_cost) = msg
                        .get("cost")
                        .and_then(|c| c["total"].as_f64())
                        .or_else(|| msg["cost"].as_f64())
                    {
                        computed_cost += top_cost;
                    }
                }
            }
            _ => {}
        }
    }

    let last_compact_idx = path_entries
        .iter()
        .rposition(|e| e["type"].as_str() == Some("compaction"));
    let previous_compaction_summary = last_compact_idx
        .and_then(|ci| path_entries[ci]["summary"].as_str())
        .map(str::to_string);

    let mut selected_entries: Vec<&Value> = Vec::new();
    if let Some(ci) = last_compact_idx {
        let compact_entry = &path_entries[ci];
        let compact_id = compact_entry["id"].as_str().unwrap_or("");
        let first_kept_id = compact_entry["firstKeptEntryId"].as_str().unwrap_or("");
        selected_entries.push(compact_entry);

        if !first_kept_id.is_empty() && first_kept_id != compact_id {
            if let Some(pos) = path_entries[..ci]
                .iter()
                .position(|e| e["id"].as_str() == Some(first_kept_id))
            {
                for e in &path_entries[pos..ci] {
                    if e["type"].as_str() == Some("message")
                        && e["message"]["role"].as_str() == Some("system")
                    {
                        continue;
                    }
                    selected_entries.push(e);
                }
            }
        }
        for e in &path_entries[ci + 1..] {
            selected_entries.push(e);
        }
    } else {
        for e in path_entries {
            selected_entries.push(e);
        }
    }

    let mut context_edits: HashMap<&str, &Value> = HashMap::new();
    for e in path_entries {
        if e["type"].as_str() == Some("context_edit") {
            if let Some(tid) = e["targetId"].as_str().filter(|s| !s.is_empty()) {
                if let Some(repl) = e.get("replacement") {
                    context_edits.insert(tid, repl);
                }
            }
        }
    }

    let mut ui_messages = Vec::new();
    let mut context_messages = Vec::new();
    let mut active_entries = Vec::new();

    for entry in selected_entries {
        let entry_id = entry["id"].as_str();
        let entry_type = entry["type"].as_str().unwrap_or_default();
        let edit_override = entry_id.and_then(|eid| context_edits.get(eid).copied());

        match entry_type {
            "compaction" => {
                let msg = json!({
                    "role": "compactionSummary",
                    "summary": entry["summary"].as_str().unwrap_or_default(),
                    "tokensBefore": entry["tokensBefore"].as_u64().unwrap_or(0),
                    "timestamp": parse_iso_to_epoch_ms(entry["timestamp"].as_str()),
                });
                ui_messages.push(msg.clone());
                context_messages.push(msg);
            }
            "branch_summary" => {
                let msg = json!({
                    "role": "branchSummary",
                    "summary": entry["summary"].as_str().unwrap_or_default(),
                    "fromId": entry["fromId"],
                    "timestamp": parse_iso_to_epoch_ms(entry["timestamp"].as_str()),
                });
                ui_messages.push(msg.clone());
                context_messages.push(msg);
            }
            "custom_message" => {
                let display = entry["display"].as_bool().unwrap_or(true);
                let msg = json!({
                    "role": "custom",
                    "customType": entry["customType"].as_str().unwrap_or_default(),
                    "content": entry["content"],
                    "display": display,
                    "details": entry["details"],
                    "timestamp": parse_iso_to_epoch_ms(entry["timestamp"].as_str()),
                });
                if display {
                    ui_messages.push(msg.clone());
                }
                match edit_override {
                    Some(v) if v.is_null() => {}
                    Some(repl) => context_messages.push(apply_context_replacement(&msg, repl)),
                    None => context_messages.push(msg),
                }
                active_entries.push(entry.clone());
            }
            "bashExecution" => {
                let mut msg = entry.clone();
                if msg.get("role").and_then(|r| r.as_str()).is_none() {
                    msg["role"] = json!("bashExecution");
                }
                ui_messages.push(msg.clone());
                if msg["excludeFromContext"].as_bool() != Some(true) {
                    match edit_override {
                        Some(v) if v.is_null() => {}
                        Some(repl) => context_messages.push(apply_context_replacement(&msg, repl)),
                        None => context_messages.push(msg),
                    }
                }
                active_entries.push(entry.clone());
            }
            "message" => {
                let msg = &entry["message"];
                if msg.is_null() {
                    continue;
                }
                let role = msg["role"].as_str().unwrap_or_default();
                if role == "system" {
                    continue;
                }
                ui_messages.push(msg.clone());
                match edit_override {
                    Some(v) if v.is_null() => {}
                    Some(repl) => context_messages.push(apply_context_replacement(msg, repl)),
                    None => context_messages.push(msg.clone()),
                }
                active_entries.push(entry.clone());
            }
            _ => {}
        }
    }

    let llm_history = ui_history_to_llm(&context_messages);
    let total_tokens = token_totals.total().max(stats_floor_tokens);
    let total_cost = computed_cost.max(stats_floor_cost);

    RebuiltContext {
        ui_messages,
        llm_history,
        active_entries,
        previous_compaction_summary,
        token_totals,
        total_tokens,
        total_cost,
        last_context_tokens,
        context_unknown,
    }
}


pub(super) fn extract_message_text_and_images(msg: &Value) -> (String, Vec<(String, String)>) {
    let mut text = String::new();
    let mut images = Vec::new();
    if let Some(s) = msg["content"].as_str() {
        text = s.to_string();
    } else if let Some(arr) = msg["content"].as_array() {
        let mut texts = Vec::new();
        for b in arr {
            if let Some(t) = b["text"].as_str() {
                texts.push(t);
            } else if b["type"].as_str() == Some("image") {
                let mime = b["mimeType"].as_str().unwrap_or("image/jpeg").to_string();
                let data = b["data"].as_str().unwrap_or_default().to_string();
                if !data.is_empty() {
                    images.push((mime, data));
                }
            }
        }
        text = texts.join("\n");
    }
    (text, images)
}

pub(super) fn extract_user_message_text(msg: &Value) -> String {
    extract_message_text_and_images(msg).0
}

pub(super) fn ui_message_to_llm(msg: &Value) -> Option<Value> {
    match msg["role"].as_str()? {
        "user" | "custom" => {
            let (text, images) = extract_message_text_and_images(msg);
            if text.is_empty() && images.is_empty() {
                None
            } else {
                Some(user_message_with_images(&text, &images))
            }
        }
        "bashExecution" => {
            if msg["excludeFromContext"].as_bool() == Some(true) {
                return None;
            }
            let cmd = msg["command"].as_str().unwrap_or_default();
            let out = msg["output"].as_str().unwrap_or_default();
            let code = msg["exitCode"].as_i64();
            let mut body = format!("$ {}", cmd);
            if !out.is_empty() {
                body.push('\n');
                body.push_str(out);
            }
            if let Some(c) = code {
                if c != 0 {
                    body.push_str(&format!("\n[exit code: {}]", c));
                }
            }
            Some(user_text(&body))
        }
        "compactionSummary" => {
            let summary = msg["summary"].as_str().unwrap_or_default().trim();
            if summary.is_empty() {
                None
            } else {
                Some(user_text(&format!(
                    "[Ringkasan Kompak Konteks Sebelumnya]\n{}",
                    summary
                )))
            }
        }
        "branchSummary" => {
            let summary = msg["summary"].as_str().unwrap_or_default().trim();
            if summary.is_empty() {
                None
            } else {
                Some(user_text(&format!(
                    "[Ringkasan Branch Sebelumnya]\n{}",
                    summary
                )))
            }
        }
        "assistant" => {
            let arr = msg["content"].as_array()?;
            let mut parts = Vec::new();
            for b in arr {
                match b["type"].as_str().unwrap_or_default() {
                    "thinking" => {
                        // Hanya kirim ulang thought bila memiliki thoughtSignature asli.
                        // Thinking sintetis tanpa signature tidak boleh diputar ulang ke LLM.
                        if let Some(sig) = b["thoughtSignature"]
                            .as_str()
                            .filter(|s| !s.trim().is_empty())
                        {
                            if let Some(th) = b["thinking"].as_str().filter(|s| !s.trim().is_empty())
                            {
                                parts.push(json!({
                                    "thought": true,
                                    "text": th,
                                    "thoughtSignature": sig,
                                }));
                            }
                        }
                    }
                    "text" => {
                        if let Some(t) = b["text"].as_str().filter(|s| !s.is_empty()) {
                            parts.push(json!({ "text": t }));
                        }
                    }
                    "toolCall" => {
                        let raw_name = b["name"].as_str().unwrap_or_default();
                        let internal_name = reverse_map_tool_name(raw_name);
                        let args = b
                            .get("arguments")
                            .or_else(|| b.get("args"))
                            .cloned()
                            .unwrap_or_else(|| json!({}));
                        let mut fc = json!({
                            "name": internal_name,
                            "args": args,
                        });
                        if let Some(cid) = b["id"].as_str().filter(|s| !s.is_empty()) {
                            fc["id"] = json!(cid);
                        }
                        let mut part = json!({ "functionCall": fc });
                        if let Some(sig) = b["thoughtSignature"]
                            .as_str()
                            .filter(|s| !s.trim().is_empty())
                        {
                            part["thoughtSignature"] = json!(sig);
                        }
                        parts.push(part);
                    }
                    _ => {}
                }
            }
            if parts.is_empty() {
                None
            } else {
                Some(json!({ "role": "model", "parts": parts }))
            }
        }
        _ => None,
    }
}

pub(super) fn tool_result_to_response_payload(msg: &Value) -> Value {
    let text = extract_user_message_text(msg);
    if msg["isError"].as_bool() == Some(true) {
        json!({ "error": text })
    } else {
        json!({ "output": text })
    }
}

pub(super) fn ui_history_to_llm(messages: &[Value]) -> Vec<Value> {
    let mut llm = Vec::new();
    let mut i = 0usize;
    let mut prev_was_summary_user = false;

    while i < messages.len() {
        let msg = &messages[i];
        let role = msg["role"].as_str().unwrap_or_default();

        if role == "toolResult" {
            // toolResult yatim tanpa didahului assistant toolCall diabaikan agar tidak memicu HTTP 400.
            i += 1;
            continue;
        }

        if role == "assistant" {
            let Some(mut llm_item) = ui_message_to_llm(msg) else {
                i += 1;
                continue;
            };
            prev_was_summary_user = false;

            let mut expected_calls: Vec<(String, String, bool)> = Vec::new();
            if let Some(parts) = llm_item.get_mut("parts").and_then(|p| p.as_array_mut()) {
                for (call_idx, p) in parts.iter_mut().enumerate() {
                    let Some(fc) = p.get_mut("functionCall") else {
                        continue;
                    };
                    let name = fc["name"].as_str().unwrap_or_default().to_string();
                    let orig_id = fc["id"]
                        .as_str()
                        .filter(|s| !s.is_empty())
                        .map(str::to_string);
                    let (exp_id, had_orig_id) = match orig_id {
                        Some(id) => (id, true),
                        None => {
                            let gen_id = format!("reloaded_call_{}_{}", i, call_idx);
                            fc["id"] = json!(gen_id);
                            (gen_id, false)
                        }
                    };
                    expected_calls.push((exp_id, name, had_orig_id));
                }
            }

            llm.push(llm_item);
            i += 1;

            if !expected_calls.is_empty() {
                let mut collected_results: Vec<(String, String, Value)> = Vec::new();
                while i < messages.len() && messages[i]["role"].as_str() == Some("toolResult") {
                    let tr = &messages[i];
                    let cid = tr["toolCallId"].as_str().unwrap_or_default().to_string();
                    let tname = reverse_map_tool_name(tr["toolName"].as_str().unwrap_or_default())
                        .to_string();
                    let payload = tool_result_to_response_payload(tr);
                    collected_results.push((cid, tname, payload));
                    i += 1;
                }

                let expected_ids: HashSet<&str> = expected_calls
                    .iter()
                    .map(|(id, _, _)| id.as_str())
                    .collect();
                let exact_reserved: HashSet<usize> = collected_results
                    .iter()
                    .enumerate()
                    .filter(|(_, (cid, _, _))| {
                        !cid.is_empty() && expected_ids.contains(cid.as_str())
                    })
                    .map(|(idx, _)| idx)
                    .collect();

                let mut used_results = HashSet::new();
                let mut assigned: Vec<Option<usize>> = vec![None; expected_calls.len()];

                // Pass 1: exact non-empty toolCallId match
                for (cidx, (exp_id, _, _)) in expected_calls.iter().enumerate() {
                    if let Some((ridx, _)) =
                        collected_results.iter().enumerate().find(|(idx, (cid, _, _))| {
                            !used_results.contains(idx) && !cid.is_empty() && cid == exp_id
                        })
                    {
                        used_results.insert(ridx);
                        assigned[cidx] = Some(ridx);
                    }
                }

                // Pass 2a: calls without original ID -> match unreserved by tool name (or empty name & ID)
                for (cidx, (_, exp_name, had_orig)) in expected_calls.iter().enumerate() {
                    if *had_orig || assigned[cidx].is_some() {
                        continue;
                    }
                    if let Some((ridx, _)) = collected_results.iter().enumerate().find(
                        |(idx, (cid, tname, _))| {
                            !used_results.contains(idx)
                                && !exact_reserved.contains(idx)
                                && (tname == exp_name || (cid.is_empty() && tname.is_empty()))
                        },
                    ) {
                        used_results.insert(ridx);
                        assigned[cidx] = Some(ridx);
                    }
                }

                // Pass 2b: remaining calls with original ID -> match unreserved empty-ID result with same/empty name
                for (cidx, (_, exp_name, _)) in expected_calls.iter().enumerate() {
                    if assigned[cidx].is_some() {
                        continue;
                    }
                    if let Some((ridx, _)) = collected_results.iter().enumerate().find(
                        |(idx, (cid, tname, _))| {
                            !used_results.contains(idx)
                                && !exact_reserved.contains(idx)
                                && cid.is_empty()
                                && (tname == exp_name || tname.is_empty())
                        },
                    ) {
                        used_results.insert(ridx);
                        assigned[cidx] = Some(ridx);
                    }
                }

                // Pass 3: remaining calls without original ID -> first unused & unreserved empty-ID result
                for (cidx, (_, _, had_orig)) in expected_calls.iter().enumerate() {
                    if *had_orig || assigned[cidx].is_some() {
                        continue;
                    }
                    if let Some((ridx, _)) =
                        collected_results.iter().enumerate().find(|(idx, (cid, _, _))| {
                            !used_results.contains(idx)
                                && !exact_reserved.contains(idx)
                                && cid.is_empty()
                        })
                    {
                        used_results.insert(ridx);
                        assigned[cidx] = Some(ridx);
                    }
                }

                let mut response_parts = Vec::with_capacity(expected_calls.len());
                for (cidx, (exp_id, exp_name, _)) in expected_calls.iter().enumerate() {
                    if let Some(ridx) = assigned[cidx] {
                        let (_, tname, resp_val) = &collected_results[ridx];
                        let name = if tname.is_empty() {
                            exp_name.as_str()
                        } else {
                            tname.as_str()
                        };
                        response_parts.push(json!({
                            "functionResponse": {
                                "id": exp_id,
                                "name": name,
                                "response": resp_val,
                            }
                        }));
                    } else {
                        response_parts.push(json!({
                            "functionResponse": {
                                "id": exp_id,
                                "name": exp_name,
                                "response": { "error": "Tool call interrupted before completion" },
                            }
                        }));
                    }
                }

                if !response_parts.is_empty() {
                    llm.push(json!({ "role": "user", "parts": response_parts }));
                }
            }
            continue;
        }

        if let Some(llm_item) = ui_message_to_llm(msg) {
            if prev_was_summary_user {
                llm.push(json!({
                    "role": "model",
                    "parts": [{ "text": "Ringkasan konteks dipahami." }]
                }));
            }
            llm.push(llm_item);
            prev_was_summary_user = matches!(role, "compactionSummary" | "branchSummary");
        }
        i += 1;
    }

    if prev_was_summary_user {
        llm.push(json!({
            "role": "model",
            "parts": [{ "text": "Ringkasan konteks dipahami." }]
        }));
    }

    llm
}

