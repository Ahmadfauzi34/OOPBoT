package io.jeph.oopbot.host

import io.jeph.oopbot.AppStrings
import io.jeph.oopbot.R
import org.json.JSONObject

/**
 * The host ran the operation and refused it; [code] is the host's machine code (`oopbot_version`, `bad_id`, …) and
 * [englishError] its reason as the host wrote it, for logs. [error] is that reason in the app's language, read when
 * it's shown so it follows a change of language; [message] is the same. [text] is null when the host gave no reason;
 * its arguments may say more, such as the form field a validation failure is about.
 */
internal class HostRejected(
  val code: String,
  val englishError: String,
  val text: HostText? = HostText(englishError),
  val data: JSONObject? = null,
) : Exception() {
  val error: String get() = text?.resolve() ?: AppStrings.get(R.string.host_rejected_without_reason)

  override val message: String get() = error

  companion object {
    /** Null for an `ok:true` `oopbot_result`. */
    fun from(result: JSONObject): HostRejected? {
      if (result.optBoolean("ok")) return null
      val code = result.optString("code", "error")
      val data = result.optJSONObject("data")
      val englishError = result.opt("error") as? String
      if (englishError.isNullOrEmpty()) return HostRejected(code, "The Termux helper failed.", text = null, data = data)
      return HostRejected(code, englishError, HostText.of(englishError, result.optJSONObject("i18n")), data = data)
    }
  }
}
