package io.jeph.oopbot.agent

import android.app.Service
import android.content.res.Resources
import io.jeph.oopbot.R
import org.robolectric.annotation.Implementation
import org.robolectric.annotation.Implements
import org.robolectric.shadows.ShadowResources
import org.robolectric.shadows.ShadowService

/**
 * Lightweight string stub so Robolectric tests run with `manifest = Config.NONE` (avoiding `mergeDebugAssets` and
 * native/host build tasks) while still resolving app notification strings used by [AgentService] and [AppGraph].
 */
@Implements(Resources::class)
class StubResourcesShadow : ShadowResources() {
  @Implementation
  fun getText(id: Int): CharSequence = when (id) {
    R.string.agent_notification_standby_title -> "OOPBoT standby"
    R.string.agent_notification_standby_text -> "Ready in background"
    R.string.agent_notification_background_title -> "Background task"
    R.string.agent_notification_background_text -> "Waiting for results"
    R.string.agent_notification_title -> "Agent working"
    R.string.agent_notification_text -> "Running"
    R.string.agent_notification_terminal_title -> "Terminal running"
    R.string.agent_notification_terminal_text -> "Running command"
    R.string.agent_notification_stop -> "Stop"
    R.string.agent_notification_channel -> "Agent"
    R.string.turn_notification_channel -> "Turns"
    R.string.dialog_notification_channel -> "Dialogs"
    else -> "res:$id"
  }
}

/**
 * Emulates real Android `Service.stopSelfResult(startId)` behavior (stopping only when `startId` matches
 * [latestStartId]) and fails if argumentless `stopSelf()` is ever invoked.
 */
@Implements(Service::class)
class StartIdShadowService : ShadowService() {
  var latestStartId: Int? = null
  var lastStopSelfResultId: Int? = null

  @Implementation
  override fun stopSelf(): Unit =
    throw AssertionError("AgentService must call stopSelfResult(startId), never argumentless stopSelf()")

  @Implementation
  override fun stopSelfResult(id: Int): Boolean {
    lastStopSelfResultId = id
    val latest = latestStartId
    if (latest != null && id != latest) return false
    return super.stopSelfResult(id)
  }
}
