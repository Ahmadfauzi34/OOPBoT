package io.jeph.oopbot.schedule

import java.time.Instant
import java.time.ZoneId
import java.time.ZoneOffset
import java.time.ZonedDateTime
import org.junit.Assert.assertEquals
import org.junit.Assert.assertNotNull
import org.junit.Assert.assertThrows
import org.junit.Test

class CronExpressionTest {
  @Test
  fun evaluatesRangesStepsListsAndDowSevenWrap() {
    val expr = CronExpression.parse("*/15 9-17/2 * * 0,7")
    // 2025-01-05 is Sunday.
    val base = zdt("2025-01-05T08:50:00Z")
    val first = expr.nextAfter(base, ZoneOffset.UTC)
    assertEquals(zdt("2025-01-05T09:00:00Z"), first)

    val second = expr.nextAfter(first!!, ZoneOffset.UTC)
    assertEquals(zdt("2025-01-05T09:15:00Z"), second)

    // After 09:45, next matching hour is 11:00 because of 9-17/2.
    val afterNineFortyFive = expr.nextAfter(zdt("2025-01-05T09:45:00Z"), ZoneOffset.UTC)
    assertEquals(zdt("2025-01-05T11:00:00Z"), afterNineFortyFive)

    // After 17:45 Sunday, skips Mon-Sat to next Sunday 09:00.
    val nextWeek = expr.nextAfter(zdt("2025-01-05T17:45:00Z"), ZoneOffset.UTC)
    assertEquals(zdt("2025-01-12T09:00:00Z"), nextWeek)
  }

  @Test
  fun supportsStandardMacrosCaseInsensitively() {
    val base = zdt("2025-03-15T10:30:00Z")
    assertEquals(zdt("2025-03-15T11:00:00Z"), CronExpression.parse("@hourly").nextAfter(base, ZoneOffset.UTC))
    assertEquals(zdt("2025-03-16T00:00:00Z"), CronExpression.parse("@DAILY").nextAfter(base, ZoneOffset.UTC))
    assertEquals(zdt("2025-03-16T00:00:00Z"), CronExpression.parse("@midnight").nextAfter(base, ZoneOffset.UTC))
    assertEquals(zdt("2025-03-16T00:00:00Z"), CronExpression.parse("@weekly").nextAfter(base, ZoneOffset.UTC))
    assertEquals(zdt("2025-04-01T00:00:00Z"), CronExpression.parse("@monthly").nextAfter(base, ZoneOffset.UTC))
    assertEquals(zdt("2026-01-01T00:00:00Z"), CronExpression.parse("@yearly").nextAfter(base, ZoneOffset.UTC))
    assertEquals(zdt("2026-01-01T00:00:00Z"), CronExpression.parse("@annually").nextAfter(base, ZoneOffset.UTC))
  }

  @Test
  fun appliesPosixOrSemanticsWhenBothDomAndDowAreRestricted() {
    // 15th of month OR Monday (1) at 08:00.
    // In Jan 2025: Jan 13 is Monday, Jan 15 is Wednesday.
    val expr = CronExpression.parse("0 8 15 * 1")
    val afterJan12 = expr.nextAfter(zdt("2025-01-12T08:00:00Z"), ZoneOffset.UTC)
    assertEquals(zdt("2025-01-13T08:00:00Z"), afterJan12)

    val afterJan13 = expr.nextAfter(afterJan12!!, ZoneOffset.UTC)
    assertEquals(zdt("2025-01-15T08:00:00Z"), afterJan13)
  }

  @Test
  fun matchesLeapDayAcrossYears() {
    val expr = CronExpression.parse("0 0 29 2 *")
    val next = expr.nextAfter(zdt("2025-01-01T00:00:00Z"), ZoneOffset.UTC)
    assertEquals(zdt("2028-02-29T00:00:00Z"), next)
  }

  @Test
  fun skipsDstSpringForwardGap() {
    val ny = ZoneId.of("America/New_York")
    // 2025-03-09 clocks jump 02:00 EST -> 03:00 EDT; 02:30 does not exist.
    val expr = CronExpression.parse("30 2 * * *")
    val beforeGap = ZonedDateTime.of(2025, 3, 9, 1, 59, 0, 0, ny).toInstant().toEpochMilli()
    val next = expr.nextAfter(beforeGap, ny)
    assertNotNull(next)
    val expected = ZonedDateTime.of(2025, 3, 10, 2, 30, 0, 0, ny).toInstant().toEpochMilli()
    assertEquals(expected, next)
  }

  @Test
  fun firesOnceOnDstFallBackOverlap() {
    val ny = ZoneId.of("America/New_York")
    // 2025-11-02 clocks fall back 02:00 EDT (-04:00) -> 01:00 EST (-05:00); 01:30 occurs twice.
    val expr = CronExpression.parse("30 1 * * *")
    val beforeFirst = Instant.parse("2025-11-02T05:15:00Z").toEpochMilli() // 01:15 EDT
    val firstFire = expr.nextAfter(beforeFirst, ny)
    assertEquals(Instant.parse("2025-11-02T05:30:00Z").toEpochMilli(), firstFire) // 01:30 EDT (earlier offset)

    // Immediately after 01:30 EDT (and even during 01:15 EST second pass), it must NOT fire again on Nov 2.
    val expectedNextDay = ZonedDateTime.of(2025, 11, 3, 1, 30, 0, 0, ny).toInstant().toEpochMilli()
    assertEquals(expectedNextDay, expr.nextAfter(firstFire!!, ny))
    val duringSecondPass = Instant.parse("2025-11-02T06:15:00Z").toEpochMilli() // 01:15 EST
    assertEquals(expectedNextDay, expr.nextAfter(duringSecondPass, ny))
  }

  @Test
  fun rejectsInvalidSyntaxAndImpossibleCalendarDates() {
    val invalid = listOf(
      "",
      "   ",
      "* * * *",
      "* * * * * *",
      "60 * * * *",
      "* 24 * * *",
      "* * 0 * *",
      "* * 32 * *",
      "* * * 13 *",
      "* * * * 8",
      "*/0 * * * *",
      "10-5 * * * *",
      "0 0 31 2 *",
      "0 0 31 4 *",
    )
    for (expr in invalid) {
      assertThrows("Expected failure for '$expr'", IllegalArgumentException::class.java) {
        CronExpression.parse(expr)
      }
    }
  }

  private fun zdt(iso: String): Long = Instant.parse(iso).toEpochMilli()
}
