package io.jeph.oopbot.ui.chat

import io.jeph.oopbot.data.JdbcSqlDb
import io.jeph.oopbot.data.OopbotDatabase
import java.util.concurrent.Executor
import org.junit.Assert.assertEquals
import org.junit.Assert.assertFalse
import org.junit.Assert.assertNull
import org.junit.Assert.assertTrue
import org.junit.Rule
import org.junit.Test
import org.junit.rules.TemporaryFolder

class ChatDraftsTest {
  @get:Rule
  val folder = TemporaryFolder()

  private val syncExecutor = Executor(Runnable::run)

  @Test
  fun draftsSteersScrollAndExpandedFoldsRoundtripAcrossDatabaseRestart() {
    val dbFile = folder.newFile("drafts.db")
    val path = "/s/chat-1.jsonl"
    JdbcSqlDb(dbFile).use { sqlDb ->
      val db = OopbotDatabase(sqlDb, syncExecutor)
      val views = ChatViews(db)
      val view = ChatView().apply {
        draft.edit { append("Unsent composer prompt") }
        steerDraft("call_1").edit { append("Steer subagent") }
        steerDraft("call_empty")
        scroll.follow.value = false
        scroll.top = StreamTop("m.4", offset = 38)
        expanded.addAll(setOf("u.0", "steps.2"))
      }

      views.saveNow(path, view)
    }

    JdbcSqlDb(dbFile).use { reopenedDb ->
      val restarted = ChatViews(OopbotDatabase(reopenedDb, syncExecutor))
      val restoredView = ChatView()
      restarted.restoreFor(path, restoredView)

      assertEquals("Unsent composer prompt", restoredView.draft.text.toString())
      assertEquals("Steer subagent", restoredView.steerDraft("call_1").text.toString())
      assertEquals("", restoredView.steerDraft("call_empty").text.toString())
      assertFalse(restoredView.scroll.follow.value)
      assertEquals(StreamTop("m.4", offset = 38), restoredView.scroll.top)
      assertEquals(setOf("u.0", "steps.2"), restoredView.expanded.toSet())
    }
  }

  @Test
  fun clearingDraftsAndReturningToFollowDeletesStoredRowAndForgetDropsCache() {
    val dbFile = folder.newFile("drafts-clear.db")
    val path = "/s/chat-2.jsonl"
    JdbcSqlDb(dbFile).use { sqlDb ->
      val db = OopbotDatabase(sqlDb, syncExecutor)
      val views = ChatViews(db)
      val view = ChatView().apply {
        draft.edit { append("Temp draft") }
        scroll.follow.value = false
        scroll.top = StreamTop("m.1", offset = 10)
      }
      views.saveNow(path, view)
      assertEquals(1, db.drafts.loadAll().size)

      view.draft.edit { replace(0, length, "") }
      view.scroll.follow.value = true
      views.saveNow(path, view)
      assertTrue(db.drafts.loadAll().isEmpty())

      view.draft.edit { append("Re-typed") }
      views.saveNow(path, view)
      views.forget(listOf(path))
      val freshView = ChatView()
      views.restoreFor(path, freshView)
      assertEquals("", freshView.draft.text.toString())
      assertNull(freshView.scroll.top)
    }
  }
}
