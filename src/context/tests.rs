use super::*;
use crate::common::BoxError;
use async_trait::async_trait;
use serde_json::json;
use std::sync::atomic::{AtomicUsize, Ordering};

struct MockSummarizer {
    calls: AtomicUsize,
    reply: String,
}

impl MockSummarizer {
    fn new(reply: &str) -> Self {
        Self {
            calls: AtomicUsize::new(0),
            reply: reply.to_string(),
        }
    }
}

#[async_trait]
impl ContextSummarizer for MockSummarizer {
    async fn summarize(
        &self,
        _compaction_prompt: &str,
        cancel_rx: tokio::sync::watch::Receiver<bool>,
    ) -> Result<String, BoxError> {
        if *cancel_rx.borrow() {
            return Err("aborted".into());
        }
        self.calls.fetch_add(1, Ordering::SeqCst);
        Ok(self.reply.clone())
    }
}

fn u_msg(text: &str) -> Value {
    json!({ "role": "user", "parts": [{ "text": text }] })
}

fn m_msg(text: &str) -> Value {
    json!({ "role": "model", "parts": [{ "text": text }] })
}

fn m_tool_call(name: &str, args: Value) -> Value {
    json!({
        "role": "model",
        "parts": [{ "functionCall": { "id": "c1", "name": name, "args": args } }]
    })
}

fn u_tool_resp(name: &str, resp: Value) -> Value {
    json!({
        "role": "user",
        "parts": [{ "functionResponse": { "id": "c1", "name": name, "response": resp } }]
    })
}

#[test]
fn budget_evaluates_thresholds_and_calibrates_with_observed_tokens() {
    let policy = ContextPolicy {
        max_input_tokens: 1_000,
        reserve_output_tokens: 200,
        auto_compact_ratio: 0.80, // usable=800, threshold=640
        keep_recent_tokens: 200,
    };
    let small_history = vec![u_msg("halo"), m_msg("hai")];

    let ok = evaluate_budget(&policy, "sys", &[], &small_history, None);
    assert_eq!(ok.status, BudgetStatus::WithinBudget);
    assert_eq!(ok.usable_limit, 800);

    // Observed input tokens from provider override low local estimate
    let needs = evaluate_budget(&policy, "sys", &[], &small_history, Some(650));
    assert_eq!(needs.status, BudgetStatus::NeedsCompaction);
    assert_eq!(needs.effective_tokens(), 650);

    let over = evaluate_budget(&policy, "sys", &[], &small_history, Some(850));
    assert_eq!(over.status, BudgetStatus::OverLimit);
}

#[test]
fn truncate_head_tail_preserves_utf8_and_both_ends() {
    let s = format!("HEAD_🚀_{}_TAIL_✅", "x".repeat(500));
    let t = truncate_head_tail_chars(&s, 60);
    assert!(t.starts_with("HEAD_🚀_"));
    assert!(t.ends_with("_TAIL_✅"));
    assert!(t.contains("dipadatkan untuk compaction"));
}

#[test]
fn compaction_cut_point_never_splits_tool_call_and_function_response_pair() {
    let history = vec![
        u_msg(&"turn 1 ".repeat(120)),
        m_tool_call("read_file", json!({ "path": "a.rs" })),
        u_tool_resp("read_file", json!({ "content": "a".repeat(800) })),
        m_msg("done 1"),
        // Safe cut point at index 4:
        u_msg("turn 2: edit b.rs"),
        m_tool_call("edit_file", json!({ "path": "b.rs" })),
        u_tool_resp("edit_file", json!({ "edits_applied": 1 })),
        m_msg("done 2"),
    ];

    assert!(!is_valid_canonical_cut_point(&history, 0));
    assert!(!is_valid_canonical_cut_point(&history, 1));
    assert!(!is_valid_canonical_cut_point(&history, 2)); // functionResponse MUST NOT be cut point
    assert!(!is_valid_canonical_cut_point(&history, 3));
    assert!(is_valid_canonical_cut_point(&history, 4)); // regular user turn IS valid
    assert!(!is_valid_canonical_cut_point(&history, 6)); // functionResponse MUST NOT be cut point

    let plan = plan_canonical_compaction(&history, 60, None, None).expect("plan");
    assert_eq!(plan.cut_index, 4);
    assert_eq!(plan.summarized_messages, 4);
    assert_eq!(plan.kept_messages, 4);
    assert!(!is_function_response_message(&history[plan.cut_index]));
}

#[test]
fn rebuild_canonical_history_maintains_user_model_alternation_and_chains_summary() {
    let kept = vec![u_msg("lanjutkan step 2"), m_msg("siap")];
    let rebuilt = rebuild_canonical_history("Tujuan: refactor context.", &kept);
    assert_eq!(rebuilt.len(), 4);
    assert_eq!(rebuilt[0]["role"], "user");
    assert_eq!(rebuilt[1]["role"], "model");
    assert_eq!(rebuilt[2]["role"], "user");
    assert_eq!(rebuilt[3]["role"], "model");

    let extracted = extract_existing_compaction_summary(&rebuilt).expect("summary extracted");
    assert_eq!(extracted, "Tujuan: refactor context.");

    // Second compaction must chain previous summary into <previous_summary>
    let mut extended = rebuilt;
    extended.push(u_msg("tambah tes"));
    extended.push(m_msg("tes selesai"));
    let plan2 = plan_canonical_compaction(&extended, 10, None, Some("Fokus ke file path")).unwrap();
    assert!(plan2.prompt.contains("<previous_summary>"));
    assert!(plan2.prompt.contains("Tujuan: refactor context."));
    assert!(plan2.prompt.contains("Fokus ke file path"));
}

#[test]
fn compaction_prompt_bounds_giant_tool_outputs_and_total_budget() {
    let giant = "Z".repeat(50_000);
    let mut history = Vec::new();
    for i in 0..40 {
        history.push(u_msg(&format!("pesan ke-{i}")));
        history.push(m_tool_call("run_shell", json!({ "command": "cat big" })));
        history.push(u_tool_resp("run_shell", json!({ "stdout": giant })));
        history.push(m_msg("oke"));
    }
    let plan = plan_canonical_compaction(&history, 100, None, None).expect("plan");
    assert!(
        plan.prompt.chars().count() <= MAX_COMPACTION_PROMPT_CHARS,
        "prompt too large: {}",
        plan.prompt.chars().count()
    );
    assert!(plan.prompt.contains("dipadatkan"));
}

#[test]
fn compaction_prompt_caps_oversized_custom_instructions_and_single_giant_message() {
    // 1. Custom instructions & previous summary jauh di atas MAX_COMPACTION_PROMPT_CHARS
    let giant_instructions = format!("INST_HEAD_{}_INST_TAIL", "I".repeat(200_000));
    let giant_prev = format!("PREV_HEAD_{}_PREV_TAIL", "P".repeat(200_000));
    // 2. Satu pesan user dengan 50 functionResponse besar sekaligus
    let giant_resp = "R".repeat(10_000);
    let many_parts: Vec<Value> = (0..50)
        .map(|i| {
            json!({
                "functionResponse": {
                    "id": format!("c{i}"),
                    "name": "read_file",
                    "response": { "content": giant_resp }
                }
            })
        })
        .collect();
    let history = vec![
        u_msg("turn 1"),
        json!({ "role": "user", "parts": many_parts }),
        m_msg("done"),
        u_msg("turn 2"),
        m_msg("siap"),
    ];

    let plan = plan_canonical_compaction(
        &history,
        10,
        Some(&giant_prev),
        Some(&giant_instructions),
    )
    .expect("plan");
    assert!(
        plan.prompt.chars().count() <= MAX_COMPACTION_PROMPT_CHARS,
        "prompt exceeded MAX_COMPACTION_PROMPT_CHARS: {} > {}",
        plan.prompt.chars().count(),
        MAX_COMPACTION_PROMPT_CHARS
    );
    assert!(plan.prompt.contains("INST_HEAD_"));
    assert!(plan.prompt.contains("_INST_TAIL"));
    assert!(plan.prompt.contains("PREV_HEAD_"));
    assert!(plan.prompt.contains("_PREV_TAIL"));
    assert!(plan.prompt.contains("<conversation>"));
}

#[tokio::test]
async fn context_manager_skips_when_within_budget_and_compacts_when_forced_or_over_threshold() {
    let policy = ContextPolicy {
        max_input_tokens: 400,
        reserve_output_tokens: 100,
        auto_compact_ratio: 0.80, // usable=300, threshold=240
        keep_recent_tokens: 40,
    };
    let mgr = ContextManager::new(policy);
    let summarizer = MockSummarizer::new("Ringkasan singkat.");
    let (_tx, rx) = tokio::sync::watch::channel(false);

    let short = vec![u_msg("a"), m_msg("b"), u_msg("c"), m_msg("d")];
    let prep = mgr
        .prepare_or_compact("sys", &[], &short, None, false, None, &summarizer, rx.clone())
        .await
        .unwrap();
    assert!(!prep.compacted);
    assert_eq!(summarizer.calls.load(Ordering::SeqCst), 0);

    // Forced compaction runs even when WithinBudget
    let forced = mgr
        .prepare_or_compact("sys", &[], &short, None, true, None, &summarizer, rx.clone())
        .await
        .unwrap();
    assert!(forced.compacted);
    assert_eq!(summarizer.calls.load(Ordering::SeqCst), 1);
    assert_eq!(forced.summary.as_deref(), Some("Ringkasan singkat."));
    assert_eq!(forced.history[0]["role"], "user");
    assert_eq!(forced.history[1]["role"], "model");

    // Auto compaction triggers when observed tokens exceed threshold
    let auto = mgr
        .prepare_or_compact("sys", &[], &short, Some(260), false, None, &summarizer, rx)
        .await
        .unwrap();
    assert!(auto.compacted);
    assert_eq!(summarizer.calls.load(Ordering::SeqCst), 2);
}

#[tokio::test]
async fn context_manager_skips_wasted_summarization_when_retained_tail_is_over_limit() {
    let policy = ContextPolicy {
        max_input_tokens: 400,
        reserve_output_tokens: 100, // usable=300
        auto_compact_ratio: 0.80,
        keep_recent_tokens: 40,
    };
    let mgr = ContextManager::new(policy);
    let summarizer = MockSummarizer::new("Ringkasan.");
    let (_tx, rx) = tokio::sync::watch::channel(false);

    // Pesan user terbaru sendirian sudah > 300 token (usable_limit).
    let history = vec![
        u_msg("turn lama"),
        m_msg("balasan lama"),
        u_msg(&"konten sangat besar ".repeat(150)),
    ];

    // Auto-compaction (`force = false`) harus melewati summarizer agar tidak membuang request LLM.
    let auto = mgr
        .prepare_or_compact("sys", &[], &history, None, false, None, &summarizer, rx.clone())
        .await
        .unwrap();
    assert!(!auto.compacted);
    assert_eq!(auto.budget.status, BudgetStatus::OverLimit);
    assert_eq!(summarizer.calls.load(Ordering::SeqCst), 0);

    // Manual `/compact` (`force = true`) tetap diizinkan meringkas pesan lama.
    let forced = mgr
        .prepare_or_compact("sys", &[], &history, None, true, None, &summarizer, rx)
        .await
        .unwrap();
    assert!(forced.compacted);
    assert_eq!(summarizer.calls.load(Ordering::SeqCst), 1);
    assert_eq!(forced.budget.status, BudgetStatus::OverLimit);
}
