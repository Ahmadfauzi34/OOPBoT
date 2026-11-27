package io.jeph.oopbot.data

import io.jeph.oopbot.host.FileEntry
import io.jeph.oopbot.host.GitChange
import io.jeph.oopbot.host.GitChanges
import io.jeph.oopbot.host.GitCommit
import io.jeph.oopbot.host.GitCommits
import io.jeph.oopbot.host.GitDetection
import io.jeph.oopbot.host.GitDiff
import io.jeph.oopbot.host.GitDiffMode
import io.jeph.oopbot.host.GitFile
import io.jeph.oopbot.host.GitFileKind
import io.jeph.oopbot.host.GitHunk
import java.io.File
import java.io.IOException
import java.security.MessageDigest
import org.json.JSONArray
import org.json.JSONException
import org.json.JSONObject

/** Cached snapshot of one workspace's git detection, diff, and commits for instant re-entry. */
internal data class GitWorkspaceSnapshot(
  val detection: GitDetection?,
  val defaultMode: GitDiffMode?,
  val changesMode: GitDiffMode?,
  val ignoreWhitespace: Boolean,
  val changes: GitChanges?,
  val commits: GitCommits?,
)

/** Cached snapshot of one workspace's listed directories and expanded folders. */
internal data class FileTreeSnapshot(val directories: Map<String, List<FileEntry>>, val expanded: Set<String>)

/**
 * Bounded, atomic disk cache for Explorer git snapshots and file listings under app cache storage.
 * All file I/O is blocking and must be called off the main thread (e.g. `Dispatchers.IO`). Corrupt or
 * unreadable cache files are deleted silently and treated as misses.
 */
internal class ExplorerCache(
  private val dir: File,
  private val maxEntriesPerKind: Int = DEFAULT_MAX_ENTRIES,
  private val maxBytesPerFile: Int = DEFAULT_MAX_BYTES,
) {
  private val lock = Any()

  fun loadGit(root: String): GitWorkspaceSnapshot? = readJson(GIT_PREFIX, root)?.let(ExplorerCodec::decodeGit)

  fun saveGit(root: String, snapshot: GitWorkspaceSnapshot) {
    writeJson(GIT_PREFIX, root, ExplorerCodec.encodeGit(snapshot))
  }

  fun loadFiles(root: String): FileTreeSnapshot? = readJson(FILES_PREFIX, root)?.let(ExplorerCodec::decodeFiles)

  fun saveFiles(root: String, snapshot: FileTreeSnapshot) {
    writeJson(FILES_PREFIX, root, ExplorerCodec.encodeFiles(snapshot))
  }

  private fun readJson(prefix: String, root: String): JSONObject? {
    val file = cacheFile(prefix, root)
    synchronized(lock) {
      if (!file.isFile) return null
      if (file.length() > maxBytesPerFile) {
        file.delete()
        return null
      }
      return try {
        val text = file.readText(Charsets.UTF_8)
        file.setLastModified(System.currentTimeMillis())
        JSONObject(text)
      } catch (_: IOException) {
        null
      } catch (_: JSONException) {
        file.delete()
        null
      }
    }
  }

  private fun writeJson(prefix: String, root: String, payload: JSONObject) {
    val bytes = payload.toString().toByteArray(Charsets.UTF_8)
    val file = cacheFile(prefix, root)
    if (bytes.size > maxBytesPerFile) {
      synchronized(lock) { file.delete() }
      return
    }
    synchronized(lock) {
      if (!dir.exists() && !dir.mkdirs()) return
      val tmp = File(dir, "${file.name}.tmp")
      try {
        tmp.writeBytes(bytes)
        if (!tmp.renameTo(file)) {
          file.writeBytes(bytes)
          tmp.delete()
        }
        evictOldestLocked(prefix)
      } catch (_: IOException) {
        tmp.delete()
      }
    }
  }

  private fun evictOldestLocked(prefix: String) {
    val files = dir.listFiles { entry ->
      entry.isFile && entry.name.startsWith(prefix) && entry.name.endsWith(JSON_SUFFIX)
    } ?: return
    if (files.size <= maxEntriesPerKind) return
    files.sortBy { it.lastModified() }
    val excess = files.size - maxEntriesPerKind
    for (i in 0 until excess) {
      files[i].delete()
    }
  }

  private fun cacheFile(prefix: String, root: String): File =
    File(dir, "$prefix${sha256Hex(folderKey(root))}$JSON_SUFFIX")

  companion object {
    private const val GIT_PREFIX = "git_"
    private const val FILES_PREFIX = "files_"
    private const val JSON_SUFFIX = ".json"
    const val DEFAULT_MAX_ENTRIES = 12
    const val DEFAULT_MAX_BYTES = 2 * 1024 * 1024
  }
}

private fun sha256Hex(text: String): String {
  val digest = MessageDigest.getInstance("SHA-256").digest(text.toByteArray(Charsets.UTF_8))
  val hex = CharArray(digest.size * 2)
  for (i in digest.indices) {
    val v = digest[i].toInt() and 0xFF
    hex[i * 2] = HEX_CHARS[v ushr 4]
    hex[i * 2 + 1] = HEX_CHARS[v and 0x0F]
  }
  return String(hex)
}

private const val HEX_CHARS = "0123456789abcdef"

private object ExplorerCodec {
  private const val VERSION = 1

  fun encodeGit(snapshot: GitWorkspaceSnapshot): JSONObject = JSONObject().apply {
    put("v", VERSION)
    snapshot.detection?.let {
      put("detection", JSONObject().put("repo", it.repo).put("git", it.git))
    }
    snapshot.defaultMode?.let { put("defaultMode", it.name) }
    snapshot.changesMode?.let { put("changesMode", it.name) }
    put("ignoreWhitespace", snapshot.ignoreWhitespace)
    snapshot.changes?.let { put("changes", encodeChanges(it)) }
    snapshot.commits?.let { put("commits", encodeCommits(it)) }
  }

  fun decodeGit(json: JSONObject): GitWorkspaceSnapshot? {
    if (json.optInt("v", -1) != VERSION) return null
    val detection = json.optJSONObject("detection")?.let {
      GitDetection(repo = it.optBoolean("repo"), git = it.optBoolean("git"))
    }
    val defaultMode = json.optString("defaultMode").takeIf { it.isNotEmpty() }?.let(::parseDiffMode)
    val changesMode = json.optString("changesMode").takeIf { it.isNotEmpty() }?.let(::parseDiffMode)
    val ignoreWhitespace = json.optBoolean("ignoreWhitespace", false)
    val changes = json.optJSONObject("changes")?.let(::decodeChanges)
    val commits = json.optJSONObject("commits")?.let(::decodeCommits)
    return GitWorkspaceSnapshot(detection, defaultMode, changesMode, ignoreWhitespace, changes, commits)
  }

  fun encodeFiles(snapshot: FileTreeSnapshot): JSONObject = JSONObject().apply {
    put("v", VERSION)
    val dirs = JSONObject()
    for ((path, entries) in snapshot.directories) {
      val list = JSONArray()
      for (entry in entries) {
        list.put(
          JSONObject()
            .put("name", entry.name)
            .put("path", entry.path)
            .put("isDirectory", entry.isDirectory)
            .put("size", entry.size)
            .put("modified", entry.modified),
        )
      }
      dirs.put(path, list)
    }
    put("directories", dirs)
    val expanded = JSONArray()
    for (path in snapshot.expanded) expanded.put(path)
    put("expanded", expanded)
  }

  fun decodeFiles(json: JSONObject): FileTreeSnapshot? {
    if (json.optInt("v", -1) != VERSION) return null
    val dirsObj = json.optJSONObject("directories") ?: return null
    val directories = LinkedHashMap<String, List<FileEntry>>()
    val keys = dirsObj.keys()
    while (keys.hasNext()) {
      val dirPath = keys.next()
      val arr = dirsObj.optJSONArray(dirPath) ?: continue
      val entries = ArrayList<FileEntry>(arr.length())
      for (i in 0 until arr.length()) {
        val item = arr.optJSONObject(i) ?: continue
        entries += FileEntry(
          name = item.optString("name"),
          path = item.optString("path"),
          isDirectory = item.optBoolean("isDirectory"),
          size = item.optLong("size"),
          modified = item.optLong("modified"),
        )
      }
      directories[dirPath] = entries
    }
    val expanded = LinkedHashSet<String>()
    val expandedArr = json.optJSONArray("expanded")
    if (expandedArr != null) {
      for (i in 0 until expandedArr.length()) {
        expandedArr.optString(i).takeIf { it.isNotEmpty() }?.let { expanded += it }
      }
    }
    return FileTreeSnapshot(directories, expanded)
  }

  private fun encodeChanges(changes: GitChanges): JSONObject = JSONObject().apply {
    put("fingerprint", changes.fingerprint)
    changes.branch?.let { put("branch", it) }
    changes.baseRef?.let { put("baseRef", it) }
    put("tooLarge", changes.diff.tooLarge)
    val files = JSONArray()
    for (file in changes.diff.files) {
      val hunks = JSONArray()
      for (hunk in file.hunks) {
        val lines = JSONArray()
        for (line in hunk.lines) lines.put(line)
        hunks.put(
          JSONObject()
            .put("header", hunk.header)
            .put("oldStart", hunk.oldStart)
            .put("newStart", hunk.newStart)
            .put("lines", lines),
        )
      }
      files.put(
        JSONObject()
          .put("path", file.path)
          .put("change", file.change.name)
          .put("additions", file.additions)
          .put("deletions", file.deletions)
          .put("kind", file.kind.name)
          .put("hunks", hunks),
      )
    }
    put("files", files)
  }

  private fun decodeChanges(json: JSONObject): GitChanges {
    val filesArr = json.optJSONArray("files")
    val files = ArrayList<GitFile>(filesArr?.length() ?: 0)
    if (filesArr != null) {
      for (i in 0 until filesArr.length()) {
        val item = filesArr.optJSONObject(i) ?: continue
        files += decodeFile(item)
      }
    }
    return GitChanges(
      fingerprint = json.optString("fingerprint"),
      branch = json.optString("branch").takeIf { it.isNotEmpty() },
      baseRef = json.optString("baseRef").takeIf { it.isNotEmpty() },
      diff = GitDiff(files, tooLarge = json.optBoolean("tooLarge")),
    )
  }

  private fun decodeFile(json: JSONObject): GitFile {
    val hunksArr = json.optJSONArray("hunks")
    val hunks = ArrayList<GitHunk>(hunksArr?.length() ?: 0)
    if (hunksArr != null) {
      for (i in 0 until hunksArr.length()) {
        val hunkObj = hunksArr.optJSONObject(i) ?: continue
        val linesArr = hunkObj.optJSONArray("lines")
        val lines = ArrayList<String>(linesArr?.length() ?: 0)
        if (linesArr != null) {
          for (j in 0 until linesArr.length()) lines += linesArr.optString(j)
        }
        hunks += GitHunk(
          header = hunkObj.optString("header"),
          oldStart = hunkObj.optInt("oldStart"),
          newStart = hunkObj.optInt("newStart"),
          lines = lines,
        )
      }
    }
    return GitFile(
      path = json.optString("path"),
      change = parseChange(json.optString("change")),
      additions = json.optInt("additions"),
      deletions = json.optInt("deletions"),
      kind = parseFileKind(json.optString("kind")),
      hunks = hunks,
    )
  }

  private fun encodeCommits(commits: GitCommits): JSONObject = JSONObject().apply {
    put("fingerprint", commits.fingerprint)
    commits.baseRef?.let { put("baseRef", it) }
    put("total", commits.total)
    val list = JSONArray()
    for (commit in commits.commits) {
      list.put(
        JSONObject()
          .put("sha", commit.sha)
          .put("subject", commit.subject)
          .put("authorTime", commit.authorTime),
      )
    }
    put("commits", list)
  }

  private fun decodeCommits(json: JSONObject): GitCommits {
    val arr = json.optJSONArray("commits")
    val commits = ArrayList<GitCommit>(arr?.length() ?: 0)
    if (arr != null) {
      for (i in 0 until arr.length()) {
        val item = arr.optJSONObject(i) ?: continue
        commits += GitCommit(
          sha = item.optString("sha"),
          subject = item.optString("subject"),
          authorTime = item.optLong("authorTime"),
        )
      }
    }
    return GitCommits(
      fingerprint = json.optString("fingerprint"),
      baseRef = json.optString("baseRef").takeIf { it.isNotEmpty() },
      commits = commits,
      total = json.optInt("total", commits.size),
    )
  }

  private fun parseDiffMode(name: String): GitDiffMode? = GitDiffMode.entries.firstOrNull { it.name == name }

  private fun parseChange(name: String): GitChange =
    GitChange.entries.firstOrNull { it.name == name } ?: GitChange.Modified

  private fun parseFileKind(name: String): GitFileKind =
    GitFileKind.entries.firstOrNull { it.name == name } ?: GitFileKind.Text
}
