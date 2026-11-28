package io.jeph.oopbot.schedule

import android.app.AlarmManager
import android.app.PendingIntent
import android.content.Context
import android.content.Intent
import android.os.Build
import android.util.Log

/** Low-level alarm driver abstracted for deterministic unit testing. */
internal interface AlarmDriver {
  fun canScheduleExactAlarms(): Boolean

  fun scheduleInexact(triggerAtMillis: Long)

  fun scheduleExact(triggerAtMillis: Long)

  fun cancel()
}

/**
 * Coordinates the single Android [AlarmManager] wake alarm pointing at the earliest enabled job (`MIN(next_run_at)`).
 *
 * Defaults to `setAndAllowWhileIdle` (zero extra manifest permissions, Doze-capable) and upgrades to
 * `setExactAndAllowWhileIdle` only when the user opts into exact timing AND Android grants `SCHEDULE_EXACT_ALARM`,
 * catching `SecurityException` as defense-in-depth if permission is revoked concurrently.
 */
internal class AlarmScheduler(private val driver: AlarmDriver) {
  constructor(context: Context) : this(AndroidAlarmDriver(context.applicationContext))

  fun canScheduleExactAlarms(): Boolean = driver.canScheduleExactAlarms()

  /** Arms the single wake alarm for [nextTriggerAtMillis], or cancels any existing alarm when null. */
  fun sync(nextTriggerAtMillis: Long?, preferExact: Boolean) {
    if (nextTriggerAtMillis == null) {
      driver.cancel()
      return
    }
    if (preferExact && driver.canScheduleExactAlarms()) {
      try {
        driver.scheduleExact(nextTriggerAtMillis)
        return
      } catch (error: SecurityException) {
        Log.w(TAG, "Exact alarm permission revoked; falling back to inexact allow-while-idle alarm", error)
      }
    }
    driver.scheduleInexact(nextTriggerAtMillis)
  }

  private companion object {
    const val TAG = "OOPBoT"
  }
}

private class AndroidAlarmDriver(private val context: Context) : AlarmDriver {
  private val alarmManager: AlarmManager = context.getSystemService(AlarmManager::class.java)

  override fun canScheduleExactAlarms(): Boolean =
    Build.VERSION.SDK_INT < Build.VERSION_CODES.S || alarmManager.canScheduleExactAlarms()

  override fun scheduleInexact(triggerAtMillis: Long) {
    alarmManager.setAndAllowWhileIdle(AlarmManager.RTC_WAKEUP, triggerAtMillis, alarmPendingIntent())
  }

  override fun scheduleExact(triggerAtMillis: Long) {
    alarmManager.setExactAndAllowWhileIdle(AlarmManager.RTC_WAKEUP, triggerAtMillis, alarmPendingIntent())
  }

  override fun cancel() {
    alarmManager.cancel(alarmPendingIntent())
  }

  private fun alarmPendingIntent(): PendingIntent = PendingIntent.getBroadcast(
    context,
    ALARM_REQUEST_CODE,
    Intent(context, ScheduleAlarmReceiver::class.java).setAction(ScheduleAlarmReceiver.ACTION_RUN_DUE),
    PendingIntent.FLAG_UPDATE_CURRENT or PendingIntent.FLAG_IMMUTABLE,
  )

  private companion object {
    const val ALARM_REQUEST_CODE = 40
  }
}
