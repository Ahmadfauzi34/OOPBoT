# Native Termux TCP Host Bridge (`src/host/`)

## Purpose
Implements the zero-Node.js native TCP host bridge (`oopbot host <mode> <port>`) connecting Termux execution modes directly to Android's local `HostServer` socket over loopback TCP (`127.0.0.1:<port>`).

## Ownership
- **Mode Router & Handshake:** `src/host/mod.rs` (`run_host_mode`, `hello`, `login`, `rpc`, `utility`, `package_install`, `backup_create`, `backup_restore`).
- **File Explorer & Workspace Browsing:** `src/host/browse_files.rs`.
- **Git Inspection:** `src/host/git.rs` (`parse_git_diff`, `sha256_hex`, `handle_git_mode`).
- **Interactive PTY/Shell Terminal:** `src/host/terminal.rs`.
- **Utility Sub-Handlers:** `src/host/providers.rs`, `src/host/sessions.rs`, `src/host/remote.rs`, `src/host/backups.rs`, `src/host/ensure.rs`.
- **Tests:** `src/host/tests.rs` and binary wire test `tests/host_binary_wire.rs`.

## Local Contracts
- **TCP Handshake & Auth Token:** `OOPBOT_TOKEN` MUST be validated from env or first stdin line before opening `127.0.0.1:<port>` and immediately sent in `oopbot_hello` (`protocol: 1`, `oopbotVersion: CARGO_PKG_VERSION`).
- **Version Guard Compatibility:** `rpc` mode MUST validate `expectedOopbotVersion` via `check_rpc_expected_version` so Android `HostClient` detects stale Termux binaries and triggers upgrades safely. Version bumps MUST update `src/host/tests.rs` alongside `Cargo.toml` and `TermuxSetup.kt`.
- **Unsupported Legacy Node Modes:** `package_install`, `update_extensions`, and unsupported interactive SSH modes MUST return structured `oopbot_result` (`ok: false`, `code: "unsupported"`) rather than crashing or hanging stdout/TCP.
- **File & Output Sink Cleanup:** `handle_rpc_mode` MUST bind `set_output_sink(Some(...))` before starting `RpcServer` and clear `set_output_sink(None)` before emitting `oopbot_exit`.
- **Secret Hygiene:** Never log raw API keys or tokens in JSON error payloads; write `auth.json` with `0o600`.
- **Binary Upgrade Acceleration:** `ensure.rs` (`ensure_oopbot_binary_with`) uses `aria2c -x 8 -s 8 -k 1M` when available for remote HTTPS release downloads and falls back cleanly to `reqwest`, skipping `aria2c` for loopback test URLs.

## Work Guidance
- Any change to message `type`, `op`, `code`, or JSON payload fields MUST stay synchronized with `android/app/src/main/kotlin/io/jeph/oopbot/host/HostModels.kt`, `HostProtocol.kt`, and `tests/host_binary_wire.rs`.
- Use bounded temp-file/atomic JSON helpers (`write_json_atomic`) for config and backup persistence.

## Verification
- `cargo test --locked host::`
- `cargo test --locked --test host_binary_wire`
- Full crate regression: `cargo clippy --all-targets --locked -- -D warnings && cargo test --locked -- --test-threads=2`

## Child graph Index
- None (leaf domain; Android counterparty is `android/app/src/main/kotlin/io/jeph/oopbot/host/AGENTS.md`).
