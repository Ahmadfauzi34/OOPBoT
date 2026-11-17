# OOPBoT Rust Core (`src/`)

## Purpose
Core Rust crate (`oopbot`) compiled as both the Termux CLI/daemon binary (`oopbot` via `src/main.rs`) and the Android JNI/C-ABI shared library (`liboopbot.so` via `src/lib.rs` and `src/ffi.rs`). Orchestrates multi-provider LLM calls, tool execution, session/tab state, plugin lifecycles, JSONL RPC serving, and Termux TCP host bridging.

## Ownership
- **CLI & Mode Dispatch:** `src/main.rs` (interactive REPL, `-p` single prompt, `--mode rpc|catalog|scan-models`, `host <mode> <port>`).
- **Agent Loop & Provider Transport:** `src/agent.rs`, `src/gemini.rs` (multi-provider OpenAI-compat / Gemini / Anthropic requests & stream/schema handling), `src/events.rs`, `src/session.rs`, `src/models.rs`, `src/tools.rs`, `src/util.rs`.
- **Native FFI & Android JNI:** `src/ffi.rs` (`oopbot_*` C-ABI and `Java_io_jeph_oopbot_host_OopbotBridge_*` JNI exports).
- **Wire Integration Tests:** `tests/host_binary_wire.rs`.

## Local Contracts
- **Zero-OpenSSL Portability:** All HTTP/TLS dependencies (`reqwest`) MUST use `rustls-tls` + `webpki-roots` (no OpenSSL) so `aarch64-linux-android` cross-compilation in `.github/workflows/build-termux.yml` stays self-contained for Termux.
- **Gemini Schema Sanitization:** `google-generative-ai` rejects `additionalProperties` inside function declaration parameter schemas (HTTP 400). `src/gemini.rs` (`sanitize_gemini_function_declarations` / `sanitize_gemini_schema`) MUST strip unsupported fields before dispatching requests, and built-in tools should avoid emitting `additionalProperties`.
- **FFI Panic & ABI Safety:** All `extern "C"` C-ABI and JNI functions in `src/ffi.rs` MUST guard boundaries with `std::panic::catch_unwind`, validate null pointers/handles without panicking, and match the exact Java signatures in `android/app/src/main/kotlin/io/jeph/oopbot/host/OopbotBridge.kt`.
- **Atomic File Writes & Credential Safety:** Persistent config/session writes use `src/util.rs` (`write_json_atomic`); secrets (`auth.json`) must retain restrictive permissions (`0o600`).
- **Release Profile:** Keep `[profile.release]` (`lto = true`, `codegen-units = 1`, `strip = true`, `panic = "abort"`) clean and warn-free under `cargo clippy --all-targets --locked -- -D warnings`.

## Work Guidance
- Place core LLM/provider logic in `src/gemini.rs` or `src/agent.rs`, built-in filesystem/shell tools in `src/tools.rs`, model/provider catalog definitions in `src/models.rs`, and higher-level optional capabilities inside `src/plugins/`.
- Do not bypass `Plugin` / `Tool` traits by hardcoding feature logic directly into `src/main.rs` when it belongs in `src/plugins/` or `src/rpc/`.
- Preserve existing `cargo test` coverage and add unit or mock-HTTP tests alongside any schema, provider, or wire change.

## Verification
- `cargo clippy --all-targets --locked -- -D warnings`
- `cargo test --locked -- --test-threads=2`
- Release cross-check (in CI `.github/workflows/build-termux.yml`): `cargo build --release --locked --target aarch64-linux-android`

## Child graph Index
- [src/plugins/AGENTS.md](plugins/AGENTS.md) — Modular `Plugin` trait, built-in `todo`, `web_search`/`fetch_page`, `background`, `interact`, and `subagents` plugins.
- [src/rpc/AGENTS.md](rpc/AGENTS.md) — JSONL `--mode rpc` daemon, queue serialization, background auto-wakeup, UI dialog prompts, compaction, and session JSONL persistence.
- [src/host/AGENTS.md](host/AGENTS.md) — Native Termux `oopbot host <mode> <port>` TCP bridge replacing Node.js host scripts.
