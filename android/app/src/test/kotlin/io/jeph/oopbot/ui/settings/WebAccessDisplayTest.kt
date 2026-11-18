package io.jeph.oopbot.ui.settings

import io.jeph.oopbot.R
import io.jeph.oopbot.data.BundledExtension
import io.jeph.oopbot.host.AuthStatus
import io.jeph.oopbot.host.BuiltinProvider
import io.jeph.oopbot.host.CustomProvider
import org.junit.Assert.assertEquals
import org.junit.Assert.assertFalse
import org.junit.Assert.assertNull
import org.junit.Assert.assertTrue
import org.junit.Test

/** Dynamic Settings display label switching for WebAccess -> Google Search when Gemini is connected. */
class WebAccessDisplayTest {
  private fun status(
    builtin: List<BuiltinProvider> = emptyList(),
    custom: List<CustomProvider> = emptyList(),
  ) = AuthStatus(
    builtin = builtin,
    custom = custom,
    modelsError = null,
  )

  @Test
  fun `null status or unconfigured providers stay on standard web access copy`() {
    assertFalse(null.hasGeminiProvider())
    assertEquals(
      R.string.settings_extension_web_access to R.string.settings_extension_web_access_summary,
      extensionDisplayCopy(BundledExtension.WebAccess, null),
    )

    val disconnected = status(
      builtin = listOf(
        BuiltinProvider(id = "google", name = "Google Gemini", configured = false, type = null),
        BuiltinProvider(id = "anthropic", name = "Claude", configured = true, type = "oauth"),
      ),
      custom = listOf(
        CustomProvider(
          id = "openai-local",
          baseUrl = "http://localhost:8080/v1",
          api = "openai-completions",
          apiKey = "sk-test",
          models = emptyList(),
        ),
        CustomProvider(
          id = "gemini-empty-key",
          baseUrl = "https://generativelanguage.googleapis.com/v1beta",
          api = "google-generative-ai",
          apiKey = "no-api-key",
          models = emptyList(),
        ),
      ),
    )
    assertFalse(disconnected.hasGeminiProvider())
    assertEquals(
      R.string.settings_extension_web_access to R.string.settings_extension_web_access_summary,
      extensionDisplayCopy(BundledExtension.WebAccess, disconnected),
    )
  }

  @Test
  fun `configured builtin google provider switches web access copy to Google Search`() {
    val connected = status(
      builtin = listOf(
        BuiltinProvider(id = "google", name = "Google Gemini", configured = true, type = "api_key"),
      ),
    )
    assertTrue(connected.hasGeminiProvider())
    assertEquals(
      R.string.settings_extension_google_search to R.string.settings_extension_google_search_summary,
      extensionDisplayCopy(BundledExtension.WebAccess, connected),
    )
    assertEquals(
      R.string.settings_extension_todo to R.string.settings_extension_todo_summary,
      extensionDisplayCopy(BundledExtension.Todo, connected),
    )
    assertNull(extensionDisplayCopy(BundledExtension.Remote, connected))
  }

  @Test
  fun `custom google generative ai provider with key also activates Google Search copy`() {
    val connectedCustom = status(
      custom = listOf(
        CustomProvider(
          id = "my-gemini-proxy",
          baseUrl = "https://proxy.example/v1beta",
          api = "google-generative-ai",
          apiKey = "AIzaSyCustomTestKey",
          models = emptyList(),
        ),
      ),
    )
    assertTrue(connectedCustom.hasGeminiProvider())
    assertEquals(
      R.string.settings_extension_google_search to R.string.settings_extension_google_search_summary,
      extensionDisplayCopy(BundledExtension.WebAccess, connectedCustom),
    )
  }
}
