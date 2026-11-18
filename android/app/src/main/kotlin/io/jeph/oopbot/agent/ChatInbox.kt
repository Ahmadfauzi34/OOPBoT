package io.jeph.oopbot.agent

import io.jeph.oopbot.data.OopbotDatabase
import kotlinx.coroutines.flow.MutableStateFlow
import kotlinx.coroutines.flow.StateFlow
import kotlinx.coroutines.flow.asStateFlow
import kotlinx.coroutines.flow.update

/** Why a chat is in the [ChatInbox]. */
internal enum class InboxReason { Finished, Failed, Exited, Waiting }

/**
 * A chat the user hasn't seen since [reason] happened in it at [at] (epoch millis): its [title] and [detail] (the
 * reply, error or question) as they were then, either null when there was none. [asker] names the subagent whose
 * question the chat waits on, set only when no one else could have asked it (see [askingSubagent]).
 */
internal data class InboxEntry(
  val location: ChatLocation,
  val reason: InboxReason,
  val title: String?,
  val detail: String?,
  val at: Long,
  val asker: String? = null,
)

/**
 * Chats whose work ended or that wait on a dialog while they were off screen, newest first, one entry per chat. An
 * entry stays until its chat comes on screen in any pane or is archived, the user opens or clears it, or, for a dialog,
 * the dialog goes away. Takes nothing in while [enabled] is off, and drops what it holds when switched off. Main thread
 * only.
 */
internal class ChatInbox(private val db: OopbotDatabase? = null) {
  private val entriesState = MutableStateFlow(db?.inbox?.loadAll().orEmpty())
  private var onScreen: Set<String> = emptySet()
  private var archived: Set<String> = emptySet()

  val entries: StateFlow<List<InboxEntry>> = entriesState.asStateFlow()

  var enabled: Boolean = true
    set(value) {
      field = value
      if (!value) clear()
    }

  /** Adds [entry] in place of its chat's earlier one, unless the chat is on screen or archived. */
  fun add(entry: InboxEntry) {
    val path = entry.location.sessionPath
    if (!enabled || path in onScreen || path in archived) return
    entriesState.update { entries -> listOf(entry) + entries.filterNot { it.location.sessionPath == path } }
    db?.inbox?.upsert(entry)
  }

  /** The dialog the chat in [sessionPath] waited on went away, so it no longer needs the user for it. */
  fun dialogGone(sessionPath: String) {
    if (entriesState.value.none { it.location.sessionPath == sessionPath && it.reason == InboxReason.Waiting }) return
    entriesState.update { entries ->
      entries.filterNot { it.location.sessionPath == sessionPath && it.reason == InboxReason.Waiting }
    }
    db?.inbox?.deleteWaiting(sessionPath)
  }

  /**
   * Names [asker] as the subagent whose question the chat in [sessionPath] waits on, once that's known after its entry
   * went in, keeping the entry's place and time.
   */
  fun nameAsker(sessionPath: String, asker: String) {
    if (entriesState.value.none { it.location.sessionPath == sessionPath && it.reason == InboxReason.Waiting }) return
    entriesState.update { entries ->
      entries.map { entry ->
        if (entry.location.sessionPath == sessionPath && entry.reason == InboxReason.Waiting) {
          entry.copy(asker = asker)
        } else {
          entry
        }
      }
    }
    db?.inbox?.updateAsker(sessionPath, asker)
  }

  /** Sets the session files of the chats the user sees, taking theirs out. */
  fun setOnScreen(sessionPaths: Set<String>) {
    onScreen = sessionPaths
    val dropped = entriesState.value.mapNotNull { it.location.sessionPath.takeIf { path -> path in sessionPaths } }
    if (dropped.isEmpty()) return
    entriesState.update { entries -> entries.filterNot { it.location.sessionPath in sessionPaths } }
    db?.inbox?.deletePaths(dropped)
  }

  /**
   * Sets the session files of the archived chats, taking theirs out. Archiving a running chat stops it, which ends its
   * work after it was archived, so this also keeps that out.
   */
  fun setArchived(sessionPaths: Set<String>) {
    archived = sessionPaths
    val dropped = entriesState.value.mapNotNull { it.location.sessionPath.takeIf { path -> path in sessionPaths } }
    if (dropped.isEmpty()) return
    entriesState.update { entries -> entries.filterNot { it.location.sessionPath in sessionPaths } }
    db?.inbox?.deletePaths(dropped)
  }

  fun remove(sessionPath: String) {
    if (entriesState.value.none { it.location.sessionPath == sessionPath }) return
    entriesState.update { entries -> entries.filterNot { it.location.sessionPath == sessionPath } }
    db?.inbox?.deletePaths(listOf(sessionPath))
  }

  fun clear() {
    if (entriesState.value.isEmpty()) return
    entriesState.value = emptyList()
    db?.inbox?.clear()
  }
}
