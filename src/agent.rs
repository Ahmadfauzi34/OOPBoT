//! Agent harness: loop "model → tool call → hasil tool → model" dengan:
//! - Multi-tab (`WorkspaceTabs`) & pemilih model (`ModelCatalog`) ala Pipkin
//! - Event stream terstruktur (`EventSink` / `AgentEvent`) yang otomatis dilipat
//!   ke `TimelineEntry::StepsFold`
//! - Kebijakan konfirmasi (`Approver`) yang bisa diganti (CLI / Auto / UI Card)

use crate::events::{
    AgentEvent, CliEventSink, EventSink, MemoryEventCollector, StepsOutcome, TimelineEntry,
    ToolStatus,
};
use crate::gemini::{
    function_response_part, user_message_with_images, user_text, BoxError, FunctionCall,
    GeminiClient, GenerationOptions, TokenUsage,
};
use crate::models::{ModelCatalog, ModelInfo};
use crate::session::{ChatSession, WorkspaceTabs};
use crate::tools::ToolRegistry;
use async_trait::async_trait;
use serde_json::{json, Value};
use std::io::{self, Write};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Instant;

/// Hook sinkron untuk menyuntikkan pesan `steer` yang sedang antre di sela langkah tool.
pub type SteerHook = Arc<dyn Fn() -> Vec<ChatInput> + Send + Sync>;

/// Input satu giliran obrolan: prompt teks dan daftar gambar inline opsional.
#[derive(Debug, Clone, Default)]
pub struct ChatInput {
    pub text: String,
    pub images: Vec<(String, String)>, // (mime, base64) — kosong di CLI/FFI
}

impl ChatInput {
    pub fn text_only(text: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            images: Vec::new(),
        }
    }

    pub fn with_images(text: impl Into<String>, images: Vec<(String, String)>) -> Self {
        Self {
            text: text.into(),
            images,
        }
    }
}

/// Hasil satu giliran obrolan agen: teks jawaban asisten, rekaman event,
/// jumlah langkah tool, status penyelesaian, dan akumulasi penggunaan token.
#[derive(Debug, Clone)]
pub struct TurnOutcome {
    pub text: String,
    pub events: Vec<AgentEvent>,
    pub steps: usize,
    pub outcome: StepsOutcome,
    pub usage: Option<TokenUsage>,
    pub last_usage: Option<TokenUsage>,
    pub aborted: bool,
}

/// Interface agen (OOP).
#[async_trait]
pub trait Agent {
    fn get_name(&self) -> &str;
    async fn chat(&self, prompt: &str) -> Result<String, BoxError>;
    async fn chat_turn(&self, input: &ChatInput) -> Result<TurnOutcome, BoxError>;
    fn clear_memory(&self);
}

/// Kebijakan persetujuan sebelum tool "berbahaya" dijalankan.
#[async_trait]
pub trait Approver: Send + Sync {
    async fn approve(&self, tool_name: &str, preview: &str) -> bool;
}

/// Approver interaktif via terminal. Jawaban:
/// `y` = ya sekali, `a` = ya untuk semua sisa sesi, selain itu = tolak.
pub struct CliApprover {
    always: AtomicBool,
}

impl CliApprover {
    pub fn new(auto_approve: bool) -> Self {
        Self {
            always: AtomicBool::new(auto_approve),
        }
    }
}

#[async_trait]
impl Approver for CliApprover {
    async fn approve(&self, tool_name: &str, preview: &str) -> bool {
        if self.always.load(Ordering::Relaxed) {
            return true;
        }
        print!("│  ⚠️  Izinkan {} → {} ? [y/N/a=selalu] ", tool_name, preview);
        let _ = io::stdout().flush();
        let mut answer = String::new();
        if io::stdin().read_line(&mut answer).is_err() {
            return false;
        }
        match answer.trim().to_lowercase().as_str() {
            "y" | "ya" | "yes" => true,
            "a" | "all" | "selalu" => {
                self.always.store(true, Ordering::Relaxed);
                true
            }
            _ => false,
        }
    }
}

pub struct GeminiAgent {
    name: String,
    client: GeminiClient,
    tools: ToolRegistry,
    approver: Box<dyn Approver>,
    event_sink: Box<dyn EventSink>,
    system_instruction: String,
    max_steps: usize,
    catalog: Mutex<ModelCatalog>,
    tabs: Mutex<WorkspaceTabs>,
    /// Ke mana tab dipersist (`Some` = path default CLI; `None` = jangan persist,
    /// dipakai jalur RPC yang store-nya adalah file sesi JSONL, bukan tabs.json).
    tabs_path: Option<PathBuf>,
    thinking_level: Mutex<String>,
}

impl GeminiAgent {
    pub fn new(
        name: &str,
        client: GeminiClient,
        tools: ToolRegistry,
        approver: Box<dyn Approver>,
        max_steps: usize,
    ) -> Self {
        Self::with_event_sink(
            name,
            client,
            tools,
            approver,
            Box::new(CliEventSink),
            max_steps,
        )
    }

    pub fn with_event_sink(
        name: &str,
        client: GeminiClient,
        tools: ToolRegistry,
        approver: Box<dyn Approver>,
        event_sink: Box<dyn EventSink>,
        max_steps: usize,
    ) -> Self {
        let default_m = client.model();
        let tabs = WorkspaceTabs::default_storage_path()
            .and_then(|p| WorkspaceTabs::load_from_file(&p))
            .unwrap_or_else(|| WorkspaceTabs::new(&default_m));
        Self::with_event_sink_and_tabs(
            name,
            client,
            tools,
            approver,
            event_sink,
            max_steps,
            tabs,
            WorkspaceTabs::default_storage_path(),
        )
    }

    /// Varian konstruktor yang tab-nya disuplai pemanggil beserta lokasi
    /// persistensinya (`None` = tab hanya di memori, tidak ditulis ke disk).
    /// Dipakai jalur RPC: tab di-seed dari file sesi JSONL dan store-nya tetap
    /// file sesi itu, bukan `tabs.json` milik CLI.
    #[allow(clippy::too_many_arguments)]
    pub fn with_event_sink_and_tabs(
        name: &str,
        client: GeminiClient,
        tools: ToolRegistry,
        approver: Box<dyn Approver>,
        event_sink: Box<dyn EventSink>,
        max_steps: usize,
        tabs: WorkspaceTabs,
        tabs_path: Option<PathBuf>,
    ) -> Self {
        let system_instruction = default_system_instruction(&tools);
        let mut catalog = ModelCatalog::all_builtin();
        let default_m = client.model();
        if catalog.find(&default_m).is_none() {
            catalog.register_custom(&default_m, &default_m);
        }
        catalog.set_default_model(&default_m);

        Self {
            name: name.to_string(),
            client,
            tools,
            approver,
            event_sink,
            system_instruction,
            max_steps,
            catalog: Mutex::new(catalog),
            tabs: Mutex::new(tabs),
            tabs_path,
            thinking_level: Mutex::new("medium".to_string()),
        }
    }

    /// Model yang sedang aktif di tab saat ini.
    pub fn model(&self) -> String {
        self.tabs.lock().unwrap().active().model.clone()
    }

    /// Info lengkap model aktif saat ini.
    pub fn active_model_info(&self) -> ModelInfo {
        let active_id = self.model();
        let cat = self.catalog.lock().unwrap();
        cat.find(&active_id).cloned().unwrap_or_else(|| ModelInfo {
            id: active_id.clone(),
            label: active_id,
            provider: "google".into(),
            supports_tools: true,
            input_token_limit: Some(1_048_576),
            output_token_limit: Some(65_536),
            description: "Active model".into(),
            api: "google-generative-ai".into(),
            reasoning: true,
            thinking_level_map: None,
            input: vec!["text".into(), "image".into()],
            cost: None,
        })
    }

    pub fn thinking_level(&self) -> String {
        self.thinking_level.lock().unwrap().clone()
    }

    pub fn set_thinking_level(&self, level: &str) -> String {
        let clamped = self.active_model_info().clamp_thinking_level(level.trim());
        *self.thinking_level.lock().unwrap() = clamped.clone();
        clamped
    }

    /// Ganti system instruction (jalur RPC memakai instruksinya sendiri,
    /// bukan default CLI yang dibangun di konstruktor).
    pub fn set_system_instruction(&mut self, instruction: String) {
        self.system_instruction = instruction;
    }

    pub fn system_instruction(&self) -> &str {
        &self.system_instruction
    }

    /// Teruskan perubahan kredensial ke client (dipakai refresh_credentials RPC).
    pub fn update_credentials(&self, api_key: String, base_url: Option<String>) {
        self.client.update_credentials(api_key, base_url);
    }

    pub fn api_key(&self) -> String {
        self.client.api_key()
    }

    pub fn base_url(&self) -> String {
        self.client.base_url()
    }

    /// Masukkan satu entri ModelInfo lengkap ke katalog agen (dipakai untuk
    /// menyelaraskan katalog agen dengan katalog RPC yang membawa override
    /// models.json dan model hasil scan provider).
    pub fn upsert_model(&self, info: ModelInfo) {
        self.catalog.lock().unwrap().upsert_model(info);
    }

    /// Bersihkan histori + timeline tab aktif tanpa mencetak apa pun.
    /// Dipakai jalur RPC (stdout adalah aliran JSONL — println akan merusaknya).
    pub fn clear_memory_silent(&self) {
        let mut tabs = self.tabs.lock().unwrap();
        tabs.active_mut().clear();
        self.persist_tabs(&tabs);
    }

    /// Terapkan konfigurasi override ke model di katalog (`maxTokens`, `contextWindow`, `reasoning`, `thinkingLevelMap`, dsb.).
    pub fn configure_model(&self, model_id: &str, override_json: &Value) -> ModelInfo {
        let mut cat = self.catalog.lock().unwrap();
        let mut info = cat.find(model_id).cloned().unwrap_or_else(|| ModelInfo {
            id: model_id.to_string(),
            label: model_id.to_string(),
            provider: "custom".into(),
            supports_tools: true,
            input_token_limit: Some(1_048_576),
            output_token_limit: Some(65_536),
            description: "Configured model".into(),
            api: "google-generative-ai".into(),
            reasoning: true,
            thinking_level_map: None,
            input: vec!["text".into(), "image".into()],
            cost: None,
        });
        info.apply_override(override_json);
        cat.upsert_model(info.clone());
        info
    }

    /// Ganti model pada tab yang sedang aktif (bisa lewat nomor katalog `1..N` atau ID model).
    pub fn set_active_model(&self, query: &str) -> String {
        let resolved = {
            let mut cat = self.catalog.lock().unwrap();
            if let Some(info) = cat.find(query) {
                info.id.clone()
            } else {
                cat.register_custom(query, query);
                query.to_string()
            }
        };
        let mut tabs = self.tabs.lock().unwrap();
        tabs.active_mut().model = resolved.clone();
        self.persist_tabs(&tabs);
        self.client.set_default_model(&resolved);
        resolved
    }

    pub fn list_models(&self) -> Vec<ModelInfo> {
        self.catalog.lock().unwrap().list().to_vec()
    }

    /// Scan daftar model dari server provider menggunakan token aktif,
    /// lalu perbarui `ModelCatalog`.
    pub async fn scan_models(&self) -> Result<Vec<ModelInfo>, BoxError> {
        let scanned = self.client.scan_models().await?;
        let mut cat = self.catalog.lock().unwrap();
        cat.replace_with_scanned(scanned.clone());
        Ok(scanned)
    }

    /// Ganti token (dan opsional `base_url`) secara langsung, simpan ke `~/.config/oopbot/.env`,
    /// lalu otomatis scan daftar model dari provider tersebut.
    pub async fn update_provider_token(
        &self,
        api_key: &str,
        base_url: Option<&str>,
    ) -> Result<Vec<ModelInfo>, BoxError> {
        let clean_key = api_key.trim().to_string();
        if clean_key.is_empty() {
            return Err("Token/API key tidak boleh kosong".into());
        }
        let clean_url = base_url.map(|u| u.trim().to_string()).filter(|u| !u.is_empty());
        self.client.update_credentials(clean_key.clone(), clean_url.clone());
        save_token_to_user_env(&clean_key, clean_url.as_deref(), &self.model());
        self.scan_models().await
    }

    pub fn list_tabs(&self) -> (usize, Vec<ChatSession>) {
        let tabs = self.tabs.lock().unwrap();
        (tabs.active_index(), tabs.list().to_vec())
    }

    pub fn active_tab_summary(&self) -> (String, String, String) {
        let tabs = self.tabs.lock().unwrap();
        let t = tabs.active();
        (t.id.clone(), t.title.clone(), t.model.clone())
    }

    pub fn new_tab(&self, model: Option<&str>) -> ChatSession {
        let model_id = model
            .map(str::to_string)
            .unwrap_or_else(|| self.model());
        let mut tabs = self.tabs.lock().unwrap();
        let created = tabs.new_tab(&model_id).clone();
        self.persist_tabs(&tabs);
        self.client.set_default_model(&created.model);
        created
    }

    pub fn switch_tab(&self, selector: &str) -> Option<ChatSession> {
        let mut tabs = self.tabs.lock().unwrap();
        let switched = tabs.switch_to(selector).cloned();
        if let Some(ref s) = switched {
            self.persist_tabs(&tabs);
            self.client.set_default_model(&s.model);
        }
        switched
    }

    pub fn close_tab(&self, selector: &str) -> bool {
        let fallback = self.client.model();
        let mut tabs = self.tabs.lock().unwrap();
        let ok = tabs.close_tab(selector, &fallback);
        if ok {
            self.persist_tabs(&tabs);
            self.client.set_default_model(&tabs.active().model);
        }
        ok
    }

    pub fn active_timeline(&self) -> Vec<TimelineEntry> {
        self.tabs.lock().unwrap().active().timeline.clone()
    }

    pub fn active_history(&self) -> Vec<Value> {
        self.tabs.lock().unwrap().active().history.clone()
    }

    pub fn replace_active_history(&self, id: &str, title: Option<&str>, history: Vec<Value>) {
        let mut tabs = self.tabs.lock().unwrap();
        let active = tabs.active_mut();
        active.id = id.to_string();
        active.title = match title {
            Some(t) if !t.trim().is_empty() => t.to_string(),
            _ => format!("Tab {}", id),
        };
        active.history = history;
        active.timeline.clear();
        self.persist_tabs(&tabs);
    }

    pub fn append_user_message_to_active_history(&self, input: &ChatInput) {
        let msg = if input.images.is_empty() {
            user_text(&input.text)
        } else {
            user_message_with_images(&input.text, &input.images)
        };
        let mut tabs = self.tabs.lock().unwrap();
        let active = tabs.active_mut();
        active.update_title_from_prompt(&input.text);
        active.history.push(msg);
        self.persist_tabs(&tabs);
    }

    pub fn tool_names(&self) -> Vec<&'static str> {
        self.tools.names()
    }

    fn persist_tabs(&self, tabs: &WorkspaceTabs) {
        if let Some(path) = &self.tabs_path {
            let _ = tabs.save_to_file(path);
        }
    }

    fn emit(&self, event: AgentEvent, collector: &MemoryEventCollector) {
        self.event_sink.on_event(&event);
        collector.on_event(&event);
    }

    /// Buat ringkasan konteks (untuk `compact` RPC) dengan dukungan pembatalan asinkron.
    pub async fn generate_compaction_summary_cancellable(
        &self,
        prompt: &str,
        mut cancel_rx: tokio::sync::watch::Receiver<bool>,
    ) -> Result<(String, Option<TokenUsage>), BoxError> {
        let (session_id, active_model) = {
            let tabs = self.tabs.lock().unwrap();
            let s = tabs.active();
            (s.id.clone(), s.model.clone())
        };
        let model_info = self.active_model_info();
        let gen_options = GenerationOptions {
            api: Some(model_info.api.clone()),
            max_tokens: model_info.output_token_limit,
            thinking_level: Some(self.thinking_level()),
            reasoning: model_info.reasoning,
            thinking_level_map: model_info.thinking_level_map.clone(),
            session_id: Some(session_id),
        };
        let contents = vec![user_text(prompt)];
        let sys = "Kamu adalah asisten peringkas percakapan yang akurat dan ringkas.";
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
                let rejected =
                    json!({ "error": "User menolak eksekusi tool ini. Tanyakan alternatif ke user." });
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
                Some(result)
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
        mut cancel_rx: tokio::sync::watch::Receiver<bool>,
        steer_hook: Option<SteerHook>,
    ) -> Result<TurnOutcome, BoxError> {
        let (session_id, active_model, mut history) = {
            let tabs = self.tabs.lock().unwrap();
            let s = tabs.active();
            (s.id.clone(), s.model.clone(), s.history.clone())
        };

        let user_msg = if input.images.is_empty() {
            user_text(&input.text)
        } else {
            user_message_with_images(&input.text, &input.images)
        };
        history.push(user_msg);

        let decls = self.tools.declarations();
        let collector = MemoryEventCollector::new();
        let turn_started = Instant::now();
        let mut tool_steps = 0usize;
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
                    }
                }
                if *cancel_rx.borrow() {
                    was_aborted = true;
                    break;
                }
            }

            let turn = tokio::select! {
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
                ) => res?,
            };
            history.push(turn.raw_content.clone());

            if let Some(u) = turn.usage {
                let cur = total_usage.get_or_insert(TokenUsage::default());
                cur.input_tokens += u.input_tokens;
                cur.output_tokens += u.output_tokens;
                last_usage = Some(u);
            }

            for thought in &turn.thoughts {
                self.emit(
                    AgentEvent::Thinking {
                        step_index: step_idx,
                        text: thought.clone(),
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
                let mut tabs = self.tabs.lock().unwrap();
                let active = tabs.active_mut();
                active.update_title_from_prompt(&input.text);
                active.history = history;
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
        let mut tabs = self.tabs.lock().unwrap();
        let active = tabs.active_mut();
        active.update_title_from_prompt(&input.text);
        active.history = history;
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

#[async_trait]
impl Agent for GeminiAgent {
    fn get_name(&self) -> &str {
        &self.name
    }

    async fn chat(&self, prompt: &str) -> Result<String, BoxError> {
        let outcome = self.chat_turn(&ChatInput::text_only(prompt)).await?;
        match outcome.outcome {
            StepsOutcome::Done => Ok(outcome.text),
            StepsOutcome::Stopped | StepsOutcome::Working => Err(format!(
                "Batas {} langkah tool tercapai. Ketik 'lanjut' untuk meneruskan.",
                self.max_steps
            )
            .into()),
        }
    }

    async fn chat_turn(&self, input: &ChatInput) -> Result<TurnOutcome, BoxError> {
        let (_cancel_tx, cancel_rx) = tokio::sync::watch::channel(false);
        self.chat_turn_cancellable(input, cancel_rx, None).await
    }

    fn clear_memory(&self) {
        let id = {
            let tabs = self.tabs.lock().unwrap();
            tabs.active().id.clone()
        };
        self.clear_memory_silent();
        println!("🧠 Memori Tab #{} ({}) telah dibersihkan.", id, self.name);
    }
}

fn default_system_instruction(tools: &ToolRegistry) -> String {
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

fn save_token_to_user_env(api_key: &str, base_url: Option<&str>, model: &str) {
    let Ok(home) = std::env::var("HOME") else {
        return;
    };
    let dir = std::path::PathBuf::from(home).join(".config/oopbot");
    if std::fs::create_dir_all(&dir).is_err() {
        return;
    }
    let env_path = dir.join(".env");
    let mut content = String::new();
    let key = api_key.trim();
    let url_lower = base_url.unwrap_or("").to_lowercase();

    if key.starts_with("sk-or-") || url_lower.contains("openrouter.ai") {
        content.push_str(&format!("OPENROUTER_API_KEY={}\n", key));
        if let Some(url) = base_url {
            content.push_str(&format!("OPENROUTER_BASE_URL={}\n", url));
        }
        content.push_str(&format!("OOPBOT_MODEL={}\n", model));
    } else if url_lower.contains("deepseek.com") {
        content.push_str(&format!("DEEPSEEK_API_KEY={}\n", key));
        if let Some(url) = base_url {
            content.push_str(&format!("DEEPSEEK_BASE_URL={}\n", url));
        }
        content.push_str(&format!("OOPBOT_MODEL={}\n", model));
    } else if url_lower.contains("anthropic.com") || key.starts_with("sk-ant-") {
        content.push_str(&format!("ANTHROPIC_API_KEY={}\n", key));
        if let Some(url) = base_url {
            content.push_str(&format!("ANTHROPIC_BASE_URL={}\n", url));
        }
        content.push_str(&format!("OOPBOT_MODEL={}\n", model));
    } else if url_lower.contains("openai.com") {
        content.push_str(&format!("OPENAI_API_KEY={}\n", key));
        if let Some(url) = base_url {
            content.push_str(&format!("OPENAI_BASE_URL={}\n", url));
        }
        content.push_str(&format!("OOPBOT_MODEL={}\n", model));
    } else if url_lower.contains("googleapis.com") || key.starts_with("AIzaSy") {
        content.push_str(&format!("GEMINI_API_KEY={}\nGEMINI_MODEL={}\n", key, model));
        if let Some(url) = base_url {
            content.push_str(&format!("GEMINI_BASE_URL={}\n", url));
        }
    } else {
        content.push_str(&format!("OOPBOT_API_KEY={}\n", key));
        if let Some(url) = base_url {
            content.push_str(&format!("OOPBOT_BASE_URL={}\n", url));
        }
        content.push_str(&format!("OOPBOT_MODEL={}\n", model));
    }
    let _ = std::fs::write(&env_path, content);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn chat_input_text_only_constructs_correctly() {
        let input = ChatInput::text_only("halo dunia");
        assert_eq!(input.text, "halo dunia");
        assert!(input.images.is_empty());
    }

    #[test]
    fn chat_input_with_images_constructs_correctly() {
        let images = vec![("image/png".to_string(), "base64data".to_string())];
        let input = ChatInput::with_images("analisis gambar ini", images.clone());
        assert_eq!(input.text, "analisis gambar ini");
        assert_eq!(input.images.len(), 1);
        assert_eq!(input.images[0].0, "image/png");
        assert_eq!(input.images[0].1, "base64data");
    }

    #[tokio::test]
    async fn cli_approver_auto_approve() {
        let approver = CliApprover::new(true);
        assert!(approver.approve("run_shell", "$ ls -la").await);
        assert!(approver.approve("run_shell", "$ rm -rf /").await);
    }

    #[test]
    fn turn_outcome_aggregates_usage_and_steps() {
        let outcome = TurnOutcome {
            text: "Jawaban asisten".to_string(),
            events: vec![],
            steps: 2,
            outcome: StepsOutcome::Done,
            usage: Some(TokenUsage {
                input_tokens: 150,
                output_tokens: 80,
            }),
            last_usage: None,
            aborted: false,
        };

        assert_eq!(outcome.steps, 2);
        assert_eq!(outcome.outcome, StepsOutcome::Done);
        assert_eq!(outcome.usage.unwrap().input_tokens, 150);
        assert_eq!(outcome.usage.unwrap().output_tokens, 80);
        assert!(!outcome.aborted);
    }
}

