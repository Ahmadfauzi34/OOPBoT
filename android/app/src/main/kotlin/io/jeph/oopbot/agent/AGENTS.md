# Android Agent Runtime & Timeline State (`io.jeph.oopbot.agent`)

## Purpose
Manages per-chat agent processes (`AgentProcess`), pooling/parking lifecycle (`AgentRegistry`), JSONL RPC communication (`RpcClient`), Android foreground keep-alive (`AgentService`, `KeepAlive`), streaming timeline models (`ChatTimeline`, `TimelineModels`), extension widgets/subagents, and user turn/dialog notifications.

## Ownership
- **Process Lifecycle & Pooling:** `AgentProcess.kt`, `AgentRegistry.kt`, `AgentService.kt`, `KeepAlive.kt`, `StandbyBootReceiver.kt`, `IdleStop.kt`, `KeptBySession.kt`, `AppVisibility.kt`, `RemoteOutages.kt`.
- **RPC Client & Streaming Decoders:** `RpcClient.kt`, `SessionCommands.kt`, `PartialJson.kt`, `MessageContent.kt`, `StepStream.kt`, `StepFigure.kt`, `ContextUsage.kt`.
- **Timeline, Widgets & Subagents:** `ChatTimeline.kt`, `TimelineModels.kt`, `TimelineText.kt`, `ExtensionWidget.kt`, `LiveSubagent.kt`, `SubagentRuns.kt`.
- **Inbox & OS Notifications:** `ChatInbox.kt`, `ChatNotification.kt`, `TurnNotifications.kt`, `DialogNotifications.kt`.

## Local Contracts
- **Process Hold Grace (`holdsPi`):** `AgentRegistry` relies on `ManagedProcess.holdsPi` (`busy` or active background tasks + `releasedAfter` grace window) so the daemon/service does not drop mid-handoff between an LLM turn and a background task wakeup.
- **RPC Parity with Rust (`src/rpc/`):** All `RpcClient` commands (`prompt`, `steer`, `follow_up`, `abort`, `compact`, `get_state`, etc.) and incoming event schemas (`extension_ui_request`, `oopbot_channel`, `response`) MUST match `src/rpc/mod.rs` and `src/rpc/commands.rs`.
- **Compose Stability:** Public data models consumed by `ui/chat/` (`TimelineModels.kt`, etc.) MUST be immutable and annotated `@Immutable` when wrapping collections or sealed hierarchies so scrolling chat timelines avoid unnecessary recomposition.
- **Safe Partial JSON & Streaming:** `PartialJson.kt` and `StepStream.kt` MUST tolerate truncated/malformed JSON fragments during token streaming without throwing on the UI loop.
- **3-State Foreground Service & Smart WakeLock (`KeepAlive.kt`, `AgentService.kt`, `StandbyBootReceiver.kt`):** `KeepAlive` tracks `Active` (`acquire`, foreground + renewed partial wake lock) and `Standby` (`acquireStandby`, foreground with **zero wake lock**) separately, distinguishes desired `mode` from last-accepted `appliedMode`, serializes `AgentService.update()` IPC outside its lock, and exposes `sync()` to retry failed starts or refresh sticky policy. `AgentService` checks `setupComplete && backgroundStandby` for `stickyMode()` and `START_STICKY` null-intent restarts, catches `IllegalStateException` (`ForegroundServiceStartNotAllowedException`) in `enterForeground()`, reconciles against `keepAlive.mode`, and stops via `stopSelfResult(startId)`. `AppGraph.holdWhileWorking()` defers initial dispatch off `Application.onCreate()`, uses `registry.waitingOnBackground` so normal `HOLD_GRACE_MILLIS` windows never flash `Standby`, and `StandbyBootReceiver` stays manifest-disabled until `backgroundStandby && setupComplete` enables it via `PackageManager`.
- **Background Agent Wakeup (`AgentRegistry.kt`, `AgentProcess.kt`):** `AgentRegistry.open(..., ProcessLaunchOptions(allowBackground, engineOverride))` and `ManagedProcess.restart(allowBackground)` forward background authorization into `HostClient.openRpc(..., allowBackground = true)`. While in background (`allowBackground && !hostClient.isForeground()`), `AgentProcess.connect()` MUST fail fast on `oopbot_version` or disconnected remote hosts instead of hanging on foreground-only `ensure()`/`remoteOutages.reconnect()`.
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
