package io.jeph.oopbot.data

import io.jeph.oopbot.agent.ChatLocation
import io.jeph.oopbot.agent.InboxEntry
import io.jeph.oopbot.agent.InboxReason
import io.jeph.oopbot.host.SessionInfo
import java.util.concurrent.Executor
import org.json.JSONArray
import org.json.JSONObject
import org.junit.Assert.assertEquals
import org.junit.Assert.assertFalse
import org.junit.Assert.assertTrue
import org.junit.Rule
import org.junit.Test
import org.junit.rules.TemporaryFolder

class OopbotDatabaseTest {
  @get:Rule
  val folder = TemporaryFolder()

  private val syncExecutor = Executor(Runnable::run)

  @Test
  fun opensFileDatabaseWithWalAndForeignKeysEnabled() {
    val dbFile = folder.newFile("oopbot.db")
    JdbcSqlDb(dbFile).use { sqlDb ->
      assertEquals("wal", sqlDb.pragmaString("journal_mode").lowercase())
      assertEquals("1", sqlDb.pragmaString("foreign_keys"))
      val db = OopbotDatabase(sqlDb, syncExecutor)
      db.sessions.sync(listOf(session("/s/a.jsonl", modified = 100L)))
      assertEquals(1, db.sessions.list().size)
    }
  }

  @Test
  fun fts4IndexesTranscriptsStrippingImagesAndSkillsAndHandlesOperatorQueriesSafely() {
    val dbFile = folder.newFile("fts.db")
    JdbcSqlDb(dbFile).use { sqlDb ->
      val db = OopbotDatabase(sqlDb, syncExecutor)
      val messagesA = sampleTranscriptWithSkillAndImage()
      val messagesB = JSONArray().put(
        JSONObject().put("role", "user").put("content", "Deploy Kubernetes manifests to staging"),
      )

      val bodyA = TranscriptIndexer.extractBody(messagesA)
      val bodyB = TranscriptIndexer.extractBody(messagesB)

      assertFalse(bodyA.contains("BASE64_SHOULD_BE_IGNORED"))
      assertFalse(bodyA.contains("hidden-skill-instructions"))
      assertTrue(bodyA.contains("/skill:deploy staging"))
      assertTrue(bodyA.contains("run_shell"))
      assertTrue(bodyA.contains("Authentication token failed"))

      db.sessions.saveMessagesSync("/s/a.jsonl", 10L, messagesA.toString(), bodyA)
      db.sessions.saveMessagesSync("/s/b.jsonl", 20L, messagesB.toString(), bodyB)

      val allPaths = setOf("/s/a.jsonl", "/s/b.jsonl")
      assertEquals(setOf("/s/b.jsonl"), db.sessions.searchMessages("kuber", allPaths))
      assertEquals(setOf("/s/a.jsonl"), db.sessions.searchMessages("NOT OR (fail) \"token\"", allPaths))
      assertEquals(setOf("/s/a.jsonl"), db.sessions.searchMessages("c++ stag", allPaths))
      assertEquals(emptySet<String>(), db.sessions.searchMessages("kuber", setOf("/s/a.jsonl")))
      assertEquals(emptySet<String>(), db.sessions.searchMessages("   ", allPaths))
      assertEquals(emptySet<String>(), db.sessions.searchMessages("!@#$%^&*()", allPaths))
    }
  }

  @Test
  fun purgeRemovesAllRowsAcrossSessionTables() {
    val dbFile = folder.newFile("purge.db")
    JdbcSqlDb(dbFile).use { sqlDb ->
      val db = OopbotDatabase(sqlDb, syncExecutor)
      db.sessions.sync(listOf(session("/s/a.jsonl", modified = 1L), session("/s/b.jsonl", modified = 2L)))
      db.sessions.saveMessagesSync("/s/a.jsonl", 1L, "[]", "hello world")
      db.sessions.saveMessagesSync("/s/b.jsonl", 2L, "[]", "hello again")
      db.drafts.save(
        ChatDraftSnapshot("/s/a.jsonl", "draft", emptyMap(), scrollFollow = false, "k1", 12, setOf("f1")),
      )
      db.inbox.upsert(InboxEntry(ChatLocation("/w", "/s/a.jsonl"), InboxReason.Finished, "Title", "Done", 50L))

      db.sessions.purge(listOf("/s/a.jsonl"))

      assertEquals(listOf("/s/b.jsonl"), db.sessions.list().map { it.path })
      assertEquals(null, db.sessions.cachedMessages("/s/a.jsonl"))
      assertEquals(emptySet<String>(), db.sessions.searchMessages("hello", setOf("/s/a.jsonl")))
      assertEquals(setOf("/s/b.jsonl"), db.sessions.searchMessages("hello", setOf("/s/b.jsonl")))
      assertEquals(emptyList<ChatDraftSnapshot>(), db.drafts.loadAll())
      assertEquals(emptyList<InboxEntry>(), db.inbox.loadAll())
    }
  }

  private fun sampleTranscriptWithSkillAndImage(): JSONArray = JSONArray()
    .put(
      JSONObject().put("role", "user").put(
        "content",
        JSONArray()
          .put(
            JSONObject().put("type", "text").put(
              "text",
              "<skill name=\"deploy\" location=\"/x/SKILL.md\">\nhidden-skill-instructions\n</skill>\n\nstaging C++",
            ),
          )
          .put(JSONObject().put("type", "image").put("data", "BASE64_SHOULD_BE_IGNORED")),
      ),
    )
    .put(
      JSONObject()
        .put("role", "assistant")
        .put("stopReason", "error")
        .put("errorMessage", "Authentication token failed")
        .put(
          "content",
          JSONArray().put(
            JSONObject()
              .put("type", "toolCall")
              .put("name", "run_shell")
              .put("arguments", JSONObject().put("command", "pytest -k smoke")),
          ),
        ),
    )

  private fun session(path: String, modified: Long) = SessionInfo(
    path = path,
    id = path,
    cwd = "/w/project",
    cwdExists = true,
    name = "Chat",
    created = 10L,
    modified = modified,
    messageCount = 2,
    firstMessage = "hi",
  )
}
