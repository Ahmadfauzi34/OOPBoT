package io.jeph.oopbot.termux

import android.content.ClipData
import android.content.ClipDescription
import android.content.ClipboardManager
import android.content.Context
import android.os.Build
import android.os.PersistableBundle
import io.jeph.oopbot.MainActivity
import io.jeph.oopbot.agent.KeepAlive
import io.jeph.oopbot.host.HostLauncher
import io.jeph.oopbot.host.PlaySetup
import io.jeph.oopbot.host.TermuxBuild

/**
 * One-paste Termux preparation. Every part checks before it acts, so pasting a command again after a failure (no
 * network, a package mirror outage, a closed Termux) finishes the setup instead of breaking it.
 */
internal object TermuxSetup {
  const val DEFAULT_OOPBOT_VERSION = "0.5.23"
  private const val RELEASE_BASE = "https://github.com/Ahmadfauzi34/OOPBoT/releases"
  private const val RELEASE_ASSET = "oopbot-aarch64-linux-android"
  internal val INTERNET_PACKAGES = listOf(
    "aria2",
    "wget",
    "jq",
    "dnsutils",
    "netcat-openbsd",
    "openssh",
    "git",
  )
  private val INTERNET_TOOL_SPECS = listOf(
    "aria2:aria2c",
    "wget:wget",
    "jq:jq",
    "dnsutils:dig",
    "netcat-openbsd:nc",
    "openssh:ssh",
    "git:git",
  )
  private val SAFE_VERSION = Regex("""^\d+\.\d+\.\d+(?:[-+.][0-9A-Za-z._+-]+)?$""")

  internal fun sanitizeVersion(raw: String): String = raw.trim().takeIf(SAFE_VERSION::matches) ?: DEFAULT_OOPBOT_VERSION

  /**
   * F-Droid: OOPBoT native binary, internet-bound CLI toolkit (aria2c, wget, jq, dnsutils/dig, netcat, openssh, git),
   * allow-external-apps, then back to OOPBoT, the build with [packageName]: the debug build has its own package
   * beside the release. `dpkg --configure -a` finishes an upgrade an earlier paste left half done. `yes` answers
   * dpkg's conffile prompts. Storage access is its own setup step.
   */
  fun fdroidCommand(packageName: String, expectedVersion: String = DEFAULT_OOPBOT_VERSION): String {
    val version = sanitizeVersion(expectedVersion)
    val tagUrl = "$RELEASE_BASE/download/v$version/$RELEASE_ASSET"
    val latestUrl = "$RELEASE_BASE/latest/download/$RELEASE_ASSET"
    val rootBin = "${'$'}HOME/.local/share/${TermuxBuild.NAME}/bin/oopbot"
    val netPkgs = INTERNET_PACKAGES.joinToString(" ")
    val netSpecs = INTERNET_TOOL_SPECS.joinToString(" ")
    val netReadyCheck = INTERNET_TOOL_SPECS.joinToString(" && ") { spec ->
      "command -v ${spec.substringAfter(':')} >/dev/null 2>&1"
    }
    val ariaArgs = "-x 8 -s 8 -k 1M --connect-timeout=15 --timeout=60 --max-tries=2 " +
      "--allow-overwrite=true --auto-file-renaming=false --summary-interval=0"
    val ariaTag = "command -v aria2c >/dev/null 2>&1 && " +
      "aria2c $ariaArgs -d \"${'$'}{t%/*}\" -o \"${'$'}{t##*/}\" \"$tagUrl\""
    val ariaLatest = "command -v aria2c >/dev/null 2>&1 && " +
      "aria2c $ariaArgs -d \"${'$'}{t%/*}\" -o \"${'$'}t_name\" \"$latestUrl\""
    val curlTag = "rm -f \"${'$'}t\" \"${'$'}t.aria2\" && curl -fsSL -m 120 \"$tagUrl\" -o \"${'$'}t\""
    val curlLatest = "rm -f \"${'$'}t\" \"${'$'}t.aria2\" && curl -fsSL -m 120 \"$latestUrl\" -o \"${'$'}t\""
    return inBash(
      "u=0 && { command -v curl >/dev/null 2>&1 || " +
        "{ dpkg --configure -a; yes | pkg upgrade -y && u=1 && " +
        "{ pkg install -y curl $netPkgs || pkg install -y curl; }; }; } && " +
        "{ { $netReadyCheck; } || " +
        "{ { [ \"${'$'}u\" = 1 ] || { dpkg --configure -a || true; yes | pkg upgrade -y || true; }; } && " +
        "{ pkg install -y $netPkgs || for p in $netSpecs; do " +
        "command -v \"${'$'}{p#*:}\" >/dev/null 2>&1 || pkg install -y \"${'$'}{p%:*}\" || true; done; }; }; } && " +
        "{ { command -v oopbot >/dev/null 2>&1 && case \"${'$'}(oopbot --version 2>/dev/null)\" in " +
        "\"oopbot $version\"|\"oopbot $version \"*) true;; *) false;; esac; } || " +
        "{ mkdir -p \"${'$'}PREFIX/bin\" && t=\"${'$'}PREFIX/bin/oopbot.tmp\" && t_name=\"${'$'}{t##*/}\" && " +
        "rm -f \"${'$'}t\" \"${'$'}t.aria2\" && " +
        "{ { $ariaTag; } || { $curlTag; } || " +
        "{ rm -f \"${'$'}t\" \"${'$'}t.aria2\" && $ariaLatest; } || { $curlLatest; }; } && " +
        "chmod 755 \"${'$'}t\" && " +
        "case \"${'$'}(\"${'$'}t\" --version 2>/dev/null)\" in " +
        "\"oopbot $version\"|\"oopbot $version \"*) true;; *) false;; esac && " +
        "mv -f \"${'$'}t\" \"${'$'}PREFIX/bin/oopbot\" || " +
        "{ rm -f \"${'$'}PREFIX/bin/oopbot.tmp\" \"${'$'}PREFIX/bin/oopbot.tmp.aria2\"; false; }; }; } && " +
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
   * that folder, files included. OOPBoT comes back once access is granted, after a minute, or at once on a "no"; the
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
   * it only when it's about to be copied; [keepAlive] keeps OOPBoT serving its installer while the user is in Termux.
   */
  fun command(context: Context, keepAlive: KeepAlive): String = when {
    TermuxInstall.status(context).isPlay -> PlaySetup.newCommand(context, keepAlive)
    else -> fdroidCommand(context.packageName, DEFAULT_OOPBOT_VERSION)
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
