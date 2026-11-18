package io.jeph.oopbot.ui

import androidx.activity.compose.BackHandler
import androidx.compose.animation.core.Spring
import androidx.compose.animation.core.spring
import androidx.compose.foundation.gestures.AnchoredDraggableDefaults
import androidx.compose.foundation.gestures.AnchoredDraggableState
import androidx.compose.foundation.gestures.DraggableAnchors
import androidx.compose.foundation.gestures.Orientation
import androidx.compose.foundation.gestures.anchoredDraggable
import androidx.compose.foundation.gestures.animateTo
import androidx.compose.foundation.gestures.awaitEachGesture
import androidx.compose.foundation.gestures.awaitFirstDown
import androidx.compose.foundation.gestures.snapTo
import androidx.compose.foundation.layout.Box
import androidx.compose.foundation.layout.WindowInsets
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.layout.safeDrawing
import androidx.compose.foundation.layout.windowInsetsPadding
import androidx.compose.material3.VerticalDivider
import androidx.compose.runtime.Composable
import androidx.compose.runtime.CompositionLocalProvider
import androidx.compose.runtime.LaunchedEffect
import androidx.compose.runtime.MutableState
import androidx.compose.runtime.Stable
import androidx.compose.runtime.derivedStateOf
import androidx.compose.runtime.getValue
import androidx.compose.runtime.key
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.remember
import androidx.compose.runtime.rememberCoroutineScope
import androidx.compose.runtime.rememberUpdatedState
import androidx.compose.runtime.saveable.rememberSaveable
import androidx.compose.runtime.setValue
import androidx.compose.runtime.snapshotFlow
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.draw.clipToBounds
import androidx.compose.ui.graphics.graphicsLayer
import androidx.compose.ui.input.pointer.PointerEventPass
import androidx.compose.ui.input.pointer.pointerInput
import androidx.compose.ui.layout.onSizeChanged
import androidx.compose.ui.platform.LocalContext
import androidx.compose.ui.platform.LocalFocusManager
import androidx.compose.ui.platform.LocalResources
import androidx.compose.ui.platform.LocalSoftwareKeyboardController
import androidx.compose.ui.res.stringResource
import androidx.compose.ui.unit.dp
import androidx.lifecycle.compose.collectAsStateWithLifecycle
import io.jeph.oopbot.R
import io.jeph.oopbot.agent.AgentProcess
import io.jeph.oopbot.agent.ChatLocation
import io.jeph.oopbot.appGraph
import io.jeph.oopbot.terminal.TerminalSession
import io.jeph.oopbot.ui.chat.ChatScreen
import io.jeph.oopbot.ui.components.LocalPotsSimmer
import io.jeph.oopbot.ui.components.SnackbarStack
import io.jeph.oopbot.ui.components.SnackbarStackState
import io.jeph.oopbot.ui.files.ExplorerPanel
import io.jeph.oopbot.ui.files.FileScreen
import io.jeph.oopbot.ui.git.CommitScreen
import io.jeph.oopbot.ui.sidebar.Sidebar
import io.jeph.oopbot.ui.sidebar.SidebarLinks
import io.jeph.oopbot.ui.terminal.TerminalScreen
import io.jeph.oopbot.ui.theme.OopbotTokens
import io.jeph.oopbot.ui.theme.oopbotColors
import io.jeph.oopbot.ui.workspace.PaneControls
import io.jeph.oopbot.ui.workspace.WorkspaceTabsSheet
import io.jeph.oopbot.ui.workspace.rememberPaneItems
import io.jeph.oopbot.ui.workspace.splitScreenAvailable
import io.jeph.oopbot.workspace.Pane
import io.jeph.oopbot.workspace.PaneItem
import io.jeph.oopbot.workspace.WorkspaceTab
import kotlinx.coroutines.CoroutineScope
import kotlinx.coroutines.flow.MutableStateFlow
import kotlinx.coroutines.flow.distinctUntilChanged
import kotlinx.coroutines.flow.filter
import kotlinx.coroutines.flow.filterNotNull
import kotlinx.coroutines.launch

private enum class Panel { Sidebar, Chat, Explorer }

/** Gesture fling and programmatic slide spring for side panels, carrying swipe momentum cleanly to rest. */
private val PanelFlingSpring = spring<Float>(
  dampingRatio = Spring.DampingRatioNoBouncy,
  stiffness = Spring.StiffnessMediumLow,
)

/**
 * Each pane's sliding panels: its sidebar, its screen and its explorer, which slide within that pane's width; which
 * side each pane is on; and which pane Back goes back in first. Main thread only.
 */
@Stable
private class PanePanels(
  private val primary: AnchoredDraggableState<Panel>,
  private val secondary: AnchoredDraggableState<Panel>,
  private val swappedState: MutableState<Boolean>,
  private val activeState: MutableState<Pane>,
  private val scope: CoroutineScope,
) {
  operator fun get(pane: Pane): AnchoredDraggableState<Panel> = if (pane == Pane.Primary) primary else secondary

  /** Whether the second pane is on the main pane's side, the start, and the main pane after it. */
  val swapped: Boolean get() = swappedState.value

  /** The pane Back goes back in first: the one last touched, or brought over by a swap. */
  var active: Pane
    get() = activeState.value
    set(pane) {
      activeState.value = pane
    }

  /**
   * Swaps the panes' sides, bringing the pane beside [pane] over to its side on its screen, so what was asked for there
   * shows rather than a side panel left open over it.
   */
  fun swapFrom(pane: Pane) {
    swappedState.value = !swappedState.value
    show(pane.other, Panel.Chat)
    active = pane.other
  }

  fun show(pane: Pane, panel: Panel) {
    val state = get(pane)
    if (state.targetValue != panel) scope.launch { state.animateTo(panel, PanelFlingSpring) }
  }

  /** Shows [panel] in [pane], or the pane's screen when [panel] already shows. */
  fun toggle(pane: Pane, panel: Panel) = show(pane, if (get(pane).targetValue == panel) Panel.Chat else panel)

  /**
   * Puts the second pane back on its screen at once and after the main pane, as when split screen closes, so it opens
   * there whichever panel it showed and side it was on; Back then goes back in the main pane.
   */
  fun resetSecondary() {
    swappedState.value = false
    active = Pane.Primary
    if (secondary.currentValue != Panel.Chat) scope.launch { secondary.snapTo(Panel.Chat) }
  }

  /**
   * [pane]'s header controls; [closesSplit] when [pane] is split screen's end-side pane. The caller keeps them while
   * these stay the same, so the shown tab's actions stay in their slot.
   */
  fun controls(pane: Pane, actions: ShellActions, closesSplit: Boolean, onOpenTabs: () -> Unit) = PaneControls(
    pane = pane,
    onToggleSidebar = { toggle(pane, Panel.Sidebar) },
    onOpenTabs = onOpenTabs,
    onToggleExplorer = { toggle(pane, Panel.Explorer) },
    onCloseTab = actions::closeTab,
    onCloseSplit = { actions.closeSplit(pane) }.takeIf { closesSplit },
  )

  @Composable
  fun rememberControls(pane: Pane, actions: ShellActions, closesSplit: Boolean, onOpenTabs: () -> Unit) =
    remember(pane, this, actions, closesSplit) { controls(pane, actions, closesSplit, onOpenTabs) }

  fun isCovered(pane: Pane): Boolean = get(pane).let { it.targetValue != Panel.Chat || it.currentValue != Panel.Chat }

  fun onShownInSplit(pane: Pane, splitAvailable: Boolean): (() -> Unit)? =
    { show(Pane.Secondary, Panel.Chat) }.takeIf { pane == Pane.Primary && splitAvailable }
}

@Composable
private fun rememberPanePanels(): PanePanels {
  val primary = rememberSaveable(saver = AnchoredDraggableState.Saver()) { AnchoredDraggableState(Panel.Chat) }
  val secondary = rememberSaveable(saver = AnchoredDraggableState.Saver()) { AnchoredDraggableState(Panel.Chat) }
  val swapped = rememberSaveable { mutableStateOf(false) }
  val active = rememberSaveable { mutableStateOf(Pane.Primary) }
  val scope = rememberCoroutineScope()
  return remember(primary, secondary, swapped, active, scope) { PanePanels(primary, secondary, swapped, active, scope) }
}

/**
 * The main screen: the workspace's panes, each with its own sidebar and explorer sliding within it. Opens a chat on
 * entry when none is shown, and shows [requestedChat] in the main pane when a notification asks for it (an archived
 * one opens as its transcript instead); one-shot shell notices show as snackbars. A window too narrow for split screen,
 * such as a folded phone's, closes it. Tells [onSidePanelOpen] whether a sidebar or explorer covers a shown pane.
 * [links] are the pages the sidebar opens; a pane also opens Settings, setup and [onOpenProviders].
 */
@Composable
internal fun MainScreen(
  actions: ShellActions,
  requestedChat: MutableStateFlow<ChatLocation?>,
  links: SidebarLinks,
  onOpenProviders: () -> Unit,
  onOpenArchivedChat: (path: String) -> Unit,
  onSidePanelOpen: (Boolean) -> Unit,
) {
  val snackbar = remember { SnackbarStackState() }
  ShellNotices(actions, snackbar)
  val items = rememberPaneItems()
  val primaryEmpty = items[Pane.Primary] == null
  LaunchedEffect(actions, primaryEmpty) { if (primaryEmpty) actions.openAtStartup() }
  val splitAvailable = splitScreenAvailable()
  // Also when a restored layout fills the second pane after the window was found too narrow.
  val secondaryShown = items[Pane.Secondary] != null
  LaunchedEffect(splitAvailable, secondaryShown) {
    if (!splitAvailable && secondaryShown) actions.closeSplit(Pane.Secondary)
  }
  val panels = rememberPanePanels()
  RequestedChatEffect(actions, requestedChat, onOpenArchivedChat, onShown = {
    for (pane in Pane.entries) panels.show(pane, Panel.Chat)
  })
  val shownPanes = if (splitAvailable && secondaryShown) Pane.entries else listOf(Pane.Primary)
  val currentShownPanes by rememberUpdatedState(shownPanes)
  LaunchedEffect(panels) {
    snapshotFlow { currentShownPanes.any { panels[it].targetValue != Panel.Chat } }
      .distinctUntilChanged()
      .collect(onSidePanelOpen)
  }

  Box(modifier = Modifier.fillMaxSize()) {
    WorkspacePanes(
      actions = actions,
      splitAvailable = splitAvailable,
      panels = panels,
      snackbar = snackbar,
      onOpenSettings = links.onOpenSettings,
      onOpenProviders = onOpenProviders,
      onOpenOnboarding = links.onOpenOnboarding,
      sidebar = { pane, onShownInSplit ->
        Sidebar(
          pane = pane,
          actions = actions,
          onShownInSplit = onShownInSplit,
          onClose = { panels.show(pane, Panel.Chat) },
          links = links,
        )
      },
    )
    SnackbarStack(
      state = snackbar,
      modifier = Modifier
        .align(Alignment.BottomCenter)
        .windowInsetsPadding(WindowInsets.safeDrawing)
        .padding(OopbotTokens.Spacing.s16),
    )
  }
}

/**
 * Shows [requestedChat] in the main pane as a notification asks for it, then [onShown]; an archived one opens as its
 * transcript through [onOpenArchivedChat] instead.
 */
@Composable
private fun RequestedChatEffect(
  actions: ShellActions,
  requestedChat: MutableStateFlow<ChatLocation?>,
  onOpenArchivedChat: (path: String) -> Unit,
  onShown: () -> Unit,
) {
  val sessionArchive = LocalContext.current.appGraph.sessionArchive
  LaunchedEffect(requestedChat) {
    requestedChat.filterNotNull().collect { chat ->
      requestedChat.value = null
      when {
        sessionArchive.isArchived(chat.sessionPath) -> onOpenArchivedChat(chat.sessionPath)
        actions.openChat(Pane.Primary, chat.cwd, chat.sessionPath) -> onShown()
      }
    }
  }
}

/** Shows [actions]' notices as they arrive, alongside any already showing; an archive notice offers Undo while it shows. */
@Composable
private fun ShellNotices(actions: ShellActions, snackbar: SnackbarStackState) {
  val resources = LocalResources.current
  val genericError = stringResource(R.string.shell_error_generic)
  val undo = stringResource(R.string.archive_undo)
  LaunchedEffect(actions) {
    actions.notices.collect { notice ->
      launch {
        when (notice) {
          is ShellNotice.Failure -> snackbar.show(notice.message ?: genericError)

          is ShellNotice.Archived -> {
            val count = notice.paths.size
            val message = resources.getQuantityString(R.plurals.archive_archived, count, count)
            if (snackbar.show(message, actionLabel = undo)) actions.unarchive(notice.paths)
          }

          is ShellNotice.AutoArchived -> {
            val count = notice.paths.size
            val message = resources.getQuantityString(R.plurals.archive_auto_archived, count, count)
            if (snackbar.show(message, actionLabel = undo)) actions.unarchive(notice.paths)
          }
        }
      }
    }
  }
}

/**
 * Opening a side panel hides the keyboard immediately so it doesn't cover the panel, while refreshing the chat list
 * and remote hosts waits until the sidebar slide settles to avoid frame drops mid-transition.
 */
@Composable
private fun SidePanelOpenedEffect(state: AnchoredDraggableState<Panel>) {
  val keyboard = LocalSoftwareKeyboardController.current
  val focusManager = LocalFocusManager.current
  val graph = LocalContext.current.appGraph
  LaunchedEffect(state) {
    snapshotFlow { state.targetValue }
      .distinctUntilChanged()
      .filter { it != Panel.Chat }
      .collect {
        keyboard?.hide()
        focusManager.clearFocus(force = true)
      }
  }
  LaunchedEffect(state) {
    snapshotFlow { state.settledValue }
      .distinctUntilChanged()
      .filter { it == Panel.Sidebar }
      .collect {
        graph.sessionRepository.refresh()
        // Hosts added, renamed or connected elsewhere label their workspaces as the sidebar opens.
        graph.remoteHosts.refresh()
      }
  }
}

/**
 * Three layers as wide as the pane that slide together: the sidebar at offset 0, the pane's screen at its width and
 * the file explorer at twice it. A horizontal drag anywhere in the pane moves between neighbours; children with their
 * own horizontal scroll see the drag first and consume it, and the terminal keeps every touch that starts on it.
 */
@Composable
private fun MobilePanels(
  state: AnchoredDraggableState<Panel>,
  sidebar: @Composable () -> Unit,
  workspace: @Composable () -> Unit,
  explorer: @Composable () -> Unit,
  modifier: Modifier = Modifier,
) {
  val fling = AnchoredDraggableDefaults.flingBehavior(state, animationSpec = PanelFlingSpring)
  Box(
    modifier = modifier
      .fillMaxSize()
      .clipToBounds()
      .onSizeChanged { size ->
        state.updateAnchors(
          DraggableAnchors {
            Panel.Sidebar at 0f
            Panel.Chat at size.width.toFloat()
            Panel.Explorer at size.width * 2f
          },
          // Keep the shown panel when the width changes in place (multi-window, foldables).
          newTarget = state.targetValue,
        )
      }
      // Dragging right (towards the sidebar) lowers the offset.
      .anchoredDraggable(state, reverseDirection = true, orientation = Orientation.Horizontal, flingBehavior = fling),
  ) {
    Box(
      modifier = Modifier.fillMaxSize().graphicsLayer {
        val x = -state.offsetOr(size.width)
        translationX = x
        alpha = if (x <= -size.width || x >= size.width) 0f else 1f
      },
    ) {
      sidebar()
    }
    Box(
      modifier = Modifier.fillMaxSize().graphicsLayer {
        val x = size.width - state.offsetOr(size.width)
        translationX = x
        alpha = if (x <= -size.width || x >= size.width) 0f else 1f
      },
    ) {
      workspace()
    }
    Box(
      modifier = Modifier.fillMaxSize().graphicsLayer {
        val x = size.width * 2 - state.offsetOr(size.width)
        translationX = x
        alpha = if (x <= -size.width || x >= size.width) 0f else 1f
      },
    ) {
      explorer()
    }
  }
}

/** The offset, or the settled value's position before the first layout set the anchors. */
private fun AnchoredDraggableState<Panel>.offsetOr(width: Float): Float = offset.takeUnless { it.isNaN() }
  ?: when (currentValue) {
    Panel.Sidebar -> 0f
    Panel.Chat -> width
    Panel.Explorer -> width * 2
  }

/**
 * The main pane and, in split screen, the second pane beside it, each half as wide, each with its own [sidebar] and
 * explorer. Only the main pane opens chats in split screen, and the second pane slides back to its chat when it does.
 * Showing what the other pane shows swaps the panes' sides instead, which brings it to the side it was asked for on.
 * Only the pane on the end side, whichever it is, has the button that closes split screen from it.
 * Back shows what a pane showed before, in the pane last touched first, then in the other.
 */
@Composable
private fun WorkspacePanes(
  actions: ShellActions,
  splitAvailable: Boolean,
  panels: PanePanels,
  snackbar: SnackbarStackState,
  onOpenSettings: () -> Unit,
  onOpenProviders: () -> Unit,
  onOpenOnboarding: () -> Unit,
  sidebar: @Composable (pane: Pane, onShownInSplit: (() -> Unit)?) -> Unit,
) {
  val graph = LocalContext.current.appGraph
  val items = rememberPaneItems()
  // Shared by both panes, so a chat that moves to the other pane keeps its scroll position, parsed replies and draft.
  val views = graph.chatViews
  LaunchedEffect(views, graph.registry) { views.warmHidden(graph.registry) }
  // Not shown for the frame before a narrowed window closes split screen.
  val split = splitAvailable && items[Pane.Secondary] != null
  LaunchedEffect(split) { if (!split) panels.resetSecondary() }
  LaunchedEffect(panels.swapped) { graph.layout.setPanesSwapped(panels.swapped) }
  val shownPanes = if (split) Pane.entries else listOf(Pane.Primary)
  val endPane = if (panels.swapped) Pane.Primary else Pane.Secondary
  ClearFocusWhenTerminalLeaves(
    shownPanes.mapNotNullTo(HashSet()) { ((items[it] as? PaneItem.Tab)?.tab as? WorkspaceTab.Terminal)?.session },
  )
  PaneHistoryBack(panels, shownPanes)
  val visibleChats by remember(shownPanes, items, panels) {
    derivedStateOf {
      shownPanes
        .filter { panels[it].targetValue == Panel.Chat }
        .mapNotNullTo(HashSet()) { (items[it] as? PaneItem.Chat)?.process }
    }
  }
  ChatsOnScreen(visibleChats)
  PaneRow(panels.swapped) {
    for (pane in shownPanes) {
      if (pane == Pane.Secondary) VerticalDivider(thickness = 1.dp, color = oopbotColors.border)
      // Keyed, so each pane keeps its own state whichever panes show.
      key(pane) {
        WorkspacePaneSlot(
          pane = pane,
          item = items[pane],
          panels = panels,
          splitAvailable = splitAvailable,
          closesSplit = split && pane == endPane,
          actions = actions,
          snackbar = snackbar,
          onOpenSettings = onOpenSettings,
          onOpenProviders = onOpenProviders,
          onOpenOnboarding = onOpenOnboarding,
          sidebar = sidebar,
        )
      }
    }
  }
}

@Suppress("LongParameterList")
@Composable
private fun WorkspacePaneSlot(
  pane: Pane,
  item: PaneItem<AgentProcess>?,
  panels: PanePanels,
  splitAvailable: Boolean,
  closesSplit: Boolean,
  actions: ShellActions,
  snackbar: SnackbarStackState,
  onOpenSettings: () -> Unit,
  onOpenProviders: () -> Unit,
  onOpenOnboarding: () -> Unit,
  sidebar: @Composable (pane: Pane, onShownInSplit: (() -> Unit)?) -> Unit,
) {
  val views = LocalContext.current.appGraph.chatViews
  val panelState = panels[pane]
  val hasOpenPanel by remember(panelState) { derivedStateOf { panelState.targetValue != Panel.Chat } }
  // The session sheet belongs to the pane rather than either of its screens, so it stays open when archiving from
  // it replaces the shown chat and with it the tab shown in its place; it shows the shell's snackbar over itself.
  var tabsOpen by rememberSaveable { mutableStateOf(false) }
  val controls = panels.rememberControls(pane, actions, closesSplit) { tabsOpen = true }
  val onShownInSplit = panels.onShownInSplit(pane, splitAvailable)
  val swapPanes = { panels.swapFrom(pane) }
  BackHandler(enabled = hasOpenPanel) { panels.show(pane, Panel.Chat) }
  SidePanelOpenedEffect(panelState)
  MobilePanels(
    modifier = Modifier.onTouch { panels.active = pane },
    state = panelState,
    sidebar = {
      // The sidebar stays composed behind the pane, where its working chats' pots would redraw for nothing.
      val shimmer by remember(panelState) { derivedStateOf { panelState.targetValue == Panel.Sidebar } }
      CompositionLocalProvider(LocalPotsSimmer provides shimmer) { sidebar(pane, onShownInSplit) }
    },
    workspace = {
      // Covered until the panel has settled on the screen, so a chat picked in the sidebar comes in with it.
      WorkspacePane(
        item = item,
        isCovered = { panels.isCovered(pane) },
        actions = actions,
        views = views,
        controls = controls,
        onOpenSettings = onOpenSettings,
        onOpenProviders = onOpenProviders,
        onOpenOnboarding = onOpenOnboarding,
      )
    },
    explorer = {
      PaneExplorer(
        pane = pane,
        item = item,
        isOpen = { panelState.settledValue == Panel.Explorer },
        snackbar = snackbar,
        onSwapPanes = swapPanes,
      ) { panels.show(pane, Panel.Chat) }
    },
  )
  if (tabsOpen && item != null) {
    WorkspaceTabsSheet(pane, item.cwd, actions, snackbar, controls.shownTab, onShownInSplit, swapPanes) {
      tabsOpen = false
    }
  }
}

/**
 * Back in the panes: shows what the pane last touched showed before, else what the other showed; composed before the
 * panes, so their side panels, sheets and dialogs take Back first.
 */
@Composable
private fun PaneHistoryBack(panels: PanePanels, shownPanes: List<Pane>) {
  val graph = LocalContext.current.appGraph
  val history by graph.panes.history.collectAsStateWithLifecycle()
  BackHandler(enabled = shownPanes.any { !history[it].isNullOrEmpty() }) {
    val order = listOf(panels.active, panels.active.other).filter { it in shownPanes }
    order.firstOrNull { graph.panes.back(it) }?.let { panels.active = it }
  }
}

/** Runs [action] as a touch starts anywhere within, before the children see it and without taking it from them. */
private fun Modifier.onTouch(action: () -> Unit): Modifier = pointerInput(Unit) {
  awaitEachGesture {
    awaitFirstDown(requireUnconsumed = false, pass = PointerEventPass.Initial)
    action()
  }
}

/**
 * Removing a focused terminal view hands focus to the first text field, a chat's composer, raising the keyboard; so
 * focus is cleared whenever a terminal leaves a pane.
 */
@Composable
private fun ClearFocusWhenTerminalLeaves(terminals: Set<TerminalSession>) {
  val focus = LocalFocusManager.current
  var previous by remember { mutableStateOf(emptySet<TerminalSession>()) }
  LaunchedEffect(terminals) {
    if (!terminals.containsAll(previous)) focus.clearFocus()
    previous = terminals
  }
}
