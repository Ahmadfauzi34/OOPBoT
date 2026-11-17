# Android Host Bridge & Transport (`io.jeph.oopbot.host`)

## Purpose
Manages communication between the Android app and Termux / remote SSH hosts: local TCP socket server (`HostServer`), connection/version manager (`HostClient`), Termux wake/launcher intents (`HostLauncher`, `TermuxWaker`), helper daemon pairing (`HelperBridge`, `HelperPairing`, `HelperProtocol`), RPC file/git clients (`FileHost`, `GitHost`), and the in-process Rust JNI binding (`OopbotBridge`).

## Ownership
- **Socket Server & Client Coordination:** `HostServer.kt`, `HostClient.kt`, `HostChannel.kt`, `HostProtocol.kt`, `HostModels.kt`, `HostFailure.kt`, `HostRejected.kt`, `SharedHost.kt`, `LineReader.kt`.
- **Termux Launch & Variant Isolation:** `HostLauncher.kt`, `HostResultReceiver.kt`, `TermuxWakeActivity.kt`, `TermuxWaker.kt`, `PlaySetup.kt`, and variant-specific `TermuxBuild.kt` (`src/debug/.../TermuxBuild.kt` & `src/release/.../TermuxBuild.kt`).
- **Helper Daemon Bridge:** `HelperBridge.kt`, `HelperPairing.kt`, `HelperProtocol.kt`.
- **Domain RPC Clients & In-Process JNI:** `FileHost.kt`, `FolderBrowser.kt`, `GitHost.kt`, `ModelConfig.kt`, `RemoteModels.kt`, `OopbotBridge.kt`.

## Local Contracts
- **Strict Build Isolation (`TermuxBuild`):** Debug (`OOPBoT Dev`) and Release builds MUST keep disjoint `TermuxBuild` names, folder paths, and helper ports (`src/debug/` vs `src/release/`) so both apps can share one Termux install safely.
- **Socket Token & Handshake Integrity:** Every incoming TCP connection to `HostServer` MUST authenticate via the one-time `OOPBOT_TOKEN` handshake (`oopbot_hello`) before dispatching `oopbot_request` / `oopbot_utility_request`. Never bind `HostServer` beyond loopback.
- **Client Version Enforcement:** `HostClient` version checks (`HostClientVersionGuardTest.kt`) MUST match `src/host/mod.rs` (`check_rpc_expected_version`) so outdated Termux binaries fail fast with an actionable upgrade signal (`fixedInTermux`).
- **JNI Bridge Contract (`OopbotBridge.kt` <-> `src/ffi.rs`):** Keep `package io.jeph.oopbot.host`, `object OopbotBridge`, and all `@JvmStatic external fun` names/signatures locked to `Java_io_jeph_oopbot_host_OopbotBridge_*` exports in `src/ffi.rs`, and guard `System.loadLibrary("oopbot")` failures via `isAvailable`.
- **Wire Parity:** `HostModels.kt` & `HostProtocol.kt` MUST stay aligned with Rust `src/host/` and TS `android/host/src/helper/` (`HostModelsWireContractTest.kt`).

## Work Guidance
- Never invoke raw Termux shell commands from UI screens; route file, git, browse, and utility operations through `FileHost`, `GitHost`, `FolderBrowser`, or `HostClient`.
- Update `HostModelsWireContractTest.kt` / `HelperProtocolTest.kt` whenever adding wire fields or modes.

## Verification
- `cd android && gradle :app:testDebugUnitTest --tests "io.jeph.oopbot.host.*"`
- Rust wire counterpart: `cargo test --locked --test host_binary_wire`

## Child graph Index
- None (leaf domain; paired with `src/host/AGENTS.md`, `src/AGENTS.md` [`ffi.rs`], and `android/host/AGENTS.md`).
