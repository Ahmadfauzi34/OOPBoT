package io.jeph.oopbot.schedule

import java.time.Instant
import org.junit.Assert.assertEquals
import org.junit.Assert.assertFalse
import org.junit.Assert.assertNull
import org.junit.Assert.assertTrue
import org.junit.Test

class ScheduleCalculatorAndAlarmTest {
  @Test
  fun computesDriftFreeIntervalAndOnceTriggers() {
    val once = ScheduleSpec.Once(50_000L)
    assertEquals(50_000L, ScheduleCalculator.nextTriggerAfter(once, 49_999L))
    assertNull(ScheduleCalculator.nextTriggerAfter(once, 50_000L))

    val interval = ScheduleSpec.Interval(everyMinutes = 15, anchorMillis = 1_000_000L)
    val step = 15 * 60_000L
    assertEquals(1_000_000L, ScheduleCalculator.nextTriggerAfter(interval, 900_000L))
    assertEquals(1_000_000L + step, ScheduleCalculator.nextTriggerAfter(interval, 1_000_000L))
    // Even if woke up 2 minutes late, next tick stays aligned to anchor + 2*step.
    assertEquals(1_000_000L + 2 * step, ScheduleCalculator.nextTriggerAfter(interval, 1_000_000L + step + 120_000L))
  }

  @Test
  fun evaluatesDueWithDozeGraceAndMisfirePolicies() {
    val step = 60 * 60_000L
    val baseJob = job(
      spec = ScheduleSpec.Interval(60, anchorMillis = 0L),
      nextRunAt = step,
      misfire = MisfirePolicy.Skip,
    )

    // 4 minutes late (within 5-minute Doze grace): runs normally.
    val withinGrace = ScheduleCalculator.evaluateDue(baseJob, nowMillis = step + 4 * 60_000L)
    assertTrue(withinGrace.shouldRun)
    assertFalse(withinGrace.missed)
    assertEquals(2 * step, withinGrace.nextRunAt)
    assertTrue(withinGrace.enabled)

    // 10 minutes late with MisfirePolicy.Skip: marked missed and advances to next tick.
    val skipped = ScheduleCalculator.evaluateDue(baseJob, nowMillis = step + 10 * 60_000L)
    assertFalse(skipped.shouldRun)
    assertTrue(skipped.missed)
    assertEquals(2 * step, skipped.nextRunAt)
    assertTrue(skipped.enabled)

    // 10 minutes late with MisfirePolicy.RunOnce: executes once and advances.
    val runOnce = ScheduleCalculator.evaluateDue(
      baseJob.copy(misfirePolicy = MisfirePolicy.RunOnce),
      nowMillis = step + 10 * 60_000L,
    )
    assertTrue(runOnce.shouldRun)
    assertFalse(runOnce.missed)
    assertEquals(2 * step, runOnce.nextRunAt)

    // One-shot job auto-disables after running.
    val onceJob = job(spec = ScheduleSpec.Once(step), nextRunAt = step)
    val onceEval = ScheduleCalculator.evaluateDue(onceJob, nowMillis = step + 1_000L)
    assertTrue(onceEval.shouldRun)
    assertNull(onceEval.nextRunAt)
    assertFalse(onceEval.enabled)
  }

  @Test
  fun recomputesClockAndTimezoneChangesSafely() {
    val now = Instant.parse("2025-06-01T00:30:00Z").toEpochMilli()
    val cronJob = job(
      spec = ScheduleSpec.Cron("0 8 * * *", "UTC"),
      timezone = "UTC",
      nextRunAt = Instant.parse("2025-06-01T08:00:00Z").toEpochMilli(),
    )

    // TIMEZONE_CHANGED switches cron zone to Asia/Jakarta (+07:00), so 08:00 local is 01:00 UTC.
    val tzUpdated = ScheduleCalculator.recomputeForClockChange(
      job = cronJob,
      nowMillis = now,
      systemZoneId = "Asia/Jakarta",
      timezoneChanged = true,
    )
    assertEquals("Asia/Jakarta", tzUpdated.timezone)
    assertEquals(Instant.parse("2025-06-01T01:00:00Z").toEpochMilli(), tzUpdated.nextRunAt)

    // TIME_CHANGED (not timezone) preserves explicit UTC zone AND preserves already-due triggers for MisfirePolicy.
    val dueCron = cronJob.copy(nextRunAt = now - 60_000L)
    val keptDue = ScheduleCalculator.recomputeForClockChange(
      job = dueCron,
      nowMillis = now,
      systemZoneId = "Asia/Jakarta",
      timezoneChanged = false,
    )
    assertEquals(dueCron, keptDue)

    // TIME_CHANGED backward jump pulls future nextRunAt back without altering timezone.
    val strandedCron = cronJob.copy(nextRunAt = Instant.parse("2025-06-02T08:00:00Z").toEpochMilli())
    val pulledBack = ScheduleCalculator.recomputeForClockChange(
      job = strandedCron,
      nowMillis = now,
      systemZoneId = "Asia/Jakarta",
      timezoneChanged = false,
    )
    assertEquals("UTC", pulledBack.timezone)
    assertEquals(Instant.parse("2025-06-01T08:00:00Z").toEpochMilli(), pulledBack.nextRunAt)

    // Once job is never silently disabled by clock/timezone broadcasts.
    val onceJob = job(spec = ScheduleSpec.Once(now - 30_000L), nextRunAt = now - 30_000L)
    assertEquals(
      onceJob,
      ScheduleCalculator.recomputeForClockChange(onceJob, now, "Asia/Jakarta", timezoneChanged = false),
    )
  }

  @Test
  fun alarmSchedulerDefaultsToInexactUpgradesOptInExactAndFallsBackOnSecurityException() {
    val driver = RecordingAlarmDriver()
    val scheduler = AlarmScheduler(driver)

    scheduler.sync(nextTriggerAtMillis = null, preferExact = false)
    assertEquals(listOf("cancel"), driver.events)

    driver.canExact = true
    scheduler.sync(nextTriggerAtMillis = 10_000L, preferExact = false)
    assertEquals("inexact:10000", driver.events.last())

    driver.canExact = false
    scheduler.sync(nextTriggerAtMillis = 20_000L, preferExact = true)
    assertEquals("inexact:20000", driver.events.last())

    driver.canExact = true
    scheduler.sync(nextTriggerAtMillis = 30_000L, preferExact = true)
    assertEquals("exact:30000", driver.events.last())

    driver.throwOnExact = true
    scheduler.sync(nextTriggerAtMillis = 40_000L, preferExact = true)
    assertEquals(listOf("exact-sec-fail:40000", "inexact:40000"), driver.events.takeLast(2))
  }

  private fun job(
    spec: ScheduleSpec,
    timezone: String = "UTC",
    nextRunAt: Long? = null,
    misfire: MisfirePolicy = MisfirePolicy.Skip,
  ) = ScheduledJob(
    id = "job-1",
    name = "Test",
    cwd = "/w",
    prompt = "hi",
    spec = spec,
    timezone = timezone,
    enabled = true,
    misfirePolicy = misfire,
    nextRunAt = nextRunAt,
    createdAt = 0L,
    updatedAt = 0L,
  )

  private class RecordingAlarmDriver : AlarmDriver {
    var canExact = false
    var throwOnExact = false
    val events = mutableListOf<String>()

    override fun canScheduleExactAlarms(): Boolean = canExact

    override fun scheduleInexact(triggerAtMillis: Long) {
      events += "inexact:$triggerAtMillis"
    }

    override fun scheduleExact(triggerAtMillis: Long) {
      if (throwOnExact) {
        events += "exact-sec-fail:$triggerAtMillis"
        throw SecurityException("Revoked")
      }
      events += "exact:$triggerAtMillis"
    }

    override fun cancel() {
      events += "cancel"
    }
  }
}
