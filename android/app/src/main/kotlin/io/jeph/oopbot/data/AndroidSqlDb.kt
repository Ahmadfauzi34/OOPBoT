package io.jeph.oopbot.data

import android.content.Context
import android.database.Cursor
import android.database.SQLException
import android.database.sqlite.SQLiteDatabase
import android.database.sqlite.SQLiteOpenHelper

/**
 * Framework [SQLiteOpenHelper] backing [OopbotDatabase], configured with Write-Ahead Logging (`WAL`), `NORMAL`
 * synchronous commits, in-memory temp tables, and foreign keys enabled, avoiding any bundled native SQLite bloat.
 */
internal class AndroidSqlDb(context: Context, name: String = OopbotSchema.DB_NAME) :
  SQLiteOpenHelper(context.applicationContext, name, null, OopbotSchema.VERSION),
  SqlDb {
  init {
    setWriteAheadLoggingEnabled(true)
  }

  override fun onConfigure(db: SQLiteDatabase) {
    super.onConfigure(db)
    db.setForeignKeyConstraintsEnabled(true)
    db.rawQuery("PRAGMA busy_timeout = 5000", null).use { it.moveToFirst() }
    db.rawQuery("PRAGMA synchronous = NORMAL", null).use { it.moveToFirst() }
    db.rawQuery("PRAGMA temp_store = MEMORY", null).use { it.moveToFirst() }
  }

  override fun onCreate(db: SQLiteDatabase) {
    OopbotSchema.create(AndroidConnection(db))
  }

  override fun onUpgrade(db: SQLiteDatabase, oldVersion: Int, newVersion: Int) {
    OopbotSchema.upgrade(AndroidConnection(db), oldVersion, newVersion)
  }

  override fun exec(sql: String, args: Array<Any?>) {
    AndroidConnection(writableDatabase).exec(sql, args)
  }

  override fun <T> query(sql: String, args: Array<Any?>, mapper: (SqlCursor) -> T): List<T> =
    AndroidConnection(readableDatabase).query(sql, args, mapper)

  override fun <T> queryOne(sql: String, args: Array<Any?>, mapper: (SqlCursor) -> T): T? =
    AndroidConnection(readableDatabase).queryOne(sql, args, mapper)

  override fun <T> transaction(block: SqlDb.() -> T): T = AndroidConnection(writableDatabase).transaction(block)
}

private class AndroidConnection(private val db: SQLiteDatabase) : SqlDb {
  override fun exec(sql: String, args: Array<Any?>) {
    try {
      if (args.isEmpty()) db.execSQL(sql) else db.execSQL(sql, args)
    } catch (error: SQLException) {
      throw OopbotSqlException(error.message, error)
    }
  }

  override fun <T> query(sql: String, args: Array<Any?>, mapper: (SqlCursor) -> T): List<T> = try {
    db.rawQuery(sql, bindSelection(args)).use { cursor ->
      val wrapper = AndroidCursor(cursor)
      buildList { while (cursor.moveToNext()) add(mapper(wrapper)) }
    }
  } catch (error: SQLException) {
    throw OopbotSqlException(error.message, error)
  }

  override fun <T> queryOne(sql: String, args: Array<Any?>, mapper: (SqlCursor) -> T): T? = try {
    db.rawQuery(sql, bindSelection(args)).use { cursor ->
      if (cursor.moveToFirst()) mapper(AndroidCursor(cursor)) else null
    }
  } catch (error: SQLException) {
    throw OopbotSqlException(error.message, error)
  }

  private fun bindSelection(args: Array<Any?>): Array<String>? {
    if (args.isEmpty()) return null
    return Array(args.size) { index ->
      requireNotNull(args[index]) { "Null arguments are not supported in SQLite rawQuery" }.toString()
    }
  }

  override fun <T> transaction(block: SqlDb.() -> T): T {
    db.beginTransactionNonExclusive()
    return try {
      val result = block()
      db.setTransactionSuccessful()
      result
    } catch (error: SQLException) {
      throw OopbotSqlException(error.message, error)
    } finally {
      db.endTransaction()
    }
  }

  override fun close() = Unit
}

private class AndroidCursor(private val cursor: Cursor) : SqlCursor {
  override fun next(): Boolean = cursor.moveToNext()

  override fun string(index: Int): String = cursor.getString(index)

  override fun stringOrNull(index: Int): String? = if (cursor.isNull(index)) null else cursor.getString(index)

  override fun long(index: Int): Long = cursor.getLong(index)

  override fun longOrNull(index: Int): Long? = if (cursor.isNull(index)) null else cursor.getLong(index)

  override fun int(index: Int): Int = cursor.getInt(index)

  override fun close() = cursor.close()
}
