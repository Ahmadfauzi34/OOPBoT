package io.jeph.oopbot.schedule

import java.time.Instant
import java.time.LocalDateTime
import java.time.YearMonth
import java.time.ZoneId
import java.time.ZoneOffset
import java.time.temporal.ChronoUnit

/**
 * Pure-Kotlin 5-field POSIX cron parser and next-trigger evaluator (`minute hour day-of-month month day-of-week`).
 *
 * Supports wildcards, single values, ranges (`1-5`), steps (wildcard/N, `9-17/2`, `5/10`), comma lists, standard macros
 * (`@hourly`, `@daily`, `@midnight`, `@weekly`, `@monthly`, `@yearly`, `@annually`), and POSIX DOM/DOW OR semantics
 * when both day fields are restricted. Evaluates wall-clock times via [ZoneId], skipping non-existent DST
 * spring-forward gaps and firing once (on the earlier offset) across DST fall-back overlaps.
 */
internal class CronExpression private constructor(
  val source: String,
  private val fields: CronFields,
  private val domUnrestricted: Boolean,
  private val dowUnrestricted: Boolean,
) {
  /**
   * Returns the next epoch millisecond strictly greater than [afterEpochMillis] matching this expression in [zoneId],
   * or `null` if no match occurs within [MAX_SEARCH_YEARS] years.
   */
  fun nextAfter(afterEpochMillis: Long, zoneId: ZoneId): Long? {
    val startZdt = Instant.ofEpochMilli(afterEpochMillis).atZone(zoneId)
    val maxYear = startZdt.year + MAX_SEARCH_YEARS
    var candidate = startZdt.toLocalDateTime().truncatedTo(ChronoUnit.MINUTES).plusMinutes(1)
    val rules = zoneId.rules

    while (candidate.year <= maxYear) {
      val (matched, next) = stepCandidate(candidate)
      if (matched) {
        val offsets = rules.getValidOffsets(next)
        if (offsets.isNotEmpty()) {
          // On DST fall-back overlaps (2 valid offsets), pick the earlier offset once so crons never double-fire.
          val instantMillis = next.toInstant(offsets.first()).toEpochMilli()
          if (instantMillis > afterEpochMillis) return instantMillis
        }
        candidate = next.plusMinutes(1)
      } else {
        candidate = next
      }
    }
    return null
  }

  /**
   * Advances [candidate] either to an exact field match (`true to matchedDateTime`) or to the next boundary after
   * whichever field overflowed (`false to rolledDateTime`).
   */
  private fun stepCandidate(candidate: LocalDateTime): Pair<Boolean, LocalDateTime> {
    val (dateOk, atDate) = stepDate(candidate)
    if (!dateOk) return false to atDate
    val hour = nextSet(fields.hours, atDate.hour)
    if (hour == -1) return false to atDate.plusDays(1).withHour(0).withMinute(0)
    val atHour = if (hour != atDate.hour) atDate.withHour(hour).withMinute(0) else atDate
    val minute = nextSet(fields.minutes, atHour.minute)
    return if (minute == -1) {
      false to atHour.plusHours(1).withMinute(0)
    } else {
      true to if (minute != atHour.minute) atHour.withMinute(minute) else atHour
    }
  }

  private fun stepDate(candidate: LocalDateTime): Pair<Boolean, LocalDateTime> {
    val month = nextSet(fields.months, candidate.monthValue)
    if (month == -1) return false to LocalDateTime.of(candidate.year + 1, 1, 1, 0, 0)
    val atMonth = if (month != candidate.monthValue) LocalDateTime.of(candidate.year, month, 1, 0, 0) else candidate
    val maxDay = YearMonth.of(atMonth.year, atMonth.monthValue).lengthOfMonth()
    val day = nextMatchingDay(atMonth.year, atMonth.monthValue, atMonth.dayOfMonth, maxDay)
    return if (day == -1) {
      false to atMonth.withDayOfMonth(1).plusMonths(1).withHour(0).withMinute(0)
    } else {
      val atDay = if (day != atMonth.dayOfMonth) {
        LocalDateTime.of(atMonth.year, atMonth.monthValue, day, 0, 0)
      } else {
        atMonth
      }
      true to atDay
    }
  }

  private fun nextMatchingDay(year: Int, month: Int, fromDay: Int, maxDay: Int): Int {
    for (day in fromDay..maxDay) {
      val domMatch = fields.daysOfMonth[day]
      val dow = LocalDateTime.of(year, month, day, 0, 0).dayOfWeek.value % 7
      val dowMatch = fields.daysOfWeek[dow]
      val matches = if (!domUnrestricted && !dowUnrestricted) domMatch || dowMatch else domMatch && dowMatch
      if (matches) return day
    }
    return -1
  }

  private fun nextSet(bits: BooleanArray, from: Int): Int {
    for (i in from until bits.size) {
      if (bits[i]) return i
    }
    return -1
  }

  private class CronFields(
    val minutes: BooleanArray,
    val hours: BooleanArray,
    val daysOfMonth: BooleanArray,
    val months: BooleanArray,
    val daysOfWeek: BooleanArray,
  )

  companion object {
    private const val MAX_SEARCH_YEARS = 8

    /** 2024-01-01T00:00:00Z (leap year) used to validate that an expression can actually match at least one date. */
    private const val VALIDATION_EPOCH_MILLIS = 1_704_067_200_000L

    private val MACROS = mapOf(
      "@hourly" to "0 * * * *",
      "@daily" to "0 0 * * *",
      "@midnight" to "0 0 * * *",
      "@weekly" to "0 0 * * 0",
      "@monthly" to "0 0 1 * *",
      "@yearly" to "0 0 1 1 *",
      "@annually" to "0 0 1 1 *",
    )

    /** Parses [raw] or throws [IllegalArgumentException] if the syntax, range, or calendar date is invalid. */
    fun parse(raw: String): CronExpression {
      val trimmed = raw.trim()
      require(trimmed.isNotEmpty()) { "Cron expression must not be blank" }
      val expanded = MACROS[trimmed.lowercase()] ?: trimmed
      val parts = expanded.split(Regex("\\s+"))
      require(parts.size == 5) { "Cron expression must have exactly 5 fields (got ${parts.size}): '$raw'" }

      val fields = CronFields(
        minutes = parseField(parts[0], 0, 59, wrapSevenToZero = false),
        hours = parseField(parts[1], 0, 23, wrapSevenToZero = false),
        daysOfMonth = parseField(parts[2], 1, 31, wrapSevenToZero = false),
        months = parseField(parts[3], 1, 12, wrapSevenToZero = false),
        daysOfWeek = parseField(parts[4], 0, 7, wrapSevenToZero = true),
      )

      val parsed = CronExpression(
        source = trimmed,
        fields = fields,
        domUnrestricted = parts[2] == "*",
        dowUnrestricted = parts[4] == "*",
      )
      requireNotNull(parsed.nextAfter(VALIDATION_EPOCH_MILLIS, ZoneOffset.UTC)) {
        "Cron expression never matches any calendar date: '$raw'"
      }
      return parsed
    }

    private fun parseField(token: String, min: Int, max: Int, wrapSevenToZero: Boolean): BooleanArray {
      val arraySize = if (wrapSevenToZero) 7 else max + 1
      val bits = BooleanArray(arraySize)
      val items = token.split(',')
      require(items.isNotEmpty()) { "Empty cron field" }
      for (item in items) {
        applySegment(item, min, max, wrapSevenToZero, bits)
      }
      require(bits.any { it }) { "Cron field '$token' matched no values" }
      return bits
    }

    private fun applySegment(segment: String, min: Int, max: Int, wrapSevenToZero: Boolean, bits: BooleanArray) {
      require(segment.isNotEmpty()) { "Empty segment in cron field" }
      val slashParts = segment.split('/')
      require(slashParts.size <= 2) { "Invalid step syntax: '$segment'" }
      val step = if (slashParts.size == 2) {
        val parsed = slashParts[1].toIntOrNull()
        require(parsed != null && parsed > 0) { "Cron step must be a positive integer: '$segment'" }
        parsed
      } else {
        1
      }
      val base = slashParts[0]
      val (start, end) = parseBounds(base, min, max, hasStep = slashParts.size == 2)
      var value = start
      while (value <= end) {
        val normalized = if (wrapSevenToZero && value == 7) 0 else value
        bits[normalized] = true
        value += step
      }
    }

    private fun parseBounds(base: String, min: Int, max: Int, hasStep: Boolean): Pair<Int, Int> {
      if (base == "*") return min to max
      if ('-' in base) {
        val rangeParts = base.split('-')
        require(rangeParts.size == 2) { "Invalid cron range: '$base'" }
        val start = parseIntBound(rangeParts[0], min, max)
        val end = parseIntBound(rangeParts[1], min, max)
        require(start <= end) { "Cron range start must be <= end: '$base'" }
        return start to end
      }
      val single = parseIntBound(base, min, max)
      return if (hasStep) single to max else single to single
    }

    private fun parseIntBound(text: String, min: Int, max: Int): Int {
      val value = text.toIntOrNull()
      require(value != null && value in min..max) { "Cron value '$text' out of range $min..$max" }
      return value
    }
  }
}
