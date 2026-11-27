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
  private var draining = false
  private var pendingSync = false
  private var pendingForce = false
  private var pendingReason = ""
  private var appliedGeneration = 0L

  /** Desired mode derived from current holds; updated synchronously before service IPC completes. */
  @Volatile
  var mode: KeepAliveMode = KeepAliveMode.Off
    private set

  /** Last mode successfully accepted by [updater] (or reported running/stopped by [AgentService]). */
  @Volatile
  var appliedMode: KeepAliveMode = KeepAliveMode.Off
    private set

  /** Holds [AgentService] in [KeepAliveMode.Active] (foreground + partial wake lock) until closed. */
  fun acquire(reason: String): AutoCloseable = hold(active = true, reason = reason, dispatch = true)

  /**
   * Holds [AgentService] in [KeepAliveMode.Standby] (foreground, no wake lock) until closed.
   * Pass [dispatch] = false when [AgentService] is already starting itself (e.g. `START_STICKY` null-intent restart).
   */
  fun acquireStandby(reason: String, dispatch: Boolean = true): AutoCloseable =
    hold(active = false, reason = reason, dispatch = dispatch)

  /**
   * Retries applying [mode] when a prior [updater] call failed ([appliedMode] != [mode]).
   * When [force] is true and [mode] is non-Off, re-applies [mode] even if already applied (e.g. to refresh
   * `START_STICKY` / `START_NOT_STICKY` policy on [AgentService] while staying Active).
   */
  fun sync(reason: String, force: Boolean = false) {
    val shouldDrain = synchronized(lock) {
      if (mode == appliedMode && (!force || mode == KeepAliveMode.Off)) return
      scheduleLocked(reason, force)
    }
    if (shouldDrain) drain()
  }

  /** Notes that [AgentService] entered [runningMode] (for example, on a `START_STICKY` null-intent restart). */
  internal fun noteRunning(runningMode: KeepAliveMode) {
    synchronized(lock) {
      if (mode == runningMode) {
        appliedMode = runningMode
        appliedGeneration += 1
      }
    }
  }

  /** Notes that [AgentService] stopped or was destroyed so future [sync] calls can restart it if still desired. */
  internal fun noteStopped() {
    synchronized(lock) {
      appliedMode = KeepAliveMode.Off
      appliedGeneration += 1
    }
  }

  private fun hold(active: Boolean, reason: String, dispatch: Boolean): AutoCloseable {
    val shouldDrain = synchronized(lock) {
      if (active) activeHolders += 1 else standbyHolders += 1
      val changed = recomputeDesiredLocked()
      if (dispatch && (changed || mode != appliedMode)) {
        scheduleLocked(reason, force = false)
      } else {
        false
      }
    }
    if (shouldDrain) drain()
    val released = AtomicBoolean(false)
    return AutoCloseable {
      if (released.compareAndSet(false, true)) {
        val drainOnRelease = synchronized(lock) {
          if (active) activeHolders -= 1 else standbyHolders -= 1
          val changed = recomputeDesiredLocked()
          if (changed || mode != appliedMode) {
            scheduleLocked(reason, force = false)
          } else {
            false
          }
        }
        if (drainOnRelease) drain()
      }
    }
  }

  private fun recomputeDesiredLocked(): Boolean {
    val desired = when {
      activeHolders > 0 -> KeepAliveMode.Active
      standbyHolders > 0 -> KeepAliveMode.Standby
      else -> KeepAliveMode.Off
    }
    if (desired == mode) return false
    mode = desired
    return true
  }

  private fun scheduleLocked(reason: String, force: Boolean): Boolean {
    pendingSync = true
    pendingForce = pendingForce || force
    pendingReason = reason
    if (draining) return false
    draining = true
    return true
  }

  private fun drain() {
    var completed = false
    try {
      while (true) {
        val step = nextStepLocked() ?: break
        val ok = updater(step.mode, step.reason)
        synchronized(lock) {
          if (ok) {
            if (appliedGeneration == step.generation) {
              appliedMode = step.mode
              if (mode != appliedMode) pendingSync = true
            }
          } else {
            Log.w(TAG, "Keep-alive (${step.reason}, mode=${step.mode}) not applied; Android may stop OOPBoT")
          }
        }
      }
      completed = true
    } finally {
      if (!completed) {
        synchronized(lock) { draining = false }
      }
    }
  }

  private fun nextStepLocked(): DrainStep? = synchronized(lock) {
    if (!pendingSync) {
      draining = false
      return null
    }
    pendingSync = false
    val target = mode
    val force = pendingForce
    val reason = pendingReason
    pendingForce = false
    if (target == appliedMode && (!force || target == KeepAliveMode.Off)) {
      draining = false
      return null
    }
    DrainStep(target, reason, appliedGeneration)
  }

  private data class DrainStep(val mode: KeepAliveMode, val reason: String, val generation: Long)

  private companion object {
    const val TAG = "OOPBoT"
  }
}
