package io.jeph.oopbot.agent

import android.content.Context
import android.util.Log
import java.util.concurrent.atomic.AtomicBoolean

/** Power tier requested by [KeepAlive] on [AgentService]. */
internal enum class KeepAliveMode {
  /** Service stopped; OS may sleep or reclaim the process freely. */
  Off,

  /** Foreground service alive ("semi-alive") with NO wake lock so CPU can Doze normally. */
  Standby,

  /** Foreground service alive WITH a partial wake lock during active burst work. */
  Active,
}

/**
 * Reference-counted hold on [AgentService] with two tiers:
 * - [acquire] ([KeepAliveMode.Active]): foreground service + partial wake lock during active turns, terminal
 *   commands, sign-in, installs, backups, or local-model inference.
 * - [acquireStandby] ([KeepAliveMode.Standby]): foreground service WITHOUT a wake lock so OOPBoT stays ready
 *   in the background (waiting on background tasks, schedules, or external wakeups) with minimal battery impact.
 */
internal class KeepAlive internal constructor(private val updater: (KeepAliveMode, String) -> Boolean) {
  constructor(context: Context) : this(
    updater = { mode, _ -> AgentService.update(context.applicationContext, mode) },
  )

  private val lock = Any()
  private var activeHolders = 0
  private var standbyHolders = 0

  @Volatile
  var mode: KeepAliveMode = KeepAliveMode.Off
    private set

  /** Holds [AgentService] in [KeepAliveMode.Active] (foreground + partial wake lock) until closed. */
  fun acquire(reason: String): AutoCloseable = hold(active = true, reason = reason)

  /** Holds [AgentService] in [KeepAliveMode.Standby] (foreground, no wake lock) until closed. */
  fun acquireStandby(reason: String): AutoCloseable = hold(active = false, reason = reason)

  private fun hold(active: Boolean, reason: String): AutoCloseable {
    synchronized(lock) {
      if (active) activeHolders += 1 else standbyHolders += 1
      applyTransitionLocked(reason)
    }
    val released = AtomicBoolean(false)
    return AutoCloseable {
      if (released.compareAndSet(false, true)) {
        synchronized(lock) {
          if (active) activeHolders -= 1 else standbyHolders -= 1
          applyTransitionLocked(reason)
        }
      }
    }
  }

  private fun applyTransitionLocked(reason: String) {
    val desired = when {
      activeHolders > 0 -> KeepAliveMode.Active
      standbyHolders > 0 -> KeepAliveMode.Standby
      else -> KeepAliveMode.Off
    }
    if (desired == mode) return
    mode = desired
    if (!updater(desired, reason)) {
      Log.w(TAG, "Keep-alive ($reason, mode=$desired) not applied; Android may stop OOPBoT in the background")
    }
  }

  private companion object {
    const val TAG = "OOPBoT"
  }
}
