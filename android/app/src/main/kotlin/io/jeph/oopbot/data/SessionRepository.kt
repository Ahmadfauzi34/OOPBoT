package io.jeph.oopbot.data

import android.util.Log
import io.jeph.oopbot.agent.ExtensionWidget
import io.jeph.oopbot.agent.KeptBySession
import io.jeph.oopbot.host.HostRejected
import io.jeph.oopbot.host.SessionInfo
import java.io.IOException
import kotlinx.coroutines.CoroutineScope
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.channels.BufferOverflow
import kotlinx.coroutines.channels.Channel
import kotlinx.coroutines.flow.MutableSharedFlow
import kotlinx.coroutines.flow.MutableStateFlow
import kotlinx.coroutines.flow.SharedFlow
import kotlinx.coroutines.flow.StateFlow
import kotlinx.coroutines.flow.asSharedFlow
import kotlinx.coroutines.flow.asStateFlow
import kotlinx.coroutines.flow.receiveAsFlow
import kotlinx.coroutines.launch
import kotlinx.coroutines.withContext
import org.json.JSONArray
import org.json.JSONException

/**
 * Host RPC operations needed by [SessionRepository], abstracted from [io.jeph.oopbot.host.HostClient] for unit tests.
 */
internal interface SessionHost {
  suspend fun sessions(): List<SessionInfo>

  suspend fun deleteSessions(paths: List<String>)

  suspend fun sessionMessages(path: String): JSONArray
}

/**
 * Pi's session list, newest first. Seeded on cold start from SQLite `session_index` so the sidebar and archive
 * render immediately while Termux starts, and reconciled incrementally on each host refresh. [loaded] stays false and
 * [listings] stays quiet until a live host refresh completes.
 */
internal class SessionRepository(
  private val hostClient: SessionHost,
  /** Each session's extension notices, which Pi does not keep. */
  val notices: NoticeStore,
  scope: CoroutineScope,
  private val db: OopbotDatabase? = null,
  private val onSessionsDeleted: (Collection<String>) -> Unit = {},
) {
  private val sessionsState = MutableStateFlow(db?.sessions?.list().orEmpty())
  private val loadedState = MutableStateFlow(false)
  private val listingsFlow = MutableSharedFlow<List<SessionInfo>>(
    extraBufferCapacity = 1,
    onBufferOverflow = BufferOverflow.DROP_OLDEST,
  )
  private val requests = Channel<Unit>(Channel.CONFLATED)

  /** The extension titles of sessions whose Pi stopped, which a later Pi on the session starts with. */
  val extensionTitles = KeptBySession<String>()

  /** The extension statuses of sessions whose Pi stopped, by key, which a later Pi on the session starts with. */
  val extensionStatuses = KeptBySession<Map<String, String>>()

  /** The extension widgets of sessions whose Pi stopped, by key, which a later Pi on the session starts with. */
  val extensionWidgets = KeptBySession<Map<String, ExtensionWidget>>()

  /** Forgets every session's kept extension UI, for when the extensions loaded change and wouldn't clear it. */
  fun forgetExtensionUi() {
    extensionTitles.forgetAll()
    extensionStatuses.forgetAll()
    extensionWidgets.forgetAll()
  }

  val sessions: StateFlow<List<SessionInfo>> = sessionsState.asStateFlow()

  /** True once the first live refresh finished, successfully or not. */
  val loaded: StateFlow<Boolean> = loadedState.asStateFlow()

  /** Each list a refresh reads from the host, as it arrives; a failed refresh sends none, and none are replayed. */
  val listings: SharedFlow<List<SessionInfo>> = listingsFlow.asSharedFlow()

  init {
    scope.launch { requests.receiveAsFlow().collect { load() } }
  }

  /** Requests a refresh; requests made while one runs coalesce into a single follow-up refresh. */
  fun refresh() {
    requests.trySend(Unit)
  }

  /** Refreshes when [path], a session Pi just saved a message to, isn't listed yet. */
  fun refreshIfUnlisted(path: String) {
    if (sessionsState.value.none { it.path == path }) refresh()
  }

  /** The last host-reported modification timestamp for [path], or null if not indexed yet. */
  fun knownModified(path: String): Long? = sessionsState.value.firstOrNull { it.path == path }?.modified

  /**
   * Deletes the session files and purges their SQLite metadata, transcripts, FTS rows, drafts, inbox alerts, and
   * notices; throws [HostFailure][io.jeph.oopbot.host.HostFailure] or [HostRejected].
   */
  suspend fun delete(paths: Collection<String>) {
    val target = paths.distinct()
    val targetSet = target.toSet()
    hostClient.deleteSessions(target)
    notices.delete(target)
    db?.sessions?.purge(target)
    onSessionsDeleted(target)
    sessionsState.value = sessionsState.value.filterNot { it.path in targetSet }
    refresh()
  }

  /**
   * Returns [path]'s messages, serving SQLite `message_cache` immediately when [preferCache] is true and
   * `synced_modified` matches the host's [knownModified], and falling back to cached messages if Termux is unreachable.
   * Freshly fetched transcripts are stored and indexed for FTS search.
   */
  suspend fun messages(path: String, preferCache: Boolean = true): JSONArray {
    val store = db?.sessions
    val knownModified = knownModified(path)
    val cached = if (preferCache && store != null) withContext(Dispatchers.IO) { store.cachedMessages(path) } else null
    if (cached != null && knownModified != null && cached.syncedModified == knownModified) {
      parseCachedArray(cached.messagesJson)?.let { return it }
    }
    val fetched = try {
      hostClient.sessionMessages(path)
    } catch (error: IOException) {
      val fallback = cached?.takeIf { preferCache }?.let { parseCachedArray(it.messagesJson) }
      return fallback ?: throw error
    } catch (error: HostRejected) {
      if (error.code != NOT_FOUND && preferCache) {
        cached?.let { parseCachedArray(it.messagesJson) }?.let { return it }
      }
      throw error
    }
    if (store != null) {
      val json = fetched.toString()
      val body = withContext(Dispatchers.Default) { TranscriptIndexer.extractBody(fetched) }
      withContext(Dispatchers.IO) {
        store.saveMessagesSync(path, knownModified ?: 0L, json, body)
      }
    }
    return fetched
  }

  /** Caches and FTS-indexes messages already fetched by a live chat without extra Termux IPC. */
  suspend fun cacheMessages(path: String, messages: JSONArray, syncedModified: Long = 0L) {
    val store = db?.sessions ?: return
    if (path.isBlank()) return
    val json = messages.toString()
    val body = withContext(Dispatchers.Default) { TranscriptIndexer.extractBody(messages) }
    store.saveMessagesAsync(path, syncedModified, json, body)
  }

  /** Returns session paths among [paths] whose indexed transcript matches [query] via FTS4. */
  suspend fun searchMessages(query: String, paths: Set<String>): Set<String> {
    val store = db?.sessions ?: return emptySet()
    if (query.isBlank() || paths.isEmpty()) return emptySet()
    return withContext(Dispatchers.IO) { store.searchMessages(query, paths) }
  }

  /**
   * Reads the list from the host now, for an action that must not go by a stale or missing one, such as removing a
   * host's workspaces; throws like [delete], or [JSONException].
   */
  suspend fun listNow(): List<SessionInfo> {
    val listed = hostClient.sessions().sortedWith(SESSION_ORDER)
    sessionsState.value = listed
    listingsFlow.tryEmit(listed)
    loadedState.value = true
    db?.sessions?.syncAsync(listed)
    return listed
  }

  private fun parseCachedArray(json: String): JSONArray? = try {
    JSONArray(json)
  } catch (_: JSONException) {
    null
  }

  private suspend fun load() {
    try {
      listNow()
    } catch (error: IOException) {
      Log.w(TAG, "Refreshing sessions failed", error)
    } catch (error: HostRejected) {
      Log.w(TAG, "Refreshing sessions refused: ${error.code}")
    } catch (error: JSONException) {
      Log.w(TAG, "Refreshing sessions returned malformed data", error)
    } finally {
      loadedState.value = true
    }
  }

  private companion object {
    const val TAG = "OOPBoT"
    const val NOT_FOUND = "not_found"
    val SESSION_ORDER = compareByDescending<SessionInfo> { it.modified }.thenBy { it.path }
  }
}
