# Jetpack Compose UI, Navigation & Design System (`io.jeph.oopbot.ui`)

## Purpose
Contains the entire Jetpack Compose presentation layer, app navigation, theme tokens, shared UI primitives, and feature-specific screens/panes.

## Ownership & Feature Placement Rules
Place every UI file strictly in its matching feature folder:
- **Shell & Root Navigation:** `MainScreen.kt`, `AppNavigator.kt`, `PaneRow.kt`, `WorkspacePane.kt`, `ShellActions.kt`, `StackTransition.kt`, `ChatInboxPill.kt`.
- **Chat & Conversation View (`ui/chat/`):** Message stream, input composer, slash commands, model picker sheet, extension widgets/tabs, tool cards, export sheets.
- **Shared Design Primitives (`ui/components/`):** Cross-feature reusable composables (`Primitives.kt`, `ScrollFades.kt`, `SearchField.kt`, `SelectionCircle.kt`, `SimmeringPot.kt`, `SnackbarStack.kt`, `SwipeAway.kt`, Markdown/code highlighting blocks).
- **Files & Markdown Preview (`ui/files/`):** File explorer, source code viewer (`FileScreen.kt`), Markdown preview.
- **Git Diff & Commit Viewer (`ui/git/`):** Changes pane, commit viewer, diff renderer (`DiffDocument.kt`, `DiffView.kt`), git workspace.
- **Settings & Configuration (`ui/settings/`):** Provider/model editors, remote host forms, backups, local models, subscriptions, theme picker.
- **Workspace Sidebar & Folder Picker (`ui/sidebar/`):** `Sidebar.kt`, `WorkspaceRow.kt`, folder browser/search, machine selector.
- **Other Features:** `ui/archive/`, `ui/auth/`, `ui/onboarding/`, `ui/remote/`, `ui/subagents/`, `ui/terminal/`, `ui/workspace/`.
- **Design Tokens (`ui/theme/`):** `AppTheme.kt`, `OopbotTheme.kt`, `OopbotColors.kt`, `OopbotTokens.kt`, `OopbotFonts.kt`, `LauncherIcon.kt`.

## Local Contracts
- **Strict UI / Business Separation:** Composables under `ui/` MUST NOT manage sockets, file I/O, or RPC transport directly; delegate actions and observe state from `AppGraph` repositories (`agent/`, `host/`, `data/`, `workspace/`).
- **Compose Draw & Allocation Performance:**
  - Prefer `Modifier.drawWithCache` over `drawBehind` / `drawWithContent` whenever creating `Brush`, gradient shaders, `Path`, or `Paint` objects per frame.
  - Reuse `Path` buffers via `Path.rewind()` instead of `reset()` on animated shapes (`SimmeringPot.kt`).
  - Cache `Stroke`, `TextStyle`, `NumberFormat`, and style allocations across recompositions with `remember(...)`; keep UI parse/highlight/split/thumbnail caches (`MarkdownCache`, `HighlightCache`, `SourceLinesCache`, `ThumbnailCache`) bounded by entry and memory/char budgets.
  - Avoid subcomposition (`BoxWithConstraints`) inside scrolling Markdown items (`MarkdownTable.kt` uses `Modifier.layout` + `TableWidthsCache`) and full-pane wrappers (`ChangesPane.kt` uses `Modifier.onSizeChanged`).
  - Pause infinite loading animations inside side panels composed off-screen (`MainScreen.kt` provides `LocalSpinnersAnimate` to `PaneExplorer` alongside `LocalPotsSimmer` on `Sidebar`, and `ChangesPane.kt` suppresses the toolbar spinner during initial full-pane loading).
  - Keep swipe-to-remove row chrome (`SwipeAway.kt`) pre-composed for swipeable rows so drag frames run in layer/draw phases without per-frame or pixel-1 recomposition.
  - Wrap `stackTransition` destinations in `TransitionFrame` (`StackTransition.kt`) and defer non-critical refreshes and heavy stream/asset mounts until `LocalTransitionSettled.current` settles; keep `PackageManager` and `AssetManager` reads off the main thread.
  - Keep `SelectionContainer` opt-in on large virtualized code lists (`FileScreen.kt`) and disabled while an assistant message is actively streaming (`OopbotMarkdown.kt`).
  - Provide stable `key` and `contentType` lambdas on `LazyColumn` / `LazyRow` item lists (`FileScreen.kt`, `CommitsSection.kt`, timelines, diffs, logs); avoid `IntrinsicSize.Min` + per-row `Canvas` in `LazyColumn` rows (`CommitsSection.kt` draws the commit rail in `Modifier.drawWithCache`).
  - Annotate immutable UI state/model classes wrapping lists, sealed trees, or row styling with `@Immutable` (`DiffDocument.kt`, `DiffView.kt` `DiffRowStyle`, `TimelineModels.kt`), and run `DiffView.kt` Tree-Sitter highlighting lazily for visible + ±1 neighbor expanded files via `HighlightCache.shared` with a per-side char cap (`MAX_DIFF_HIGHLIGHT_CHARS`).
- **Offline Cache & Persistent UI State (`ui/chat/`, `ui/subagents/`, `ui/archive/`):**
  - `ChatViews` hydrates and persists per-session drafts, subagent steer drafts, stream scroll position, and expanded folds to SQLite `chat_drafts`; debounce text draft saves and flush on disposal / scroll settle. Read-only `ArchivedChatScreen` and `SubagentScreen` views leave `ChatView.onSave == null`.
  - `SubagentScreen` fetches transcripts with `preferCache = !live` so running subagents poll Termux directly while finished runs use cached transcripts & offline fallback.
  - `ArchiveScreen` combines instant metadata matching with debounced FTS4 `SessionRepository.searchMessages` content queries.
- **Localization & Pseudolocale Compliance:** Zero hardcoded user-facing strings in Kotlin; add all UI copy to `android/app/src/main/res/values/strings_*.xml` matching the domain file (`strings_chat.xml`, `strings_settings.xml`, etc.).
- **Shared Theme & Primitives:** Consume colors, spacing, and typography exclusively from `ui/theme/` (`OopbotTheme`, `OopbotColors`, `OopbotTokens`, `OopbotFonts`) instead of raw hex literals or ad-hoc wrappers.

## Work Guidance
- Promote a widget to `ui/components/` only when reused across 2+ features; keep single-feature UI inside its `ui/<feature>/` folder.
- Mirror pure UI helper/state unit tests under `android/app/src/test/kotlin/io/jeph/oopbot/ui/<feature>/`.

## Verification
- `cd android && gradle :app:ktlintCheck :app:detektDebug :app:testDebugUnitTest --tests "io.jeph.oopbot.ui.*"`
- Optional stability audit: `cd android && gradle -Poopbot.composeReports=true :app:compileReleaseKotlin`

## Child graph Index
- None (leaf domain covering all `ui/*` subpackages).
