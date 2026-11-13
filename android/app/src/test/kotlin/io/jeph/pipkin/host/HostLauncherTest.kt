package io.jeph.pipkin.host

import java.nio.file.Files
import java.nio.file.Path
import org.junit.Assert.assertEquals
import org.junit.Assert.assertTrue
import org.junit.Assume.assumeTrue
import org.junit.Test

class HostLauncherTest {
  @Test
  fun bootstrapRejectsLegacyAndMalformedOopbotBinariesWithExit73() {
    assumeTrue(Files.isExecutable(Path.of("/bin/bash")))
    val staleOutputs = listOf(
      "oopbot 0.5.0",
      "oopbot 0.5.1",
      "oopbot 0.5.2",
      "oopbot 0.5.3",
      "oopbot 0.5.4",
      "oopbot 0.5.4-rc1",
      "oopbot 0.5.4 (dirty)",
      "oopbot 0.4.9",
      "oopbot 0.1.0",
      "oopbot",
      "",
      "garbage",
    )
    for (versionLine in staleOutputs) {
      val result = runBootstrap(versionLine = versionLine)
      assertEquals("Expected stale '$versionLine' to exit 73", 73, result.exitCode)
    }
  }

  @Test
  fun bootstrapLaunchesMinimumAndNewerOopbotVersions() {
    assumeTrue(Files.isExecutable(Path.of("/bin/bash")))
    val acceptedOutputs = listOf(
      "oopbot 0.5.5",
      "oopbot 0.5.5-rc1",
      "oopbot 0.5.5+local",
      "oopbot 0.5.5 (deadbeef)",
      "oopbot 0.5.9",
      "oopbot 0.5.10",
      "oopbot 0.6.0",
      "oopbot 0.10.0",
      "oopbot 1.0.0",
      "oopbot 2.1.3",
    )
    for (versionLine in acceptedOutputs) {
      val result = runBootstrap(versionLine = versionLine, mode = "rpc", port = 43210)
      assertEquals("Expected '$versionLine' to launch (stderr=${result.stderr})", 0, result.exitCode)
      assertTrue(
        "Expected fake oopbot host invocation for '$versionLine': ${result.stdout}",
        result.stdout.contains("ran:host rpc 43210:tok123"),
      )
    }
  }

  @Test
  fun bootstrapReportsMissingEnvTokenOrEngine() {
    assumeTrue(Files.isExecutable(Path.of("/bin/bash")))
    assertEquals(70, runBootstrap(versionLine = null, clearPrefix = true).exitCode)
    assertEquals(80, runBootstrap(versionLine = null, stdinToken = "").exitCode)
    assertEquals(72, runBootstrap(versionLine = null).exitCode)
  }

  private fun runBootstrap(
    versionLine: String?,
    mode: String = "ensure",
    port: Int = 12345,
    stdinToken: String = "tok123\n",
    clearPrefix: Boolean = false,
  ): RunResult {
    val root = Files.createTempDirectory("oopbot-launcher-test")
    try {
      val home = Files.createDirectories(root.resolve("home"))
      val prefix = Files.createDirectories(root.resolve("prefix"))
      val sysBin = Files.createDirectories(root.resolve("sysbin"))
      val userBin = Files.createDirectories(root.resolve("userbin"))
      for (tool in listOf("mkdir", "cmp")) {
        symlinkTool(tool, sysBin)
      }
      if (versionLine != null) {
        val fakeOopbot = userBin.resolve("oopbot")
        val quotedLine = versionLine.replace("'", "'\\''")
        val script =
          """
          #!/bin/sh
          if [ "${'$'}1" = "--version" ]; then
            printf '%s\n' '$quotedLine'
            exit 0
          fi
          printf 'ran:%s:%s\n' "${'$'}*" "${'$'}OOPBOT_TOKEN"
          """.trimIndent() + "\n"
        Files.writeString(fakeOopbot, script)
        fakeOopbot.toFile().setExecutable(true)
      }
      val processBuilder = ProcessBuilder(
        "/bin/bash",
        "--noprofile",
        "--norc",
        "-c",
        HostLauncher.BOOTSTRAP,
        "pipkin-host",
        mode,
        port.toString(),
      )
      val env = processBuilder.environment()
      env.clear()
      env["HOME"] = home.toString()
      if (!clearPrefix) env["PREFIX"] = prefix.toString()
      env["PATH"] = "$userBin:$sysBin"
      val process = processBuilder.start()
      process.outputStream.use { it.write(stdinToken.toByteArray(Charsets.UTF_8)) }
      val stdout = process.inputStream.bufferedReader().readText()
      val stderr = process.errorStream.bufferedReader().readText()
      val exitCode = process.waitFor()
      return RunResult(exitCode, stdout, stderr)
    } finally {
      root.toFile().deleteRecursively()
    }
  }

  private fun symlinkTool(name: String, destDir: Path) {
    val target = listOf("/bin/$name", "/usr/bin/$name").map(Path::of).first { Files.isExecutable(it) }
    Files.createSymbolicLink(destDir.resolve(name), target)
  }

  private data class RunResult(val exitCode: Int, val stdout: String, val stderr: String)
}
