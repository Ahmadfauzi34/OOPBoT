//! Komponen UI terminal untuk CLI: [`CliApprover`], [`CliEventSink`], dan helper render.

use crate::agent::{
    AgentEvent, AgentHarness, Approver, EventSink, StepRecord, TimelineEntry, ToolStatus,
};
use async_trait::async_trait;
use std::io::{self, Write};
use std::sync::atomic::{AtomicBool, Ordering};

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

/// Sink bawaan untuk terminal CLI (menampilkan proses kerja agen secara rapi).
pub struct CliEventSink;

impl EventSink for CliEventSink {
    fn on_event(&self, event: &AgentEvent) {
        match event {
            AgentEvent::TurnStart { model, .. } => {
                println!("┌─ 🤖 Turn dimulai (model: {})", model);
            }
            AgentEvent::Thinking {
                step_index, text, ..
            } => {
                if !text.is_empty() {
                    println!("├─ 💭 [step {}] {}", step_index, text);
                }
            }
            AgentEvent::ToolCallStart {
                step_index,
                tool_name,
                preview,
                ..
            } => {
                println!("├─ 🔧 [step {}] {} → {}", step_index, tool_name, preview);
            }
            AgentEvent::ToolCallEnd {
                status,
                output,
                duration_ms,
                ..
            } => match status {
                ToolStatus::Done => {
                    for key in ["stdout", "stderr"] {
                        if let Some(s) = output[key].as_str().filter(|s| !s.trim().is_empty()) {
                            for line in crate::tools::truncate(s.trim_end(), 1200).lines() {
                                println!("│  │ {}", line);
                            }
                        }
                    }
                    if let Some(code) = output.get("exit_code") {
                        println!("│  └─ exit {} ({} ms)", code, duration_ms);
                    } else {
                        println!("│  └─ ok ({} ms)", duration_ms);
                    }
                }
                ToolStatus::Rejected => {
                    println!("│  └─ 🚫 ditolak user");
                }
                ToolStatus::Error => {
                    let err = output["error"].as_str().unwrap_or("error");
                    println!("│  └─ ❌ {} ({} ms)", err, duration_ms);
                }
                _ => {}
            },
            AgentEvent::AssistantText { .. } => {}
            AgentEvent::TurnEnd {
                steps_count,
                duration_ms,
                outcome,
            } => {
                println!(
                    "└─ ✅ {:?} ({} langkah tool, {} ms)",
                    outcome, steps_count, duration_ms
                );
            }
        }
    }
}

pub fn render_models(agent: &AgentHarness) {
    let active = agent.model();
    println!("\n📦 Daftar Model (pilih dengan `/model <nomor|id>`, atau `/scan` untuk refresh):");
    for (idx, m) in agent.list_models().iter().enumerate() {
        let mark = if m.id == active { "●" } else { " " };
        let ctx = m
            .input_token_limit
            .map(|n| format!(" [ctx: {}k]", n / 1000))
            .unwrap_or_default();
        println!(
            "  {} [{:>2}] {}{} ({})",
            mark,
            idx + 1,
            m.id,
            ctx,
            m.label
        );
    }
    println!();
}

pub fn render_tabs(agent: &AgentHarness) {
    let (active_idx, tabs) = agent.list_tabs();
    println!("\n🗂️  Daftar Tab Sesi (`/new`, `/tab <no>`, `/close <no>`):");
    for (idx, t) in tabs.iter().enumerate() {
        let mark = if idx == active_idx { "●" } else { " " };
        println!(
            "  {} [{}] #{} \"{}\" | model: {} | {} item timeline",
            mark,
            idx + 1,
            t.id,
            t.title,
            t.model,
            t.timeline.len()
        );
    }
    println!();
}

pub fn render_timeline_steps(agent: &AgentHarness) {
    let entries = agent.active_timeline();
    if entries.is_empty() {
        println!("\n📭 Belum ada langkah di tab ini.\n");
        return;
    }
    println!("\n📜 Timeline Tab Aktif:");
    for entry in entries {
        match entry {
            TimelineEntry::User { text } => println!("  👤 User: {}", text),
            TimelineEntry::StepsFold { outcome, steps } => {
                println!("  ▸ Steps ({:?}, {} langkah):", outcome, steps.len());
                for s in steps {
                    match s {
                        StepRecord::Thinking { text } => println!("      💭 {}", text),
                        StepRecord::ToolCall {
                            tool_name,
                            preview,
                            status,
                            duration_ms,
                            ..
                        } => println!(
                            "      🔧 {} ({}) → {:?} [{} ms]",
                            tool_name, preview, status, duration_ms
                        ),
                    }
                }
            }
            TimelineEntry::Assistant { text } => {
                let first_line = text.lines().next().unwrap_or("");
                println!("  🤖 Asisten: {}", first_line);
            }
            TimelineEntry::Notice { text } => {
                println!("  ℹ️  {}", text);
            }
        }
    }
    println!();
}

pub fn render_context_budget(agent: &AgentHarness) {
    let snap = agent.context_budget();
    let status_label = match snap.status {
        crate::context::BudgetStatus::WithinBudget => "OK",
        crate::context::BudgetStatus::NeedsCompaction => "Perlu Compaction",
        crate::context::BudgetStatus::OverLimit => "Melebihi Batas",
    };
    let auto_state = if agent.auto_compact_enabled() {
        "aktif"
    } else {
        "nonaktif"
    };
    println!(
        "\n📊 Anggaran Konteks Tab Aktif (model: {} | auto-compact: {}):",
        agent.model(),
        auto_state
    );
    println!(
        "  • System prompt : ~{} token",
        snap.system_tokens
    );
    println!(
        "  • Skema tool    : ~{} token",
        snap.tool_schema_tokens
    );
    println!(
        "  • Histori pesan : ~{} token ({} entri)",
        snap.history_tokens,
        agent.active_history_len()
    );
    if let Some(obs) = snap.last_observed_input_tokens {
        println!("  • Observasi LLM : {} token input terakhir", obs);
    }
    println!(
        "  • Efektif       : {} / {} usable token ({:.1}% | max window: {}) → [{}]\n",
        snap.effective_tokens(),
        snap.usable_limit,
        snap.usage_percent(),
        snap.max_input_tokens,
        status_label
    );
}

pub fn warn_if_persist_failed(agent: &AgentHarness) {
    if let Some(err) = agent.take_persist_error() {
        eprintln!("⚠️  Gagal menyimpan state tab ke disk: {}", err);
    }
}

pub fn print_error_chain(err: &dyn std::error::Error) {
    let mut msg = err.to_string();
    let mut src = err.source();
    while let Some(s) = src {
        msg.push_str(&format!("\n   ↳ {}", s));
        src = s.source();
    }
    eprintln!("\n❌ Terjadi error: {}\n", msg);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn cli_approver_auto_approve() {
        let approver = CliApprover::new(true);
        assert!(approver.approve("run_shell", "$ ls -la").await);
        assert!(approver.approve("run_shell", "$ rm -rf /").await);
    }
}
