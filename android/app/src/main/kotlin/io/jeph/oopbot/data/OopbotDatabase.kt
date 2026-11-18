package io.jeph.oopbot.data

import android.content.Context
import android.util.Log
import java.util.concurrent.Executor
import java.util.concurrent.Executors

/**
 * Single owner of OOPBoT's local SQLite WAL + FTS4 database (`oopbot.db`), coordinating non-blocking serialized writes
 * through [writer] and exposing domain stores for preferences, notices, sessions, drafts, and the inbox.
 */
internal class OopbotDatabase(
  val db: SqlDb,
  val writer: Executor = defaultWriter(),
  clock: () -> Long = System::currentTimeMillis,
) : AutoCloseable {
  val prefs = PrefsDbStore(db, ::enqueueWrite)
  val notices = NoticesDbStore(db)
  val sessions = SessionsDbStore(db, ::enqueueWrite, clock)
  val drafts = DraftsDbStore(db, ::enqueueWrite, clock)
  val inbox = InboxDbStore(db, ::enqueueWrite)

  /** Runs [block] inside a serialized SQLite transaction on [writer]. */
  fun enqueueWrite(block: SqlDb.() -> Unit) {
    writer.execute {
      try {
        db.transaction(block)
      } catch (error: OopbotSqlException) {
        Log.w(TAG, "Async SQLite write failed", error)
      }
    }
  }

  override fun close() {
    db.close()
  }

  companion object {
    private const val TAG = "OopbotDatabase"

    fun open(context: Context): OopbotDatabase = OopbotDatabase(AndroidSqlDb(context))

    private fun defaultWriter(): Executor = Executors.newSingleThreadExecutor { runnable ->
      Thread(runnable, "oopbot-db-writer").apply { isDaemon = true }
    }
  }
}
