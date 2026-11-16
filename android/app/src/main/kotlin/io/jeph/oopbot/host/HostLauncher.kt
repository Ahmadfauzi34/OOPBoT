package io.jeph.oopbot.host

import android.app.PendingIntent
import android.content.ComponentName
import android.content.Context
import android.content.Intent
import androidx.core.net.toUri
import io.jeph.oopbot.termux.TermuxIdentity
import io.jeph.oopbot.termux.TermuxInstall
import io.jeph.oopbot.termux.TermuxSetup
import java.security.SecureRandom
import java.util.UUID
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.delay
import kotlinx.coroutines.withContext
import kotlinx.coroutines.withTimeoutOrNull

/**
 * Starts one short-lived `oopbot host` in Termux and waits for it to connect back to [HostServer]: through Termux's
 * RUN_COMMAND service in the F-Droid build, through OOPBoT's helper ([HelperBridge]) in the Play build. The auth
 * token travels only on stdin.
 */
internal object HostLauncher {
  @Suppress("SdCardPath") // Termux's own prefix, not this app's storage.
  private const val BASH = "/data/data/com.termux/files/usr/bin/bash"
  private val SERVICE = ComponentName("com.termux", "com.termux.app.RunCommandService")
  private const val CONNECT_TIMEOUT_MILLIS = 30_000L
  private const val TOKEN_BYTES = 32
  private const val INTERRUPTED_RETRIES = 2
  private const val RETRY_DELAY_MILLIS = 300L

  private val random = SecureRandom()

  /**
   * Retries a launch that Termux cancelled while stopping its service (its last command just exited);
   * the retry starts a fresh Termux service. A host the helper's child was killed in is retried too.
   */
  suspend fun launch(context: Context, mode: String): HostChannel {
    repeat(INTERRUPTED_RETRIES) {
      try {
        return launchOnce(context, mode)
      } catch (_: HostFailure.Interrupted) {
        delay(RETRY_DELAY_MILLIS)
      }
    }
    return launchOnce(context, mode)
  }

  private suspend fun launchOnce(context: Context, mode: String): HostChannel = withContext(Dispatchers.IO) {
    // Checked on every launch, not only during onboarding: Termux can be swapped for another build after setup.
    val status = TermuxInstall.status(context)
    if (status == TermuxInstall.Status.NotInstalled) throw HostFailure.MissingTermux()
    if (!status.isTrusted) throw HostFailure.UntrustedTermux()
    TermuxIdentity.verify(context)
    val server = HostServer.shared
    val token = ByteArray(TOKEN_BYTES).also(random::nextBytes).toHexString()
    val arguments = arguments(mode, server.port)
    val stdin = token + "\n"
    val connected = server.register(token)
    val onFailure: (HostFailure) -> Unit = { connected.completeExceptionally(it) }
    var started: StartedHost? = null
    var channel: HostChannel? = null
    try {
      started = if (status.isPlay) {
        HelperBridge.start(context, mode, arguments, stdin, onFailure)
      } else {
        runCommand(context, mode, arguments, stdin, onFailure)
      }
      val result = withTimeoutOrNull(CONNECT_TIMEOUT_MILLIS) { connected.await() } ?: throw HostFailure.Timeout()
      channel = result
      result
    } finally {
      server.unregister(token)
      // Once connected, the channel reports the host's end; Termux's result only adds the host's diagnostics.
      started?.settle(connected = channel != null)
      // If the host matched in the same instant, it gets no request and exits on its own after 10 s.
      if (channel == null) connected.cancel()
    }
  }

  /** `bash` arguments for one host: [BOOTSTRAP], `<mode> <port>`. */
  private fun arguments(mode: String, port: Int): List<String> =
    listOf("--noprofile", "--norc", "-c", BOOTSTRAP, "oopbot-host", mode, port.toString())

  /** Termux reports the command's exit to [HostResultReceiver] through a one-shot PendingIntent. */
  private fun runCommand(
    context: Context,
    mode: String,
    arguments: List<String>,
    stdin: String,
    onFailure: (HostFailure) -> Unit,
  ): StartedHost {
    val id = UUID.randomUUID().toString()
    HostResultReceiver.register(id, TermuxSetup.DEFAULT_OOPBOT_VERSION, onFailure)
    var sent = false
    try {
      start(context, mode, arguments, stdin, resultIntent(context, id))
      sent = true
    } finally {
      if (!sent) HostResultReceiver.unregister(id)
    }
    return StartedHost { connected ->
      if (connected) HostResultReceiver.logExit(id, mode) else HostResultReceiver.unregister(id)
    }
  }

  private fun resultIntent(context: Context, id: String): PendingIntent = PendingIntent.getBroadcast(
    context,
    0,
    Intent(context, HostResultReceiver::class.java).setData("oopbot://termux-result/$id".toUri()),
    PendingIntent.FLAG_CANCEL_CURRENT or PendingIntent.FLAG_ONE_SHOT or PendingIntent.FLAG_MUTABLE,
  )

  private fun start(
    context: Context,
    mode: String,
    arguments: List<String>,
    stdin: String,
    resultIntent: PendingIntent,
  ) = send(
    context,
    Intent("com.termux.RUN_COMMAND").setComponent(SERVICE)
      .putExtra("com.termux.RUN_COMMAND_PATH", BASH)
      .putExtra("com.termux.RUN_COMMAND_ARGUMENTS", arguments.toTypedArray())
      .putExtra("com.termux.RUN_COMMAND_STDIN", stdin)
      .putExtra("com.termux.RUN_COMMAND_BACKGROUND", true)
      .putExtra("com.termux.RUN_COMMAND_BACKGROUND_CUSTOM_LOG_LEVEL", "0")
      .putExtra("com.termux.RUN_COMMAND_COMMAND_LABEL", "OOPBoT: $mode")
      .putExtra("com.termux.RUN_COMMAND_PENDING_INTENT", resultIntent),
  )

  /**
   * Runs [command] in a new terminal session that Termux switches to, for commands that need Termux on screen, such as
   * its storage prompt. The caller opens Termux, since Android may not let Termux bring itself forward.
   */
  fun runInTerminal(context: Context, label: String, command: String) = send(
    context,
    Intent("com.termux.RUN_COMMAND").setComponent(SERVICE)
      .putExtra("com.termux.RUN_COMMAND_PATH", BASH)
      .putExtra("com.termux.RUN_COMMAND_ARGUMENTS", arrayOf("-c", command))
      .putExtra("com.termux.RUN_COMMAND_BACKGROUND", false)
      .putExtra("com.termux.RUN_COMMAND_COMMAND_LABEL", "OOPBoT: $label"),
  )

  private fun send(context: Context, intent: Intent) {
    try {
      // A plain startService is refused while Termux is idle in the background. Termux's
      // RunCommandService calls startForeground() on every start, so this path is always allowed.
      if (context.startForegroundService(intent) == null) throw HostFailure.MissingTermux()
    } catch (_: SecurityException) {
      throw HostFailure.Permission()
    } catch (_: IllegalStateException) {
      throw HostFailure.Background()
    }
  }

  /**
   * `<mode> <port>` for native `oopbot host`; the token is the one stdin line.
   * Exit codes: 70 Termux env, 72 engine missing, 73 engine too old, 80 token missing.
   */
  internal val BOOTSTRAP = $$$"""
    set -eu
    [ -n "${PREFIX:-}" ] && [ -n "${HOME:-}" ] || exit 70
    IFS= read -r OOPBOT_TOKEN || exit 80
    [ -n "$OOPBOT_TOKEN" ] || exit 80
    export OOPBOT_TOKEN
    exec </dev/null
    export OOPBOT_ROOT="$HOME/.local/share/$$${TermuxBuild.NAME}"
    export OOPBOT_CONFIG_DIR="$HOME/.oopbot"
    mkdir -p "$OOPBOT_ROOT" "$OOPBOT_CONFIG_DIR"

    mode=$1
    port=$2
    shift 2
    if command -v oopbot >/dev/null 2>&1; then
      oopbot_ver=$(oopbot --version 2>/dev/null || echo "")
      case "$oopbot_ver" in
        "oopbot 0.5."[5-9] | "oopbot 0.5."[5-9][-+.]* | "oopbot 0.5."[5-9]" "* | "oopbot 0.5."[1-9][0-9]*)
          exec oopbot host "$mode" "$port"
          ;;
        "oopbot 0."[6-9]* | "oopbot 0."[1-9][0-9]* | "oopbot "[1-9]*)
          exec oopbot host "$mode" "$port"
          ;;
        *)
          exit 73
          ;;
      esac
    else
      exit 72
    fi
  """.trimIndent()
}

/** A host command on its way; [settle] tells the bridge whether the host connected, once the launch is decided. */
internal fun interface StartedHost {
  fun settle(connected: Boolean)
}
