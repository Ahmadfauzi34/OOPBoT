package io.jeph.oopbot.data

/**
 * DDL for `oopbot.db`.
 *
 * Uses FTS4 (`tokenize=unicode61`, `notindexed=session_path`) rather than FTS5 because AOSP's framework SQLite build
 * (`SQLiteOpenHelper`, minSdk 29+) compiles `-DSQLITE_ENABLE_FTS4` without `-DSQLITE_ENABLE_FTS5`, providing fast
 * Unicode full-text and prefix search with zero bundled native APK bloat. Queries also stick to SQLite 3.8+ SQL
 * (no `UPSERT`/`RETURNING`/`STRICT`) for full API 29 compatibility.
 */
internal object OopbotSchema {
  const val DB_NAME = "oopbot.db"
  const val VERSION = 1

  fun create(db: SqlDb) {
    for (statement in STATEMENTS) db.exec(statement)
  }

  fun upgrade(db: SqlDb, oldVersion: Int, newVersion: Int) {
    if (oldVersion < newVersion) create(db)
  }

  private val STATEMENTS = listOf(
    """
    CREATE TABLE IF NOT EXISTS schema_meta (
      key TEXT PRIMARY KEY NOT NULL,
      value TEXT NOT NULL
    )
    """.trimIndent(),
    """
    CREATE TABLE IF NOT EXISTS workspace_prefs (
      scope TEXT NOT NULL,
      item_key TEXT NOT NULL,
      value TEXT NOT NULL,
      sort_order INTEGER NOT NULL DEFAULT 0,
      PRIMARY KEY (scope, item_key)
    )
    """.trimIndent(),
    "CREATE INDEX IF NOT EXISTS idx_workspace_prefs_scope ON workspace_prefs(scope, sort_order)",
    """
    CREATE TABLE IF NOT EXISTS session_meta (
      path TEXT PRIMARY KEY NOT NULL,
      archived_at INTEGER,
      touched_at INTEGER
    )
    """.trimIndent(),
    "CREATE INDEX IF NOT EXISTS idx_session_meta_archived ON session_meta(archived_at)",
    """
    CREATE TABLE IF NOT EXISTS session_notices (
      id INTEGER PRIMARY KEY AUTOINCREMENT,
      session_path TEXT NOT NULL,
      ord INTEGER NOT NULL,
      level TEXT NOT NULL,
      message TEXT NOT NULL,
      after_ts INTEGER,
      after_run INTEGER NOT NULL,
      app_notice TEXT
    )
    """.trimIndent(),
    "CREATE INDEX IF NOT EXISTS idx_notices_path ON session_notices(session_path)",
    """
    CREATE TABLE IF NOT EXISTS session_index (
      path TEXT PRIMARY KEY NOT NULL,
      id TEXT NOT NULL,
      cwd TEXT NOT NULL,
      cwd_exists INTEGER NOT NULL,
      name TEXT,
      created INTEGER NOT NULL,
      modified INTEGER NOT NULL,
      message_count INTEGER NOT NULL,
      first_message TEXT NOT NULL,
      subagents INTEGER NOT NULL,
      remote_host_id TEXT,
      remote_path TEXT
    )
    """.trimIndent(),
    "CREATE INDEX IF NOT EXISTS idx_session_index_modified ON session_index(modified DESC)",
    """
    CREATE TABLE IF NOT EXISTS message_cache (
      session_path TEXT PRIMARY KEY NOT NULL,
      synced_modified INTEGER NOT NULL,
      messages_json TEXT NOT NULL,
      updated_at INTEGER NOT NULL
    )
    """.trimIndent(),
    """
    CREATE VIRTUAL TABLE IF NOT EXISTS message_fts USING fts4(
      session_path,
      body,
      notindexed=session_path,
      tokenize=unicode61
    )
    """.trimIndent(),
    """
    CREATE TABLE IF NOT EXISTS chat_drafts (
      session_path TEXT PRIMARY KEY NOT NULL,
      draft_text TEXT NOT NULL,
      steer_drafts_json TEXT NOT NULL,
      scroll_follow INTEGER NOT NULL,
      scroll_top_key TEXT,
      scroll_top_offset INTEGER NOT NULL,
      expanded_keys_json TEXT NOT NULL,
      updated_at INTEGER NOT NULL
    )
    """.trimIndent(),
    """
    CREATE TABLE IF NOT EXISTS chat_inbox (
      session_path TEXT PRIMARY KEY NOT NULL,
      cwd TEXT NOT NULL,
      reason TEXT NOT NULL,
      title TEXT,
      detail TEXT,
      at INTEGER NOT NULL,
      asker TEXT
    )
    """.trimIndent(),
    "CREATE INDEX IF NOT EXISTS idx_chat_inbox_at ON chat_inbox(at DESC)",
  )
}
