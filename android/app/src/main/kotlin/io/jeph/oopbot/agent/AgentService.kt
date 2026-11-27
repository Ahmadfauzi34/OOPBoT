package io.jeph.oopbot.agent

import android.app.Notification
import android.app.NotificationChannel
import android.app.NotificationManager
import android.app.PendingIntent
import android.app.Service
import android.content.Context
import android.content.Intent
import android.content.pm.ServiceInfo
import android.os.Build
import android.os.Handler
import android.os.IBinder
import android.os.Looper
import android.os.PowerManager
import android.util.Log
import io.jeph.oopbot.MainActivity
import io.jeph.oopbot.R
import io.jeph.oopbot.appGraph

/**
 * Keeps OOPBoT alive in two foreground power tiers:
 * - **Active (`ACTION_BUSY`)**: foreground service + short renewed partial wake lock while a chat turn, terminal
 *   command, sign-in, install, or local-model turn is actively executing.
 * - **Standby (`ACTION_STANDBY`)**: foreground service WITHOUT a wake lock so OOPBoT stays ready in the background
 *   for background-task completions, schedules, or external wakeups while allowing the CPU to sleep normally.
 */
class AgentService : Service() {
  private val handler = Handler(Looper.getMainLooper())
  private lateinit var wakeLock: PowerManager.WakeLock
  private val renew = object : Runnable {
    override fun run() {
      wakeLock.acquire(WAKE_LEASE_MILLIS)
      handler.postDelayed(this, WAKE_RENEW_MILLIS)
    }
  }

  override fun onCreate() {
    super.onCreate()
    wakeLock = getSystemService(PowerManager::class.java).newWakeLock(PowerManager.PARTIAL_WAKE_LOCK, WAKE_TAG).apply {
      setReferenceCounted(false)
    }
    getSystemService(NotificationManager::class.java).createNotificationChannel(
      NotificationChannel(CHANNEL, getString(R.string.agent_notification_channel), NotificationManager.IMPORTANCE_LOW),
    )
  }

  override fun onStartCommand(intent: Intent?, flags: Int, startId: Int): Int {
    val action = intent?.action ?: if (appGraph.preferences.backgroundStandby.value) ACTION_STANDBY else ACTION_IDLE
    return when (action) {
      ACTION_BUSY -> {
        enterForeground()
        handler.removeCallbacks(renew)
        renew.run()
        stickyMode()
      }

      ACTION_STANDBY -> {
        enterForeground()
        releaseWakeLock()
        START_STICKY
      }

      // Chats settle after the abort; KeepAlive then drops to Standby (if enabled) or Off.
      ACTION_STOP -> {
        appGraph.registry.abortAll()
        stickyMode()
      }

      else -> {
        release()
        START_NOT_STICKY
      }
    }
  }

  private fun enterForeground() {
    if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.UPSIDE_DOWN_CAKE) {
      startForeground(NOTIFICATION_ID, notification(this), ServiceInfo.FOREGROUND_SERVICE_TYPE_SPECIAL_USE)
    } else {
      startForeground(NOTIFICATION_ID, notification(this))
    }
    foreground = true
  }

  private fun stickyMode(): Int = if (appGraph.preferences.backgroundStandby.value) START_STICKY else START_NOT_STICKY

  private fun releaseWakeLock() {
    handler.removeCallbacks(renew)
    if (wakeLock.isHeld) wakeLock.release()
  }

  private fun release() {
    foreground = false
    releaseWakeLock()
    stopForeground(STOP_FOREGROUND_REMOVE)
    stopSelf()
  }

  override fun onDestroy() {
    foreground = false
    releaseWakeLock()
    super.onDestroy()
  }

  override fun onBind(intent: Intent?): IBinder? = null

  companion object {
    const val ACTION_BUSY = "io.jeph.oopbot.action.BUSY"
    const val ACTION_STANDBY = "io.jeph.oopbot.action.STANDBY"
    const val ACTION_IDLE = "io.jeph.oopbot.action.IDLE"
    const val ACTION_STOP = "io.jeph.oopbot.action.STOP"
    private const val TAG = "OOPBoT"
    private const val CHANNEL = "agent"
    private const val NOTIFICATION_ID = 1
    private const val WAKE_TAG = "OOPBoT:Agent"
    private const val WAKE_LEASE_MILLIS = 10 * 60_000L
    private const val WAKE_RENEW_MILLIS = 5 * 60_000L

    /** Whether the service runs in the foreground, showing its notification. Main-thread confined. */
    @Volatile
    private var foreground = false

    private fun intent(context: Context, action: String) = Intent(context, AgentService::class.java).setAction(action)

    /**
     * Says what keeps OOPBoT running: active chats (with Stop), background tasks awaiting results (with Stop, low
     * power), active terminal commands, low-power Standby mode, or another Active holder (sign-in/install).
     */
    private fun notification(context: Context): Notification {
      val graph = context.appGraph
      val chatsWorking = graph.registry.busy.value
      val backgroundOnly = !chatsWorking && graph.registry.holding.value
      val terminalsOnly = !chatsWorking && !backgroundOnly && graph.terminals.busy.value
      val standbyOnly = !chatsWorking && !backgroundOnly && !terminalsOnly &&
        graph.keepAlive.mode == KeepAliveMode.Standby
      val open =
        PendingIntent.getActivity(context, 20, Intent(context, MainActivity::class.java), PendingIntent.FLAG_IMMUTABLE)
      val builder = Notification.Builder(context, CHANNEL)
        .setSmallIcon(R.drawable.ic_launcher_foreground)
        .setContentIntent(open)
        .setOngoing(true)
      return when {
        terminalsOnly ->
          builder
            .setContentTitle(context.getString(R.string.agent_notification_terminal_title))
            .setContentText(context.getString(R.string.agent_notification_terminal_text))
            .build()

        standbyOnly ->
          builder
            .setContentTitle(context.getString(R.string.agent_notification_standby_title))
            .setContentText(context.getString(R.string.agent_notification_standby_text))
            .build()

        else -> {
          val stop = PendingIntent.getService(context, 21, intent(context, ACTION_STOP), PendingIntent.FLAG_IMMUTABLE)
          val title =
            if (backgroundOnly) R.string.agent_notification_background_title else R.string.agent_notification_title
          val text =
            if (backgroundOnly) R.string.agent_notification_background_text else R.string.agent_notification_text
          builder
            .setContentTitle(context.getString(title))
            .setContentText(context.getString(text))
            .addAction(
              Notification.Action.Builder(null, context.getString(R.string.agent_notification_stop), stop).build(),
            )
            .build()
        }
      }
    }

    /** Re-posts the notification after what keeps OOPBoT running changed; nothing while the service is stopped. */
    fun refresh(context: Context) {
      if (!foreground) return
      context.getSystemService(NotificationManager::class.java).notify(NOTIFICATION_ID, notification(context))
    }

    /**
     * Applies [mode] to [AgentService]:
     * - [KeepAliveMode.Active] → foreground with renewed partial wake lock
     * - [KeepAliveMode.Standby] → foreground with wake lock released
     * - [KeepAliveMode.Off] → stop foreground and stop service
     *
     * Safe from any thread; returns false when Android refuses a background foreground-service start.
     */
    internal fun update(context: Context, mode: KeepAliveMode): Boolean = try {
      when (mode) {
        KeepAliveMode.Active -> {
          val target = intent(context, ACTION_BUSY)
          if (foreground) context.startService(target) else context.startForegroundService(target)
        }

        KeepAliveMode.Standby -> {
          val target = intent(context, ACTION_STANDBY)
          if (foreground) context.startService(target) else context.startForegroundService(target)
        }

        KeepAliveMode.Off -> context.startService(intent(context, ACTION_IDLE))
      }
      true
    } catch (error: IllegalStateException) {
      Log.w(TAG, "AgentService.update($mode) refused", error)
      false
    }
  }
}
