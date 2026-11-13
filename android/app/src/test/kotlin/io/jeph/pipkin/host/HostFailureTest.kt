package io.jeph.pipkin.host

import io.jeph.pipkin.termux.assertRunsInAnyShell
import java.nio.file.Files
import java.nio.file.Path
import org.junit.Assert.assertEquals
import org.junit.Assert.assertNotEquals
import org.junit.Assert.assertTrue
import org.junit.Assume.assumeTrue
import org.junit.Test

class HostFailureTest {
  @Test
  fun nodeTooOldCommandPinsExpectedVersionAndRunsInAnyShell() {
    val failure = HostFailure.NodeTooOld("0.5.5")
    val cmd = requireNotNull(failure.command)
    assertRunsInAnyShell(cmd)
    assertTrue(cmd.contains("/releases/download/v0.5.5/oopbot-aarch64-linux-android"))
    assertTrue(cmd.contains("/releases/latest/download/oopbot-aarch64-linux-android"))
    assertTrue(cmd.contains("\"oopbot 0.5.5\""))
    assertTrue(cmd.contains("\$PREFIX/bin/oopbot"))
    assertTrue(failure.needsSetup)
    assertTrue(failure.fixedInTermux)
  }

  @Test
  fun upgradeCommandSanitizesBlankOrUnsafeExpectedVersion() {
    val cmd = HostFailure.upgradeCommand(" 0.5.5; rm -rf / ")
    assertRunsInAnyShell(cmd)
    assertTrue(cmd.contains("0.5.5rm-rf"))
    assertTrue(!cmd.contains(";"))
  }

  @Test
  fun upgradeCommandReplacesBinaryAndSyncsSecondaryCopyOnlyWhenVersionMatches() {
    assumeTrue(Files.isExecutable(Path.of("/bin/bash")))
    // 1. Stale 0.5.4 response from server must fail closed and leave existing 0.5.4 intact.
    val rejected = runUpgradeSandbox(tagVersion = "0.5.4", latestVersion = "0.5.4")
    assertNotEquals(0, rejected.exitCode)
    assertEquals("oopbot 0.5.4", rejected.primaryVersion)
    assertEquals("oopbot 0.5.4", rejected.secondaryVersion)
    assertEquals("0.5.4", rejected.versionFileText)

    // 2. Matching 0.5.5 pinned tag succeeds and syncs primary, secondary copy, and oopbot.version.
    val upgraded = runUpgradeSandbox(tagVersion = "0.5.5", latestVersion = "0.5.4")
    assertEquals("Expected upgrade to succeed (stderr=${upgraded.stderr})", 0, upgraded.exitCode)
    assertEquals("oopbot 0.5.5", upgraded.primaryVersion)
    assertEquals("oopbot 0.5.5", upgraded.secondaryVersion)
    assertEquals("0.5.5", upgraded.versionFileText)

    // 3. Pinned 404 falls back to latest when latest serves valid 0.5.5 binary.
    val fallback = runUpgradeSandbox(tagVersion = null, latestVersion = "0.5.5")
    assertEquals("Expected fallback to /latest/ to succeed (stderr=${fallback.stderr})", 0, fallback.exitCode)
    assertEquals("oopbot 0.5.5", fallback.primaryVersion)
    assertEquals("0.5.5", fallback.versionFileText)
  }

  private fun runUpgradeSandbox(tagVersion: String?, latestVersion: String): UpgradeSandboxResult {
    val root = Files.createTempDirectory("oopbot-upgrade-cmd-test")
    try {
      val home = Files.createDirectories(root.resolve("home"))
      val prefix = Files.createDirectories(root.resolve("prefix"))
      val prefixBin = Files.createDirectories(prefix.resolve("bin"))
      val secondaryBinDir = Files.createDirectories(home.resolve(".local/share/${TermuxBuild.NAME}/bin"))
      val sysBin = Files.createDirectories(root.resolve("sysbin"))
      for (tool in listOf("bash", "mkdir", "chmod", "mv", "cp", "rm")) {
        symlinkTool(tool, sysBin)
      }

      val primaryBin = prefixBin.resolve("oopbot")
      val secondaryBin = secondaryBinDir.resolve("oopbot")
      val versionFile = secondaryBinDir.resolve("oopbot.version")
      writeFakeBinary(primaryBin, "0.5.4")
      writeFakeBinary(secondaryBin, "0.5.4")
      Files.writeString(versionFile, "0.5.4\n")

      val curlScript = buildFakeCurl(tagVersion, latestVersion)
      val fakeCurl = sysBin.resolve("curl")
      Files.writeString(fakeCurl, curlScript)
      fakeCurl.toFile().setExecutable(true)

      val command = HostFailure.upgradeCommand("0.5.5")
      val pb = ProcessBuilder("/bin/bash", "-c", command)
      val env = pb.environment()
      env.clear()
      env["HOME"] = home.toString()
      env["PREFIX"] = prefix.toString()
      env["PATH"] = "$prefixBin:$sysBin"

      val process = pb.start()
      val stderr = process.errorStream.bufferedReader().readText()
      val exitCode = process.waitFor()

      return UpgradeSandboxResult(
        exitCode = exitCode,
        stderr = stderr,
        primaryVersion = runVersion(primaryBin),
        secondaryVersion = runVersion(secondaryBin),
        versionFileText = Files.readString(versionFile).trim(),
      )
    } finally {
      root.toFile().deleteRecursively()
    }
  }

  private fun buildFakeCurl(tagVersion: String?, latestVersion: String): String {
    val tagBranch = if (tagVersion == null) {
      "    exit 22\n"
    } else {
      "    printf '#!/bin/sh\\necho \"oopbot $tagVersion\"\\n' > \"${'$'}out\"\n"
    }
    return """
      #!/bin/sh
      out=""
      url=""
      while [ "${'$'}#" -gt 0 ]; do
        case "${'$'}1" in
          -o) out="${'$'}2"; shift 2 ;;
          http*) url="${'$'}1"; shift ;;
          *) shift ;;
        esac
      done
      case "${'$'}url" in
        */download/v0.5.5/*)
      $tagBranch
          ;;
        */latest/download/*)
          printf '#!/bin/sh\necho "oopbot $latestVersion"\n' > "${'$'}out"
          ;;
        *)
          exit 22
          ;;
      esac
      """.trimIndent() + "\n"
  }

  private fun writeFakeBinary(path: Path, version: String) {
    Files.writeString(path, "#!/bin/sh\necho \"oopbot $version\"\n")
    path.toFile().setExecutable(true)
  }

  private fun runVersion(path: Path): String {
    val proc = ProcessBuilder(path.toString(), "--version").start()
    val out = proc.inputStream.bufferedReader().readText().trim()
    proc.waitFor()
    return out
  }

  private fun symlinkTool(name: String, destDir: Path) {
    val target = listOf("/bin/$name", "/usr/bin/$name").map(Path::of).first { Files.isExecutable(it) }
    Files.createSymbolicLink(destDir.resolve(name), target)
  }

  private data class UpgradeSandboxResult(
    val exitCode: Int,
    val stderr: String,
    val primaryVersion: String,
    val secondaryVersion: String,
    val versionFileText: String,
  )
}
