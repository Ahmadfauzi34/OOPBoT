package io.jeph.oopbot.ui.git

import androidx.compose.foundation.background
import androidx.compose.foundation.gestures.Orientation
import androidx.compose.foundation.gestures.ScrollableState
import androidx.compose.foundation.gestures.scrollable
import androidx.compose.foundation.layout.PaddingValues
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.height
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.layout.width
import androidx.compose.foundation.lazy.LazyColumn
import androidx.compose.foundation.lazy.LazyListScope
import androidx.compose.foundation.lazy.LazyListState
import androidx.compose.foundation.shape.RoundedCornerShape
import androidx.compose.material3.Text
import androidx.compose.runtime.Composable
import androidx.compose.runtime.Immutable
import androidx.compose.runtime.LaunchedEffect
import androidx.compose.runtime.Stable
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableFloatStateOf
import androidx.compose.runtime.remember
import androidx.compose.runtime.rememberUpdatedState
import androidx.compose.runtime.setValue
import androidx.compose.runtime.snapshotFlow
import androidx.compose.ui.Modifier
import androidx.compose.ui.draw.clip
import androidx.compose.ui.draw.clipToBounds
import androidx.compose.ui.draw.drawBehind
import androidx.compose.ui.geometry.Offset
import androidx.compose.ui.geometry.Size
import androidx.compose.ui.graphics.Color
import androidx.compose.ui.graphics.RectangleShape
import androidx.compose.ui.layout.layout
import androidx.compose.ui.platform.LocalContext
import androidx.compose.ui.platform.LocalDensity
import androidx.compose.ui.res.stringResource
import androidx.compose.ui.text.AnnotatedString
import androidx.compose.ui.text.SpanStyle
import androidx.compose.ui.text.TextStyle
import androidx.compose.ui.text.font.FontStyle
import androidx.compose.ui.text.rememberTextMeasurer
import androidx.compose.ui.text.style.TextAlign
import androidx.compose.ui.text.style.TextDirection
import androidx.compose.ui.unit.Constraints
import androidx.compose.ui.unit.Dp
import androidx.compose.ui.unit.LayoutDirection
import androidx.compose.ui.unit.dp
import io.jeph.oopbot.R
import io.jeph.oopbot.host.GitFileKind
import io.jeph.oopbot.syntax.Grammar
import io.jeph.oopbot.ui.components.fileLanguage
import io.jeph.oopbot.ui.theme.OopbotColors
import io.jeph.oopbot.ui.theme.OopbotFonts
import io.jeph.oopbot.ui.theme.OopbotTokens
import io.jeph.oopbot.ui.theme.oopbotColors
import kotlin.math.roundToInt
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.flow.conflate
import kotlinx.coroutines.flow.distinctUntilChanged
import kotlinx.coroutines.withContext
import kotlinx.coroutines.yield

/**
 * A diff document: each file of [document] as a card, under a header that sticks to the top while its lines scroll,
 * then its hunks as unified rows in a well, with one gutter of line numbers, added and removed lines on green and red,
 * coloured by the file's grammar. Long lines wrap with [wrapLines], otherwise the code scrolls sideways together, under
 * a gutter that stays put. With [onToggle] a header's tap folds or unfolds its file, folded ones being [collapsed];
 * without it every file shows unfolded. Only the rows on screen are composed, and highlighting runs lazily off the main
 * thread for visible expanded files first.
 */
@Composable
internal fun DiffView(
  document: DiffDocument,
  wrapLines: Boolean,
  collapsed: Set<String>,
  onToggle: ((path: String) -> Unit)?,
  state: LazyListState,
  modifier: Modifier = Modifier,
  contentPadding: PaddingValues = PaddingValues(bottom = CARD_GAP),
) {
  HighlightEffect(document, collapsed, state)
  val colors = oopbotColors
  val style = codeStyle()
  val measurer = rememberTextMeasurer()
  val density = LocalDensity.current
  val digitWidth = remember(measurer, style, density) {
    with(density) { measurer.measure("0", style).size.width.toDp() }
  }
  // A new document starts unscrolled, measured afresh.
  val scroller = remember(document) { SideScroller() }
  val sideways = if (wrapLines) Modifier else Modifier.scrollable(scroller.scrollable, Orientation.Horizontal)
  LazyColumn(
    modifier = modifier.fillMaxSize().then(sideways),
    state = state,
    contentPadding = contentPadding,
  ) {
    for (section in document.sections) {
      val expanded = section.file.path !in collapsed
      stickyHeader(key = section.file.path, contentType = HEADER) {
        FileHeader(section, expanded, onToggle?.let { toggle -> { toggle(section.file.path) } })
      }
      if (expanded) {
        val rowStyle = DiffRowStyle(colors, style, gutterWidth(section.maxLineNumber, digitWidth), wrapLines, scroller)
        sectionRows(section, rowStyle)
      }
    }
  }
}

/** The code font: 12sp JetBrains Mono on 18sp lines, ligatures off, and left to right in any language. */
@Composable
private fun codeStyle(): TextStyle {
  val colors = oopbotColors
  return remember(colors) {
    TextStyle(
      color = colors.foreground,
      fontFamily = OopbotFonts.mono,
      fontSize = OopbotTokens.FontSize.code,
      lineHeight = OopbotTokens.LineHeight.code,
      fontFeatureSettings = OopbotFonts.FEATURES,
      textDirection = TextDirection.Ltr,
    )
  }
}

/**
 * Colours visible expanded files of [document] lazily off the main thread, reusing the shared highlight cache and
 * skipping collapsed or oversized files so giant diffs stay responsive.
 */
@Composable
private fun HighlightEffect(document: DiffDocument, collapsed: Set<String>, state: LazyListState) {
  val theme = oopbotColors
  val assets = LocalContext.current.assets
  val latestCollapsed by rememberUpdatedState(collapsed)
  val sectionsByPath = remember(document) { document.sections.associateBy { it.file.path } }
  val indexByPath = remember(document) {
    document.sections.withIndex().associate { (idx, sec) -> sec.file.path to idx }
  }
  LaunchedEffect(document, theme, state) {
    snapshotFlow {
      val hidden = latestCollapsed
      val visibleIndices = state.layoutInfo.visibleItemsInfo.mapNotNullTo(LinkedHashSet()) { item ->
        val rawKey = item.key as? String ?: return@mapNotNullTo null
        val path = rawKey.substringBefore('\n')
        indexByPath[path]
      }
      val targets = LinkedHashSet<String>()
      for (idx in visibleIndices) {
        val lo = (idx - 1).coerceAtLeast(0)
        val hi = (idx + 1).coerceAtMost(document.sections.lastIndex)
        for (neighbor in lo..hi) {
          val path = document.sections[neighbor].file.path
          if (path !in hidden) targets += path
        }
      }
      targets.toList()
    }
      .distinctUntilChanged()
      .conflate()
      .collect { targetPaths ->
        for (path in targetPaths) {
          val section = sectionsByPath[path]
          val grammar = fileLanguage(path)?.let(Grammar::forName)
          if (section != null && grammar != null && section.canHighlightFor(theme)) {
            section.highlight = withContext(Dispatchers.Default) {
              highlightDiffSection(section, grammar, assets, theme)
            }
            yield()
          }
        }
      }
  }
}

/** What every row of a file's body is drawn with. */
@Immutable
private data class DiffRowStyle(
  val colors: OopbotColors,
  val code: TextStyle,
  val gutterWidth: Dp,
  val wrapLines: Boolean,
  val scroller: SideScroller,
)

private fun LazyListScope.sectionRows(section: DiffSection, style: DiffRowStyle) {
  val path = section.file.path
  val last = section.rows.lastIndex
  items(
    count = section.rows.size,
    key = { "$path\n$it" },
    contentType = { section.rows[it]::class },
  ) { index ->
    val card = Modifier.cardRow(style.colors, first = index == 0, last = index == last)
    when (val row = section.rows[index]) {
      is DiffRow.HunkHeader -> HunkHeaderRow(row.header, style, card)
      is DiffRow.Line -> LineRow(row, section.highlight, style, card)
      is DiffRow.Placeholder -> PlaceholderRow(row.kind, card)
    }
  }
}

/**
 * A body row's place in its file's card: inset in the card, which ends below the [last] row, on the well of page
 * colour whose corners round at the [first] and [last] rows. The clip happens only there, so most rows draw no layer.
 */
private fun Modifier.cardRow(colors: OopbotColors, first: Boolean, last: Boolean): Modifier {
  val inCard = padding(horizontal = CARD_MARGIN)
    .fillMaxWidth()
    .background(colors.surface1, if (last) CardBottomShape else RectangleShape)
    .padding(start = WELL_INSET, end = WELL_INSET, bottom = if (last) WELL_INSET else 0.dp)
  val well = when {
    first && last -> RoundedCornerShape(WELL_RADIUS)
    first -> WellTopShape
    last -> WellBottomShape
    else -> null
  }
  return (if (well == null) inCard else inCard.clip(well)).background(colors.surface0)
}

/** The `@@` line, faint and slanted, on the hunk band. */
@Composable
private fun HunkHeaderRow(header: String, style: DiffRowStyle, modifier: Modifier) {
  val styledHeader = remember(header) { AnnotatedString(header, HunkHeaderStyle) }
  Row(modifier = modifier.fillMaxWidth().background(style.colors.surface2)) {
    CodeText(
      text = styledHeader,
      style = style,
      color = style.colors.foregroundExtraMuted,
      modifier = Modifier.padding(
        start = OopbotTokens.Spacing.s12,
        top = OopbotTokens.Spacing.s2,
        bottom = OopbotTokens.Spacing.s2,
      ),
    )
  }
}

@Composable
private fun LineRow(line: DiffRow.Line, highlight: SideHighlight?, style: DiffRowStyle, modifier: Modifier) {
  val colors = style.colors
  val (background, numberColor) = when (line.kind) {
    DiffLineKind.Added -> colors.successSoft to colors.dotSuccess
    DiffLineKind.Removed -> colors.dangerSoft to colors.destructive
    DiffLineKind.Context -> Color.Transparent to colors.foregroundExtraMuted
  }
  val gutterWidth = style.gutterWidth
  // The wash again over the gutter makes it a step stronger, whatever height the wrapped line takes.
  val gutter = if (line.kind == DiffLineKind.Context) {
    Modifier
  } else {
    Modifier.drawBehind {
      val width = gutterWidth.toPx()
      val start = if (layoutDirection == LayoutDirection.Ltr) 0f else size.width - width
      drawRect(background, topLeft = Offset(start, 0f), size = Size(width, size.height))
    }
  }
  val text = remember(line, highlight) { highlightedLine(line, highlight) }
  Row(modifier = modifier.fillMaxWidth().background(background).then(gutter)) {
    Text(
      text = line.number?.toString().orEmpty(),
      modifier = Modifier.width(gutterWidth).padding(horizontal = OopbotTokens.Spacing.s8),
      style = style.code,
      color = numberColor,
      textAlign = TextAlign.End,
      maxLines = 1,
    )
    CodeText(
      text = text,
      style = style,
      color = colors.foreground,
      modifier = Modifier.weight(1f).padding(start = OopbotTokens.Spacing.s8),
    )
  }
}

/** [line] coloured from its side's highlighting, or plain until that's done or where the two don't line up. */
private fun highlightedLine(line: DiffRow.Line, highlight: SideHighlight?): AnnotatedString {
  val lines = when (line.kind) {
    DiffLineKind.Removed -> highlight?.oldLines
    DiffLineKind.Added, DiffLineKind.Context -> highlight?.newLines
  }
  // A line holding a lone carriage return splits in two when highlighted, so later lines would get the wrong colours.
  return lines?.getOrNull(line.sideLine)?.takeIf { it.text == line.text } ?: AnnotatedString(line.text)
}

/** Code that wraps in the width it has, or keeps to one line that the [SideScroller] moves sideways. */
@Composable
private fun CodeText(text: AnnotatedString, style: DiffRowStyle, color: Color, modifier: Modifier) {
  val padded = modifier.padding(end = OopbotTokens.Spacing.s12)
  if (style.wrapLines) {
    Text(text = text, modifier = padded, style = style.code, color = color)
    return
  }
  val scroller = style.scroller
  Text(
    text = text,
    modifier = padded.clipToBounds().layout { measurable, constraints ->
      val placeable = measurable.measure(constraints.copy(minWidth = 0, maxWidth = Constraints.Infinity))
      val width = if (constraints.hasBoundedWidth) constraints.maxWidth else placeable.width
      scroller.fit(placeable.width - width)
      layout(width, placeable.height) {
        // Read only while placing, so a sideways scroll moves the text without composing or measuring it again.
        placeable.place((-scroller.offset).roundToInt(), 0)
      }
    },
    style = style.code,
    color = color,
    softWrap = false,
    maxLines = 1,
  )
}

@Composable
private fun PlaceholderRow(kind: GitFileKind, modifier: Modifier) {
  Text(
    text = stringResource(if (kind == GitFileKind.Binary) R.string.git_binary else R.string.git_file_too_large),
    modifier = modifier.fillMaxWidth().padding(OopbotTokens.Spacing.s12),
    color = oopbotColors.foregroundMuted,
    fontSize = OopbotTokens.FontSize.sm,
    textAlign = TextAlign.Center,
  )
}

/** Room for the widest line number of a file plus the gutter's padding. */
private fun gutterWidth(maxLineNumber: Int, digitWidth: Dp): Dp =
  digitWidth * maxLineNumber.coerceAtLeast(1).toString().length + OopbotTokens.Spacing.s8 * 2

/**
 * How far the unwrapped code of every row is scrolled sideways, moved by drags across the document, up to the
 * widest overflow any row has laid out so far.
 */
@Stable
private class SideScroller {
  var offset by mutableFloatStateOf(0f)
    private set

  private var maxOffset = 0f

  val scrollable = ScrollableState { delta ->
    val target = (offset - delta).coerceIn(0f, maxOffset)
    val consumed = offset - target
    offset = target
    consumed
  }

  /** A row laid out [overflow] pixels wider than its room; the scroll can reach its end. */
  fun fit(overflow: Int) {
    if (overflow > maxOffset) maxOffset = overflow.toFloat()
  }
}

private const val HEADER = "header"

private val HunkHeaderStyle = SpanStyle(fontStyle = FontStyle.Italic)

/** The space between file cards, and above the first. */
internal val CARD_GAP = OopbotTokens.Spacing.s12

/** How far a file card sits in from the list's sides. */
internal val CARD_MARGIN = OopbotTokens.Spacing.s12

internal val CardShape = RoundedCornerShape(OopbotTokens.Radius.xl)
internal val CardTopShape = RoundedCornerShape(topStart = OopbotTokens.Radius.xl, topEnd = OopbotTokens.Radius.xl)
private val CardBottomShape =
  RoundedCornerShape(bottomStart = OopbotTokens.Radius.xl, bottomEnd = OopbotTokens.Radius.xl)

/** How far the well of lines sits in from its card's sides and bottom. */
private val WELL_INSET = OopbotTokens.Spacing.s6
private val WELL_RADIUS = OopbotTokens.Radius.lg
private val WellTopShape = RoundedCornerShape(topStart = WELL_RADIUS, topEnd = WELL_RADIUS)
private val WellBottomShape = RoundedCornerShape(bottomStart = WELL_RADIUS, bottomEnd = WELL_RADIUS)
