# Agent Client Protocol Bridge (`src/interfaces/acp/`)

## Purpose
Implements the Layer 3 Agent Client Protocol (ACP JSON-RPC 2.0 over newline-delimited stdio) client and bidirectional bridge translating external CLI agents (such as `agy acp`) into OOPBoT's Android JSONL RPC event stream.

## Ownership
- **Module Exports:** `src/interfaces/acp/mod.rs`.
- **Wire Types & Parsers:** `src/interfaces/acp/protocol.rs` (`IncomingJsonRpc`, `AcpSessionUpdate`, `AcpPermissionRequest`, `AcpPlanEntry`, `AcpModelOption`, `AcpConfigOption`, `AcpConfigChoice`, `parse_session_models`, `parse_session_config_options`, `map_acp_tool_kind_to_oopbot`, request/response builders).
- **Async Subprocess Client:** `src/interfaces/acp/client.rs` (`AcpClient`, `AcpLaunchConfig`, `AcpAgentEvent`, bounded stderr ring buffer, `kill_on_drop`).
- **OOPBoT JSONL Bridge:** `src/interfaces/acp/bridge.rs` (`AcpRpcBridge`, `AcpBridgeConfig`, JSONL session mirror with `acpSessionId` persistence, `session/load` recovery, and ACP model/thinking switching via `session/set_model` / `session/set_config_option`).
- **Tests & Gates:** `src/interfaces/acp/tests.rs`.

## Local Contracts
- **Strict Layering (DAG):** `src/interfaces/acp/` MUST NOT import `src/agent/`, `src/llm/`, or `src/plugins/`; it only depends on `src/common/` and `src/config/`.
- **Non-Blocking Concurrent I/O:** `AcpRpcBridge::run_loop_with_input` MUST multiplex Android RPC commands, incoming ACP `session/update` / `session/request_permission` frames, and in-flight `session/prompt` responses concurrently so `abort` (`session/cancel`) and `tool_approval` never deadlock.
- **No Silent Subprocess Hang:** When an ACP subprocess exits or closes `stdout`, `AcpClient` MUST immediately fail all pending request waiters and emit `AcpAgentEvent::Closed` with bounded `stderr` diagnostics.
- **Session Mirror & Model/Thinking Switching:** `AcpRpcBridge` MUST record `acpSessionId` in the JSONL session file, attempt `session/load` when supported by `agentCapabilities.loadSession` before falling back to `session/new`, and handle `get_available_models`, `set_model`, `get_available_thinking_levels`, and `set_thinking_level` via ACP `models` / `configOptions` (or sibling `(Level)` model variants) without altering native `settings.json` Pi defaults.

## Verification
- `cargo test --locked interfaces::acp:: -- --test-threads=2`
- Full crate regression: `cargo clippy --all-targets --locked -- -D warnings && cargo test --locked -- --test-threads=2`

## Child graph Index
- None (leaf domain; paired with `src/config/engines.rs` and `src/interfaces/host/`).
