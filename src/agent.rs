//! Agent harness: loop "model → tool call → hasil tool → model" sampai model
//! memberi jawaban akhir (tanpa function call) atau batas langkah tercapai.

use crate::gemini::{function_response_part, user_text, BoxError, FunctionCall, GeminiClient};
use crate::tools::{truncate, ToolRegistry};
use async_trait::async_trait;
use serde_json::{json, Value};
use std::io::{self, Write};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;

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
        print!("⚠️  Izinkan {} → {} ? [y/N/a=selalu] ", tool_name, preview);
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
    system_instruction: String,
    max_steps: usize,
    /// Riwayat percakapan dalam format `contents` Gemini (JSON mentah).
    memory: Mutex<Vec<Value>>,
}

impl GeminiAgent {
    pub fn new(
        name: &str,
        client: GeminiClient,
        tools: ToolRegistry,
        approver: Box<dyn Approver>,
        max_steps: usize,
    ) -> Self {
        let system_instruction = default_system_instruction(&tools);
        Self {
            name: name.to_string(),
            client,
            tools,
            approver,
            system_instruction,
            max_steps,
            memory: Mutex::new(Vec::new()),
        }
    }

    pub fn model(&self) -> &str {
        self.client.model()
    }

    pub fn tool_names(&self) -> Vec<&'static str> {
        self.tools.names()
    }

    /// Jalankan satu function call dan kembalikan objek `response` untuk model.
    async fn run_tool(&self, call: &FunctionCall) -> Value {
        let Some(tool) = self.tools.get(&call.name) else {
            return json!({ "error": format!("tool '{}' tidak dikenal", call.name) });
        };

        let preview = tool.preview(&call.args);
        if tool.requires_confirmation() && !self.approver.approve(tool.name(), &preview) {
            println!("   🚫 ditolak");
            return json!({ "error": "User menolak eksekusi tool ini. Tanyakan alternatif ke user." });
        }

        println!("🔧 {} {}", tool.name(), preview);
        match tool.execute(&call.args).await {
            Ok(result) => {
                print_tool_output(&result);
                result
            }
            Err(e) => {
                println!("   ❌ {}", e);
                json!({ "error": e })
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
        // Kerja di salinan riwayat; baru di-commit ke memori jika sukses,
        // sehingga error API otomatis "rollback" seperti versi sebelumnya.
        let mut history = self.memory.lock().unwrap().clone();
        history.push(user_text(prompt));
        let decls = self.tools.declarations();

        for _ in 0..self.max_steps {
            let turn = self
                .client
                .generate(&self.system_instruction, &history, &decls)
                .await?;
            history.push(turn.raw_content.clone());

            if turn.function_calls.is_empty() {
                *self.memory.lock().unwrap() = history;
                return Ok(turn.text);
            }

            if !turn.text.is_empty() {
                println!("💭 {}", turn.text);
            }

            let mut parts = Vec::with_capacity(turn.function_calls.len());
            for call in &turn.function_calls {
                let response = self.run_tool(call).await;
                parts.push(function_response_part(call, response));
            }
            history.push(json!({ "role": "user", "parts": parts }));
        }

        *self.memory.lock().unwrap() = history;
        Err(format!(
            "Batas {} langkah tool tercapai. Ketik 'lanjut' untuk meneruskan.",
            self.max_steps
        )
        .into())
    }

    fn clear_memory(&self) {
        self.memory.lock().unwrap().clear();
        println!("🧠 Memori {} telah dibersihkan.", self.name);
    }
}

fn print_tool_output(result: &Value) {
    for key in ["stdout", "stderr"] {
        if let Some(s) = result[key].as_str().filter(|s| !s.trim().is_empty()) {
            for line in truncate(s.trim_end(), 1500).lines() {
                println!("   │ {}", line);
            }
        }
    }
    if let Some(code) = result.get("exit_code") {
        println!("   └ exit {}", code);
    } else {
        println!("   └ ok");
    }
}

fn default_system_instruction(tools: &ToolRegistry) -> String {
    let cwd = std::env::current_dir()
        .map(|p| p.display().to_string())
        .unwrap_or_else(|_| "?".into());
    let prefix = std::env::var("PREFIX").unwrap_or_default();
    let is_termux = prefix.contains("com.termux");
    format!(
        "Kamu adalah OOPBoT, agen CLI yang berjalan langsung di perangkat user.\n\
         Lingkungan: {env}, OS {os}/{arch}, direktori kerja: {cwd}{prefix_info}.\n\
         Tool yang tersedia: {tools}.\n\
         Aturan:\n\
         - Gunakan tool untuk memeriksa fakta (isi file, output perintah) daripada menebak.\n\
         - Untuk tugas multi-langkah, kerjakan bertahap dan verifikasi hasilnya.\n\
         - Jangan menjalankan perintah destruktif (rm -rf, format, dsb.) tanpa alasan jelas.\n\
         - Jawab dalam Bahasa Indonesia secara ringkas, gunakan format teks yang enak dibaca di terminal.",
        env = if is_termux { "Termux di Android" } else { "terminal Linux/Unix" },
        os = std::env::consts::OS,
        arch = std::env::consts::ARCH,
        cwd = cwd,
        prefix_info = if is_termux { format!(", $PREFIX={}", prefix) } else { String::new() },
        tools = tools.names().join(", "),
    )
}
