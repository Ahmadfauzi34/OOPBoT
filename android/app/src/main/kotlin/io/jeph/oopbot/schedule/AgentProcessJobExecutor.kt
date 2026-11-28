package io.jeph.oopbot.schedule

import android.content.Context
import io.jeph.oopbot.agent.AgentProcess
import io.jeph.oopbot.agent.AgentProcessState
import io.jeph.oopbot.agent.AgentRegistry
import io.jeph.oopbot.agent.ProcessLaunchOptions
import io.jeph.oopbot.agent.RpcError
import io.jeph.oopbot.agent.TimelineItem
import io.jeph.oopbot.agent.TooManyRunning
import io.jeph.oopbot.agent.text
import kotlinx.coroutines.CancellationException
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.NonCancellable
import kotlinx.coroutines.flow.combine
import kotlinx.coroutines.flow.first
import kotlinx.coroutines.withContext
import kotlinx.coroutines.withTimeoutOrNull

/**
 * Production [ScheduledJobExecutor] that wakes/reuses an [AgentProcess] through [AgentRegistry] with
 * `allowBackground = true`, sends the scheduled prompt, waits for the turn to settle, and aborts cleanly on timeout.
 */
internal class AgentProcessJobExecutor(private val registry: AgentRegistry<AgentProcess>, context: Context) :
  ScheduledJobExecutor {
  private val resources = context.applicationContext.resources

  override suspend fun execute(job: ScheduledJob): JobExecutionResult {
    val process = try {
      withContext(Dispatchers.Main.immediate) {
        registry.open(
          cwd = job.cwd,
          sessionPath = job.sessionPath,
          options = ProcessLaunchOptions(allowBackground = true, engineOverride = job.engine),
        )
      }
    } catch (error: TooManyRunning) {
      return JobExecutionResult(
        status = ScheduleRunStatus.Skipped,
        sessionPath = job.sessionPath,
        error = error.message,
      )
    }

    return try {
      runOnProcess(process, job)
    } catch (cancelled: CancellationException) {
      withContext(NonCancellable) {
        runCatching { process.abort() }
        runCatching { process.stopBackgroundTasks() }
      }
      throw cancelled
    }
  }

  private suspend fun runOnProcess(process: AgentProcess, job: ScheduledJob): JobExecutionResult {
    val readyState = withTimeoutOrNull(CONNECT_TIMEOUT_MILLIS) {
      process.state.first { it !is AgentProcessState.Starting && it !is AgentProcessState.Installing }
    } ?: return abortAndFail(process, job, ScheduleRunStatus.TimedOut, "Timed out connecting to agent")

    val readyError = readyStateFailure(readyState, process, job)
    if (readyError != null) return readyError

    val disposition = try {
      process.prompt(job.prompt)
    } catch (error: RpcError) {
      return JobExecutionResult(
        status = ScheduleRunStatus.Failed,
        sessionPath = process.sessionFile.value ?: job.sessionPath,
        error = error.message,
      )
    }

    if (disposition != "handled") {
      awaitTurnCompletion(process)
    }

    return buildFinalResult(process, job)
  }

  private fun readyStateFailure(
    readyState: AgentProcessState,
    process: AgentProcess,
    job: ScheduledJob,
  ): JobExecutionResult? = when (readyState) {
    AgentProcessState.Ready -> null

    is AgentProcessState.Failed -> JobExecutionResult(
      status = ScheduleRunStatus.Failed,
      sessionPath = process.sessionFile.value ?: job.sessionPath,
      error = readyState.message,
    )

    is AgentProcessState.Exited -> JobExecutionResult(
      status = ScheduleRunStatus.Failed,
      sessionPath = process.sessionFile.value ?: job.sessionPath,
      error = readyState.stderr.ifBlank { "Agent exited (code=${readyState.code ?: "unknown"})" },
    )

    AgentProcessState.Starting, is AgentProcessState.Installing -> JobExecutionResult(
      status = ScheduleRunStatus.Failed,
      sessionPath = process.sessionFile.value ?: job.sessionPath,
      error = "Agent did not become ready",
    )
  }

  private fun buildFinalResult(process: AgentProcess, job: ScheduledJob): JobExecutionResult {
    val finalSession = process.sessionFile.value ?: job.sessionPath
    return when (val finalState = process.state.value) {
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

      else -> JobExecutionResult(
        status = ScheduleRunStatus.Succeeded,
        sessionPath = finalSession,
        summary = extractSummary(process.timeline.state.value.items),
      )
    }
  }

  private suspend fun awaitTurnCompletion(process: AgentProcess) {
    // Prompt ack can arrive milliseconds before agent_start flips busy=true; observe both busy and terminal state.
    val statusFlow = combine(process.busy, process.state) { busy, state -> busy to state }
    withTimeoutOrNull(TURN_START_GRACE_MILLIS) {
      statusFlow.first { (busy, state) ->
        busy || state is AgentProcessState.Failed || state is AgentProcessState.Exited
      }
    }
    if (process.busy.value) {
      statusFlow.first { (busy, state) ->
        !busy || state is AgentProcessState.Failed || state is AgentProcessState.Exited
      }
    }
  }

  private suspend fun abortAndFail(
    process: AgentProcess,
    job: ScheduledJob,
    status: ScheduleRunStatus,
    error: String,
  ): JobExecutionResult {
    withContext(NonCancellable) {
      runCatching { process.abort() }
      runCatching { process.stopBackgroundTasks() }
    }
    return JobExecutionResult(
      status = status,
      sessionPath = process.sessionFile.value ?: job.sessionPath,
      error = error,
    )
  }

  private fun extractSummary(items: List<TimelineItem>): String? {
    for (item in items.asReversed()) {
      when (item) {
        is TimelineItem.AssistantText -> if (item.markdown.isNotBlank()) return item.markdown.take(MAX_SUMMARY_CHARS)
        is TimelineItem.Error -> return item.cause.text(resources).take(MAX_SUMMARY_CHARS)
        is TimelineItem.User -> return null
        else -> Unit
      }
    }
    return null
  }

  private companion object {
    const val CONNECT_TIMEOUT_MILLIS = 45_000L
    const val TURN_START_GRACE_MILLIS = 2_000L
    const val MAX_SUMMARY_CHARS = 500
  }
}
