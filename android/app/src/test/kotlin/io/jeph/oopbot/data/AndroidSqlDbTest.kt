package io.jeph.oopbot.data

import android.app.Application
import android.content.Context
import android.database.sqlite.SQLiteDatabase
import android.database.sqlite.SQLiteOpenHelper
import androidx.test.core.app.ApplicationProvider
import io.jeph.oopbot.agent.ChatLocation
import io.jeph.oopbot.agent.InboxEntry
import io.jeph.oopbot.agent.InboxReason
import io.jeph.oopbot.host.GitDiffMode
import io.jeph.oopbot.host.SessionInfo
import io.jeph.oopbot.schedule.ScheduleRunStatus
import io.jeph.oopbot.schedule.ScheduleSpec
import io.jeph.oopbot.schedule.ScheduledJob
import io.jeph.oopbot.ui.git.ExplorerTab
import java.util.UUID
import java.util.concurrent.Executor
import org.junit.After
import org.junit.Assert.assertEquals
import org.junit.Assert.assertNotNull
import org.junit.Assert.assertThrows
import org.junit.Assert.assertTrue
import org.junit.Test
import org.junit.runner.RunWith
import org.robolectric.RobolectricTestRunner
import org.robolectric.annotation.Config

/**
 * Verifies [AndroidSqlDb] directly against Android's framework `SQLiteOpenHelper` across API 29 (`minSdk`) and
 * API 35, ensuring PRAGMAs, FTS4 (`unicode61`), nullable inserts, `rawQuery` bindings, v1->v2 upgrades, and WAL queries
 * stay valid.
 */
@RunWith(RobolectricTestRunner::class)
@Config(manifest = Config.NONE, application = Application::class, sdk = [29, 35])
class AndroidSqlDbTest {
  private val context: Context = ApplicationProvider.getApplicationContext()
  private val opened = ArrayList<Pair<String, AndroidSqlDb>>()
  private val syncExecutor = Executor(Runnable::run)

  @After
  fun tearDown() {
    for ((name, db) in opened) {
      runCatching { db.close() }
      context.deleteDatabase(name)
    }
    opened.clear()
  }

  private fun openDb(name: String = "test-${UUID.randomUUID()}.db"): Pair<AndroidSqlDb, OopbotDatabase> {
    val sqlDb = AndroidSqlDb(context, name)
    opened += name to sqlDb
    return sqlDb to OopbotDatabase(sqlDb, syncExecutor)
  }

  @Test
  fun opensFrameworkDatabaseWithWalForeignKeysAndFts4() {
    val (sqlDb, db) = openDb()
    val foreignKeys = sqlDb.queryOne("PRAGMA foreign_keys") { it.string(0) }
    val journalMode = sqlDb.queryOne("PRAGMA journal_mode") { it.string(0).lowercase() }
    assertEquals("1", foreignKeys)
    assertEquals("wal", journalMode)

    val info = SessionInfo(
      path = "/s/alpha.jsonl",
      id = "alpha",
      cwd = "/w/app",
      cwdExists = true,
      name = "Alpha",
      created = 10L,
      modified = 20L,
      messageCount = 3,
      firstMessage = "hello",
    )
    db.sessions.sync(listOf(info))
    db.sessions.saveMessagesSync("/s/alpha.jsonl", 20L, "[]", "Kubernetes rollout token_v2")
    db.sessions.saveMessagesSync("/s/beta.jsonl", 30L, "[]", "AND OR NOT NEAR operator smoke")

    val all = setOf("/s/alpha.jsonl", "/s/beta.jsonl")
    assertEquals(listOf(info), db.sessions.list())
    assertEquals(setOf("/s/alpha.jsonl"), db.sessions.searchMessages("kuber", all))
    assertEquals(setOf("/s/beta.jsonl"), db.sessions.searchMessages("NOT OR AND", all))

    db.sessions.purge(listOf("/s/alpha.jsonl"))
    assertEquals(emptySet<String>(), db.sessions.searchMessages("kuber", all))
  }

  @Test
  fun migratesNullableMetadataDraftsInboxAndWrapsSqlErrors() {
    val (sqlDb, db) = openDb()
    db.prefs.migrateLegacy(
      "prefs_v1_migrated",
      LegacyPrefsData(
        workspaces = listOf("/w/one"),
        archived = mapOf("/s/archived.jsonl" to 111L),
        touched = mapOf("/s/touched.jsonl" to 222L),
        explorerTabs = mapOf("/w/one" to ExplorerTab.Files),
        diffModes = mapOf("/w/one" to GitDiffMode.Uncommitted),
        trustedRepositories = setOf("/w/one"),
        remoteHostsCache = null,
      ),
    )
    assertTrue(db.prefs.isMigrated("prefs_v1_migrated"))
    assertEquals(mapOf("/s/archived.jsonl" to 111L), db.prefs.readArchived())
    assertEquals(mapOf("/s/touched.jsonl" to 222L), db.prefs.readTouched())

    val draft = ChatDraftSnapshot("/s/touched.jsonl", "wip", emptyMap(), false, "item-1", 8, setOf("fold-1"))
    val inbox = InboxEntry(ChatLocation("/w/one", "/s/touched.jsonl"), InboxReason.Finished, "Done", "OK", 333L)
    db.drafts.save(draft)
    db.inbox.upsert(inbox)
    assertEquals(listOf(draft), db.drafts.loadAll())
    assertEquals(listOf(inbox), db.inbox.loadAll())

    assertThrows(OopbotSqlException::class.java) {
      sqlDb.query("SELECT * FROM missing_table") { it.string(0) }
    }
    assertThrows(IllegalArgumentException::class.java) {
      sqlDb.queryOne("SELECT 1 WHERE 1 = ?", arrayOf<Any?>(null)) { it.int(0) }
    }
  }

  @Test
  fun upgradesV1DatabaseToV2PreservingExistingData() {
    val name = "v1-upgrade-${UUID.randomUUID()}.db"
    val legacy = object : SQLiteOpenHelper(context, name, null, 1) {
      override fun onCreate(db: SQLiteDatabase) {
        db.execSQL("CREATE TABLE schema_meta (key TEXT PRIMARY KEY NOT NULL, value TEXT NOT NULL)")
        db.execSQL("INSERT INTO schema_meta (key, value) VALUES ('prefs_v1_migrated', '1')")
      }

      override fun onUpgrade(db: SQLiteDatabase, oldVersion: Int, newVersion: Int) = Unit
    }
    legacy.writableDatabase.close()
    legacy.close()

    val (_, db) = openDb(name)
    assertTrue(db.prefs.isMigrated("prefs_v1_migrated"))
    val job = sampleJob("job-v2")
    db.schedules.upsertJob(job)
    assertEquals(job, db.schedules.getJob("job-v2"))
  }

  @Test
  fun scheduleStoreUpdatesPreserveRunsCascadesDeletesPrunesAndRecovers() {
    val (_, db) = openDb()
    val job = sampleJob("job-cascade")
    db.schedules.upsertJob(job)

    val runId = db.schedules.recordRunStart(job.id, startedAt = 1_000L, sessionPath = "/s/1.jsonl")
    assertTrue(runId > 0L)
    db.schedules.recordRunFinish(
      runId = runId,
      jobId = job.id,
      finishedAt = 2_000L,
      status = ScheduleRunStatus.Succeeded,
      sessionPath = "/s/1.jsonl",
      summary = "ok",
      error = null,
    )

    // Updating an existing job MUST use UPDATE (not INSERT OR REPLACE) so schedule_runs survive FK CASCADE.
    db.schedules.upsertJob(job.copy(name = "Renamed", updatedAt = 3_000L))
    assertEquals("Renamed", db.schedules.getJob(job.id)?.name)
    assertEquals(1, db.schedules.listRuns(job.id).size)

    // Inserting beyond MAX_RUNS_PER_JOB prunes oldest runs to 50.
    for (i in 2..55) {
      db.schedules.recordInstantRun(job.id, atMillis = i * 1_000L, status = ScheduleRunStatus.Succeeded)
    }
    val pruned = db.schedules.listRuns(job.id)
    assertEquals(ScheduleDbStore.MAX_RUNS_PER_JOB, pruned.size)
    assertEquals(55_000L, pruned.first().startedAt)

    // Interrupted running row recovers to Failed.
    db.schedules.recordRunStart(job.id, startedAt = 60_000L, sessionPath = null)
    assertEquals(1, db.schedules.recoverInterruptedRuns(nowMillis = 61_000L, errorMessage = "killed"))
    assertEquals(ScheduleRunStatus.Failed, db.schedules.getJob(job.id)?.lastStatus)
    assertEquals("killed", db.schedules.listRuns(job.id, limit = 1).first().error)

    // Deleting parent job cascades to schedule_runs.
    db.schedules.deleteJob(job.id)
    assertEquals(emptyList<Any>(), db.schedules.listRuns(job.id))
    assertNotNull(db.schedules.listJobs())
    assertTrue(db.schedules.listJobs().isEmpty())
  }

  private fun sampleJob(id: String) = ScheduledJob(
    id = id,
    name = "Morning Brief",
    cwd = "/w/app",
    sessionPath = null,
    prompt = "Summarize git status",
    engine = null,
    spec = ScheduleSpec.Cron("0 8 * * *", "Asia/Jakarta"),
    timezone = "Asia/Jakarta",
    enabled = true,
    nextRunAt = 100_000L,
    createdAt = 500L,
    updatedAt = 500L,
  )
}
