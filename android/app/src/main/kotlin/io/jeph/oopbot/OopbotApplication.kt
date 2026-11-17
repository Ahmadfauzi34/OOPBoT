package io.jeph.oopbot

import android.app.Application
import io.jeph.oopbot.ui.theme.LauncherIcon
import io.jeph.oopbot.ui.theme.OopbotFonts
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.launch

class OopbotApplication : Application() {
  internal lateinit var graph: AppGraph
    private set

  override fun onCreate() {
    super.onCreate()
    // The :inference process only hosts a local model and builds none of the app.
    if (getProcessName() != packageName) return
    AppStrings.init(this)
    graph = AppGraph(this)
    // Started with the process rather than the activity, so the fonts are usually loaded by the first frame.
    graph.scope.launch(Dispatchers.Default) { OopbotFonts.preload(this@OopbotApplication) }
    graph.scope.launch(Dispatchers.IO) { LauncherIcon.follow(this@OopbotApplication, graph.preferences.theme) }
  }
}
