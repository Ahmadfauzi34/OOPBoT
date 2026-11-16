package io.jeph.oopbot.data

import androidx.annotation.StringRes
import io.jeph.oopbot.R

/**
 * Built-in OOPBoT plugins controlled by Android. One with a [switch] is listed in Settings, which can turn it off;
 * one without is part of how OOPBoT works, so every session requests it.
 */
internal enum class BundledExtension(val id: String, val switch: Switch?) {
  Todo(
    "oopbot-todo",
    Switch(R.string.settings_extension_todo, R.string.settings_extension_todo_summary),
  ),

  /** Runs shell commands past the agent's turn; it tells the app over the host channel while they run. */
  Background(
    "oopbot-background",
    Switch(R.string.settings_extension_background, R.string.settings_extension_background_summary),
  ),

  /** Web search and content extraction tools. */
  WebAccess(
    "oopbot-web-access",
    Switch(R.string.settings_extension_web_access, R.string.settings_extension_web_access_summary),
  ),

  /** Hands the agent's tasks to subagents in the same engine; it tells the app about their runs over the channel. */
  Subagents(
    "oopbot-subagents",
    Switch(R.string.settings_extension_subagents, R.string.settings_extension_subagents_summary),
  ),

  /**
   * Remote workspace mode indicator; sent only when needed by remote workspaces, so there's no switch.
   */
  Remote(
    "oopbot-remote",
    switch = null,
  ),
  ;

  /** How Settings lists an extension the user can turn off. */
  class Switch(@StringRes val title: Int, @StringRes val summary: Int)
}
