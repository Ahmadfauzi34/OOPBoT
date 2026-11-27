# Native Termux TCP Host Bridge (`src/interfaces/host/`)

## Purpose
Implements the zero-Node.js native TCP host bridge (`oopbot host <mode> <port>`) connecting Termux execution modes directly to Android's local `HostServer` socket over loopback TCP (`127.0.0.1:<port>`).

## Ownership
- **Mode Router & Handshake:** `src/interfaces/host/mod.rs` (`run_host_mode`, `hello`, `login`, `engine_install`, `engine_login`, `rpc`, `utility`, `package_install`, `backup_create`, `backup_restore`).
- **External ACP Engine Manager:** `src/interfaces/host/engines.rs` (`handle_engine_list`, `handle_engine_uninstall`, `install_engine_archive_with_prefix`, `handle_engine_install_mode`, `handle_engine_login_mode`, `detect_session_engine`, `is_termux_glibc_ready`, `missing_termux_engine_prereqs`, `termux_subprocess_env`).
- **File Explorer & Workspace Browsing:** `src/interfaces/host/browse_files.rs`.
- **Git Inspection:** `src/interfaces/host/git.rs` (`parse_git_diff`, `sha256_hex`, `handle_git_mode`).
- **Interactive PTY/Shell Terminal:** `src/interfaces/host/terminal.rs`.
- **Utility Sub-Handlers:** `src/interfaces/host/providers.rs`, `src/interfaces/host/sessions.rs`, `src/interfaces/host/remote.rs`, `src/interfaces/host/backups.rs`, `src/interfaces/host/ensure.rs`.
- **Tests:** `src/interfaces/host/tests.rs` and binary wire test `tests/host_binary_wire.rs`.

## Local Contracts
- **TCP Handshake & Auth Token:** `OOPBOT_TOKEN` MUST be validated from env or first stdin line before opening `127.0.0.1:<port>` and immediately sent in `oopbot_hello` (`protocol: 1`, `oopbotVersion: CARGO_PKG_VERSION`).
- **Version Guard Compatibility:** `rpc` mode MUST validate `expectedOopbotVersion` via `check_rpc_expected_version` so Android `HostClient` detects stale Termux binaries and triggers upgrades safely. Version bumps MUST update `src/interfaces/host/tests.rs` alongside `Cargo.toml` and `TermuxSetup.kt`.
- **External ACP Engine Routing & Safe Install:** `handle_rpc_mode` routes `params["engine"] == "acp:<id>"` (or JSONL session header `"engine": "acp:<id>"`) to `AcpRpcBridge` using `load_engine_manifest`, returning `code: "engine_not_installed"` or `code: "acp_start_failed"` on failure without touching native `RpcServer`. `install_engine_archive_with_prefix` MUST stream tarball downloads to disk, locate both flat twin-binary layouts (`agy` + `agy.va39`) and `bin/`/nested layouts, mark companion binaries `0o755`, verify, and atomically swap `~/.oopbot/engines/<id>/` with backup/rollback on error. `handle_engine_install_mode` MUST attempt Termux runtime auto-install (`glibc-repo`, `glibc`, `resolv-conf`, `ca-certificates`, and `qemu-user-aarch64` if CPU lacks LSE atomics) with a structured `manualCommand` fallback. `handle_engine_login_mode` MUST flush non-newline prompts, emit `oopbot_auth_prompt` (`manual_code`) alongside `oopbot_auth_url`, forward pasted codes to stdin, record `authenticated: true` in `manifest.json` on exit 0, and inject `termux_subprocess_env` across verification, login, and ACP launches.
- **Unsupported Legacy Node Modes:** `package_install`, `update_extensions`, and unsupported interactive SSH modes MUST return structured `oopbot_result` (`ok: false`, `code: "unsupported"`) rather than crashing or hanging stdout/TCP.
- **File & Output Sink Cleanup:** `handle_rpc_mode` MUST bind `set_output_sink(Some(...))` before starting `RpcServer` and clear `set_output_sink(None)` before emitting `oopbot_exit`.
- **Secret Hygiene:** Never log raw API keys or tokens in JSON error payloads; write `auth.json` with `0o600` via `src/config/store.rs`.
- **Git Inspection Resource Bounds & Concurrency (`git.rs`):** All git subprocesses MUST run via `run_git_bounded` (`process_group(0)` isolation, process-group `SIGKILL` + `kill -0` verification retry on timeout/overflow, bounded reader join on both error [`200ms`] and success [`2s`] paths, and explicit `SpawnFailed` error surfacing): heavy commands (`git add --intent-to-add`, `git diff`, `git log`, `git rev-list`) use a `30s` timeout (`10 MiB` raw diff cap, `4 MiB` log/add cap, `64 KiB` `rev-list` cap, `-c diff.renameLimit=200` on `-M`), and metadata helpers (`--version`, `rev-parse`, `symbolic-ref`, `merge-base`) use `run_git_small` (`10s` timeout, `64 KiB` cap). Diffs are parsed via `parse_git_diff_bounded` (`1 MiB` per-file early hunk drop, `7 MiB` total retained cap, exact CRLF byte accounting). `handle_git_mode` (`run_git_mode_concurrent`) routes requests through 3 separate panic-isolated, queue-bounded (`32` queued/lane with immediate `code: "failed"` backpressure when full) worker lanes — `detect` (`2` workers), `commits` (`2` workers), and `heavy` (`2` workers for `changes`/`commit_diff`) — without blocking the socket reader.
- **Binary Upgrade Acceleration:** `ensure.rs` (`ensure_oopbot_binary_with`) uses `aria2c -x 8 -s 8 -k 1M` when available for remote HTTPS release downloads and falls back cleanly to `reqwest`, skipping `aria2c` for loopback test URLs.

## Work Guidance
- Any change to message `type`, `op`, `code`, or JSON payload fields MUST stay synchronized with `android/app/src/main/kotlin/io/jeph/oopbot/host/HostModels.kt`, `HostProtocol.kt`, and `tests/host_binary_wire.rs`.
- Use `src/config/store.rs` for `auth.json`/`settings.json` persistence and bounded temp-file/atomic JSON helpers (`write_json_atomic`) for other config and backup files.

## Verification
- `cargo test --locked interfaces::host::`
- `cargo test --locked --test host_binary_wire`
- Full crate regression: `cargo clippy --all-targets --locked -- -D warnings && cargo test --locked -- --test-threads=2`

## Child graph Index
- None (leaf domain; Android counterparty is `android/app/src/main/kotlin/io/jeph/oopbot/host/AGENTS.md`).
