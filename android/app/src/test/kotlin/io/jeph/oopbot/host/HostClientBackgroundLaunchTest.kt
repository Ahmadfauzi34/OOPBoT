package io.jeph.oopbot.host

import android.app.Application
import android.content.Context
import androidx.test.core.app.ApplicationProvider
import io.jeph.oopbot.AppStrings
import io.jeph.oopbot.agent.KeepAlive
import io.jeph.oopbot.agent.KeepAliveMode
import io.jeph.oopbot.agent.StubResourcesShadow
import java.io.BufferedReader
import java.io.IOException
import java.io.InputStreamReader
import java.net.InetAddress
import java.net.ServerSocket
import java.net.Socket
import java.util.concurrent.CountDownLatch
import java.util.concurrent.Executors
import java.util.concurrent.TimeUnit
import java.util.concurrent.atomic.AtomicInteger
import kotlinx.coroutines.CompletableDeferred
import kotlinx.coroutines.CoroutineScope
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.SupervisorJob
import kotlinx.coroutines.async
import kotlinx.coroutines.cancel
import kotlinx.coroutines.flow.MutableStateFlow
import kotlinx.coroutines.runBlocking
import kotlinx.coroutines.withTimeout
import kotlinx.coroutines.withTimeoutOrNull
import org.json.JSONObject
import org.junit.After
import org.junit.Assert.assertEquals
import org.junit.Assert.assertNull
import org.junit.Assert.assertTrue
import org.junit.Before
import org.junit.Test
import org.junit.runner.RunWith
import org.robolectric.RobolectricTestRunner
import org.robolectric.annotation.Config

@RunWith(RobolectricTestRunner::class)
@Config(manifest = Config.NONE, application = Application::class, sdk = [35], shadows = [StubResourcesShadow::class])
class HostClientBackgroundLaunchTest {
  private lateinit var context: Context
  private lateinit var scope: CoroutineScope
  private val closables = mutableListOf<AutoCloseable>()

  @Before
  fun setUp() {
    val application = ApplicationProvider.getApplicationContext<Application>()
    AppStrings.init(application)
    context = application
    scope = CoroutineScope(SupervisorJob() + Dispatchers.IO)
  }

  @After
  fun tearDown() {
    scope.cancel()
    for (item in closables) {
      runCatching { item.close() }
    }
    closables.clear()
  }

  private data class LoopbackPair(val channel: HostChannel, val peerReader: BufferedReader, val peerSocket: Socket)

  private fun openLoopbackChannel(mode: String): LoopbackPair {
    val server = ServerSocket(0, 1, InetAddress.getByName("127.0.0.1")).also { closables += it }
    val accepted = CompletableDeferred<Socket>()
    val acceptThread = Thread {
      try {
        accepted.complete(server.accept())
      } catch (error: IOException) {
        accepted.completeExceptionally(error)
      }
    }.apply {
      isDaemon = true
      start()
    }
    val clientSocket = Socket("127.0.0.1", server.localPort).also { closables += it }
    acceptThread.join(2_000)
    val peerSocket = runBlocking { withTimeout(2_000) { accepted.await() } }.also { closables += it }
    val hello = JSONObject().put("mode", mode).put("oopbotVersion", "0.5.23")
    val channel = HostChannel(clientSocket, LineReader(clientSocket.getInputStream()), hello, scope)
    closables += AutoCloseable { channel.close() }
    return LoopbackPair(channel, BufferedReader(InputStreamReader(peerSocket.getInputStream())), peerSocket)
  }

  @Test
  fun defaultOpenRpcWaitsForForegroundWhenInBackground() = runBlocking {
    val foreground = MutableStateFlow(false)
    val launches = AtomicInteger(0)
    val client = HostClient(
      context = context,
      foreground = foreground,
      scope = scope,
      canLaunchInBackground = { true },
      launcher = { _, mode ->
        launches.incrementAndGet()
        openLoopbackChannel(mode).channel
      },
    )

    val pending = async { client.openRpc("/work", null, emptySet()) }
    assertNull(withTimeoutOrNull(150) { pending.await() })
    assertEquals(0, launches.get())

    foreground.value = true
    val channel = withTimeout(2_000) { pending.await() }
    assertEquals(1, launches.get())
    channel.close()
  }

  @Test
  fun allowBackgroundSucceedsUnderActiveKeepAliveAndSendsRpcRequest() = runBlocking {
    val foreground = MutableStateFlow(false)
    var peer: LoopbackPair? = null
    val client = HostClient(
      context = context,
      foreground = foreground,
      scope = scope,
      canLaunchInBackground = { true },
      launcher = { _, mode ->
        openLoopbackChannel(mode).also { peer = it }.channel
      },
    )

    val channel = withTimeout(2_000) {
      client.openRpc("/work/scheduled", "/s/job.jsonl", emptySet(), engine = "acp:demo", allowBackground = true)
    }
    val line = withTimeout(2_000) { checkNotNull(peer).peerReader.readLine() }
    val request = JSONObject(line)
    assertEquals(HostProtocol.REQUEST, request.getString("type"))
    val params = request.getJSONObject("params")
    assertEquals("/work/scheduled", params.getString("cwd"))
    assertEquals("/s/job.jsonl", params.getString("sessionPath"))
    assertEquals("acp:demo", params.getString("engine"))
    assertNull(client.lastFailure.value)
    channel.close()
  }

  @Test
  fun allowBackgroundFailsFastWhenNoActiveKeepAlive() = runBlocking {
    val foreground = MutableStateFlow(false)
    val launches = AtomicInteger(0)
    val client = HostClient(
      context = context,
      foreground = foreground,
      scope = scope,
      canLaunchInBackground = { false },
      launcher = { _, mode ->
        launches.incrementAndGet()
        openLoopbackChannel(mode).channel
      },
    )

    var thrown: HostFailure? = null
    try {
      withTimeout(1_000) {
        client.openRpc("/work", null, emptySet(), allowBackground = true)
      }
    } catch (failure: HostFailure) {
      thrown = failure
    }

    assertTrue("Expected HostFailure.Background, got $thrown", thrown is HostFailure.Background)
    assertEquals(0, launches.get())
    assertTrue(client.lastFailure.value is HostFailure.Background)
  }

  @Test
  fun allowBackgroundSucceedsInForegroundEvenWithoutActiveKeepAlive() = runBlocking {
    val foreground = MutableStateFlow(true)
    val launches = AtomicInteger(0)
    val client = HostClient(
      context = context,
      foreground = foreground,
      scope = scope,
      canLaunchInBackground = { false },
      launcher = { _, mode ->
        launches.incrementAndGet()
        openLoopbackChannel(mode).channel
      },
    )

    val channel = withTimeout(2_000) {
      client.openRpc("/work", null, emptySet(), allowBackground = true)
    }
    assertEquals(1, launches.get())
    channel.close()
  }

  @Test
  fun allowBackgroundPropagatesHelperUnreachableFromPlayTermux() = runBlocking {
    val foreground = MutableStateFlow(false)
    val client = HostClient(
      context = context,
      foreground = foreground,
      scope = scope,
      canLaunchInBackground = { true },
      launcher = { _, _ -> throw HostFailure.HelperUnreachable() },
    )

    var thrown: HostFailure? = null
    try {
      withTimeout(1_000) {
        client.openRpc("/work", null, emptySet(), allowBackground = true)
      }
    } catch (failure: HostFailure) {
      thrown = failure
    }

    assertTrue("Expected HostFailure.HelperUnreachable, got $thrown", thrown is HostFailure.HelperUnreachable)
    assertTrue(checkNotNull(thrown).fixedInTermux)
    assertTrue(client.lastFailure.value is HostFailure.HelperUnreachable)
  }

  @Test
  fun foregroundOnlyHostCallsStillWaitEvenWhenBackgroundLaunchIsAllowed() = runBlocking {
    val foreground = MutableStateFlow(false)
    val launches = AtomicInteger(0)
    val client = HostClient(
      context = context,
      foreground = foreground,
      scope = scope,
      canLaunchInBackground = { true },
      launcher = { _, mode ->
        launches.incrementAndGet()
        openLoopbackChannel(mode).channel
      },
    )

    val pendingHello = async { client.hello() }
    assertNull(withTimeoutOrNull(150) { pendingHello.await() })
    assertEquals(0, launches.get())
    pendingHello.cancel()
  }

  @Test
  fun allowBackgroundAwaitsInFlightKeepAliveDrainAndFailsFastWhenRefused() = runBlocking {
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
    val client = HostClient(
      context = context,
      foreground = MutableStateFlow(false),
      scope = scope,
      canLaunchInBackground = keepAlive::awaitActiveApplied,
      launcher = { _, mode -> openLoopbackChannel(mode).channel },
    )

    val pool = Executors.newSingleThreadExecutor()
    try {
      val standbyFuture = pool.submit<AutoCloseable> { keepAlive.acquireStandby("standby") }
      assertTrue(enteredStandby.await(5, TimeUnit.SECONDS))

      // Acquire Active mid-drain and immediately openRpc(allowBackground = true): must wait for drain, not fail.
      val activeHold = keepAlive.acquire("schedule")
      val rpcDeferred = async { client.openRpc("/work", null, emptySet(), allowBackground = true) }
      releaseStandby.countDown()
      val channel = withTimeout(5_000) { rpcDeferred.await() }
      channel.close()
      activeHold.close()
      val standbyHold = standbyFuture.get(5, TimeUnit.SECONDS)

      // When updater(Active) is refused, openRpc(allowBackground = true) still fails fast with HostFailure.Background.
      allowActive = false
      val refusedHold = keepAlive.acquire("schedule refused")
      var thrown: HostFailure? = null
      try {
        withTimeout(1_000) { client.openRpc("/work", null, emptySet(), allowBackground = true) }
      } catch (failure: HostFailure) {
        thrown = failure
      }
      assertTrue("Expected HostFailure.Background, got $thrown", thrown is HostFailure.Background)
      refusedHold.close()
      standbyHold.close()
    } finally {
      pool.shutdownNow()
    }
  }
}
