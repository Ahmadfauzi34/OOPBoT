package io.jeph.oopbot.termux

import org.junit.Assert.assertFalse
import org.junit.Assert.assertTrue
import org.junit.Test

class TermuxSetupTest {
  @Test
  fun fdroidCommandRunsInAnyShell() {
    assertRunsInAnyShell(TermuxSetup.fdroidCommand("io.jeph.oopbot.debug"))
  }

  @Test
  fun fdroidCommandChecksExactExpectedVersionAndRunsInAnyShell() {
    val cmd = TermuxSetup.fdroidCommand("io.jeph.oopbot.debug", "0.5.9")
    assertTrue(cmd.contains("\"oopbot 0.5.9\""))
    assertTrue(cmd.contains("/releases/download/v0.5.9/oopbot-aarch64-linux-android"))
    assertRunsInAnyShell(cmd)
  }

  @Test
  fun fdroidCommandInstallsInternetCliToolsAndUsesAria2cWithCurlFallback() {
    val cmd = TermuxSetup.fdroidCommand("io.jeph.oopbot.debug", "0.5.13")
    for (pkg in TermuxSetup.INTERNET_PACKAGES) {
      assertTrue("Missing Termux internet package $pkg", cmd.contains(pkg))
    }
    assertTrue(cmd.contains("aria2c -x 8 -s 8 -k 1M"))
    assertTrue(cmd.contains("curl -fsSL -m 120"))
    assertRunsInAnyShell(cmd)
  }
}

/**
 * [command] is one line that bash runs from a single-quoted argument, with nothing inside that bash, zsh and fish
 * would read differently there.
 */
internal fun assertRunsInAnyShell(command: String) {
  assertTrue(command, command.startsWith("bash -c '") && command.endsWith("'"))
  val script = command.removePrefix("bash -c '").removeSuffix("'")
  assertFalse(command, script.any { it == '\'' || it == '\\' || it == '\n' })
}
