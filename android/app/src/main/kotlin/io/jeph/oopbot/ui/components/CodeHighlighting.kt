package io.jeph.oopbot.ui.components

import androidx.compose.runtime.Composable
import androidx.compose.runtime.LaunchedEffect
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.remember
import androidx.compose.runtime.rememberUpdatedState
import androidx.compose.runtime.setValue
import androidx.compose.runtime.snapshotFlow
import androidx.compose.ui.graphics.Color
import androidx.compose.ui.platform.LocalContext
import androidx.compose.ui.text.AnnotatedString
import androidx.compose.ui.text.SpanStyle
import androidx.compose.ui.text.buildAnnotatedString
import io.jeph.oopbot.syntax.CaptureSpan
import io.jeph.oopbot.syntax.Grammar
import io.jeph.oopbot.syntax.LoadedGrammar
import io.jeph.oopbot.syntax.SyntaxSession
import io.jeph.oopbot.syntax.loadGrammar
import io.jeph.oopbot.ui.theme.OopbotColors
import io.jeph.oopbot.ui.theme.oopbotColors
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.flow.conflate
import kotlinx.coroutines.withContext

/**
 * Bounded thread-safe in-memory cache of Tree-Sitter highlighted [AnnotatedString]s, keyed by grammar, theme
 * palette and source text. Avoids flashing plain text and opening native [SyntaxSession]s again as code blocks
 * re-enter the viewport.
 */
internal class HighlightCache(
  private val maxEntries: Int = DEFAULT_MAX_ENTRIES,
  private val maxChars: Int = DEFAULT_MAX_CHARS,
) {
  private data class Key(val grammar: Grammar, val colors: OopbotColors, val code: String)

  private val lock = Any()
  private val entries = LinkedHashMap<Key, AnnotatedString>(64, 0.75f, true)
  private var totalChars = 0

  fun get(grammar: Grammar, colors: OopbotColors, code: String): AnnotatedString? =
    synchronized(lock) { entries[Key(grammar, colors, code)] }

  fun put(grammar: Grammar, colors: OopbotColors, code: String, highlighted: AnnotatedString) {
    if (code.length > maxChars / 2) return
    val key = Key(grammar, colors, code)
    synchronized(lock) {
      val previous = entries.put(key, highlighted)
      if (previous == null) totalChars += code.length
      val iterator = entries.entries.iterator()
      while ((entries.size > maxEntries || totalChars > maxChars) && iterator.hasNext()) {
        val eldest = iterator.next()
        totalChars -= eldest.key.code.length
        iterator.remove()
      }
    }
  }

  fun clear() = synchronized(lock) {
    entries.clear()
    totalChars = 0
  }

  internal companion object {
    const val DEFAULT_MAX_ENTRIES = 128
    const val DEFAULT_MAX_CHARS = 512_000
    val shared = HighlightCache()
  }
}

/**
 * Bounded thread-safe LRU cache of pre-split source lines so switching back to an open file tab
 * renders lines from frame 1 without an empty flash while splitting off the main thread.
 */
internal class SourceLinesCache(
  private val maxEntries: Int = DEFAULT_MAX_ENTRIES,
  private val maxChars: Int = DEFAULT_MAX_CHARS,
) {
  private val lock = Any()
  private val entries = LinkedHashMap<AnnotatedString, List<AnnotatedString>>(16, 0.75f, true)
  private var totalChars = 0

  fun get(source: AnnotatedString): List<AnnotatedString>? = synchronized(lock) { entries[source] }

  fun put(source: AnnotatedString, lines: List<AnnotatedString>) {
    if (source.length > maxChars / 2) return
    synchronized(lock) {
      val previous = entries.put(source, lines)
      if (previous == null) totalChars += source.length
      val iterator = entries.entries.iterator()
      while ((entries.size > maxEntries || totalChars > maxChars) && iterator.hasNext()) {
        val eldest = iterator.next()
        totalChars -= eldest.key.length
        iterator.remove()
      }
    }
  }

  fun clear() = synchronized(lock) {
    entries.clear()
    totalChars = 0
  }

  internal companion object {
    const val DEFAULT_MAX_ENTRIES = 16
    const val DEFAULT_MAX_CHARS = 512_000
    val shared = SourceLinesCache()
  }
}

/**
 * [code] coloured for [language] (a fence name or file extension), or plain when no grammar knows it.
 * Highlighting runs off the main thread, one pass at a time with the latest text, reparsing only what
 * changed; while a streamed block grows, the colours already computed stay on its start instead of
 * flashing back to plain. Finished blocks read directly from [HighlightCache.shared] on the first frame.
 */
@Composable
internal fun rememberHighlightedCode(code: String, language: String?): AnnotatedString {
  val grammar = language?.let(Grammar::forName) ?: return AnnotatedString(code)
  val themeColors = oopbotColors
  val cached = remember(code, grammar, themeColors) { HighlightCache.shared.get(grammar, themeColors, code) }
  if (cached != null) return cached
  val palette = remember(themeColors) { palette(themeColors) }
  val assets = LocalContext.current.assets
  val latestCode by rememberUpdatedState(code)
  var highlighted by remember(grammar, themeColors) { mutableStateOf<AnnotatedString?>(null) }
  LaunchedEffect(grammar, themeColors) {
    var colors: List<Color?> = emptyList()
    var session: SyntaxSession? = null
    try {
      snapshotFlow { latestCode }.conflate().collect { text ->
        val hit = HighlightCache.shared.get(grammar, themeColors, text)
        if (hit != null) {
          highlighted = hit
        } else {
          val active = session ?: withContext(Dispatchers.Default) {
            val loaded = loadGrammar(grammar, assets)
            colors = captureColors(loaded, palette)
            SyntaxSession(loaded)
          }.also { session = it }
          val painted = withContext(Dispatchers.Default) {
            paint(text, active.captures(text) { colors[it] != null }, colors::get)
          }
          HighlightCache.shared.put(grammar, themeColors, text, painted)
          highlighted = painted
        }
      }
    } finally {
      // Cancelling waits for a running parse to return, so the session is never closed under one.
      session?.close()
    }
  }
  return withPlainTail(highlighted, code)
}

/**
 * [code] coloured by [grammar] for the [theme]'s palette, in one parse on the calling thread, for callers that colour
 * many texts off the main thread themselves.
 */
internal fun highlightCode(code: String, grammar: LoadedGrammar, theme: OopbotColors): AnnotatedString {
  val colors = captureColors(grammar, palette(theme))
  return SyntaxSession(grammar).use { session ->
    paint(code, session.captures(code) { colors[it] != null }, colors::get)
  }
}

/** Each of [grammar]'s captures' colour in [palette], by capture index; null for one that colours nothing. */
private fun captureColors(grammar: LoadedGrammar, palette: Map<Token, Color>): List<Color?> =
  grammar.captureNames.map { name -> tokenFor(name)?.let(palette::getValue) }

/** [highlighted] extended with the rest of [code] when [code] continues it; otherwise [code] unstyled. */
internal fun withPlainTail(highlighted: AnnotatedString?, code: String): AnnotatedString = when {
  highlighted == null || !code.startsWith(highlighted.text) -> AnnotatedString(code)
  highlighted.length == code.length -> highlighted
  else -> highlighted + AnnotatedString(code.substring(highlighted.length))
}

/**
 * The language [path]'s file name picks, which [Grammar.forName] reads like a fence name: a file known by its whole
 * name (`Makefile`, `.bashrc`, `Dockerfile.dev`, `.env.local`) picks its language, any other its extension. Null
 * without either.
 */
internal fun fileLanguage(path: String): String? {
  val name = path.substringAfterLast('/')
  val lowercaseName = name.lowercase()
  return when {
    lowercaseName in FILE_NAME_LANGUAGES -> FILE_NAME_LANGUAGES.getValue(lowercaseName)
    lowercaseName.startsWith("dockerfile.") -> "dockerfile"
    lowercaseName.startsWith(".env.") -> "sh"
    else -> name.substringAfterLast('.', "").ifEmpty { null }
  }
}

/** Files with no extension, or a dotfile's name in its place, by lowercase name. */
private val FILE_NAME_LANGUAGES = mapOf(
  "containerfile" to "dockerfile",
  "dockerfile" to "dockerfile",
  "gnumakefile" to "make",
  "makefile" to "make",
  ".bash_login" to "sh",
  ".bash_logout" to "sh",
  ".bash_profile" to "sh",
  ".bashrc" to "sh",
  ".envrc" to "sh",
  ".profile" to "sh",
  ".zlogin" to "sh",
  ".zlogout" to "sh",
  ".zprofile" to "sh",
  ".zshenv" to "sh",
  ".zshrc" to "sh",
  ".editorconfig" to "ini",
  ".gitconfig" to "ini",
  ".gitmodules" to "ini",
)

/**
 * Each line of this text with its styles, split at `\n`, `\r\n` or `\r` like [String.lines]. One pass over the
 * styles, so a whole file's worth of lines costs about its length rather than lines times styles.
 */
internal fun AnnotatedString.splitLines(): List<AnnotatedString> {
  val styles = spanStyles.sortedBy { it.start }
  val lines = mutableListOf<AnnotatedString>()
  var next = 0
  val open = mutableListOf<AnnotatedString.Range<SpanStyle>>()
  fun addLine(start: Int, end: Int) {
    while (next < styles.size && styles[next].start < end) open += styles[next++]
    val lineStyles = open.mapNotNull { style ->
      if (style.end <= start || style.start >= end) return@mapNotNull null
      AnnotatedString.Range(style.item, maxOf(style.start, start) - start, minOf(style.end, end) - start)
    }
    open.removeAll { it.end <= end }
    lines += AnnotatedString(text.substring(start, end), lineStyles)
  }
  // A scan rather than Regex.findAll: on Android each match makes a Matcher that copies the whole text, so a file of
  // many lines would take its length times its line count.
  var start = 0
  var index = 0
  while (index < length) {
    val char = text[index]
    if (char != '\n' && char != '\r') {
      index++
      continue
    }
    addLine(start, index)
    index += if (char == '\r' && text.getOrNull(index + 1) == '\n') 2 else 1
    start = index
  }
  addLine(start, length)
  return lines
}

/** How OOPBoT colours a highlight capture. [Plain] takes the text's own colour, even inside a coloured capture. */
internal enum class Token { Plain, Keyword, String, Constant, Comment, Function, Type, Tag }

/**
 * The token a capture name colours as: its own entry, else its parent's (`function.method.builtin`, then
 * `function.method`, then `function`). Null for captures that colour nothing, such as `@_name` helpers.
 */
internal fun tokenFor(capture: String): Token? {
  var name = capture
  while (true) {
    TOKENS[name]?.let { return it }
    if ('.' !in name) return null
    name = name.substringBeforeLast('.')
  }
}

/**
 * [text] coloured by [spans], outermost first as [io.jeph.oopbot.syntax.captureSpans] gives them, so an
 * inner capture repaints its part of an outer one. [colorOf] gives a capture's colour: null leaves what is
 * under it, [Color.Unspecified] makes it plain.
 */
internal fun paint(text: String, spans: List<CaptureSpan>, colorOf: (capture: Int) -> Color?): AnnotatedString {
  val colors = arrayOfNulls<Color>(text.length)
  for (span in spans) {
    val color = colorOf(span.capture) ?: continue
    // Kept inside the text: a range past the end would throw and take the app down with it.
    colors.fill(color, span.start.coerceIn(0, text.length), span.end.coerceIn(0, text.length))
  }
  return buildAnnotatedString {
    append(text)
    var start = 0
    while (start < text.length) {
      val color = colors[start]
      var end = start + 1
      while (end < text.length && colors[end] == color) end++
      if (color != null && color != Color.Unspecified) addStyle(SpanStyle(color = color), start, end)
      start = end
    }
  }
}

private val TOKENS = mapOf(
  "comment" to Token.Comment,
  "string" to Token.String,
  "character" to Token.String,
  "string.escape" to Token.Constant,
  "string.special" to Token.Constant,
  "character.special" to Token.Constant,
  "escape" to Token.Constant,
  "keyword" to Token.Keyword,
  "conditional" to Token.Keyword,
  "repeat" to Token.Keyword,
  "include" to Token.Keyword,
  "exception" to Token.Keyword,
  "operator" to Token.Keyword,
  "preproc" to Token.Keyword,
  "punctuation.special" to Token.Keyword,
  "number" to Token.Constant,
  "identifier.constant" to Token.Constant,
  "float" to Token.Constant,
  "boolean" to Token.Constant,
  "constant" to Token.Constant,
  "attribute" to Token.Constant,
  "tag.attribute" to Token.Constant,
  "variable.builtin" to Token.Constant,
  "function" to Token.Function,
  "method" to Token.Function,
  "constructor" to Token.Function,
  "type" to Token.Type,
  "module" to Token.Type,
  "namespace" to Token.Type,
  "tag" to Token.Tag,
  "tag.delimiter" to Token.Plain,
  "variable" to Token.Plain,
  "property" to Token.Plain,
  "field" to Token.Plain,
  "label" to Token.Plain,
  "punctuation" to Token.Plain,
  "delimiter" to Token.Plain,
  "embedded" to Token.Plain,
  "none" to Token.Plain,
)

/** The syntax palette, by token, from the theme's own roles so code reads as part of the app. */
private fun palette(colors: OopbotColors) = mapOf(
  Token.Plain to Color.Unspecified,
  Token.Keyword to colors.accent,
  Token.String to colors.dotSuccess,
  Token.Constant to colors.info,
  Token.Comment to colors.codeComment,
  Token.Function to colors.dotWarning,
  Token.Type to colors.extension,
  Token.Tag to colors.dotDanger,
)
