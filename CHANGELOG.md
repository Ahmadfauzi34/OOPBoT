# Changelog

All notable changes to **OOPBoT** are documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

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
