package io.jeph.oopbot.data

import java.io.File
import java.sql.Connection
import java.sql.DriverManager
import java.sql.PreparedStatement
import java.sql.ResultSet
import java.sql.SQLException
import java.sql.Types

/**
 * Real SQLite implementation of [SqlDb] backed by `org.xerial:sqlite-jdbc` so JVM tests execute the exact same DDL,
 * WAL pragmas, migrations, transactions, and FTS4 queries as [AndroidSqlDb] on device.
 */
internal class JdbcSqlDb(file: File? = null) : SqlDb {
  private val lock = Any()
  private val conn: Connection = DriverManager.getConnection(
    if (file == null) "jdbc:sqlite::memory:" else "jdbc:sqlite:${file.absolutePath}",
  )
  private var txDepth = 0

  init {
    exec("PRAGMA foreign_keys = ON")
    exec("PRAGMA busy_timeout = 5000")
    if (file != null) exec("PRAGMA journal_mode = WAL")
    exec("PRAGMA synchronous = NORMAL")
    exec("PRAGMA temp_store = MEMORY")
    OopbotSchema.create(this)
  }

  fun pragmaString(name: String): String = queryOne("PRAGMA $name") { it.string(0) }.orEmpty()

  override fun exec(sql: String, args: Array<Any?>) {
    synchronized(lock) {
      try {
        conn.prepareStatement(sql).use { stmt ->
          bindArgs(stmt, args)
          stmt.execute()
        }
      } catch (error: SQLException) {
        throw OopbotSqlException(error.message, error)
      }
    }
  }

  override fun <T> query(sql: String, args: Array<Any?>, mapper: (SqlCursor) -> T): List<T> = synchronized(lock) {
    try {
      conn.prepareStatement(sql).use { stmt ->
        bindArgs(stmt, args)
        stmt.executeQuery().use { rs ->
          val cursor = JdbcCursor(rs)
          buildList { while (rs.next()) add(mapper(cursor)) }
        }
      }
    } catch (error: SQLException) {
      throw OopbotSqlException(error.message, error)
    }
  }

  override fun <T> queryOne(sql: String, args: Array<Any?>, mapper: (SqlCursor) -> T): T? = synchronized(lock) {
    try {
      conn.prepareStatement(sql).use { stmt ->
        bindArgs(stmt, args)
        stmt.executeQuery().use { rs ->
          if (rs.next()) mapper(JdbcCursor(rs)) else null
        }
      }
    } catch (error: SQLException) {
      throw OopbotSqlException(error.message, error)
    }
  }

  override fun <T> transaction(block: SqlDb.() -> T): T = synchronized(lock) {
    if (txDepth == 0) {
      runRootTransaction(block)
    } else {
      runNestedTransaction(block)
    }
  }

  private fun <T> runRootTransaction(block: SqlDb.() -> T): T {
    txDepth++
    var committed = false
    return try {
      conn.autoCommit = false
      val result = block()
      conn.commit()
      committed = true
      result
    } catch (error: SQLException) {
      throw OopbotSqlException(error.message, error)
    } finally {
      if (!committed) runCatching { conn.rollback() }
      txDepth--
      runCatching { conn.autoCommit = true }
    }
  }

  private fun <T> runNestedTransaction(block: SqlDb.() -> T): T {
    txDepth++
    val savepoint = conn.setSavepoint()
    var committed = false
    return try {
      val result = block()
      conn.releaseSavepoint(savepoint)
      committed = true
      result
    } catch (error: SQLException) {
      throw OopbotSqlException(error.message, error)
    } finally {
      if (!committed) runCatching { conn.rollback(savepoint) }
      txDepth--
    }
  }

  override fun close() {
    synchronized(lock) {
      conn.close()
    }
  }

  private fun bindArgs(stmt: PreparedStatement, args: Array<Any?>) {
    args.forEachIndexed { index, arg ->
      val pos = index + 1
      when (arg) {
        null -> stmt.setNull(pos, Types.NULL)
        is Int -> stmt.setInt(pos, arg)
        is Long -> stmt.setLong(pos, arg)
        is Boolean -> stmt.setInt(pos, if (arg) 1 else 0)
        else -> stmt.setString(pos, arg.toString())
      }
    }
  }
}

private class JdbcCursor(private val rs: ResultSet) : SqlCursor {
  override fun next(): Boolean = rs.next()

  override fun string(index: Int): String = rs.getString(index + 1)

  override fun stringOrNull(index: Int): String? = rs.getString(index + 1)

  override fun long(index: Int): Long = rs.getLong(index + 1)

  override fun longOrNull(index: Int): Long? {
    val value = rs.getLong(index + 1)
    return if (rs.wasNull()) null else value
  }

  override fun int(index: Int): Int = rs.getInt(index + 1)

  override fun close() = rs.close()
}
