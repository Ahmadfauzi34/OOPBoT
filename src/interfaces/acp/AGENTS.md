# Agent Client Protocol Bridge (`src/interfaces/acp/`)

## Purpose
Implements the Layer 3 Agent Client Protocol (ACP JSON-RPC 2.0 over newline-delimited stdio) client and bidirectional bridge translating external CLI agents (such as `agy acp`) into OOPBoT's Android JSONL RPC event stream.

## Ownership
- **Module Exports:** `src/interfaces/acp/mod.rs`.
- **Wire Types & Parsers:** `src/interfaces/acp/protocol.rs` (`IncomingJsonRpc`, `AcpSessionUpdate`, `AcpPermissionRequest`, `AcpPlanEntry`, `AcpModelOption`, `AcpConfigOption`, `AcpConfigChoice`, `AcpSlashCommand`, `AcpSessionMode`, `AcpUsageStats`, `AcpUsageSample`, `parse_session_models`, `parse_session_config_options`, `parse_session_modes`, `parse_available_commands`, `extract_usage_stats`, `map_acp_tool_kind_to_oopbot`, request/response builders).
- **Async Subprocess Client:** `src/interfaces/acp/client.rs` (`AcpClient`, `AcpResponder`, `AcpLaunchConfig`, `AcpAgentEvent`, bounded stderr ring buffer, `kill_on_drop`).
- **Client-Side FS, Terminal & HTML Export Ops:** `src/interfaces/acp/client_ops.rs` (`handle_fs_read_text_file`, `handle_fs_write_text_file`, `AcpTerminalManager`, `AcpTerminalExit`, `render_acp_session_html`).
- **OOPBoT JSONL Bridge:** `src/interfaces/acp/bridge.rs` (`AcpRpcBridge`, `AcpBridgeConfig`, JSONL session mirror with `acpSessionId` & `session_info` persistence, `session/load` recovery, priority `steer`/`follow_up` queues, usage/stats tracking, and ACP model/thinking/mode switching).
- **Tests & Gates:** `src/interfaces/acp/tests.rs`.

## Local Contracts
- **Strict Layering (DAG):** `src/interfaces/acp/` MUST NOT import `src/agent/`, `src/llm/`, or `src/plugins/`; it only depends on `src/common/` and `src/config/`.
- **Non-Blocking Concurrent I/O:** `AcpRpcBridge::run_loop_with_input` MUST multiplex Android RPC commands, incoming ACP `session/update` / `session/request_permission` / `fs/*` / `terminal/*` frames, and in-flight `session/prompt` responses concurrently; `terminal/wait_for_exit` MUST wait via a spawned task + `AcpResponder` so the bridge event loop is never blocked.
- **No Silent Subprocess Hang:** When an ACP subprocess exits or closes `stdout`, `AcpClient` MUST immediately fail all pending request waiters and emit `AcpAgentEvent::Closed` with bounded `stderr` diagnostics, and `AcpTerminalManager::shutdown_all` MUST terminate child process trees on exit.
- **Session Mirror & Full Capability Parity:** `AcpRpcBridge` MUST record `acpSessionId` and `session_info` in the JSONL session file, attempt `session/load` when supported by `agentCapabilities.loadSession` before falling back to `session/new`, advertise `fs.readTextFile`/`fs.writeTextFile`/`terminal` capabilities cleanly, and support `steer`, `follow_up`, `clear_queue`, `set_session_name`, `get_commands`, `get_session_stats`, `get_last_assistant_text`, `get_fork_messages`, `export_html`, `new_session`, `switch_session`, `get_modes`, `set_mode`, `cycle_model`, `cycle_thinking_level`, `set_steering_mode`, `set_follow_up_mode`, `set_auto_compaction`, `set_auto_retry`, and `extension_ui_response`.

## Verification
- `cargo test --locked interfaces::acp:: -- --test-threads=2`
- Full crate regression: `cargo clippy --all-targets --locked -- -D warnings && cargo test --locked -- --test-threads=2`

## Child graph Index
- None (leaf domain; paired with `src/config/engines.rs` and `src/interfaces/host/`).
