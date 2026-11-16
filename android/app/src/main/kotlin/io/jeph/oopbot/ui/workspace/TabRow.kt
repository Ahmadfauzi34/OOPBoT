package io.jeph.oopbot.ui.workspace

import android.text.format.DateUtils
import androidx.compose.foundation.ExperimentalFoundationApi
import androidx.compose.foundation.Image
import androidx.compose.foundation.background
import androidx.compose.foundation.combinedClickable
import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Box
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.heightIn
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.layout.size
import androidx.compose.foundation.shape.RoundedCornerShape
import androidx.compose.material3.Text
import androidx.compose.runtime.Composable
import androidx.compose.runtime.getValue
import androidx.compose.runtime.remember
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.draw.clip
import androidx.compose.ui.graphics.Color
import androidx.compose.ui.platform.LocalContext
import androidx.compose.ui.res.painterResource
import androidx.compose.ui.res.stringResource
import androidx.compose.ui.text.font.FontWeight
import androidx.compose.ui.text.style.TextOverflow
import androidx.compose.ui.unit.dp
import androidx.compose.ui.unit.sp
import androidx.lifecycle.compose.collectAsStateWithLifecycle
import io.jeph.oopbot.R
import io.jeph.oopbot.agent.AgentProcess
import io.jeph.oopbot.agent.AgentProcessState
import io.jeph.oopbot.files.FileTab
import io.jeph.oopbot.git.CommitTab
import io.jeph.oopbot.host.SessionInfo
import io.jeph.oopbot.terminal.TerminalSession
import io.jeph.oopbot.ui.chat.extensionStatusLine
import io.jeph.oopbot.ui.components.LucideIcon
import io.jeph.oopbot.ui.components.PanningText
import io.jeph.oopbot.ui.components.RoundIconButton
import io.jeph.oopbot.ui.components.SimmeringPot
import io.jeph.oopbot.ui.components.StatusDot
import io.jeph.oopbot.ui.files.fileIcon
import io.jeph.oopbot.ui.terminal.terminalTitle
import io.jeph.oopbot.ui.theme.PipkinTokens
import io.jeph.oopbot.ui.theme.pipkinColors

/**
 * One chat in the tab menu: what Pi is doing in it, its title, and the statuses its extensions set, else its age;
 * highlighted while it is the one shown, with an actions button that long-press also opens.
 */
@Composable
internal fun ChatTabRow(
  title: String,
  modified: Long,
  statuses: Map<String, String>,
  process: AgentProcess?,
  current: Boolean,
  onOpen: () -> Unit,
  onActions: () -> Unit,
  modifier: Modifier = Modifier,
) {
  val colors = pipkinColors
  val status = remember(statuses, colors) { extensionStatusLine(statuses, colors) }
  TabRow(
    mark = { if (process != null) ActivityMark(process) else IdleMark() },
    title = title,
    detail = { if (status != null) PanningText(status) else DetailText(relativeTime(modified)) },
    current = current,
    onOpen = onOpen,
    onActions = onActions,
    actionsDescription = stringResource(R.string.shell_session_actions),
    modifier = modifier,
  )
}

/** One terminal in the tab menu: the terminal icon and its name, highlighted while it is the one shown. */
@Composable
internal fun TerminalTabRow(
  session: TerminalSession,
  current: Boolean,
  onOpen: () -> Unit,
  onActions: () -> Unit,
  modifier: Modifier = Modifier,
) {
  TabRow(
    mark = {
      LucideIcon(
        icon = R.drawable.ic_lucide_square_terminal,
        contentDescription = null,
        tint = pipkinColors.foregroundMuted,
      )
    },
    title = terminalTitle(session),
    detail = null,
    current = current,
    onOpen = onOpen,
    onActions = onActions,
    actionsDescription = stringResource(R.string.terminal_actions),
    modifier = modifier,
  )
}

/** One open file in the tab menu: its file-type icon, its name over its folder, highlighted while it is the one shown. */
@Composable
internal fun FileTabRow(
  file: FileTab,
  current: Boolean,
  onOpen: () -> Unit,
  onActions: () -> Unit,
  modifier: Modifier = Modifier,
) {
  TabRow(
    mark = {
      Image(
        painter = painterResource(fileIcon(file.name)),
        contentDescription = null,
        modifier = Modifier.size(PipkinTokens.IconSize.md),
      )
    },
    title = file.name,
    detail = file.folder.takeIf { it.isNotEmpty() }?.let { folder -> { DetailText(folder) } },
    current = current,
    onOpen = onOpen,
    onActions = onActions,
    actionsDescription = stringResource(R.string.file_actions),
    modifier = modifier,
  )
}

/** One open commit in the tab menu: the commit icon and its short SHA and subject, highlighted while it's shown. */
@Composable
internal fun CommitTabRow(
  commit: CommitTab,
  current: Boolean,
  onOpen: () -> Unit,
  onActions: () -> Unit,
  modifier: Modifier = Modifier,
) {
  TabRow(
    mark = {
      LucideIcon(
        icon = R.drawable.ic_lucide_git_commit_horizontal,
        contentDescription = null,
        tint = pipkinColors.foregroundMuted,
      )
    },
    title = commit.title,
    detail = null,
    current = current,
    onOpen = onOpen,
    onActions = onActions,
    actionsDescription = stringResource(R.string.git_commit_actions),
    modifier = modifier,
  )
}

/** A tab's row: its [mark] in a tile, its title over its [detail], and its actions; the shown tab is a raised card. */
@OptIn(ExperimentalFoundationApi::class)
@Composable
private fun TabRow(
  mark: @Composable () -> Unit,
  title: String,
  detail: (@Composable () -> Unit)?,
  current: Boolean,
  onOpen: () -> Unit,
  onActions: () -> Unit,
  actionsDescription: String,
  modifier: Modifier,
) {
  val colors = pipkinColors
  Row(
    modifier = modifier
      .fillMaxWidth()
      .padding(horizontal = PipkinTokens.Spacing.s12, vertical = 1.dp)
      .clip(ROW_SHAPE)
      .background(if (current) colors.surface2 else Color.Transparent)
      .combinedClickable(onClick = onOpen, onLongClick = onActions)
      .heightIn(min = 56.dp)
      .padding(start = 10.dp, end = PipkinTokens.Spacing.s4),
    verticalAlignment = Alignment.CenterVertically,
    horizontalArrangement = Arrangement.spacedBy(PipkinTokens.Spacing.s12),
  ) {
    Box(
      modifier = Modifier
        .size(MARK_TILE_SIZE)
        .clip(MARK_TILE_SHAPE)
        .background(if (current) colors.surface3 else colors.surface2),
      contentAlignment = Alignment.Center,
    ) { mark() }
    Column(modifier = Modifier.weight(1f).padding(vertical = PipkinTokens.Spacing.s8)) {
      Text(
        text = title,
        color = colors.foreground,
        fontSize = TITLE_SIZE,
        fontWeight = if (current) FontWeight.Bold else FontWeight.SemiBold,
        maxLines = 1,
        overflow = TextOverflow.Ellipsis,
      )
      detail?.invoke()
    }
    RoundIconButton(
      icon = R.drawable.ic_lucide_ellipsis,
      contentDescription = actionsDescription,
      onClick = onActions,
      modifier = Modifier.size(32.dp),
      tint = colors.foregroundMuted,
    )
  }
}

private val ROW_SHAPE = RoundedCornerShape(16.dp)
private val MARK_TILE_SIZE = 32.dp
private val MARK_TILE_SHAPE = RoundedCornerShape(10.dp)
private val TITLE_SIZE = 14.5.sp

/** A row's second line; a long one keeps its end, where a folder path names the folder. */
@Composable
private fun DetailText(text: String) {
  Text(
    text = text,
    color = pipkinColors.foregroundMuted,
    fontSize = PipkinTokens.FontSize.sm,
    maxLines = 1,
    overflow = TextOverflow.StartEllipsis,
  )
}

/** The chat's name, else its first message, else "New chat". */
@Composable
internal fun sessionTitle(session: SessionInfo): String = session.name?.takeIf { it.isNotBlank() }
  ?: session.firstMessage.takeIf { it.isNotBlank() }
  ?: stringResource(R.string.shell_session_untitled)

/** The simmering pot while the chat works, a danger dot after Pi exited with a failure, otherwise [IdleMark]. */
@Composable
private fun ActivityMark(process: AgentProcess) {
  val working by process.working.collectAsStateWithLifecycle()
  val state by process.state.collectAsStateWithLifecycle()
  val crashed = (state as? AgentProcessState.Exited)?.code.let { it != null && it != 0 }
  when {
    working -> SimmeringPot()
    crashed -> StatusDot(pipkinColors.dotDanger)
    else -> IdleMark()
  }
}

@Composable
private fun IdleMark() {
  LucideIcon(icon = R.drawable.ic_lucide_bot, contentDescription = null, tint = pipkinColors.foregroundMuted)
}

/** `now`, `5m`, `2h`, `3d`, then a short date. */
@Composable
internal fun relativeTime(epochMillis: Long): String {
  val elapsed = (System.currentTimeMillis() - epochMillis).coerceAtLeast(0)
  val minutes = elapsed / DateUtils.MINUTE_IN_MILLIS
  val hours = elapsed / DateUtils.HOUR_IN_MILLIS
  val days = elapsed / DateUtils.DAY_IN_MILLIS
  return when {
    minutes < 1 -> stringResource(R.string.shell_time_now)

    hours < 1 -> stringResource(R.string.shell_time_minutes, minutes)

    days < 1 -> stringResource(R.string.shell_time_hours, hours)

    days < 7 -> stringResource(R.string.shell_time_days, days)

    else -> DateUtils.formatDateTime(
      LocalContext.current,
      epochMillis,
      DateUtils.FORMAT_SHOW_DATE or DateUtils.FORMAT_ABBREV_MONTH,
    )
  }
}
