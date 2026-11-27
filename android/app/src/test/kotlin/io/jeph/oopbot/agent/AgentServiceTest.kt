package io.jeph.oopbot.agent

import android.app.NotificationManager
import android.app.Service
import android.content.Intent
import androidx.test.core.app.ApplicationProvider
import io.jeph.oopbot.OopbotApplication
import io.jeph.oopbot.R
import java.util.concurrent.TimeUnit
import java.util.concurrent.atomic.AtomicInteger
import org.junit.After
import org.junit.Assert.assertEquals
import org.junit.Assert.assertFalse
import org.junit.Assert.assertNotNull
import org.junit.Assert.assertTrue
import org.junit.Before
import org.junit.Test
import org.junit.runner.RunWith
import org.robolectric.Robolectric
import org.robolectric.RobolectricTestRunner
import org.robolectric.Shadows
import org.robolectric.annotation.Config
import org.robolectric.shadow.api.Shadow
import org.robolectric.shadows.ShadowLooper
import org.robolectric.shadows.ShadowPowerManager

@RunWith(RobolectricTestRunner::class)
@Config(
  manifest = Config.NONE,
  application = OopbotApplication::class,
  sdk = [35],
  shadows = [StubResourcesShadow::class, StartIdShadowService::class],
)
class AgentServiceTest {
  private lateinit var app: OopbotApplication

  @Before
  fun setUp() {
    AgentService.startForegroundForTest = null
    app = ApplicationProvider.getApplicationContext()
    app.graph.preferences.setSetupComplete(false)
    app.graph.preferences.setBackgroundStandby(false)
    ShadowLooper.idleMainLooper()
    Shadows.shadowOf(app).clearStartedServices()
  }

  @After
  fun tearDown() {
    AgentService.startForegroundForTest = null
    app.graph.preferences.setBackgroundStandby(false)
    app.graph.preferences.setSetupComplete(false)
    ShadowLooper.idleMainLooper()
  }

  @Test
  fun nullIntentRestartHonorsSetupCompleteAndBackgroundStandbyMatrix() {
    val cases = listOf(
      Triple(false, false, false),
      Triple(true, false, false),
      Triple(false, true, false),
      Triple(true, true, true),
    )
    for ((setupComplete, standbyEnabled, expectForeground) in cases) {
      app.graph.preferences.setSetupComplete(setupComplete)
      app.graph.preferences.setBackgroundStandby(standbyEnabled)
      ShadowLooper.idleMainLooper()
      val service = Robolectric.buildService(AgentService::class.java).create().get()
      val shadowService = Shadows.shadowOf(service)
      val wakeLock = ShadowPowerManager.getLatestWakeLock()

      val result = service.onStartCommand(null, 0, 1)

      if (expectForeground) {
        assertEquals(Service.START_STICKY, result)
        assertTrue(AgentService.foreground)
        assertFalse(shadowService.isStoppedBySelf)
        assertFalse(wakeLock.isHeld)
        assertEquals(KeepAliveMode.Standby, app.graph.keepAlive.mode)
        assertEquals(KeepAliveMode.Standby, app.graph.keepAlive.appliedMode)
        val notification = shadowService.lastForegroundNotification
        assertNotNull(notification)
        assertEquals(
          app.getString(R.string.agent_notification_standby_title),
          notification.extras.getString("android.title"),
        )
      } else {
        assertEquals(Service.START_NOT_STICKY, result)
        assertFalse(AgentService.foreground)
        assertTrue(shadowService.isStoppedBySelf)
      }
      service.onDestroy()
    }
  }

  @Test
  fun nullIntentRestartCatchesForegroundStartNotAllowedAndStopsCleanly() {
    app.graph.preferences.setSetupComplete(true)
    app.graph.preferences.setBackgroundStandby(true)
    // Simulate a cold restart before holdWhileWorking has applied the service start.
    app.graph.keepAlive.noteStopped()
    AgentService.startForegroundForTest = { _, _, _ ->
      throw IllegalStateException("ForegroundServiceStartNotAllowedException")
    }

    val service = Robolectric.buildService(AgentService::class.java).create().get()
    val shadowService = Shadows.shadowOf(service)
    val wakeLock = ShadowPowerManager.getLatestWakeLock()

    val result = service.onStartCommand(null, 0, 7)

    assertEquals(Service.START_NOT_STICKY, result)
    assertFalse(AgentService.foreground)
    assertFalse(wakeLock.isHeld)
    assertTrue(shadowService.isStoppedBySelf)
    assertEquals(KeepAliveMode.Standby, app.graph.keepAlive.mode)
    assertEquals(KeepAliveMode.Off, app.graph.keepAlive.appliedMode)
    service.onDestroy()
  }

  @Test
  fun actionBusyToActionStandbyReleasesWakeLockCancelsRenewAndHonorsStickyPolicy() {
    app.graph.preferences.setSetupComplete(true)
    app.graph.preferences.setBackgroundStandby(false)
    ShadowLooper.idleMainLooper()

    val service = Robolectric.buildService(AgentService::class.java).create().get()
    val wakeLock = ShadowPowerManager.getLatestWakeLock()

    val activeHold = app.graph.keepAlive.acquire("chats working")
    val busyIntent = Intent(app, AgentService::class.java).setAction(AgentService.ACTION_BUSY)
    val busyResult = service.onStartCommand(busyIntent, 0, 1)
    assertEquals(Service.START_NOT_STICKY, busyResult)
    assertTrue(AgentService.foreground)
    assertTrue(wakeLock.isHeld)

    // Transition to Standby while backgroundStandby == false (e.g. waiting on background tasks).
    val standbyHold = app.graph.keepAlive.acquireStandby("waiting on background tasks")
    activeHold.close()
    val standbyIntent = Intent(app, AgentService::class.java).setAction(AgentService.ACTION_STANDBY)
    val standbyResult = service.onStartCommand(standbyIntent, 0, 2)

    assertEquals(Service.START_NOT_STICKY, standbyResult)
    assertTrue(AgentService.foreground)
    assertFalse(wakeLock.isHeld)

    // Advancing past the 5-minute renewal period must NOT re-acquire the wake lock.
    ShadowLooper.idleMainLooper(6, TimeUnit.MINUTES)
    assertFalse(wakeLock.isHeld)

    // Enabling Background Standby flips ACTION_STANDBY to START_STICKY.
    app.graph.preferences.setBackgroundStandby(true)
    val stickyResult = service.onStartCommand(standbyIntent, 0, 3)
    assertEquals(Service.START_STICKY, stickyResult)

    standbyHold.close()
    service.onDestroy()
  }

  @Test
  fun actionBusyReconcilesWithOffAfterSatisfyingForegroundContract() {
    val startForegroundCalls = AtomicInteger(0)
    AgentService.startForegroundForTest = { _, _, _ -> startForegroundCalls.incrementAndGet() }

    val service = Robolectric.buildService(AgentService::class.java).create().get()
    val shadowService = Shadows.shadowOf(service)
    val wakeLock = ShadowPowerManager.getLatestWakeLock()

    // KeepAlive is already Off when the queued ACTION_BUSY intent is delivered.
    assertEquals(KeepAliveMode.Off, app.graph.keepAlive.mode)
    val busyIntent = Intent(app, AgentService::class.java).setAction(AgentService.ACTION_BUSY)
    val result = service.onStartCommand(busyIntent, 0, 4)

    assertEquals(1, startForegroundCalls.get())
    assertEquals(Service.START_NOT_STICKY, result)
    assertFalse(AgentService.foreground)
    assertFalse(wakeLock.isHeld)
    assertTrue(shadowService.isStoppedBySelf)
    service.onDestroy()
  }

  @Test
  fun actionIdleUsesStopSelfResultAndDoesNotKillNewerStartId() {
    val busyIntent = Intent(app, AgentService::class.java).setAction(AgentService.ACTION_BUSY)
    val idleIntent = Intent(app, AgentService::class.java).setAction(AgentService.ACTION_IDLE)
    val controller = Robolectric.buildService(AgentService::class.java, busyIntent).create()
    val service = controller.get()
    val shadowService = Shadow.extract<StartIdShadowService>(service)
    val activeHold = app.graph.keepAlive.acquire("chats working")

    shadowService.latestStartId = 1
    controller.startCommand(0, 1)
    assertTrue(AgentService.foreground)
    assertFalse(shadowService.isStoppedBySelf)

    // Record a newer startId (2) before delivering stale ACTION_IDLE with startId 1.
    shadowService.latestStartId = 2
    controller.startCommand(0, 2)
    val staleIdleResult = service.onStartCommand(idleIntent, 0, 1)
    assertEquals(Service.START_NOT_STICKY, staleIdleResult)
    assertEquals(1, shadowService.lastStopSelfResultId)
    assertFalse(shadowService.isStoppedBySelf)
    assertEquals(KeepAliveMode.Active, app.graph.keepAlive.appliedMode)

    // Delivering ACTION_IDLE with the latest startId stops the service cleanly.
    activeHold.close()
    val latestIdleResult = service.onStartCommand(idleIntent, 0, 2)
    assertEquals(Service.START_NOT_STICKY, latestIdleResult)
    assertEquals(2, shadowService.lastStopSelfResultId)
    assertTrue(shadowService.isStoppedBySelf)
    assertEquals(KeepAliveMode.Off, app.graph.keepAlive.appliedMode)
    service.onDestroy()
  }

  @Test
  fun refreshIsNoOpWhenKeepAliveModeIsOff() {
    val service = Robolectric.buildService(AgentService::class.java).create().get()
    val notificationManager = app.getSystemService(NotificationManager::class.java)
    val nm = Shadows.shadowOf(notificationManager)
    val activeHold = app.graph.keepAlive.acquire("chats working")

    service.onStartCommand(Intent(app, AgentService::class.java).setAction(AgentService.ACTION_BUSY), 0, 1)
    assertTrue(AgentService.foreground)
    notificationManager.cancelAll()

    // Close hold so KeepAlive.mode becomes Off while AgentService.foreground is still true before ACTION_IDLE runs.
    activeHold.close()
    assertEquals(KeepAliveMode.Off, app.graph.keepAlive.mode)
    assertTrue(AgentService.foreground)

    AgentService.refresh(app)
    assertTrue("refresh() must not post a notification while transitioning Off", nm.allNotifications.isEmpty())

    service.onDestroy()
  }

  @Test
  fun actionStopWithNoActiveChatsStopsServiceViaStopSelfResult() {
    val service = Robolectric.buildService(AgentService::class.java).create().get()
    val shadowService = Shadows.shadowOf(service)
    assertEquals(KeepAliveMode.Off, app.graph.keepAlive.mode)

    val result = service.onStartCommand(Intent(app, AgentService::class.java).setAction(AgentService.ACTION_STOP), 0, 9)

    assertEquals(Service.START_NOT_STICKY, result)
    assertTrue(shadowService.isStoppedBySelf)
    assertFalse(AgentService.foreground)
    service.onDestroy()
  }
}
