package io.jeph.oopbot.data

import io.jeph.oopbot.host.GitDiffMode
import io.jeph.oopbot.ui.git.ExplorerTab
import java.util.concurrent.Executor
import org.junit.Assert.assertEquals
import org.junit.Assert.assertFalse
import org.junit.Assert.assertNull
import org.junit.Assert.assertTrue
import org.junit.Rule
import org.junit.Test
import org.junit.rules.TemporaryFolder

class PreferencesMigrationTest {
  @get:Rule
  val folder = TemporaryFolder()

  private val syncExecutor = Executor(Runnable::run)

  @Test
  fun migratesLegacyJsonCollectionsOnceAndRemovesXmlKeys() {
    val legacyPrefs = InMemorySharedPreferences(
      mapOf(
        "workspaces" to """["/w/a/","/w/b","/w/a"]""",
        "archivedSessions" to """{"/s/1.jsonl":1000,"/s/2.jsonl":2000}""",
        "touchedSessions" to """{"/s/3.jsonl":3000}""",
        "explorerTabs" to """{"/w/a":"Files","/w/b":"UnknownTab"}""",
        "diffModes" to """{"/w/a":"Base"}""",
        "trustedRepositories" to """["/w/a"]""",
        "remoteHosts" to """[{"id":"r1","name":"box"}]""",
        "theme" to "Cobalt",
      ),
    )
    val dbFile = folder.newFile("prefs.db")
    JdbcSqlDb(dbFile).use { sqlDb ->
      val database = OopbotDatabase(sqlDb, syncExecutor)
      val prefs = Preferences(legacyPrefs, "build-1", database)

      assertEquals(listOf("/w/a", "/w/b"), prefs.workspaces.value)
      assertEquals(mapOf("/s/1.jsonl" to 1000L, "/s/2.jsonl" to 2000L), prefs.archivedSessions.value)
      assertEquals(mapOf("/s/3.jsonl" to 3000L), prefs.touchedSessions.value)
      assertEquals(mapOf("/w/a" to ExplorerTab.Files), prefs.explorerTabs.value)
      assertEquals(mapOf("/w/a" to GitDiffMode.Base), prefs.diffModes.value)
      assertEquals(setOf("/w/a"), prefs.trustedRepositories.value)
      assertEquals("""[{"id":"r1","name":"box"}]""", prefs.remoteHostsCache())

      assertFalse("workspaces" in legacyPrefs.keys())
      assertFalse("archivedSessions" in legacyPrefs.keys())
      assertFalse("touchedSessions" in legacyPrefs.keys())
      assertFalse("explorerTabs" in legacyPrefs.keys())
      assertFalse("diffModes" in legacyPrefs.keys())
      assertFalse("trustedRepositories" in legacyPrefs.keys())
      assertFalse("remoteHosts" in legacyPrefs.keys())
      assertTrue("theme" in legacyPrefs.keys())

      // Inject stale XML again: re-opening must be idempotent because prefs_v1_migrated is set.
      legacyPrefs.edit().putString("workspaces", """["/w/stale"]""").apply()
      val restarted = Preferences(legacyPrefs, "build-1", database)
      assertEquals(listOf("/w/a", "/w/b"), restarted.workspaces.value)
    }
  }

  @Test
  fun perItemWritesPersistInSqliteAcrossRestarts() {
    val dbFile = folder.newFile("prefs-ops.db")
    val xmlPrefs = InMemorySharedPreferences()
    JdbcSqlDb(dbFile).use { sqlDb ->
      val prefs = Preferences(xmlPrefs, "build-1", OopbotDatabase(sqlDb, syncExecutor))
      prefs.addWorkspace("/w/first")
      prefs.addWorkspace("/w/second/")
      prefs.trustRepository("/w/second")
      prefs.setExplorerTab("/w/second", ExplorerTab.Files)
      prefs.setDiffMode("/w/second", GitDiffMode.Uncommitted)
      prefs.touchSessions(listOf("/s/1.jsonl", "/s/2.jsonl"), at = 500L)
      prefs.archiveSessions(listOf("/s/1.jsonl"), at = 600L)
      prefs.archiveSessions(listOf("/s/1.jsonl", "/s/3.jsonl"), at = 700L)
      prefs.unarchiveSessions(listOf("/s/3.jsonl"))
      prefs.removeWorkspace("/w/first")
      prefs.setRemoteHostsCache("""[{"id":"srv"}]""")
    }

    JdbcSqlDb(dbFile).use { reopenedDb ->
      val restored = Preferences(xmlPrefs, "build-1", OopbotDatabase(reopenedDb, syncExecutor))
      assertEquals(listOf("/w/second"), restored.workspaces.value)
      assertEquals(setOf("/w/second"), restored.trustedRepositories.value)
      assertEquals(mapOf("/w/second" to ExplorerTab.Files), restored.explorerTabs.value)
      assertEquals(mapOf("/w/second" to GitDiffMode.Uncommitted), restored.diffModes.value)
      assertEquals(mapOf("/s/1.jsonl" to 600L), restored.archivedSessions.value)
      assertEquals(mapOf("/s/2.jsonl" to 500L), restored.touchedSessions.value)
      assertEquals("""[{"id":"srv"}]""", restored.remoteHostsCache())
      assertNull(xmlPrefs.getString("workspaces", null))
    }
  }
}
