# Changelog

All notable changes to **OOPBoT** are documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [0.5.23] - 2026-11-26

### Performance & Reliability
- **Bounded & 3-lane Native Termux Git Host (`src/interfaces/host/git.rs`):**
  - Routed every git subprocess through `run_git_bounded` (`process_group(0)` isolation, full process-group `SIGKILL` with `kill -0` verification retry, bounded reader join on both timeout/overflow [`200ms`] and success [`2s`] paths, and explicit `SpawnFailed` / `git log` `TooLarge` error surfacing).
  - Bounded diff parsing via `parse_git_diff_bounded` (`1 MiB` per-file early hunk drop, `7 MiB` total retained cap, exact CRLF byte accounting via `split_inclusive('\n')`, and `-c diff.renameLimit=200`).
  - Split `handle_git_mode` into 3 isolated, panic-resilient, queue-bounded (`32` queued/lane with immediate `code: "failed"` backpressure) worker lanes — `detect` (2), `commits` (2), and `heavy` (2) — so slow diffs or commits never block repo detection or commit lists.
- **Android Explorer / Git Changes Render Optimization (`android/.../ui/`):**
  - Paused off-screen `CircularProgressIndicator` redraws in `PaneExplorer` via `LocalSpinnersAnimate`, and suppressed duplicate toolbar spinners during initial loading.
  - Replaced full-pane `BoxWithConstraints` subcomposition in `ChangesPane` with `Modifier.onSizeChanged` and replaced `IntrinsicSize.Min` + per-row `Canvas` in `CommitsSection` with `Modifier.commitGraphRail` (`drawWithCache`, RTL-aware) + `LazyColumn` `contentType`.
  - Marked `DiffRowStyle` `@Immutable`, memoized `AnnotatedString` rows in `DiffView`, and switched Tree-Sitter diff highlighting to lazy visible + ±1 neighbor highlighting with `HighlightCache.shared` reuse and a `120_000`-char per-side budget (`DiffDocument.kt`).

### Fixed
- **Tolerant `tabs.json` Recovery (`src/session/tabs.rs`):** Recovers healthy tab entries individually on partial corruption, remaps `active_index` to the nearest surviving tab, and reserves `.corrupt-<ts>.bak` backups atomically (`hard_link` / `O_CREAT|O_EXCL`) without clobbering existing backups.
- **CI Reliability (`.github/workflows/android-test.yml`, `android/app/build.gradle.kts`):** Cached `~/.m2/repository` and forwarded Google's Maven Central mirror URL to Robolectric unit tests to eliminate cold-cache download flakes.

## [0.5.22] - 2026-11-26

### Added
- Layer 1 `src/context/` (`ContextManager`, `ContextPolicy`, `BudgetSnapshot`, `plan_canonical_compaction_*`, `assemble_compaction_prompt_with_limit`, `rebuild_canonical_history`) with CLI `/context`, `/compact [instructions]`, opt-in pre-turn/mid-turn auto-compaction, and atomic `0o600` `tabs.json` persistence.
- Atomic `edit_file` tool (`src/tools/fs.rs`) with `0o600` same-directory temp writes, UTF-8 BOM and per-line mixed CRLF/LF preservation, Pi-style numbered diffs (`details.{diff, firstChangedLine, linesAdded, linesRemoved}`), and accurate Android `StepFigure` badge counts on truncated diffs.
- CLI system instruction support for active plugin guidelines and `--append-system-prompt`.

### Fixed
- Hardened context compaction and session recovery across CLI & RPC:
  - Rejected empty/whitespace-only compaction summaries without replacing conversation history (both CLI `ContextManager` and RPC `perform_compaction`).
  - Forwarded bounded recent inline images (`inlineData`) with 1-to-1 synchronized `[gambar inline #N]` labels and deferred base64 cloning for multimodal compaction summarizers while omitting them on text-only models.
  - Added fingerprint-keyed `last_context_tokens` with local token-delta calibration, apples-to-apples anti-thrash across tool steps, post-compaction `OverLimit` dispatch gating, and `/compact` `OverLimit` warnings (including no-op and error outcomes).
  - Hardened `tabs.json` loading with OOB `active_index` clamping, `next_id` normalization, `.corrupt-<ts>.bak` quarantine with preserved `load_warning` across startup `--model` overrides, and provider/API compatibility checks on restored tabs (`sync_client_to_active_tab_model_or_fallback`, `/token`, `/tab`, `/close`).

## [0.5.21] - 2026-11-20

### Fixed
- Fixed external ACP tool execution hanging indefinitely in `Running...` when an agent requested permission (`session/request_permission`):
  - `AcpRpcBridge` now emits native Android `extension_ui_request` (`select` with the agent's permission options, or `confirm` fallback) alongside `oopbot_channel` `tool_approval`, and resolves pending ACP permissions directly from Android's `extension_ui_response` (`value`, `confirmed`, or `cancelled`).
  - Normalized ACP tool arguments & `locations` (`file_path`, `CodeContent`, `CommandLine`, etc.) to OOPBoT's `path`/`content`/`command` schema, mapped `"write"`/`"shell"` kinds cleanly, and forwarded non-terminal `tool_call_update` args/output incrementally via `toolcall_end` and `tool_execution_update`.
  - Added `~/` tilde expansion in `resolve_acp_path` for `fs/read_text_file` and `fs/write_text_file`.

## [0.5.20] - 2026-11-19

### Added
- Full ACP capability & OOPBoT JSONL RPC parity in `src/interfaces/acp/`:
  - Advertised ACP client capabilities (`fs.readTextFile`, `fs.writeTextFile`, `terminal: true`) and implemented Agent → Client handlers (`fs/read_text_file`, `fs/write_text_file`, `terminal/create`, `terminal/output`, non-blocking `terminal/wait_for_exit` via `AcpResponder`, `terminal/kill`, `terminal/release`) in `client_ops.rs` with process-tree cleanup on shutdown/drop.
  - Handled extended ACP `session/update` notifications (`available_commands_update`, `session_info_update`, `current_mode_update`, `usage_update`) and `session/prompt` `usage` metadata.
  - Implemented remaining JSONL RPC commands in `AcpRpcBridge`: `steer`, `follow_up`, `prompt` `streamingBehavior`, priority `clear_queue` + `queue_update`, `set_session_name` (persisted via `session_info`), `get_commands`, `get_session_stats` (live token/cost/contextUsage metrics), `get_last_assistant_text`, `get_fork_messages`, `export_html`, `new_session`, `switch_session`, `get_modes`, `set_mode`, `cycle_model`, `cycle_thinking_level`, `set_steering_mode`, `set_follow_up_mode`, `set_auto_compaction`, `set_auto_retry`, and `extension_ui_response`.

## [0.5.19] - 2026-11-19

### Added
- Model and thinking-level selection in external ACP mode (`get_available_models`, `set_model`, `get_available_thinking_levels`, `set_thinking_level` in `AcpRpcBridge`):
  - Exposes ACP `models.availableModels` and `configOptions` (`model` / `thought_level`) plus `config_option_update` notifications.
  - Supports both explicit `session/set_config_option` thinking controls and sibling `(Low)/(Medium)/(High)/(XHigh)` model variants via `session/set_model`, infers initial thinking level from the active model name, and persists per-engine defaults in `~/.oopbot/engines/<id>/manifest.json`.
  - Updated `ModelSheet.kt` so ACP models bypass Pi's `enabledModels` filter and `ChatController.kt` so ACP switches do not overwrite Pi's native `settings.json` defaults.

## [0.5.18] - 2026-11-19

### Fixed
- Fixed external ACP engine OAuth sign-in (`oopbot host engine_login`) never presenting the authorization code input box after opening the browser: `handle_engine_login_mode` now emits `oopbot_auth_prompt` (`type: "manual_code"`) alongside `oopbot_auth_url` (and flushes non-newline-terminated CLI prompts via `spawn_login_pipe_reader`), writes the pasted code to the child's stdin, and records `authenticated: true` in `~/.oopbot/engines/<id>/manifest.json` upon success.
- Added a one-tap **Paste** button inside `TextPrompt` (`LoginSheet.kt`) for `manual_code` prompts.

## [0.5.17] - 2026-11-19

### Fixed
- Fixed external Antigravity CLI (`antigravity-termux-standalone.tar.gz`) archive extraction failing with `Binary 'bin/agy' tidak ditemukan di dalam arsip`: matched the actual archive root twin-binary layout (`agy` + `agy.va39`) while preserving `bin/` and nested-folder compatibility and ensuring `0o755` permissions on companion binaries.
- Streamed large engine archive downloads in chunks with MB progress reporting (`connect_timeout` 30s / total 600s) instead of buffering the entire payload in memory.
- Expanded Termux engine prerequisite detection & installation to include `resolv-conf`, `ca-certificates`, and `qemu-user-aarch64` (when ARM CPU lacks LSE atomics) alongside `glibc-repo`/`glibc`, and injected `PREFIX`/`TERMUX_VERSION` into engine verification, login, and ACP subprocesses launched via `RunCommandService`.

## [0.5.16] - 2026-11-19

### Added
- External ACP (Agent Client Protocol, JSON-RPC 2.0 over stdio) CLI engine bridge (`src/interfaces/acp/`) with first-class **Antigravity CLI** (`wallentx/antigravity-cli-termux` -> `agy acp`) integration ([#40](https://github.com/Ahmadfauzi34/OOPBoT/pull/40)):
  - Downloadable engine catalog and manifest store (`src/config/engines.rs`, `~/.oopbot/engines/<id>/manifest.json`).
  - Bidirectional `AcpRpcBridge` translating OOPBoT JSONL RPC commands (`prompt`, `steer`, `follow_up`, `abort`, `get_state`, `get_messages`, `get_available_models`, `set_model`, `set_thinking_level`, `new_session`, `switch_session`, `export_html`, `extension_ui_response`) and ACP events/callbacks (`session/update`, `session/request_permission` mapped to Android's native `confirm` dialog, `fs/read_text_file`, `fs/write_text_file`, `terminal/*` lifecycle with `process_kill_tree` cleanup).
  - Native Termux host installer, uninstaller, and OAuth login bridge (`src/interfaces/host/engines.rs`: `engine_install`, `engine_login`, `engine_list`, `engine_uninstall`) with staging-directory archive extraction, backup/rollback on failure, automatic Termux `glibc-repo`/`glibc` installation (`manualCommand` fallback), and `acp:<id>` routing in `oopbot host rpc`.
  - Android unified provider list & `EngineSetupSheet` (`ProvidersScreens.kt`, `ProviderRows.kt`, `EngineSetupSheet.kt`, `HostClient`, `AccountRepository`, `Preferences.activeEngine`, `AgentProcess`) supporting one-tap download/install, browser + callback-paste OAuth login (`agy acp --login`), per-session engine header persistence, and active engine toggle.

### Fixed
- Hardened session context resilience across LLM transport retries, JSONL transcript recovery, context compaction, and live session reloads, backed by regression gate tests ([#39](https://github.com/Ahmadfauzi34/OOPBoT/pull/39)):
  - Treated HTTP `408 Request Timeout`, `429 Too Many Requests`, and `529 Overloaded` as retryable alongside `5xx`/network errors across Gemini, OpenAI Chat/Responses, and Anthropic clients.
  - Sanitized orphan `ToolCall` / `ToolResult` messages (`Session::sanitize_tool_Transcript`) after crashes, partial turns, or interrupted tool batches so strict tool-pairing APIs never reject subsequent turns with `400 Bad Request`.
  - Replaced single-shot context compaction with multi-step progressive fallback (`33%` -> `50%` -> `75%` -> deterministic emergency trim when summarizer LLM calls fail) and proactive `context_warning` notifications at 85% budget usage.
  - Reloaded disk changes via `SessionStore::sync_from_disk_if_newer` before RPC turns and skipped blank/corrupt JSONL lines gracefully instead of failing entire session loads.

## [0.5.15] - 2026-11-19

### Changed
- Reorganized Rust core (`src/`) into a strict 4-layer acyclic (DAG) module hierarchy: `common/`, `config/`, `llm/` (`LlmClient`), `tools/`, headless `agent/` (`AgentHarness`), `plugins/` (`todo/`, `web_access/`, `background/`), and `interfaces/` (`cli/`, `rpc/`, `host/`, `ffi/`), with slimmed `src/main.rs` dispatcher and synchronized `AGENTS.md` contracts ([#38](https://github.com/Ahmadfauzi34/OOPBoT/pull/38)).
- Clarified `BackgroundPlugin` prompt guidelines to reserve `background` for long-running processes and use `run_shell` for short one-off commands ([#38](https://github.com/Ahmadfauzi34/OOPBoT/pull/38)).

### Fixed
- Isolated internal background auto-wakeups into a dedicated `wakeup_queue` separate from user `steering_queue`/`follow_up_queue` so system wakeups never leak into `queue_update` (Android `STEERING` card with `Edit`/`Send now`) or `clear_queue` ([#38](https://github.com/Ahmadfauzi34/OOPBoT/pull/38)).
- Added race-safe auto-dismissal (`WakeupDismissNotifier` + `dismissed_bg_wakeups`) whenever finished background tasks are observed (`output`/`wait`/`list`) or cancelled (`kill`/`clear`/`shutdown_all`/`abort`/`background: stop`), eliminating delayed ghost wakeups from instant-failing tasks, guarded by regression gate tests ([#38](https://github.com/Ahmadfauzi34/OOPBoT/pull/38)).

## [0.5.14] - 2026-11-19

### Added
- Termux internet-bound Linux CLI bundle (`aria2`, `wget`, `jq`, `dnsutils`/`dig`, `netcat-openbsd`/`nc`, `openssh`/`ssh`, `git`) installed best-effort alongside mandatory `curl` in `TermuxSetup.fdroidCommand` and `oopbot-setup.sh`.
- Multi-connection `aria2c -x 8 -s 8 -k 1M` download acceleration (with `.aria2` cleanup and automatic `curl`/`reqwest` fallback) for GitHub Releases binary downloads during both Termux setup and native `oopbot host ensure` upgrades.
- Enriched `run_shell` tool description highlighting available internet CLI utilities (`aria2c -x 8 -s 8`, `curl`, `wget`, `jq`, `dig`/`nslookup`, `nc`, `ssh`/`scp`, `git`).

## [0.5.13] - 2026-11-18

### Added
- Automatic Google Search Grounding (`google_search`) via isolated Gemini `:generateContent` sub-calls in `WebAccessPlugin` when a Google Gemini provider is connected, with dynamic **Google Search** / **OOPBoT web access** switch branding in Settings (`ExtensionsSection`).
- Multi-engine `WebAccessPlugin` fallback chain (`tavily`, `brave`, `serper`, `searxng`, DuckDuckGo HTML challenge detection, and zero-key public JSON endpoints: `ddg_instant`, `wikipedia` `en`/`id`, `github`, `hn`) with cleaned HTML page fetching.
- GNU Bash command runner (`resolve_command_shell`) preferring `$PREFIX/bin/bash`, `/data/data/com.termux/files/usr/bin/bash`, or `/bin/bash` with `-c` (falling back to `sh -c`) across `run_shell` and `background` tasks so bash builtins (`time`, `history`, `[[ ... ]]`, `pipefail`, arrays, brace expansion) work reliably.

### Fixed
- Deferred fresh-install onboarding `AutoInstall` (`RunCommandService`) until returning from Termux after copying the setup command, preventing Termux's premature `Error Code: 2 (allow-external-apps)` notification.

## [0.5.12] - 2026-11-18

### Changed
- Build and sign R8-minified, resource-shrunk, 16 KB page-aligned release APKs (`:app:assembleRelease` + `sign-release-apk.sh`) in `.github/workflows/build-termux.yml`, and guard C/C++ JNI entrypoints (`TreeSitter`, `OpenClProbe`, `LlamaServer`) under R8 full mode.
- Replace fixed `tween(220)` panel and stack transitions with `PanelFlingSpring` (`DampingRatioNoBouncy`, `StiffnessMediumLow`) in `MainScreen` and `StackTransition` ([#37](https://github.com/Ahmadfauzi34/OOPBoT/pull/37)).
- Eliminate mid-drag recompositions in `MainScreen`, `WorkspacePane`, and `FileExplorer` using `snapshotFlow`, `derivedStateOf`, and lambda state readers (`isCovered`, `isOpen`), plus offscreen `alpha` culling in `MobilePanels` ([#37](https://github.com/Ahmadfauzi34/OOPBoT/pull/37)).
- Isolate `StreamList` (`ChatStream`) from background `markdown.version` and `ChatImages.decodeVersion` updates while scrolled up (`follow == false`), and hoist static `OopbotMarkdown` lambdas ([#37](https://github.com/Ahmadfauzi34/OOPBoT/pull/37)).

### Added
- Automated Compose compiler stability and skippability verification in CI (`android/scripts/check-compose-reports.py` over `:app:compileDebugKotlin -Poopbot.composeReports=true`) ([#37](https://github.com/Ahmadfauzi34/OOPBoT/pull/37)).
- Direct Android framework `SQLiteOpenHelper` verification (`AndroidSqlDbTest`) via Robolectric 4.16.1 across API 29 (`minSdk`) and API 35 ([#37](https://github.com/Ahmadfauzi34/OOPBoT/pull/37)).

## [0.5.11] - 2026-11-18

### Added
- Zero-dependency SQLite (`oopbot.db`) persistence layer (`OopbotDatabase`, `AndroidSqlDb`, `OopbotSchema`) using WAL mode, `PRAGMA foreign_keys = ON`, and API 29-compatible FTS4 (`unicode61`) full-text search ([#36](https://github.com/Ahmadfauzi34/OOPBoT/pull/36)).
- One-time `Preferences` migration from large XML JSON blobs into `workspace_prefs` and `session_meta`, lazy `NoticeStore` migration, offline/subagent `message_cache`, debounced FTS4 archive search, and process-death recovery for `chat_drafts` and `chat_inbox` ([#36](https://github.com/Ahmadfauzi34/OOPBoT/pull/36)).
- Hierarchical `AGENTS.md` repository contracts across root, `src/`, and `android/` domains.

### Changed
- Added bounded `HighlightCache`, `ChatImages` thumbnail LRU, `SourceLinesCache`, `TableWidthsCache`, and `Modifier.layout`-based `MarkdownTable` sizing ([#34](https://github.com/Ahmadfauzi34/OOPBoT/pull/34), [#35](https://github.com/Ahmadfauzi34/OOPBoT/pull/35)).
- Annotated timeline and diff models with `@Immutable`, made `SelectionContainer` opt-in in `FileScreen`, pre-composed `SwipeAway`, optimized `OopbotSheet` close tracking, and added `BottomAnchor` revision tracking and `TransitionFrame` deferred loading ([#34](https://github.com/Ahmadfauzi34/OOPBoT/pull/34), [#35](https://github.com/Ahmadfauzi34/OOPBoT/pull/35)).

### Fixed
- Applied formal-audit hardening across `core`, `rpc`, and `host` modules and fixed the Linux zombie-process check in TypeScript helper-daemon tests ([#33](https://github.com/Ahmadfauzi34/OOPBoT/pull/33)).

## [0.5.10] - 2026-11-17

### Changed
- Cached draw allocations, annotated immutable Compose models, and tuned R8/Gradle optimization settings ([#32](https://github.com/Ahmadfauzi34/OOPBoT/pull/32)).

## [0.5.9] - 2026-11-17

### Fixed
- Stripped unsupported JSON Schema fields from Gemini tool declarations (including `background` plugin schemas) to prevent Google API schema rejection ([#31](https://github.com/Ahmadfauzi34/OOPBoT/pull/31)).

## [0.5.8] - 2026-11-17

### Added
- Modularized `BackgroundPlugin` with automatic session wake-up notifications and `interact` tools across plugins and RPC ([#30](https://github.com/Ahmadfauzi34/OOPBoT/pull/30)).

## [0.5.7] - 2026-11-17

### Changed
- Completed multi-stage removal of legacy Pi JS extensions, Node host fallback, and `pi-coding-agent` assets ([#29](https://github.com/Ahmadfauzi34/OOPBoT/pull/29)).
- Renamed remaining internal packages, wire protocol identifiers, JNI symbols, native libraries, UI theme/resources, and helper scripts from `Pipkin`/`Pi` to `OOPBoT`/`oopbot` ([#29](https://github.com/Ahmadfauzi34/OOPBoT/pull/29)).

## [0.5.6] - 2026-11-16

### Added
- Extracted `TodoPlugin` into `src/plugins/todo.rs` with full capability support and session persistence ([#27](https://github.com/Ahmadfauzi34/OOPBoT/pull/27)).
- Enabled `:app:ktlintCheck`, `:app:detektDebug`, `:app:detektDebugUnitTest`, and `HostModelsWireContractTest` gates in Android CI ([#23](https://github.com/Ahmadfauzi34/OOPBoT/pull/23)).

### Changed
- Modularized Rust `src/rpc/` and `src/host/` components and resolved Clippy warnings ([#24](https://github.com/Ahmadfauzi34/OOPBoT/pull/24)).

### Fixed
- Hardened error handling across LLM clients, host bridge, RPC, FFI, and `TodoPlugin` batch/payload invariants ([#28](https://github.com/Ahmadfauzi34/OOPBoT/pull/28)).
- Disambiguated duplicate model display names in `ModelSheet` ([#26](https://github.com/Ahmadfauzi34/OOPBoT/pull/26)).

## [0.5.5] - 2026-11-13

### Fixed
- Enforced `>= 0.5.5` strict semver version guards and sanitization across Termux bootstrap, `ensure`, and RPC handshakes, with `ETXTBSY` retry on binary version checks ([#19](https://github.com/Ahmadfauzi34/OOPBoT/pull/19), [#20](https://github.com/Ahmadfauzi34/OOPBoT/pull/20)).
- Implemented non-blocking `RpcRuntime` cancellation (`abort`/`steer`), full session history rebuild, and test isolation ([#21](https://github.com/Ahmadfauzi34/OOPBoT/pull/21)).
- Resolved host utility, multi-provider auth/model scan, OpenAI `/responses`, native `pty_unix`, local model sync, session metadata, path containment, and plugin child-kill / live-widget regressions, backed by live binary wire tests ([#22](https://github.com/Ahmadfauzi34/OOPBoT/pull/22)).
- Fixed `$` escaping in `BOOTSTRAP` so exported `OOPBOT` paths resolve properly in Termux ([#18](https://github.com/Ahmadfauzi34/OOPBoT/pull/18)).
- Added Android unit test workflow in GitHub Actions ([#19](https://github.com/Ahmadfauzi34/OOPBoT/pull/19)).

## [0.5.4] - 2026-11-11

### Added
- Unified the agent execution loop so RPC `execute_prompt` delegates to `GeminiAgent::chat_turn` with `RpcUiState`, `RpcEventSink`, `RpcApprover` (`tool_approval` protocol), non-blocking reader task, and selective dangerous command approval ([#14](https://github.com/Ahmadfauzi34/OOPBoT/pull/14), [#15](https://github.com/Ahmadfauzi34/OOPBoT/pull/15), [#16](https://github.com/Ahmadfauzi34/OOPBoT/pull/16), [#17](https://github.com/Ahmadfauzi34/OOPBoT/pull/17)).
- Implemented native file explorer, folder browser, git host, terminal modes, and `pipkin_channel` extension event routing ([#7](https://github.com/Ahmadfauzi34/OOPBoT/pull/7), [#8](https://github.com/Ahmadfauzi34/OOPBoT/pull/8), [#9](https://github.com/Ahmadfauzi34/OOPBoT/pull/9)).
- Migrated Android UI branding, config paths, `SharedPreferences`, `TermuxBuild`, and setup scripts toward OOPBoT ([#5](https://github.com/Ahmadfauzi34/OOPBoT/pull/5), [#6](https://github.com/Ahmadfauzi34/OOPBoT/pull/6), [#10](https://github.com/Ahmadfauzi34/OOPBoT/pull/10), [#11](https://github.com/Ahmadfauzi34/OOPBoT/pull/11), [#12](https://github.com/Ahmadfauzi34/OOPBoT/pull/12), [#13](https://github.com/Ahmadfauzi34/OOPBoT/pull/13)).

## [0.5.3] - 2026-11-09

### Fixed
- Returned `piVersion` in host `ensure` output, validated `oopbot` version in `HostLauncher`, and escaped `$PREFIX` in `fdroidCommand` ([#4](https://github.com/Ahmadfauzi34/OOPBoT/pull/4)).

## [0.5.2] - 2026-11-09

### Added
- Native Tokio host daemon eliminating the Node.js Termux runtime requirement ([#3](https://github.com/Ahmadfauzi34/OOPBoT/pull/3)).
- Pure OOP plugin architecture with dynamic extension registry and multi-provider neutrality across Gemini, Anthropic, OpenAI, OpenRouter, DeepSeek, Kimi, and Z.ai.
- Persistent context-window token counts and cost across RPC sessions.

### Fixed
- Used `x-goog-api-key` for native Google AI Studio endpoints.

## [0.5.1] - 2026-11-08

### Changed
- Decoupled Android and Termux host setup from `@earendil-works/pi-coding-agent`.

### Fixed
- Added `gh release download` fast path in `PinnedSources` and pinned Gradle 9.8.2 CI installation.

## [0.5.0] - 2026-11-08

### Changed
- Switched chat model selection to live auto-scanned models from configured provider tokens, removing hardcoded chat models.

### Fixed
- Prevented sending `Authorization: Bearer` headers to Google Generative Language API endpoints.

## [0.4.0] - 2026-11-08

### Added
- Concept 2 Termux Rust Host integration (`oopbot --mode rpc` + `oopbot-sdk`).

## [0.3.1] - 2026-11-07

### Added
- Live `/scan` and `/token` provider model discovery and FFI `scan_models` / `set_provider` bindings.

### Fixed
- Standardized CI output to `arm64-v8a` APK discovery across `app/build`.

## [0.3.0] - 2026-11-07

### Added
- Integrated the Jetpack Compose Android app under `android/` with `liboopbot.so` JNI bridge and GitHub Actions APK CI.

## [0.2.0] - 2026-11-07

### Added
- Hybrid Rust core (`liboopbot.so` + CLI), tabbed sessions, model catalog, and collapsible step folding.

## [0.1.0] - 2026-11-07

### Added
- Initial clean release of the OOPBoT Termux Rust agent harness.

[Unreleased]: https://github.com/Ahmadfauzi34/OOPBoT/compare/v0.5.12...HEAD
[0.5.12]: https://github.com/Ahmadfauzi34/OOPBoT/compare/v0.5.11...v0.5.12
[0.5.11]: https://github.com/Ahmadfauzi34/OOPBoT/compare/v0.5.10...v0.5.11
[0.5.10]: https://github.com/Ahmadfauzi34/OOPBoT/compare/v0.5.9...v0.5.10
[0.5.9]: https://github.com/Ahmadfauzi34/OOPBoT/compare/v0.5.8...v0.5.9
[0.5.8]: https://github.com/Ahmadfauzi34/OOPBoT/compare/v0.5.7...v0.5.8
[0.5.7]: https://github.com/Ahmadfauzi34/OOPBoT/compare/v0.5.6...v0.5.7
[0.5.6]: https://github.com/Ahmadfauzi34/OOPBoT/compare/v0.5.5...v0.5.6
[0.5.5]: https://github.com/Ahmadfauzi34/OOPBoT/compare/v0.5.4...v0.5.5
[0.5.4]: https://github.com/Ahmadfauzi34/OOPBoT/compare/v0.5.3...v0.5.4
[0.5.3]: https://github.com/Ahmadfauzi34/OOPBoT/compare/v0.5.2...v0.5.3
[0.5.2]: https://github.com/Ahmadfauzi34/OOPBoT/compare/v0.5.1...v0.5.2
[0.5.1]: https://github.com/Ahmadfauzi34/OOPBoT/compare/v0.5.0...v0.5.1
[0.5.0]: https://github.com/Ahmadfauzi34/OOPBoT/compare/v0.4.0...v0.5.0
[0.4.0]: https://github.com/Ahmadfauzi34/OOPBoT/compare/v0.3.1...v0.4.0
[0.3.1]: https://github.com/Ahmadfauzi34/OOPBoT/compare/v0.3.0...v0.3.1
[0.3.0]: https://github.com/Ahmadfauzi34/OOPBoT/compare/v0.2.0...v0.3.0
[0.2.0]: https://github.com/Ahmadfauzi34/OOPBoT/compare/v0.1.0...v0.2.0
[0.1.0]: https://github.com/Ahmadfauzi34/OOPBoT/releases/tag/v0.1.0
