package io.jeph.oopbot.host

/**
 * The release's names in Termux, apart from OOPBoT Dev's (src/debug) so both builds can share one Termux: Pi's config
 * and the helper under `~/.oopbot`, the host under `~/.local/share/oopbot`, `oopbot-start`.
 */
internal object TermuxBuild {
  const val NAME = "oopbot"

  /** The first of the five loopback ports the Play-Termux helper may listen on. */
  const val HELPER_FIRST_PORT = 47310

  /** The loopback port Pi reaches OOPBoT's local models on, just past the helper's. */
  const val LOCAL_MODEL_PORT = 47315
}
