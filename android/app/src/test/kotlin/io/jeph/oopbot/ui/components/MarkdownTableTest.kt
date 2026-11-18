package io.jeph.oopbot.ui.components

import androidx.compose.ui.unit.Constraints
import org.junit.Assert.assertArrayEquals
import org.junit.Assert.assertEquals
import org.junit.Assert.assertSame
import org.junit.Assert.assertTrue
import org.junit.Test

class MarkdownTableTest {
  @Test
  fun columnsThatFitOnOneLineTakeOnlyWhatTheyNeed() {
    val widths = columnWidths(intArrayOf(20, 30), intArrayOf(40, 60), available = 400, FLOOR, CAP)
    assertArrayEquals(intArrayOf(40, 60), widths)
  }

  @Test
  fun columnsFillingTheWidthExactlyStayOnOneLine() {
    val widths = columnWidths(intArrayOf(20, 30), intArrayOf(150, 250), available = 400, FLOOR, CAP)
    assertArrayEquals(intArrayOf(150, 250), widths)
  }

  @Test
  fun shortColumnsKeepTheirWidthAndLongOnesShareTheRest() {
    val widths = columnWidths(intArrayOf(60, 30, 70), intArrayOf(100, 40, 900), available = 400, FLOOR, CAP)
    assertArrayEquals(intArrayOf(100, 40, 260), widths)
  }

  @Test
  fun sharedRoomFillsTheWidthExactly() {
    val widths = columnWidths(intArrayOf(10, 10, 10), intArrayOf(300, 301, 302), available = 500, FLOOR, CAP)
    assertEquals(500, widths.sum())
    assertTrue(widths.all { it >= FLOOR })
  }

  @Test
  fun columnsThatCannotFitAtTheirFloorScroll() {
    val widths = columnWidths(IntArray(7) { 50 }, IntArray(7) { 500 }, available = 400, FLOOR, CAP)
    assertArrayEquals(IntArray(7) { FLOOR }, widths)
  }

  @Test
  fun aLongUnbreakableWordHoldsItsColumnOpenOnlyToTheCap() {
    val widths = columnWidths(intArrayOf(1000, 50), intArrayOf(1000, 600), available = 300, FLOOR, CAP)
    assertArrayEquals(intArrayOf(CAP, FLOOR), widths)
  }

  @Test
  fun widthsCacheReusesWidthsAndAvoidsRemeasuringIntrinsicsOnResize() {
    val cache = TableWidthsCache(columns = 3)
    var intrinsicPasses = 0
    val measure: (IntArray, IntArray) -> Unit = { minimums, naturals ->
      intrinsicPasses += 1
      minimums[0] = 60
      minimums[1] = 30
      minimums[2] = 70
      naturals[0] = 100
      naturals[1] = 40
      naturals[2] = 900
    }

    val first = cache.widths(available = 400, FLOOR, CAP, measure)
    val repeat = cache.widths(available = 400, FLOOR, CAP, measure)
    val probe = cache.widths(available = Constraints.Infinity, FLOOR, CAP, measure)
    val afterProbe = cache.widths(available = 400, FLOOR, CAP, measure)
    val resized = cache.widths(available = 500, FLOOR, CAP, measure)

    assertSame(first, repeat)
    assertSame(first, afterProbe)
    assertArrayEquals(intArrayOf(100, 40, 900), probe)
    assertArrayEquals(intArrayOf(100, 40, 360), resized)
    assertEquals(1, intrinsicPasses)
  }

  private companion object {
    const val FLOOR = 120
    const val CAP = 200
  }
}
