package io.jeph.oopbot.schedule

import io.jeph.oopbot.data.ScheduleDbStore
import java.time.ZoneId
import java.util.UUID
import kotlinx.coroutines.CoroutineDispatcher
import kotlinx.coroutines.CoroutineScope
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.flow.Flow
import kotlinx.coroutines.flow.MutableStateFlow
import kotlinx.coroutines.flow.StateFlow
import kotlinx.coroutines.flow.asStateFlow
import kotlinx.coroutines.flow.distinctUntilChanged
import kotlinx.coroutines.flow.filterNotNull
import kotlinx.coroutines.flow.map
import kotlinx.coroutines.launch
import kotlinx.coroutines.sync.Mutex
import kotlinx.coroutines.sync.withLock
import kotlinx.coroutines.withContext

/** Injectable time & timezone source for deterministic testing. */
internal class ScheduleClock(
  val nowMillis: () -> Long = System::currentTimeMillis,
  val systemZoneId: () -> String = { ZoneId.systemDefault().id },
)

/**
 * Single source of truth for scheduled jobs, serializing SQLite mutations and single-alarm [AlarmScheduler] updates
 * off the main thread.
 */
internal class ScheduleRepository(
  private val store: ScheduleDbStore,
  private val alarmScheduler: AlarmScheduler,
  private val preferExactAlarms: StateFlow<Boolean>,
  scope: CoroutineScope,
  private val ioDispatcher: CoroutineDispatcher = Dispatchers.IO,
  private val clock: ScheduleClock = ScheduleClock(),
) {
  private val mutex = Mutex()

  @Volatile
  private var initialized = false

  /** Null until [ensureInitialized] loads persisted jobs so component-enable watchers never see a spurious false. */
  private val loadedJobs = MutableStateFlow<List<ScheduledJob>?>(null)
  private val jobsState = MutableStateFlow<List<ScheduledJob>>(emptyList())

  /** Current list of configured jobs for UI observers. */
  val jobs: StateFlow<List<ScheduledJob>> = jobsState.asStateFlow()

  /** Emits whether at least one enabled job exists once initial load completes (drives `ScheduleAlarmReceiver`). */
  val hasEnabledJobs: Flow<Boolean> = loadedJobs
    .filterNotNull()
    .map { list -> list.any { it.enabled } }
    .distinctUntilChanged()

  init {
    scope.launch(ioDispatcher) {
      preferExactAlarms.collect {
        if (initialized) resyncAlarm()
      }
    }
  }

  /** Recovers interrupted `running` records once, loads jobs into [jobs], and arms/cancels the wake alarm. */
  suspend fun ensureInitialized() {
    mutex.withLock {
      if (initialized) return
      withContext(ioDispatcher) {
        store.recoverInterruptedRuns(clock.nowMillis(), INTERRUPTED_ERROR)
        refreshAndSyncAlarmLocked()
        initialized = true
      }
    }
  }

  /** Validates and creates a new job, computing its initial `nextRunAt` and syncing the wake alarm. */
  suspend fun createJob(draft: NewScheduledJob): ScheduledJob = mutex.withLock {
    ensureInitializedLocked()
    validateJobFields(draft.name, draft.cwd, draft.prompt)
    val now = clock.nowMillis()
    val (timezone, normalizedSpec) = normalizeTimezoneAndSpec(draft.timezone, draft.spec)
    ScheduleCalculator.validate(normalizedSpec, timezone)
    val clampedMaxRun = draft.maxRunSeconds.coerceIn(
      ScheduleSpec.MIN_MAX_RUN_SECONDS,
      ScheduleSpec.MAX_MAX_RUN_SECONDS,
    )
    val nextRunAt = if (draft.enabled) ScheduleCalculator.nextTriggerAfter(normalizedSpec, now) else null
    val effectiveEnabled = if (normalizedSpec is ScheduleSpec.Once) {
      draft.enabled && nextRunAt != null
    } else {
      draft.enabled
    }
    val job = ScheduledJob(
      id = UUID.randomUUID().toString(),
      name = draft.name.trim(),
      cwd = draft.cwd.trim(),
      sessionPath = draft.sessionPath,
      prompt = draft.prompt.trim(),
      engine = draft.engine?.takeIf { it.isNotBlank() },
      spec = normalizedSpec,
      timezone = timezone,
      enabled = effectiveEnabled,
      overlapPolicy = draft.overlapPolicy,
      misfirePolicy = draft.misfirePolicy,
      maxRunSeconds = clampedMaxRun,
      nextRunAt = nextRunAt,
      createdAt = now,
      updatedAt = now,
    )
    withContext(ioDispatcher) {
      store.upsertJob(job)
      refreshAndSyncAlarmLocked()
    }
    job
  }

  /**
   * Validates and updates an existing job in place (preserving run history AND live `lastRunAt`/`lastStatus`/
   * `lastError`/`createdAt` fields from SQLite), normalizing timezone and recomputing `nextRunAt`.
   * Returns `null` without resurrecting the row if [job] was deleted concurrently.
   */
  suspend fun updateJob(job: ScheduledJob): ScheduledJob? = mutex.withLock {
    ensureInitializedLocked()
    validateJobFields(job.name, job.cwd, job.prompt)
    val now = clock.nowMillis()
    val (timezone, normalizedSpec) = normalizeTimezoneAndSpec(job.timezone, job.spec)
    ScheduleCalculator.validate(normalizedSpec, timezone)
    val clampedMaxRun = job.maxRunSeconds.coerceIn(
      ScheduleSpec.MIN_MAX_RUN_SECONDS,
      ScheduleSpec.MAX_MAX_RUN_SECONDS,
    )
    val nextRunAt = if (job.enabled) ScheduleCalculator.nextTriggerAfter(normalizedSpec, now) else null
    val effectiveEnabled = if (normalizedSpec is ScheduleSpec.Once) job.enabled && nextRunAt != null else job.enabled
    withContext(ioDispatcher) {
      val existing = store.getJob(job.id) ?: return@withContext null
      val updated = job.copy(
        name = job.name.trim(),
        cwd = job.cwd.trim(),
        prompt = job.prompt.trim(),
        engine = job.engine?.takeIf { it.isNotBlank() },
        spec = normalizedSpec,
        timezone = timezone,
        maxRunSeconds = clampedMaxRun,
        enabled = effectiveEnabled,
        nextRunAt = nextRunAt,
        lastRunAt = existing.lastRunAt,
        lastStatus = existing.lastStatus,
        lastError = existing.lastError,
        createdAt = existing.createdAt,
        updatedAt = now,
      )
      store.upsertJob(updated)
      refreshAndSyncAlarmLocked()
      updated
    }
  }

  /** Toggles [enabled] on [jobId], recomputing `nextRunAt` when re-enabling, and syncs the wake alarm. */
  suspend fun setEnabled(jobId: String, enabled: Boolean): ScheduledJob? = mutex.withLock {
    ensureInitializedLocked()
    val now = clock.nowMillis()
    withContext(ioDispatcher) {
      val current = store.getJob(jobId) ?: return@withContext null
      val nextRun = if (enabled) ScheduleCalculator.nextTriggerAfter(current.spec, now) else null
      val effectiveEnabled = if (current.spec is ScheduleSpec.Once) enabled && nextRun != null else enabled
      store.updateJobScheduleState(jobId, nextRun, effectiveEnabled, now)
      refreshAndSyncAlarmLocked()
      store.getJob(jobId)
    }
  }

  /** Deletes [jobId] (and its cascaded `schedule_runs` history) and resyncs the wake alarm. */
  suspend fun deleteJob(jobId: String) = mutex.withLock {
    ensureInitializedLocked()
    withContext(ioDispatcher) {
      store.deleteJob(jobId)
      refreshAndSyncAlarmLocked()
    }
  }

  suspend fun getJob(jobId: String): ScheduledJob? = withContext(ioDispatcher) {
    store.getJob(jobId)
  }

  suspend fun listRuns(jobId: String, limit: Int = ScheduleDbStore.MAX_RUNS_PER_JOB): List<ScheduleRun> =
    withContext(ioDispatcher) { store.listRuns(jobId, limit) }

  /**
   * Atomically claims all enabled jobs due at or before [nowMillis], advancing each job's `nextRunAt` (and recording
   * missed triggers under [MisfirePolicy.Skip]) BEFORE execution starts so the next alarm is always persisted.
   */
  suspend fun claimDueJobs(nowMillis: Long = clock.nowMillis()): List<ScheduledJob> = mutex.withLock {
    ensureInitializedLocked()
    val exactActive = preferExactAlarms.value && alarmScheduler.canScheduleExactAlarms()
    val graceMillis = ScheduleCalculator.misfireGraceMillis(exactActive)
    withContext(ioDispatcher) {
      val due = store.listDueJobs(nowMillis)
      val toRun = mutableListOf<ScheduledJob>()
      for (job in due) {
        val eval = ScheduleCalculator.evaluateDue(job, nowMillis, graceMillis)
        if (eval.missed) {
          store.recordInstantRun(
            jobId = job.id,
            atMillis = nowMillis,
            status = ScheduleRunStatus.Missed,
            sessionPath = job.sessionPath,
            error = MISSED_ERROR,
          )
          store.updateJobScheduleState(job.id, eval.nextRunAt, eval.enabled, nowMillis)
        } else {
          store.updateJobScheduleState(job.id, eval.nextRunAt, eval.enabled, nowMillis)
          if (eval.shouldRun) toRun += job
        }
      }
      refreshAndSyncAlarmLocked()
      toRun
    }
  }

  /** Re-arms the single wake alarm after boot or app update without altering stored `nextRunAt` values. */
  suspend fun resyncAfterBoot() = mutex.withLock {
    ensureInitializedLocked()
    withContext(ioDispatcher) { refreshAndSyncAlarmLocked() }
  }

  /** Recomputes `nextRunAt` for enabled jobs after `ACTION_TIMEZONE_CHANGED` or `ACTION_TIME_CHANGED`. */
  suspend fun recomputeForClockChange(timezoneChanged: Boolean) = mutex.withLock {
    ensureInitializedLocked()
    val now = clock.nowMillis()
    val zone = clock.systemZoneId()
    withContext(ioDispatcher) {
      for (job in store.listJobs()) {
        if (!job.enabled) continue
        val recomputed = ScheduleCalculator.recomputeForClockChange(job, now, zone, timezoneChanged)
        if (recomputed != job) store.upsertJob(recomputed)
      }
      refreshAndSyncAlarmLocked()
    }
  }

  suspend fun recordRunStart(jobId: String, startedAt: Long, sessionPath: String?): Long? = mutex.withLock {
    ensureInitializedLocked()
    withContext(ioDispatcher) {
      val runId = store.recordRunStart(jobId, startedAt, sessionPath)
      refreshSnapshotLocked()
      runId
    }
  }

  suspend fun recordInstantRun(
    jobId: String,
    atMillis: Long,
    status: ScheduleRunStatus,
    sessionPath: String? = null,
    summary: String? = null,
    error: String? = null,
  ): Long? = mutex.withLock {
    ensureInitializedLocked()
    withContext(ioDispatcher) {
      val runId = store.recordInstantRun(jobId, atMillis, status, sessionPath, summary, error)
      refreshSnapshotLocked()
      runId
    }
  }

  suspend fun recordRunFinish(
    runId: Long,
    jobId: String,
    finishedAt: Long,
    status: ScheduleRunStatus,
    sessionPath: String?,
    summary: String?,
    error: String?,
  ) = mutex.withLock {
    ensureInitializedLocked()
    withContext(ioDispatcher) {
      store.recordRunFinish(runId, jobId, finishedAt, status, sessionPath, summary, error)
      refreshSnapshotLocked()
    }
  }

  private fun validateJobFields(name: String, cwd: String, prompt: String) {
    require(name.isNotBlank()) { "Schedule name must not be blank" }
    require(cwd.isNotBlank()) { "Schedule workspace cwd must not be blank" }
    require(prompt.isNotBlank()) { "Schedule prompt must not be blank" }
  }

  private fun normalizeTimezoneAndSpec(rawTimezone: String, spec: ScheduleSpec): Pair<String, ScheduleSpec> {
    val timezone = rawTimezone.ifBlank { clock.systemZoneId() }
    val normalizedSpec = when (spec) {
      is ScheduleSpec.Cron -> spec.copy(zoneId = spec.zoneId.ifBlank { timezone })
      is ScheduleSpec.Once, is ScheduleSpec.Interval -> spec
    }
    return timezone to normalizedSpec
  }

  private suspend fun resyncAlarm() = mutex.withLock {
    ensureInitializedLocked()
    withContext(ioDispatcher) {
      alarmScheduler.sync(store.nextTriggerAt(), preferExactAlarms.value)
    }
  }

  private suspend fun ensureInitializedLocked() {
    if (initialized) return
    withContext(ioDispatcher) {
      store.recoverInterruptedRuns(clock.nowMillis(), INTERRUPTED_ERROR)
      refreshAndSyncAlarmLocked()
      initialized = true
    }
  }

  private fun refreshAndSyncAlarmLocked() {
    refreshSnapshotLocked()
    alarmScheduler.sync(store.nextTriggerAt(), preferExactAlarms.value)
  }

  private fun refreshSnapshotLocked() {
    val all = store.listJobs()
    loadedJobs.value = all
    jobsState.value = all
  }

  private companion object {
    const val INTERRUPTED_ERROR = "Interrupted by process stop"
    const val MISSED_ERROR = "Missed schedule window"
  }
}

/** Input payload for creating a new [ScheduledJob] via [ScheduleRepository.createJob]. */
internal data class NewScheduledJob(
  val name: String,
  val cwd: String,
  val sessionPath: String? = null,
  val prompt: String,
  val engine: String? = null,
  val spec: ScheduleSpec,
  val timezone: String = "",
  val enabled: Boolean = true,
  val overlapPolicy: OverlapPolicy = OverlapPolicy.Skip,
  val misfirePolicy: MisfirePolicy = MisfirePolicy.Skip,
  val maxRunSeconds: Int = ScheduleSpec.DEFAULT_MAX_RUN_SECONDS,
)
