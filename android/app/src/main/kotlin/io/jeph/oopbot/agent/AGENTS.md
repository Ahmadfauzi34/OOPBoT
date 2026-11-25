# Android Agent Runtime & Timeline State (`io.jeph.oopbot.agent`)

## Purpose
Manages per-chat agent processes (`AgentProcess`), pooling/parking lifecycle (`AgentRegistry`), JSONL RPC communication (`RpcClient`), Android foreground keep-alive (`AgentService`, `KeepAlive`), streaming timeline models (`ChatTimeline`, `TimelineModels`), extension widgets/subagents, and user turn/dialog notifications.

## Ownership
- **Process Lifecycle & Pooling:** `AgentProcess.kt`, `AgentRegistry.kt`, `AgentService.kt`, `KeepAlive.kt`, `IdleStop.kt`, `KeptBySession.kt`, `AppVisibility.kt`, `RemoteOutages.kt`.
- **RPC Client & Streaming Decoders:** `RpcClient.kt`, `SessionCommands.kt`, `PartialJson.kt`, `MessageContent.kt`, `StepStream.kt`, `StepFigure.kt`, `ContextUsage.kt`.
- **Timeline, Widgets & Subagents:** `ChatTimeline.kt`, `TimelineModels.kt`, `TimelineText.kt`, `ExtensionWidget.kt`, `LiveSubagent.kt`, `SubagentRuns.kt`.
- **Inbox & OS Notifications:** `ChatInbox.kt`, `ChatNotification.kt`, `TurnNotifications.kt`, `DialogNotifications.kt`.

## Local Contracts
- **Process Hold Grace (`holdsPi`):** `AgentRegistry` relies on `ManagedProcess.holdsPi` (`busy` or active background tasks + `releasedAfter` grace window) so the daemon/service does not drop mid-handoff between an LLM turn and a background task wakeup.
- **RPC Parity with Rust (`src/rpc/`):** All `RpcClient` commands (`prompt`, `steer`, `follow_up`, `abort`, `compact`, `get_state`, etc.) and incoming event schemas (`extension_ui_request`, `oopbot_channel`, `response`) MUST match `src/rpc/mod.rs` and `src/rpc/commands.rs`.
- **Compose Stability:** Public data models consumed by `ui/chat/` (`TimelineModels.kt`, etc.) MUST be immutable and annotated `@Immutable` when wrapping collections or sealed hierarchies so scrolling chat timelines avoid unnecessary recomposition.
- **Safe Partial JSON & Streaming:** `PartialJson.kt` and `StepStream.kt` MUST tolerate truncated/malformed JSON fragments during token streaming without throwing on the UI loop.
- **Background Foreground Service Sync:** `AgentService.refresh()` MUST be invoked via `AppGraph.holdWhileWorking()` only on transitions of `KeepAlive` holdings, preventing Android foreground service start-rate exceptions.
- **Transcript Indexing & Inbox Persistence:** `AgentProcess` eagerly pushes non-empty transcripts into `SessionRepository.cacheMessages` on session load, move, resync, and `agent_settled`; `ChatInbox` hydrates from and persists mutations to SQLite `chat_inbox` so unread alerts survive process death.
- **Edit StepFigure Badge-Count Contract (`StepFigure.kt`):** For `"edit"` tool results, `stepFigure` MUST prefer non-negative `details.linesAdded` and `details.linesRemoved` when present (so large edits whose `details.diff` was truncated at 32 KiB still display exact `+N/-M` badges) and fall back to counting `+`/`-` lines from `details.diff` for Pi/legacy entries, always returning `null` when `failed == true`.

## Work Guidance
- Keep `agent/` free of Compose UI composables (Compose views live in `ui/chat/` and `ui/subagents/`).
- Guard any new RPC event or widget parser with unit tests in `android/app/src/test/kotlin/io/jeph/oopbot/agent/`.

## Verification
- `cd android && gradle :app:testDebugUnitTest --tests "io.jeph.oopbot.agent.*"`
- Static checks: `cd android && gradle :app:ktlintCheck :app:detektDebug :app:detektDebugUnitTest`

## Child graph Index
- None (leaf domain; communicates with `src/rpc/AGENTS.md` and feeds `ui/AGENTS.md`).
