package io.jeph.oopbot

import android.app.Application
import android.content.Context
import androidx.lifecycle.DefaultLifecycleObserver
import androidx.lifecycle.Lifecycle
import androidx.lifecycle.LifecycleOwner
import androidx.lifecycle.ProcessLifecycleOwner
import io.jeph.oopbot.agent.AgentProcess
import io.jeph.oopbot.agent.AgentProcessState
import io.jeph.oopbot.agent.AgentRegistry
import io.jeph.oopbot.agent.AgentService
import io.jeph.oopbot.agent.AppVisibility
import io.jeph.oopbot.agent.ChatInbox
import io.jeph.oopbot.agent.DialogNotifications
import io.jeph.oopbot.agent.KeepAlive
import io.jeph.oopbot.agent.KeepAliveMode
import io.jeph.oopbot.agent.TurnNotifications
import io.jeph.oopbot.data.AccountRepository
import io.jeph.oopbot.data.ConfigBackups
import io.jeph.oopbot.data.ExplorerCache
import io.jeph.oopbot.data.ModelDefaults
import io.jeph.oopbot.data.NoticeStore
import io.jeph.oopbot.data.OopbotDatabase
import io.jeph.oopbot.data.PiExtensionUpdate
import io.jeph.oopbot.data.Preferences
import io.jeph.oopbot.data.RemoteHosts
import io.jeph.oopbot.data.RemotePasswords
import io.jeph.oopbot.data.SessionArchive
import io.jeph.oopbot.data.SessionRepository
import io.jeph.oopbot.files.FileTabRegistry
import io.jeph.oopbot.git.CommitTabRegistry
import io.jeph.oopbot.host.FileHost
import io.jeph.oopbot.host.GitHost
import io.jeph.oopbot.host.HostClient
import io.jeph.oopbot.localmodel.LocalModels
import io.jeph.oopbot.terminal.TerminalRegistry
import io.jeph.oopbot.ui.chat.ChatViews
import io.jeph.oopbot.ui.files.ExplorerTrees
import io.jeph.oopbot.ui.git.GitWorkspaces
import io.jeph.oopbot.ui.sidebar.folderNames
import io.jeph.oopbot.workspace.OpenTabs
import io.jeph.oopbot.workspace.Panes
import io.jeph.oopbot.workspace.WorkspaceLayout
import java.io.File
import kotlinx.coroutines.CoroutineScope
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.ExperimentalCoroutinesApi
import kotlinx.coroutines.SupervisorJob
import kotlinx.coroutines.flow.MutableStateFlow
import kotlinx.coroutines.flow.combine
import kotlinx.coroutines.flow.distinctUntilChanged
import kotlinx.coroutines.flow.filter
import kotlinx.coroutines.flow.flatMapLatest
import kotlinx.coroutines.flow.flowOf
import kotlinx.coroutines.flow.map
import kotlinx.coroutines.flow.merge
import kotlinx.coroutines.launch

/** The app's long-lived objects, built once by [OopbotApplication]. Reach it with [Context.appGraph]. */
internal class AppGraph(private val application: Application) {
  /** Lives as long as the process; main-thread confined. */
  val scope = CoroutineScope(SupervisorJob() + Dispatchers.Main.immediate)
  val keepAlive = KeepAlive(application)
  val hostClient = HostClient(
    application,
    foreground = ProcessLifecycleOwner.get().lifecycle.currentStateFlow.map { it.isAtLeast(Lifecycle.State.STARTED) },
    scope = scope,
    canLaunchInBackground = keepAlive::awaitActiveApplied,
  )
  val database = OopbotDatabase.open(application)
  val preferences = Preferences(application, database)
  val remoteHosts = RemoteHosts(hostClient, preferences, RemotePasswords.of(application), keepAlive, scope)

  /** What the chat screens keep of each chat, such as unsent drafts, backed by SQLite across process restarts. */
  val chatViews = ChatViews(database)
  val sessionRepository = SessionRepository(
    hostClient = hostClient,
    notices = NoticeStore(db = database, dir = File(application.filesDir, "notices")),
    scope = scope,
    db = database,
    onSessionsDeleted = chatViews::forget,
  )
  val sessionArchive = SessionArchive(sessionRepository, preferences, scope)
  private val visibility = AppVisibility(application)

  /** The chats the user sees: those the panes show while OOPBoT is on screen and nothing covers them. */
  val chatsOnScreen = MutableStateFlow<Set<AgentProcess>>(emptySet())
  val chatInbox = ChatInbox(database)
  private val turnNotifications = TurnNotifications(application, visibility, chatInbox) { folderNames() }
  val registry: AgentRegistry<AgentProcess> =
    AgentRegistry(
      scope,
      maxRunning = preferences.maxRunningChats,
      idleParkMillis = preferences.idleStop.map { it.millis },
      onWorkEnded = { process ->
        turnNotifications.workEnded(process)
        // Pi saves the run's messages a moment before the session list hears of them.
        process.sessionFile.value?.let { sessionArchive.touch(listOf(it)) }
      },
    ) { cwd, sessionPath, options ->
      AgentProcess(
        hostClient,
        keepAlive,
        cwd,
        sessionPath,
        scope,
        sessionRepository,
        remoteHosts,
        preferences.disabledExtensions,
        preferences.activeEngine,
        launchOptions = options,
      )
    }
  val accountRepository = AccountRepository(hostClient, registry, keepAlive)
  val configBackups = ConfigBackups(hostClient, registry, keepAlive, accountRepository, sessionRepository, scope)
  val piExtensionUpdate = PiExtensionUpdate(hostClient, registry, keepAlive, sessionRepository, scope)
  val modelDefaults = ModelDefaults(hostClient)
  val panes = Panes(registry, sessionArchive::isArchived)
  val tabs = OpenTabs(panes)
  val terminals = TerminalRegistry(application, hostClient, scope, tabs)
  val fileHost = FileHost(hostClient, scope, remoteHosts::isRemote)
  val files = FileTabRegistry(fileHost, tabs)
  val gitHost = GitHost(hostClient, scope)
  private val explorerCache = ExplorerCache(File(application.cacheDir, "explorer_cache"))
  val explorerTrees = ExplorerTrees(fileHost, explorerCache, scope)
  val gitWorkspaces = GitWorkspaces(gitHost, explorerCache, scope)
  val commits = CommitTabRegistry(gitHost, tabs, preferences.trustedRepositories, scope)
  val layout = WorkspaceLayout(this)
  val localModels = LocalModels(application, scope, keepAlive, accountRepository, preferences)

  /** The session files of the chats on screen, which Pi can name late. */
  @OptIn(ExperimentalCoroutinesApi::class)
  private val sessionFilesOnScreen = chatsOnScreen.flatMapLatest { chats ->
    if (chats.isEmpty()) flowOf(emptySet()) else combine(chats.map { it.sessionFile }) { it.filterNotNullTo(HashSet()) }
  }

  private var activeHold: AutoCloseable? = null
  private var standbyHold: AutoCloseable? = null
  private var lastStandbyPolicy: Boolean? = null

  init {
    holdWhileWorking()
    restartChatsOnceTermuxWorks()
    feedChatInbox()
    touchChatsOnScreen()
    endLocalModelLeases()
    val dialogNotifications =
      DialogNotifications(application, registry, chatsOnScreen, visibility, chatInbox) { folderNames() }
    scope.launch { dialogNotifications.watch() }
    ProcessLifecycleOwner.get().lifecycle.addObserver(
      object : DefaultLifecycleObserver {
        override fun onStart(owner: LifecycleOwner) {
          syncStandby("foreground")
          // Before setup, Termux may not be able to run the host yet.
          if (preferences.setupComplete.value) {
            sessionRepository.refresh()
            remoteHosts.refresh()
          }
          // Events may have gone missing while OOPBoT was in the background.
          for (process in registry.processes.value.values) scope.launch { process.resync() }
        }
      },
    )
  }

  /**
   * Synchronizes the current power-tier holds and retries any previously refused [KeepAlive] transition.
   * Called on foreground return and from [io.jeph.oopbot.agent.StandbyBootReceiver.onReceive] so boot FGS starts
   * execute inside the broadcast receiver exemption window instead of during `Application.onCreate`.
   */
  internal fun syncStandby(reason: String) {
    applyWorkPowerState(currentWorkPowerState(), dispatch = true)
    keepAlive.sync(reason)
  }

  /**
   * Records the Standby hold without issuing a recursive service start when [AgentService] is already restarting
   * itself via a `START_STICKY` null-intent delivery.
   */
  internal fun ensureStandbyHoldForRestart() {
    applyWorkPowerState(currentWorkPowerState(), dispatch = false)
  }

  /** Keeps [chatInbox] following the setting, the archived chats, and the chats on screen. */
  private fun feedChatInbox() {
    scope.launch { preferences.inAppNotifications.collect { chatInbox.enabled = it } }
    scope.launch { preferences.archivedSessions.collect { chatInbox.setArchived(it.keys) } }
    scope.launch { sessionFilesOnScreen.collect(chatInbox::setOnScreen) }
  }

  /** Touches each chat as it comes on screen and as it leaves, so auto archive counts from when the user last saw it. */
  private fun touchChatsOnScreen() {
    scope.launch {
      var previous = emptySet<String>()
      sessionFilesOnScreen.collect { files ->
        sessionArchive.touch(previous + files)
        previous = files
      }
    }
  }

  /**
   * A host that connects after a failure fixed in Termux (a setup step, or a helper that wasn't running) proves the
   * fix, so chats that failed on it start again instead of waiting on Retry.
   */
  private fun restartChatsOnceTermuxWorks() {
    scope.launch {
      var termuxBroken = false
      hostClient.lastFailure.collect { failure ->
        if (failure != null) {
          termuxBroken = termuxBroken || failure.fixedInTermux
          return@collect
        }
        if (!termuxBroken) return@collect
        termuxBroken = false
        for (process in registry.processes.value.values) {
          val state = process.state.value
          if (state is AgentProcessState.Failed && state.failure?.fixedInTermux == true) process.restart()
        }
      }
    }
  }

  /**
   * Splits [KeepAlive] into two power tiers so OOPBoT stays reliable without draining battery:
   * - Active (foreground + partial wake lock) while a chat turn (plus its brief post-turn handoff grace when no
   *   background tasks are running) or a terminal command is running.
   * - Standby (foreground, 0 wake lock) while waiting on background tasks or when Background Standby is enabled.
   * Launched on non-immediate [Dispatchers.Main] so cold starts on boot/restart never invoke `startForegroundService`
   * synchronously inside `Application.onCreate`. Acquires new holds before releasing old ones so transitions between
   * Active and Standby never dip to Off.
   */
  private fun holdWhileWorking() {
    scope.launch(Dispatchers.Main) {
      val standbyAllowed = combine(preferences.backgroundStandby, preferences.setupComplete) { standby, done ->
        standby && done
      }
      combine(
        registry.busy,
        registry.waitingOnBackground,
        registry.holding,
        terminals.busy,
        standbyAllowed,
      ) { chatsBusy, chatsWaitingOnBackground, chatsHolding, terminalsBusy, standby ->
        WorkPowerState(chatsBusy, chatsWaitingOnBackground, chatsHolding, terminalsBusy, standby)
      }.distinctUntilChanged().collect { applyWorkPowerState(it) }
    }
  }

  private fun currentWorkPowerState(): WorkPowerState = WorkPowerState(
    chatsBusy = registry.busy.value,
    chatsWaitingOnBackground = registry.waitingOnBackground.value,
    chatsHolding = registry.holding.value,
    terminalsBusy = terminals.busy.value,
    standby = preferences.backgroundStandby.value && preferences.setupComplete.value,
  )

  private fun applyWorkPowerState(state: WorkPowerState, dispatch: Boolean = true) {
    val needsActive = state.needsActive
    val needsStandby = state.needsStandby
    val previousMode = keepAlive.mode
    if (needsActive && activeHold == null) {
      activeHold = keepAlive.acquire(state.activeReason)
    }
    if (needsStandby && standbyHold == null) {
      standbyHold = keepAlive.acquireStandby(state.standbyReason, dispatch = dispatch)
    }
    if (!needsActive) {
      activeHold?.close()
      activeHold = null
    }
    if (!needsStandby) {
      standbyHold?.close()
      standbyHold = null
    }
    val standbyPolicyChanged = lastStandbyPolicy != null && lastStandbyPolicy != state.standby
    lastStandbyPolicy = state.standby
    if (dispatch && standbyPolicyChanged && keepAlive.mode != KeepAliveMode.Off && keepAlive.mode == previousMode) {
      keepAlive.sync("background standby policy", force = true)
    }
    AgentService.refresh(application)
  }

  private data class WorkPowerState(
    val chatsBusy: Boolean,
    val chatsWaitingOnBackground: Boolean,
    val chatsHolding: Boolean,
    val terminalsBusy: Boolean,
    val standby: Boolean,
  ) {
    val needsActive: Boolean get() = chatsBusy || terminalsBusy || (chatsHolding && !chatsWaitingOnBackground)
    val needsStandby: Boolean get() = standby || (chatsWaitingOnBackground && !chatsBusy)
    val activeReason: String get() = when {
      chatsBusy -> "chats working"
      terminalsBusy -> "terminal command"
      else -> "chat handoff grace"
    }
    val standbyReason: String get() =
      if (chatsWaitingOnBackground) "waiting on background tasks" else "background standby"
  }

  /**
   * A chat keeps the local model reserved between the requests of its agent turn; when the turn ends, the model is
   * free for the next chat at once.
   */
  @OptIn(ExperimentalCoroutinesApi::class)
  private fun endLocalModelLeases() {
    scope.launch {
      registry.processes
        .flatMapLatest { processes ->
          processes.values.map { process -> process.busy.filter { !it }.map { process.sessionId.value } }.merge()
        }
        .collect { session -> session?.let(localModels.runtime::endLease) }
    }
  }
}

internal val Context.appGraph: AppGraph get() = (applicationContext as OopbotApplication).graph
