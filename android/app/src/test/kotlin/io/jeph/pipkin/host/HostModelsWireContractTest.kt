package io.jeph.pipkin.host

import java.net.InetAddress
import java.net.Socket
import kotlinx.coroutines.CoroutineScope
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.SupervisorJob
import kotlinx.coroutines.cancel
import kotlinx.coroutines.flow.first
import kotlinx.coroutines.runBlocking
import kotlinx.coroutines.withTimeout
import org.json.JSONArray
import org.json.JSONObject
import org.junit.After
import org.junit.Assert.assertEquals
import org.junit.Assert.assertFalse
import org.junit.Assert.assertNull
import org.junit.Assert.assertTrue
import org.junit.Test

class HostModelsWireContractTest {
  private val scope = CoroutineScope(SupervisorJob() + Dispatchers.IO)
  private val server = HostServer(scope)
  private val clients = mutableListOf<Socket>()

  @After
  fun tearDown() {
    clients.forEach(Socket::close)
    server.close()
    scope.cancel()
  }

  @Test
  fun hostHelloParsesOopbotAndLegacyPiVersion() {
    val modern = HostHello.from(
      JSONObject()
        .put("oopbotVersion", "0.5.5")
        .put("nodeVersion", "oopbot-native")
        .put("configDir", "/data/data/com.termux/files/home/.pipkin")
        .put("home", "/data/data/com.termux/files/home")
        .put("sharedStorage", true),
    )
    assertEquals("0.5.5", modern.piVersion)
    assertEquals("oopbot-native", modern.nodeVersion)
    assertTrue(modern.sharedStorage)

    val legacy = HostHello.from(JSONObject().put("piVersion", "0.5.4"))
    assertEquals("0.5.4", legacy.piVersion)
    assertFalse(legacy.sharedStorage)
  }

  @Test
  fun backupAndSubagentPayloadsMatchRustWireContract() {
    val validBackup = BackupInfo.from(
      JSONObject()
        .put("name", "backup-2026-11-13.tar.gz")
        .put("size", 4096)
        .put("createdAt", "2026-11-13T10:00:00Z")
        .put("piVersion", "0.5.5"),
    )
    assertEquals("backup-2026-11-13.tar.gz", validBackup.name)
    assertEquals(4096L, validBackup.size)
    assertTrue(validBackup.createdAt > 0L)
    assertEquals("0.5.5", validBackup.piVersion)
    assertNull(validBackup.error)

    val corruptBackup = BackupInfo.from(
      JSONObject()
        .put("name", "corrupt.tar.gz")
        .put("size", 12)
        .put("createdAt", "not-an-iso-timestamp")
        .put("error", JSONObject().put("key", "backup_corrupt")),
    )
    assertEquals(0L, corruptBackup.createdAt)
    assertEquals("backup_corrupt", corruptBackup.error?.key)

    val subagent = SubagentSession.from(
      JSONObject()
        .put("path", "/tmp/subagents/call_1/run.jsonl")
        .put("toolCallId", "call_1")
        .put("agent", "research")
        .put("description", "Inspect repo")
        .put("created", 1_700_000_000_000L)
        .put("modified", 1_700_000_005_000L)
        .put("messageCount", 4),
    )
    assertEquals("call_1", subagent.toolCallId)
    assertEquals("research", subagent.agent)
    assertEquals(4, subagent.messageCount)
  }

  @Test
  fun authStatusDistinguishesNullScopeFromExplicitScopeList() {
    val unrestricted = AuthStatus.from(
      JSONObject()
        .put(
          "builtin",
          JSONArray().put(
            JSONObject()
              .put("id", "google")
              .put("name", "Google")
              .put("configured", true)
              .put("type", "oauth")
              .put("enabled", JSONObject.NULL),
          ),
        )
        .put("custom", JSONArray())
        .put("scope", JSONObject.NULL),
    )
    assertNull(unrestricted.scope)
    assertTrue(unrestricted.anyConfigured)
    assertEquals("oauth", unrestricted.builtin.single().type)
    assertNull(unrestricted.builtin.single().enabled)

    val scoped = AuthStatus.from(
      JSONObject()
        .put(
          "builtin",
          JSONArray().put(
            JSONObject()
              .put("id", "openai")
              .put("configured", true)
              .put("type", "api_key")
              .put("enabled", JSONArray().put("gpt-5")),
          ),
        )
        .put("custom", JSONArray())
        .put("scope", JSONArray().put("openai/gpt-5"))
        .put("modelsError", JSONObject().put("key", "models_corrupt")),
    )
    assertEquals(setOf("openai/gpt-5"), scoped.scope)
    assertEquals(listOf("gpt-5"), scoped.builtin.single().enabled)
    assertEquals("models_corrupt", scoped.modelsError?.key)
  }

  @Test
  fun hostChannelExchangesRequestProgressAndResultLinesOverTcp() = runBlocking {
    val token = "c".repeat(64)
    val deferred = server.register(token)
    val client = Socket(InetAddress.getByName("127.0.0.1"), server.port).also {
      it.soTimeout = TIMEOUT.toInt()
      clients += it
    }
    val out = client.getOutputStream()
    val reader = client.getInputStream().bufferedReader(Charsets.UTF_8)

    val hello = JSONObject()
      .put("type", HostProtocol.HELLO)
      .put("protocol", HostProtocol.VERSION)
      .put("token", token)
      .put("mode", "ensure")
      .put("oopbotVersion", "0.5.5")
    out.write((hello.toString() + "\n").toByteArray(Charsets.UTF_8))
    out.flush()

    val channel = withTimeout(TIMEOUT) { deferred.await() }
    assertEquals("0.5.5", HostHello.from(channel.hello).piVersion)

    channel.send(
      JSONObject()
        .put("type", HostProtocol.REQUEST)
        .put("mode", "ensure")
        .put("params", JSONObject().put("expectedVersion", "0.5.5")),
    )
    val requestLine = JSONObject(reader.readLine())
    assertEquals(HostProtocol.REQUEST, requestLine.getString("type"))
    assertEquals("ensure", requestLine.getString("mode"))

    val progress = JSONObject()
      .put("type", HostProtocol.PROGRESS)
      .put("text", JSONObject().put("key", "starting"))
    val result = JSONObject()
      .put("type", HostProtocol.RESULT)
      .put("ok", true)
      .put("data", JSONObject().put("oopbotVersion", "0.5.5"))
    out.write((progress.toString() + "\n" + result.toString() + "\n").toByteArray(Charsets.UTF_8))
    out.flush()

    val seenProgress = withTimeout(TIMEOUT) {
      channel.lines.first { HostProtocol.isProgress(it.optString("type")) }
    }
    assertEquals("starting", HostText.field(seenProgress, "text")?.key)

    val seenResult = withTimeout(TIMEOUT) {
      channel.lines.first { HostProtocol.isResult(it.optString("type")) }
    }
    assertTrue(seenResult.getBoolean("ok"))
    assertEquals("0.5.5", seenResult.getJSONObject("data").getString("oopbotVersion"))
    channel.close()
  }

  private companion object {
    const val TIMEOUT = 5_000L
  }
}
