package io.jeph.pipkin.host

/**
 * JNI bridge to `liboopbot.so` (built from the root Rust crate `src/ffi.rs`).
 *
 * Exposes the in-process Rust agent harness (`GeminiAgent`, `ModelCatalog`, `WorkspaceTabs`,
 * and `TimelineEntry::StepsFold`) to the Android app via a single JSON request/response protocol.
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

  /** Creates a new native `OopbotHandle` (returns 0 on error). */
  @JvmStatic
  external fun engineNew(apiKey: String, defaultModel: String, autoApprove: Boolean): Long

  /**
   * Executes a JSON operation (`models`, `set_model`, `tabs`, `new_tab`, `switch_tab`,
   * `close_tab`, `timeline`, `chat`).
   */
  @JvmStatic
  external fun engineRequest(handle: Long, requestJson: String): String

  /** Frees a native `OopbotHandle` created by [engineNew]. */
  @JvmStatic
  external fun engineFree(handle: Long)
}
