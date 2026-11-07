//! Event stream dan model timeline ala Pipkin (`TimelineModels.kt` & `StepStream.kt`).
//!
//! Desain OOP berbasis trait [`EventSink`] memisahkan *mesin loop agen* dari
//! tampilan:
//! - Di terminal Termux, [`CliEventSink`] merender langkah-langkah secara ringkas.
//! - Di Android (lewat `liboopbot.so`), `JsonLineEventSink` atau callback JNI
//!   mengirim setiap [`AgentEvent`] sebagai JSON untuk dirender menjadi
//!   lipatan `Steps` (Thinking + ToolCall) dan kartu `AssistantText`.

use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::sync::Mutex;

/// Status eksekusi satu tool call di timeline.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolStatus {
    PendingApproval,
    Running,
    Done,
    Rejected,
    Error,
}

/// Hasil akhir satu grup langkah (`Steps` fold di Pipkin).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StepsOutcome {
    Working,
    Done,
    Stopped,
}

/// Event real-time yang dipancarkan oleh loop agen selama satu giliran (`turn`).
/// Format JSON `{"type": "...", ...}` sengaja dibuat kompatibel untuk dikonsumsi UI.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum AgentEvent {
    /// Giliran baru dimulai untuk sesi/tab tertentu.
    TurnStart {
        session_id: String,
        model: String,
    },
    /// Blok proses berpikir model (bisa dilipat ke dalam `Steps`).
    Thinking {
        step_index: usize,
        text: String,
    },
    /// Model meminta eksekusi tool.
    ToolCallStart {
        step_index: usize,
        call_id: String,
        tool_name: String,
        args: Value,
        preview: String,
        requires_confirmation: bool,
    },
    /// Tool selesai (atau ditolak/error).
    ToolCallEnd {
        step_index: usize,
        call_id: String,
        tool_name: String,
        status: ToolStatus,
        output: Value,
        duration_ms: u64,
    },
    /// Jawaban teks akhir dari asisten.
    AssistantText {
        text: String,
    },
    /// Giliran selesai beserta total langkah dan durasi.
    TurnEnd {
        steps_count: usize,
        duration_ms: u64,
        outcome: StepsOutcome,
    },
}

/// Satu item pada timeline percakapan setelah langkah-langkah berurutan dilipat
/// (setara dengan `StepStream` di Pipkin).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum TimelineEntry {
    User {
        text: String,
    },
    /// Lipatan dari satu atau lebih langkah `Thinking` / `ToolCall` berurutan.
    StepsFold {
        outcome: StepsOutcome,
        steps: Vec<StepRecord>,
    },
    Assistant {
        text: String,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "step_type", rename_all = "snake_case")]
pub enum StepRecord {
    Thinking {
        text: String,
    },
    ToolCall {
        call_id: String,
        tool_name: String,
        preview: String,
        status: ToolStatus,
        duration_ms: u64,
    },
}

/// Interface OOP untuk konsumen event loop agen (UI Terminal, JSON-RPC, atau JNI Android).
pub trait EventSink: Send + Sync {
    fn on_event(&self, event: &AgentEvent);
}

/// Sink bawaan untuk terminal CLI (menampilkan proses kerja agen secara rapi).
pub struct CliEventSink;

impl EventSink for CliEventSink {
    fn on_event(&self, event: &AgentEvent) {
        match event {
            AgentEvent::TurnStart { model, .. } => {
                println!("┌─ 🤖 Turn dimulai (model: {})", model);
            }
            AgentEvent::Thinking { step_index, text } => {
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

/// Kolektor event di memori (dipakai oleh FFI `.so` dan pengujian untuk membangun
/// lipatan `TimelineEntry::StepsFold` seperti `StepStream.kt` di Pipkin).
#[derive(Default)]
pub struct MemoryEventCollector {
    events: Mutex<Vec<AgentEvent>>,
}

impl MemoryEventCollector {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn take_events(&self) -> Vec<AgentEvent> {
        std::mem::take(&mut *self.events.lock().unwrap())
    }

    /// Lipat deretan event menjadi `TimelineEntry` (menggabungkan `Thinking` dan
    /// `ToolCallEnd` berurutan ke dalam satu `StepsFold`).
    pub fn fold_turn(user_prompt: &str, events: &[AgentEvent]) -> Vec<TimelineEntry> {
        let mut out = vec![TimelineEntry::User {
            text: user_prompt.to_string(),
        }];
        let mut steps = Vec::new();
        let mut previews = std::collections::HashMap::<String, String>::new();
        let mut final_outcome = StepsOutcome::Done;
        let mut reply_text = None;

        for ev in events {
            match ev {
                AgentEvent::Thinking { text, .. } if !text.is_empty() => {
                    steps.push(StepRecord::Thinking { text: text.clone() });
                }
                AgentEvent::ToolCallStart {
                    call_id, preview, ..
                } => {
                    previews.insert(call_id.clone(), preview.clone());
                }
                AgentEvent::ToolCallEnd {
                    call_id,
                    tool_name,
                    status,
                    duration_ms,
                    ..
                } => {
                    let preview = previews.remove(call_id).unwrap_or_default();
                    steps.push(StepRecord::ToolCall {
                        call_id: call_id.clone(),
                        tool_name: tool_name.clone(),
                        preview,
                        status: *status,
                        duration_ms: *duration_ms,
                    });
                }
                AgentEvent::AssistantText { text } => {
                    reply_text = Some(text.clone());
                }
                AgentEvent::TurnEnd { outcome, .. } => {
                    final_outcome = *outcome;
                }
                _ => {}
            }
        }

        if !steps.is_empty() {
            out.push(TimelineEntry::StepsFold {
                outcome: final_outcome,
                steps,
            });
        }
        if let Some(text) = reply_text {
            out.push(TimelineEntry::Assistant { text });
        }
        out
    }
}

impl EventSink for MemoryEventCollector {
    fn on_event(&self, event: &AgentEvent) {
        self.events.lock().unwrap().push(event.clone());
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn folds_consecutive_steps_like_pipkin_step_stream() {
        let collector = MemoryEventCollector::new();
        collector.on_event(&AgentEvent::TurnStart {
            session_id: "tab-1".into(),
            model: "gemini-2.5-flash".into(),
        });
        collector.on_event(&AgentEvent::Thinking {
            step_index: 1,
            text: "Cek direktori kerja dulu.".into(),
        });
        collector.on_event(&AgentEvent::ToolCallStart {
            step_index: 1,
            call_id: "c1".into(),
            tool_name: "list_dir".into(),
            args: json!({"path": "."}),
            preview: ".".into(),
            requires_confirmation: false,
        });
        collector.on_event(&AgentEvent::ToolCallEnd {
            step_index: 1,
            call_id: "c1".into(),
            tool_name: "list_dir".into(),
            status: ToolStatus::Done,
            output: json!({"entries": []}),
            duration_ms: 12,
        });
        collector.on_event(&AgentEvent::AssistantText {
            text: "Direktori kosong.".into(),
        });
        collector.on_event(&AgentEvent::TurnEnd {
            steps_count: 1,
            duration_ms: 50,
            outcome: StepsOutcome::Done,
        });

        let entries = MemoryEventCollector::fold_turn("cek", &collector.take_events());
        assert_eq!(entries.len(), 3); // User + StepsFold(2 steps) + Assistant
        match &entries[1] {
            TimelineEntry::StepsFold { outcome, steps } => {
                assert_eq!(*outcome, StepsOutcome::Done);
                assert_eq!(steps.len(), 2);
            }
            _ => panic!("expected StepsFold"),
        }
    }
}
