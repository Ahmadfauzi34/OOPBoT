package io.jeph.oopbot.agent

import org.junit.Assert.assertEquals
import org.junit.Test

class KeepAliveTest {
  @Test
  fun `transitions cleanly between Off, Standby, and Active without redundant updates`() {
    val transitions = mutableListOf<KeepAliveMode>()
    val keepAlive = KeepAlive { mode, _ ->
      transitions += mode
      true
    }

    assertEquals(KeepAliveMode.Off, keepAlive.mode)

    val standby1 = keepAlive.acquireStandby("background standby")
    val standby2 = keepAlive.acquireStandby("waiting on background tasks")
    assertEquals(KeepAliveMode.Standby, keepAlive.mode)
    assertEquals(listOf(KeepAliveMode.Standby), transitions)

    // Promote Standby -> Active while standby holds remain open.
    val active = keepAlive.acquire("chats working")
    assertEquals(KeepAliveMode.Active, keepAlive.mode)
    assertEquals(listOf(KeepAliveMode.Standby, KeepAliveMode.Active), transitions)

    // Closing Active drops back to Standby (not Off!) because standby holds are still open.
    active.close()
    assertEquals(KeepAliveMode.Standby, keepAlive.mode)
    assertEquals(listOf(KeepAliveMode.Standby, KeepAliveMode.Active, KeepAliveMode.Standby), transitions)

    // Closing an already-closed handle is a no-op.
    active.close()
    assertEquals(KeepAliveMode.Standby, keepAlive.mode)

    // Releasing first standby hold keeps Standby until the last standby hold closes.
    standby1.close()
    assertEquals(KeepAliveMode.Standby, keepAlive.mode)
    standby2.close()
    assertEquals(KeepAliveMode.Off, keepAlive.mode)
    assertEquals(
      listOf(KeepAliveMode.Standby, KeepAliveMode.Active, KeepAliveMode.Standby, KeepAliveMode.Off),
      transitions,
    )
  }

  @Test
  fun `overlapping Active to Standby handoff never dips to Off`() {
    val transitions = mutableListOf<KeepAliveMode>()
    val keepAlive = KeepAlive { mode, _ ->
      transitions += mode
      true
    }

    val active = keepAlive.acquire("chats working")
    val standby = keepAlive.acquireStandby("waiting on background tasks")
    active.close()

    assertEquals(KeepAliveMode.Standby, keepAlive.mode)
    assertEquals(listOf(KeepAliveMode.Active, KeepAliveMode.Standby), transitions)

    // Reverse handoff: Standby -> Active without dipping to Off.
    val burst = keepAlive.acquire("chats working")
    standby.close()
    assertEquals(KeepAliveMode.Active, keepAlive.mode)
    assertEquals(listOf(KeepAliveMode.Active, KeepAliveMode.Standby, KeepAliveMode.Active), transitions)

    burst.close()
    assertEquals(KeepAliveMode.Off, keepAlive.mode)
  }
}
