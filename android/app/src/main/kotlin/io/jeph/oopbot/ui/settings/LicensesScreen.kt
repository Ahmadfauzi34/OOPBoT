package io.jeph.oopbot.ui.settings

import android.content.res.AssetManager
import androidx.compose.foundation.text.selection.SelectionContainer
import androidx.compose.material3.Text
import androidx.compose.runtime.Composable
import androidx.compose.runtime.getValue
import androidx.compose.runtime.produceState
import androidx.compose.runtime.rememberUpdatedState
import androidx.compose.runtime.snapshotFlow
import androidx.compose.ui.platform.LocalContext
import androidx.compose.ui.res.stringResource
import io.jeph.oopbot.R
import io.jeph.oopbot.ui.LocalTransitionSettled
import io.jeph.oopbot.ui.components.LucideIcon
import io.jeph.oopbot.ui.theme.OopbotFonts
import io.jeph.oopbot.ui.theme.OopbotTokens
import io.jeph.oopbot.ui.theme.oopbotColors
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.flow.first
import kotlinx.coroutines.withContext

/** The licence files of the code, fonts and icons the app ships: `src/main/assets/licenses` and the host build's. */
private const val COMPONENT_LICENSES = "licenses"

/** tree-sitter's and each grammar's licence, which the `syntaxSources` build step copies out of their sources. */
private const val SYNTAX_LICENSES = "syntax/licenses"

private class LicenseGroups(val components: List<String>, val grammars: List<String>)

/** The third-party licences the app ships, one row each, reached from Settings › About. */
@Composable
internal fun LicensesScreen(onBack: () -> Unit, onOpenLicense: (path: String) -> Unit) {
  val assets = LocalContext.current.assets
  val settled by rememberUpdatedState(LocalTransitionSettled.current)
  val groups by produceState<LicenseGroups?>(null, assets) {
    val loaded = withContext(Dispatchers.IO) {
      LicenseGroups(
        components = licensePaths(assets, COMPONENT_LICENSES),
        grammars = licensePaths(assets, SYNTAX_LICENSES),
      )
    }
    snapshotFlow { settled }.first { it }
    value = loaded
  }
  SettingsPage(title = stringResource(R.string.settings_licenses), onBack = onBack) {
    SettingsCaption(text = stringResource(R.string.settings_licenses_caption))
    groups?.let { loaded ->
      if (loaded.components.isNotEmpty()) {
        LicenseSection(stringResource(R.string.settings_licenses_components), loaded.components, onOpenLicense)
      }
      if (loaded.grammars.isNotEmpty()) {
        LicenseSection(stringResource(R.string.settings_licenses_syntax), loaded.grammars, onOpenLicense)
      }
    }
  }
}

/** The text of the licence asset at [path], selectable for copying. */
@Composable
internal fun LicenseScreen(path: String, onBack: () -> Unit) {
  val assets = LocalContext.current.assets
  val settled by rememberUpdatedState(LocalTransitionSettled.current)
  val text by produceState<String?>(null, path, assets) {
    val loaded = withContext(Dispatchers.IO) { assets.open(path).bufferedReader(Charsets.UTF_8).use { it.readText() } }
    snapshotFlow { settled }.first { it }
    value = loaded
  }
  SettingsPage(title = licenseName(path), onBack = onBack) {
    text?.let {
      SelectionContainer {
        Text(
          text = it,
          color = oopbotColors.foreground,
          fontFamily = OopbotFonts.mono,
          fontSize = OopbotTokens.FontSize.code,
          lineHeight = OopbotTokens.LineHeight.code,
        )
      }
    }
  }
}

@Composable
private fun LicenseSection(title: String, paths: List<String>, onOpenLicense: (path: String) -> Unit) {
  SettingsSection(title = title) {
    paths.forEachIndexed { index, path ->
      if (index > 0) SettingsDivider()
      SettingsRow(title = licenseName(path), onClick = { onOpenLicense(path) }) {
        LucideIcon(
          icon = R.drawable.ic_lucide_chevron_right,
          contentDescription = null,
          tint = oopbotColors.foregroundMuted,
        )
      }
    }
  }
}

/** The asset paths of the licence files in [dir], by name, so `perl` comes before `perl-bsearch`. */
private fun licensePaths(assets: AssetManager, dir: String): List<String> =
  assets.list(dir).orEmpty().filter { it.endsWith(LICENSE_SUFFIX) }.map { "$dir/$it" }.sortedBy(::licenseName)

/** A licence file's name, which names the component it covers. */
private fun licenseName(path: String): String = path.substringAfterLast('/').removeSuffix(LICENSE_SUFFIX)

private const val LICENSE_SUFFIX = ".txt"
