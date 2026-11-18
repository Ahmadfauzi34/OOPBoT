package io.jeph.oopbot.data

import android.util.Log
import io.jeph.oopbot.agent.AppNotice
import io.jeph.oopbot.agent.NoticeLevel
import io.jeph.oopbot.agent.SavedNotice
import io.jeph.oopbot.host.GitDiffMode
import io.jeph.oopbot.ui.git.ExplorerTab

internal data class LegacyPrefsData(
  val workspaces: List<String>,
  val archived: Map<String, Long>,
  val touched: Map<String, Long>,
  val explorerTabs: Map<String, ExplorerTab>,
  val diffModes: Map<String, GitDiffMode>,
  val trustedRepositories: Set<String>,
  val remoteHostsCache: String?,
)

/** Per-item preference and session metadata storage replacing large JSON blobs in SharedPreferences XML. */
@Suppress("TooManyFunctions") // Focused CRUD queries over schema_meta, workspace_prefs, and session_meta.
internal class PrefsDbStore(
  private val db: SqlDb,
  private val enqueueWrite: (SqlDb.() -> Unit) -> Unit,
) {
  fun isMigrated(flagKey: String): Boolean =
    db.queryOne("SELECT value FROM schema_meta WHERE key = ?", arrayOf(flagKey)) { it.string(0) } == "1"

  fun migrateLegacy(flagKey: String, legacy: LegacyPrefsData) {
    db.transaction {
      insertLegacyWorkspaces(legacy)
      insertLegacySessions(legacy)
      exec("INSERT OR REPLACE INTO schema_meta(key, value) VALUES (?, ?)", arrayOf(flagKey, "1"))
    }
  }

  private fun SqlDb.insertLegacyWorkspaces(legacy: LegacyPrefsData) {
    legacy.workspaces.map(::folderKey).distinct().forEachIndexed { index, cwd ->
      exec(
        "INSERT OR REPLACE INTO workspace_prefs(scope, item_key, value, sort_order) VALUES (?, ?, ?, ?)",
        arrayOf(SCOPE_WORKSPACE, cwd, cwd, index),
      )
    }
    for ((folder, tab) in legacy.explorerTabs) {
      exec(
        "INSERT OR REPLACE INTO workspace_prefs(scope, item_key, value, sort_order) VALUES (?, ?, ?, 0)",
        arrayOf(SCOPE_EXPLORER_TAB, folder, tab.name),
      )
    }
    for ((folder, mode) in legacy.diffModes) {
      exec(
        "INSERT OR REPLACE INTO workspace_prefs(scope, item_key, value, sort_order) VALUES (?, ?, ?, 0)",
        arrayOf(SCOPE_DIFF_MODE, folder, mode.name),
      )
    }
    for (folder in legacy.trustedRepositories) {
      exec(
        "INSERT OR REPLACE INTO workspace_prefs(scope, item_key, value, sort_order) VALUES (?, ?, ?, 0)",
        arrayOf(SCOPE_TRUSTED_REPO, folder, "1"),
      )
    }
    legacy.remoteHostsCache?.let { json ->
      exec(
        "INSERT OR REPLACE INTO workspace_prefs(scope, item_key, value, sort_order) VALUES (?, ?, ?, 0)",
        arrayOf(SCOPE_CACHE, KEY_REMOTE_HOSTS, json),
      )
    }
  }

  private fun SqlDb.insertLegacySessions(legacy: LegacyPrefsData) {
    for (path in legacy.archived.keys + legacy.touched.keys) {
      exec(
        "INSERT OR REPLACE INTO session_meta(path, archived_at, touched_at) VALUES (?, ?, ?)",
        arrayOf(path, legacy.archived[path], legacy.touched[path]),
      )
    }
  }

  fun readWorkspaces(): List<String> = db.query(
    "SELECT item_key FROM workspace_prefs WHERE scope = ? ORDER BY sort_order ASC, rowid ASC",
    arrayOf(SCOPE_WORKSPACE),
  ) { it.string(0) }

  fun readArchived(): Map<String, Long> = db.query(
    "SELECT path, archived_at FROM session_meta WHERE archived_at IS NOT NULL",
  ) { it.string(0) to it.long(1) }.toMap()

  fun readTouched(): Map<String, Long> = db.query(
    "SELECT path, touched_at FROM session_meta WHERE touched_at IS NOT NULL",
  ) { it.string(0) to it.long(1) }.toMap()

  fun <T : Enum<T>> readEnumMap(scope: String, entries: List<T>): Map<String, T> = db.query(
    "SELECT item_key, value FROM workspace_prefs WHERE scope = ?",
    arrayOf(scope),
  ) { row ->
    val value = row.string(1)
    entries.firstOrNull { it.name == value }?.let { row.string(0) to it }
  }.filterNotNull().toMap()

  fun readTrustedRepositories(): Set<String> = db.query(
    "SELECT item_key FROM workspace_prefs WHERE scope = ?",
    arrayOf(SCOPE_TRUSTED_REPO),
  ) { it.string(0) }.toSet()

  fun readRemoteHostsCache(): String? = db.queryOne(
    "SELECT value FROM workspace_prefs WHERE scope = ? AND item_key = ?",
    arrayOf(SCOPE_CACHE, KEY_REMOTE_HOSTS),
  ) { it.string(0) }

  fun addWorkspace(folderKey: String) {
    enqueueWrite {
      exec(
        """
        INSERT OR REPLACE INTO workspace_prefs(scope, item_key, value, sort_order)
        VALUES (
          ?, ?, ?,
          COALESCE((SELECT MIN(sort_order) FROM workspace_prefs WHERE scope = ?), 0) - 1
        )
        """.trimIndent(),
        arrayOf(SCOPE_WORKSPACE, folderKey, folderKey, SCOPE_WORKSPACE),
      )
    }
  }

  fun removeWorkspace(folderKey: String) {
    enqueueWrite {
      exec(
        "DELETE FROM workspace_prefs WHERE scope IN (?, ?) AND item_key = ?",
        arrayOf(SCOPE_WORKSPACE, SCOPE_TRUSTED_REPO, folderKey),
      )
    }
  }

  fun archiveSessions(paths: Collection<String>, at: Long) {
    if (paths.isEmpty()) return
    enqueueWrite {
      for (path in paths) {
        exec(
          """
          INSERT OR REPLACE INTO session_meta(path, archived_at, touched_at)
          VALUES (?, COALESCE((SELECT archived_at FROM session_meta WHERE path = ?), ?), NULL)
          """.trimIndent(),
          arrayOf(path, path, at),
        )
      }
    }
  }

  fun unarchiveSessions(paths: Collection<String>) {
    if (paths.isEmpty()) return
    enqueueWrite {
      forEachSqlChunk(paths) { chunk ->
        val marks = sqlPlaceholders(chunk.size)
        val args = chunk.toTypedArray<Any?>()
        exec("UPDATE session_meta SET archived_at = NULL WHERE path IN ($marks)", args)
        exec("DELETE FROM session_meta WHERE archived_at IS NULL AND touched_at IS NULL AND path IN ($marks)", args)
      }
    }
  }

  fun touchSessions(paths: Collection<String>, at: Long) {
    if (paths.isEmpty()) return
    enqueueWrite {
      for (path in paths) {
        exec(
          """
          INSERT OR REPLACE INTO session_meta(path, archived_at, touched_at)
          VALUES (?, (SELECT archived_at FROM session_meta WHERE path = ?), ?)
          """.trimIndent(),
          arrayOf(path, path, at),
        )
      }
    }
  }

  fun setExplorerTab(folderKey: String, tab: ExplorerTab) {
    enqueueWrite {
      exec(
        "INSERT OR REPLACE INTO workspace_prefs(scope, item_key, value, sort_order) VALUES (?, ?, ?, 0)",
        arrayOf(SCOPE_EXPLORER_TAB, folderKey, tab.name),
      )
    }
  }

  fun setDiffMode(folderKey: String, mode: GitDiffMode) {
    enqueueWrite {
      exec(
        "INSERT OR REPLACE INTO workspace_prefs(scope, item_key, value, sort_order) VALUES (?, ?, ?, 0)",
        arrayOf(SCOPE_DIFF_MODE, folderKey, mode.name),
      )
    }
  }

  fun trustRepository(folderKey: String) {
    enqueueWrite {
      exec(
        "INSERT OR REPLACE INTO workspace_prefs(scope, item_key, value, sort_order) VALUES (?, ?, ?, 0)",
        arrayOf(SCOPE_TRUSTED_REPO, folderKey, "1"),
      )
    }
  }

  fun setRemoteHostsCache(json: String) {
    enqueueWrite {
      exec(
        "INSERT OR REPLACE INTO workspace_prefs(scope, item_key, value, sort_order) VALUES (?, ?, ?, 0)",
        arrayOf(SCOPE_CACHE, KEY_REMOTE_HOSTS, json),
      )
    }
  }

  companion object {
    const val SCOPE_WORKSPACE = "workspace"
    const val SCOPE_EXPLORER_TAB = "explorer_tab"
    const val SCOPE_DIFF_MODE = "diff_mode"
    const val SCOPE_TRUSTED_REPO = "trusted_repo"
    const val SCOPE_CACHE = "cache"
    const val KEY_REMOTE_HOSTS = "remote_hosts"
  }
}

/** Persistent store for `session_notices` replacing one `.json` file per session. */
internal class NoticesDbStore(private val db: SqlDb) {
  fun load(sessionPath: String): List<SavedNotice> = try {
    db.query(
      """
      SELECT level, message, after_ts, after_run, app_notice
      FROM session_notices
      WHERE session_path = ?
      ORDER BY ord ASC, id ASC
      """.trimIndent(),
      arrayOf(sessionPath),
    ) { row ->
      val level = NoticeLevel.entries.firstOrNull { it.name == row.string(0) } ?: return@query null
      val appNoticeName = row.stringOrNull(4)
      val appNotice = if (appNoticeName == null) {
        null
      } else {
        AppNotice.entries.firstOrNull { it.name == appNoticeName } ?: return@query null
      }
      SavedNotice(
        level = level,
        message = row.string(1),
        after = row.longOrNull(2),
        afterRun = row.int(3) != 0,
        appNotice = appNotice,
      )
    }.filterNotNull()
  } catch (error: OopbotSqlException) {
    Log.w(TAG, "Reading notices for $sessionPath failed", error)
    emptyList()
  }

  fun replace(sessionPath: String, notices: List<SavedNotice>) {
    try {
      db.transaction {
        exec("DELETE FROM session_notices WHERE session_path = ?", arrayOf(sessionPath))
        notices.forEachIndexed { index, notice ->
          exec(
            """
            INSERT INTO session_notices(session_path, ord, level, message, after_ts, after_run, app_notice)
            VALUES (?, ?, ?, ?, ?, ?, ?)
            """.trimIndent(),
            arrayOf(
              sessionPath,
              index,
              notice.level.name,
              notice.message,
              notice.after,
              if (notice.afterRun) 1 else 0,
              notice.appNotice?.name,
            ),
          )
        }
      }
    } catch (error: OopbotSqlException) {
      Log.w(TAG, "Saving notices for $sessionPath failed", error)
    }
  }

  fun deleteBatch(sessionPaths: Collection<String>) {
    if (sessionPaths.isEmpty()) return
    try {
      db.transaction {
        forEachSqlChunk(sessionPaths) { chunk ->
          val marks = sqlPlaceholders(chunk.size)
          exec("DELETE FROM session_notices WHERE session_path IN ($marks)", chunk.toTypedArray<Any?>())
        }
      }
    } catch (error: OopbotSqlException) {
      Log.w(TAG, "Deleting notices batch failed", error)
    }
  }

  private companion object {
    const val TAG = "OopbotNoticesDb"
  }
}
