package io.jeph.oopbot.data

/** Unchecked SQLite error shared between [AndroidSqlDb] and JVM JDBC tests so callers avoid broad Exception catches. */
internal class OopbotSqlException(message: String?, cause: Throwable? = null) : RuntimeException(message, cause)

/** Row cursor over a SQLite query result with 0-indexed column accessors. */
internal interface SqlCursor : AutoCloseable {
  fun next(): Boolean

  fun string(index: Int): String

  fun stringOrNull(index: Int): String?

  fun long(index: Int): Long

  fun longOrNull(index: Int): Long?

  fun int(index: Int): Int
}

/**
 * Minimal SQLite abstraction implemented by [AndroidSqlDb] on device and JDBC SQLite in JVM tests so production DDL,
 * WAL pragmas, migrations, and FTS queries run against real SQLite everywhere.
 */
internal interface SqlDb : AutoCloseable {
  fun exec(sql: String, args: Array<Any?> = emptyArray())

  fun <T> query(sql: String, args: Array<Any?> = emptyArray(), mapper: (SqlCursor) -> T): List<T>

  fun <T> queryOne(sql: String, args: Array<Any?> = emptyArray(), mapper: (SqlCursor) -> T): T?

  fun <T> transaction(block: SqlDb.() -> T): T
}

/** Splits [items] under SQLite's 999 bind-variable limit (`SQLITE_MAX_VARIABLE_NUMBER`) and runs [block] per chunk. */
internal inline fun <T> forEachSqlChunk(items: Collection<T>, chunkSize: Int = MAX_SQL_VARS, block: (List<T>) -> Unit) {
  if (items.isEmpty()) return
  items.chunked(chunkSize).forEach(block)
}

internal fun sqlPlaceholders(count: Int): String = List(count) { "?" }.joinToString(", ")

internal const val MAX_SQL_VARS = 400
