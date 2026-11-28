package io.jeph.oopbot.schedule

import android.app.AlarmManager
import android.content.ComponentName
import android.content.Intent
import android.content.pm.PackageManager
import androidx.test.core.app.ApplicationProvider
import io.jeph.oopbot.OopbotApplication
import io.jeph.oopbot.agent.AgentService
import io.jeph.oopbot.agent.KeepAliveMode
import io.jeph.oopbot.agent.StubResourcesShadow
import kotlinx.coroutines.runBlocking
import org.junit.After
import org.junit.Assert.assertEquals
import org.junit.Assert.assertNotNull
import org.junit.Assert.assertNull
import org.junit.Assert.assertTrue
import org.junit.Before
import org.junit.Test
import org.junit.runner.RunWith
import org.robolectric.RobolectricTestRunner
import org.robolectric.Shadows
import org.robolectric.annotation.Config
import org.robolectric.shadows.ShadowLooper

@RunWith(RobolectricTestRunner::class)
@Config(
  manifest = Config.NONE,
  application = OopbotApplication::class,
  sdk = [35],
  shadows = [StubResourcesShadow::class],
)
class ScheduleAlarmReceiverTest {
  private lateinit var app: OopbotApplication

  @Before
  fun setUp() {
    app = ApplicationProvider.getApplicationContext()
    app.graph.preferences.setSetupComplete(false)
    app.graph.preferences.setBackgroundStandby(false)
    runBlocking {
      app.graph.scheduleRepository.ensureInitialized()
      for (job in app.graph.scheduleRepository.jobs.value) {
        app.graph.scheduleRepository.deleteJob(job.id)
      }
    }
    ShadowLooper.idleMainLooper()
    Shadows.shadowOf(app).clearStartedServices()
  }

  @After
  fun tearDown() {
    runBlocking {
      for (job in app.graph.scheduleRepository.jobs.value) {
        app.graph.scheduleRepository.deleteJob(job.id)
      }
    }
    app.graph.preferences.setSetupComplete(false)
    app.graph.preferences.setBackgroundStandby(false)
    ShadowLooper.idleMainLooper()
  }

  @Test
  fun syncEnabledTogglesReceiverComponentState() {
    val component = ComponentName(app, ScheduleAlarmReceiver::class.java)
    val pm = app.packageManager

    ScheduleAlarmReceiver.syncEnabled(app, true)
    assertEquals(PackageManager.COMPONENT_ENABLED_STATE_ENABLED, pm.getComponentEnabledSetting(component))

    ScheduleAlarmReceiver.syncEnabled(app, false)
    assertEquals(PackageManager.COMPONENT_ENABLED_STATE_DEFAULT, pm.getComponentEnabledSetting(component))
  }

  @Test
  fun ignoresNullUnknownAndUnconfiguredBroadcasts() {
    val receiver = ScheduleAlarmReceiver()
    val shadowApp = Shadows.shadowOf(app)

    receiver.onReceive(app, null)
    receiver.onReceive(app, Intent("com.example.UNKNOWN"))
    assertNull(shadowApp.nextStartedService)

    // setupComplete == false -> ACTION_RUN_DUE must not acquire KeepAlive or start AgentService.
    receiver.onReceive(app, Intent(ScheduleAlarmReceiver.ACTION_RUN_DUE))
    assertNull(shadowApp.nextStartedService)
    assertEquals(KeepAliveMode.Off, app.graph.keepAlive.mode)
  }

  @Test
  fun runDueSynchronouslyAcquiresActiveHoldAndReleasesWhenNoJobsDue() {
    app.graph.preferences.setSetupComplete(true)
    val shadowApp = Shadows.shadowOf(app)
    shadowApp.clearStartedServices()

    val receiver = ScheduleAlarmReceiver()
    receiver.onReceive(app, Intent(ScheduleAlarmReceiver.ACTION_RUN_DUE))

    // Synchronously requested AgentService.ACTION_BUSY inside AlarmManager's broadcast exemption window.
    val started = shadowApp.nextStartedService
    assertEquals(AgentService.ACTION_BUSY, started?.action)

    // Wait briefly for IO dispatch to see 0 due jobs and release the initial Active hold back to Off.
    awaitUntil { app.graph.keepAlive.mode == KeepAliveMode.Off }
    assertEquals(KeepAliveMode.Off, app.graph.keepAlive.mode)
  }

  @Test
  fun bootAndClockBroadcastsRearmWakeAlarm() {
    val futureTrigger = System.currentTimeMillis() + 60 * 60_000L
    runBlocking {
      app.graph.scheduleRepository.createJob(
        NewScheduledJob(
          name = "Hourly",
          cwd = "/w/app",
          prompt = "Check status",
          spec = ScheduleSpec.Once(futureTrigger),
          timezone = "UTC",
        ),
      )
    }
    val alarmManager = app.getSystemService(AlarmManager::class.java)
    val shadowAlarm = Shadows.shadowOf(alarmManager)
    shadowAlarm.scheduledAlarms.clear()

    val receiver = ScheduleAlarmReceiver()
    receiver.onReceive(app, Intent(Intent.ACTION_BOOT_COMPLETED))
    awaitUntil { shadowAlarm.peekNextScheduledAlarm() != null }
    assertEquals(futureTrigger, shadowAlarm.peekNextScheduledAlarm()?.triggerAtMs)

    shadowAlarm.scheduledAlarms.clear()
    receiver.onReceive(app, Intent(Intent.ACTION_TIME_CHANGED))
    awaitUntil { shadowAlarm.peekNextScheduledAlarm() != null }
    assertNotNull(shadowAlarm.peekNextScheduledAlarm())
  }

  private fun awaitUntil(timeoutMillis: Long = 3_000L, predicate: () -> Boolean) {
    val deadline = System.currentTimeMillis() + timeoutMillis
    while (!predicate() && System.currentTimeMillis() < deadline) {
      ShadowLooper.idleMainLooper()
      Thread.sleep(15L)
    }
    assertTrue("Timed out waiting for condition", predicate())
  }
}
