package io.jeph.pipkin.host

/**
 * JNI bridge to `liboopbot.so` (built from the root Rust crate `src/ffi.rs`).
 *
 * Exposes the in-process Rust reference engine (`GeminiAgent`, `ModelCatalog`, `builtin_providers`,
 * `WorkspaceTabs`, and `TimelineEntry::StepsFold`) to the Android app via a single JSON
 * request/response protocol.
 */
internal object OopbotBridge {
  val isAvailable: Boolean = try {
    System.loadLibrary("oopbot")
    true
  } catch (_: UnsatisfiedLinkError) {
    false
  }

  /** Version of the bundled `liboopbot.so` Rust core. */
  @JvmStatic
  external fun version(): String

  /**
   * Returns the authoritative built-in provider & model reference catalog from Rust (`src/models.rs`)
   * as a JSON string (`{ version, defaultProvider, defaultModel, providers: [...] }`).
   */
  @JvmStatic
  external fun referenceCatalog(): String

  /** Creates a new native `OopbotHandle` (returns 0 on error). */
  @JvmStatic
  external fun engineNew(apiKey: String, defaultModel: String, autoApprove: Boolean): Long

  /**
   * Executes a JSON operation (`catalog`, `get_state`, `models`, `scan_models`, `set_provider`,
   * `set_model`, `set_thinking_level`, `configure_model`, `tabs`, `new_tab`, `switch_tab`,
   * `close_tab`, `timeline`, `clear`, `chat`).
   */
  @JvmStatic
  external fun engineRequest(handle: Long, requestJson: String): String

  /** Frees a native `OopbotHandle` created by [engineNew]. */
  @JvmStatic
  external fun engineFree(handle: Long)
}

