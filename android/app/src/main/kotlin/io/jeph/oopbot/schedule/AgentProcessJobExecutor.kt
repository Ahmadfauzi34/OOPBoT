package io.jeph.oopbot.schedule

import android.content.Context
import io.jeph.oopbot.agent.AgentProcess
import io.jeph.oopbot.agent.AgentProcessState
import io.jeph.oopbot.agent.AgentRegistry
import io.jeph.oopbot.agent.ErrorCause
import io.jeph.oopbot.agent.ProcessLaunchOptions
import io.jeph.oopbot.agent.RpcError
import io.jeph.oopbot.agent.TimelineItem
import io.jeph.oopbot.agent.TimelineState
import io.jeph.oopbot.agent.TooManyRunning
import io.jeph.oopbot.agent.text
import kotlinx.coroutines.CancellationException
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.NonCancellable
import kotlinx.coroutines.flow.StateFlow
import kotlinx.coroutines.flow.combine
import kotlinx.coroutines.flow.first
import kotlinx.coroutines.withContext
import kotlinx.coroutines.withTimeoutOrNull

/** Narrow session surface used by [AgentProcessJobExecutor] so all execution edge cases are JVM-testable. */
internal interface ScheduledAgentSession {
  val state: StateFlow<AgentProcessState>
  val busy: StateFlow<Boolean>
  val sessionFile: StateFlow<String?>
  val timelineState: StateFlow<TimelineState>
  val completedRuns: Long

  suspend fun prompt(message: String): String

  suspend fun abort()

  suspend fun stopBackgroundTasks()
}

/** Narrow opener/cleanup surface bridging [AgentProcessJobExecutor] to [AgentRegistry]. */
internal interface ScheduledAgentOpener {
  suspend fun open(cwd: String, sessionPath: String?, options: ProcessLaunchOptions): ScheduledAgentSession

  suspend fun parkIfHidden(session: ScheduledAgentSession)
}

/**
 * Production [ScheduledJobExecutor] that wakes/reuses an [AgentProcess] through [AgentRegistry] with
 * `allowBackground = true`, sends the scheduled prompt, waits for the turn to settle, inspects timeline errors, and
 * cancels/parks cleanly on timeout or cancellation.
 */
internal class AgentProcessJobExecutor(
  private val opener: ScheduledAgentOpener,
  private val errorText: (ErrorCause) -> String,
  private val connectTimeoutMillis: Long = CONNECT_TIMEOUT_MILLIS,
  private val turnStartGraceMillis: Long = TURN_START_GRACE_MILLIS,
) : ScheduledJobExecutor {
  constructor(registry: AgentRegistry<AgentProcess>, context: Context) : this(
    opener = RegistryAgentOpener(registry),
    errorText = { cause -> cause.text(context.applicationContext.resources) },
  )

  override suspend fun execute(job: ScheduledJob, onSessionPath: (String) -> Unit): JobExecutionResult {
    val session = try {
      opener.open(
        cwd = job.cwd,
        sessionPath = job.sessionPath,
        options = ProcessLaunchOptions(allowBackground = true, engineOverride = job.engine),
      )
    } catch (error: TooManyRunning) {
      return JobExecutionResult(
        status = ScheduleRunStatus.Skipped,
        sessionPath = job.sessionPath,
        error = runCatching { error.message }.getOrElse { "Too many running agents (${error.open}/${error.limit})" },
      )
    }

    return try {
      session.sessionFile.value?.let(onSessionPath)
      runOnSession(session, job, onSessionPath)
    } catch (cancelled: CancellationException) {
      withContext(NonCancellable) {
        abortSession(session)
      }
      throw cancelled
    } finally {
      // Runs before CancellationException/TimeoutCancellationException unwinds so TimedOut preserves sessionPath.
      session.sessionFile.value?.let(onSessionPath)
    }
  }

  private suspend fun runOnSession(
    session: ScheduledAgentSession,
    job: ScheduledJob,
    onSessionPath: (String) -> Unit,
  ): JobExecutionResult {
    val readyState = withTimeoutOrNull(connectTimeoutMillis) {
      session.state.first { it !is AgentProcessState.Starting && it !is AgentProcessState.Installing }
    } ?: return abortAndFail(session, job, ScheduleRunStatus.TimedOut, "Timed out connecting to agent")

    session.sessionFile.value?.let(onSessionPath)
    val readyError = readyStateFailure(readyState, session, job)
    if (readyError != null) return readyError

    val runsBefore = session.completedRuns
    val prePromptItemKeys = session.timelineState.value.items.mapTo(HashSet()) { it.key }
    val disposition = try {
      session.prompt(job.prompt)
    } catch (error: RpcError) {
      return JobExecutionResult(
        status = ScheduleRunStatus.Failed,
        sessionPath = session.sessionFile.value ?: job.sessionPath,
        error = error.message,
      )
    }

    return if (disposition != "handled" && !awaitTurnCompletion(session, runsBefore)) {
      abortAndFail(session, job, ScheduleRunStatus.Failed, TURN_START_TIMEOUT_ERROR)
    } else {
      buildFinalResult(session, job, prePromptItemKeys)
    }
  }

  private fun readyStateFailure(
    readyState: AgentProcessState,
    session: ScheduledAgentSession,
    job: ScheduledJob,
  ): JobExecutionResult? = when (readyState) {
    AgentProcessState.Ready -> null

    is AgentProcessState.Failed -> JobExecutionResult(
      status = ScheduleRunStatus.Failed,
      sessionPath = session.sessionFile.value ?: job.sessionPath,
      error = readyState.message,
    )

    is AgentProcessState.Exited -> JobExecutionResult(
      status = ScheduleRunStatus.Failed,
      sessionPath = session.sessionFile.value ?: job.sessionPath,
      error = readyState.stderr.ifBlank { "Agent exited (code=${readyState.code ?: "unknown"})" },
    )

    AgentProcessState.Starting, is AgentProcessState.Installing -> JobExecutionResult(
      status = ScheduleRunStatus.Failed,
      sessionPath = session.sessionFile.value ?: job.sessionPath,
      error = "Agent did not become ready",
    )
  }

  private fun buildFinalResult(
    session: ScheduledAgentSession,
    job: ScheduledJob,
    prePromptItemKeys: Set<String>,
  ): JobExecutionResult {
    val finalSession = session.sessionFile.value ?: job.sessionPath
    return when (val finalState = session.state.value) {
      is AgentProcessState.Failed -> JobExecutionResult(
        status = ScheduleRunStatus.Failed,
        sessionPath = finalSession,
        error = finalState.message,
      )

      is AgentProcessState.Exited -> JobExecutionResult(
        status = ScheduleRunStatus.Failed,
        sessionPath = finalSession,
        error = finalState.stderr.ifBlank { "Agent exited mid-run (code=${finalState.code ?: "unknown"})" },
      )

      else -> when (val outcome = extractTurnOutcome(session.timelineState.value.items, prePromptItemKeys)) {
        is TurnOutcome.Error -> JobExecutionResult(
          status = ScheduleRunStatus.Failed,
          sessionPath = finalSession,
          error = outcome.message,
        )

        is TurnOutcome.Success -> JobExecutionResult(
          status = ScheduleRunStatus.Succeeded,
          sessionPath = finalSession,
          summary = outcome.summary,
        )
      }
    }
  }

  /**
   * Waits for the prompted turn to begin (`busy == true`), settle (`completedRuns > runsBefore`), or reach a terminal
   * process state, ignoring non-turn events (such as pre-turn compaction or queue updates). Returns `false` if the turn
   * never starts within [turnStartGraceMillis].
   */
  private suspend fun awaitTurnCompletion(session: ScheduledAgentSession, runsBefore: Long): Boolean {
    val startFlow = combine(session.busy, session.state, session.timelineState) { busy, state, _ ->
      busy to state
    }
    val started = withTimeoutOrNull(turnStartGraceMillis) {
      startFlow.first { (busy, state) ->
        busy || session.completedRuns > runsBefore || state.isTerminal()
      }
    } ?: return false

    if (session.busy.value && !started.second.isTerminal()) {
      combine(session.busy, session.state) { busy, state -> busy to state }.first { (busy, state) ->
        !busy || state.isTerminal()
      }
    }
    return true
  }

  private suspend fun abortAndFail(
    session: ScheduledAgentSession,
    job: ScheduledJob,
    status: ScheduleRunStatus,
    error: String,
  ): JobExecutionResult {
    withContext(NonCancellable) {
      abortSession(session)
    }
    return JobExecutionResult(
      status = status,
      sessionPath = session.sessionFile.value ?: job.sessionPath,
      error = error,
    )
  }

  private suspend fun abortSession(session: ScheduledAgentSession) {
    val currentState = session.state.value
    if (currentState is AgentProcessState.Starting || currentState is AgentProcessState.Installing) {
      runCatching { opener.parkIfHidden(session) }
      return
    }
    runCatching { session.abort() }
    runCatching { session.stopBackgroundTasks() }
  }

  private fun extractTurnOutcome(items: List<TimelineItem>, prePromptItemKeys: Set<String>): TurnOutcome {
    var summary: String? = null
    for (item in items.asReversed()) {
      if (item.key in prePromptItemKeys) return TurnOutcome.Success(summary)
      when (item) {
        is TimelineItem.Error -> return TurnOutcome.Error(errorText(item.cause).take(MAX_SUMMARY_CHARS))

        is TimelineItem.AssistantText -> {
          if (summary == null && item.markdown.isNotBlank()) summary = item.markdown.take(MAX_SUMMARY_CHARS)
        }

        is TimelineItem.User -> return TurnOutcome.Success(summary)

        else -> Unit
      }
    }
    return TurnOutcome.Success(summary)
  }

  private fun AgentProcessState.isTerminal(): Boolean =
    this is AgentProcessState.Failed || this is AgentProcessState.Exited

  private sealed interface TurnOutcome {
    data class Success(val summary: String?) : TurnOutcome

    data class Error(val message: String) : TurnOutcome
  }

  companion object {
    const val CONNECT_TIMEOUT_MILLIS = 45_000L
    const val TURN_START_GRACE_MILLIS = 15_000L
    private const val MAX_SUMMARY_CHARS = 500
    private const val TURN_START_TIMEOUT_ERROR = "Timed out waiting for agent turn to start"
  }
}

private class RegistryAgentOpener(private val registry: AgentRegistry<AgentProcess>) : ScheduledAgentOpener {
  override suspend fun open(cwd: String, sessionPath: String?, options: ProcessLaunchOptions): ScheduledAgentSession =
    withContext(Dispatchers.Main.immediate) {
      ProcessSessionAdapter(registry.open(cwd = cwd, sessionPath = sessionPath, options = options))
    }

  override suspend fun parkIfHidden(session: ScheduledAgentSession) {
    val adapter = session as? ProcessSessionAdapter ?: return
    withContext(Dispatchers.Main.immediate) {
      registry.parkIfHidden(adapter.process)
    }
  }
}

private class ProcessSessionAdapter(val process: AgentProcess) : ScheduledAgentSession {
  override val state: StateFlow<AgentProcessState> get() = process.state
  override val busy: StateFlow<Boolean> get() = process.busy
  override val sessionFile: StateFlow<String?> get() = process.sessionFile
  override val timelineState: StateFlow<TimelineState> get() = process.timeline.state
  override val completedRuns: Long get() = process.timeline.completedRuns

  override suspend fun prompt(message: String): String = process.prompt(message)

  override suspend fun abort() = process.abort()

  override suspend fun stopBackgroundTasks() = process.stopBackgroundTasks()
}
