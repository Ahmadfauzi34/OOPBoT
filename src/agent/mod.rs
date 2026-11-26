//! Core agent harness (headless): loop "model → tool call → hasil tool → model" dengan:
//! - Multi-tab ([`WorkspaceTabs`]) & pemilih model (`ModelCatalog`) ala OOPBoT
//! - Event stream terstruktur ([`EventSink`] / [`AgentEvent`]) yang otomatis dilipat
//!   ke [`TimelineEntry::StepsFold`]
//! - Kebijakan konfirmasi ([`Approver`]) yang dapat diganti per-interface (CLI / RPC / FFI)
//!
//! Modul ini sengaja **headless** (bebas I/O terminal `stdin`/`stdout`) agar bersih dipakai
//! oleh `interfaces/cli`, `interfaces/rpc`, dan `interfaces/ffi`.

pub mod events;
mod runner;
pub mod session;

#[cfg(test)]
mod tests;

pub use events::{
    AgentEvent, EventSink, MemoryEventCollector, NoopEventSink, StepRecord, StepsOutcome,
    TimelineEntry, ToolStatus,
};
pub use session::{ChatSession, WorkspaceTabs};

use crate::common::BoxError;
use crate::config::store::save_token_to_user_env;
use crate::config::{ModelCatalog, ModelInfo};
use crate::context::{
    BudgetSnapshot, ContextManager, ContextPolicy, ContextSummarizer, PreparedContext,
};
use crate::llm::{user_message_with_images, user_text, LlmClient, TokenUsage};
use crate::tools::ToolRegistry;
use async_trait::async_trait;
use runner::default_system_instruction;
use serde_json::Value;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

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

pub struct AgentHarness {
    name: String,
    client: LlmClient,
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
    /// Aktifkan auto-compaction di dalam `runner.rs` (dipakai jalur CLI; RPC mengelola
    /// compaction tree JSONL-nya sendiri sehingga tetap `false`).
    auto_compact: bool,
    /// Error persistensi `tabs.json` terakhir (headless: dibaca & ditampilkan oleh interface).
    last_persist_error: Mutex<Option<String>>,
}

impl AgentHarness {
    /// Konstruktor headless default menggunakan [`NoopEventSink`].
    /// Untuk tampilan terminal interaktif, gunakan [`Self::with_event_sink`] dengan `CliEventSink`.
    pub fn new(
        name: &str,
        client: LlmClient,
        tools: ToolRegistry,
        approver: Box<dyn Approver>,
        max_steps: usize,
    ) -> Self {
        Self::with_event_sink(
            name,
            client,
            tools,
            approver,
            Box::new(NoopEventSink),
            max_steps,
        )
    }

    pub fn with_event_sink(
        name: &str,
        client: LlmClient,
        tools: ToolRegistry,
        approver: Box<dyn Approver>,
        event_sink: Box<dyn EventSink>,
        max_steps: usize,
    ) -> Self {
        let default_m = client.model();
        let storage_path = WorkspaceTabs::default_storage_path();
        let (loaded_tabs, load_warning) = storage_path
            .as_deref()
            .map(WorkspaceTabs::load_from_file_checked)
            .unwrap_or((None, None));
        let tabs = loaded_tabs.unwrap_or_else(|| WorkspaceTabs::new(&default_m));
        let harness = Self::with_event_sink_and_tabs(
            name,
            client,
            tools,
            approver,
            event_sink,
            max_steps,
            tabs,
            storage_path,
        );
        if let Some(warn) = load_warning {
            *harness
                .last_persist_error
                .lock()
                .unwrap_or_else(|e| e.into_inner()) = Some(warn);
        }
        harness
    }

    /// Varian konstruktor yang tab-nya disuplai pemanggil beserta lokasi
    /// persistensinya (`None` = tab hanya di memori, tidak ditulis ke disk).
    /// Dipakai jalur RPC: tab di-seed dari file sesi JSONL dan store-nya tetap
    /// file sesi itu, bukan `tabs.json` milik CLI.
    #[allow(clippy::too_many_arguments)]
    pub fn with_event_sink_and_tabs(
        name: &str,
        client: LlmClient,
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
            auto_compact: false,
            last_persist_error: Mutex::new(None),
        }
    }

    /// Model yang sedang aktif di tab saat ini.
    pub fn model(&self) -> String {
        self.tabs
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .active()
            .model
            .clone()
    }

    /// Info lengkap model aktif saat ini.
    pub fn active_model_info(&self) -> ModelInfo {
        let active_id = self.model();
        let cat = self.catalog.lock().unwrap_or_else(|e| e.into_inner());
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
        self.thinking_level
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    pub fn set_thinking_level(&self, level: &str) -> String {
        let clamped = self.active_model_info().clamp_thinking_level(level.trim());
        *self
            .thinking_level
            .lock()
            .unwrap_or_else(|e| e.into_inner()) = clamped.clone();
        clamped
    }

    /// Ganti system instruction (jalur RPC memakai instruksinya sendiri,
    /// bukan default CLI yang dibangun di konstruktor).
    pub fn set_system_instruction(&mut self, instruction: String) {
        if self.system_instruction != instruction {
            self.system_instruction = instruction;
            let decls = self.tools.declarations();
            let mut tabs = self.tabs.lock().unwrap_or_else(|e| e.into_inner());
            for tab in tabs.list_mut() {
                let expected = self.context_observation_fingerprint(&tab.model, &decls);
                if tab.last_context_fingerprint.as_deref() != Some(expected.as_str()) {
                    tab.clear_observed_context_tokens();
                }
            }
        }
    }

    pub fn system_instruction(&self) -> &str {
        &self.system_instruction
    }

    /// Teruskan perubahan kredensial ke client (dipakai refresh_credentials RPC).
    pub fn update_credentials(&self, api_key: String, base_url: Option<String>) {
        let prev_base_url = self.client.base_url();
        self.client.update_credentials(api_key, base_url);
        if prev_base_url != self.client.base_url() {
            let mut tabs = self.tabs.lock().unwrap_or_else(|e| e.into_inner());
            for tab in tabs.list_mut() {
                tab.clear_observed_context_tokens();
            }
            self.persist_tabs(&tabs);
        }
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
        self.catalog
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .upsert_model(info);
    }

    /// Bersihkan histori + timeline tab aktif tanpa mencetak apa pun.
    pub fn clear_memory_silent(&self) {
        let mut tabs = self.tabs.lock().unwrap_or_else(|e| e.into_inner());
        tabs.active_mut().clear();
        self.persist_tabs(&tabs);
    }

    /// Terapkan konfigurasi override ke model di katalog (`maxTokens`, `contextWindow`, `reasoning`, `thinkingLevelMap`, dsb.).
    pub fn configure_model(&self, model_id: &str, override_json: &Value) -> ModelInfo {
        let mut cat = self.catalog.lock().unwrap_or_else(|e| e.into_inner());
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
            let mut cat = self.catalog.lock().unwrap_or_else(|e| e.into_inner());
            if let Some(info) = cat.find(query) {
                info.id.clone()
            } else {
                cat.register_custom(query, query);
                query.to_string()
            }
        };
        let mut tabs = self.tabs.lock().unwrap_or_else(|e| e.into_inner());
        let active = tabs.active_mut();
        if active.model != resolved {
            active.model = resolved.clone();
            // Token input observasi sebelumnya milik tokenizer model/provider lama; reset agar tidak menyesatkan.
            active.clear_observed_context_tokens();
        }
        self.persist_tabs(&tabs);
        self.client.set_default_model(&resolved);
        resolved
    }

    pub fn list_models(&self) -> Vec<ModelInfo> {
        self.catalog
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .list()
            .to_vec()
    }

    /// Scan daftar model dari server provider menggunakan token aktif,
    /// lalu perbarui `ModelCatalog`.
    pub async fn scan_models(&self) -> Result<Vec<ModelInfo>, BoxError> {
        let scanned = self.client.scan_models().await?;
        let mut cat = self.catalog.lock().unwrap_or_else(|e| e.into_inner());
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
        let clean_url = base_url
            .map(|u| u.trim().to_string())
            .filter(|u| !u.is_empty());
        self.update_credentials(clean_key.clone(), clean_url.clone());
        save_token_to_user_env(&clean_key, clean_url.as_deref(), &self.model());
        self.scan_models().await
    }

    pub fn list_tabs(&self) -> (usize, Vec<ChatSession>) {
        let tabs = self.tabs.lock().unwrap_or_else(|e| e.into_inner());
        (tabs.active_index(), tabs.list().to_vec())
    }

    pub fn active_tab_summary(&self) -> (String, String, String) {
        let tabs = self.tabs.lock().unwrap_or_else(|e| e.into_inner());
        let t = tabs.active();
        (t.id.clone(), t.title.clone(), t.model.clone())
    }

    pub fn new_tab(&self, model: Option<&str>) -> ChatSession {
        let model_id = model.map(str::to_string).unwrap_or_else(|| self.model());
        let mut tabs = self.tabs.lock().unwrap_or_else(|e| e.into_inner());
        let created = tabs.new_tab(&model_id).clone();
        self.persist_tabs(&tabs);
        self.client.set_default_model(&created.model);
        created
    }

    pub fn switch_tab(&self, selector: &str) -> Option<ChatSession> {
        let mut tabs = self.tabs.lock().unwrap_or_else(|e| e.into_inner());
        let switched = tabs.switch_to(selector).cloned();
        if let Some(ref s) = switched {
            self.persist_tabs(&tabs);
            self.client.set_default_model(&s.model);
        }
        switched
    }

    pub fn close_tab(&self, selector: &str) -> bool {
        let fallback = self.client.model();
        let mut tabs = self.tabs.lock().unwrap_or_else(|e| e.into_inner());
        let ok = tabs.close_tab(selector, &fallback);
        if ok {
            self.persist_tabs(&tabs);
            self.client.set_default_model(&tabs.active().model);
        }
        ok
    }

    pub fn active_timeline(&self) -> Vec<TimelineEntry> {
        self.tabs
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .active()
            .timeline
            .clone()
    }

    pub fn active_history(&self) -> Vec<Value> {
        self.tabs
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .active()
            .history
            .clone()
    }

    pub fn replace_active_history(&self, id: &str, title: Option<&str>, history: Vec<Value>) {
        let mut tabs = self.tabs.lock().unwrap_or_else(|e| e.into_inner());
        let active = tabs.active_mut();
        active.id = id.to_string();
        active.title = match title {
            Some(t) if !t.trim().is_empty() => t.to_string(),
            _ => format!("Tab {}", id),
        };
        active.history = history;
        active.clear_observed_context_tokens();
        active.timeline.clear();
        self.persist_tabs(&tabs);
    }

    pub fn append_user_message_to_active_history(&self, input: &ChatInput) {
        let msg = if input.images.is_empty() {
            user_text(&input.text)
        } else {
            user_message_with_images(&input.text, &input.images)
        };
        let mut tabs = self.tabs.lock().unwrap_or_else(|e| e.into_inner());
        let active = tabs.active_mut();
        active.update_title_from_prompt(&input.text);
        active.history.push(msg);
        self.persist_tabs(&tabs);
    }

    pub fn active_history_len(&self) -> usize {
        self.tabs
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .active()
            .history
            .len()
    }

    pub fn tool_names(&self) -> Vec<&'static str> {
        self.tools.names()
    }

    pub fn set_auto_compact(&mut self, enabled: bool) {
        self.auto_compact = enabled;
    }

    pub fn auto_compact_enabled(&self) -> bool {
        self.auto_compact
    }

    /// Ambil error persistensi `tabs.json` terakhir (jika ada) untuk ditampilkan interface.
    pub fn take_persist_error(&self) -> Option<String> {
        self.last_persist_error
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .take()
    }

    /// Fingerprint deterministik untuk mengikat `last_context_tokens` ke kombinasi aktif
    /// `(model, base_url, system_instruction, tool_declarations)`.
    pub(super) fn context_observation_fingerprint(&self, model: &str, decls: &[Value]) -> String {
        fn fnv1a_update(mut hash: u64, bytes: &[u8]) -> u64 {
            for &b in bytes {
                hash ^= b as u64;
                hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
            }
            hash
        }
        let mut h: u64 = 0xcbf2_9ce4_8422_2325;
        h = fnv1a_update(h, model.as_bytes());
        h = fnv1a_update(h, &[0]);
        h = fnv1a_update(h, self.client.base_url().as_bytes());
        h = fnv1a_update(h, &[0]);
        h = fnv1a_update(h, self.system_instruction.as_bytes());
        h = fnv1a_update(h, &[0]);
        let decls_json = serde_json::to_string(decls).unwrap_or_default();
        h = fnv1a_update(h, decls_json.as_bytes());
        format!("{:016x}", h)
    }

    /// Sinkronkan default model `LlmClient` dan katalog ke model tab aktif yang dimuat dari `tabs.json`
    /// tanpa menimpa model tab tersimpan atau mereset `last_context_tokens`.
    pub fn sync_client_to_active_tab_model(&self) -> String {
        let active_model = self.model();
        {
            let mut cat = self.catalog.lock().unwrap_or_else(|e| e.into_inner());
            if cat.find(&active_model).is_none() {
                cat.register_custom(&active_model, &active_model);
            }
        }
        self.client.set_default_model(&active_model);
        active_model
    }

    /// Ambil `(last_context_tokens, last_context_estimated_tokens)` hanya bila fingerprint
    /// observasinya masih cocok dengan konfigurasi aktif saat ini.
    pub(super) fn observed_context_pair_for(
        &self,
        session: &ChatSession,
        decls: &[Value],
    ) -> (Option<u64>, Option<u64>) {
        let Some(tokens) = session.last_context_tokens else {
            return (None, None);
        };
        let expected = self.context_observation_fingerprint(&session.model, decls);
        if session.last_context_fingerprint.as_deref() == Some(expected.as_str()) {
            (Some(tokens), session.last_context_estimated_tokens)
        } else {
            (None, None)
        }
    }

    /// Kalibrasi `last_observed_input_tokens` dengan menambahkan delta estimasi lokal
    /// dari pesan baru yang ditambahkan setelah respons provider terakhir.
    pub(super) fn calibrated_observed_tokens(
        policy: &ContextPolicy,
        system_instruction: &str,
        decls: &[Value],
        history: &[Value],
        observed: Option<u64>,
        baseline_estimated: Option<u64>,
    ) -> Option<u64> {
        let obs = observed?;
        let delta = baseline_estimated
            .map(|base| {
                crate::context::evaluate_budget(policy, system_instruction, decls, history, None)
                    .total_estimated_tokens
                    .saturating_sub(base)
            })
            .unwrap_or(0);
        Some(obs.saturating_add(delta))
    }

    /// Kebijakan konteks berdasarkan limit & kapabilitas input model aktif saat ini.
    pub fn context_policy(&self) -> ContextPolicy {
        let info = self.active_model_info();
        let supports_images = info.input.iter().any(|m| m == "image");
        ContextPolicy::for_model_limits(info.input_token_limit, info.output_token_limit)
            .with_supports_images(supports_images)
    }

    /// Evaluasi anggaran konteks tab aktif terhadap limit model aktif.
    pub fn context_budget(&self) -> BudgetSnapshot {
        let decls = self.tools.declarations();
        let policy = self.context_policy();
        let (history, observed, baseline) = {
            let tabs = self.tabs.lock().unwrap_or_else(|e| e.into_inner());
            let active = tabs.active();
            let (obs, base) = self.observed_context_pair_for(active, &decls);
            (active.history.clone(), obs, base)
        };
        let last_observed = Self::calibrated_observed_tokens(
            &policy,
            &self.system_instruction,
            &decls,
            &history,
            observed,
            baseline,
        );
        ContextManager::new(policy).evaluate(
            &self.system_instruction,
            &decls,
            &history,
            last_observed,
        )
    }

    /// Padatkan konteks tab aktif secara eksplisit (mis. perintah `/compact` di CLI)
    /// tanpa menghapus `timeline` visual user.
    pub async fn compact_active_tab(
        &self,
        custom_instructions: Option<&str>,
        cancel_rx: tokio::sync::watch::Receiver<bool>,
    ) -> Result<PreparedContext, BoxError> {
        let decls = self.tools.declarations();
        let policy = self.context_policy();
        let (history, observed, baseline) = {
            let tabs = self.tabs.lock().unwrap_or_else(|e| e.into_inner());
            let active = tabs.active();
            let (obs, base) = self.observed_context_pair_for(active, &decls);
            (active.history.clone(), obs, base)
        };
        let last_observed = Self::calibrated_observed_tokens(
            &policy,
            &self.system_instruction,
            &decls,
            &history,
            observed,
            baseline,
        );
        let manager = ContextManager::new(policy);
        let prepared = manager
            .prepare_or_compact(
                &self.system_instruction,
                &decls,
                &history,
                last_observed,
                true,
                custom_instructions,
                self,
                cancel_rx,
            )
            .await?;

        if prepared.compacted {
            let notice = format!(
                "🗜️ Konteks dipadatkan ({} pesan diringkas, {} dipertahankan; estimasi baru ~{} token)",
                prepared.summarized_messages,
                prepared.kept_messages,
                prepared.budget.total_estimated_tokens
            );
            let mut tabs = self.tabs.lock().unwrap_or_else(|e| e.into_inner());
            tabs.active_mut()
                .apply_compacted_history(prepared.history.clone(), notice);
            self.persist_tabs(&tabs);
        }

        Ok(prepared)
    }

    fn persist_tabs(&self, tabs: &WorkspaceTabs) {
        if let Some(path) = &self.tabs_path {
            let res = tabs.save_to_file(path);
            let mut err_slot = self
                .last_persist_error
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            match res {
                Ok(()) => *err_slot = None,
                Err(e) => *err_slot = Some(format!("{} ({})", path.display(), e)),
            }
        }
    }

    fn emit(&self, event: AgentEvent, collector: &MemoryEventCollector) {
        self.event_sink.on_event(&event);
        collector.on_event(&event);
    }
}

#[async_trait]
impl ContextSummarizer for AgentHarness {
    async fn summarize(
        &self,
        compaction_prompt: &str,
        cancel_rx: tokio::sync::watch::Receiver<bool>,
    ) -> Result<String, BoxError> {
        let (summary, _usage) = self
            .generate_compaction_summary_cancellable(compaction_prompt, cancel_rx)
            .await?;
        Ok(summary)
    }

    async fn summarize_with_images(
        &self,
        compaction_prompt: &str,
        inline_images: &[Value],
        cancel_rx: tokio::sync::watch::Receiver<bool>,
    ) -> Result<String, BoxError> {
        let (summary, _usage) = self
            .generate_compaction_summary_with_images_cancellable(
                compaction_prompt,
                inline_images,
                cancel_rx,
            )
            .await?;
        Ok(summary)
    }
}

#[async_trait]
impl Agent for AgentHarness {
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
        self.clear_memory_silent();
    }
}
