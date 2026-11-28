package io.jeph.oopbot.schedule

import io.jeph.oopbot.agent.KeepAlive
import io.jeph.oopbot.agent.KeepAliveMode
import io.jeph.oopbot.data.JdbcSqlDb
import io.jeph.oopbot.data.ScheduleDbStore
import java.util.concurrent.CopyOnWriteArrayList
import java.util.concurrent.atomic.AtomicInteger
import kotlinx.coroutines.CompletableDeferred
import kotlinx.coroutines.CoroutineScope
import kotlinx.coroutines.ExperimentalCoroutinesApi
import kotlinx.coroutines.SupervisorJob
import kotlinx.coroutines.cancel
import kotlinx.coroutines.delay
import kotlinx.coroutines.flow.MutableStateFlow
import kotlinx.coroutines.launch
import kotlinx.coroutines.test.StandardTestDispatcher
import kotlinx.coroutines.test.TestScope
import kotlinx.coroutines.test.advanceTimeBy
import kotlinx.coroutines.test.runCurrent
import kotlinx.coroutines.test.runTest
import org.junit.After
import org.junit.Assert.assertEquals
import org.junit.Assert.assertFalse
import org.junit.Assert.assertTrue
import org.junit.Test

@OptIn(ExperimentalCoroutinesApi::class)
class ScheduleRunnerTest {
  private val sqlDb = JdbcSqlDb()
  private val store = ScheduleDbStore(sqlDb)

  @After
  fun tearDown() {
    sqlDb.close()
  }

  @Test
  fun hasEnabledJobsWaitsForEnsureInitializedThenTracksChanges() = runTest {
    val harness = createHarness(this)
    val emissions = CopyOnWriteArrayList<Boolean>()
    val collector = launch { harness.repository.hasEnabledJobs.collect { emissions += it } }
    runCurrent()

    // No emission before initial load completes (prevents spurious PackageManager disable on cold boot).
    assertTrue(emissions.isEmpty())

    harness.repository.ensureInitialized()
    runCurrent()
    assertEquals(listOf(false), emissions)

    val job = harness.repository.createJob(intervalDraft("j1", everyMinutes = 15, anchorMillis = 10_000L))
    runCurrent()
    assertEquals(listOf(false, true), emissions)

    harness.repository.setEnabled(job.id, false)
    runCurrent()
    assertEquals(listOf(false, true, false), emissions)
    collector.cancel()
  }

  @Test
  fun dispatchHandsOffInitialHoldWithoutDipAndAdvancesNextRunBeforeExecution() = runTest {
    val started = CompletableDeferred<Unit>()
    val finishGate = CompletableDeferred<Unit>()
    val dispatched = AtomicInteger(0)
    val harness = createHarness(this) { job, _ ->
      // nextRunAt must ALREADY be advanced & next alarm armed before execution finishes.
      val persisted = store.getJob(job.id)
      assertEquals(10_000L + 15 * 60_000L, persisted?.nextRunAt)
      started.complete(Unit)
      finishGate.await()
      JobExecutionResult(
        status = ScheduleRunStatus.Succeeded,
        sessionPath = "/s/new.jsonl",
        summary = "Done!",
      )
    }

    harness.now = 5_000L
    val job = harness.repository.createJob(intervalDraft("brief", everyMinutes = 15, anchorMillis = 10_000L))
    assertEquals(10_000L, job.nextRunAt)
    assertEquals(10_000L, harness.alarmDriver.lastTriggerAt)

    // Simulate AlarmReceiver acquiring Active hold synchronously before dispatchDueJobs.
    harness.now = 10_500L
    val initialHold = harness.keepAlive.acquire("alarm")
    harness.runner.dispatchDueJobs(
      initialHold = initialHold,
      nowMillis = harness.now,
      onDispatched = { dispatched.incrementAndGet() },
    )
    runCurrent()
    started.await()

    // onDispatched fired once claim/re-arm finished (without waiting for agent run) and Active never dipped.
    assertEquals(1, dispatched.get())
    assertEquals(listOf(KeepAliveMode.Active), harness.modeTransitions)
    assertEquals(10_000L + 15 * 60_000L, harness.alarmDriver.lastTriggerAt)

    finishGate.complete(Unit)
    runCurrent()

    assertEquals(listOf(KeepAliveMode.Active, KeepAliveMode.Off), harness.modeTransitions)
    val run = harness.repository.listRuns(job.id).single()
    assertEquals(ScheduleRunStatus.Succeeded, run.status)
    assertEquals("/s/new.jsonl", run.sessionPath)
    assertEquals("Done!", run.summary)
  }

  @Test
  fun dispatchReleasesInitialHoldAndCallsOnDispatchedWhenNoJobsDueOrScopeCancelled() = runTest {
    val harness = createHarness(this)
    harness.repository.ensureInitialized()

    val dispatched = AtomicInteger(0)
    val hold = harness.keepAlive.acquire("alarm-empty")
    val dispatchJob = harness.runner.dispatchDueJobs(
      initialHold = hold,
      nowMillis = 1_000L,
      onDispatched = { dispatched.incrementAndGet() },
    )
    runCurrent()
    assertTrue(dispatchJob.isCompleted)
    assertEquals(1, dispatched.get())
    assertEquals(KeepAliveMode.Off, harness.keepAlive.mode)

    // Cancelled scope must still release initialHold and call onDispatched immediately.
    val cancelledScope = CoroutineScope(SupervisorJob() + StandardTestDispatcher(testScheduler))
    cancelledScope.cancel()
    val cancelledRunner = ScheduleRunner(
      repository = harness.repository,
      keepAlive = harness.keepAlive,
      executor = { _, _ -> JobExecutionResult(ScheduleRunStatus.Succeeded) },
      scope = cancelledScope,
      clock = { harness.now },
    )
    val cancelledHold = harness.keepAlive.acquire("alarm-cancelled")
    assertEquals(KeepAliveMode.Active, harness.keepAlive.mode)
    cancelledRunner.dispatchDueJobs(
      initialHold = cancelledHold,
      nowMillis = 2_000L,
      onDispatched = { dispatched.incrementAndGet() },
    )
    assertEquals(2, dispatched.get())
    assertEquals(KeepAliveMode.Off, harness.keepAlive.mode)
  }

  @Test
  fun overlapSkipRecordsSkippedAndQueueCoalescesSingleFollowUp() = runTest {
    val skipGate = CompletableDeferred<Unit>()
    val queueFirstStarted = CompletableDeferred<Unit>()
    val queueFirstGate = CompletableDeferred<Unit>()
    val queueExecutions = AtomicInteger(0)

    val harness = createHarness(this) { job, _ ->
      when (job.name) {
        "skip-job" -> {
          skipGate.await()
          JobExecutionResult(ScheduleRunStatus.Succeeded)
        }

        else -> {
          val n = queueExecutions.incrementAndGet()
          if (n == 1) {
            queueFirstStarted.complete(Unit)
            queueFirstGate.await()
          }
          JobExecutionResult(ScheduleRunStatus.Succeeded, summary = "run-$n")
        }
      }
    }

    val skipJob = harness.repository.createJob(
      intervalDraft("skip-job", everyMinutes = 10, anchorMillis = 100_000L, overlap = OverlapPolicy.Skip),
    )
    val queueJob = harness.repository.createJob(
      intervalDraft("queue-job", everyMinutes = 10, anchorMillis = 100_000L, overlap = OverlapPolicy.Queue),
    )

    assertTrue(harness.runner.runNow(skipJob.id))
    assertTrue(harness.runner.runNow(skipJob.id))
    runCurrent()
    skipGate.complete(Unit)
    runCurrent()

    val skipStatuses = harness.repository.listRuns(skipJob.id).map { it.status }.toSet()
    assertEquals(setOf(ScheduleRunStatus.Succeeded, ScheduleRunStatus.Skipped), skipStatuses)

    harness.modeTransitions.clear()
    assertTrue(harness.runner.runNow(queueJob.id))
    runCurrent()
    queueFirstStarted.await()
    // Fire 3 overlapping triggers while first run is active -> coalesced into ONE follow-up run.
    assertTrue(harness.runner.runNow(queueJob.id))
    assertTrue(harness.runner.runNow(queueJob.id))
    assertTrue(harness.runner.runNow(queueJob.id))
    queueFirstGate.complete(Unit)
    runCurrent()

    assertEquals(2, queueExecutions.get())
    // Seamless Active handoff between run 1 and coalesced run 2 (never dipped to Off mid-queue).
    assertEquals(listOf(KeepAliveMode.Active, KeepAliveMode.Off), harness.modeTransitions)
    // runNow never modified nextRunAt.
    assertEquals(100_000L, harness.repository.getJob(queueJob.id)?.nextRunAt)
  }

  @Test
  fun enforcesGlobalConcurrencyCapTimeoutSessionCaptureAndUnexpectedExceptions() = runTest {
    val active = AtomicInteger(0)
    val peak = AtomicInteger(0)
    val harness = createHarness(this, maxConcurrentRuns = 2) { job, onSessionPath ->
      when (job.name) {
        "timeout-job" -> {
          onSessionPath("/s/timed-out-session.jsonl")
          delay(60_000L)
          JobExecutionResult(ScheduleRunStatus.Succeeded)
        }

        "crash-job" -> error("boom")

        else -> {
          val nowActive = active.incrementAndGet()
          peak.accumulateAndGet(nowActive, ::maxOf)
          delay(500L)
          active.decrementAndGet()
          JobExecutionResult(ScheduleRunStatus.Succeeded)
        }
      }
    }

    harness.now = 5_000L
    for (i in 1..4) {
      harness.repository.createJob(intervalDraft("cap-$i", everyMinutes = 10, anchorMillis = 10_000L))
    }
    harness.runner.dispatchDueJobs(nowMillis = 10_000L)
    runCurrent()
    advanceTimeBy(1_500L)
    runCurrent()
    assertEquals(2, peak.get())
    assertEquals(KeepAliveMode.Off, harness.keepAlive.mode)

    val timeoutJob = harness.repository.createJob(
      intervalDraft("timeout-job", everyMinutes = 10, anchorMillis = 999_000L, maxRunSeconds = 30),
    )
    harness.runner.runNow(timeoutJob.id)
    runCurrent()
    advanceTimeBy(31_000L)
    runCurrent()
    val timedOutRun = harness.repository.listRuns(timeoutJob.id).single()
    assertEquals(ScheduleRunStatus.TimedOut, timedOutRun.status)
    assertEquals("/s/timed-out-session.jsonl", timedOutRun.sessionPath)
    assertEquals(KeepAliveMode.Off, harness.keepAlive.mode)

    val crashJob = harness.repository.createJob(
      intervalDraft("crash-job", everyMinutes = 10, anchorMillis = 999_000L),
    )
    harness.runner.runNow(crashJob.id)
    runCurrent()
    val crashRun = harness.repository.listRuns(crashJob.id).single()
    assertEquals(ScheduleRunStatus.Failed, crashRun.status)
    assertEquals("boom", crashRun.error)
    assertEquals(KeepAliveMode.Off, harness.keepAlive.mode)

    assertFalse(harness.runner.runNow("non-existent-id"))
  }

  @Test
  fun deletingJobWhileQueuedBehindSemaphoreOrSkippingDoesNotCrashWithFkError() = runTest {
    val gate = CompletableDeferred<Unit>()
    val executedNames = CopyOnWriteArrayList<String>()
    val harness = createHarness(this, maxConcurrentRuns = 1) { job, _ ->
      executedNames += job.name
      if (job.name == "holder") gate.await()
      JobExecutionResult(ScheduleRunStatus.Succeeded)
    }

    harness.now = 5_000L
    val holder = harness.repository.createJob(intervalDraft("holder", everyMinutes = 10, anchorMillis = 10_000L))
    val queued = harness.repository.createJob(intervalDraft("queued", everyMinutes = 10, anchorMillis = 10_000L))

    harness.runner.dispatchDueJobs(nowMillis = 10_000L)
    runCurrent()
    assertEquals(listOf("holder"), executedNames)

    // Delete queued job before semaphore frees, AND trigger OverlapAction.Skip on holder then delete holder too.
    harness.repository.deleteJob(queued.id)
    harness.runner.runNow(holder.id)
    harness.repository.deleteJob(holder.id)
    gate.complete(Unit)
    runCurrent()

    // Queued deleted job never executed; zero FK crash; KeepAlive returned cleanly to Off.
    assertEquals(listOf("holder"), executedNames)
    assertEquals(KeepAliveMode.Off, harness.keepAlive.mode)
  }

  @Test
  fun updateJobPreservesConcurrentRunStatusNormalizesTimezoneAndValidatesBlankInputs() = runTest {
    val harness = createHarness(this)
    harness.now = 1_000L
    val created = harness.repository.createJob(
      NewScheduledJob(
        name = "Daily",
        cwd = "/w/app",
        prompt = "Check repo",
        spec = ScheduleSpec.Cron("0 8 * * *", ""),
        timezone = "",
      ),
    )
    assertEquals("UTC", created.timezone)

    // Simulate a background run updating live DB fields while the UI holds an older snapshot (`created`).
    val runId = requireNotNull(harness.repository.recordRunStart(created.id, startedAt = 2_000L, sessionPath = "/s/1"))
    harness.repository.recordRunFinish(
      runId = runId,
      jobId = created.id,
      finishedAt = 3_000L,
      status = ScheduleRunStatus.Succeeded,
      sessionPath = "/s/1",
      summary = "ok",
      error = null,
    )

    // Save edit using the stale `created` snapshot (lastStatus=Never, blank timezone).
    val updated = harness.repository.updateJob(
      created.copy(
        name = "Daily Edited",
        spec = ScheduleSpec.Cron("0 9 * * *", ""),
        timezone = "",
      ),
    )
    assertEquals("Daily Edited", updated.name)
    assertEquals("UTC", updated.timezone)
    assertEquals(2_000L, updated.lastRunAt)
    assertEquals(ScheduleRunStatus.Succeeded, updated.lastStatus)
    assertEquals(1_000L, updated.createdAt)

    // Blank name, cwd, or prompt must be rejected.
    val blankNameErr = runCatching {
      harness.repository.createJob(intervalDraft("   ", everyMinutes = 10, anchorMillis = 10_000L))
    }.exceptionOrNull()
    assertTrue(blankNameErr is IllegalArgumentException)

    val blankPromptErr = runCatching {
      harness.repository.updateJob(updated.copy(prompt = "  "))
    }.exceptionOrNull()
    assertTrue(blankPromptErr is IllegalArgumentException)
  }

  private fun intervalDraft(
    name: String,
    everyMinutes: Int,
    anchorMillis: Long,
    overlap: OverlapPolicy = OverlapPolicy.Skip,
    maxRunSeconds: Int = ScheduleSpec.DEFAULT_MAX_RUN_SECONDS,
  ) = NewScheduledJob(
    name = name,
    cwd = "/w/app",
    prompt = "check",
    spec = ScheduleSpec.Interval(everyMinutes, anchorMillis),
    timezone = "UTC",
    overlapPolicy = overlap,
    maxRunSeconds = maxRunSeconds,
  )

  private fun createHarness(
    testScope: TestScope,
    maxConcurrentRuns: Int = ScheduleRunner.MAX_CONCURRENT_RUNS,
    executor: ScheduledJobExecutor = ScheduledJobExecutor { _, _ -> JobExecutionResult(ScheduleRunStatus.Succeeded) },
  ): Harness {
    val dispatcher = StandardTestDispatcher(testScope.testScheduler)
    val transitions = CopyOnWriteArrayList<KeepAliveMode>()
    val keepAlive = KeepAlive { mode, _ ->
      transitions += mode
      true
    }
    val alarmDriver = FakeAlarmDriver()
    val preferExact = MutableStateFlow(false)
    val clockBox = MutableClock()
    val repo = ScheduleRepository(
      store = store,
      alarmScheduler = AlarmScheduler(alarmDriver),
      preferExactAlarms = preferExact,
      scope = testScope.backgroundScope,
      ioDispatcher = dispatcher,
      clock = ScheduleClock(nowMillis = { clockBox.now }, systemZoneId = { "UTC" }),
    )
    val runner = ScheduleRunner(
      repository = repo,
      keepAlive = keepAlive,
      executor = executor,
      scope = testScope.backgroundScope,
      clock = { clockBox.now },
      maxConcurrentRuns = maxConcurrentRuns,
    )
    return Harness(repo, runner, keepAlive, transitions, alarmDriver, clockBox)
  }

  private class MutableClock(var now: Long = 0L)

  private class Harness(
    val repository: ScheduleRepository,
    val runner: ScheduleRunner,
    val keepAlive: KeepAlive,
    val modeTransitions: CopyOnWriteArrayList<KeepAliveMode>,
    val alarmDriver: FakeAlarmDriver,
    private val clockBox: MutableClock,
  ) {
    var now: Long
      get() = clockBox.now
      set(value) {
        clockBox.now = value
      }
  }

  private class FakeAlarmDriver : AlarmDriver {
    var lastTriggerAt: Long? = null

    override fun canScheduleExactAlarms(): Boolean = false

    override fun scheduleInexact(triggerAtMillis: Long) {
      lastTriggerAt = triggerAtMillis
    }

    override fun scheduleExact(triggerAtMillis: Long) {
      lastTriggerAt = triggerAtMillis
    }

    override fun cancel() {
      lastTriggerAt = null
    }
  }
}
