package io.jeph.oopbot.data

import io.jeph.oopbot.host.HostRejected
import io.jeph.oopbot.host.SessionInfo
import java.io.IOException
import java.util.concurrent.Executor
import kotlinx.coroutines.CoroutineScope
import kotlinx.coroutines.CoroutineStart
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.SupervisorJob
import kotlinx.coroutines.async
import kotlinx.coroutines.cancel
import kotlinx.coroutines.flow.first
import kotlinx.coroutines.runBlocking
import kotlinx.coroutines.withTimeout
import org.json.JSONArray
import org.json.JSONObject
import org.junit.Assert.assertEquals
import org.junit.Assert.assertFalse
import org.junit.Assert.assertTrue
import org.junit.Rule
import org.junit.Test
import org.junit.rules.TemporaryFolder

class SessionRepositoryTest {
  @get:Rule
  val folder = TemporaryFolder()

  private class FakeSessionHost : SessionHost {
    var sessionList: List<SessionInfo> = emptyList()
    var messageFetcher: (String) -> JSONArray = { JSONArray() }
    val messageCalls = ArrayList<String>()
    val deleted = ArrayList<String>()

    override suspend fun sessions(): List<SessionInfo> = sessionList

    override suspend fun deleteSessions(paths: List<String>) {
      deleted += paths
      sessionList = sessionList.filterNot { it.path in paths }
    }

    override suspend fun sessionMessages(path: String): JSONArray {
      messageCalls += path
      return messageFetcher(path)
    }
  }

  private val syncExecutor = Executor(Runnable::run)

  @Test
  fun coldStartSeedsSessionsFromSqliteWhileKeepingLoadedFalseAndListingsQuiet() {
    val dbFile = folder.newFile("sessions.db")
    val host = FakeSessionHost()
    val scope = CoroutineScope(SupervisorJob() + Dispatchers.Unconfined)
    try {
      JdbcSqlDb(dbFile).use { sqlDb ->
        val db = OopbotDatabase(sqlDb, syncExecutor)
        db.sessions.sync(listOf(session("/s/a.jsonl", modified = 100L), session("/s/b.jsonl", modified = 200L)))

        val repo = SessionRepository(host, NoticeStore(db = db, writer = syncExecutor), scope, db)
        assertEquals(listOf("/s/b.jsonl", "/s/a.jsonl"), repo.sessions.value.map { it.path })
        assertFalse(repo.loaded.value)
        assertEquals(emptyList<List<SessionInfo>>(), repo.listings.replayCache)

        host.sessionList = listOf(session("/s/b.jsonl", modified = 300L), session("/s/c.jsonl", modified = 400L))
        runBlocking {
          val emission = async(start = CoroutineStart.UNDISPATCHED) { repo.listings.first() }
          repo.listNow()
          val emitted = withTimeout(5_000L) { emission.await() }
          assertEquals(listOf("/s/c.jsonl", "/s/b.jsonl"), emitted.map { it.path })
        }
        assertTrue(repo.loaded.value)
        assertEquals(listOf("/s/c.jsonl", "/s/b.jsonl"), db.sessions.list().map { it.path })
      }
    } finally {
      scope.cancel()
    }
  }

  @Test
  fun messagesServeCacheOnModifiedMatchAndFallbackWhenOffline() {
    val dbFile = folder.newFile("messages.db")
    val host = FakeSessionHost()
    host.sessionList = listOf(session("/s/a.jsonl", modified = 100L))
    host.messageFetcher = { JSONArray().put(JSONObject().put("role", "user").put("content", "cached phrase")) }
    val scope = CoroutineScope(SupervisorJob() + Dispatchers.Unconfined)
    try {
      JdbcSqlDb(dbFile).use { sqlDb ->
        val db = OopbotDatabase(sqlDb, syncExecutor)
        val repo = SessionRepository(host, NoticeStore(db = db, writer = syncExecutor), scope, db)
        runBlocking {
          repo.listNow()
          assertEquals(1, repo.messages("/s/a.jsonl").length())
          assertEquals(1, host.messageCalls.size)

          // Same modified timestamp: serves SQLite cache with zero extra host RPC calls.
          val cachedHit = repo.messages("/s/a.jsonl")
          assertEquals("cached phrase", cachedHit.getJSONObject(0).getString("content"))
          assertEquals(1, host.messageCalls.size)

          // Changed modified timestamp offline falls back to cached copy on IOException / transient error.
          host.sessionList = listOf(session("/s/a.jsonl", modified = 200L))
          repo.listNow()
          host.messageFetcher = { throw IOException("Termux unreachable") }
          assertEquals("cached phrase", repo.messages("/s/a.jsonl").getJSONObject(0).getString("content"))

          host.messageFetcher = { throw HostRejected("termux_busy", "Busy") }
          assertEquals("cached phrase", repo.messages("/s/a.jsonl").getJSONObject(0).getString("content"))
        }
      }
    } finally {
      scope.cancel()
    }
  }

  @Test
  fun preferCacheFalseAndNotFoundBypassCacheAndDeletePurgesEverything() {
    val dbFile = folder.newFile("messages-purge.db")
    val host = FakeSessionHost()
    host.sessionList = listOf(session("/s/a.jsonl", modified = 100L))
    host.messageFetcher = { JSONArray().put(JSONObject().put("role", "user").put("content", "cached phrase")) }
    val scope = CoroutineScope(SupervisorJob() + Dispatchers.Unconfined)
    try {
      JdbcSqlDb(dbFile).use { sqlDb ->
        val db = OopbotDatabase(sqlDb, syncExecutor)
        val deletedFromViews = ArrayList<String>()
        val repo = SessionRepository(
          host,
          NoticeStore(db = db, writer = syncExecutor),
          scope,
          db,
          onSessionsDeleted = deletedFromViews::addAll,
        )
        runBlocking {
          repo.listNow()
          repo.messages("/s/a.jsonl")

          host.sessionList = listOf(session("/s/a.jsonl", modified = 200L))
          repo.listNow()
          host.messageFetcher = { throw HostRejected("termux_busy", "Busy") }
          assertFalse(runCatching { repo.messages("/s/a.jsonl", preferCache = false) }.isSuccess)

          host.messageFetcher = { throw HostRejected("not_found", "Missing") }
          assertFalse(runCatching { repo.messages("/s/a.jsonl") }.isSuccess)

          assertEquals(setOf("/s/a.jsonl"), repo.searchMessages("phrase", setOf("/s/a.jsonl")))
          repo.delete(listOf("/s/a.jsonl"))
          assertEquals(listOf("/s/a.jsonl"), host.deleted)
          assertEquals(listOf("/s/a.jsonl"), deletedFromViews)
          assertEquals(emptySet<String>(), repo.searchMessages("phrase", setOf("/s/a.jsonl")))
        }
      }
    } finally {
      scope.cancel()
    }
  }

  private fun session(path: String, modified: Long) = SessionInfo(
    path = path,
    id = path,
    cwd = "/w/app",
    cwdExists = true,
    name = "Chat",
    created = 1L,
    modified = modified,
    messageCount = 1,
    firstMessage = "hello",
  )
}
