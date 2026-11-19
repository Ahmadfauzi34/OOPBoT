Graph FRAMEWORK CONTRACTS & RAIL (INTELLIGENT REPOSITORY RAIL)

This repository follows the **graph (Documentation-as-Contract)** hierarchical system to enforce global development coherence and absolute quality standards across all folders.

### Core Contracts & System Behavior
- **Durable Hierarchies:** `AGENTS.md` files located in directory subtrees hold binding work contracts, runtime side-effects, and quality specifications for that domain. All edits and code increments within a subtree must trace direct authority from its closest governing `AGENTS.md`.
- **The graph Pass Process:**
  1. **Read-Before-Edit:** Before modifying any files, always recursively trace the exact path from repository root to your target directory, reading every `AGENTS.md` document encountered along the parent chain.
  2. **Reflect-and-Update:** Whenever behavior, data design, or functional dependencies are modified, immediately verify and update the nearest subtree `AGENTS.md` to keep rules fresh and aligned. Remove contradiction or staled operational records dynamically.
- **Unified Section Schema:**
  - **Purpose:** Broad architectural role of this subdirectory tree.
  - **Ownership:** Responsible modules, sub-agents, or core operators.
  - **Local Contracts:** Invariants, security restrictions, performance budgets, or critical rules of structural operation.
  - **Work Guidance:** Idiomatic guidelines, styling, or programming best practices.
  - **Verification:** Specific regression validation test commands and suites.
  - **Child graph Index:** Linking child files/directories that have independent contracts.

### Top-Level Verification & Cross-Cutting Version Contract
- **Version Lockstep:** Any bump to `package.version` in `Cargo.toml` MUST update `Cargo.lock`, `android/app/src/main/kotlin/io/jeph/oopbot/termux/TermuxSetup.kt` (`DEFAULT_OOPBOT_VERSION`), and `src/interfaces/host/tests.rs` together.
- **Verification Suites:**
  - Rust Core/RPC/Host: `cargo clippy --all-targets --locked -- -D warnings && cargo test --locked -- --test-threads=2`
  - Android & Host TS: see `android/AGENTS.md` and `.github/workflows/android-test.yml`.

### Child graph Index
- [src/AGENTS.md](src/AGENTS.md) — Rust core agent harness, multi-provider HTTP clients, C-ABI/JNI FFI, plugins, JSONL RPC daemon, and native Termux TCP host bridge.
- [android/AGENTS.md](android/AGENTS.md) — Android Kotlin/Jetpack Compose app, Gradle build/R8 config, local inference engines, native C/C++, and TypeScript helper daemon.

