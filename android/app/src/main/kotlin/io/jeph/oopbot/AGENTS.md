# Android Kotlin Application (`android/app/src/main/kotlin/io/jeph/oopbot/`)

## Purpose
Root Kotlin package for the Android OOPBoT application: dependency graph (`AppGraph`), application lifecycle (`OopbotApplication`), activities (`MainActivity`, `LauncherActivity`), and feature packages.

## Ownership & Package Map
Every feature has a strict home—do NOT place code in unrelated packages or dump helpers at the root package:
- **App Entry & Manual DI:** `OopbotApplication.kt`, `AppGraph.kt`, `AppStrings.kt`, `LauncherActivity.kt`, `MainActivity.kt`.
- **Agent Lifecycle & RPC State (`agent/`):** See [agent/AGENTS.md](agent/AGENTS.md).
- **Termux & Remote Host Transport (`host/`):** See [host/AGENTS.md](host/AGENTS.md).
- **On-Device LLM Inference (`localmodel/`):** See [localmodel/AGENTS.md](localmodel/AGENTS.md).
- **Compose Screens, Theme & Components (`ui/`):** See [ui/AGENTS.md](ui/AGENTS.md).
- **Persistent State & Repositories (`data/`):** `Preferences.kt`, `AccountRepository.kt`, `SessionRepository.kt`, `SessionArchive.kt`, `AutoArchive.kt`, `ConfigBackups.kt`, `ModelDefaults.kt`, `NoticeStore.kt`, `PendingReload.kt`, `PiExtensionUpdate.kt`, `RemoteHosts.kt`, `RemotePasswords.kt`, `Workspaces.kt`, `BundledExtension.kt`.
- **Termux Installation & Properties Setup (`termux/`):** `TermuxSetup.kt` (`DEFAULT_OOPBOT_VERSION`), `TermuxInstall.kt`, `TermuxIdentity.kt`.
- **Interactive Shell Sessions (`terminal/`):** `TerminalRegistry.kt`, `TerminalSession.kt`.
- **Multi-Pane & Tab Workspace State (`workspace/`, `files/`, `git/`):** `OpenTabs.kt`, `Pane.kt`, `Panes.kt`, `SavedLayout.kt`, `WorkspaceLayout.kt`, `FileTabs.kt`, `CommitTabs.kt`.
- **Tree-Sitter Highlighting Wrapper (`syntax/`):** `TreeSitter.kt`, `Grammar.kt`, `Captures.kt`.

## Local Contracts
- **Centralized AppGraph DI:** Long-lived stateful registries, repositories, and background monitors MUST be constructed and wired inside `AppGraph` on `OopbotApplication`. Avoid ad-hoc mutable singletons outside `AppGraph`.
- **Main-Thread Scope Discipline:** `AppGraph.scope` uses `SupervisorJob() + Dispatchers.Main.immediate`. Blocking disk/network/JNI work inside `data/`, `host/`, or `syntax/` must switch to `Dispatchers.IO` or `Dispatchers.Default` cleanly.
- **Process Keep-Alive & Lease Invariants:** `AppGraph.holdWhileWorking()` owns the single `KeepAlive` token driven by active chats or terminal commands; `AppGraph.endLocalModelLeases()` releases local model reservations as soon as a chat turn finishes.
- **Data Security (`data/`):** Encrypted/protected credentials (`RemotePasswords.kt`) and user notices (`NoticeStore.kt`) must remain isolated in app-private storage; never log API keys or SSH passwords.
- **Termux Version Lockstep (`termux/`):** `TermuxSetup.DEFAULT_OOPBOT_VERSION` MUST always match root `Cargo.toml` `version`.

## Work Guidance
- Business state and repositories belong in `agent/`, `data/`, `host/`, `localmodel/`, `workspace/`, `files/`, or `git/`—never inside `@Composable` UI files under `ui/`.
- Unit tests mirror the exact package path under `android/app/src/test/kotlin/io/jeph/oopbot/...`.

## Verification
- `cd android && gradle :app:ktlintCheck :app:detektDebug :app:detektDebugUnitTest :app:testDebugUnitTest --no-daemon --stacktrace --continue`

## Child graph Index
- [agent/AGENTS.md](agent/AGENTS.md) — Chat agent processes, `AgentService`, RPC client, timeline models, inbox, and notifications.
- [host/AGENTS.md](host/AGENTS.md) — `HostServer`, `HostClient`, `OopbotBridge` JNI, Termux/SSH transport, and file/git RPC hosts.
- [localmodel/AGENTS.md](localmodel/AGENTS.md) — Local LLM catalog, `:inference` AIDL process, LiteRT-LM & llama.cpp engines, and lease scheduler.
- [ui/AGENTS.md](ui/AGENTS.md) — Jetpack Compose screens, navigation, shared components, theme tokens, and draw/recomposition rules.
