package io.jeph.oopbot.schedule

import androidx.compose.runtime.Immutable

/** Schedule recurrence kind stored in `scheduled_jobs.schedule_kind`. */
@Immutable
internal enum class ScheduleKind(val wire: String) {
  Once("once"),
  Interval("interval"),
  Cron("cron"),
  ;

  companion object {
    fun fromWire(value: String): ScheduleKind = entries.firstOrNull { it.wire == value } ?: Once
  }
}

/** What to do when a job's trigger fires while a prior run of the same job is still running. */
@Immutable
internal enum class OverlapPolicy(val wire: String) {
  Skip("skip"),
  Queue("queue"),
  ;

  companion object {
    fun fromWire(value: String): OverlapPolicy = entries.firstOrNull { it.wire == value } ?: Skip
  }
}

/** What to do when a job's trigger was missed beyond the Doze grace window (e.g. device powered off). */
@Immutable
internal enum class MisfirePolicy(val wire: String) {
  Skip("skip"),
  RunOnce("run_once"),
  ;

  companion object {
    fun fromWire(value: String): MisfirePolicy = entries.firstOrNull { it.wire == value } ?: Skip
  }
}

/** Lifecycle status of a scheduled job's latest run and each `schedule_runs` history row. */
@Immutable
internal enum class ScheduleRunStatus(val wire: String) {
  Never("never"),
  Running("running"),
  Succeeded("succeeded"),
  Failed("failed"),
  Skipped("skipped"),
  Missed("missed"),
  TimedOut("timed_out"),
  ;

  companion object {
    fun fromWire(value: String): ScheduleRunStatus = entries.firstOrNull { it.wire == value } ?: Never
  }
}

/** Typed schedule specification serialized into `(schedule_kind, schedule_spec, timezone)` columns. */
@Immutable
internal sealed interface ScheduleSpec {
  val kind: ScheduleKind

  /** One-shot trigger at [triggerAtMillis] (epoch ms); auto-disables after running or missing. */
  @Immutable
  data class Once(val triggerAtMillis: Long) : ScheduleSpec {
    override val kind: ScheduleKind get() = ScheduleKind.Once
  }

  /** Fixed-period interval every [everyMinutes] aligned to [anchorMillis] so schedule times do not drift. */
  @Immutable
  data class Interval(val everyMinutes: Int, val anchorMillis: Long) : ScheduleSpec {
    override val kind: ScheduleKind get() = ScheduleKind.Interval

    init {
      require(everyMinutes in MIN_INTERVAL_MINUTES..MAX_INTERVAL_MINUTES) {
        "Interval must be between $MIN_INTERVAL_MINUTES and $MAX_INTERVAL_MINUTES minutes (got $everyMinutes)"
      }
    }
  }

  /** Standard 5-field POSIX cron expression evaluated in [zoneId] (IANA zone id, e.g. `Asia/Jakarta`). */
  @Immutable
  data class Cron(val expression: String, val zoneId: String) : ScheduleSpec {
    override val kind: ScheduleKind get() = ScheduleKind.Cron
  }

  companion object {
    const val MIN_INTERVAL_MINUTES = 5
    const val MAX_INTERVAL_MINUTES = 525_600
    const val DEFAULT_MAX_RUN_SECONDS = 900
    const val MIN_MAX_RUN_SECONDS = 30
    const val MAX_MAX_RUN_SECONDS = 3_600

    fun encodeSpec(spec: ScheduleSpec): String = when (spec) {
      is Once -> spec.triggerAtMillis.toString()
      is Interval -> "${spec.everyMinutes}:${spec.anchorMillis}"
      is Cron -> spec.expression
    }

    fun decodeSpec(kind: ScheduleKind, rawSpec: String, timezone: String, fallbackAnchorMillis: Long): ScheduleSpec =
      when (kind) {
        ScheduleKind.Once -> Once(rawSpec.toLongOrNull() ?: fallbackAnchorMillis)

        ScheduleKind.Interval -> {
          val parts = rawSpec.split(':')
          val minutes = (parts.firstOrNull()?.toIntOrNull() ?: MIN_INTERVAL_MINUTES)
            .coerceIn(MIN_INTERVAL_MINUTES, MAX_INTERVAL_MINUTES)
          val anchor = parts.getOrNull(1)?.toLongOrNull() ?: fallbackAnchorMillis
          Interval(minutes, anchor)
        }

        ScheduleKind.Cron -> Cron(rawSpec, timezone)
      }
  }
}

/** Persistent user-configured scheduled agent task stored in `scheduled_jobs`. */
@Immutable
internal data class ScheduledJob(
  val id: String,
  val name: String,
  val cwd: String,
  val sessionPath: String? = null,
  val prompt: String,
  val engine: String? = null,
  val spec: ScheduleSpec,
  val timezone: String,
  val enabled: Boolean = true,
  val overlapPolicy: OverlapPolicy = OverlapPolicy.Skip,
  val misfirePolicy: MisfirePolicy = MisfirePolicy.Skip,
  val maxRunSeconds: Int = ScheduleSpec.DEFAULT_MAX_RUN_SECONDS,
  val nextRunAt: Long? = null,
  val lastRunAt: Long? = null,
  val lastStatus: ScheduleRunStatus = ScheduleRunStatus.Never,
  val lastError: String? = null,
  val createdAt: Long,
  val updatedAt: Long,
)

/** One execution record stored in `schedule_runs` (pruned to the latest 50 per job). */
@Immutable
internal data class ScheduleRun(
  val id: Long,
  val jobId: String,
  val startedAt: Long,
  val finishedAt: Long? = null,
  val status: ScheduleRunStatus,
  val sessionPath: String? = null,
  val summary: String? = null,
  val error: String? = null,
)
