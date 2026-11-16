package io.jeph.oopbot.ui.workspace

import androidx.compose.runtime.Composable
import androidx.lifecycle.compose.collectAsStateWithLifecycle
import io.jeph.oopbot.agent.AgentProcess
import io.jeph.oopbot.host.SessionInfo

/**
 * The title an extension set on the chat: its running [process]'s, or the [keptTitle] OOPBoT kept when it stopped the
 * chat's Pi. Else [sessionTitle].
 */
@Composable
internal fun chatTitle(session: SessionInfo, process: AgentProcess?, keptTitle: String?): String {
  val extensionTitle = if (process != null) process.extensionTitle.collectAsStateWithLifecycle().value else keptTitle
  return extensionTitle ?: sessionTitle(session)
}

/** The statuses extensions set on the chat: its running [process]'s, or the [kept] ones from when OOPBoT stopped its Pi. */
@Composable
internal fun chatStatuses(process: AgentProcess?, kept: Map<String, String>?): Map<String, String> =
  if (process != null) process.extensionStatuses.collectAsStateWithLifecycle().value else kept.orEmpty()
