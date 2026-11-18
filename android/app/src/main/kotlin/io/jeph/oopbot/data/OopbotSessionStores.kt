package io.jeph.oopbot.data

import android.util.Log
import io.jeph.oopbot.agent.ChatLocation
import io.jeph.oopbot.agent.InboxEntry
import io.jeph.oopbot.agent.InboxReason
import io.jeph.oopbot.host.RemotePath
import io.jeph.oopbot.host.SessionInfo
import org.json.JSONArray
import org.json.JSONException
import org.json.JSONObject

internal data class CachedMessages(val syncedModified: Long, val messagesJson: String)

internal data class ChatDraftSnapshot(
  val sessionPath: String,
  val draftText: String,
  val steerDrafts: Map<String, String>,
  val scrollFollow: Boolean,
  val scrollTopKey: String?,
  val scrollTopOffset: Int,
  val expandedKeys: Set<String>,
) {
  val isEmpty: Boolean
    get() = draftText.isEmpty() &&
      steerDrafts.isEmpty() &&
      scrollFollow &&
      scrollTopKey == null &&
      scrollTopOffset == 0 &&
      expandedKeys.isEmpty()
}

/** SQLite storage for `session_index`, `message_cache`, and FTS4 `message_fts`. */
internal class SessionsDbStore(
  private val db: SqlDb,
  private val enqueueWrite: (SqlDb.() -> Unit) -> Unit,
  private val clock: () -> Long,
) {
  fun list(): List<SessionInfo> = querySessions(db)

  /** Incrementally reconciles `session_index` against [listed] on the DB writer. */
  fun syncAsync(listed: List<SessionInfo>) {
    enqueueWrite { applySync(listed) }
  }

  /** Incrementally reconciles `session_index` against [listed] synchronously inside one transaction. */
  fun sync(listed: List<SessionInfo>) {
    db.transaction { applySync(listed) }
  }

  private fun querySessions(target: SqlDb): List<SessionInfo> = target.query(
    """
    SELECT path, id, cwd, cwd_exists, name, created, modified, message_count, first_message, subagents,
      remote_host_id, remote_path
    FROM session_index
    ORDER BY modified DESC, path ASC
    """.trimIndent(),
  ) { row ->
    val remoteHostId = row.stringOrNull(10)
    SessionInfo(
      path = row.string(0),
      id = row.string(1),
      cwd = row.string(2),
      cwdExists = row.int(3) != 0,
      name = row.stringOrNull(4),
      created = row.long(5),
      modified = row.long(6),
      messageCount = row.int(7),
      firstMessage = row.string(8),
      subagents = row.int(9),
      remote = remoteHostId?.let { RemotePath(it, row.stringOrNull(11).orEmpty()) },
    )
  }

  private fun SqlDb.applySync(listed: List<SessionInfo>) {
    val existing = querySessions(this).associateBy { it.path }
    val listedPaths = listed.mapTo(HashSet()) { it.path }
    val removed = existing.keys.filter { it !in listedPaths }
    forEachSqlChunk(removed) { chunk ->
      val marks = sqlPlaceholders(chunk.size)
      exec("DELETE FROM session_index WHERE path IN ($marks)", chunk.toTypedArray<Any?>())
    }
    for (info in listed) {
      if (existing[info.path] == info) continue
      exec(
        """
        INSERT OR REPLACE INTO session_index(
          path, id, cwd, cwd_exists, name, created, modified, message_count, first_message, subagents,
          remote_host_id, remote_path
        ) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
        """.trimIndent(),
        arrayOf(
          info.path,
          info.id,
          info.cwd,
          if (info.cwdExists) 1 else 0,
          info.name,
          info.created,
          info.modified,
          info.messageCount,
          info.firstMessage,
          info.subagents,
          info.remote?.hostId,
          info.remote?.path,
        ),
      )
    }
  }

  fun cachedMessages(path: String): CachedMessages? = try {
    db.queryOne(
      "SELECT synced_modified, messages_json FROM message_cache WHERE session_path = ?",
      arrayOf(path),
    ) { row -> CachedMessages(syncedModified = row.long(0), messagesJson = row.string(1)) }
  } catch (error: OopbotSqlException) {
    Log.w(TAG, "Reading cached messages for $path failed", error)
    null
  }

  fun saveMessagesSync(path: String, syncedModified: Long, messagesJson: String, bodyText: String) {
    try {
      db.transaction {
        writeMessages(path, syncedModified, messagesJson, bodyText, clock())
      }
    } catch (error: OopbotSqlException) {
      Log.w(TAG, "Saving cached messages for $path failed", error)
    }
  }

  fun saveMessagesAsync(path: String, syncedModified: Long, messagesJson: String, bodyText: String) {
    val now = clock()
    enqueueWrite { writeMessages(path, syncedModified, messagesJson, bodyText, now) }
  }

  private fun SqlDb.writeMessages(
    path: String,
    syncedModified: Long,
    messagesJson: String,
    bodyText: String,
    updatedAt: Long,
  ) {
    exec(
      """
      INSERT OR REPLACE INTO message_cache(session_path, synced_modified, messages_json, updated_at)
      VALUES (?, ?, ?, ?)
      """.trimIndent(),
      arrayOf(path, syncedModified, messagesJson, updatedAt),
    )
    exec("DELETE FROM message_fts WHERE session_path = ?", arrayOf(path))
    if (bodyText.isNotBlank()) {
      exec("INSERT INTO message_fts(session_path, body) VALUES (?, ?)", arrayOf(path, bodyText))
    }
  }

  /** Returns matching session paths among [allowedPaths] via FTS4 `MATCH`, or empty set when query has no tokens. */
  fun searchMessages(query: String, allowedPaths: Set<String>): Set<String> {
    if (allowedPaths.isEmpty()) return emptySet()
    val ftsQuery = TranscriptIndexer.buildFtsQuery(query) ?: return emptySet()
    return try {
      db.query(
        "SELECT session_path FROM message_fts WHERE message_fts MATCH ?",
        arrayOf(ftsQuery),
      ) { it.string(0) }.filterTo(HashSet()) { it in allowedPaths }
    } catch (error: OopbotSqlException) {
      Log.w(TAG, "FTS search failed for query", error)
      emptySet()
    }
  }

  /** Deletes every indexed trace of deleted sessions across index, cache, FTS, drafts, and inbox. */
  fun purge(paths: Collection<String>) {
    if (paths.isEmpty()) return
    enqueueWrite {
      forEachSqlChunk(paths) { chunk ->
        val marks = sqlPlaceholders(chunk.size)
        val args = chunk.toTypedArray<Any?>()
        exec("DELETE FROM session_index WHERE path IN ($marks)", args)
        exec("DELETE FROM message_cache WHERE session_path IN ($marks)", args)
        exec("DELETE FROM message_fts WHERE session_path IN ($marks)", args)
        exec("DELETE FROM chat_drafts WHERE session_path IN ($marks)", args)
        exec("DELETE FROM chat_inbox WHERE session_path IN ($marks)", args)
      }
    }
  }

  private companion object {
    const val TAG = "OopbotSessionsDb"
  }
}

/** Process-death persistence for per-chat composer drafts, steer drafts, stream scroll, and expanded folds. */
internal class DraftsDbStore(
  private val db: SqlDb,
  private val enqueueWrite: (SqlDb.() -> Unit) -> Unit,
  private val clock: () -> Long,
) {
  fun loadAll(): List<ChatDraftSnapshot> = try {
    db.query(
      """
      SELECT session_path, draft_text, steer_drafts_json, scroll_follow, scroll_top_key, scroll_top_offset,
        expanded_keys_json
      FROM chat_drafts
      """.trimIndent(),
    ) { row ->
      ChatDraftSnapshot(
        sessionPath = row.string(0),
        draftText = row.string(1),
        steerDrafts = parseStringMap(row.string(2)),
        scrollFollow = row.int(3) != 0,
        scrollTopKey = row.stringOrNull(4),
        scrollTopOffset = row.int(5),
        expandedKeys = parseStringSet(row.string(6)),
      )
    }
  } catch (error: OopbotSqlException) {
    Log.w(TAG, "Loading chat drafts failed", error)
    emptyList()
  }

  fun save(snapshot: ChatDraftSnapshot) {
    val now = clock()
    enqueueWrite {
      if (snapshot.isEmpty) {
        exec("DELETE FROM chat_drafts WHERE session_path = ?", arrayOf(snapshot.sessionPath))
      } else {
        exec(
          """
          INSERT OR REPLACE INTO chat_drafts(
            session_path, draft_text, steer_drafts_json, scroll_follow, scroll_top_key, scroll_top_offset,
            expanded_keys_json, updated_at
          ) VALUES (?, ?, ?, ?, ?, ?, ?, ?)
          """.trimIndent(),
          arrayOf(
            snapshot.sessionPath,
            snapshot.draftText,
            JSONObject(snapshot.steerDrafts).toString(),
            if (snapshot.scrollFollow) 1 else 0,
            snapshot.scrollTopKey,
            snapshot.scrollTopOffset,
            JSONArray(snapshot.expandedKeys).toString(),
            now,
          ),
        )
      }
    }
  }

  private fun parseStringMap(json: String): Map<String, String> = try {
    val obj = JSONObject(json)
    obj.keys().asSequence().associateWith { obj.optString(it) }.filterValues { it.isNotEmpty() }
  } catch (_: JSONException) {
    emptyMap()
  }

  private fun parseStringSet(json: String): Set<String> = try {
    val array = JSONArray(json)
    List(array.length()) { array.optString(it) }.filterTo(HashSet()) { it.isNotEmpty() }
  } catch (_: JSONException) {
    emptySet()
  }

  private companion object {
    const val TAG = "OopbotDraftsDb"
  }
}

/** Process-death persistence for unread [InboxEntry] alerts in `chat_inbox`. */
internal class InboxDbStore(
  private val db: SqlDb,
  private val enqueueWrite: (SqlDb.() -> Unit) -> Unit,
) {
  fun loadAll(): List<InboxEntry> = try {
    db.query(
      "SELECT session_path, cwd, reason, title, detail, at, asker FROM chat_inbox ORDER BY at DESC",
    ) { row ->
      val reason = InboxReason.entries.firstOrNull { it.name == row.string(2) } ?: return@query null
      InboxEntry(
        location = ChatLocation(cwd = row.string(1), sessionPath = row.string(0)),
        reason = reason,
        title = row.stringOrNull(3),
        detail = row.stringOrNull(4),
        at = row.long(5),
        asker = row.stringOrNull(6),
      )
    }.filterNotNull()
  } catch (error: OopbotSqlException) {
    Log.w(TAG, "Loading chat inbox failed", error)
    emptyList()
  }

  fun upsert(entry: InboxEntry) {
    enqueueWrite {
      exec(
        """
        INSERT OR REPLACE INTO chat_inbox(session_path, cwd, reason, title, detail, at, asker)
        VALUES (?, ?, ?, ?, ?, ?, ?)
        """.trimIndent(),
        arrayOf(
          entry.location.sessionPath,
          entry.location.cwd,
          entry.reason.name,
          entry.title,
          entry.detail,
          entry.at,
          entry.asker,
        ),
      )
    }
  }

  fun deleteWaiting(sessionPath: String) {
    enqueueWrite {
      exec(
        "DELETE FROM chat_inbox WHERE session_path = ? AND reason = ?",
        arrayOf(sessionPath, InboxReason.Waiting.name),
      )
    }
  }

  fun updateAsker(sessionPath: String, asker: String) {
    enqueueWrite {
      exec(
        "UPDATE chat_inbox SET asker = ? WHERE session_path = ? AND reason = ?",
        arrayOf(asker, sessionPath, InboxReason.Waiting.name),
      )
    }
  }

  fun deletePaths(paths: Collection<String>) {
    if (paths.isEmpty()) return
    enqueueWrite {
      forEachSqlChunk(paths) { chunk ->
        val marks = sqlPlaceholders(chunk.size)
        exec("DELETE FROM chat_inbox WHERE session_path IN ($marks)", chunk.toTypedArray<Any?>())
      }
    }
  }

  fun clear() {
    enqueueWrite { exec("DELETE FROM chat_inbox") }
  }

  private companion object {
    const val TAG = "OopbotInboxDb"
  }
}
