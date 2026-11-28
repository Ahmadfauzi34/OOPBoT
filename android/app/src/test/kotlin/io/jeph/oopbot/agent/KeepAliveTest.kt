package io.jeph.oopbot.agent

import java.util.concurrent.CopyOnWriteArrayList
import java.util.concurrent.CountDownLatch
import java.util.concurrent.ExecutorService
import java.util.concurrent.Executors
import java.util.concurrent.TimeUnit
import java.util.concurrent.atomic.AtomicBoolean
import java.util.concurrent.atomic.AtomicInteger
import kotlinx.coroutines.async
import kotlinx.coroutines.runBlocking
import kotlinx.coroutines.withTimeout
import org.junit.Assert.assertEquals
import org.junit.Assert.assertFalse
import org.junit.Assert.assertTrue
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
    assertEquals(KeepAliveMode.Off, keepAlive.appliedMode)

    val standby1 = keepAlive.acquireStandby("background standby")
    val standby2 = keepAlive.acquireStandby("waiting on background tasks")
    assertEquals(KeepAliveMode.Standby, keepAlive.mode)
    assertEquals(KeepAliveMode.Standby, keepAlive.appliedMode)
    assertEquals(listOf(KeepAliveMode.Standby), transitions)

    // Promote Standby -> Active while standby holds remain open.
    val active = keepAlive.acquire("chats working")
    assertEquals(KeepAliveMode.Active, keepAlive.mode)
    assertEquals(KeepAliveMode.Active, keepAlive.appliedMode)
    assertEquals(listOf(KeepAliveMode.Standby, KeepAliveMode.Active), transitions)

    // Closing Active drops back to Standby (not Off!) because standby holds are still open.
    active.close()
    assertEquals(KeepAliveMode.Standby, keepAlive.mode)
    assertEquals(KeepAliveMode.Standby, keepAlive.appliedMode)
    assertEquals(listOf(KeepAliveMode.Standby, KeepAliveMode.Active, KeepAliveMode.Standby), transitions)

    // Closing an already-closed handle is a no-op.
    active.close()
    assertEquals(KeepAliveMode.Standby, keepAlive.mode)

    // Releasing first standby hold keeps Standby until the last standby hold closes.
    standby1.close()
    assertEquals(KeepAliveMode.Standby, keepAlive.mode)
    standby2.close()
    assertEquals(KeepAliveMode.Off, keepAlive.mode)
    assertEquals(KeepAliveMode.Off, keepAlive.appliedMode)
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

  @Test
  fun `failed Off to Standby preserves appliedMode Off and recovers on sync`() {
    val transitions = mutableListOf<Pair<KeepAliveMode, String>>()
    var allow = false
    val keepAlive = KeepAlive { mode, reason ->
      transitions += mode to reason
      allow
    }

    val standby = keepAlive.acquireStandby("background standby")
    assertEquals(KeepAliveMode.Standby, keepAlive.mode)
    assertEquals(KeepAliveMode.Off, keepAlive.appliedMode)
    assertEquals(listOf(KeepAliveMode.Standby to "background standby"), transitions)

    // Retry while still refused leaves appliedMode = Off without spinning.
    keepAlive.sync("BOOT_COMPLETED")
    assertEquals(KeepAliveMode.Standby, keepAlive.mode)
    assertEquals(KeepAliveMode.Off, keepAlive.appliedMode)
    assertEquals(2, transitions.size)

    // Foreground / boot recovery succeeds and marks appliedMode = Standby.
    allow = true
    keepAlive.sync("foreground")
    assertEquals(KeepAliveMode.Standby, keepAlive.mode)
    assertEquals(KeepAliveMode.Standby, keepAlive.appliedMode)
    assertEquals(KeepAliveMode.Standby to "foreground", transitions.last())

    // Subsequent sync is a no-op once converged.
    keepAlive.sync("foreground")
    assertEquals(3, transitions.size)

    standby.close()
    assertEquals(KeepAliveMode.Off, keepAlive.mode)
    assertEquals(KeepAliveMode.Off, keepAlive.appliedMode)
  }

  @Test
  fun `failed Off to Active suppresses spurious Off on close`() {
    val transitions = mutableListOf<KeepAliveMode>()
    val keepAlive = KeepAlive { mode, _ ->
      transitions += mode
      false
    }

    val active = keepAlive.acquire("chats working")
    assertEquals(KeepAliveMode.Active, keepAlive.mode)
    assertEquals(KeepAliveMode.Off, keepAlive.appliedMode)
    assertEquals(listOf(KeepAliveMode.Active), transitions)

    // Closing the failed hold must NOT dispatch a spurious ACTION_IDLE.
    active.close()
    assertEquals(KeepAliveMode.Off, keepAlive.mode)
    assertEquals(KeepAliveMode.Off, keepAlive.appliedMode)
    assertEquals(listOf(KeepAliveMode.Active), transitions)
  }

  @Test
  fun `failed Active to Off keeps appliedMode Active until sync retries Off`() {
    val transitions = mutableListOf<KeepAliveMode>()
    var allowOff = false
    val keepAlive = KeepAlive { mode, _ ->
      transitions += mode
      mode != KeepAliveMode.Off || allowOff
    }

    val active = keepAlive.acquire("chats working")
    assertEquals(KeepAliveMode.Active, keepAlive.appliedMode)

    active.close()
    assertEquals(KeepAliveMode.Off, keepAlive.mode)
    assertEquals(KeepAliveMode.Active, keepAlive.appliedMode)
    assertEquals(listOf(KeepAliveMode.Active, KeepAliveMode.Off), transitions)

    allowOff = true
    keepAlive.sync("foreground")
    assertEquals(KeepAliveMode.Off, keepAlive.mode)
    assertEquals(KeepAliveMode.Off, keepAlive.appliedMode)
    assertEquals(listOf(KeepAliveMode.Active, KeepAliveMode.Off, KeepAliveMode.Off), transitions)
  }

  @Test
  fun `force sync refreshes active policy without affecting Off and respects noteStopped`() {
    val transitions = mutableListOf<Pair<KeepAliveMode, String>>()
    val keepAlive = KeepAlive { mode, reason ->
      transitions += mode to reason
      true
    }

    // Force sync while Off is a no-op.
    keepAlive.sync("policy", force = true)
    assertTrue(transitions.isEmpty())

    val active = keepAlive.acquire("chats working")
    keepAlive.sync("policy", force = true)
    assertEquals(
      listOf(KeepAliveMode.Active to "chats working", KeepAliveMode.Active to "policy"),
      transitions,
    )

    // Simulate OS destroying service while hold is still open; sync must restart it.
    keepAlive.noteStopped()
    assertEquals(KeepAliveMode.Active, keepAlive.mode)
    assertEquals(KeepAliveMode.Off, keepAlive.appliedMode)
    keepAlive.sync("foreground")
    assertEquals(KeepAliveMode.Active, keepAlive.appliedMode)
    assertEquals(3, transitions.size)

    active.close()
    assertEquals(KeepAliveMode.Off, keepAlive.appliedMode)
  }

  @Test
  fun `concurrent acquire and close serializes updater outside lock without stalling callers`() {
    val enteredFirstUpdate = CountDownLatch(1)
    val releaseFirstUpdate = CountDownLatch(1)
    val inUpdater = AtomicBoolean(false)
    val overlapped = AtomicBoolean(false)
    val callCount = AtomicInteger(0)
    val observed = CopyOnWriteArrayList<KeepAliveMode>()

    val keepAlive = KeepAlive { mode, _ ->
      if (!inUpdater.compareAndSet(false, true)) overlapped.set(true)
      try {
        observed += mode
        if (callCount.incrementAndGet() == 1) {
          enteredFirstUpdate.countDown()
          assertTrue(releaseFirstUpdate.await(5, TimeUnit.SECONDS))
        }
        true
      } finally {
        inUpdater.set(false)
      }
    }

    val pool = Executors.newFixedThreadPool(4)
    try {
      // Thread 1 starts an Active hold and blocks inside updater().
      val firstHoldFuture = pool.submit<AutoCloseable> { keepAlive.acquire("io work") }
      assertTrue(enteredFirstUpdate.await(5, TimeUnit.SECONDS))

      // Caller on another thread acquires Standby without blocking on Thread 1's updater IPC.
      val standby = keepAlive.acquireStandby("main standby")
      assertEquals(KeepAliveMode.Active, keepAlive.mode)

      // Release Thread 1's updater so it finishes draining.
      releaseFirstUpdate.countDown()
      val firstHold = firstHoldFuture.get(5, TimeUnit.SECONDS)
      assertEquals(KeepAliveMode.Active, keepAlive.appliedMode)

      // Stress concurrent acquire/close across 4 worker threads.
      runConcurrentHoldStorm(pool, keepAlive, workers = 4, iterations = 50)

      firstHold.close()
      assertEquals(KeepAliveMode.Standby, keepAlive.mode)
      assertEquals(KeepAliveMode.Standby, keepAlive.appliedMode)

      standby.close()
      assertEquals(KeepAliveMode.Off, keepAlive.mode)
      assertEquals(KeepAliveMode.Off, keepAlive.appliedMode)
      assertFalse("updater must never run concurrently", overlapped.get())
      assertEquals(KeepAliveMode.Off, observed.last())
    } finally {
      pool.shutdownNow()
    }
  }

  private fun runConcurrentHoldStorm(pool: ExecutorService, keepAlive: KeepAlive, workers: Int, iterations: Int) {
    val startGate = CountDownLatch(1)
    val doneGate = CountDownLatch(workers)
    repeat(workers) { worker ->
      pool.submit {
        assertTrue(startGate.await(5, TimeUnit.SECONDS))
        try {
          repeat(iterations) { i ->
            val h = if ((worker + i) % 2 == 0) {
              keepAlive.acquire("w$worker-$i")
            } else {
              keepAlive.acquireStandby("s$worker-$i")
            }
            h.close()
          }
        } finally {
          doneGate.countDown()
        }
      }
    }
    startGate.countDown()
    assertTrue(doneGate.await(10, TimeUnit.SECONDS))
  }

  @Test
  fun `noteStopped during in-flight updater is not overwritten when updater returns true`() {
    val calls = mutableListOf<KeepAliveMode>()
    lateinit var keepAlive: KeepAlive
    var simulateStopDuringFirstUpdate = true
    keepAlive = KeepAlive { mode, _ ->
      calls += mode
      if (simulateStopDuringFirstUpdate) {
        simulateStopDuringFirstUpdate = false
        // Simulates AgentService failing startForeground() and calling noteStopped() before updater() returns.
        keepAlive.noteStopped()
      }
      true
    }

    val active = keepAlive.acquire("chats working")
    assertEquals(KeepAliveMode.Active, keepAlive.mode)
    assertEquals(KeepAliveMode.Off, keepAlive.appliedMode)
    assertEquals(listOf(KeepAliveMode.Active), calls)

    // Subsequent foreground sync sees appliedMode == Off and retries Active cleanly.
    keepAlive.sync("foreground")
    assertEquals(KeepAliveMode.Active, keepAlive.appliedMode)
    assertEquals(listOf(KeepAliveMode.Active, KeepAliveMode.Active), calls)

    active.close()
    assertEquals(KeepAliveMode.Off, keepAlive.appliedMode)
  }

  @Test
  fun `awaitActiveApplied waits for in-flight drain and fails fast if Active is refused`() = runBlocking {
    val enteredStandby = CountDownLatch(1)
    val releaseStandby = CountDownLatch(1)
    var allowActive = true

    val keepAlive = KeepAlive { mode, _ ->
      if (mode == KeepAliveMode.Standby) {
        enteredStandby.countDown()
        assertTrue(releaseStandby.await(5, TimeUnit.SECONDS))
        true
      } else {
        allowActive
      }
    }

    assertFalse(keepAlive.awaitActiveApplied())

    val pool = Executors.newSingleThreadExecutor()
    try {
      val standbyFuture = pool.submit<AutoCloseable> { keepAlive.acquireStandby("standby") }
      assertTrue(enteredStandby.await(5, TimeUnit.SECONDS))

      // Acquire Active while Standby drain is in-flight; acquire returns before Active is applied.
      val activeHold = keepAlive.acquire("schedule")
      assertEquals(KeepAliveMode.Active, keepAlive.mode)
      assertEquals(KeepAliveMode.Off, keepAlive.appliedMode)

      val awaiting = async { keepAlive.awaitActiveApplied() }
      releaseStandby.countDown()
      assertTrue(withTimeout(5_000) { awaiting.await() })
      assertEquals(KeepAliveMode.Active, keepAlive.appliedMode)

      val standbyHold = standbyFuture.get(5, TimeUnit.SECONDS)
      activeHold.close()
      assertFalse(keepAlive.awaitActiveApplied())

      // Now verify awaitActiveApplied returns false immediately if Active was refused.
      allowActive = false
      val refusedHold = keepAlive.acquire("schedule refused")
      assertEquals(KeepAliveMode.Active, keepAlive.mode)
      assertEquals(KeepAliveMode.Standby, keepAlive.appliedMode)
      assertFalse(keepAlive.awaitActiveApplied())

      refusedHold.close()
      standbyHold.close()
    } finally {
      pool.shutdownNow()
    }
  }
}
