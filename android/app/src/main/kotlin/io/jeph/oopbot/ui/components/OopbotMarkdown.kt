package io.jeph.oopbot.ui.components

import androidx.compose.foundation.layout.PaddingValues
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.text.selection.SelectionContainer
import androidx.compose.runtime.Composable
import androidx.compose.runtime.LaunchedEffect
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.remember
import androidx.compose.runtime.rememberUpdatedState
import androidx.compose.runtime.setValue
import androidx.compose.runtime.snapshotFlow
import androidx.compose.ui.Modifier
import androidx.compose.ui.text.SpanStyle
import androidx.compose.ui.text.TextLinkStyles
import androidx.compose.ui.text.TextStyle
import androidx.compose.ui.text.font.FontWeight
import androidx.compose.ui.text.style.TextDecoration
import androidx.compose.ui.text.withStyle
import androidx.compose.ui.unit.dp
import androidx.compose.ui.unit.sp
import com.mikepenz.markdown.compose.components.markdownComponents
import com.mikepenz.markdown.compose.elements.MarkdownCodeBlock
import com.mikepenz.markdown.compose.elements.MarkdownCodeFence
import com.mikepenz.markdown.m3.Markdown
import com.mikepenz.markdown.m3.markdownColor
import com.mikepenz.markdown.m3.markdownTypography
import com.mikepenz.markdown.model.MarkdownAnnotator
import com.mikepenz.markdown.model.MarkdownTypography
import com.mikepenz.markdown.model.State
import com.mikepenz.markdown.model.markdownAnimations
import com.mikepenz.markdown.model.markdownAnnotator
import com.mikepenz.markdown.model.markdownDimens
import com.mikepenz.markdown.model.markdownPadding
import com.mikepenz.markdown.utils.resolveImageAlt
import io.jeph.oopbot.ui.theme.OopbotColors
import io.jeph.oopbot.ui.theme.OopbotFonts
import io.jeph.oopbot.ui.theme.OopbotTokens
import io.jeph.oopbot.ui.theme.oopbotColors
import kotlinx.coroutines.flow.conflate
import org.intellij.markdown.MarkdownElementTypes

/**
 * Assistant markdown: 15/22sp body, and code on raised cards that scroll sideways, headed by their language. With a
 * [cache], markdown it has parsed before shows from the first frame, and each parse of it is kept there. While a
 * reply is streaming ([selectable] false), selection tracking is skipped until the message settles.
 */
@Composable
fun OopbotMarkdown(
  markdown: String,
  modifier: Modifier = Modifier,
  cache: MarkdownCache? = null,
  selectable: Boolean = true,
) {
  val colors = oopbotColors
  val body = remember(colors) {
    TextStyle(
      color = colors.foreground,
      fontFamily = OopbotFonts.ui,
      fontSize = OopbotTokens.FontSize.content,
      lineHeight = OopbotTokens.LineHeight.markdown,
      fontFeatureSettings = OopbotFonts.FEATURES,
    )
  }
  val state = rememberParsedMarkdown(markdown, cache)
  if (selectable) {
    SelectionContainer(modifier = modifier) {
      OopbotMarkdownContent(state = state, colors = colors, body = body, modifier = Modifier.fillMaxWidth())
    }
  } else {
    OopbotMarkdownContent(state = state, colors = colors, body = body, modifier = modifier.fillMaxWidth())
  }
}

@Composable
private fun OopbotMarkdownContent(
  state: State,
  colors: OopbotColors,
  body: TextStyle,
  modifier: Modifier,
) {
  Markdown(
    state = state,
    modifier = modifier,
    colors =
      markdownColor(
        text = colors.foreground,
        codeBackground = colors.surface2,
        inlineCodeBackground = colors.surface2,
        dividerColor = colors.border,
      ),
    typography = typography(body),
    padding = markdownPadding(block = OopbotTokens.Spacing.s6, codeBlock = CODE_BLOCK_PADDING),
    dimens = markdownDimens(codeBackgroundCornerSize = OopbotTokens.Radius.base),
    components =
      markdownComponents(
        codeFence = { MarkdownCodeFence(it.content, it.node, style = it.typography.code, block = OopbotCodeBlock) },
        codeBlock = { MarkdownCodeBlock(it.content, it.node, style = it.typography.code, block = OopbotCodeBlock) },
        table = { MarkdownTable(it.content, it.node, it.typography.table) },
      ),
    // The library animates each text block's size by default, which relayouts every frame a streamed
    // block grows and leaves the chat's end moving after the text has landed.
    animations = markdownAnimations(animateTextSize = { this }),
    annotator = imageAltAnnotator(),
  )
}

private val CODE_BLOCK_PADDING = PaddingValues(0.dp)

// CodeText is OOPBoT's one code style, so the library's code typography is ignored here.
private val OopbotCodeBlock: @Composable (String, String?, TextStyle) -> Unit = { text, language, _ ->
  CodeBox(
    modifier = Modifier.padding(vertical = OopbotTokens.Spacing.s6),
    label = language?.takeIf { it.isNotBlank() },
  ) { CodeText(text, language) }
}

/**
 * [markdown] parsed off the main thread, one parse at a time with the latest text. Until the current text's parse
 * lands the last result stays, so a streamed reply doesn't collapse to the empty loading box between deltas. A text
 * [cache] holds needs no parse or snapshot collector, and each result is kept in [cache].
 */
@Composable
private fun rememberParsedMarkdown(markdown: String, cache: MarkdownCache?): State {
  val cached = remember(markdown, cache) { cache?.get(markdown) }
  var parsed by remember { mutableStateOf<State>(cached ?: State.Loading()) }
  val latestMarkdown by rememberUpdatedState(markdown)
  LaunchedEffect(cache, cached != null) {
    if (cached != null) return@LaunchedEffect
    // Conflated, so text that changes faster than it parses skips to the latest instead of queueing every delta.
    snapshotFlow { latestMarkdown }.conflate().collect { text ->
      val shown = parsed
      val next = when {
        shown is State.Success && shown.content == text -> shown
        else -> cache?.get(text) ?: parseOffMain(text)
      }
      parsed = next
      cache?.put(text, next)
    }
  }
  return cached ?: parsed
}

/**
 * OOPBoT has no image loader, so an image shows as Paseo shows one that failed to load: its alt text, small and muted
 * on surface2, or nothing without alt. Left to the library, each image reserves an empty box the size of a picture.
 */
@Composable
private fun imageAltAnnotator(): MarkdownAnnotator {
  val colors = oopbotColors
  val style = remember(colors) {
    SpanStyle(color = colors.foregroundMuted, background = colors.surface2, fontSize = IMAGE_ALT_FONT_SIZE)
  }
  return remember(style) {
    markdownAnnotator { content, node ->
      if (node.type != MarkdownElementTypes.IMAGE) return@markdownAnnotator false
      val alt = node.resolveImageAlt(content)
      if (!alt.isNullOrBlank()) withStyle(style) { append(alt) }
      true
    }
  }
}

private val IMAGE_ALT_FONT_SIZE = 10.sp

@Composable
private fun typography(body: TextStyle): MarkdownTypography {
  val colors = oopbotColors
  val styles = remember(body, colors) {
    val code = TextStyle(
      color = colors.foreground,
      fontFamily = OopbotFonts.mono,
      fontSize = OopbotTokens.FontSize.code,
      lineHeight = OopbotTokens.LineHeight.code,
      fontFeatureSettings = OopbotFonts.FEATURES,
    )
    val headings = HeadingStyles(
      h1 = body.copy(fontSize = OopbotTokens.FontSize.xxl, lineHeight = 28.sp, fontWeight = FontWeight.SemiBold),
      h2 = body.copy(fontSize = OopbotTokens.FontSize.xl, lineHeight = 26.sp, fontWeight = FontWeight.SemiBold),
      h3 = body.copy(fontSize = OopbotTokens.FontSize.lg, lineHeight = 24.sp, fontWeight = FontWeight.SemiBold),
      small = body.copy(fontWeight = FontWeight.SemiBold),
    )
    MemoizedMarkdownStyles(
      headings = headings,
      code = code,
      inlineCode = code.copy(lineHeight = OopbotTokens.LineHeight.markdown),
      quote = body.copy(color = colors.foregroundMuted),
      textLink = TextLinkStyles(style = SpanStyle(color = colors.accent, textDecoration = TextDecoration.Underline)),
      // The front matter table's cell type, a size under the body so more of a row fits across.
      table = body.copy(fontSize = OopbotTokens.FontSize.base, lineHeight = 20.sp),
    )
  }
  return markdownTypography(
    h1 = styles.headings.h1,
    h2 = styles.headings.h2,
    h3 = styles.headings.h3,
    h4 = styles.headings.small,
    h5 = styles.headings.small,
    h6 = styles.headings.small,
    text = body,
    code = styles.code,
    inlineCode = styles.inlineCode,
    quote = styles.quote,
    paragraph = body,
    ordered = body,
    bullet = body,
    list = body,
    textLink = styles.textLink,
    table = styles.table,
    alertTitle = styles.headings.small,
  )
}

private class HeadingStyles(val h1: TextStyle, val h2: TextStyle, val h3: TextStyle, val small: TextStyle)

private class MemoizedMarkdownStyles(
  val headings: HeadingStyles,
  val code: TextStyle,
  val inlineCode: TextStyle,
  val quote: TextStyle,
  val textLink: TextLinkStyles,
  val table: TextStyle,
)
