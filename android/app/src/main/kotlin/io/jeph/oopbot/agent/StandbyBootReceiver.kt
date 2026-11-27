package io.jeph.oopbot.agent

import android.content.BroadcastReceiver
import android.content.Context
import android.content.Intent
import io.jeph.oopbot.appGraph

/**
 * Restores OOPBoT's low-power background Standby service after device reboot or app update when the user has
 * enabled Background Standby in Settings.
 */
class StandbyBootReceiver : BroadcastReceiver() {
  override fun onReceive(context: Context, intent: Intent?) {
    val action = intent?.action ?: return
    if (action != Intent.ACTION_BOOT_COMPLETED && action != Intent.ACTION_MY_PACKAGE_REPLACED) return
    val graph = context.applicationContext.appGraph
    if (!graph.preferences.setupComplete.value || !graph.preferences.backgroundStandby.value) return
    // Accessing appGraph starts holdWhileWorking(), which acquires the Standby hold on Main.immediate.
    if (graph.keepAlive.mode == KeepAliveMode.Off) {
      AgentService.update(context.applicationContext, KeepAliveMode.Standby)
    }
  }
}
