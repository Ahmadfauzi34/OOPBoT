package io.jeph.oopbot.data

import io.jeph.oopbot.schedule.MisfirePolicy
import io.jeph.oopbot.schedule.OverlapPolicy
import io.jeph.oopbot.schedule.ScheduleKind
import io.jeph.oopbot.schedule.ScheduleRun
import io.jeph.oopbot.schedule.ScheduleRunStatus
import io.jeph.oopbot.schedule.ScheduleSpec
import io.jeph.oopbot.schedule.ScheduledJob

/**
 * SQLite store for `scheduled_jobs` and `schedule_runs` in `oopbot.db`.
 *
 * Uses explicit `UPDATE` / `INSERT` rather than `INSERT OR REPLACE` on `scheduled_jobs` because `PRAGMA foreign_keys =
 * ON` causes `REPLACE` to delete the parent row and cascade-delete all `schedule_runs` history.
 */
internal class ScheduleDbStore(private val db: SqlDb) {
  fun listJobs(): List<ScheduledJob> = db.query(
    "SELECT $JOB_COLUMNS FROM scheduled_jobs ORDER BY created_at ASC, id ASC",
    mapper = ::readJob,
  )

  fun getJob(id: String): ScheduledJob? = db.queryOne(
    "SELECT $JOB_COLUMNS FROM scheduled_jobs WHERE id = ?",
    arrayOf(id),
    mapper = ::readJob,
  )

  fun listDueJobs(nowMillis: Long): List<ScheduledJob> = db.query(
    "SELECT $JOB_COLUMNS FROM scheduled_jobs " +
      "WHERE enabled = 1 AND next_run_at IS NOT NULL AND next_run_at <= ? " +
      "ORDER BY next_run_at ASC, id ASC",
    arrayOf(nowMillis),
    mapper = ::readJob,
  )

  fun nextTriggerAt(): Long? = db.queryOne(
    "SELECT MIN(next_run_at) FROM scheduled_jobs WHERE enabled = 1 AND next_run_at IS NOT NULL",
  ) { it.longOrNull(0) }

  fun hasEnabledJobs(): Boolean = db.queryOne(
    "SELECT 1 FROM scheduled_jobs WHERE enabled = 1 LIMIT 1",
  ) { it.int(0) } != null

  /** Inserts a new job or updates an existing job in place without triggering `ON DELETE CASCADE` on run history. */
  fun upsertJob(job: ScheduledJob) {
    db.transaction {
      val exists = queryOne("SELECT 1 FROM scheduled_jobs WHERE id = ?", arrayOf(job.id)) { it.int(0) } != null
      if (exists) {
        exec(
          "UPDATE scheduled_jobs SET name = ?, cwd = ?, session_path = ?, prompt = ?, engine = ?, " +
            "schedule_kind = ?, schedule_spec = ?, timezone = ?, enabled = ?, overlap_policy = ?, " +
            "misfire_policy = ?, max_run_seconds = ?, next_run_at = ?, last_run_at = ?, " +
            "last_status = ?, last_error = ?, updated_at = ? WHERE id = ?",
          arrayOf(
            job.name,
            job.cwd,
            job.sessionPath,
            job.prompt,
            job.engine,
            job.spec.kind.wire,
            ScheduleSpec.encodeSpec(job.spec),
            job.timezone,
            if (job.enabled) 1 else 0,
            job.overlapPolicy.wire,
            job.misfirePolicy.wire,
            job.maxRunSeconds,
            job.nextRunAt,
            job.lastRunAt,
            job.lastStatus.wire,
            job.lastError,
            job.updatedAt,
            job.id,
          ),
        )
      } else {
        exec(
          "INSERT INTO scheduled_jobs ($JOB_COLUMNS) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
          arrayOf(
            job.id,
            job.name,
            job.cwd,
            job.sessionPath,
            job.prompt,
            job.engine,
            job.spec.kind.wire,
            ScheduleSpec.encodeSpec(job.spec),
            job.timezone,
            if (job.enabled) 1 else 0,
            job.overlapPolicy.wire,
            job.misfirePolicy.wire,
            job.maxRunSeconds,
            job.nextRunAt,
            job.lastRunAt,
            job.lastStatus.wire,
            job.lastError,
            job.createdAt,
            job.updatedAt,
          ),
        )
      }
    }
  }

  /** Updates scheduling state (`next_run_at`, `enabled`, and optionally `last_status`/`last_error`) atomically. */
  fun updateJobScheduleState(
    jobId: String,
    nextRunAt: Long?,
    enabled: Boolean,
    updatedAt: Long,
    lastStatus: ScheduleRunStatus? = null,
    lastError: String? = null,
  ) {
    db.transaction {
      if (lastStatus != null) {
        exec(
          "UPDATE scheduled_jobs SET next_run_at = ?, enabled = ?, last_status = ?, last_error = ?, updated_at = ? " +
            "WHERE id = ?",
          arrayOf(nextRunAt, if (enabled) 1 else 0, lastStatus.wire, lastError, updatedAt, jobId),
        )
      } else {
        exec(
          "UPDATE scheduled_jobs SET next_run_at = ?, enabled = ?, updated_at = ? WHERE id = ?",
          arrayOf(nextRunAt, if (enabled) 1 else 0, updatedAt, jobId),
        )
      }
    }
  }

  fun deleteJob(id: String) {
    db.transaction {
      exec("DELETE FROM scheduled_jobs WHERE id = ?", arrayOf(id))
    }
  }

  /**
   * Records a run start, updates the parent job's status, prunes old runs beyond [MAX_RUNS_PER_JOB], and returns the
   * new run id, or `null` if [jobId] was deleted concurrently (preventing a foreign-key constraint violation).
   */
  fun recordRunStart(jobId: String, startedAt: Long, sessionPath: String?): Long? = db.transaction {
    val exists = queryOne("SELECT 1 FROM scheduled_jobs WHERE id = ?", arrayOf(jobId)) { it.int(0) } != null
    if (!exists) return@transaction null
    exec(
      "INSERT INTO schedule_runs (job_id, started_at, finished_at, status, session_path, summary, error) " +
        "VALUES (?, ?, NULL, ?, ?, NULL, NULL)",
      arrayOf(jobId, startedAt, ScheduleRunStatus.Running.wire, sessionPath),
    )
    val runId = queryOne("SELECT last_insert_rowid()") { it.long(0) } ?: 0L
    exec(
      "UPDATE scheduled_jobs SET last_run_at = ?, last_status = ?, last_error = NULL, updated_at = ? WHERE id = ?",
      arrayOf(startedAt, ScheduleRunStatus.Running.wire, startedAt, jobId),
    )
    pruneRunsLocked(jobId)
    runId
  }

  /**
   * Records an immediate terminal run (such as `skipped` or `missed`) without leaving a `running` row.
   * Returns `null` if [jobId] was deleted concurrently.
   */
  fun recordInstantRun(
    jobId: String,
    atMillis: Long,
    status: ScheduleRunStatus,
    sessionPath: String? = null,
    summary: String? = null,
    error: String? = null,
  ): Long? = db.transaction {
    val exists = queryOne("SELECT 1 FROM scheduled_jobs WHERE id = ?", arrayOf(jobId)) { it.int(0) } != null
    if (!exists) return@transaction null
    exec(
      "INSERT INTO schedule_runs (job_id, started_at, finished_at, status, session_path, summary, error) " +
        "VALUES (?, ?, ?, ?, ?, ?, ?)",
      arrayOf(jobId, atMillis, atMillis, status.wire, sessionPath, summary, error),
    )
    val runId = queryOne("SELECT last_insert_rowid()") { it.long(0) } ?: 0L
    exec(
      "UPDATE scheduled_jobs SET last_run_at = ?, last_status = ?, last_error = ?, updated_at = ? WHERE id = ?",
      arrayOf(atMillis, status.wire, error, atMillis, jobId),
    )
    pruneRunsLocked(jobId)
    runId
  }

  /** Completes a run row and updates the parent job's `last_status` / `last_error`. */
  fun recordRunFinish(
    runId: Long,
    jobId: String,
    finishedAt: Long,
    status: ScheduleRunStatus,
    sessionPath: String?,
    summary: String?,
    error: String?,
  ) {
    db.transaction {
      exec(
        "UPDATE schedule_runs SET finished_at = ?, status = ?, session_path = ?, summary = ?, error = ? WHERE id = ?",
        arrayOf(finishedAt, status.wire, sessionPath, summary, error, runId),
      )
      exec(
        "UPDATE scheduled_jobs SET last_status = ?, last_error = ?, updated_at = ? WHERE id = ?",
        arrayOf(status.wire, error, finishedAt, jobId),
      )
    }
  }

  fun listRuns(jobId: String, limit: Int = MAX_RUNS_PER_JOB): List<ScheduleRun> = db.query(
    "SELECT id, job_id, started_at, finished_at, status, session_path, summary, error " +
      "FROM schedule_runs WHERE job_id = ? ORDER BY started_at DESC, id DESC LIMIT ?",
    arrayOf(jobId, limit.coerceIn(1, MAX_RUNS_PER_JOB)),
    mapper = ::readRun,
  )

  /** Marks any `running` rows left over from a prior LMK-killed process as `failed`. Returns count recovered. */
  fun recoverInterruptedRuns(nowMillis: Long, errorMessage: String): Int = db.transaction {
    val interrupted = queryOne(
      "SELECT COUNT(*) FROM schedule_runs WHERE status = ?",
      arrayOf(ScheduleRunStatus.Running.wire),
    ) { it.int(0) } ?: 0
    if (interrupted > 0) {
      exec(
        "UPDATE schedule_runs SET finished_at = ?, status = ?, error = ? WHERE status = ?",
        arrayOf(nowMillis, ScheduleRunStatus.Failed.wire, errorMessage, ScheduleRunStatus.Running.wire),
      )
    }
    exec(
      "UPDATE scheduled_jobs SET last_status = ?, last_error = ?, updated_at = ? WHERE last_status = ?",
      arrayOf(ScheduleRunStatus.Failed.wire, errorMessage, nowMillis, ScheduleRunStatus.Running.wire),
    )
    interrupted
  }

  private fun SqlDb.pruneRunsLocked(jobId: String) {
    exec(
      "DELETE FROM schedule_runs WHERE job_id = ? AND id NOT IN (" +
        "SELECT id FROM schedule_runs WHERE job_id = ? ORDER BY started_at DESC, id DESC LIMIT ?" +
        ")",
      arrayOf(jobId, jobId, MAX_RUNS_PER_JOB),
    )
  }

  private fun readJob(c: SqlCursor): ScheduledJob {
    val kind = ScheduleKind.fromWire(c.string(6))
    val timezone = c.string(8)
    val createdAt = c.long(17)
    return ScheduledJob(
      id = c.string(0),
      name = c.string(1),
      cwd = c.string(2),
      sessionPath = c.stringOrNull(3),
      prompt = c.string(4),
      engine = c.stringOrNull(5),
      spec = ScheduleSpec.decodeSpec(kind, c.string(7), timezone, createdAt),
      timezone = timezone,
      enabled = c.int(9) != 0,
      overlapPolicy = OverlapPolicy.fromWire(c.string(10)),
      misfirePolicy = MisfirePolicy.fromWire(c.string(11)),
      maxRunSeconds = c.int(12),
      nextRunAt = c.longOrNull(13),
      lastRunAt = c.longOrNull(14),
      lastStatus = ScheduleRunStatus.fromWire(c.string(15)),
      lastError = c.stringOrNull(16),
      createdAt = createdAt,
      updatedAt = c.long(18),
    )
  }

  private fun readRun(c: SqlCursor): ScheduleRun = ScheduleRun(
    id = c.long(0),
    jobId = c.string(1),
    startedAt = c.long(2),
    finishedAt = c.longOrNull(3),
    status = ScheduleRunStatus.fromWire(c.string(4)),
    sessionPath = c.stringOrNull(5),
    summary = c.stringOrNull(6),
    error = c.stringOrNull(7),
  )

  companion object {
    const val MAX_RUNS_PER_JOB = 50
    private const val JOB_COLUMNS =
      "id, name, cwd, session_path, prompt, engine, schedule_kind, schedule_spec, timezone, enabled, " +
        "overlap_policy, misfire_policy, max_run_seconds, next_run_at, last_run_at, last_status, " +
        "last_error, created_at, updated_at"
  }
}
