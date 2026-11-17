# OOPBoT JSONL RPC Daemon (`src/rpc/`)

## Purpose
Implements the asynchronous JSONL agent daemon (`oopbot --mode rpc`) used by the Android app (`AgentProcess.kt`, `RpcClient.kt`, `ChatTimeline.kt`) to drive prompts, streaming events, steering/follow-up queues, background wakeups, interactive UI dialogs, context compaction, and session JSONL persistence.

## Ownership
- **Runtime Coordinator & Queue/Wakeup State Machine:** `src/rpc/mod.rs` (`RpcRuntime`, `RpcServer`, `ActiveOp`, `CoordinatorFinishGuard`).
- **Command Handlers:** `src/rpc/commands.rs` (RPC request dispatch: `prompt`, `steer`, `follow_up`, `abort`, `compact`, `get_state`, extension UI responses, etc.).
- **Streaming Event Sink:** `src/rpc/events.rs` (`RpcEventSink`, agent lifecycle & tool event emission).
- **Interactive Tool Approval:** `src/rpc/approver.rs` (`RpcApprover`, `PendingApprovals`).
- **Context Compaction:** `src/rpc/compaction.rs` (branch summarization & compaction lifecycle).
- **Provider Config & Discovery:** `src/rpc/providers.rs` (`load_providers`, model endpoint resolution, auth/settings sync).
- **JSONL Session Persistence:** `src/rpc/session_store.rs` (`RpcUiState`, branch entries, `TokenTotals`).
- **Unit & Protocol Tests:** `src/rpc/tests.rs`.

## Local Contracts
- **Single Active Operation & Queue Ordering:** At most ONE `ActiveOp` (`Streaming` or `Compacting`) may run at a time, guarded by `CoordinatorFinishGuard` to ensure `ActiveOp::Idle` restoration even on early return or panic.
- **Defer Settlement & Auto-Wakeup Serialization:** Background task completion wakeups (`trigger_background_wakeup`) and queued prompts (`steering_queue`, `follow_up_queue`) MUST respect `defer_settled` and never start overlapping LLM turns.
- **Output Sink Routing:** All protocol output MUST pass through `RpcRuntime::send_line` / `send_channel` so TCP host wrappers (`OUTPUT_SINK`) and test harnesses capture frames cleanly. Any unit test overriding `OUTPUT_SINK` MUST hold `output_sink_test_mutex()`.
- **Non-Blocking UI & Approval Cleanup:** Interactive dialog futures (`request_ui_dialog`) and tool approvals (`PendingApprovals`) MUST clean up map entries on timeout/drop (`PendingUiRequestGuard`) so orphaned requests cannot leak memory or wedge turns.
- **Config & Auth Security:** `auth.json` entries MUST be written atomically with restrictive permissions (`0o600`), and JSONL session files MUST preserve valid parent/leaf chains (`active_branch_entries`).

## Work Guidance
- Keep JSON field names in camelCase matching Android Kotlin deserializers (`sessionFile`, `sessionId`, `isStreaming`, `isCompacting`, `extension_ui_request`, `oopbot_channel`).
- Install plugin callbacks (`BackgroundPlugin` completion notifier, `InteractPlugin` notify/ask handlers) via `Arc::downgrade` in `install_plugin_callbacks` to avoid `Arc<RpcRuntime>` reference cycles.

## Verification
- `cargo test --locked rpc:: -- --test-threads=2`
- Full crate regression: `cargo clippy --all-targets --locked -- -D warnings && cargo test --locked -- --test-threads=2`

## Child graph Index
- None (leaf domain; cross-boundary contract partners are `src/plugins/AGENTS.md` and `android/app/src/main/kotlin/io/jeph/oopbot/agent/AGENTS.md`).
