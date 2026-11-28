package io.jeph.oopbot.schedule

import io.jeph.oopbot.agent.AgentProcessState
import io.jeph.oopbot.agent.ChatTimeline
import io.jeph.oopbot.agent.ErrorCause
import io.jeph.oopbot.agent.ProcessLaunchOptions
import io.jeph.oopbot.agent.TimelineItem
import io.jeph.oopbot.agent.TimelineState
import io.jeph.oopbot.agent.TooManyRunning
import kotlinx.coroutines.CancellationException
import kotlinx.coroutines.CompletableDeferred
import kotlinx.coroutines.ExperimentalCoroutinesApi
import kotlinx.coroutines.async
import kotlinx.coroutines.delay
import kotlinx.coroutines.flow.MutableStateFlow
import kotlinx.coroutines.launch
import kotlinx.coroutines.test.advanceTimeBy
import kotlinx.coroutines.test.runCurrent
import kotlinx.coroutines.test.runTest
import org.json.JSONObject
import org.junit.Assert.assertEquals
import org.junit.Assert.assertNull
import org.junit.Assert.assertTrue
import org.junit.Test

@OptIn(ExperimentalCoroutinesApi::class)
class AgentProcessJobExecutorTest {
  @Test
  fun recordsTimelineErrorAsFailedInsteadOfSucceeded() = runTest {
    val session = FakeAgentSession(initialState = AgentProcessState.Ready)
    session.onPrompt = {
      session.sessionFile.value = "/s/err.jsonl"
      session.busy.value = true
      session.appendItem(TimelineItem.User(key = "u1", text = "hi", images = emptyList(), timestamp = 1L))
      session.appendItem(TimelineItem.Error(key = "e1", cause = ErrorCause.Request("HTTP 429 rate limited")))
      session.settleRun()
      "started"
    }
    val opener = FakeAgentOpener(session)
    val executor = AgentProcessJobExecutor(opener, errorText = ::formatCause)

    val result = executor.execute(sampleJob()) {}
    assertEquals(ScheduleRunStatus.Failed, result.status)
    assertEquals("/s/err.jsonl", result.sessionPath)
    assertEquals("HTTP 429 rate limited", result.error)
    assertNull(result.summary)
  }

  @Test
  fun waitsForSlowStartingTurnAndFailsExplicitlyIfTurnNeverStarts() = runTest {
    // 1. Slow-starting turn (agent_start arrives after 5s, beyond old 2s grace) completes normally.
    val slowSession = FakeAgentSession(initialState = AgentProcessState.Ready)
    slowSession.appendItem(TimelineItem.User(key = "u0", text = "old", images = emptyList(), timestamp = 1L))
    slowSession.appendItem(
      TimelineItem.AssistantText(key = "a0", markdown = "stale summary", streaming = false, timestamp = 2L),
    )
    slowSession.onPrompt = {
      backgroundScope.launch {
        delay(5_000L)
        slowSession.busy.value = true
        slowSession.appendItem(TimelineItem.User(key = "u1", text = "new", images = emptyList(), timestamp = 3L))
        delay(1_000L)
        slowSession.appendItem(
          TimelineItem.AssistantText(key = "a1", markdown = "fresh summary", streaming = false, timestamp = 4L),
        )
        slowSession.settleRun()
      }
      "started"
    }
    val slowExecutor = AgentProcessJobExecutor(FakeAgentOpener(slowSession), errorText = ::formatCause)
    val slowDeferred = async { slowExecutor.execute(sampleJob()) {} }
    runCurrent()
    advanceTimeBy(7_000L)
    runCurrent()
    val slowResult = slowDeferred.await()
    assertEquals(ScheduleRunStatus.Succeeded, slowResult.status)
    assertEquals("fresh summary", slowResult.summary)

    // 2. Prompt acknowledged as "started" but turn never starts -> must fail instead of returning stale summary.
    val stuckSession = FakeAgentSession(initialState = AgentProcessState.Ready)
    stuckSession.appendItem(TimelineItem.User(key = "u0", text = "old", images = emptyList(), timestamp = 1L))
    stuckSession.appendItem(
      TimelineItem.AssistantText(key = "a0", markdown = "stale summary", streaming = false, timestamp = 2L),
    )
    stuckSession.onPrompt = { "started" }
    val stuckExecutor = AgentProcessJobExecutor(
      opener = FakeAgentOpener(stuckSession),
      errorText = ::formatCause,
      turnStartGraceMillis = 15_000L,
    )
    val stuckDeferred = async { stuckExecutor.execute(sampleJob()) {} }
    runCurrent()
    advanceTimeBy(16_000L)
    runCurrent()
    val stuckResult = stuckDeferred.await()
    assertEquals(ScheduleRunStatus.Failed, stuckResult.status)
    assertNull(stuckResult.summary)
    assertTrue(stuckSession.aborted)
  }

  @Test
  fun ignoresPreTurnCompactionEventsBeforeAgentStartAndWaitsForRealTurn() = runTest {
    val session = FakeAgentSession(initialState = AgentProcessState.Ready)
    session.appendItem(TimelineItem.User(key = "u0", text = "old", images = emptyList(), timestamp = 1L))
    session.appendItem(
      TimelineItem.AssistantText(key = "a0", markdown = "stale summary", streaming = false, timestamp = 2L),
    )
    session.onPrompt = {
      // Simulate compaction_start -> compaction_end completing inside prompt() before agent_start arrives.
      session.emitNonTurnTimelineUpdate()
      backgroundScope.launch {
        delay(3_000L)
        session.busy.value = true
        session.appendItem(TimelineItem.User(key = "u1", text = "new", images = emptyList(), timestamp = 3L))
        delay(1_000L)
        session.appendItem(
          TimelineItem.AssistantText(
            key = "a1",
            markdown = "fresh post-compaction summary",
            streaming = false,
            timestamp = 4L,
          ),
        )
        session.settleRun()
      }
      "started"
    }

    val executor = AgentProcessJobExecutor(FakeAgentOpener(session), errorText = ::formatCause)
    val deferred = async { executor.execute(sampleJob()) {} }
    runCurrent()
    // Must still be waiting at 1s (did NOT exit prematurely on compaction_end with stale summary).
    advanceTimeBy(1_000L)
    runCurrent()
    assertTrue(deferred.isActive)

    advanceTimeBy(4_000L)
    runCurrent()
    val result = deferred.await()
    assertEquals(ScheduleRunStatus.Succeeded, result.status)
    assertEquals("fresh post-compaction summary", result.summary)
  }

  @Test
  fun handledTurnWithoutNewUserItemDoesNotLeakStaleSummary() = runTest {
    // 1. "handled" slash command appending nothing -> summary must be null, not the prior turn's AssistantText.
    val noOutputSession = FakeAgentSession(initialState = AgentProcessState.Ready)
    noOutputSession.appendItem(TimelineItem.User(key = "u0", text = "old", images = emptyList(), timestamp = 1L))
    noOutputSession.appendItem(
      TimelineItem.AssistantText(key = "a0", markdown = "stale summary", streaming = false, timestamp = 2L),
    )
    noOutputSession.onPrompt = { "handled" }
    val noOutputExecutor = AgentProcessJobExecutor(FakeAgentOpener(noOutputSession), errorText = ::formatCause)
    val noOutputResult = noOutputExecutor.execute(sampleJob()) {}
    assertEquals(ScheduleRunStatus.Succeeded, noOutputResult.status)
    assertNull(noOutputResult.summary)

    // 2. "handled" command appending a fresh AssistantText without a User item -> captures only the new item.
    val freshOutputSession = FakeAgentSession(initialState = AgentProcessState.Ready)
    freshOutputSession.appendItem(TimelineItem.User(key = "u0", text = "old", images = emptyList(), timestamp = 1L))
    freshOutputSession.appendItem(
      TimelineItem.AssistantText(key = "a0", markdown = "stale summary", streaming = false, timestamp = 2L),
    )
    freshOutputSession.onPrompt = {
      freshOutputSession.appendItem(
        TimelineItem.AssistantText(key = "a1", markdown = "slash output", streaming = false, timestamp = 3L),
      )
      "handled"
    }
    val freshExecutor = AgentProcessJobExecutor(FakeAgentOpener(freshOutputSession), errorText = ::formatCause)
    val freshResult = freshExecutor.execute(sampleJob()) {}
    assertEquals(ScheduleRunStatus.Succeeded, freshResult.status)
    assertEquals("slash output", freshResult.summary)
  }

  @Test
  fun fastTurnObservedViaCompletedRunsSucceedsWithoutSeeingBusy() = runTest {
    val session = FakeAgentSession(initialState = AgentProcessState.Ready)
    session.onPrompt = {
      // Turn completes synchronously before awaitTurnCompletion checks busy.
      session.appendItem(TimelineItem.User(key = "u1", text = "ping", images = emptyList(), timestamp = 1L))
      session.appendItem(TimelineItem.AssistantText(key = "a1", markdown = "pong", streaming = false, timestamp = 2L))
      session.settleRun()
      "started"
    }
    val executor = AgentProcessJobExecutor(FakeAgentOpener(session), errorText = ::formatCause)
    val result = executor.execute(sampleJob()) {}
    assertEquals(ScheduleRunStatus.Succeeded, result.status)
    assertEquals("pong", result.summary)
  }

  @Test
  fun chatTimelineCompletedRunsIgnoresCompactionAndIncrementsOnAgentSettled() {
    val timeline = ChatTimeline()
    assertEquals(0L, timeline.completedRuns)

    timeline.apply(JSONObject().put("type", "compaction_start"))
    timeline.apply(JSONObject().put("type", "compaction_end").put("aborted", false))
    timeline.apply(JSONObject().put("type", "queue_update"))
    assertEquals(0L, timeline.completedRuns)

    timeline.apply(JSONObject().put("type", "agent_start"))
    assertEquals(0L, timeline.completedRuns)

    timeline.apply(JSONObject().put("type", "agent_settled").put("aborted", false))
    assertEquals(1L, timeline.completedRuns)
  }

  @Test
  fun connectTimeoutAndStartingCancellationParkHiddenSession() = runTest {
    val session = FakeAgentSession(initialState = AgentProcessState.Starting)
    val opener = FakeAgentOpener(session)
    val executor = AgentProcessJobExecutor(
      opener = opener,
      errorText = ::formatCause,
      connectTimeoutMillis = 10_000L,
    )
    val timedOut = async { executor.execute(sampleJob()) {} }
    runCurrent()
    advanceTimeBy(11_000L)
    runCurrent()
    val result = timedOut.await()
    assertEquals(ScheduleRunStatus.TimedOut, result.status)
    assertEquals(1, opener.parkedCount)
    assertEquals(false, session.aborted)

    // External cancellation (e.g. maxRunSeconds < connectTimeoutMillis) while Starting also parks cleanly.
    val session2 = FakeAgentSession(initialState = AgentProcessState.Starting)
    val opener2 = FakeAgentOpener(session2)
    val executor2 = AgentProcessJobExecutor(opener2, errorText = ::formatCause)
    val job = launch {
      try {
        executor2.execute(sampleJob()) {}
      } catch (_: CancellationException) {
        // Expected on cancellation.
      }
    }
    runCurrent()
    job.cancel()
    runCurrent()
    assertEquals(1, opener2.parkedCount)
  }

  @Test
  fun cancellationWhileRunningAbortsAndPreservesCreatedSessionPath() = runTest {
    val started = CompletableDeferred<Unit>()
    val session = FakeAgentSession(initialState = AgentProcessState.Ready)
    session.onPrompt = {
      session.sessionFile.value = "/s/created-before-timeout.jsonl"
      session.busy.value = true
      started.complete(Unit)
      "started"
    }
    val opener = FakeAgentOpener(session)
    val executor = AgentProcessJobExecutor(opener, errorText = ::formatCause)
    var capturedPath: String? = null
    val job = launch {
      try {
        executor.execute(sampleJob(sessionPath = null)) { path -> capturedPath = path }
      } catch (_: CancellationException) {
        // Expected on timeout/cancel.
      }
    }
    runCurrent()
    started.await()
    job.cancel()
    runCurrent()

    assertEquals("/s/created-before-timeout.jsonl", capturedPath)
    assertTrue(session.aborted)
    assertTrue(session.stoppedBackground)
    assertEquals(0, opener.parkedCount)
  }

  @Test
  fun tooManyRunningReturnsSkipped() = runTest {
    val opener = object : ScheduledAgentOpener {
      override suspend fun open(
        cwd: String,
        sessionPath: String?,
        options: ProcessLaunchOptions,
      ): ScheduledAgentSession = throw TooManyRunning(open = 2, limit = 2)

      override suspend fun parkIfHidden(session: ScheduledAgentSession) = Unit
    }
    val executor = AgentProcessJobExecutor(opener, errorText = ::formatCause)
    val result = executor.execute(sampleJob()) {}
    assertEquals(ScheduleRunStatus.Skipped, result.status)
  }

  private fun formatCause(cause: ErrorCause): String = when (cause) {
    is ErrorCause.Request -> cause.message ?: "Request failed"
    is ErrorCause.Compaction -> cause.message ?: "Compaction failed"
    is ErrorCause.Extension -> "${cause.path}: ${cause.error}"
  }

  private fun sampleJob(sessionPath: String? = "/s/existing.jsonl") = ScheduledJob(
    id = "job-1",
    name = "Nightly",
    cwd = "/w/app",
    sessionPath = sessionPath,
    prompt = "Run checks",
    spec = ScheduleSpec.Interval(60, 0L),
    timezone = "UTC",
    enabled = true,
    createdAt = 0L,
    updatedAt = 0L,
  )

  private class FakeAgentOpener(private val session: FakeAgentSession) : ScheduledAgentOpener {
    var parkedCount = 0

    override suspend fun open(
      cwd: String,
      sessionPath: String?,
      options: ProcessLaunchOptions,
    ): ScheduledAgentSession {
      assertTrue(options.allowBackground)
      return session
    }

    override suspend fun parkIfHidden(session: ScheduledAgentSession) {
      parkedCount++
    }
  }

  private class FakeAgentSession(initialState: AgentProcessState) : ScheduledAgentSession {
    override val state = MutableStateFlow(initialState)
    override val busy = MutableStateFlow(false)
    override val sessionFile = MutableStateFlow<String?>(null)
    override val timelineState = MutableStateFlow(TimelineState())
    override var completedRuns: Long = 0L
      private set

    var onPrompt: suspend (String) -> String = { "started" }
    var aborted = false
    var stoppedBackground = false

    fun appendItem(item: TimelineItem) {
      val current = timelineState.value
      timelineState.value = current.copy(items = current.items + item)
    }

    fun emitNonTurnTimelineUpdate() {
      val current = timelineState.value
      timelineState.value = current.copy(thinkingLevel = "compacting")
      timelineState.value = current.copy(thinkingLevel = null)
    }

    fun settleRun() {
      completedRuns++
      busy.value = false
      val current = timelineState.value
      timelineState.value = current.copy(items = current.items.toList())
    }

    override suspend fun prompt(message: String): String = onPrompt(message)

    override suspend fun abort() {
      aborted = true
    }

    override suspend fun stopBackgroundTasks() {
      stoppedBackground = true
    }
  }
}
