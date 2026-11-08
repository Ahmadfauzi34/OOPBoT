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
    function_response_part, user_text, BoxError, FunctionCall, GeminiClient, GenerationOptions,
};
use crate::models::{ModelCatalog, ModelInfo};
use crate::session::{ChatSession, WorkspaceTabs};
use crate::tools::ToolRegistry;
use async_trait::async_trait;
use serde_json::{json, Value};
use std::io::{self, Write};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;
use std::time::Instant;

/// Interface agen (OOP).
#[async_trait]
pub trait Agent {
    fn get_name(&self) -> &str;
    async fn chat(&self, prompt: &str) -> Result<String, BoxError>;
    fn clear_memory(&self);
}

/// Kebijakan persetujuan sebelum tool "berbahaya" dijalankan.
pub trait Approver: Send + Sync {
    fn approve(&self, tool_name: &str, preview: &str) -> bool;
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

impl Approver for CliApprover {
    fn approve(&self, tool_name: &str, preview: &str) -> bool {
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
        let system_instruction = default_system_instruction(&tools);
        let mut catalog = ModelCatalog::builtin();
        catalog.set_default_model(client.model());
        let tabs = WorkspaceTabs::default_storage_path()
            .and_then(|p| WorkspaceTabs::load_from_file(&p))
            .unwrap_or_else(|| WorkspaceTabs::new(client.model()));

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
        created
    }

    pub fn switch_tab(&self, selector: &str) -> Option<ChatSession> {
        let mut tabs = self.tabs.lock().unwrap();
        let switched = tabs.switch_to(selector).cloned();
        if switched.is_some() {
            self.persist_tabs(&tabs);
        }
        switched
    }

    pub fn close_tab(&self, selector: &str) -> bool {
        let fallback = self.client.model().to_string();
        let mut tabs = self.tabs.lock().unwrap();
        let ok = tabs.close_tab(selector, &fallback);
        if ok {
            self.persist_tabs(&tabs);
        }
        ok
    }

    pub fn active_timeline(&self) -> Vec<TimelineEntry> {
        self.tabs.lock().unwrap().active().timeline.clone()
    }

    pub fn tool_names(&self) -> Vec<&'static str> {
        self.tools.names()
    }

    fn persist_tabs(&self, tabs: &WorkspaceTabs) {
        if let Some(path) = WorkspaceTabs::default_storage_path() {
            let _ = tabs.save_to_file(&path);
        }
    }

    fn emit(&self, event: AgentEvent, collector: &MemoryEventCollector) {
        self.event_sink.on_event(&event);
        collector.on_event(&event);
    }

    /// Jalankan satu function call dan pancarkan event `ToolCallStart` + `ToolCallEnd`.
    async fn run_tool(
        &self,
        step_index: usize,
        call: &FunctionCall,
        collector: &MemoryEventCollector,
    ) -> Value {
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
            return err_val;
        };

        let preview = tool.preview(&call.args);
        let requires_confirmation = tool.requires_confirmation();

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

        if requires_confirmation && !self.approver.approve(tool.name(), &preview) {
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
            return rejected;
        }

        let started = Instant::now();
        match tool.execute(&call.args).await {
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
                result
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
                err_val
            }
        }
    }
}

#[async_trait]
impl Agent for GeminiAgent {
    fn get_name(&self) -> &str {
        &self.name
    }

    async fn chat(&self, prompt: &str) -> Result<String, BoxError> {
        let (session_id, active_model, mut history) = {
            let tabs = self.tabs.lock().unwrap();
            let s = tabs.active();
            (s.id.clone(), s.model.clone(), s.history.clone())
        };

        history.push(user_text(prompt));
        let decls = self.tools.declarations();
        let collector = MemoryEventCollector::new();
        let turn_started = Instant::now();
        let mut tool_steps = 0usize;

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
            let turn = self
                .client
                .generate_with_options(
                    &active_model,
                    &self.system_instruction,
                    &history,
                    &decls,
                    &gen_options,
                )
                .await?;
            history.push(turn.raw_content.clone());

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

                let folded = MemoryEventCollector::fold_turn(prompt, &collector.take_events());
                let mut tabs = self.tabs.lock().unwrap();
                let active = tabs.active_mut();
                active.update_title_from_prompt(prompt);
                active.history = history;
                active.timeline.extend(folded);
                self.persist_tabs(&tabs);

                return Ok(turn.text);
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
            for call in &turn.function_calls {
                tool_steps += 1;
                let response = self.run_tool(step_idx, call, &collector).await;
                parts.push(function_response_part(call, response));
            }
            history.push(json!({ "role": "user", "parts": parts }));
        }

        self.emit(
            AgentEvent::TurnEnd {
                steps_count: tool_steps,
                duration_ms: turn_started.elapsed().as_millis() as u64,
                outcome: StepsOutcome::Stopped,
            },
            &collector,
        );
        let folded = MemoryEventCollector::fold_turn(prompt, &collector.take_events());
        let mut tabs = self.tabs.lock().unwrap();
        let active = tabs.active_mut();
        active.update_title_from_prompt(prompt);
        active.history = history;
        active.timeline.extend(folded);
        self.persist_tabs(&tabs);

        Err(format!(
            "Batas {} langkah tool tercapai. Ketik 'lanjut' untuk meneruskan.",
            self.max_steps
        )
        .into())
    }

    fn clear_memory(&self) {
        let mut tabs = self.tabs.lock().unwrap();
        let id = tabs.active().id.clone();
        tabs.active_mut().clear();
        self.persist_tabs(&tabs);
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
    let mut content = format!("GEMINI_API_KEY={}\nGEMINI_MODEL={}\n", api_key, model);
    if let Some(url) = base_url {
        content.push_str(&format!("GEMINI_BASE_URL={}\n", url));
    }
    let _ = std::fs::write(&env_path, content);
}
