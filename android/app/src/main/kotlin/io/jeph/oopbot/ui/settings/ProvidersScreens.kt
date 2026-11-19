package io.jeph.oopbot.ui.settings

import androidx.compose.foundation.background
import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Box
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.ColumnScope
import androidx.compose.foundation.layout.PaddingValues
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.imePadding
import androidx.compose.foundation.layout.navigationBarsPadding
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.layout.widthIn
import androidx.compose.foundation.lazy.LazyColumn
import androidx.compose.foundation.lazy.LazyListScope
import androidx.compose.foundation.lazy.LazyListState
import androidx.compose.foundation.rememberScrollState
import androidx.compose.foundation.verticalScroll
import androidx.compose.material3.Text
import androidx.compose.runtime.Composable
import androidx.compose.runtime.LaunchedEffect
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.remember
import androidx.compose.runtime.rememberCoroutineScope
import androidx.compose.runtime.rememberUpdatedState
import androidx.compose.runtime.saveable.rememberSaveable
import androidx.compose.runtime.setValue
import androidx.compose.runtime.snapshotFlow
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.platform.LocalContext
import androidx.compose.ui.res.stringResource
import androidx.lifecycle.compose.collectAsStateWithLifecycle
import io.jeph.oopbot.R
import io.jeph.oopbot.appGraph
import io.jeph.oopbot.host.AuthStatus
import io.jeph.oopbot.host.ExternalEngineInfo
import io.jeph.oopbot.host.HostRejected
import io.jeph.oopbot.ui.LocalTransitionSettled
import io.jeph.oopbot.ui.auth.LoginSheet
import io.jeph.oopbot.ui.auth.ProviderLogo
import io.jeph.oopbot.ui.components.HeaderBar
import io.jeph.oopbot.ui.components.HeaderButton
import io.jeph.oopbot.ui.components.LucideIcon
import io.jeph.oopbot.ui.components.SimmeringPot
import io.jeph.oopbot.ui.components.SnackbarStack
import io.jeph.oopbot.ui.components.SnackbarStackState
import io.jeph.oopbot.ui.components.StatusDot
import io.jeph.oopbot.ui.components.scrollFades
import io.jeph.oopbot.ui.theme.OopbotTokens
import io.jeph.oopbot.ui.theme.oopbotColors
import java.io.IOException
import kotlinx.coroutines.flow.first
import kotlinx.coroutines.launch

/**
 * The added providers: signed-in providers Pi ships, installed external ACP CLI engines, then custom
 * OpenAI-compatible endpoints; below them, the experimental models that run on the phone.
 */
@Composable
internal fun ProvidersScreen(
  onBack: () -> Unit,
  onAdd: () -> Unit,
  onOpenSubscription: (id: String) -> Unit,
  onEditProvider: (id: String) -> Unit,
  onOpenLocalModels: () -> Unit,
) {
  val graph = LocalContext.current.appGraph
  val accounts = graph.accountRepository
  val status by accounts.status.collectAsStateWithLifecycle()
  val engines by accounts.engines.collectAsStateWithLifecycle()
  val activeEngine by graph.preferences.activeEngine.collectAsStateWithLifecycle()
  val refreshError by accounts.refreshError.collectAsStateWithLifecycle()
  val settled by rememberUpdatedState(LocalTransitionSettled.current)
  var managingEngine by rememberSaveable { mutableStateOf<String?>(null) }
  LaunchedEffect(Unit) {
    if (accounts.status.value != null) snapshotFlow { settled }.first { it }
    accounts.refresh()
  }
  SettingsPage(title = stringResource(R.string.providers_title), onBack = onBack) {
    ProvidersListSection(
      status = status,
      installedEngines = engines.filter { it.installed },
      activeEngine = activeEngine,
      refreshError = refreshError,
      onOpenSubscription = onOpenSubscription,
      onManageEngine = { managingEngine = it },
      onEditProvider = onEditProvider,
    )
    DashedActionRow(label = stringResource(R.string.providers_add), icon = R.drawable.ic_lucide_plus, onClick = onAdd)
    status?.modelsError?.let { SettingsCaption(text = it.localized(), color = oopbotColors.destructive) }
    refreshError?.let { SettingsCaption(text = it, color = oopbotColors.destructive) }
    LocalModelsSection(preferences = graph.preferences, onOpenLocalModels = onOpenLocalModels)
  }
  managingEngine?.let { engineId ->
    engines.firstOrNull { it.id == engineId }?.let { target ->
      EngineSetupSheet(engine = target, onDismiss = { managingEngine = null })
    }
  }
}

@Composable
private fun ProvidersListSection(
  status: AuthStatus?,
  installedEngines: List<ExternalEngineInfo>,
  activeEngine: String?,
  refreshError: String?,
  onOpenSubscription: (id: String) -> Unit,
  onManageEngine: (id: String) -> Unit,
  onEditProvider: (id: String) -> Unit,
) {
  val signedIn = subscriptions.filter { status?.isSignedIn(it.id) == true }
  val custom = status?.custom.orEmpty()
  when {
    status == null && refreshError == null ->
      Box(modifier = Modifier.fillMaxWidth(), contentAlignment = Alignment.Center) { SimmeringPot() }

    signedIn.isEmpty() && installedEngines.isEmpty() && custom.isEmpty() ->
      SettingsCaption(text = stringResource(R.string.providers_empty))

    else -> SettingsSection(title = null) {
      signedIn.forEachIndexed { index, subscription ->
        if (index > 0) SettingsDivider()
        SettingsRow(
          title = stringResource(subscription.title),
          subtitle = stringResource(subscription.subtitle),
          onClick = { onOpenSubscription(subscription.id) },
          leading = { ProviderLogo(subscription.id) },
        ) { Chevron() }
      }
      installedEngines.forEachIndexed { index, engine ->
        if (index > 0 || signedIn.isNotEmpty()) SettingsDivider()
        InstalledEngineRow(
          engine = engine,
          isActive = activeEngine == engine.selector,
          onClick = { onManageEngine(engine.id) },
        )
      }
      custom.forEachIndexed { index, provider ->
        if (index > 0 || signedIn.isNotEmpty() || installedEngines.isNotEmpty()) SettingsDivider()
        CustomProviderRow(provider = provider, onClick = { onEditProvider(provider.id) })
      }
    }
  }
}

/**
 * The kinds of provider to add. Built-in providers and external ACP CLI engines (such as Antigravity)
 * appear together in one unified list; the OpenAI-compatible option continues to the provider editor.
 */
@Composable
internal fun AddProviderScreen(onBack: () -> Unit, onAddCompatible: () -> Unit, onAdded: (providerId: String) -> Unit) {
  val graph = LocalContext.current.appGraph
  val accounts = graph.accountRepository
  val status by accounts.status.collectAsStateWithLifecycle()
  val engines by accounts.engines.collectAsStateWithLifecycle()
  val activeEngine by graph.preferences.activeEngine.collectAsStateWithLifecycle()
  val refreshError by accounts.refreshError.collectAsStateWithLifecycle()
  var signingIn by rememberSaveable { mutableStateOf<String?>(null) }
  var settingUpEngine by rememberSaveable { mutableStateOf<String?>(null) }
  val displayEngines = engines.ifEmpty { listOf(defaultAntigravityEngine()) }
  // A screen restored after process death is composed without the providers list that loads status.
  LaunchedEffect(Unit) {
    when {
      accounts.status.value == null -> accounts.refresh()
      accounts.engines.value.isEmpty() -> accounts.refreshEngines()
    }
  }
  SettingsPage(title = stringResource(R.string.providers_add), onBack = onBack) {
    SettingsSection(title = null) {
      subscriptions.forEach { subscription ->
        AddSubscriptionRow(
          subscription = subscription,
          added = status?.isSignedIn(subscription.id) == true,
          onClick = { signingIn = subscription.id },
        )
        SettingsDivider()
      }
      displayEngines.forEach { engine ->
        AddEngineRow(
          engine = engine,
          isActive = activeEngine == engine.selector,
          onClick = { settingUpEngine = engine.id },
        )
        SettingsDivider()
      }
      SettingsRow(
        title = stringResource(R.string.providers_compatible),
        subtitle = stringResource(R.string.providers_compatible_subtitle),
        onClick = onAddCompatible,
        leading = { RowIcon(R.drawable.ic_lucide_key_round) },
      ) { Chevron() }
    }
    refreshError?.let { SettingsCaption(text = it, color = oopbotColors.destructive) }
  }
  AddProviderSheets(
    signingIn = signingIn,
    settingUpEngine = settingUpEngine,
    displayEngines = displayEngines,
    onDismissLogin = { id ->
      signingIn = null
      if (accounts.status.value?.isSignedIn(id) == true) onAdded(id)
    },
    onDismissEngine = { settingUpEngine = null },
  )
}

@Composable
private fun AddProviderSheets(
  signingIn: String?,
  settingUpEngine: String?,
  displayEngines: List<ExternalEngineInfo>,
  onDismissLogin: (String) -> Unit,
  onDismissEngine: () -> Unit,
) {
  signingIn?.let { id -> LoginSheet(providerId = id, onDismiss = { onDismissLogin(id) }) }
  settingUpEngine?.let { engineId ->
    displayEngines.firstOrNull { it.id == engineId }?.let { target ->
      EngineSetupSheet(engine = target, onDismiss = onDismissEngine)
    }
  }
}

/**
 * Header with a back button, or [topBar] in its place while there is one, over a scrolling, width-capped column of
 * settings cards.
 */
@Composable
internal fun SettingsPage(
  title: String,
  onBack: () -> Unit,
  snackbar: SnackbarStackState? = null,
  topBar: (@Composable () -> Unit)? = null,
  content: @Composable ColumnScope.() -> Unit,
) {
  SettingsPageFrame(title, onBack, snackbar, topBar) {
    Column(
      modifier =
        Modifier
          .fillMaxSize()
          .verticalScroll(rememberScrollState())
          .imePadding()
          .navigationBarsPadding()
          .padding(horizontal = OopbotTokens.Spacing.s16, vertical = OopbotTokens.Spacing.s24),
      horizontalAlignment = Alignment.CenterHorizontally,
    ) {
      Column(
        modifier = SettingsItemWidth,
        verticalArrangement = Arrangement.spacedBy(OopbotTokens.Spacing.s12),
        content = content,
      )
    }
  }
}

/**
 * [SettingsPage] for a page as long as a provider's hundreds of models: only the items on screen are composed, and
 * only they scroll, between [header] at the top of the screen and [footer] at the bottom, fading out where they scroll
 * past either edge. Items are centred but not spaced; each caps its own width with [SettingsItemWidth] and spaces
 * itself.
 */
@Composable
internal fun LazySettingsPage(
  title: String,
  onBack: () -> Unit,
  listState: LazyListState,
  header: @Composable ColumnScope.() -> Unit,
  footer: @Composable () -> Unit,
  content: LazyListScope.() -> Unit,
) {
  val spacing = OopbotTokens.Spacing
  val background = oopbotColors.surface0
  SettingsPageFrame(title, onBack, snackbar = null, topBar = null) {
    Column(
      modifier = Modifier.fillMaxSize().imePadding().navigationBarsPadding(),
      horizontalAlignment = Alignment.CenterHorizontally,
    ) {
      Column(modifier = SettingsItemWidth.padding(start = spacing.s16, top = spacing.s8, end = spacing.s16)) {
        header()
      }
      LazyColumn(
        modifier = Modifier.weight(1f).fillMaxWidth().scrollFades(listState, background),
        state = listState,
        contentPadding = PaddingValues(start = spacing.s16, end = spacing.s16, bottom = spacing.s12),
        horizontalAlignment = Alignment.CenterHorizontally,
        content = content,
      )
      Box(modifier = SettingsItemWidth.padding(start = spacing.s16, end = spacing.s16, bottom = spacing.s16)) {
        footer()
      }
    }
  }
}

/** The width of a settings page's content, capped on wide screens. */
internal val SettingsItemWidth = Modifier.widthIn(max = OopbotTokens.contentMaxWidth).fillMaxWidth()

@Composable
private fun SettingsPageFrame(
  title: String,
  onBack: () -> Unit,
  snackbar: SnackbarStackState?,
  topBar: (@Composable () -> Unit)?,
  body: @Composable () -> Unit,
) {
  Box(modifier = Modifier.fillMaxSize().background(oopbotColors.surface0)) {
    Column(modifier = Modifier.fillMaxSize()) {
      if (topBar != null) {
        topBar()
      } else {
        HeaderBar(
          title = title,
          leading = {
            HeaderButton(
              icon = R.drawable.ic_lucide_chevron_left,
              contentDescription = stringResource(R.string.settings_back),
              onClick = onBack,
            )
          },
        )
      }
      body()
    }
    if (snackbar != null) {
      SnackbarStack(
        snackbar,
        Modifier.align(Alignment.BottomCenter).navigationBarsPadding().padding(OopbotTokens.Spacing.s16),
      )
    }
  }
}

@Composable
internal fun Chevron() {
  LucideIcon(icon = R.drawable.ic_lucide_chevron_right, contentDescription = null, tint = oopbotColors.foregroundMuted)
}

@Composable
internal fun RowIcon(icon: Int) {
  LucideIcon(icon = icon, contentDescription = null, tint = oopbotColors.foregroundMuted)
}
