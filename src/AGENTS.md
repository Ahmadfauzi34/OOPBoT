# OOPBoT Rust Core (`src/`)

## Purpose
Core Rust crate (`oopbot`) compiled as both the Termux CLI/daemon binary (`oopbot` via `src/main.rs`) and the Android JNI/C-ABI shared library (`liboopbot.so` via `src/lib.rs` and `src/interfaces/ffi/`). Orchestrates multi-provider LLM calls, tool execution, session/tab state, plugin lifecycles, JSONL RPC serving, and Termux TCP host bridging.

## Ownership
- **Binary Entrypoint (`src/main.rs`):** Thin mode dispatcher (~45 LOC) routing CLI flags into `src/interfaces/{cli,rpc,host}` and `src/config/`.
- **Layer 3 Interface Adapters (`src/interfaces/`):**
  - **CLI (`src/interfaces/cli/`):** Argument parsing, `.env` loading, `--mode scan-models`, single-prompt `-p` (`mod.rs`), interactive REPL (`repl.rs`), slash commands (`slash.rs`), terminal `CliEventSink`/`CliApprover`/renderers (`ui.rs`), and env credential resolution (`credentials.rs`).
  - **RPC Daemon (`src/interfaces/rpc/`):** Asynchronous JSONL `--mode rpc` daemon for Android (`mod.rs`, `runtime.rs`, `dispatch.rs`, `turn.rs`, `slash.rs`, `session_ops.rs`, `session_store.rs`, `export_html.rs`, `events.rs`, `approver.rs`, `compaction.rs`, `providers.rs`, `tests/`).
  - **Native Host Bridge (`src/interfaces/host/`):** Zero-Node.js Termux TCP host bridge (`oopbot host <mode> <port>`).
  - **ACP External Engine Bridge (`src/interfaces/acp/`):** Agent Client Protocol (JSON-RPC 2.0 stdio) client & bidirectional OOPBoT JSONL RPC bridge (`protocol.rs`, `client.rs`, `bridge.rs`, `tests.rs`).
  - **Native FFI & Android JNI (`src/interfaces/ffi/`):** `mod.rs` (`OopbotHandle` lifecycle), `c_abi.rs` (`oopbot_*` C-ABI exports), `jni.rs` (`Java_io_jeph_oopbot_host_OopbotBridge_*` JNI exports), `dispatch.rs` (`dispatch_request` JSON router), and `tests.rs`.
- **Neutral Shared Foundation (`src/common/`):** `src/common/error.rs` (`BoxError`, shared HTTP/HTML error extraction), `src/common/fs.rs` (`read_json_file`, `write_text_atomic`, `write_json_atomic`), `src/common/time.rs` (`epoch_ms`, ISO-8601 format/parse). Must remain free of domain dependencies.
- **Multi-Provider LLM Transport (`src/llm/`):** `LlmClient` (`src/llm/mod.rs`), shared types (`src/llm/types.rs`), HTTP response/effort helpers (`src/llm/http.rs`), and isolated wire adapters (`src/llm/gemini.rs`, `src/llm/openai_chat.rs`, `src/llm/openai_responses.rs`, `src/llm/anthropic.rs`). Must NOT depend on `plugins/` or `agent/`.
- **Catalog, Discovery & Config Persistence (`src/config/`):** `src/config/catalog.rs` (`ModelInfo`, `ModelCost`, `ModelCatalog`, `ProviderCatalogEntry`, glob scope), `src/config/builtin_providers.rs` (built-in provider/model definitions), `src/config/discovery.rs` (`/models` response parsing & capability inference), `src/config/engines.rs` (downloadable external ACP CLI engine catalog & `~/.oopbot/engines/<id>/manifest.json`), and `src/config/store.rs` (single source of truth for `auth.json` at `0o600`, `models.json`, `settings.json`, and `~/.config/oopbot/.env`). Must only depend on `src/common/`.
- **Headless Agent Loop (`src/agent/`) & Built-in Tools (`src/tools/`):**
  - `src/agent/`: `mod.rs` (`AgentHarness`, `Agent`, `Approver`, `ChatInput`, `TurnOutcome`), `runner.rs` (cancellable turn/tool loop & compaction summary), `events.rs` (`AgentEvent`, `EventSink`, `NoopEventSink`, `MemoryEventCollector`, `TimelineEntry`), `session.rs` (`ChatSession`, `WorkspaceTabs`). Must remain headless (no terminal `stdin`/`stdout` I/O and no dependency on `interfaces/` or `plugins/`).
  - `src/tools/`: `mod.rs` (`Tool`/`ToolRegistry`), `shell.rs` (`RunShell` & shell resolution/safety), `fs.rs` (`ReadFile`/`WriteFile`/`EditFile`/`ListDir`).
- **Wire Integration Tests:** `tests/host_binary_wire.rs`.

## Local Contracts
- **Strict Acyclic Layering (DAG):** `common/` depends on no domain modules; `config/`, `tools/`, and `llm/` must NOT import `agent/`, `plugins/`, or `interfaces/`; `agent/` must NOT import `plugins/` or `interfaces/`.
- **Zero-OpenSSL Portability:** All HTTP/TLS dependencies (`reqwest`) MUST use `rustls-tls` + `webpki-roots` (no OpenSSL) so `aarch64-linux-android` cross-compilation in `.github/workflows/build-termux.yml` stays self-contained for Termux.
- **Gemini Schema Sanitization:** `google-generative-ai` rejects `additionalProperties` inside function declaration parameter schemas (HTTP 400). `src/llm/gemini.rs` (`sanitize_gemini_function_declarations` / `sanitize_gemini_schema`) MUST strip unsupported fields before dispatching requests, and built-in tools/plugins should avoid emitting `additionalProperties` (guarded by `plugins::tests::all_default_tool_and_plugin_schemas_are_gemini_compatible`).
- **FFI Panic & ABI Safety:** All `extern "C"` C-ABI and JNI functions in `src/interfaces/ffi/` MUST guard boundaries with `std::panic::catch_unwind`, validate null pointers/handles without panicking, and match the exact Java signatures in `android/app/src/main/kotlin/io/jeph/oopbot/host/OopbotBridge.kt`.
- **Atomic File Writes & Credential Safety:** Persistent config/session writes use `src/common/fs.rs` (`write_json_atomic` / `write_text_atomic`) via `src/config/store.rs`; secrets (`auth.json` and `~/.config/oopbot/.env`) must retain restrictive permissions (`0o600`).
- **Release Profile:** Keep `[profile.release]` (`lto = true`, `codegen-units = 1`, `strip = true`, `panic = "abort"`) clean and warn-free under `cargo clippy --all-targets --locked -- -D warnings`.
- **Shell Command Runner (`RunShell` & `Background`):** Both `src/tools/shell.rs` (`run_shell`) and `src/plugins/background/process.rs` (`background`) MUST execute commands via `resolve_command_shell()` (preferring `$PREFIX/bin/bash` / `/data/data/com.termux/files/usr/bin/bash` on Termux or `/bin/bash` on Linux with `-c`, falling back to `sh -c` only when `bash` is absent) so bash builtins (`time`, `history`, `[[ ... ]]`, `pipefail`, arrays) work reliably.

## Work Guidance
- Place core LLM/provider wire logic inside the matching protocol file in `src/llm/` (`gemini.rs`, `openai_chat.rs`, `openai_responses.rs`, `anthropic.rs`), headless agent orchestration in `src/agent/`, external adapters in `src/interfaces/{acp,cli,rpc,host,ffi}/`, built-in filesystem/shell tools in `src/tools/`, model catalog / discovery / config persistence in `src/config/`, and higher-level optional capabilities inside `src/plugins/`.
- Do not bypass `Plugin` / `Tool` traits by hardcoding feature logic directly into `src/main.rs` or `src/interfaces/cli/` when it belongs in `src/plugins/` or `src/interfaces/rpc/`.
- Preserve existing `cargo test` coverage and add unit or mock-HTTP tests alongside any schema, provider, or wire change.

## Verification
- `cargo clippy --all-targets --locked -- -D warnings`
- `cargo test --locked -- --test-threads=2`
- Release cross-check (in CI `.github/workflows/build-termux.yml`): `cargo build --release --locked --target aarch64-linux-android`

## Child graph Index
- [src/plugins/AGENTS.md](plugins/AGENTS.md) — Modular `Plugin` trait, built-in `todo`, `web_search`/`fetch_page`, `background`, `interact`, and `subagents` plugins.
- [src/interfaces/rpc/AGENTS.md](interfaces/rpc/AGENTS.md) — JSONL `--mode rpc` daemon, queue serialization, background auto-wakeup, UI dialog prompts, compaction, and session JSONL persistence.
- [src/interfaces/host/AGENTS.md](interfaces/host/AGENTS.md) — Native Termux `oopbot host <mode> <port>` TCP bridge replacing Node.js host scripts.
- [src/interfaces/acp/AGENTS.md](interfaces/acp/AGENTS.md) — Agent Client Protocol (JSON-RPC 2.0 stdio) client & OOPBoT RPC bridge for external CLI engines.
