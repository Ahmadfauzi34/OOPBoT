# OOPBoT Plugins (`src/plugins/`)

## Purpose
Modular `Plugin` & `PluginRegistry` layer decoupling high-level agent capabilities (task tracking, web search/fetch, background shell execution, interactive user dialogs, and subagent coordination) from the core agent harness.

## Ownership
- **Registry & Built-in Subagent Plugin:** `src/plugins/mod.rs` (`Plugin` trait, `PluginRegistry`, `SubagentsPlugin`).
- **Web Search & Fetch Plugin:** `src/plugins/web_access/` (`mod.rs`, `providers.rs`, `html.rs`, `tests.rs`) — `WebAccessPlugin`, `WebSearchTool` (Tavily, Brave, Serper, SearXNG, DuckDuckGo + automatic public JSON API fallback: DDG Instant, Wikipedia, GitHub, Hacker News), and `FetchPageTool`.
- **Background Process Runner:** `src/plugins/background/` (`mod.rs`, `buffer.rs`, `process.rs`, `tests.rs`) — `BackgroundPlugin`, `BackgroundTool`, bounded output buffers, completion callbacks (`BgCompletionEvent`).
- **User Interaction Tools:** `src/plugins/interact.rs` (`InteractPlugin`, `notify_user`, `ask_user`, `UiDialogRequest` / `UiDialogAnswer`).
- **Todo Checklist Plugin:** `src/plugins/todo.rs` (`TodoPlugin`, `TodoTool`, `TODO_STATE_ENTRY`, `TODO_WIDGET_KEY`).

## Local Contracts
- **Strict Modularity:** Every new high-level capability MUST live under `src/plugins/` implementing `Plugin` (not hardcoded directly inside `src/agent.rs` or `src/main.rs`). Complex plugins with multiple concerns should use a subfolder (`src/plugins/<name>/mod.rs`).
- **Truthful Prompt Advertisement:** Never advertise unregistered or unimplemented tools in `Plugin::system_prompt_append()` (e.g., `SubagentsPlugin` returns `None` until delegation tools are actually registered).
- **Schema Compatibility:** Tool parameter schemas returned by `Tool::parameters()` MUST NOT include `additionalProperties` (to remain compatible with Gemini's `functionDeclarations` schema validator).
- **Legitimate Search Fallbacks (No Anti-Bot Bypass, No-Gzip Requirement):** `WebAccessPlugin` MUST NEVER attempt to solve or bypass anti-bot/CAPTCHA challenges (`HTTP 202`/`403`/`429`, `anomaly-modal`, `challenge-form`). On DuckDuckGo HTML challenges, `auto` mode MUST fall back cleanly to configured official search APIs or uncompressed public JSON endpoints (`ddg_instant`, `wikipedia`, `github`, `hn`) because `reqwest` intentionally excludes `gzip` in `Cargo.toml`.
- **Branch-Replayable State:** Stateful plugins MUST emit custom entries via `state_entry_for_tool(...)` and rebuild cleanly in `on_branch_replay(branch_entries)` across resume, switch, fork, clone, and compaction.
- **Bounded Background Buffers & Safe Callbacks:** `BackgroundPlugin` MUST keep process output bounded in `buffer.rs` and trigger completion wakeup callbacks asynchronously without holding registry locks across awaits or RPC I/O.
- **Interact Dual-Mode Fallback:** In non-RPC CLI mode (when UI callbacks are unset), `notify_user` and `ask_user` MUST still operate safely or fail with clear descriptive errors instead of hanging indefinitely.

## Work Guidance
- Use `extract_plugin_id` conventions so plugins activate seamlessly by ID, short alias, or legacy Pi extension path (`/extensions/<id>/index.js`).
- Keep widget line formatting (`render_widget`) and real-time `channel_status()` payloads aligned with Android listeners (`ExtensionWidget.kt`, `ChatTimeline.kt`).

## Verification
- `cargo test --locked plugins::`
- Full crate regression: `cargo clippy --all-targets --locked -- -D warnings && cargo test --locked -- --test-threads=2`

## Child graph Index
- None (leaf domain; `src/plugins/background/` and `src/plugins/web_access/` are governed by this contract).
