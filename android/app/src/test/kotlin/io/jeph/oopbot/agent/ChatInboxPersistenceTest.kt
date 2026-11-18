package io.jeph.oopbot.agent

import io.jeph.oopbot.data.JdbcSqlDb
import io.jeph.oopbot.data.OopbotDatabase
import java.util.concurrent.Executor
import org.junit.Assert.assertEquals
import org.junit.Rule
import org.junit.Test
import org.junit.rules.TemporaryFolder

class ChatInboxPersistenceTest {
  @get:Rule
  val folder = TemporaryFolder()

  private val syncExecutor = Executor(Runnable::run)

  @Test
  fun entriesAskerDialogGoneOnScreenArchivedAndRemovePersistAcrossRestart() {
    val dbFile = folder.newFile("inbox.db")
    JdbcSqlDb(dbFile).use { sqlDb ->
      val db = OopbotDatabase(sqlDb, syncExecutor)
      val inbox = ChatInbox(db)
      inbox.add(InboxEntry(ChatLocation("/w/a", "/s/1.jsonl"), InboxReason.Finished, "First", "Done", 10L))
      inbox.add(InboxEntry(ChatLocation("/w/b", "/s/2.jsonl"), InboxReason.Waiting, "Second", "Confirm?", 20L))
      inbox.add(InboxEntry(ChatLocation("/w/c", "/s/3.jsonl"), InboxReason.Waiting, "Third", "Pick?", 30L))
      inbox.add(InboxEntry(ChatLocation("/w/d", "/s/4.jsonl"), InboxReason.Exited, "Fourth", "Exit 1", 40L))
      inbox.add(InboxEntry(ChatLocation("/w/e", "/s/5.jsonl"), InboxReason.Failed, "Fifth", "Oops", 50L))

      inbox.nameAsker("/s/2.jsonl", "Explore")
      inbox.dialogGone("/s/3.jsonl")
      inbox.setOnScreen(setOf("/s/4.jsonl"))
      inbox.setArchived(setOf("/s/5.jsonl"))

      val reopened = ChatInbox(OopbotDatabase(sqlDb, syncExecutor))
      assertEquals(listOf("/s/2.jsonl", "/s/1.jsonl"), reopened.entries.value.map { it.location.sessionPath })
      assertEquals(listOf("Explore", null), reopened.entries.value.map { it.asker })

      reopened.remove("/s/1.jsonl")
      val remaining = ChatInbox(OopbotDatabase(sqlDb, syncExecutor)).entries.value.map { it.location.sessionPath }
      assertEquals(listOf("/s/2.jsonl"), remaining)

      reopened.enabled = false
      assertEquals(emptyList<InboxEntry>(), ChatInbox(OopbotDatabase(sqlDb, syncExecutor)).entries.value)
    }
  }
}
