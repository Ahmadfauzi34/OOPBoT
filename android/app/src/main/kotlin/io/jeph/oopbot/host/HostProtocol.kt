package io.jeph.oopbot.host

/** Wire constants shared with `host/src` (see `host.mjs` modes). */
internal object HostProtocol {
  const val VERSION = 1

  /** Images arrive base64-encoded inside `prompt`, so lines can be large. */
  const val MAX_LINE_BYTES = 16 * 1024 * 1024

  /** The hello comes from an unauthenticated peer; bound it far below [MAX_LINE_BYTES]. */
  const val MAX_HELLO_BYTES = 64 * 1024

  const val HELLO = "oopbot_hello"
  const val REQUEST = "oopbot_request"
  const val RESULT = "oopbot_result"
  const val PROGRESS = "oopbot_progress"
  const val EXIT = "oopbot_exit"

  /**
   * A line on OOPBoT's channel between the app and the extensions it bundles, `{"type":…,"topic":…,"data":…}`. The
   * host routes the app's lines by their first bytes, so [type] must come first.
   */
  const val CHANNEL = "oopbot_channel"
  const val AUTH_EVENT = "oopbot_auth_event"
  const val AUTH_PROMPT = "oopbot_auth_prompt"
  const val AUTH_RESPONSE = "oopbot_auth_response"
  const val AUTH_CANCEL = "oopbot_auth_cancel"
  const val AUTH_PROMPT_DISMISS = "oopbot_auth_prompt_dismiss"
  const val REMOTE_PROMPT = "oopbot_remote_prompt"
  const val REMOTE_ANSWER = "oopbot_remote_answer"

  fun isHello(type: String?): Boolean = type == HELLO
  fun isResult(type: String?): Boolean = type == RESULT
  fun isProgress(type: String?): Boolean = type == PROGRESS
  fun isExit(type: String?): Boolean = type == EXIT
  fun isChannel(type: String?): Boolean = type == CHANNEL
  fun isAuthEvent(type: String?): Boolean = type == AUTH_EVENT
  fun isAuthPrompt(type: String?): Boolean = type == AUTH_PROMPT
  fun isAuthDismiss(type: String?): Boolean = type == AUTH_PROMPT_DISMISS
}
