# OOPBoT JSONL RPC Daemon (`src/interfaces/rpc/`)

## Purpose
Implements the asynchronous JSONL agent daemon (`oopbot --mode rpc`) used by the Android app (`AgentProcess.kt`, `RpcClient.kt`, `ChatTimeline.kt`) to drive prompts, streaming events, steering/follow-up queues, background wakeups, interactive UI dialogs, context compaction, and session JSONL persistence.

## Ownership
- **Server Wrapper & Entrypoint:** `src/interfaces/rpc/mod.rs` (`RpcServer`, `run_rpc_mode`, `set_output_sink`, `OUTPUT_SINK`).
- **Runtime State Machine & UI Dialogs:** `src/interfaces/rpc/runtime.rs` (`RpcRuntime`, `ActiveOp`, `QueuedPrompt`, `CoordinatorFinishGuard`, `PendingUiRequests`, `install_plugin_callbacks`, `request_ui_dialog`).
- **JSONL Loop & Command Router:** `src/interfaces/rpc/dispatch.rs` (`run_loop_with_input`).
- **Turn Coordinator, Wakeup & Retry:** `src/interfaces/rpc/turn.rs` (`run_prompt_coordinator`, `execute_single_turn`, `trigger_background_wakeup`, `is_transient_llm_error`).
- **RPC Slash Commands:** `src/interfaces/rpc/slash.rs` (`handle_slash_command`).
- **Session Operations & Channel Events:** `src/interfaces/rpc/session_ops.rs` (`fork`, `clone`, `new_session`, `switch_session`, `get_session_stats`, `export_html`, `handle_channel_message`).
- **HTML Exporter:** `src/interfaces/rpc/export_html.rs` (`render_session_html`, `html_escape`).
- **Streaming Event Sink:** `src/interfaces/rpc/events.rs` (`RpcEventSink`, agent lifecycle & tool event emission).
- **Interactive Tool Approval:** `src/interfaces/rpc/approver.rs` (`RpcApprover`, `PendingApprovals`).
- **Context Compaction:** `src/interfaces/rpc/compaction.rs` (branch summarization & compaction lifecycle).
- **Provider Config & Discovery:** `src/interfaces/rpc/providers.rs` (`load_providers`, model endpoint resolution, auth/settings sync).
- **JSONL Session Persistence:** `src/interfaces/rpc/session_store.rs` (`RpcUiState`, branch entries, `TokenTotals`).
- **Unit & Protocol Tests:** `src/interfaces/rpc/tests/` (`mod.rs`, `protocol.rs`, `providers_config.rs`, `queues_wakeup.rs`, `sessions_branch.rs`, `ui_dialogs.rs`).

## Local Contracts
- **Single Active Operation & Queue Ordering:** At most ONE `ActiveOp` (`Streaming` or `Compacting`) may run at a time, guarded by `CoordinatorFinishGuard` to ensure `ActiveOp::Idle` restoration even on early return or panic.
- **Defer Settlement & Auto-Wakeup Serialization:** Background task completion wakeups (`trigger_background_wakeup`) and queued prompts (`steering_queue`, `follow_up_queue`, `wakeup_queue`) MUST respect `defer_settled` and never start overlapping LLM turns.
- **Background Wakeup Queue Isolation & Ghost-Wakeup Gate:** System background wakeups MUST stay in `wakeup_queue` (NEVER pushed to user `steering_queue`/`follow_up_queue`, NEVER emitted in `queue_update`, and NEVER returned by `clear_queue` so they cannot leak into Android's `STEERING` card). Dismissed task IDs MUST be recorded in `dismissed_bg_wakeups` via `dismiss_background_wakeups` (and cleared on `abort` / `background: stop`) to suppress delayed/race ghost wakeups, guarded by `gate_*` tests in `src/interfaces/rpc/tests/queues_wakeup.rs`.
- **Output Sink Routing:** All protocol output MUST pass through `RpcRuntime::send_line` / `send_channel` so TCP host wrappers (`OUTPUT_SINK`) and test harnesses capture frames cleanly. Any unit test overriding `OUTPUT_SINK` MUST hold `output_sink_test_mutex()`.
- **Non-Blocking UI & Approval Cleanup:** Interactive dialog futures (`request_ui_dialog`) and tool approvals (`PendingApprovals`) MUST clean up map entries on timeout/drop (`PendingUiRequestGuard`) so orphaned requests cannot leak memory or wedge turns.
- **Config & Auth Security:** `auth.json` entries MUST be written atomically with restrictive permissions (`0o600`) via `src/config/store.rs`, and JSONL session files MUST preserve valid parent/leaf chains (`active_branch_entries`).
- **Session Context Resilience Gates:**
  - Mid-turn LLM errors after tool execution or steer injection MUST commit partial `history` in `AgentHarness` and retry via `continue_turn_cancellable` without duplicating the initial user prompt (`gate_mid_turn_tool_and_steer_context_preserved_across_http_retry`).
  - JSONL appends MUST flush full lines and `active_branch_entries` MUST stitch across missing/corrupted `parentId` links without cross-branch leaks (`gate_corrupted_jsonl_line_recovers_prior_history_without_cross_branch_leak`).
  - `build_compaction_prompt_with_limit` MUST cap per-entry tool/message lengths, `<custom_instructions>`, and total prompt budget within the active model's `ContextPolicy::max_compaction_prompt_chars()` (<= `MAX_COMPACTION_PROMPT_CHARS`) via `crate::context::assemble_compaction_prompt_with_limit` (`gate_compaction_prompt_bounds_giant_tool_outputs_and_total_budget`).
  - `RpcEventSink` MUST preserve Gemini `thoughtSignature` on `thinking`/`toolCall` blocks, `/clear` MUST persist a `context_reset` boundary, background wakeups MUST retain `details.contextText`, and unknown tool calls MUST emit paired `ToolCallStart` + `ToolCallEnd` (`gate_thought_signature_*`, `gate_slash_clear_*`, `gate_wakeup_context_text_*`).

## Work Guidance
- Keep JSON field names in camelCase matching Android Kotlin deserializers (`sessionFile`, `sessionId`, `isStreaming`, `isCompacting`, `extension_ui_request`, `oopbot_channel`).
- Install plugin callbacks (`BackgroundPlugin` completion notifier, `InteractPlugin` notify/ask handlers) via `Arc::downgrade` in `install_plugin_callbacks` to avoid `Arc<RpcRuntime>` reference cycles.

## Verification
- `cargo test --locked interfaces::rpc:: -- --test-threads=2`
- Full crate regression: `cargo clippy --all-targets --locked -- -D warnings && cargo test --locked -- --test-threads=2`

## Child graph Index
- None (leaf domain; cross-boundary contract partners are `src/plugins/AGENTS.md` and `android/app/src/main/kotlin/io/jeph/oopbot/agent/AGENTS.md`).
