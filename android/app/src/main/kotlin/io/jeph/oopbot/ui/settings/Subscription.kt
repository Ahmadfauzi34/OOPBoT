package io.jeph.oopbot.ui.settings

import androidx.annotation.StringRes
import io.jeph.oopbot.R
import io.jeph.oopbot.data.BundledExtension
import io.jeph.oopbot.host.AuthStatus

/**
 * A provider Pi ships that OOPBoT connects: a subscription signed in to with OAuth, or, when
 * [usesApiKey], one Pi stores an API key for. [api] is the API its models use.
 */
internal data class Subscription(
  val id: String,
  @StringRes val title: Int,
  @StringRes val subtitle: Int,
  @StringRes val loginTitle: Int,
  val api: String,
  val usesApiKey: Boolean = false,
)

internal val subscriptions = listOf(
  Subscription(
    "anthropic",
    R.string.settings_claude,
    R.string.settings_claude_subtitle,
    R.string.login_title_claude,
    "anthropic-messages",
  ),
  Subscription(
    "openai-codex",
    R.string.settings_openai,
    R.string.settings_openai_subtitle,
    R.string.login_title_openai,
    "openai-codex-responses",
  ),
  Subscription(
    "google",
    R.string.settings_gemini,
    R.string.settings_gemini_subtitle,
    R.string.login_title_gemini,
    "google-generative-ai",
    usesApiKey = true,
  ),
  Subscription(
    "zai",
    R.string.settings_zai,
    R.string.settings_zai_subtitle,
    R.string.login_title_zai,
    "openai-completions",
    usesApiKey = true,
  ),
  Subscription(
    "kimi-coding",
    R.string.settings_kimi,
    R.string.settings_kimi_subtitle,
    R.string.login_title_kimi,
    "openai-completions",
    usesApiKey = true,
  ),
  Subscription(
    "openrouter",
    R.string.settings_openrouter,
    R.string.settings_openrouter_subtitle,
    R.string.login_title_openrouter,
    "openai-completions",
    usesApiKey = true,
  ),
)

/** True once Pi holds a credential for the provider [id]. */
internal fun AuthStatus.isSignedIn(id: String): Boolean = builtin.any { it.id == id && it.configured }

/** True for a provider OOPBoT connects with an API key, whose models the user picks after adding it. */
internal fun usesApiKey(providerId: String): Boolean = subscriptions.any { it.id == providerId && it.usesApiKey }

/**
 * True when the user has a Google Gemini credential configured (via the built-in `google`
 * provider or a custom `google-generative-ai` / Gemini provider with a key), matching the Rust
 * `WebAccessPlugin` auto-detection for Google Search Grounding.
 */
internal fun AuthStatus?.hasGeminiProvider(): Boolean {
  if (this == null) return false
  if (isSignedIn("google") || isSignedIn("gemini")) return true
  return custom.any { provider ->
    val key = provider.apiKey.trim()
    val isGemini = provider.api == "google-generative-ai" ||
      provider.id.equals("google", ignoreCase = true) ||
      provider.id.equals("gemini", ignoreCase = true)
    isGemini && key.isNotEmpty() && key != "no-api-key"
  }
}

/**
 * Resolves the Settings row title and summary string resources for a bundled [extension].
 * When [BundledExtension.WebAccess] is shown while a Gemini provider is connected, it switches
 * to Google Search copy to reflect automatic Gemini Grounding.
 */
internal fun extensionDisplayCopy(extension: BundledExtension, authStatus: AuthStatus?): Pair<Int, Int>? {
  val switch = extension.switch ?: return null
  if (extension == BundledExtension.WebAccess && authStatus.hasGeminiProvider()) {
    return R.string.settings_extension_google_search to R.string.settings_extension_google_search_summary
  }
  return switch.title to switch.summary
}
