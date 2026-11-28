package io.jeph.oopbot.schedule

import java.time.ZoneId

/** Result of evaluating a due job against [MisfirePolicy] and [ScheduleCalculator.MISFIRE_GRACE_MILLIS]. */
internal data class DueEvaluation(
  /** Whether the job should execute right now. */
  val shouldRun: Boolean,
  /** True when the trigger was skipped because it was missed beyond the Doze grace window. */
  val missed: Boolean,
  /** The updated `nextRunAt` to persist (null if a one-shot job finished or was missed). */
  val nextRunAt: Long?,
  /** The updated `enabled` flag to persist (false for completed/missed one-shot jobs). */
  val enabled: Boolean,
)

/**
 * Pure, deterministic schedule trigger calculator for [ScheduleSpec.Once], [ScheduleSpec.Interval], and
 * [ScheduleSpec.Cron]. Validates specs and handles misfire grace windows and timezone changes without Android deps.
 */
internal object ScheduleCalculator {
  /**
   * Alarms delivered up to 5 minutes past `nextRunAt` are treated as normal Doze/inexact jitter rather than a misfire.
   */
  const val MISFIRE_GRACE_MILLIS = 5 * 60_000L

  private const val MILLIS_PER_MINUTE = 60_000L

  /** Validates [spec] (cron syntax/calendar reachability, timezone id, interval bounds). Throws on invalid input. */
  fun validate(spec: ScheduleSpec, timezone: String) {
    ZoneId.of(timezone)
    when (spec) {
      is ScheduleSpec.Once -> require(spec.triggerAtMillis > 0L) { "One-shot trigger time must be positive" }

      is ScheduleSpec.Interval -> {
        require(spec.everyMinutes in ScheduleSpec.MIN_INTERVAL_MINUTES..ScheduleSpec.MAX_INTERVAL_MINUTES) {
          "Interval out of bounds: ${spec.everyMinutes}"
        }
      }

      is ScheduleSpec.Cron -> {
        ZoneId.of(spec.zoneId)
        CronExpression.parse(spec.expression)
      }
    }
  }

  /** Computes the next scheduled trigger strictly greater than [afterMillis] (or `null` if none remains). */
  fun nextTriggerAfter(spec: ScheduleSpec, afterMillis: Long): Long? = when (spec) {
    is ScheduleSpec.Once -> if (spec.triggerAtMillis > afterMillis) spec.triggerAtMillis else null

    is ScheduleSpec.Interval -> {
      val step = spec.everyMinutes.toLong() * MILLIS_PER_MINUTE
      if (afterMillis < spec.anchorMillis) {
        spec.anchorMillis
      } else {
        val elapsedSteps = (afterMillis - spec.anchorMillis) / step
        spec.anchorMillis + (elapsedSteps + 1L) * step
      }
    }

    is ScheduleSpec.Cron -> CronExpression.parse(spec.expression).nextAfter(afterMillis, ZoneId.of(spec.zoneId))
  }

  /**
   * Evaluates a due [job] (`job.nextRunAt != null && job.nextRunAt <= nowMillis`) and computes whether to run now,
   * whether it counts as missed, and what `(nextRunAt, enabled)` should be persisted before execution starts.
   */
  fun evaluateDue(job: ScheduledJob, nowMillis: Long): DueEvaluation {
    val dueAt = job.nextRunAt ?: return DueEvaluation(
      shouldRun = false,
      missed = false,
      nextRunAt = null,
      enabled = job.enabled,
    )
    val lateness = nowMillis - dueAt
    val isMisfire = lateness > MISFIRE_GRACE_MILLIS
    val shouldRun = !isMisfire || job.misfirePolicy == MisfirePolicy.RunOnce
    val missed = isMisfire && !shouldRun
    val nextAfterNow = when (job.spec) {
      is ScheduleSpec.Once -> null
      is ScheduleSpec.Interval, is ScheduleSpec.Cron -> nextTriggerAfter(job.spec, nowMillis)
    }
    val nextEnabled = when (job.spec) {
      is ScheduleSpec.Once -> false
      is ScheduleSpec.Interval, is ScheduleSpec.Cron -> job.enabled && nextAfterNow != null
    }
    return DueEvaluation(
      shouldRun = shouldRun,
      missed = missed,
      nextRunAt = nextAfterNow,
      enabled = nextEnabled,
    )
  }

  /**
   * Recomputes `nextRunAt` for an enabled job after `ACTION_TIMEZONE_CHANGED` (`timezoneChanged = true`) or
   * `ACTION_TIME_CHANGED` (`timezoneChanged = false`).
   *
   * Already-due triggers (`nextRunAt <= nowMillis`) are preserved on clock jumps so `AlarmManager` delivers them
   * immediately and [evaluateDue] applies [MisfirePolicy] instead of silently dropping the trigger.
   */
  fun recomputeForClockChange(
    job: ScheduledJob,
    nowMillis: Long,
    systemZoneId: String,
    timezoneChanged: Boolean,
  ): ScheduledJob {
    if (!job.enabled) return job
    return when (val spec = job.spec) {
      is ScheduleSpec.Once -> job

      is ScheduleSpec.Interval -> {
        if (timezoneChanged || (job.nextRunAt != null && job.nextRunAt <= nowMillis)) return job
        job.copy(nextRunAt = nextTriggerAfter(spec, nowMillis), updatedAt = nowMillis)
      }

      is ScheduleSpec.Cron -> {
        if (!timezoneChanged && job.nextRunAt != null && job.nextRunAt <= nowMillis) return job
        val targetZone = if (timezoneChanged) systemZoneId else spec.zoneId
        val updatedSpec = spec.copy(zoneId = targetZone)
        val nextRun = nextTriggerAfter(updatedSpec, nowMillis)
        job.copy(
          spec = updatedSpec,
          timezone = targetZone,
          nextRunAt = nextRun,
          updatedAt = nowMillis,
        )
      }
    }
  }
}
