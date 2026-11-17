package io.jeph.oopbot.ui.chat

import android.text.format.DateFormat
import androidx.annotation.DrawableRes
import androidx.compose.foundation.Image
import androidx.compose.foundation.background
import androidx.compose.foundation.border
import androidx.compose.foundation.clickable
import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Box
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.FlowRow
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.Spacer
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.height
import androidx.compose.foundation.layout.heightIn
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.layout.size
import androidx.compose.foundation.shape.CircleShape
import androidx.compose.foundation.shape.RoundedCornerShape
import androidx.compose.foundation.text.selection.SelectionContainer
import androidx.compose.material3.Text
import androidx.compose.runtime.Composable
import androidx.compose.runtime.getValue
import androidx.compose.runtime.produceState
import androidx.compose.runtime.remember
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.draw.clip
import androidx.compose.ui.graphics.ImageBitmap
import androidx.compose.ui.layout.ContentScale
import androidx.compose.ui.platform.LocalContext
import androidx.compose.ui.res.pluralStringResource
import androidx.compose.ui.res.stringResource
import androidx.compose.ui.text.TextStyle
import androidx.compose.ui.text.font.FontWeight
import androidx.compose.ui.text.style.TextAlign
import androidx.compose.ui.text.style.TextOverflow
import androidx.compose.ui.unit.DpSize
import androidx.compose.ui.unit.dp
import androidx.compose.ui.unit.em
import androidx.compose.ui.unit.sp
import io.jeph.oopbot.R
import io.jeph.oopbot.agent.ImagePart
import io.jeph.oopbot.agent.NoticeKind
import io.jeph.oopbot.agent.TimelineItem
import io.jeph.oopbot.agent.text
import io.jeph.oopbot.ui.components.CapsLabel
import io.jeph.oopbot.ui.components.LucideIcon
import io.jeph.oopbot.ui.components.MarkdownCache
import io.jeph.oopbot.ui.components.OopbotMarkdown
import io.jeph.oopbot.ui.components.RoundIconButton
import io.jeph.oopbot.ui.components.SimmeringPot
import io.jeph.oopbot.ui.theme.OopbotFonts
import io.jeph.oopbot.ui.theme.OopbotTokens
import io.jeph.oopbot.ui.theme.oopbotColors
import java.util.Date
import kotlinx.coroutines.delay

private val PromptShape = RoundedCornerShape(20.dp)
private val ImageMaxEdge = 160.dp
private val PROMPT_LINE_HEIGHT = 21.sp

/** How many lines of a long prompt show until Show more. */
private const val PROMPT_LINES = 8

/**
 * The user's message: a full-width card headed "You" and its time, with the copy button at the end of that line. Text
 * longer than [PROMPT_LINES] lines folds unless [expanded]; [onToggle] unfolds or folds it. Images always show whole.
 * A subagent's [task] is the first message of its transcript, which the main agent wrote, so it's headed as the task.
 */
@Composable
internal fun UserMessage(
  item: TimelineItem.User,
  expanded: Boolean,
  onToggle: () -> Unit,
  modifier: Modifier = Modifier,
  task: Boolean = false,
) {
  val colors = oopbotColors
  Column(
    modifier = modifier
      .fillMaxWidth()
      .clip(PromptShape)
      .background(colors.surface1)
      .padding(start = 14.dp, top = OopbotTokens.Spacing.s4, end = OopbotTokens.Spacing.s4, bottom = 11.dp),
  ) {
    PromptHeader(item, task)
    Column(
      modifier = Modifier.padding(end = OopbotTokens.Spacing.s8),
      verticalArrangement = Arrangement.spacedBy(OopbotTokens.Spacing.s8),
    ) {
      if (item.images.isNotEmpty()) UserImages(item.images)
      if (item.text.isNotEmpty()) {
        FoldedContent(
          PROMPT_LINES,
          slackLines = 0,
          PROMPT_LINE_HEIGHT,
          fadeTo = colors.surface1,
          expanded = expanded,
          onToggle = onToggle,
        ) {
          SelectionContainer {
            Text(text = item.text, color = colors.foreground, fontSize = 14.5.sp, lineHeight = PROMPT_LINE_HEIGHT)
          }
        }
      }
    }
  }
}

@Composable
private fun PromptHeader(item: TimelineItem.User, task: Boolean) {
  val colors = oopbotColors
  val context = LocalContext.current
  val time = remember(item.timestamp, context) { DateFormat.getTimeFormat(context).format(Date(item.timestamp)) }
  Row(modifier = Modifier.heightIn(min = 28.dp), verticalAlignment = Alignment.CenterVertically) {
    if (task) {
      CapsLabel(stringResource(R.string.subagent_task), colors.extension, fontWeight = FontWeight.SemiBold)
    } else {
      CapsLabel(stringResource(R.string.chat_you), colors.accent, fontWeight = FontWeight.SemiBold)
    }
    Text(
      text = stringResource(R.string.separator_dot) + if (task) stringResource(R.string.subagent_task_from) else time,
      modifier = Modifier.weight(1f),
      color = colors.foregroundMuted,
      fontSize = 10.5.sp,
      fontWeight = FontWeight.Medium,
      maxLines = 1,
    )
    if (item.text.isNotEmpty()) {
      CopyButton(
        text = item.text,
        contentDescription = stringResource(R.string.chat_copy_message),
        tint = colors.foregroundMuted,
        iconSize = OopbotTokens.IconSize.sm,
      )
    }
  }
}

@Composable
private fun UserImages(images: List<ImagePart>) {
  FlowRow(
    horizontalArrangement = Arrangement.spacedBy(OopbotTokens.Spacing.s8),
    verticalArrangement = Arrangement.spacedBy(OopbotTokens.Spacing.s8),
  ) {
    images.forEach { ImageThumbnail(it) }
  }
}

private val ThumbnailShape = RoundedCornerShape(OopbotTokens.Radius.md)

@Composable
internal fun ImageThumbnail(part: ImagePart) {
  val initial = remember(part) { ChatImages.cachedThumbnail(part) }
  val bitmap by produceState<ImageBitmap?>(initial, part) {
    if (value == null) value = ChatImages.thumbnail(part)
  }
  val cachedAspect = remember(part, bitmap) { ChatImages.cachedAspect(part) }
  val image = bitmap
  val aspect = image?.let { it.width.toFloat() / it.height.coerceAtLeast(1) } ?: cachedAspect
  val size = when {
    aspect == null -> DpSize(ImageMaxEdge, ImageMaxEdge)
    aspect >= 1f -> DpSize(ImageMaxEdge, ImageMaxEdge / aspect)
    else -> DpSize(ImageMaxEdge * aspect, ImageMaxEdge)
  }
  if (image == null) {
    Box(modifier = Modifier.size(size).clip(ThumbnailShape).background(oopbotColors.surface2))
    return
  }
  Image(
    bitmap = image,
    contentDescription = stringResource(R.string.chat_image),
    modifier = Modifier.size(size).clip(ThumbnailShape),
    contentScale = ContentScale.Crop,
  )
}

/**
 * A block of the agent's reply; once it finished streaming, its copy button and time sit under it. Its parse is kept
 * in [markdownCache] once it has finished, not for every text it streams through.
 */
@Composable
internal fun AssistantMessage(
  item: TimelineItem.AssistantText,
  markdownCache: MarkdownCache,
  modifier: Modifier = Modifier,
) {
  Column(modifier = modifier.fillMaxWidth().padding(top = OopbotTokens.Spacing.s12)) {
    OopbotMarkdown(
      markdown = item.markdown,
      modifier = Modifier.fillMaxWidth(),
      cache = markdownCache.takeUnless { item.streaming },
      selectable = !item.streaming,
    )
    if (item.streaming) {
      Spacer(modifier = Modifier.height(OopbotTokens.Spacing.s12))
      return@Column
    }
    Row(
      modifier = Modifier.padding(top = OopbotTokens.Spacing.s4),
      verticalAlignment = Alignment.CenterVertically,
      horizontalArrangement = Arrangement.spacedBy(OopbotTokens.Spacing.s8),
    ) {
      CopyButton(
        text = item.markdown,
        contentDescription = stringResource(R.string.chat_copy_reply),
        tint = oopbotColors.foregroundMuted,
        iconSize = OopbotTokens.IconSize.sm,
      )
      MessageTime(item.timestamp)
    }
  }
}

/** When a message was sent, as `2:47 PM` or `14:47` in the device's time zone, language and 12/24-hour setting. */
@Composable
internal fun MessageTime(timestamp: Long) {
  val context = LocalContext.current
  val time = remember(timestamp, context) { DateFormat.getTimeFormat(context).format(Date(timestamp)) }
  Text(text = time, color = oopbotColors.foregroundMuted, fontSize = OopbotTokens.FontSize.meta)
}
