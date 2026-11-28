package io.jeph.oopbot.agent

import android.app.Application
import android.content.Context
import androidx.test.core.app.ApplicationProvider
import io.jeph.oopbot.AppStrings
import io.jeph.oopbot.data.NoticeStore
import io.jeph.oopbot.data.PasswordCipher
import io.jeph.oopbot.data.Preferences
import io.jeph.oopbot.data.RemoteHosts
import io.jeph.oopbot.data.RemotePasswords
import io.jeph.oopbot.data.SessionRepository
import io.jeph.oopbot.host.HostChannel
import io.jeph.oopbot.host.HostClient
import io.jeph.oopbot.host.HostFailure
import io.jeph.oopbot.host.HostProtocol
import io.jeph.oopbot.host.LineReader
import io.jeph.oopbot.host.RemoteFailure
import io.jeph.oopbot.termux.TermuxSetup
import java.io.BufferedReader
import java.io.File
import java.io.IOException
import java.io.InputStreamReader
import java.net.InetAddress
import java.net.ServerSocket
import java.net.Socket
import java.util.UUID
import java.util.concurrent.CopyOnWriteArrayList
import java.util.concurrent.Executor
import java.util.concurrent.atomic.AtomicInteger
import kotlinx.coroutines.CompletableDeferred
import kotlinx.coroutines.CoroutineScope
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.SupervisorJob
import kotlinx.coroutines.cancel
import kotlinx.coroutines.delay
import kotlinx.coroutines.flow.MutableStateFlow
import kotlinx.coroutines.runBlocking
import kotlinx.coroutines.withTimeout
import org.json.JSONArray
import org.json.JSONObject
import org.junit.After
import org.junit.Assert.assertEquals
import org.junit.Assert.assertFalse
import org.junit.Assert.assertTrue
import org.junit.Before
import org.junit.Test
import org.junit.runner.RunWith
import org.robolectric.RobolectricTestRunner
import org.robolectric.annotation.Config
import org.robolectric.shadows.ShadowLooper

@RunWith(RobolectricTestRunner::class)
@Config(manifest = Config.NONE, application = Application::class, sdk = [35], shadows = [StubResourcesShadow::class])
class AgentProcessBackgroundTest {
  private lateinit var app: Application
  private lateinit var scope: CoroutineScope
  private lateinit var ioScope: CoroutineScope
  private val closables = mutableListOf<AutoCloseable>()

  @Before
  fun setUp() {
    app = ApplicationProvider.getApplicationContext()
    AppStrings.init(app)
    scope = CoroutineScope(SupervisorJob() + Dispatchers.Main.immediate)
    ioScope = CoroutineScope(SupervisorJob() + Dispatchers.IO)
  }

  @After
  fun tearDown() {
    scope.cancel()
    ioScope.cancel()
    for (item in closables) {
      runCatching { item.close() }
    }
    closables.clear()
  }

  private fun buildHarness(
    foreground: MutableStateFlow<Boolean>,
    canLaunchInBackground: suspend () -> Boolean = { true },
    onPeer: (attempt: Int, peerSocket: Socket, reader: BufferedReader, requestParams: JSONObject) -> Unit,
  ): Harness {
    val launchedModes = CopyOnWriteArrayList<String>()
    val requests = CopyOnWriteArrayList<JSONObject>()
    val attempts = AtomicInteger(0)
    val keepAlive = KeepAlive { _, _ -> true }
    val hostClient = HostClient(
      context = app,
      foreground = foreground,
      scope = scope,
      canLaunchInBackground = canLaunchInBackground,
      launcher = { _, mode ->
        launchedModes += mode
        val attempt = attempts.incrementAndGet()
        openMockChannel(mode, attempt, requests, onPeer)
      },
    )
    val prefs = Preferences(app.getSharedPreferences("test-${UUID.randomUUID()}", Context.MODE_PRIVATE), "test")
    val passwords = RemotePasswords(
      File(app.cacheDir, "pw-${UUID.randomUUID()}"),
      object : PasswordCipher {
        override fun seal(plaintext: ByteArray, associatedData: ByteArray): ByteArray = plaintext

        override fun open(sealed: ByteArray, associatedData: ByteArray): ByteArray = sealed
      },
    )
    val syncExecutor = Executor(Runnable::run)
    val notices = NoticeStore(dir = File(app.cacheDir, "notices-${UUID.randomUUID()}"), writer = syncExecutor)
    val sessions = SessionRepository(hostClient, notices, scope)
    val remoteHosts = RemoteHosts(hostClient, prefs, passwords, keepAlive, scope)
    return Harness(hostClient, keepAlive, sessions, remoteHosts, launchedModes, requests)
  }

  private fun openMockChannel(
    mode: String,
    attempt: Int,
    requests: CopyOnWriteArrayList<JSONObject>,
    onPeer: (Int, Socket, BufferedReader, JSONObject) -> Unit,
  ): HostChannel {
    val server = ServerSocket(0, 1, InetAddress.getByName("127.0.0.1")).also { closables += it }
    val accepted = CompletableDeferred<Socket>()
    Thread {
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
    val peerSocket = runBlocking { withTimeout(2_000) { accepted.await() } }.also { closables += it }
    val hello = JSONObject().put("mode", mode).put("oopbotVersion", TermuxSetup.DEFAULT_OOPBOT_VERSION)
    val channel = HostChannel(clientSocket, LineReader(clientSocket.getInputStream()), hello, ioScope)
    closables += AutoCloseable { channel.close() }
    Thread {
      try {
        val reader = BufferedReader(InputStreamReader(peerSocket.getInputStream()))
        val reqLine = reader.readLine() ?: return@Thread
        val params = JSONObject(reqLine).optJSONObject("params") ?: JSONObject()
        requests += params
        onPeer(attempt, peerSocket, reader, params)
      } catch (_: IOException) {
        // Test closed socket.
      }
    }.apply {
      isDaemon = true
      start()
    }
    return channel
  }

  private fun serveReadySession(
    peerSocket: Socket,
    reader: BufferedReader,
    sessionPath: String = "/s/bg.jsonl",
  ): Socket {
    val out = peerSocket.getOutputStream()
    while (true) {
      val line = reader.readLine() ?: break
      val req = JSONObject(line)
      val id = req.optString("id")
      val type = req.optString("type")
      val data = when (type) {
        "get_state" -> JSONObject()
          .put("sessionFile", sessionPath)
          .put("sessionId", "sid-1")
          .put("isStreaming", false)

        "get_messages" -> JSONObject().put("messages", JSONArray())

        "get_session_stats" -> JSONObject()

        else -> JSONObject()
      }
      val resp = JSONObject()
        .put("type", "response")
        .put("id", id)
        .put("command", type)
        .put("success", true)
        .put("data", data)
      out.write("${resp}\n".toByteArray(Charsets.UTF_8))
      out.flush()
    }
    return peerSocket
  }

  private suspend fun awaitState(process: AgentProcess, predicate: (AgentProcessState) -> Boolean): AgentProcessState =
    withTimeout(5_000) {
      while (!predicate(process.state.value)) {
        ShadowLooper.idleMainLooper()
        delay(10)
      }
      process.state.value
    }

  private fun createProcess(
    harness: Harness,
    sessionPath: String? = null,
    options: ProcessLaunchOptions = ProcessLaunchOptions(),
  ): AgentProcess = AgentProcess(
    hostClient = harness.hostClient,
    keepAlive = harness.keepAlive,
    cwd = "/work",
    sessionPath = sessionPath,
    scope = scope,
    sessions = harness.sessions,
    remoteHosts = harness.remoteHosts,
    disabledExtensions = MutableStateFlow(emptySet()),
    activeEngine = MutableStateFlow("acp:default"),
    launchOptions = options,
  ).also { closables += AutoCloseable { it.close() } }

  @Test
  fun connectInBackgroundFailsFastOnOopbotVersionWithoutRunningEnsure() = runBlocking {
    val harness = buildHarness(foreground = MutableStateFlow(false)) { _, peer, _, _ ->
      val reject = JSONObject()
        .put("type", HostProtocol.RESULT)
        .put("ok", false)
        .put("code", "oopbot_version")
        .put("error", "outdated")
      peer.getOutputStream().write("$reject\n".toByteArray(Charsets.UTF_8))
      peer.getOutputStream().flush()
    }

    val process = createProcess(harness, options = ProcessLaunchOptions(allowBackground = true))
    val state = awaitState(process) { it is AgentProcessState.Failed } as AgentProcessState.Failed

    assertTrue(state.failure is HostFailure.NodeTooOld)
    assertEquals(listOf("rpc"), harness.launchedModes)
  }

  @Test
  fun connectInBackgroundFailsFastOnRemoteNotConnectedWithoutReconnecting() = runBlocking {
    val harness = buildHarness(foreground = MutableStateFlow(false)) { _, peer, _, _ ->
      val reject = JSONObject()
        .put("type", HostProtocol.RESULT)
        .put("ok", false)
        .put("code", RemoteFailure.NOT_CONNECTED)
        .put("error", "remote offline")
      peer.getOutputStream().write("$reject\n".toByteArray(Charsets.UTF_8))
      peer.getOutputStream().flush()
    }

    val process = createProcess(harness, options = ProcessLaunchOptions(allowBackground = true))
    val state = awaitState(process) { it is AgentProcessState.Failed } as AgentProcessState.Failed

    assertEquals(RemoteFailure.NOT_CONNECTED, state.code)
    assertEquals(listOf("rpc"), harness.launchedModes)
  }

  @Test
  fun attemptAppliesEngineOverrideOnlyWhenSessionPathIsNull() = runBlocking {
    val harness = buildHarness(foreground = MutableStateFlow(false)) { _, peer, reader, _ ->
      serveReadySession(peer, reader)
    }

    val fresh = createProcess(
      harness,
      sessionPath = null,
      options = ProcessLaunchOptions(allowBackground = true, engineOverride = "acp:scheduled"),
    )
    awaitState(fresh) { it == AgentProcessState.Ready }

    val existing = createProcess(
      harness,
      sessionPath = "/s/existing.jsonl",
      options = ProcessLaunchOptions(allowBackground = true, engineOverride = "acp:scheduled"),
    )
    awaitState(existing) { it == AgentProcessState.Ready }

    assertEquals(2, harness.requests.size)
    assertEquals("acp:scheduled", harness.requests[0].optString("engine"))
    assertFalse(harness.requests[1].has("engine"))
  }

  @Test
  fun onConnectionEndedPreservesAllowBackgroundOnAutoReconnect() = runBlocking {
    val firstPeer = CompletableDeferred<Socket>()
    val harness = buildHarness(foreground = MutableStateFlow(false)) { attempt, peer, reader, _ ->
      if (attempt == 1) firstPeer.complete(peer)
      serveReadySession(peer, reader)
    }

    val process = createProcess(harness, options = ProcessLaunchOptions(allowBackground = true))
    awaitState(process) { it == AgentProcessState.Ready }
    assertEquals(1, harness.launchedModes.size)

    // Drop socket without oopbot_exit while still in background (foreground remains false).
    firstPeer.await().close()

    withTimeout(5_000) {
      while (harness.launchedModes.size < 2 || process.state.value != AgentProcessState.Ready) {
        ShadowLooper.idleMainLooper()
        delay(10)
      }
    }
    assertEquals(listOf("rpc", "rpc"), harness.launchedModes)
    assertEquals(AgentProcessState.Ready, process.state.value)
  }

  @Test
  fun promoteToBackgroundRestartsForegroundWaitingAndFailedButLeavesReadyUntouched() = runBlocking {
    var allowLaunch = true
    val harness = buildHarness(
      foreground = MutableStateFlow(false),
      canLaunchInBackground = { allowLaunch },
    ) { _, peer, reader, _ ->
      serveReadySession(peer, reader)
    }

    // 1) Foreground-waiting Starting process promoted to background connects immediately.
    val waiting = createProcess(
      harness,
      sessionPath = "/s/waiting.jsonl",
      options = ProcessLaunchOptions(allowBackground = false),
    )
    delay(50)
    assertEquals(AgentProcessState.Starting, waiting.state.value)
    assertEquals(0, harness.launchedModes.size)

    waiting.promoteToBackground()
    awaitState(waiting) { it == AgentProcessState.Ready }
    assertEquals(1, harness.launchedModes.size)

    // 2) Calling promoteToBackground() on a Ready process is a no-op.
    waiting.promoteToBackground()
    delay(50)
    assertEquals(1, harness.launchedModes.size)

    // 3) Failed process promoted to background restarts and reaches Ready once KeepAlive allows it.
    allowLaunch = false
    val failed = createProcess(
      harness,
      sessionPath = "/s/failed.jsonl",
      options = ProcessLaunchOptions(allowBackground = true),
    )
    awaitState(failed) { it is AgentProcessState.Failed }
    allowLaunch = true
    failed.promoteToBackground()
    awaitState(failed) { it == AgentProcessState.Ready }
    assertEquals(2, harness.launchedModes.size)
  }

  private class Harness(
    val hostClient: HostClient,
    val keepAlive: KeepAlive,
    val sessions: SessionRepository,
    val remoteHosts: RemoteHosts,
    val launchedModes: CopyOnWriteArrayList<String>,
    val requests: CopyOnWriteArrayList<JSONObject>,
  )
}
