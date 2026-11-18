package io.jeph.oopbot.ui.components

import androidx.compose.ui.graphics.Color
import androidx.compose.ui.text.AnnotatedString
import androidx.compose.ui.text.SpanStyle
import androidx.compose.ui.text.buildAnnotatedString
import androidx.compose.ui.text.withStyle
import io.jeph.oopbot.syntax.CaptureSpan
import io.jeph.oopbot.syntax.Grammar
import io.jeph.oopbot.ui.theme.ClayColors
import io.jeph.oopbot.ui.theme.PorcelainColors
import org.junit.Assert.assertEquals
import org.junit.Assert.assertNull
import org.junit.Test

class CodeHighlightingTest {
  @Test
  fun fenceNamesAndAliasesPickTheGrammar() {
    assertEquals(Grammar.KOTLIN, Grammar.forName("Kotlin"))
    assertEquals(Grammar.KOTLIN, Grammar.forName("kt"))
    assertEquals(Grammar.BASH, Grammar.forName("sh"))
    assertEquals(Grammar.TSX, Grammar.forName("tsx title=\"App.tsx\""))
    assertEquals(Grammar.XML, Grammar.forName("xml"))
  }

  @Test
  fun unknownFencesStayPlain() {
    assertNull(Grammar.forName("markdown"))
    assertNull(Grammar.forName("text"))
    assertNull(Grammar.forName(""))
  }

  @Test
  fun aCaptureTakesItsNearestNamedAncestorsToken() {
    assertEquals(Token.Function, tokenFor("function.method.builtin"))
    assertEquals(Token.Constant, tokenFor("tag.attribute"))
    assertEquals(Token.Tag, tokenFor("tag.builtin"))
    assertEquals(Token.Constant, tokenFor("string.special.key"))
    assertNull(tokenFor("_function"))
    assertNull(tokenFor("spell"))
  }

  @Test
  fun aFilesExtensionPicksItsLanguage() {
    assertEquals(Grammar.KOTLIN, grammarOf("app/src/Main.kt"))
    assertEquals(Grammar.TYPESCRIPT, grammarOf("/home/u/.config/app/index.d.ts"))
    assertEquals(Grammar.YAML, grammarOf(".github/workflows/ci.yml"))
    assertEquals(Grammar.DOCKERFILE, grammarOf("docker/app.Dockerfile"))
    assertEquals(Grammar.BASH, grammarOf("/home/u/project/.env"))
    assertNull(fileLanguage("notes."))
    assertNull(fileLanguage("/home/u/README"))
  }

  @Test
  fun aFileKnownByItsWholeNamePicksItsLanguage() {
    assertEquals(Grammar.MAKE, grammarOf("/home/u/project/Makefile"))
    assertEquals(Grammar.MAKE, grammarOf("GNUmakefile"))
    assertEquals(Grammar.DOCKERFILE, grammarOf("Containerfile"))
    assertEquals(Grammar.DOCKERFILE, grammarOf("deploy/Dockerfile.dev"))
    assertEquals(Grammar.BASH, grammarOf("/home/u/.bashrc"))
    assertEquals(Grammar.BASH, grammarOf(".zshrc"))
    assertEquals(Grammar.BASH, grammarOf(".env.local"))
    assertEquals(Grammar.INI, grammarOf("/home/u/.gitconfig"))
  }

  @Test
  fun aWholeNameOnlyCountsAsTheFileName() {
    assertEquals(Grammar.YAML, grammarOf("Makefile/build.yml"))
    assertNull(grammarOf("mydockerfile.dev"))
    assertNull(grammarOf("/home/u/project/env.local"))
  }

  @Test
  fun splitLinesKeepsEachSpanOnItsLine() {
    val red = SpanStyle(color = Color.Red)
    val text = buildAnnotatedString {
      append("a ")
      withStyle(red) { append("\"b\r\nc\"") }
      append("\rd\n")
    }

    val lines = text.splitLines()

    assertEquals(listOf("a \"b", "c\"", "d", ""), lines.map { it.text })
    assertEquals(listOf(AnnotatedString.Range(red, 2, 4)), lines[0].spanStyles)
    assertEquals(listOf(AnnotatedString.Range(red, 0, 2)), lines[1].spanStyles)
    assertEquals(emptyList<AnnotatedString.Range<SpanStyle>>(), lines[2].spanStyles)
  }

  @Test
  fun innerCapturesRepaintTheirPartOfAnOuterOne() {
    // `${x}` inside a template string: the substitution goes plain, its `${` keyword-coloured.
    val text = "`a \${x}`"
    val spans =
      listOf(CaptureSpan(0, 8, STRING), CaptureSpan(3, 7, PLAIN), CaptureSpan(3, 5, KEYWORD), CaptureSpan(5, 6, HELPER))
    val colors = mapOf(STRING to Color.Blue, PLAIN to Color.Unspecified, KEYWORD to Color.Red)

    val painted = paint(text, spans) { colors[it] }

    assertEquals(
      listOf(
        AnnotatedString.Range(SpanStyle(color = Color.Blue), 0, 3),
        AnnotatedString.Range(SpanStyle(color = Color.Red), 3, 5),
        AnnotatedString.Range(SpanStyle(color = Color.Blue), 7, 8),
      ),
      painted.spanStyles,
    )
  }

  @Test
  fun spansPastAShorterTextAreClipped() {
    val painted = paint("ab", listOf(CaptureSpan(1, 5, STRING))) { Color.Blue }

    assertEquals(listOf(AnnotatedString.Range(SpanStyle(color = Color.Blue), 1, 2)), painted.spanStyles)
  }

  @Test
  fun aGrowingBlockKeepsItsColoursAndAddsThePlainRest() {
    val red = SpanStyle(color = Color.Red)
    val highlighted = buildAnnotatedString {
      withStyle(red) { append("val") }
      append(" x")
    }

    val grown = withPlainTail(highlighted, "val x = 1")

    assertEquals("val x = 1", grown.text)
    assertEquals(listOf(AnnotatedString.Range(red, 0, 3)), grown.spanStyles)
    assertEquals(highlighted, withPlainTail(highlighted, "val x"))
  }

  @Test
  fun anEditedOrMissingHighlightFallsBackToPlainText() {
    val highlighted = buildAnnotatedString { withStyle(SpanStyle(color = Color.Red)) { append("val x") } }

    assertEquals(AnnotatedString("var y"), withPlainTail(highlighted, "var y"))
    assertEquals(AnnotatedString("val"), withPlainTail(highlighted, "val"))
    assertEquals(AnnotatedString("val"), withPlainTail(null, "val"))
  }

  @Test
  fun highlightCacheIsolatesByGrammarAndTheme() {
    val cache = HighlightCache(maxEntries = 8, maxChars = 1_000)
    val kotlinLight = AnnotatedString("kt-light")
    val javaLight = AnnotatedString("java-light")
    val kotlinClay = AnnotatedString("kt-clay")

    cache.put(Grammar.KOTLIN, PorcelainColors, "class A", kotlinLight)
    cache.put(Grammar.JAVA, PorcelainColors, "class A", javaLight)
    cache.put(Grammar.KOTLIN, ClayColors, "class A", kotlinClay)

    assertEquals(kotlinLight, cache.get(Grammar.KOTLIN, PorcelainColors, "class A"))
    assertEquals(javaLight, cache.get(Grammar.JAVA, PorcelainColors, "class A"))
    assertEquals(kotlinClay, cache.get(Grammar.KOTLIN, ClayColors, "class A"))
    assertNull(cache.get(Grammar.KOTLIN, PorcelainColors, "class B"))
  }

  @Test
  fun highlightCacheEvictsLeastRecentlyUsedWithinCountAndCharBudgets() {
    val cache = HighlightCache(maxEntries = 2, maxChars = 10)
    cache.put(Grammar.KOTLIN, PorcelainColors, "aaaa", AnnotatedString("1"))
    cache.put(Grammar.KOTLIN, PorcelainColors, "bbbb", AnnotatedString("2"))
    // Refresh "aaaa" so "bbbb" becomes the eldest entry.
    assertEquals(AnnotatedString("1"), cache.get(Grammar.KOTLIN, PorcelainColors, "aaaa"))

    cache.put(Grammar.KOTLIN, PorcelainColors, "cccc", AnnotatedString("3"))

    assertEquals(AnnotatedString("1"), cache.get(Grammar.KOTLIN, PorcelainColors, "aaaa"))
    assertNull(cache.get(Grammar.KOTLIN, PorcelainColors, "bbbb"))
    assertEquals(AnnotatedString("3"), cache.get(Grammar.KOTLIN, PorcelainColors, "cccc"))

    // Entry larger than maxChars / 2 is skipped without evicting existing entries.
    cache.put(Grammar.KOTLIN, PorcelainColors, "123456", AnnotatedString("huge"))
    assertNull(cache.get(Grammar.KOTLIN, PorcelainColors, "123456"))
    assertEquals(AnnotatedString("1"), cache.get(Grammar.KOTLIN, PorcelainColors, "aaaa"))

    cache.clear()
    assertNull(cache.get(Grammar.KOTLIN, PorcelainColors, "aaaa"))
  }

  @Test
  fun sourceLinesCacheEvictsEldestAndSkipsOversizedFiles() {
    val cache = SourceLinesCache(maxEntries = 2, maxChars = 20)
    val first = AnnotatedString("12345678")
    val second = AnnotatedString("abcdefgh")
    val third = AnnotatedString("ijklmnop")
    val huge = AnnotatedString("x".repeat(12))

    cache.put(first, listOf(first))
    cache.put(second, listOf(second))
    assertEquals(listOf(first), cache.get(first))

    cache.put(third, listOf(third))
    assertNull(cache.get(second))
    assertEquals(listOf(first), cache.get(first))
    assertEquals(listOf(third), cache.get(third))

    cache.put(huge, listOf(huge))
    assertNull(cache.get(huge))
    assertEquals(listOf(first), cache.get(first))

    cache.clear()
    assertNull(cache.get(first))
  }

  private fun grammarOf(path: String): Grammar? = fileLanguage(path)?.let(Grammar::forName)

  private companion object {
    const val STRING = 0
    const val PLAIN = 1
    const val KEYWORD = 2
    const val HELPER = 3
  }
}
