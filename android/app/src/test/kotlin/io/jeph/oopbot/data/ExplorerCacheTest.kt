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
import org.junit.Assert.assertEquals
import org.junit.Assert.assertNotNull
import org.junit.Assert.assertNull
import org.junit.Assert.assertTrue
import org.junit.Rule
import org.junit.Test
import org.junit.rules.TemporaryFolder

class ExplorerCacheTest {
  @get:Rule
  val temp = TemporaryFolder()

  @Test
  fun `git snapshot round-trips detection, changes, and commits cleanly`() {
    val cache = ExplorerCache(temp.newFolder("explorer"))
    val snapshot = GitWorkspaceSnapshot(
      detection = GitDetection(repo = true, git = true),
      defaultMode = GitDiffMode.Uncommitted,
      changesMode = GitDiffMode.Uncommitted,
      ignoreWhitespace = true,
      changes = GitChanges(
        fingerprint = "fp-changes-1",
        branch = "main",
        baseRef = "origin/main",
        diff = GitDiff(
          files = listOf(
            GitFile(
              path = "src/App.kt",
              change = GitChange.Modified,
              additions = 2,
              deletions = 1,
              kind = GitFileKind.Text,
              hunks = listOf(GitHunk("@@ -1,2 +1,3 @@", 1, 1, listOf(" a", "-b", "+B", "+c"))),
            ),
          ),
          tooLarge = false,
        ),
      ),
      commits = GitCommits(
        fingerprint = "fp-commits-1",
        baseRef = "origin/main",
        commits = listOf(GitCommit("abcdef1234567", "feat: cache", 1_700_000_000L)),
        total = 3,
      ),
    )

    cache.saveGit("/work/repo", snapshot)
    val loaded = cache.loadGit("/work/repo/")

    assertNotNull(loaded)
    assertEquals(GitDetection(repo = true, git = true), loaded?.detection)
    assertEquals(GitDiffMode.Uncommitted, loaded?.defaultMode)
    assertEquals(true, loaded?.ignoreWhitespace)
    assertEquals("fp-changes-1", loaded?.changes?.fingerprint)
    assertEquals("src/App.kt", loaded?.changes?.diff?.files?.single()?.path)
    assertEquals(listOf(" a", "-b", "+B", "+c"), loaded?.changes?.diff?.files?.single()?.hunks?.single()?.lines)
    assertEquals("fp-commits-1", loaded?.commits?.fingerprint)
    assertEquals(3, loaded?.commits?.total)
    assertEquals("abcdef1234567", loaded?.commits?.commits?.single()?.sha)
  }

  @Test
  fun `file tree snapshot round-trips directories and expanded folders`() {
    val cache = ExplorerCache(temp.newFolder("explorer"))
    val snapshot = FileTreeSnapshot(
      directories = mapOf(
        "." to listOf(
          FileEntry("src", "src", isDirectory = true, size = 0L, modified = 100L),
          FileEntry("README.md", "README.md", isDirectory = false, size = 42L, modified = 200L),
        ),
      ),
      expanded = setOf("src"),
    )

    cache.saveFiles("/work/repo", snapshot)
    val loaded = cache.loadFiles("/work/repo")

    assertEquals(snapshot, loaded)
  }

  @Test
  fun `corrupt cache file returns null and is deleted`() {
    val dir = temp.newFolder("explorer")
    val cache = ExplorerCache(dir)
    cache.saveFiles("/work/repo", FileTreeSnapshot(mapOf("." to emptyList()), emptySet()))
    val file = dir.listFiles()?.single() ?: error("expected cache file")
    file.writeText("{not valid json")

    assertNull(cache.loadFiles("/work/repo"))
    assertTrue(!file.exists())
  }

  @Test
  fun `oversized payloads are skipped and LRU evicts oldest entries`() {
    val dir = temp.newFolder("explorer")
    val cache = ExplorerCache(dir, maxEntriesPerKind = 2, maxBytesPerFile = 256)
    val tiny = FileTreeSnapshot(mapOf("." to emptyList()), emptySet())

    cache.saveFiles("/w1", tiny)
    File(dir, dir.listFiles()!!.single().name).setLastModified(1_000L)
    cache.saveFiles("/w2", tiny)
    cache.saveFiles("/w3", tiny)

    assertNull(cache.loadFiles("/w1"))
    assertNotNull(cache.loadFiles("/w2"))
    assertNotNull(cache.loadFiles("/w3"))

    val huge = FileTreeSnapshot(
      directories = mapOf("." to listOf(FileEntry("x".repeat(400), "x".repeat(400), false, 1L, 1L))),
      expanded = emptySet(),
    )
    cache.saveFiles("/w2", huge)
    assertNull(cache.loadFiles("/w2"))
  }
}
