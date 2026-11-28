package io.jeph.oopbot.data

import io.jeph.oopbot.schedule.MisfirePolicy
import io.jeph.oopbot.schedule.OverlapPolicy
import io.jeph.oopbot.schedule.ScheduleRunStatus
import io.jeph.oopbot.schedule.ScheduleSpec
import io.jeph.oopbot.schedule.ScheduledJob
import org.junit.After
import org.junit.Assert.assertEquals
import org.junit.Assert.assertFalse
import org.junit.Assert.assertNull
import org.junit.Assert.assertTrue
import org.junit.Test

class ScheduleDbStoreTest {
  private val sqlDb = JdbcSqlDb()
  private val store = ScheduleDbStore(sqlDb)

  @After
  fun tearDown() {
    sqlDb.close()
  }

  @Test
  fun roundTripsAllScheduleSpecsAndQueryFilters() {
    val once = makeJob("j1", ScheduleSpec.Once(10_000L), enabled = true, nextRunAt = 10_000L, createdAt = 1L)
    val interval = makeJob(
      id = "j2",
      spec = ScheduleSpec.Interval(everyMinutes = 30, anchorMillis = 5_000L),
      enabled = true,
      nextRunAt = 8_000L,
      createdAt = 2L,
      overlap = OverlapPolicy.Queue,
      misfire = MisfirePolicy.RunOnce,
    ).copy(engine = "codex", sessionPath = "/s/persistent.jsonl")
    val disabled = makeJob(
      id = "j3",
      spec = ScheduleSpec.Cron("0 9 * * 1-5", "UTC"),
      enabled = false,
      nextRunAt = 1_000L,
      createdAt = 3L,
    )

    assertFalse(store.hasEnabledJobs())
    assertNull(store.nextTriggerAt())

    store.upsertJob(once)
    store.upsertJob(interval)
    store.upsertJob(disabled)

    assertTrue(store.hasEnabledJobs())
    assertEquals(8_000L, store.nextTriggerAt())
    assertEquals(listOf(once, interval, disabled), store.listJobs())
    assertEquals(listOf(interval), store.listDueJobs(nowMillis = 8_000L))
    assertEquals(listOf(interval, once), store.listDueJobs(nowMillis = 10_000L))
    assertEquals(emptyList<ScheduledJob>(), store.listDueJobs(nowMillis = 7_999L))
  }

  @Test
  fun upsertUpdatesInPlaceWithoutCascadingRunsAndDeleteCascades() {
    val job = makeJob("j1", ScheduleSpec.Cron("*/15 * * * *", "UTC"), enabled = true, nextRunAt = 20_000L)
    store.upsertJob(job)
    val runId = requireNotNull(store.recordRunStart("j1", startedAt = 10_000L, sessionPath = "/s/a.jsonl"))
    store.recordRunFinish(
      runId = runId,
      jobId = "j1",
      finishedAt = 12_000L,
      status = ScheduleRunStatus.Succeeded,
      sessionPath = "/s/a.jsonl",
      summary = "All checks passed",
      error = null,
    )

    store.upsertJob(job.copy(prompt = "Updated prompt", updatedAt = 15_000L))
    val updated = store.getJob("j1")
    assertEquals("Updated prompt", updated?.prompt)
    assertEquals(1, store.listRuns("j1").size)
    assertEquals("All checks passed", store.listRuns("j1").first().summary)

    store.deleteJob("j1")
    assertNull(store.getJob("j1"))
    assertTrue(store.listRuns("j1").isEmpty())
    assertFalse(store.hasEnabledJobs())

    // Recording runs for a deleted/missing job returns null cleanly instead of throwing SQLITE_CONSTRAINT_FOREIGNKEY.
    assertNull(store.recordRunStart("j1", startedAt = 20_000L, sessionPath = null))
    assertNull(store.recordInstantRun("j1", atMillis = 20_000L, status = ScheduleRunStatus.Skipped))
  }

  @Test
  fun prunesRunsPerJobIndependentlyAndRecoversInterruptedRuns() {
    val jobA = makeJob("a", ScheduleSpec.Interval(15, 0L), enabled = true, nextRunAt = 1_000L)
    val jobB = makeJob("b", ScheduleSpec.Interval(15, 0L), enabled = true, nextRunAt = 2_000L)
    store.upsertJob(jobA)
    store.upsertJob(jobB)

    for (i in 1..60) {
      store.recordInstantRun("a", atMillis = i * 100L, status = ScheduleRunStatus.Succeeded)
    }
    store.recordRunStart("b", startedAt = 9_000L, sessionPath = null)

    val runsA = store.listRuns("a")
    assertEquals(ScheduleDbStore.MAX_RUNS_PER_JOB, runsA.size)
    assertEquals(6_000L, runsA.first().startedAt)
    assertEquals(1_100L, runsA.last().startedAt)

    val recovered = store.recoverInterruptedRuns(nowMillis = 10_000L, errorMessage = "process killed")
    assertEquals(1, recovered)
    val runB = store.listRuns("b").single()
    assertEquals(ScheduleRunStatus.Failed, runB.status)
    assertEquals(10_000L, runB.finishedAt)
    assertEquals("process killed", runB.error)
    assertEquals(ScheduleRunStatus.Failed, store.getJob("b")?.lastStatus)
    assertEquals("process killed", store.getJob("b")?.lastError)
  }

  private fun makeJob(
    id: String,
    spec: ScheduleSpec,
    enabled: Boolean,
    nextRunAt: Long?,
    createdAt: Long = 100L,
    overlap: OverlapPolicy = OverlapPolicy.Skip,
    misfire: MisfirePolicy = MisfirePolicy.Skip,
  ) = ScheduledJob(
    id = id,
    name = "Job $id",
    cwd = "/w/project",
    prompt = "Run diagnostics",
    spec = spec,
    timezone = "UTC",
    enabled = enabled,
    overlapPolicy = overlap,
    misfirePolicy = misfire,
    nextRunAt = nextRunAt,
    createdAt = createdAt,
    updatedAt = createdAt,
  )
}
