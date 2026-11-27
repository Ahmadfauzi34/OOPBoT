package io.jeph.oopbot.agent

import android.content.BroadcastReceiver
import android.content.ComponentName
import android.content.Context
import android.content.Intent
import android.content.pm.PackageManager
import io.jeph.oopbot.appGraph
import kotlinx.coroutines.flow.Flow
import kotlinx.coroutines.flow.distinctUntilChanged

/**
 * Restores OOPBoT's low-power background Standby service after device reboot or app update when the user has
 * enabled Background Standby in Settings. Disabled by default in the manifest and toggled via [follow] so boot
 * broadcasts never cold-start OOPBoT for users who keep Standby off.
 */
class StandbyBootReceiver : BroadcastReceiver() {
  override fun onReceive(context: Context, intent: Intent?) {
    val action = intent?.action ?: return
    if (action != Intent.ACTION_BOOT_COMPLETED && action != Intent.ACTION_MY_PACKAGE_REPLACED) return
    val graph = context.applicationContext.appGraph
    if (!graph.preferences.setupComplete.value || !graph.preferences.backgroundStandby.value) return
    graph.syncStandby(action)
  }

  companion object {
    /** Follows [enabled] until cancelled. PackageManager calls block, so call off the main thread. */
    suspend fun follow(context: Context, enabled: Flow<Boolean>) {
      enabled.distinctUntilChanged().collect { syncEnabled(context, it) }
    }

    internal fun syncEnabled(context: Context, enabled: Boolean) {
      val component = ComponentName(context, StandbyBootReceiver::class.java)
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
