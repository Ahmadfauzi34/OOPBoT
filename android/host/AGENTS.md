# Termux Helper Daemon & Framing (`android/host/`)

## Purpose
TypeScript workspace (`oopbot-host`) bundled via `esbuild` (`gradle :app:hostBuild`) into Android APK assets and deployed to Termux as the helper daemon, SSH/file transport, and JSONL framing layer alongside the native Rust `oopbot` binary.

## Ownership
- **Framing & Serialization:** `android/host/src/framing.ts` (`LineSplitter`, `encodeLine`, `MAX_LINE`).
- **Helper Daemon & Pairing:** `android/host/src/helper/` (`main.ts`, `daemon.ts`, `channel.ts`, `client.ts`, `files.ts`, `pairing.ts`, `protocol.ts`, `oopbot-setup.sh`, `run.sh`, `start.sh`).
- **Build & Tooling:** `android/host/package.json`, `pnpm-lock.yaml`, `tsconfig.json`, `vitest.config.ts`, `.oxlintrc.json`.
- **Test Suite:** `android/host/test/` (`framing.test.ts`, `helper-daemon.test.ts`, `helper-protocol.test.ts`, `global-setup.ts`).

## Local Contracts
- **Strict LF Framing:** `LineSplitter` MUST treat LF (`0x0A`) as the sole record delimiter (stripping trailing CR) and NEVER split on Unicode `U+2028` / `U+2029` (which are valid unescaped characters inside JSON strings).
- **Wire & Pairing Parity:** Helper messages, auth pairing tokens, and channel framing in `src/helper/protocol.ts` and `pairing.ts` MUST stay bit-for-bit aligned with Kotlin `io.jeph.oopbot.host.HelperProtocol` and `HelperPairing` (tested in `HelperProtocolTest.kt` / `HelperPairingTest.kt`).
- **Bundle Determinism & Clean Output:** `hostBuild` deletes prior generated asset output before bundling (`HostBundle` task in `app/build.gradle.kts`) and installs with `pnpm install --frozen-lockfile`. Zero oxlint warnings are allowed (`--deny-warnings`).
- **Runtime Boundary:** Keep `android/host/src/` self-contained with zero unbundled runtime npm dependencies in Termux; everything shipped to APK assets must bundle via `build.ts`.

## Work Guidance
- Place shared framing helpers in `src/framing.ts`, protocol schemas/validators in `src/helper/protocol.ts`, and daemon socket lifecycle logic in `src/helper/daemon.ts` / `channel.ts`.
- Add corresponding Vitest regression tests under `android/host/test/` for any framing or daemon state-machine edit.

## Verification
- `cd android/host && pnpm run check` (runs `tsc --noEmit && oxlint --deny-warnings && vitest run`)

## Child graph Index
- None (leaf domain; Kotlin counterparty is `android/app/src/main/kotlin/io/jeph/oopbot/host/AGENTS.md`).
