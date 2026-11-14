package io.jeph.pipkin.termux

import android.content.ClipData
import android.content.ClipDescription
import android.content.ClipboardManager
import android.content.Context
import android.os.Build
import android.os.PersistableBundle
import io.jeph.pipkin.MainActivity
import io.jeph.pipkin.agent.KeepAlive
import io.jeph.pipkin.host.HostLauncher
import io.jeph.pipkin.host.PlaySetup
import io.jeph.pipkin.host.TermuxBuild
import org.json.JSONObject

/**
 * One-paste Termux preparation. Every part checks before it acts, so pasting a command again after a failure (no
 * network, a package mirror outage, a closed Termux) finishes the setup instead of breaking it.
 */
internal object TermuxSetup {
  const val DEFAULT_OOPBOT_VERSION = "0.5.5"
  private const val RELEASE_BASE = "https://github.com/Ahmadfauzi34/OOPBoT/releases"
  private const val RELEASE_ASSET = "oopbot-aarch64-linux-android"
  private val SAFE_VERSION = Regex("""^\d+\.\d+\.\d+(?:[-+.][0-9A-Za-z._+-]+)?$""")

  internal fun sanitizeVersion(raw: String): String = raw.trim().takeIf(SAFE_VERSION::matches) ?: DEFAULT_OOPBOT_VERSION

  fun expectedOopbotVersion(context: Context): String = runCatching {
    val raw = context.assets.open("pi/package.json").bufferedReader(Charsets.UTF_8).use { it.readText() }
    val deps = JSONObject(raw).getJSONObject("dependencies")
    val configured = deps.optString(
      "oopbot",
      deps.optString("@earendil-works/pi-coding-agent", DEFAULT_OOPBOT_VERSION),
    )
    sanitizeVersion(configured)
  }.getOrDefault(DEFAULT_OOPBOT_VERSION)

  /**
   * F-Droid: OOPBoT native binary, allow-external-apps, then back to Pipkin, the build with [packageName]: the debug
   * build has its own package beside the release. `dpkg --configure -a` finishes an upgrade an earlier paste left half
   * done. `yes` answers dpkg's conffile prompts. Storage access is its own setup step.
   */
  fun fdroidCommand(packageName: String, expectedVersion: String = DEFAULT_OOPBOT_VERSION): String {
    val version = sanitizeVersion(expectedVersion)
    val tagUrl = "$RELEASE_BASE/download/v$version/$RELEASE_ASSET"
    val latestUrl = "$RELEASE_BASE/latest/download/$RELEASE_ASSET"
    val rootBin = "${'$'}HOME/.local/share/${TermuxBuild.NAME}/bin/oopbot"
    return inBash(
      "{ { command -v oopbot >/dev/null 2>&1 && case \"${'$'}(oopbot --version 2>/dev/null)\" in " +
        "\"oopbot $version\"|\"oopbot $version \"*) true;; *) false;; esac; } || " +
        "{ { command -v curl >/dev/null 2>&1 || " +
        "{ dpkg --configure -a; yes | pkg upgrade -y && pkg install -y curl; }; } && " +
        "mkdir -p \"${'$'}PREFIX/bin\" && t=\"${'$'}PREFIX/bin/oopbot.tmp\" && " +
        "{ curl -fsSL -m 120 \"$tagUrl\" -o \"${'$'}t\" || curl -fsSL -m 120 \"$latestUrl\" -o \"${'$'}t\"; } && " +
        "chmod 755 \"${'$'}t\" && " +
        "case \"${'$'}(\"${'$'}t\" --version 2>/dev/null)\" in " +
        "\"oopbot $version\"|\"oopbot $version \"*) true;; *) false;; esac && " +
        "mv -f \"${'$'}t\" \"${'$'}PREFIX/bin/oopbot\" || { rm -f \"${'$'}PREFIX/bin/oopbot.tmp\"; false; }; }; } && " +
        "{ [ ! -e \"$rootBin\" ] || { cp -f \"${'$'}PREFIX/bin/oopbot\" \"$rootBin.tmp\" && " +
        "chmod 755 \"$rootBin.tmp\" && mv -f \"$rootBin.tmp\" \"$rootBin\"; } || rm -f \"$rootBin.tmp\"; } && " +
        "mkdir -p ~/.termux && { grep -qx allow-external-apps=true ~/.termux/termux.properties 2>/dev/null " +
        "|| echo allow-external-apps=true >> ~/.termux/termux.properties; } && termux-reload-settings " +
        "&& echo \"OOPBoT setup done\" && am start -n $packageName/${MainActivity::class.java.name} >/dev/null " +
        "|| echo \"OOPBoT setup did not finish. Check your internet connection, then paste the command again.\"",
    )
  }

  /**
   * F-Droid: opens Termux on a session that shows Termux's own storage prompt; F-Droid's build only, as it needs
   * RUN_COMMAND. The broadcast in `termux-setup-storage` only reaches a Termux on screen, so it waits a moment for
   * Termux to open. Its question about rebuilding an existing `~/storage` stays the user's to answer: Termux empties
   * that folder, files included. Pipkin comes back once access is granted, after a minute, or at once on a "no"; the
   * line it prints is what a user who declined the prompt sees meanwhile. The wait checks shared storage itself, as
   * the host does.
   */
  fun requestFDroidStorage(context: Context) {
    val command = "echo \"OOPBoT: waiting for storage access. Switch back to OOPBoT any time.\"; sleep 1; " +
      "termux-setup-storage && " +
      "for i in $(seq 60); do [ -w \"${'$'}{EXTERNAL_STORAGE:-/sdcard}\" ] && break; sleep 1; done; " +
      "am start -n ${context.packageName}/${MainActivity::class.java.name} >/dev/null"
    HostLauncher.runInTerminal(context, "storage", command)
    TermuxInstall.openTermux(context)
  }

  /**
   * A command that runs [script] with bash, so it works in whichever shell Termux runs, fish included. [script] has
   * no single quotes or backslashes: fish reads `\'` and `\\` inside single quotes as escapes and POSIX shells don't,
   * so neither could be written the same way for both.
   */
  fun inBash(script: String): String = "bash -c '$script'"

  /**
   * The setup command for the installed build. The Play build's carries a fresh single-use pairing token, so ask for
   * it only when it's about to be copied; [keepAlive] keeps Pipkin serving its installer while the user is in Termux.
   */
  fun command(context: Context, keepAlive: KeepAlive): String = when {
    TermuxInstall.status(context).isPlay -> PlaySetup.newCommand(context, keepAlive)
    else -> fdroidCommand(context.packageName, expectedOopbotVersion(context))
  }

  /** Puts [command] on the clipboard and opens Termux so the user can paste it. */
  fun copyAndOpen(context: Context, command: String) {
    copy(context, command)
    TermuxInstall.openTermux(context)
  }

  /**
   * Copies [command]; marked sensitive so Android 13+ doesn't preview it, since the Play build's carries a pairing
   * token.
   */
  fun copy(context: Context, command: String) {
    val clip = ClipData.newPlainText("Termux command", command)
    if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.TIRAMISU) {
      clip.description.extras = PersistableBundle().apply { putBoolean(ClipDescription.EXTRA_IS_SENSITIVE, true) }
    }
    context.getSystemService(ClipboardManager::class.java).setPrimaryClip(clip)
  }
}
