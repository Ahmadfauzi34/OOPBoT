package io.jeph.oopbot.ui.settings

import androidx.compose.runtime.Composable
import androidx.compose.ui.res.stringResource
import io.jeph.oopbot.R
import io.jeph.oopbot.host.CustomProvider
import io.jeph.oopbot.host.ExternalEngineInfo
import io.jeph.oopbot.ui.auth.ProviderLogo

// Row composables shared by the providers list and the unified "Add a provider" list.

@Composable
internal fun InstalledEngineRow(engine: ExternalEngineInfo, isActive: Boolean, onClick: () -> Unit) {
  val engineSubtitle = when {
    isActive -> stringResource(R.string.engine_using_for_new_chats)
    engine.authenticated -> stringResource(R.string.engine_status_signed_in)
    else -> stringResource(R.string.engine_status_sign_in_needed)
  }
  SettingsRow(
    title = engine.displayName,
    subtitle = engineSubtitle,
    onClick = onClick,
    leading = { ProviderLogo(engine.id) },
  ) { Chevron() }
}

@Composable
internal fun CustomProviderRow(provider: CustomProvider, onClick: () -> Unit) {
  SettingsRow(
    title = provider.id,
    subtitle = provider.baseUrl,
    onClick = onClick,
    leading = { RowIcon(R.drawable.ic_lucide_key_round) },
  ) { Chevron() }
}

@Composable
internal fun AddSubscriptionRow(subscription: Subscription, added: Boolean, onClick: () -> Unit) {
  SettingsRow(
    title = stringResource(subscription.title),
    subtitle = stringResource(subscription.subtitle),
    enabled = !added,
    onClick = onClick,
    leading = { ProviderLogo(subscription.id) },
  ) {
    if (added) SettingsValue(stringResource(R.string.providers_added)) else Chevron()
  }
}

@Composable
internal fun AddEngineRow(engine: ExternalEngineInfo, isActive: Boolean, onClick: () -> Unit) {
  val subtitle = engine.subtitle.ifBlank { stringResource(R.string.engine_antigravity_subtitle) }
  SettingsRow(
    title = engine.displayName,
    subtitle = subtitle,
    onClick = onClick,
    leading = { ProviderLogo(engine.id) },
  ) {
    when {
      isActive -> SettingsValue(stringResource(R.string.engine_status_active))
      engine.installed -> SettingsValue(stringResource(R.string.engine_status_installed))
      else -> Chevron()
    }
  }
}

@Composable
internal fun defaultAntigravityEngine(): ExternalEngineInfo = ExternalEngineInfo(
  id = "antigravity",
  selector = "acp:antigravity",
  displayName = stringResource(R.string.engine_antigravity_title),
  subtitle = stringResource(R.string.engine_antigravity_subtitle),
  protocol = "acp",
  githubRepo = "wallentx/antigravity-cli-termux",
  installed = false,
  installedVersion = null,
  command = null,
  requiresTermuxGlibc = true,
  glibcReady = false,
  authenticated = false,
)
