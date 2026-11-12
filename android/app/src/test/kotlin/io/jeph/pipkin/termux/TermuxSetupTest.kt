package io.jeph.pipkin.termux

import org.junit.Assert.assertFalse
import org.junit.Assert.assertTrue
import org.junit.Test

class TermuxSetupTest {
  @Test
  fun fdroidCommandRunsInAnyShell() {
    assertRunsInAnyShell(TermuxSetup.fdroidCommand("io.jeph.pipkin.debug"))
  }

  @Test
  fun fdroidCommandChecksExactExpectedVersionAndRunsInAnyShell() {
    val cmd = TermuxSetup.fdroidCommand("io.jeph.pipkin.debug", "0.5.9")
    assertTrue(cmd.contains("\"oopbot 0.5.9\""))
    assertTrue(cmd.contains("/releases/download/v0.5.9/oopbot-aarch64-linux-android"))
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
