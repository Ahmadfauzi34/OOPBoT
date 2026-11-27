package io.jeph.oopbot.agent

import android.content.ComponentName
import android.content.Intent
import android.content.pm.PackageManager
import androidx.test.core.app.ApplicationProvider
import io.jeph.oopbot.OopbotApplication
import org.junit.After
import org.junit.Assert.assertEquals
import org.junit.Assert.assertNull
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
class StandbyBootReceiverTest {
  private lateinit var app: OopbotApplication

  @Before
  fun setUp() {
    app = ApplicationProvider.getApplicationContext()
    app.graph.preferences.setSetupComplete(false)
    app.graph.preferences.setBackgroundStandby(false)
    ShadowLooper.idleMainLooper()
    Shadows.shadowOf(app).clearStartedServices()
  }

  @After
  fun tearDown() {
    app.graph.preferences.setBackgroundStandby(false)
    app.graph.preferences.setSetupComplete(false)
    ShadowLooper.idleMainLooper()
  }

  @Test
  fun syncEnabledTogglesReceiverComponentState() {
    val component = ComponentName(app, StandbyBootReceiver::class.java)
    val pm = app.packageManager

    StandbyBootReceiver.syncEnabled(app, true)
    assertEquals(PackageManager.COMPONENT_ENABLED_STATE_ENABLED, pm.getComponentEnabledSetting(component))

    StandbyBootReceiver.syncEnabled(app, false)
    assertEquals(PackageManager.COMPONENT_ENABLED_STATE_DEFAULT, pm.getComponentEnabledSetting(component))
  }

  @Test
  fun ignoresNullUnknownAndDisabledBroadcasts() {
    val receiver = StandbyBootReceiver()
    val shadowApp = Shadows.shadowOf(app)

    receiver.onReceive(app, null)
    receiver.onReceive(app, Intent("com.example.UNKNOWN"))
    assertNull(shadowApp.nextStartedService)

    // setupComplete == false even when backgroundStandby == true -> no-op.
    app.graph.preferences.setBackgroundStandby(true)
    receiver.onReceive(app, Intent(Intent.ACTION_BOOT_COMPLETED))
    assertNull(shadowApp.nextStartedService)
    assertEquals(KeepAliveMode.Off, app.graph.keepAlive.mode)

    // setupComplete == true while backgroundStandby == false -> no-op.
    app.graph.preferences.setBackgroundStandby(false)
    app.graph.preferences.setSetupComplete(true)
    receiver.onReceive(app, Intent(Intent.ACTION_BOOT_COMPLETED))
    assertNull(shadowApp.nextStartedService)
    assertEquals(KeepAliveMode.Off, app.graph.keepAlive.mode)
  }

  @Test
  fun bootCompletedAndPackageReplacedRestoreStandbyThroughKeepAlive() {
    app.graph.preferences.setSetupComplete(true)
    app.graph.preferences.setBackgroundStandby(true)
    val shadowApp = Shadows.shadowOf(app)
    shadowApp.clearStartedServices()

    val receiver = StandbyBootReceiver()
    receiver.onReceive(app, Intent(Intent.ACTION_BOOT_COMPLETED))

    assertEquals(KeepAliveMode.Standby, app.graph.keepAlive.mode)
    assertEquals(KeepAliveMode.Standby, app.graph.keepAlive.appliedMode)
    val bootIntent = shadowApp.nextStartedService
    assertEquals(AgentService.ACTION_STANDBY, bootIntent?.action)

    // Simulate earlier start failure (mode == Standby, appliedMode == Off) and verify MY_PACKAGE_REPLACED recovers.
    app.graph.keepAlive.noteStopped()
    shadowApp.clearStartedServices()

    receiver.onReceive(app, Intent(Intent.ACTION_MY_PACKAGE_REPLACED))
    assertEquals(KeepAliveMode.Standby, app.graph.keepAlive.appliedMode)
    val replacedIntent = shadowApp.nextStartedService
    assertEquals(AgentService.ACTION_STANDBY, replacedIntent?.action)
  }
}
