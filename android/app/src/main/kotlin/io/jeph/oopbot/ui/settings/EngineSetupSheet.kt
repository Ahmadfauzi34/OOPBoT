package io.jeph.oopbot.ui.settings

import androidx.compose.foundation.background
import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.layout.size
import androidx.compose.foundation.rememberScrollState
import androidx.compose.foundation.shape.RoundedCornerShape
import androidx.compose.foundation.verticalScroll
import androidx.compose.material3.Text
import androidx.compose.runtime.Composable
import androidx.compose.runtime.LaunchedEffect
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableIntStateOf
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.remember
import androidx.compose.runtime.rememberCoroutineScope
import androidx.compose.runtime.setValue
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.platform.LocalContext
import androidx.compose.ui.res.stringResource
import androidx.compose.ui.text.font.FontWeight
import androidx.compose.ui.unit.dp
import androidx.lifecycle.compose.collectAsStateWithLifecycle
import io.jeph.oopbot.R
import io.jeph.oopbot.appGraph
import io.jeph.oopbot.host.ExternalEngineInfo
import io.jeph.oopbot.host.HostRejected
import io.jeph.oopbot.ui.auth.AuthStatusLine
import io.jeph.oopbot.ui.auth.LeadingRow
import io.jeph.oopbot.ui.auth.LoginBody
import io.jeph.oopbot.ui.auth.LoginFlowState
import io.jeph.oopbot.ui.auth.LoginOutcome
import io.jeph.oopbot.ui.auth.openUrl
import io.jeph.oopbot.ui.components.LucideIcon
import io.jeph.oopbot.ui.components.OopbotSheet
import io.jeph.oopbot.ui.components.PillButton
import io.jeph.oopbot.ui.components.PillTone
import io.jeph.oopbot.ui.components.RoundIconButton
import io.jeph.oopbot.ui.components.SimmeringPot
import io.jeph.oopbot.ui.theme.OopbotFonts
import io.jeph.oopbot.ui.theme.OopbotTokens
import io.jeph.oopbot.ui.theme.oopbotColors
import java.io.IOException
import kotlinx.coroutines.delay
import kotlinx.coroutines.launch

private sealed interface EngineSheetMode {
  data object Overview : EngineSheetMode

  data class Installing(val progressLine: String? = null) : EngineSheetMode

  data class InstallFailed(val message: String, val manualCommand: String? = null) : EngineSheetMode

  data object SigningIn : EngineSheetMode

  data object Uninstalling : EngineSheetMode
}

/**
 * Bottom sheet for installing, signing in to, activating, or removing an external ACP CLI engine
 * (such as Antigravity CLI) inside Termux.
 */
@Composable
internal fun EngineSetupSheet(engine: ExternalEngineInfo, onDismiss: () -> Unit) {
  val graph = LocalContext.current.appGraph
  val accounts = graph.accountRepository
  val preferences = graph.preferences
  val engines by accounts.engines.collectAsStateWithLifecycle()
  val activeEngine by preferences.activeEngine.collectAsStateWithLifecycle()
  val current = engines.firstOrNull { it.id == engine.id } ?: engine
  val scope = rememberCoroutineScope()
  var mode by remember(engine.id) { mutableStateOf<EngineSheetMode>(EngineSheetMode.Overview) }
  var uninstallError by remember(engine.id) { mutableStateOf<String?>(null) }
  val installFailedFallback = stringResource(R.string.engine_install_failed)
  val uninstallFailedFallback = stringResource(R.string.engine_uninstall_failed)

  val onInstall: () -> Unit = {
    uninstallError = null
    mode = EngineSheetMode.Installing()
    scope.launch {
      mode = try {
        accounts.installEngine(current.id) { _, line ->
          mode = EngineSheetMode.Installing(progressLine = line)
        }
        EngineSheetMode.Overview
      } catch (rejected: HostRejected) {
        val manual = rejected.data?.optString("manualCommand")?.takeIf { it.isNotBlank() }
        EngineSheetMode.InstallFailed(rejected.error, manual)
      } catch (failure: IOException) {
        EngineSheetMode.InstallFailed(failure.message ?: installFailedFallback)
      }
    }
  }

  val onUninstall: () -> Unit = {
    uninstallError = null
    mode = EngineSheetMode.Uninstalling
    scope.launch {
      try {
        accounts.uninstallEngine(current.id)
        if (preferences.activeEngine.value == current.selector) preferences.setActiveEngine(null)
      } catch (rejected: HostRejected) {
        uninstallError = rejected.error
      } catch (failure: IOException) {
        uninstallError = failure.message ?: uninstallFailedFallback
      }
      mode = EngineSheetMode.Overview
    }
  }

  EngineSetupSheetFrame(
    engine = current,
    isActive = activeEngine == current.selector,
    mode = mode,
    uninstallError = uninstallError,
    onDismiss = onDismiss,
    onInstall = onInstall,
    onSignIn = { mode = EngineSheetMode.SigningIn },
    onSignInDone = { mode = EngineSheetMode.Overview },
    onSetActive = { enabled -> preferences.setActiveEngine(if (enabled) current.selector else null) },
    onUninstall = onUninstall,
  )
}

@Suppress("LongParameterList") // Sheet frame delegates actions cleanly from EngineSetupSheet state holder.
@Composable
private fun EngineSetupSheetFrame(
  engine: ExternalEngineInfo,
  isActive: Boolean,
  mode: EngineSheetMode,
  uninstallError: String?,
  onDismiss: () -> Unit,
  onInstall: () -> Unit,
  onSignIn: () -> Unit,
  onSignInDone: () -> Unit,
  onSetActive: (Boolean) -> Unit,
  onUninstall: () -> Unit,
) {
  OopbotSheet(onDismissRequest = onDismiss) {
    Column(
      modifier = Modifier
        .fillMaxWidth()
        .verticalScroll(rememberScrollState())
        .padding(horizontal = OopbotTokens.Spacing.s16),
      verticalArrangement = Arrangement.spacedBy(OopbotTokens.Spacing.s12),
    ) {
      Text(
        text = engine.displayName,
        color = oopbotColors.foreground,
        fontSize = OopbotTokens.FontSize.lg,
        fontWeight = FontWeight.Bold,
      )
      when (mode) {
        EngineSheetMode.Overview -> EngineOverviewBody(
          engine = engine,
          isActive = isActive,
          error = uninstallError,
          onInstall = onInstall,
          onSignIn = onSignIn,
          onSetActive = onSetActive,
          onUninstall = onUninstall,
        )

        is EngineSheetMode.Installing -> LeadingRow(
          text = mode.progressLine ?: stringResource(R.string.engine_installing),
          color = oopbotColors.foregroundMuted,
        ) { SimmeringPot() }

        is EngineSheetMode.InstallFailed -> EngineInstallFailedBody(
          message = mode.message,
          manualCommand = mode.manualCommand,
          onRetry = onInstall,
        )

        EngineSheetMode.SigningIn -> EngineLoginBody(engineId = engine.id, onDone = onSignInDone)

        EngineSheetMode.Uninstalling -> LeadingRow(
          text = stringResource(R.string.engine_uninstalling),
          color = oopbotColors.foregroundMuted,
        ) { SimmeringPot() }
      }
    }
  }
}

@Composable
private fun EngineOverviewBody(
  engine: ExternalEngineInfo,
  isActive: Boolean,
  error: String?,
  onInstall: () -> Unit,
  onSignIn: () -> Unit,
  onSetActive: (Boolean) -> Unit,
  onUninstall: () -> Unit,
) {
  val colors = oopbotColors
  if (!engine.installed) {
    Text(
      text = stringResource(R.string.engine_install_caption),
      color = colors.foregroundMuted,
      fontSize = OopbotTokens.FontSize.base,
    )
    PillButton(label = stringResource(R.string.engine_install_button), onClick = onInstall)
    return
  }

  engine.installedVersion?.let { version ->
    Text(
      text = stringResource(R.string.engine_version_label, version),
      color = colors.foregroundMuted,
      fontSize = OopbotTokens.FontSize.base,
    )
  }

  EngineStatusBadge(authenticated = engine.authenticated, isActive = isActive)

  if (isActive) {
    PillButton(
      label = stringResource(R.string.engine_switch_to_native),
      onClick = { onSetActive(false) },
      tone = PillTone.Tonal,
    )
  } else if (engine.authenticated) {
    PillButton(label = stringResource(R.string.engine_use_for_new_chats), onClick = { onSetActive(true) })
  }

  PillButton(
    label = stringResource(
      if (engine.authenticated) R.string.engine_sign_in_again_button else R.string.engine_sign_in_button,
    ),
    onClick = onSignIn,
    tone = if (engine.authenticated) PillTone.Tonal else PillTone.Primary,
  )
  PillButton(label = stringResource(R.string.engine_reinstall_button), onClick = onInstall, tone = PillTone.Tonal)
  PillButton(
    label = stringResource(R.string.engine_uninstall_button),
    onClick = onUninstall,
    tone = PillTone.Destructive,
  )
  error?.let { Text(text = it, color = colors.destructive, fontSize = OopbotTokens.FontSize.base) }
}

@Composable
private fun EngineStatusBadge(authenticated: Boolean, isActive: Boolean) {
  val colors = oopbotColors
  if (authenticated) {
    LeadingRow(text = stringResource(R.string.engine_status_signed_in)) {
      LucideIcon(icon = R.drawable.ic_lucide_check, contentDescription = null, tint = colors.accent)
    }
  } else {
    LeadingRow(text = stringResource(R.string.engine_status_sign_in_needed), color = colors.foregroundMuted) {
      LucideIcon(icon = R.drawable.ic_lucide_info, contentDescription = null, tint = colors.foregroundMuted)
    }
  }
  if (isActive) {
    LeadingRow(text = stringResource(R.string.engine_using_for_new_chats)) {
      LucideIcon(icon = R.drawable.ic_lucide_circle_check, contentDescription = null, tint = colors.accent)
    }
  }
}

@Composable
private fun EngineInstallFailedBody(message: String, manualCommand: String?, onRetry: () -> Unit) {
  val context = LocalContext.current
  val colors = oopbotColors
  val copyLabel = stringResource(R.string.engine_copy_command)
  LeadingRow(text = message) {
    LucideIcon(icon = R.drawable.ic_lucide_circle_alert, contentDescription = null, tint = colors.destructive)
  }
  manualCommand?.let { command ->
    Text(
      text = stringResource(R.string.engine_manual_command_hint),
      color = colors.foregroundMuted,
      fontSize = OopbotTokens.FontSize.base,
    )
    Row(
      modifier = Modifier
        .fillMaxWidth()
        .background(colors.surface2, RoundedCornerShape(COMMAND_RADIUS))
        .padding(start = OopbotTokens.Spacing.s12, end = OopbotTokens.Spacing.s8)
        .padding(vertical = OopbotTokens.Spacing.s8),
      verticalAlignment = Alignment.CenterVertically,
      horizontalArrangement = Arrangement.spacedBy(OopbotTokens.Spacing.s8),
    ) {
      Text(
        text = command,
        modifier = Modifier.weight(1f),
        color = colors.foreground,
        fontFamily = OopbotFonts.mono,
        fontSize = OopbotTokens.FontSize.sm,
      )
      RoundIconButton(
        icon = R.drawable.ic_lucide_copy,
        contentDescription = copyLabel,
        onClick = { copyToClipboard(context, copyLabel, command) },
        modifier = Modifier.size(36.dp),
        background = colors.surface3,
        tint = colors.foreground,
      )
    }
  }
  PillButton(label = stringResource(R.string.login_try_again), onClick = onRetry)
}

@Composable
private fun EngineLoginBody(engineId: String, onDone: () -> Unit) {
  val context = LocalContext.current
  val accounts = context.appGraph.accountRepository
  var attempt by remember { mutableIntStateOf(0) }
  val flow = remember(attempt) { LoginFlowState() }
  val noBrowser = stringResource(R.string.login_no_browser)
  val open: (String) -> Unit = { url ->
    if (!openUrl(context, url)) flow.status = AuthStatusLine(noBrowser, emptyList())
  }

  LaunchedEffect(flow) {
    flow.runEngine(accounts, engineId)
    if (flow.outcome == LoginOutcome.SignedIn) {
      delay(SIGNED_IN_DELAY_MILLIS)
      onDone()
    }
  }

  val browserUrl = flow.browser?.url
  LaunchedEffect(browserUrl) {
    if (browserUrl != null) open(browserUrl)
  }

  LoginBody(
    flow = flow,
    usesApiKey = false,
    onOpen = open,
    onRetry = { attempt += 1 },
  )
}

private val COMMAND_RADIUS = 14.dp
private const val SIGNED_IN_DELAY_MILLIS = 800L
