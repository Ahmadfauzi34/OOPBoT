# Android Data & SQLite Persistence (`io.jeph.oopbot.data`)

## Purpose
Manages durable app state, session indexing, transcript caching, FTS full-text archive search, notice persistence, backup/config repositories, and preferences using framework SQLite (`oopbot.db`) alongside SharedPreferences and encrypted credentials.

## Ownership
- **SQLite Engine & Schema:** `SqlDb.kt`, `AndroidSqlDb.kt`, `OopbotSchema.kt`, `OopbotDatabase.kt`.
- **SQLite Domain Stores & Indexing:** `OopbotMetaStores.kt` (`PrefsDbStore`, `NoticesDbStore`), `OopbotSessionStores.kt` (`SessionsDbStore`, `DraftsDbStore`, `InboxDbStore`), `TranscriptIndexer.kt`.
- **Repositories & Settings:** `Preferences.kt`, `NoticeStore.kt`, `SessionRepository.kt` (`SessionHost`), `SessionArchive.kt`, `AutoArchive.kt`, `AccountRepository.kt`, `ConfigBackups.kt`, `ModelDefaults.kt`, `PendingReload.kt`, `PiExtensionUpdate.kt`, `RemoteHosts.kt`, `RemotePasswords.kt`, `Workspaces.kt`, `BundledExtension.kt`.

## Local Contracts
- **Framework SQLite Only (`minSdk = 29`):** `AndroidSqlDb` uses Android's zero-dependency `SQLiteOpenHelper` with `WAL` (`setWriteAheadLoggingEnabled(true)`), `PRAGMA foreign_keys = ON`, `busy_timeout = 5000`, `synchronous = NORMAL`, and `temp_store = MEMORY`.
- **FTS4 Over FTS5 & API 29 SQL Dialect:** `message_fts` uses `fts4(session_path, body, notindexed=session_path, tokenize=unicode61)` because AOSP compiles `-DSQLITE_ENABLE_FTS4` without `-DSQLITE_ENABLE_FTS5`. All SQL statements MUST stay compatible with SQLite 3.22.0 (API 29)—do NOT use `UPSERT`, `RETURNING`, `STRICT`, or FTS5 syntax.
- **SQLite Bind Limit Chunking:** Bulk `IN (...)` queries MUST chunk through `forEachSqlChunk` (`MAX_SQL_VARS = 400`) to stay under `SQLITE_MAX_VARIABLE_NUMBER = 999`.
- **Termux Source of Truth & Subagent Isolation:** Termux `.jsonl` transcripts remain canonical for live sessions. `SessionsDbStore.applySync` reconciles only `session_index` and NEVER prunes `message_cache` so cached subagent transcripts (not listed by `hostClient.sessions()`) stay intact until `purge(paths)`.
- **Main-Thread Non-Blocking Writes:** `OopbotDatabase.enqueueWrite` serializes background mutations on a single daemon `oopbot-db-writer` thread; `NoticesDbStore`, `PrefsDbStore`, `SessionsDbStore`, `DraftsDbStore`, and `InboxDbStore` must catch `SQLException` via `OopbotSqlException`.
- **One-Time Preferences Migration & Legacy Notices:** `Preferences` atomically migrates its 7 large collection/cache JSON keys into `workspace_prefs` / `session_meta` once (`prefs_v1_migrated`) and removes those keys from XML; `NoticeStore` lazily migrates legacy SHA-256 `.json` files on read or overwrite.

## Work Guidance
- Mirror every SQL schema or query change with JVM tests against `org.xerial:sqlite-jdbc` via `JdbcSqlDb` under `android/app/src/test/kotlin/io/jeph/oopbot/data/` so real SQLite compiles and executes every query.
- Keep `SessionHost` narrow and free of Android framework types so `SessionRepositoryTest` stays pure JVM.

## Verification
- `cd android && gradle :app:testDebugUnitTest --tests "io.jeph.oopbot.data.*"`
- Static checks: `cd android && gradle :app:ktlintCheck :app:detektDebug :app:detektDebugUnitTest`

## Child graph Index
- None (leaf domain serving `AppGraph`, `agent/`, and `ui/`).
