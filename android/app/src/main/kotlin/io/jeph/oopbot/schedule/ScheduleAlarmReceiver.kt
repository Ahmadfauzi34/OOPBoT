package io.jeph.oopbot.schedule

import android.content.BroadcastReceiver
import android.content.ComponentName
import android.content.Context
import android.content.Intent
import android.content.pm.PackageManager
import io.jeph.oopbot.appGraph
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.flow.Flow
import kotlinx.coroutines.flow.distinctUntilChanged
import kotlinx.coroutines.launch

/**
 * Receives wake alarms from [AlarmScheduler] and system clock/boot broadcasts to re-arm schedules.
 *
 * Disabled by default in `AndroidManifest.xml` and dynamically enabled via [follow] only when at least one enabled
 * schedule exists, so reboot/time broadcasts never cold-start OOPBoT for users without active schedules.
 */
class ScheduleAlarmReceiver : BroadcastReceiver() {
  override fun onReceive(context: Context, intent: Intent?) {
    val action = intent?.action ?: return
    if (action !in HANDLED_ACTIONS) return
    val graph = context.applicationContext.appGraph
    when (action) {
      ACTION_RUN_DUE -> {
        if (!graph.preferences.setupComplete.value) return
        // Acquire Active FGS synchronously inside AlarmManager's broadcast exemption window; ScheduleRunner closes it.
        val hold = graph.keepAlive.acquire("scheduled job")
        graph.scheduleRunner.dispatchDueJobs(initialHold = hold)
      }

      Intent.ACTION_BOOT_COMPLETED, Intent.ACTION_MY_PACKAGE_REPLACED -> {
        val pendingResult = goAsync()
        graph.scope.launch(Dispatchers.IO) {
          try {
            graph.scheduleRepository.resyncAfterBoot()
          } finally {
            pendingResult?.finish()
          }
        }
      }

      Intent.ACTION_TIME_CHANGED, Intent.ACTION_TIMEZONE_CHANGED -> {
        val timezoneChanged = action == Intent.ACTION_TIMEZONE_CHANGED
        val pendingResult = goAsync()
        graph.scope.launch(Dispatchers.IO) {
          try {
            graph.scheduleRepository.recomputeForClockChange(timezoneChanged = timezoneChanged)
          } finally {
            pendingResult?.finish()
          }
        }
      }
    }
  }

  companion object {
    const val ACTION_RUN_DUE = "io.jeph.oopbot.action.RUN_DUE_SCHEDULES"

    private val HANDLED_ACTIONS = setOf(
      ACTION_RUN_DUE,
      Intent.ACTION_BOOT_COMPLETED,
      Intent.ACTION_MY_PACKAGE_REPLACED,
      Intent.ACTION_TIME_CHANGED,
      Intent.ACTION_TIMEZONE_CHANGED,
    )

    /** Follows [enabled] until cancelled. PackageManager calls block, so call off the main thread. */
    suspend fun follow(context: Context, enabled: Flow<Boolean>) {
      enabled.distinctUntilChanged().collect { syncEnabled(context, it) }
    }

    internal fun syncEnabled(context: Context, enabled: Boolean) {
      val component = ComponentName(context, ScheduleAlarmReceiver::class.java)
      val wanted = if (enabled) {
        PackageManager.COMPONENT_ENABLED_STATE_ENABLED
      } else {
        PackageManager.COMPONENT_ENABLED_STATE_DEFAULT
      }
      val pm = context.packageManager
      if (pm.getComponentEnabledSetting(component) == wanted) return
      pm.setComponentEnabledSetting(component, wanted, PackageManager.DONT_KILL_APP)
    }
  }
}
