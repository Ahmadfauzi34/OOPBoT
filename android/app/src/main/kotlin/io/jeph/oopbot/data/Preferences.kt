package io.jeph.oopbot.data

import android.content.Context
import android.content.SharedPreferences
import android.os.Build
import androidx.core.content.edit
import io.jeph.oopbot.agent.AgentRegistry
import io.jeph.oopbot.agent.IdleStop
import io.jeph.oopbot.host.GitDiffMode
import io.jeph.oopbot.localmodel.catalog.LocalModelSettings
import io.jeph.oopbot.ui.files.ExplorerSort
import io.jeph.oopbot.ui.files.SortKey
import io.jeph.oopbot.ui.git.ExplorerTab
import io.jeph.oopbot.ui.theme.AppTheme
import io.jeph.oopbot.workspace.SavedLayout
import io.jeph.oopbot.workspace.savedLayoutOf
import io.jeph.oopbot.workspace.toJson
import kotlinx.coroutines.flow.MutableStateFlow
import kotlinx.coroutines.flow.StateFlow
import kotlinx.coroutines.flow.asStateFlow
import org.json.JSONArray
import org.json.JSONException
import org.json.JSONObject

/** App settings in the `oopbot` SharedPreferences file, with large workspace/session collections stored in SQLite. */
@Suppress("TooManyFunctions") // One setter per setting, beside the readers they share.
internal class Preferences internal constructor(
  private val prefs: SharedPreferences,
  /**
   * Android's build and when OOPBoT was last installed or updated, which [localModelFailedBuilds] are kept for. The
   * install time rather than the version, since every development build has the same version.
   */
  private val softwareVersion: String,
  private val db: OopbotDatabase? = null,
) {
  constructor(context: Context, db: OopbotDatabase? = null) : this(
    prefs = context.applicationContext.getSharedPreferences(FILE, Context.MODE_PRIVATE),
    softwareVersion =
      "${Build.FINGERPRINT} ${context.packageManager.getPackageInfo(context.packageName, 0).lastUpdateTime}",
    db = db,
  )

  init {
    migrateToSqlIfNeeded()
  }

  private val setupCompleteState = MutableStateFlow(prefs.getBoolean(SETUP_COMPLETE, false))
  private val setupCommandCopiedForState = MutableStateFlow(prefs.getString(SETUP_COMMAND_COPIED_FOR, null))
  private val setupPagesPassedState = MutableStateFlow(readStringSet(SETUP_PAGES_PASSED))
  private val themeState = MutableStateFlow(readTheme())
  private val lastProjectCwdState = MutableStateFlow(prefs.getString(LAST_PROJECT_CWD, null))
  private val workspacesState = MutableStateFlow(db?.prefs?.readWorkspaces() ?: readWorkspaces())
  private val archivedState = MutableStateFlow(db?.prefs?.readArchived() ?: readLongMap(ARCHIVED_SESSIONS))
  private val touchedState = MutableStateFlow(db?.prefs?.readTouched() ?: readLongMap(TOUCHED_SESSIONS))
  private val autoArchiveState = MutableStateFlow(prefs.getBoolean(AUTO_ARCHIVE, false))
  private val autoArchiveDaysState = MutableStateFlow(
    prefs.getInt(AUTO_ARCHIVE_DAYS, AutoArchive.DEFAULT_DAYS).coerceIn(AutoArchive.DAYS),
  )
  private val showHiddenFilesState = MutableStateFlow(prefs.getBoolean(SHOW_HIDDEN_FILES, true))
  private val explorerSortState = MutableStateFlow(readExplorerSort())
  private val explorerTabsState = MutableStateFlow(
    db?.prefs?.readEnumMap(PrefsDbStore.SCOPE_EXPLORER_TAB, ExplorerTab.entries)
      ?: readEnumMap(EXPLORER_TABS, ExplorerTab.entries),
  )
  private val diffModesState = MutableStateFlow(
    db?.prefs?.readEnumMap(PrefsDbStore.SCOPE_DIFF_MODE, GitDiffMode.entries)
      ?: readEnumMap(DIFF_MODES, GitDiffMode.entries),
  )
  private val trustedRepositoriesState = MutableStateFlow(
    db?.prefs?.readTrustedRepositories() ?: readStringSet(TRUSTED_REPOSITORIES),
  )
  private val diffWrapLinesState = MutableStateFlow(prefs.getBoolean(DIFF_WRAP_LINES, false))
  private val diffHideWhitespaceState = MutableStateFlow(prefs.getBoolean(DIFF_HIDE_WHITESPACE, false))
  private val commitsCollapsedState = MutableStateFlow(prefs.getBoolean(COMMITS_COLLAPSED, true))
  private val maxRunningChatsState = MutableStateFlow(
    prefs.getInt(MAX_RUNNING_CHATS, AgentRegistry.DEFAULT_MAX_RUNNING).coerceIn(AgentRegistry.RUNNING_LIMITS),
  )
  private val idleStopState = prefs.getString(IDLE_STOP, null).let { name ->
    MutableStateFlow(IdleStop.entries.firstOrNull { it.name == name } ?: IdleStop.Default)
  }
  private val disabledExtensionsState = MutableStateFlow(readStringSet(DISABLED_EXTENSIONS))
  private val inAppNotificationsState = MutableStateFlow(prefs.getBoolean(IN_APP_NOTIFICATIONS, true))
  private val localModelsEnabledState = MutableStateFlow(prefs.getBoolean(LOCAL_MODELS_ENABLED, false))
  private val localModelFailedBuildsState = MutableStateFlow(
    if (prefs.getString(LOCAL_MODEL_FAILED_BUILDS_ON, null) == softwareVersion) {
      readStringSet(LOCAL_MODEL_FAILED_BUILDS)
    } else {
      emptySet()
    },
  )
  private val localModelIdleUnloadMinutesState = MutableStateFlow(
    prefs.getInt(LOCAL_MODEL_IDLE_UNLOAD_MINUTES, LocalModelSettings.DEFAULT_IDLE_UNLOAD_MINUTES)
      .coerceIn(LocalModelSettings.IDLE_UNLOAD_MINUTES),
  )
  private val pickerMachineState = MutableStateFlow(prefs.getString(PICKER_MACHINE, null))
  private val activeEngineState = MutableStateFlow(prefs.getString(ACTIVE_ENGINE, null))

  @Volatile
  private var remoteHostsCacheText: String? =
    if (db != null) db.prefs.readRemoteHostsCache() else prefs.getString(REMOTE_HOSTS, null)

  /** Optional external ACP engine selector (e.g. `acp:antigravity`) for new chats; null uses OOPBoT native. */
  val activeEngine: StateFlow<String?> = activeEngineState.asStateFlow()

  /** Onboarding finished. */
  val setupComplete: StateFlow<Boolean> = setupCompleteState.asStateFlow()

  /**
   * The Termux installation (`TermuxInstall.installation`) the setup command was last copied for, so that one may have
   * run it. A reinstall wiped what it set up, so a copy for another installation doesn't count.
   */
  val setupCommandCopiedFor: StateFlow<String?> = setupCommandCopiedForState.asStateFlow()

  /** Onboarding pages the user moved past without finishing them: the welcome page and skipped optional steps. */
  val setupPagesPassed: StateFlow<Set<String>> = setupPagesPassedState.asStateFlow()

  val theme: StateFlow<AppTheme> = themeState.asStateFlow()

  /** The workspace folder last opened, or last started a chat in, from the sidebar. */
  val lastProjectCwd: StateFlow<String?> = lastProjectCwdState.asStateFlow()

  /** Folders added as sidebar workspaces, newest first, kept even while they have no chats. */
  val workspaces: StateFlow<List<String>> = workspacesState.asStateFlow()

  /** Archived session files by path, each with when it was archived (epoch milliseconds). */
  val archivedSessions: StateFlow<Map<String, Long>> = archivedState.asStateFlow()

  /**
   * Session files that are not archived by path, each with when OOPBoT last showed it, restored it, or saw its run end
   * (epoch milliseconds), which keeps it from auto archive as a new message would.
   */
  val touchedSessions: StateFlow<Map<String, Long>> = touchedState.asStateFlow()

  /** Whether chats nobody touched for [autoArchiveDays] are archived on their own; off until turned on. */
  val autoArchive: StateFlow<Boolean> = autoArchiveState.asStateFlow()

  /** How many days without a touch auto archive waits, within [AutoArchive.DAYS]. */
  val autoArchiveDays: StateFlow<Int> = autoArchiveDaysState.asStateFlow()

  /** Whether the file explorer lists dot files and folders; Paseo shows them unless asked not to. */
  val showHiddenFiles: StateFlow<Boolean> = showHiddenFilesState.asStateFlow()

  /** How the file explorer orders each folder; names A to Z until changed, as in Paseo. */
  val explorerSort: StateFlow<ExplorerSort> = explorerSortState.asStateFlow()

  /** The Explorer tab last chosen in each workspace, by [folderKey]; a workspace without one shows Changes. */
  val explorerTabs: StateFlow<Map<String, ExplorerTab>> = explorerTabsState.asStateFlow()

  /** The comparison last chosen in each workspace's Changes tab, by [folderKey]; only a user's choice is kept. */
  val diffModes: StateFlow<Map<String, GitDiffMode>> = diffModesState.asStateFlow()

  /** Workspaces, by [folderKey], whose repository the user trusted git to run in. */
  val trustedRepositories: StateFlow<Set<String>> = trustedRepositoriesState.asStateFlow()

  /** Whether diffs wrap long lines rather than scroll sideways; Paseo scrolls them until asked to wrap. */
  val diffWrapLines: StateFlow<Boolean> = diffWrapLinesState.asStateFlow()

  /** Whether diffs leave out whitespace-only changes (git's `-w`). */
  val diffHideWhitespace: StateFlow<Boolean> = diffHideWhitespaceState.asStateFlow()

  /** Whether the Changes tab's Commits section is folded; Paseo starts it folded. */
  val commitsCollapsed: StateFlow<Boolean> = commitsCollapsedState.asStateFlow()

  /** How many chats may have Pi running at once, within [AgentRegistry.RUNNING_LIMITS]. */
  val maxRunningChats: StateFlow<Int> = maxRunningChatsState.asStateFlow()

  /** Whether chats that finish or wait on a dialog off screen show in the pill under the header bar. */
  val inAppNotifications: StateFlow<Boolean> = inAppNotificationsState.asStateFlow()

  /** How long a chat that is neither working nor shown keeps its Pi. */
  val idleStop: StateFlow<IdleStop> = idleStopState.asStateFlow()

  /** The extensions OOPBoT bundles that the user turned off, by [BundledExtension.id]; a chat's next Pi leaves them out. */
  val disabledExtensions: StateFlow<Set<String>> = disabledExtensionsState.asStateFlow()

  /** Whether OOPBoT offers on-device models to Pi; off until the user turns the experiment on. */
  val localModelsEnabled: StateFlow<Boolean> = localModelsEnabledState.asStateFlow()

  /**
   * The [io.jeph.oopbot.localmodel.catalog.kind]s of build, such as `litert/gpu`, that failed to load a model on this
   * phone, so OOPBoT picks another. They're forgotten when Android or OOPBoT is updated, which may fix them.
   */
  val localModelFailedBuilds: StateFlow<Set<String>> = localModelFailedBuildsState.asStateFlow()

  /** How many minutes a loaded local model may sit unused before its memory is given back. */
  val localModelIdleUnloadMinutes: StateFlow<Int> = localModelIdleUnloadMinutesState.asStateFlow()

  /** The machine the folder picker last browsed: a remote host's id, or null for the phone. */
  val pickerMachine: StateFlow<String?> = pickerMachineState.asStateFlow()

  fun setSetupComplete(complete: Boolean) {
    prefs.edit { putBoolean(SETUP_COMPLETE, complete) }
    setupCompleteState.value = complete
  }

  fun setSetupCommandCopiedFor(installation: String) {
    if (setupCommandCopiedForState.value == installation) return
    prefs.edit { putString(SETUP_COMMAND_COPIED_FOR, installation) }
    setupCommandCopiedForState.value = installation
  }

  fun passSetupPage(page: String) {
    val updated = setupPagesPassedState.value + page
    if (updated.size == setupPagesPassedState.value.size) return
    prefs.edit { putString(SETUP_PAGES_PASSED, JSONArray(updated).toString()) }
    setupPagesPassedState.value = updated
  }

  fun setTheme(theme: AppTheme) {
    prefs.edit { putString(THEME, theme.name) }
    themeState.value = theme
  }

  fun setLastProjectCwd(cwd: String?) {
    prefs.edit { putString(LAST_PROJECT_CWD, cwd) }
    lastProjectCwdState.value = cwd
  }

  /** Adds [cwd] as the newest workspace; a folder that is already one is left where it is. */
  fun addWorkspace(cwd: String) {
    val current = workspacesState.value
    if (current.any { sameFolder(it, cwd) }) return
    val key = folderKey(cwd)
    val updated = listOf(key) + current
    if (db != null) {
      db.prefs.addWorkspace(key)
    } else {
      prefs.edit { putString(WORKSPACES, JSONArray(updated).toString()) }
    }
    workspacesState.value = updated
  }

  /**
   * Takes [cwd] off the sidebar's workspaces; it shows again only while it has chats or once added back. Its
   * repository is no longer trusted, so adding it back asks again.
   */
  fun removeWorkspace(cwd: String) {
    val key = folderKey(cwd)
    val updated = workspacesState.value.filterNot { sameFolder(it, cwd) }
    val trusted = trustedRepositoriesState.value - key
    if (db != null) {
      db.prefs.removeWorkspace(key)
    } else {
      prefs.edit {
        putString(WORKSPACES, JSONArray(updated).toString())
        putString(TRUSTED_REPOSITORIES, JSONArray(trusted).toString())
      }
    }
    workspacesState.value = updated
    trustedRepositoriesState.value = trusted
  }

  /** Marks [paths] archived at [at]; a path already archived keeps its time. Their touches go with them. */
  fun archiveSessions(paths: Collection<String>, at: Long) {
    val pathSet = paths.toSet()
    val updatedArchived = archivedState.value + paths.filterNot(archivedState.value::containsKey).associateWith { at }
    val updatedTouched = touchedState.value - pathSet
    if (db != null) {
      db.prefs.archiveSessions(pathSet, at)
      archivedState.value = updatedArchived
      touchedState.value = updatedTouched
    } else {
      writeArchived(updatedArchived, updatedTouched)
    }
  }

  fun unarchiveSessions(paths: Collection<String>) {
    val pathSet = paths.toSet()
    val updatedArchived = archivedState.value - pathSet
    if (db != null) {
      db.prefs.unarchiveSessions(pathSet)
      archivedState.value = updatedArchived
    } else {
      writeArchived(updatedArchived, touchedState.value)
    }
  }

  /** Records that [paths] were touched at [at]. */
  fun touchSessions(paths: Collection<String>, at: Long) {
    if (paths.isEmpty()) return
    val pathSet = paths.toSet()
    val updated = touchedState.value + pathSet.associateWith { at }
    if (db != null) {
      db.prefs.touchSessions(pathSet, at)
    } else {
      prefs.edit { putString(TOUCHED_SESSIONS, JSONObject(updated).toString()) }
    }
    touchedState.value = updated
  }

  fun setAutoArchive(enabled: Boolean) {
    prefs.edit { putBoolean(AUTO_ARCHIVE, enabled) }
    autoArchiveState.value = enabled
  }

  /** Sets [autoArchiveDays], kept within [AutoArchive.DAYS]. */
  fun setAutoArchiveDays(days: Int) {
    val allowed = days.coerceIn(AutoArchive.DAYS)
    prefs.edit { putInt(AUTO_ARCHIVE_DAYS, allowed) }
    autoArchiveDaysState.value = allowed
  }

  fun setShowHiddenFiles(show: Boolean) {
    prefs.edit { putBoolean(SHOW_HIDDEN_FILES, show) }
    showHiddenFilesState.value = show
  }

  fun setExplorerSort(sort: ExplorerSort) {
    prefs.edit {
      putString(EXPLORER_SORT_KEY, sort.key.name)
      putBoolean(EXPLORER_SORT_DESCENDING, sort.descending)
    }
    explorerSortState.value = sort
  }

  fun setExplorerTab(cwd: String, tab: ExplorerTab) {
    val key = folderKey(cwd)
    val updated = explorerTabsState.value + (key to tab)
    if (db != null) {
      db.prefs.setExplorerTab(key, tab)
    } else {
      prefs.edit { putString(EXPLORER_TABS, JSONObject(updated.mapValues { it.value.name }).toString()) }
    }
    explorerTabsState.value = updated
  }

  fun setDiffMode(cwd: String, mode: GitDiffMode) {
    val key = folderKey(cwd)
    val updated = diffModesState.value + (key to mode)
    if (db != null) {
      db.prefs.setDiffMode(key, mode)
    } else {
      prefs.edit { putString(DIFF_MODES, JSONObject(updated.mapValues { it.value.name }).toString()) }
    }
    diffModesState.value = updated
  }

  fun trustRepository(cwd: String) {
    val key = folderKey(cwd)
    val updated = trustedRepositoriesState.value + key
    if (db != null) {
      db.prefs.trustRepository(key)
    } else {
      prefs.edit { putString(TRUSTED_REPOSITORIES, JSONArray(updated).toString()) }
    }
    trustedRepositoriesState.value = updated
  }

  fun setDiffWrapLines(wrap: Boolean) {
    prefs.edit { putBoolean(DIFF_WRAP_LINES, wrap) }
    diffWrapLinesState.value = wrap
  }

  fun setDiffHideWhitespace(hide: Boolean) {
    prefs.edit { putBoolean(DIFF_HIDE_WHITESPACE, hide) }
    diffHideWhitespaceState.value = hide
  }

  fun setCommitsCollapsed(collapsed: Boolean) {
    prefs.edit { putBoolean(COMMITS_COLLAPSED, collapsed) }
    commitsCollapsedState.value = collapsed
  }

  /** Sets [maxRunningChats], kept within [AgentRegistry.RUNNING_LIMITS]. */
  fun setMaxRunningChats(limit: Int) {
    val allowed = limit.coerceIn(AgentRegistry.RUNNING_LIMITS)
    prefs.edit { putInt(MAX_RUNNING_CHATS, allowed) }
    maxRunningChatsState.value = allowed
  }

  fun setInAppNotifications(enabled: Boolean) {
    prefs.edit { putBoolean(IN_APP_NOTIFICATIONS, enabled) }
    inAppNotificationsState.value = enabled
  }

  fun setIdleStop(stop: IdleStop) {
    prefs.edit { putString(IDLE_STOP, stop.name) }
    idleStopState.value = stop
  }

  fun setExtensionEnabled(extension: BundledExtension, enabled: Boolean) {
    val updated = if (enabled) {
      disabledExtensionsState.value - extension.id
    } else {
      disabledExtensionsState.value + extension.id
    }
    prefs.edit { putString(DISABLED_EXTENSIONS, JSONArray(updated).toString()) }
    disabledExtensionsState.value = updated
  }

  fun setLocalModelsEnabled(enabled: Boolean) {
    prefs.edit { putBoolean(LOCAL_MODELS_ENABLED, enabled) }
    localModelsEnabledState.value = enabled
  }

  fun setActiveEngine(engine: String?) {
    val clean = engine?.trim()?.takeIf { it.isNotEmpty() }
    prefs.edit {
      if (clean == null) remove(ACTIVE_ENGINE) else putString(ACTIVE_ENGINE, clean)
    }
    activeEngineState.value = clean
  }

  /**
   * The name OpenCL gives the phone's GPU, empty when there's no driver, as found under this Android build; null when
   * it hasn't been looked up since Android was updated, which may bring a new driver.
   */
  fun localModelGpu(): String? = prefs.getString(LOCAL_MODEL_GPU, null)?.takeIf {
    prefs.getString(LOCAL_MODEL_GPU_ON, null) == Build.FINGERPRINT
  }

  /** Saves the [localModelGpu] [name] found under this Android build. Called from any thread. */
  fun setLocalModelGpu(name: String) {
    prefs.edit {
      putString(LOCAL_MODEL_GPU, name)
      putString(LOCAL_MODEL_GPU_ON, Build.FINGERPRINT)
    }
  }

  /** Adds [kind] to [localModelFailedBuilds]. */
  fun addLocalModelFailedBuild(kind: String) {
    val updated = localModelFailedBuildsState.value + kind
    prefs.edit {
      putString(LOCAL_MODEL_FAILED_BUILDS, JSONArray(updated).toString())
      putString(LOCAL_MODEL_FAILED_BUILDS_ON, softwareVersion)
    }
    localModelFailedBuildsState.value = updated
  }

  /** Sets [localModelIdleUnloadMinutes], kept within [LocalModelSettings.IDLE_UNLOAD_MINUTES]. */
  fun setLocalModelIdleUnloadMinutes(minutes: Int) {
    val allowed = minutes.coerceIn(LocalModelSettings.IDLE_UNLOAD_MINUTES)
    prefs.edit { putInt(LOCAL_MODEL_IDLE_UNLOAD_MINUTES, allowed) }
    localModelIdleUnloadMinutesState.value = allowed
  }

  fun setPickerMachine(hostId: String?) {
    if (pickerMachineState.value == hostId) return
    prefs.edit { putString(PICKER_MACHINE, hostId) }
    pickerMachineState.value = hostId
  }

  /**
   * The last `remote_hosts` result, so remote workspaces show their host and path as soon as OOPBoT starts, before
   * Termux has answered; null before the first.
   */
  fun remoteHostsCache(): String? = remoteHostsCacheText

  fun setRemoteHostsCache(json: String) {
    remoteHostsCacheText = json
    if (db != null) {
      db.prefs.setRemoteHostsCache(json)
    } else {
      prefs.edit { putString(REMOTE_HOSTS, json) }
    }
  }

  /** The tabs and panes last shown, for a restarted OOPBoT to show again; null before any were saved. */
  fun savedLayout(): SavedLayout? = prefs.getString(LAYOUT, null)?.let(::savedLayoutOf)

  fun setSavedLayout(layout: SavedLayout) {
    prefs.edit { putString(LAYOUT, layout.toJson()) }
  }

  /** Moves legacy JSON blobs from SharedPreferences XML into SQLite once and removes those keys from XML. */
  private fun migrateToSqlIfNeeded() {
    val store = db?.prefs ?: return
    if (store.isMigrated(PREFS_MIGRATED_KEY)) return
    val legacy = LegacyPrefsData(
      workspaces = readWorkspaces(),
      archived = readLongMap(ARCHIVED_SESSIONS),
      touched = readLongMap(TOUCHED_SESSIONS),
      explorerTabs = readEnumMap(EXPLORER_TABS, ExplorerTab.entries),
      diffModes = readEnumMap(DIFF_MODES, GitDiffMode.entries),
      trustedRepositories = readStringSet(TRUSTED_REPOSITORIES),
      remoteHostsCache = prefs.getString(REMOTE_HOSTS, null),
    )
    store.migrateLegacy(PREFS_MIGRATED_KEY, legacy)
    prefs.edit(commit = true) {
      remove(WORKSPACES)
      remove(ARCHIVED_SESSIONS)
      remove(TOUCHED_SESSIONS)
      remove(EXPLORER_TABS)
      remove(DIFF_MODES)
      remove(TRUSTED_REPOSITORIES)
      remove(REMOTE_HOSTS)
    }
  }

  /** The saved theme. Before there were palettes to pick, the setting was Light or Dark, which kept their colours. */
  private fun readTheme(): AppTheme = when (val name = prefs.getString(THEME, null)) {
    "Light" -> AppTheme.Porcelain
    "Dark" -> AppTheme.Cobalt
    else -> AppTheme.entries.firstOrNull { it.name == name } ?: AppTheme.System
  }

  private fun readWorkspaces(): List<String> = try {
    val array = JSONArray(prefs.getString(WORKSPACES, null) ?: return emptyList())
    List(array.length()) { array.getString(it) }
  } catch (_: JSONException) {
    emptyList()
  }

  private fun readExplorerSort(): ExplorerSort {
    val key = SortKey.entries.firstOrNull { it.name == prefs.getString(EXPLORER_SORT_KEY, null) }
      ?: return ExplorerSort.Default
    return ExplorerSort(key, prefs.getBoolean(EXPLORER_SORT_DESCENDING, key.descendingFirst))
  }

  /** The JSON array of strings at [key], as a set. */
  private fun readStringSet(key: String): Set<String> = try {
    val array = JSONArray(prefs.getString(key, null) ?: return emptySet())
    List(array.length()) { array.getString(it) }.toSet()
  } catch (_: JSONException) {
    emptySet()
  }

  /**
   * The JSON object at [key] of folder keys or model ids to names of [entries]; names no entry has any more are
   * dropped.
   */
  private fun <T : Enum<T>> readEnumMap(key: String, entries: List<T>): Map<String, T> = try {
    val json = JSONObject(prefs.getString(key, null) ?: return emptyMap())
    json.keys().asSequence()
      .mapNotNull { folder -> entries.firstOrNull { it.name == json.optString(folder) }?.let { folder to it } }
      .toMap()
  } catch (_: JSONException) {
    emptyMap()
  }

  private fun writeArchived(archived: Map<String, Long>, touched: Map<String, Long>) {
    prefs.edit {
      putString(ARCHIVED_SESSIONS, JSONObject(archived).toString())
      putString(TOUCHED_SESSIONS, JSONObject(touched).toString())
    }
    archivedState.value = archived
    touchedState.value = touched
  }

  /** The JSON object at [key] of strings to epoch milliseconds. */
  private fun readLongMap(key: String): Map<String, Long> = try {
    val json = JSONObject(prefs.getString(key, null) ?: return emptyMap())
    json.keys().asSequence().associateWith(json::getLong)
  } catch (_: JSONException) {
    emptyMap()
  }

  private companion object {
    const val FILE = "oopbot"
    const val PREFS_MIGRATED_KEY = "prefs_v1_migrated"
    const val SETUP_COMPLETE = "setupComplete"

    const val SETUP_COMMAND_COPIED_FOR = "setupCommandCopiedFor"
    const val SETUP_PAGES_PASSED = "setupPagesPassed"
    const val THEME = "theme"
    const val LAST_PROJECT_CWD = "lastProjectCwd"
    const val WORKSPACES = "workspaces"
    const val ARCHIVED_SESSIONS = "archivedSessions"
    const val TOUCHED_SESSIONS = "touchedSessions"
    const val AUTO_ARCHIVE = "autoArchive"
    const val AUTO_ARCHIVE_DAYS = "autoArchiveDays"
    const val SHOW_HIDDEN_FILES = "showHiddenFiles"
    const val EXPLORER_SORT_KEY = "explorerSortKey"
    const val EXPLORER_SORT_DESCENDING = "explorerSortDescending"
    const val EXPLORER_TABS = "explorerTabs"
    const val DIFF_MODES = "diffModes"
    const val TRUSTED_REPOSITORIES = "trustedRepositories"
    const val DIFF_WRAP_LINES = "diffWrapLines"
    const val DIFF_HIDE_WHITESPACE = "diffHideWhitespace"
    const val COMMITS_COLLAPSED = "commitsCollapsed"
    const val MAX_RUNNING_CHATS = "maxRunningChats"
    const val IDLE_STOP = "idleStop"
    const val LAYOUT = "layout"
    const val DISABLED_EXTENSIONS = "disabledExtensions"
    const val IN_APP_NOTIFICATIONS = "inAppNotifications"
    const val LOCAL_MODELS_ENABLED = "localModelsEnabled"
    const val LOCAL_MODEL_FAILED_BUILDS = "localModelFailedBuilds"
    const val LOCAL_MODEL_FAILED_BUILDS_ON = "localModelFailedBuildsOn"
    const val LOCAL_MODEL_GPU = "localModelGpu"
    const val LOCAL_MODEL_GPU_ON = "localModelGpuOn"
    const val LOCAL_MODEL_IDLE_UNLOAD_MINUTES = "localModelIdleUnloadMinutes"
    const val PICKER_MACHINE = "pickerMachine"
    const val REMOTE_HOSTS = "remoteHosts"
    const val ACTIVE_ENGINE = "activeEngine"
  }
}
