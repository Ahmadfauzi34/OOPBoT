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
        supports_images: true,
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
    // AND preserve real user messages that literally mention <compaction_summary>...</compaction_summary>.
    let mut extended = rebuilt;
    extended.push(u_msg(
        "Tolong periksa parser tag <compaction_summary>contoh</compaction_summary> di compactor.rs",
    ));
    extended.push(m_msg("siap, saya cek parsernya"));
    extended.push(u_msg("tambah tes"));
    extended.push(m_msg("tes selesai"));
    let plan2 = plan_canonical_compaction(&extended, 10, None, Some("Fokus ke file path")).unwrap();
    assert!(plan2.prompt.contains("<previous_summary>"));
    assert!(plan2.prompt.contains("Tujuan: refactor context."));
    assert!(plan2.prompt.contains("Fokus ke file path"));
    assert!(
        plan2
            .prompt
            .contains("Tolong periksa parser tag <compaction_summary>contoh</compaction_summary>"),
        "real user message mentioning literal summary markers must be preserved in compaction prompt"
    );

    // Non-synthetic history starting with a user message mentioning the markers must NOT be
    // mistaken for a synthetic compaction summary or dropped.
    let normal_history = vec![
        u_msg("Jelaskan format <compaction_summary>abc</compaction_summary>"),
        m_msg("Format itu dipakai compactor."),
        u_msg("lanjut"),
        m_msg("oke"),
    ];
    assert_eq!(extract_existing_compaction_summary(&normal_history), None);
    let plan3 = plan_canonical_compaction(&normal_history, 10, None, None).unwrap();
    assert!(!plan3.prompt.contains("<previous_summary>"));
    assert!(plan3
        .prompt
        .contains("Jelaskan format <compaction_summary>abc</compaction_summary>"));
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
        supports_images: true,
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
        supports_images: true,
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

#[test]
fn compaction_prompt_scales_down_for_small_context_window_models() {
    // Model 8k context window -> usable 6k -> prompt cap ~17,808 chars (jauh di bawah 120k default).
    let small_policy = ContextPolicy::for_model_limits(Some(8_000), Some(2_000));
    let small_cap = small_policy.max_compaction_prompt_chars();
    assert!(
        (10_000..24_000).contains(&small_cap),
        "unexpected small-model compaction prompt cap: {}",
        small_cap
    );

    // Model 128k context window tetap menggunakan plafon penuh 120k chars.
    let large_policy = ContextPolicy::for_model_limits(Some(128_000), Some(8_192));
    assert_eq!(
        large_policy.max_compaction_prompt_chars(),
        MAX_COMPACTION_PROMPT_CHARS
    );

    let mut history = Vec::new();
    for i in 0..20 {
        history.push(u_msg(&format!("pesan-{i}: {}", "A".repeat(4_000))));
        history.push(m_msg(&format!("jawab-{i}: {}", "B".repeat(4_000))));
    }
    let giant_prev = "P".repeat(20_000);
    let giant_inst = "I".repeat(20_000);

    let plan = plan_canonical_compaction_with_prompt_limit(
        &history,
        small_policy.keep_recent_tokens,
        Some(&giant_prev),
        Some(&giant_inst),
        small_cap,
    )
    .expect("plan");

    assert!(
        plan.prompt.chars().count() <= small_cap,
        "small-model compaction prompt exceeded cap: {} > {}",
        plan.prompt.chars().count(),
        small_cap
    );
    assert!(plan.prompt.contains("<conversation>"));
    assert!(plan.prompt.contains("<previous_summary>"));
    assert!(plan.prompt.contains("<custom_instructions>"));
}

#[tokio::test]
async fn compaction_plan_extracts_bounded_recent_inline_images_for_summarizer() {
    struct ImageCaptureSummarizer {
        images_seen: std::sync::Mutex<Vec<Value>>,
    }

    #[async_trait]
    impl ContextSummarizer for ImageCaptureSummarizer {
        async fn summarize(
            &self,
            _compaction_prompt: &str,
            _cancel_rx: tokio::sync::watch::Receiver<bool>,
        ) -> Result<String, BoxError> {
            Ok("fallback".into())
        }

        async fn summarize_with_images(
            &self,
            _compaction_prompt: &str,
            inline_images: &[Value],
            _cancel_rx: tokio::sync::watch::Receiver<bool>,
        ) -> Result<String, BoxError> {
            *self.images_seen.lock().unwrap() = inline_images.to_vec();
            Ok("Ringkasan termasuk detail gambar.".into())
        }
    }

    let mut history = Vec::new();
    // 6 gambar pada pesan yang diringkas -> hanya 4 terbaru (MAX_COMPACTION_INLINE_IMAGES) dilampirkan.
    for i in 1..=6 {
        history.push(json!({
            "role": "user",
            "parts": [
                { "text": format!("lihat screenshot #{i}") },
                { "inlineData": { "mimeType": "image/png", "data": format!("IMG_DATA_{i}") } }
            ]
        }));
        history.push(m_msg(&format!("oke screenshot #{i}")));
    }
    history.push(u_msg("pertanyaan lanjutan"));
    history.push(m_msg("siap"));

    let plan = plan_canonical_compaction(&history, 20, None, None).expect("plan");
    assert_eq!(plan.inline_images.len(), MAX_COMPACTION_INLINE_IMAGES);
    assert_eq!(plan.inline_images[0]["inlineData"]["data"], "IMG_DATA_3");
    assert_eq!(plan.inline_images[3]["inlineData"]["data"], "IMG_DATA_6");
    assert!(plan.prompt.contains("[gambar inline #1 (terlampir pada input ringkasan)]"));
    assert!(plan.prompt.contains("[gambar inline sebelumnya]"));

    let mgr = ContextManager::new(ContextPolicy::for_model_limits(Some(16_000), Some(2_000)));
    let summarizer = ImageCaptureSummarizer {
        images_seen: std::sync::Mutex::new(Vec::new()),
    };
    let (_tx, rx) = tokio::sync::watch::channel(false);
    let prep = mgr
        .prepare_or_compact("sys", &[], &history, None, true, None, &summarizer, rx)
        .await
        .unwrap();
    assert!(prep.compacted);
    assert_eq!(
        summarizer.images_seen.lock().unwrap().len(),
        MAX_COMPACTION_INLINE_IMAGES
    );
}

#[tokio::test]
async fn context_manager_rejects_empty_or_whitespace_summary_without_dropping_history() {
    let policy = ContextPolicy::for_model_limits(Some(8_000), Some(1_000));
    let mgr = ContextManager::new(policy);
    let history = vec![u_msg("turn 1"), m_msg("jawab 1"), u_msg("turn 2"), m_msg("jawab 2")];
    let (_tx, rx) = tokio::sync::watch::channel(false);

    for blank in ["", "   \n\t  "] {
        let summarizer = MockSummarizer::new(blank);
        let err = mgr
            .prepare_or_compact("sys", &[], &history, None, true, None, &summarizer, rx.clone())
            .await
            .expect_err("blank compaction summary must be rejected");
        assert!(err.to_string().contains("kosong"));
    }
}

#[test]
fn compaction_image_labels_stay_synchronized_when_middle_omitted_or_budget_limited() {
    // 1. Gambar di entri pertengahan terpotong oleh batas prompt -> tidak boleh ikut dilampirkan
    //    dan gambar yang bertahan di head/tail tetap diberi label berurutan #1, #2.
    let mut history = Vec::new();
    history.push(json!({
        "role": "user",
        "parts": [
            { "text": "awal dengan gambar head" },
            { "inlineData": { "mimeType": "image/png", "data": "HEAD_IMG" } }
        ]
    }));
    history.push(m_msg("oke head"));
    for i in 0..12 {
        let mut parts = vec![json!({ "text": format!("mid-{i}: {}", "M".repeat(800)) })];
        if i == 6 {
            parts.push(json!({ "inlineData": { "mimeType": "image/png", "data": "DROPPED_MID_IMG" } }));
        }
        history.push(json!({ "role": "user", "parts": parts }));
        history.push(m_msg(&format!("balas-{i}: {}", "B".repeat(800))));
    }
    history.push(json!({
        "role": "user",
        "parts": [
            { "text": "akhir dengan gambar tail" },
            { "inlineData": { "mimeType": "image/png", "data": "TAIL_IMG" } }
        ]
    }));
    history.push(m_msg("oke tail"));
    history.push(u_msg("pertanyaan terakhir"));
    history.push(m_msg("siap"));

    let plan = plan_canonical_compaction_with_limits(&history, 20, None, None, 2_200, 10_000)
        .expect("plan");
    assert!(!plan.prompt.contains('\u{1f}'));
    assert!(
        plan.inline_images
            .iter()
            .all(|v| v["inlineData"]["data"] != "DROPPED_MID_IMG"),
        "omitted middle image must not be attached"
    );
    for idx in 0..plan.inline_images.len() {
        let expected_label = format!(
            "[gambar inline #{} (terlampir pada input ringkasan)]",
            idx + 1
        );
        assert!(
            plan.prompt.contains(&expected_label),
            "missing synchronized label {} in prompt",
            expected_label
        );
    }
    let next_label = format!(
        "[gambar inline #{} (terlampir pada input ringkasan)]",
        plan.inline_images.len() + 1
    );
    assert!(!plan.prompt.contains(&next_label));

    // 2. Model text-only (`max_image_tokens == 0`) -> 0 lampiran dan tidak ada label "(terlampir...)".
    let text_only_plan =
        plan_canonical_compaction_with_limits(&history, 20, None, None, 10_000, 0).expect("plan");
    assert!(text_only_plan.inline_images.is_empty());
    assert!(!text_only_plan
        .prompt
        .contains("(terlampir pada input ringkasan)"));
    assert!(text_only_plan.prompt.contains("[gambar inline sebelumnya]"));
}

#[test]
fn compaction_skips_redundant_recompaction_on_synthetic_only_history_unless_instructed() {
    let synthetic_only = rebuild_canonical_history("Ringkasan sudah padat.", &[]);
    assert_eq!(synthetic_only.len(), 2);

    // Tanpa instruksi kustom -> tidak membuang request LLM untuk meringkas ulang ringkasan yang sama.
    assert!(plan_canonical_compaction(&synthetic_only, 40, None, None).is_none());

    // Dengan instruksi kustom `/compact fokus ke bug` -> tetap diizinkan memadatkan ulang.
    let plan = plan_canonical_compaction(&synthetic_only, 40, None, Some("Fokus ke bug auth"))
        .expect("custom instructions should allow re-compacting existing summary");
    assert!(plan.prompt.contains("<previous_summary>"));
    assert!(plan.prompt.contains("Ringkasan sudah padat."));
    assert!(plan.prompt.contains("Fokus ke bug auth"));

    // Setelah compaction, 1 giliran user + rantai tool tidak boleh memilih cut_index == 2
    // (yang meringkas 0 pesan baru), melainkan jatuh ke retain-none agar rantai tool ikut diringkas.
    let mut one_turn_after = synthetic_only;
    one_turn_after.push(u_msg("jalankan tool"));
    one_turn_after.push(m_tool_call("run_shell", json!({ "command": "ls" })));
    one_turn_after.push(u_tool_resp("run_shell", json!({ "stdout": "ok" })));
    let plan_mid = plan_canonical_compaction(&one_turn_after, 40, None, None)
        .expect("post-compaction tool chain must fall back to retain-none");
    assert_eq!(plan_mid.cut_index, one_turn_after.len());
    assert!(plan_mid.prompt.contains("<previous_summary>"));
    assert!(plan_mid.prompt.contains("jalankan tool"));
}
