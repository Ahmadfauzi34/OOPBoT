package io.jeph.oopbot.schedule

import android.util.Log
import io.jeph.oopbot.agent.KeepAlive
import java.util.concurrent.atomic.AtomicBoolean
import kotlinx.coroutines.CancellationException
import kotlinx.coroutines.CoroutineScope
import kotlinx.coroutines.Job
import kotlinx.coroutines.NonCancellable
import kotlinx.coroutines.TimeoutCancellationException
import kotlinx.coroutines.isActive
import kotlinx.coroutines.launch
import kotlinx.coroutines.sync.Semaphore
import kotlinx.coroutines.sync.withPermit
import kotlinx.coroutines.withContext
import kotlinx.coroutines.withTimeout

/** Outcome of executing one scheduled job via [ScheduledJobExecutor]. */
internal data class JobExecutionResult(
  val status: ScheduleRunStatus,
  val sessionPath: String? = null,
  val summary: String? = null,
  val error: String? = null,
)

/** Abstraction executing a single [ScheduledJob] (implemented by [AgentProcessJobExecutor] in production). */
internal fun interface ScheduledJobExecutor {
  /** Executes [job], reporting any created session file path via [onSessionPath] so timed-out runs retain it. */
  suspend fun execute(job: ScheduledJob, onSessionPath: (String) -> Unit): JobExecutionResult
}

/**
 * Coordinates headless execution of due scheduled jobs under [KeepAliveMode.Active] holds with per-job overlap guards,
 * a global concurrency cap ([MAX_CONCURRENT_RUNS]), and per-job timeouts so runaway runs cannot leak wake locks.
 */
internal class ScheduleRunner(
  private val repository: ScheduleRepository,
  private val keepAlive: KeepAlive,
  private val executor: ScheduledJobExecutor,
  private val scope: CoroutineScope,
  private val clock: () -> Long = System::currentTimeMillis,
  maxConcurrentRuns: Int = MAX_CONCURRENT_RUNS,
) {
  private val concurrency = Semaphore(maxConcurrentRuns.coerceAtLeast(1))
  private val stateLock = Any()
  private val runningJobIds = HashSet<String>()
  private val queuedJobIds = HashSet<String>()

  /**
   * Claims and dispatches all jobs due at [nowMillis]. Takes ownership of [initialHold] (acquired synchronously inside
   * [ScheduleAlarmReceiver.onReceive] while within Android's alarm FGS exemption window) and invokes [onDispatched]
   * once claiming and alarm re-arming finish (e.g. to release `BroadcastReceiver.PendingResult.finish()`).
   */
  fun dispatchDueJobs(
    initialHold: AutoCloseable? = null,
    nowMillis: Long = clock(),
    onDispatched: (() -> Unit)? = null,
  ): Job {
    if (!scope.isActive) {
      try {
        initialHold?.close()
      } finally {
        onDispatched?.invoke()
      }
      return Job().apply { cancel() }
    }
    val entered = AtomicBoolean(false)
    val launched = scope.launch {
      if (!entered.compareAndSet(false, true)) return@launch
      try {
        val dueJobs = repository.claimDueJobs(nowMillis)
        for (job in dueJobs) {
          launchJob(job, requireEnabled = true)
        }
      } finally {
        // Per-job holds are already acquired before this closes, so KeepAlive never dips to Standby/Off mid-handoff.
        try {
          initialHold?.close()
        } finally {
          onDispatched?.invoke()
        }
      }
    }
    launched.invokeOnCompletion {
      if (entered.compareAndSet(false, true)) {
        try {
          initialHold?.close()
        } finally {
          onDispatched?.invoke()
        }
      }
    }
    return launched
  }

  /** Manually executes [jobId] immediately without modifying its `nextRunAt` schedule. Returns false if not found. */
  suspend fun runNow(jobId: String): Boolean {
    repository.ensureInitialized()
    val job = repository.getJob(jobId) ?: return false
    launchJob(job, requireEnabled = false)
    return true
  }

  private fun launchJob(job: ScheduledJob, requireEnabled: Boolean) {
    if (!scope.isActive) return
    when (decideOverlap(job)) {
      OverlapAction.Queued -> return

      OverlapAction.Skip -> {
        scope.launch {
          repository.recordInstantRun(
            jobId = job.id,
            atMillis = clock(),
            status = ScheduleRunStatus.Skipped,
            sessionPath = job.sessionPath,
            error = OVERLAP_SKIP_ERROR,
          )
        }
        return
      }

      OverlapAction.Start -> Unit
    }

    // Acquire Active hold synchronously before launching coroutine so callers releasing initialHold never dip to Off.
    val jobHold = keepAlive.acquire("scheduled job: ${job.name}")
    val entered = AtomicBoolean(false)
    val launched = scope.launch {
      if (!entered.compareAndSet(false, true)) return@launch
      try {
        concurrency.withPermit {
          executeSingleRun(job, requireEnabled)
        }
      } finally {
        finishJobAndMaybeRunFollowUp(job, jobHold)
      }
    }
    launched.invokeOnCompletion {
      if (entered.compareAndSet(false, true)) {
        synchronized(stateLock) {
          runningJobIds.remove(job.id)
          queuedJobIds.remove(job.id)
        }
        jobHold.close()
      }
    }
  }

  private fun decideOverlap(job: ScheduledJob): OverlapAction = synchronized(stateLock) {
    if (job.id in runningJobIds) {
      if (job.overlapPolicy == OverlapPolicy.Queue) {
        queuedJobIds.add(job.id)
        OverlapAction.Queued
      } else {
        OverlapAction.Skip
      }
    } else {
      runningJobIds.add(job.id)
      OverlapAction.Start
    }
  }

  private suspend fun finishJobAndMaybeRunFollowUp(job: ScheduledJob, jobHold: AutoCloseable) {
    try {
      val runFollowUp = synchronized(stateLock) {
        runningJobIds.remove(job.id)
        queuedJobIds.remove(job.id)
      }
      if (runFollowUp && scope.isActive) {
        val latest = runCatching { withContext(NonCancellable) { repository.getJob(job.id) } }.getOrNull()
        if (latest != null && latest.enabled && scope.isActive) {
          // Acquire queued run's hold before closing this run's hold so Active stays seamless.
          launchJob(latest, requireEnabled = true)
        }
      }
    } finally {
      jobHold.close()
    }
  }

  private suspend fun executeSingleRun(job: ScheduledJob, requireEnabled: Boolean) {
    // Re-read after acquiring semaphore permit so jobs deleted/disabled while queued do not execute or trip FK errors.
    val current = repository.getJob(job.id) ?: return
    if (requireEnabled && !current.enabled && current.spec !is ScheduleSpec.Once) return

    val startedAt = clock()
    val runId = repository.recordRunStart(current.id, startedAt, current.sessionPath) ?: return
    val timeoutMillis = current.maxRunSeconds.toLong() * MILLIS_PER_SECOND
    val result = runExecutorWithTimeout(current, runId, timeoutMillis)
    withContext(NonCancellable) {
      repository.recordRunFinish(
        runId = runId,
        jobId = current.id,
        finishedAt = clock(),
        status = result.status,
        sessionPath = result.sessionPath ?: current.sessionPath,
        summary = result.summary,
        error = result.error,
      )
    }
  }

  // Runner boundary: unexpected executor exceptions must still finalize the schedule_runs row instead of stranding it.
  @Suppress("TooGenericExceptionCaught")
  private suspend fun runExecutorWithTimeout(job: ScheduledJob, runId: Long, timeoutMillis: Long): JobExecutionResult {
    var observedSessionPath: String? = job.sessionPath
    return try {
      withTimeout(timeoutMillis) {
        executor.execute(job) { path -> observedSessionPath = path }
      }
    } catch (_: TimeoutCancellationException) {
      Log.w(TAG, "Scheduled job '${job.name}' (${job.id}) timed out after ${job.maxRunSeconds}s")
      JobExecutionResult(
        status = ScheduleRunStatus.TimedOut,
        sessionPath = observedSessionPath,
        error = "Timed out after ${job.maxRunSeconds}s",
      )
    } catch (cancelled: CancellationException) {
      withContext(NonCancellable) {
        repository.recordRunFinish(
          runId = runId,
          jobId = job.id,
          finishedAt = clock(),
          status = ScheduleRunStatus.Failed,
          sessionPath = observedSessionPath,
          summary = null,
          error = "Cancelled",
        )
      }
      throw cancelled
    } catch (error: Exception) {
      Log.w(TAG, "Scheduled job '${job.name}' (${job.id}) threw unexpected error", error)
      JobExecutionResult(
        status = ScheduleRunStatus.Failed,
        sessionPath = observedSessionPath,
        error = error.message ?: error.javaClass.simpleName,
      )
    }
  }

  private enum class OverlapAction { Start, Skip, Queued }

  companion object {
    const val MAX_CONCURRENT_RUNS = 2
    private const val MILLIS_PER_SECOND = 1_000L
    private const val TAG = "OOPBoT"
    private const val OVERLAP_SKIP_ERROR = "Previous run still active"
  }
}
