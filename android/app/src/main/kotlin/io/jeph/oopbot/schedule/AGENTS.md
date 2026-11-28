# Android Agent Scheduler (`io.jeph.oopbot.schedule`)

## Purpose
Manages persistent scheduled agent runs (`Once`, `Interval`, and 5-field POSIX `Cron`) backed by `OopbotDatabase` v2 (`scheduled_jobs`, `schedule_runs`), zero-polling single-alarm `AlarmManager` orchestration, and headless background `AgentProcess` execution under `KeepAliveMode.Active`.

## Ownership
- **Models & Recurrence Engine:** `ScheduleModels.kt`, `CronExpression.kt`, `ScheduleCalculator.kt`.
- **Alarm Orchestration & Receiver:** `AlarmScheduler.kt`, `ScheduleAlarmReceiver.kt`.
- **Repository & Headless Runner:** `ScheduleRepository.kt`, `ScheduleRunner.kt`, `AgentProcessJobExecutor.kt`.

## Local Contracts
- **Zero-Polling Single Alarm (`AlarmScheduler.kt`):** Only ONE `PendingIntent` (`ALARM_REQUEST_CODE = 40`) is registered with `AlarmManager`, always pointing at `MIN(next_run_at)` across enabled jobs. Defaults to `setAndAllowWhileIdle` (zero extra manifest permissions); upgrades to `setExactAndAllowWhileIdle` only when `preferences.exactScheduleAlarms` is true AND `canScheduleExactAlarms()` is granted (catching `SecurityException` with inexact fallback). A unified Permissions Settings screen is planned to manage opt-in `SCHEDULE_EXACT_ALARM` later.
- **Lazy Manifest Receiver (`ScheduleAlarmReceiver.kt`):** Declared `android:enabled="false"` in `AndroidManifest.xml` and dynamically enabled via `PackageManager` off the main thread only when `ScheduleRepository.hasEnabledJobs` emits `true` after initial SQLite hydration.
- **Broadcast Exemption & Non-Blocking ANR Safety (`ScheduleAlarmReceiver.kt`, `ScheduleRunner.kt`):** On `ACTION_RUN_DUE`, `onReceive()` MUST synchronously call `keepAlive.acquire("scheduled job")` inside `AlarmManager`'s FGS exemption window AND hold `goAsync()` strictly across the short `claimDueJobs()` + alarm re-arm dispatch (`onDispatched` calls `pendingResult.finish()` — never held across long agent runs). `ScheduleRunner` acquires per-job holds before releasing `initialHold` so `KeepAlive` never dips to `Standby`/`Off` mid-handoff.
- **Advance-Before-Execute & Safety Guardrails:** `ScheduleRepository.claimDueJobs()` persists each job's next `nextRunAt` and re-arms the alarm BEFORE execution starts. Runs enforce per-job `OverlapPolicy` (`Skip` / coalesced 1-deep `Queue`), `MisfirePolicy` (`EXACT_MISFIRE_GRACE_MILLIS = 5m` when exact alarms are active, `INEXACT_MISFIRE_GRACE_MILLIS = 20m` under Doze `setAndAllowWhileIdle`), a global concurrency cap (`MAX_CONCURRENT_RUNS = 2`), FK-safe deletion checks after semaphore acquisition and inside `ScheduleDbStore`, and per-job `maxRunSeconds` timeout with `NonCancellable` cleanup (`parkIfHidden` while `Starting`/`Installing`, `abort()` + `stopBackgroundTasks()` once `Ready`) while preserving any newly created `sessionPath`. PR 2C UI should surface battery-optimization / exact-alarm guidance when schedules are active.
- **Headless Result Integrity (`AgentProcessJobExecutor.kt`):** Captures `timeline.completedRuns` (incremented only on `agent_settled`, ignoring pre-turn compaction/queue events) and pre-prompt item keys before `prompt()` so slow-starting turns wait up to `TURN_START_GRACE_MILLIS = 15s`, never-starting turns fail explicitly, `"handled"`/no-`User` turns never leak stale prior summaries, and any post-prompt `TimelineItem.Error` records `ScheduleRunStatus.Failed`.
- **Pure-Kotlin Cron & DST Correctness (`CronExpression.kt`):** Validates syntax, bounded steps (`1..(max - min + 1)` preventing `Int` overflow), and calendar reachability (rejecting impossible dates like Feb 31), handles POSIX DOM/DOW OR semantics, skips non-existent DST spring-forward gaps, and fires once on the earlier offset across DST fall-back overlaps.

## Work Guidance
- Keep `schedule/` free of Compose UI composables (Schedules UI lives in `ui/settings/` in PR 2C).
- Mirror any recurrence or DB state change with unit tests in `android/app/src/test/kotlin/io/jeph/oopbot/schedule/`.

## Verification
- `cd android && ./gradlew :app:testDebugUnitTest --tests "io.jeph.oopbot.schedule.*"`
- Static checks: `cd android && ./gradlew :app:ktlintCheck :app:detektDebug :app:detektDebugUnitTest`

## Child graph Index
- None (leaf domain coordinating `data/` [`ScheduleDbStore`], `agent/` [`KeepAlive`, `AgentRegistry`, `AgentProcess`], and `host/` [`HostClient`]).
