//! Mode RPC (`oopbot --mode rpc`) yang berkomunikasi lewat protokol JSONL
//! dengan aplikasi Android OOPBoT (`AgentProcess.kt` & `ChatTimeline.kt`).
//!
//! Menggunakan binary native Rust `oopbot` sebagai engine agent di Termux.

mod approver;
mod compaction;
mod dispatch;
mod events;
mod export_html;
mod providers;
mod runtime;
mod session_ops;
mod session_store;
mod slash;
mod turn;

#[cfg(test)]
mod tests;

pub use approver::{PendingApprovals, RpcApprover};
pub use events::RpcEventSink;
pub use runtime::{PendingUiRequests, QueueMode, RpcRuntime};
pub use session_store::{RpcUiState, TokenTotals};

use events::*;
use export_html::*;
use providers::*;
use runtime::*;
use session_store::*;
use turn::*;

use crate::common::BoxError;
use crate::tools::ToolRegistry;
use serde_json::Value;
use std::io::{self, BufRead, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicU64;
use std::sync::{Arc, Mutex, RwLock};

type OutputSink = Arc<Mutex<Box<dyn Write + Send + Sync>>>;
pub(super) static OUTPUT_SINK: RwLock<Option<OutputSink>> = RwLock::new(None);
pub(super) static SESSION_UNIQUE_SEQ: AtomicU64 = AtomicU64::new(1);

pub fn set_output_sink(sink: Option<OutputSink>) {
    if let Ok(mut lock) = OUTPUT_SINK.write() {
        *lock = sink;
    }
}

#[cfg(test)]
pub(crate) fn output_sink_test_mutex() -> &'static tokio::sync::Mutex<()> {
    static LOCK: std::sync::OnceLock<tokio::sync::Mutex<()>> = std::sync::OnceLock::new();
    LOCK.get_or_init(|| tokio::sync::Mutex::new(()))
}

pub struct RpcServer {
    runtime: Arc<RpcRuntime>,
}

impl std::ops::Deref for RpcServer {
    type Target = RpcRuntime;
    fn deref(&self) -> &Self::Target {
        &self.runtime
    }
}

impl RpcServer {
    pub fn resolve_default_config_dir() -> PathBuf {
        std::env::var("OOPBOT_CONFIG_DIR")
            .ok()
            .filter(|s| !s.trim().is_empty())
            .map(PathBuf::from)
            .unwrap_or_else(|| {
                let home = std::env::var("HOME").unwrap_or_else(|_| ".".into());
                PathBuf::from(home).join(".oopbot")
            })
    }

    pub fn new(
        session_path: Option<String>,
        system_append: Option<String>,
        extensions: Vec<String>,
    ) -> Result<Self, BoxError> {
        Self::new_in_dir(
            Self::resolve_default_config_dir(),
            session_path,
            system_append,
            extensions,
        )
    }

    pub fn new_in_dir(
        config_dir: PathBuf,
        session_path: Option<String>,
        system_append: Option<String>,
        extensions: Vec<String>,
    ) -> Result<Self, BoxError> {
        let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
        Self::new_in_dir_with_cwd(config_dir, cwd, session_path, system_append, extensions)
    }

    pub fn new_in_dir_with_cwd(
        config_dir: PathBuf,
        cwd: PathBuf,
        session_path: Option<String>,
        system_append: Option<String>,
        extensions: Vec<String>,
    ) -> Result<Self, BoxError> {
        let runtime = Arc::new(RpcRuntime::new_in_dir_with_cwd(
            config_dir,
            cwd,
            session_path,
            system_append,
            extensions,
        )?);
        runtime.install_plugin_callbacks();
        Ok(Self { runtime })
    }

    /// Kirim pesan dua arah pada OOPBoT channel: `{"type": "oopbot_channel", "topic": ..., "data": ...}`
    pub fn send_channel(topic: &str, data: Value) {
        RpcRuntime::send_channel(topic, data);
    }

    pub fn send_line(value: &Value) {
        RpcRuntime::send_line(value);
    }

    pub async fn run_loop(&mut self) -> Result<(), BoxError> {
        let stdin = io::stdin();
        let reader = io::BufReader::new(stdin);
        self.run_loop_with_input(reader).await
    }

    pub async fn run_loop_with_input<R: BufRead + Send + 'static>(
        &mut self,
        reader: R,
    ) -> Result<(), BoxError> {
        self.runtime.run_loop_with_input(reader).await
    }
}

pub async fn run_rpc_mode(
    session_path: Option<String>,
    system_append: Option<String>,
    extensions: Vec<String>,
) -> Result<(), BoxError> {
    let mut server = RpcServer::new(session_path, system_append, extensions)?;
    server.run_loop().await
}

pub(super) fn default_rpc_system_instruction(cwd: &Path, tools: &ToolRegistry) -> String {
    format!(
        "Kamu adalah OOPBoT, mesin agen Rust native yang terhubung ke antarmuka Android OOPBoT melalui Termux.\n\
         Direktori kerja (cwd): {}.\n\
         Tool yang tersedia: {}.\n\
         Gunakan tool untuk memeriksa isi file atau menjalankan perintah di Termux bila diperlukan.",
        cwd.display(),
        tools.names().join(", ")
    )
}
