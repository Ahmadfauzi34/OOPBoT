package io.jeph.oopbot.data

import android.util.Log
import io.jeph.oopbot.agent.AppNotice
import io.jeph.oopbot.agent.NoticeLevel
import io.jeph.oopbot.agent.SavedNotice
import java.io.File
import java.io.IOException
import java.security.MessageDigest
import java.util.Locale
import java.util.concurrent.Executor
import java.util.concurrent.Executors
import org.json.JSONArray
import org.json.JSONException
import org.json.JSONObject

/**
 * The extension notices of each session, by session file path. Stored in SQLite `session_notices` when [db] is
 * provided (lazily migrating any legacy SHA-256 `.json` files under [dir] on first read or overwrite), or in `.json`
 * files under [dir] when constructed standalone. [load] reads a session once and serves the in-memory cache thereafter;
 * [put] and [delete] update the cache immediately and persist asynchronously in order through [writer].
 */
internal class NoticeStore(
  private val db: OopbotDatabase? = null,
  private val dir: File? = null,
  private val writer: Executor = db?.writer ?: Executors.newSingleThreadExecutor(),
) {
  constructor(dir: File, writer: Executor = Executors.newSingleThreadExecutor()) : this(
    db = null,
    dir = dir,
    writer = writer,
  )

  private val cache = HashMap<String, List<SavedNotice>>()

  /** Reads [sessionPath]'s notices into the cache, unless it has them already. Blocking: call off the main thread. */
  fun load(sessionPath: String): List<SavedNotice> {
    synchronized(cache) { cache[sessionPath] }?.let { return it }
    val loaded = loadFromStorage(sessionPath)
    return synchronized(cache) { cache.getOrPut(sessionPath) { loaded } }
  }

  /** Saves [notices] as [sessionPath]'s; an empty list deletes its persisted rows/file. */
  fun put(sessionPath: String, notices: List<SavedNotice>) {
    synchronized(cache) {
      if (cache[sessionPath] == notices) return
      cache[sessionPath] = notices
      // Queued under the lock, so writes land in the order the cache changed.
      writer.execute { writePersisted(sessionPath, notices) }
    }
  }

  /** Forgets the notices of the deleted sessions [sessionPaths]. */
  fun delete(sessionPaths: Collection<String>) {
    if (sessionPaths.isEmpty()) return
    val unique = sessionPaths.distinct()
    synchronized(cache) {
      for (path in unique) cache[path] = emptyList()
      writer.execute { deletePersisted(unique) }
    }
  }

  private fun loadFromStorage(sessionPath: String): List<SavedNotice> =
    if (db != null) loadFromDb(sessionPath) else fileOf(sessionPath)?.let(::readFile).orEmpty()

  private fun loadFromDb(sessionPath: String): List<SavedNotice> {
    val database = db ?: return emptyList()
    val stored = database.notices.load(sessionPath)
    if (stored.isNotEmpty()) return stored
    val legacyFile = fileOf(sessionPath)?.takeIf(File::exists) ?: return emptyList()
    val migrated = readFile(legacyFile)
    if (migrated.isNotEmpty()) database.notices.replace(sessionPath, migrated)
    deleteLegacyFile(legacyFile)
    return migrated
  }

  private fun writePersisted(sessionPath: String, notices: List<SavedNotice>) {
    if (db != null) {
      db.notices.replace(sessionPath, notices)
      fileOf(sessionPath)?.let(::deleteLegacyFile)
    } else {
      fileOf(sessionPath)?.let { writeFile(it, notices) }
    }
  }

  private fun deletePersisted(sessionPaths: List<String>) {
    if (db != null) {
      db.notices.deleteBatch(sessionPaths)
      for (path in sessionPaths) fileOf(path)?.let(::deleteLegacyFile)
    } else {
      for (path in sessionPaths) fileOf(path)?.delete()
    }
  }

  private fun fileOf(sessionPath: String): File? {
    val root = dir ?: return null
    val digest = MessageDigest.getInstance("SHA-256").digest(sessionPath.toByteArray(Charsets.UTF_8))
    return File(root, digest.joinToString("", postfix = ".json") { "%02x".format(Locale.ROOT, it) })
  }

  private fun deleteLegacyFile(file: File) {
    if (file.exists()) file.delete()
    val root = dir ?: return
    if (root.exists() && root.list().isNullOrEmpty()) root.delete()
  }

  private fun writeFile(file: File, notices: List<SavedNotice>) {
    val root = dir ?: return
    if (notices.isEmpty()) {
      file.delete()
      return
    }
    try {
      root.mkdirs()
      // Renamed over the old file, so a crash mid-write leaves the previous notices rather than half a file.
      val partial = File(root, "${file.name}.partial")
      partial.writeText(notices.toJson().toString(), Charsets.UTF_8)
      if (!partial.renameTo(file)) throw IOException("Could not replace $file")
    } catch (error: IOException) {
      Log.w(TAG, "Saving notices to $file failed", error)
    }
  }

  private fun readFile(file: File): List<SavedNotice> {
    if (!file.exists()) return emptyList()
    return try {
      val array = JSONArray(file.readText(Charsets.UTF_8))
      List(array.length()) { savedNotice(array.getJSONObject(it)) }
    } catch (error: IOException) {
      Log.w(TAG, "Reading notices from $file failed", error)
      emptyList()
    } catch (error: JSONException) {
      Log.w(TAG, "Notices in $file are malformed", error)
      emptyList()
    } catch (error: IllegalArgumentException) {
      Log.w(TAG, "Notices in $file have an unknown level or kind", error)
      emptyList()
    }
  }

  private companion object {
    const val TAG = "OOPBoT"
  }
}

private fun List<SavedNotice>.toJson(): JSONArray = JSONArray().also { array ->
  for (notice in this) {
    array.put(
      JSONObject()
        .put("level", notice.level.name)
        .put("message", notice.message)
        .put("after", notice.after ?: JSONObject.NULL)
        .put("afterRun", notice.afterRun)
        .put("appNotice", notice.appNotice?.name ?: JSONObject.NULL),
    )
  }
}

private fun savedNotice(json: JSONObject) = SavedNotice(
  level = NoticeLevel.valueOf(json.getString("level")),
  message = json.getString("message"),
  after = if (json.isNull("after")) null else json.getLong("after"),
  afterRun = json.getBoolean("afterRun"),
  appNotice = if (json.isNull("appNotice")) null else AppNotice.valueOf(json.getString("appNotice")),
)
