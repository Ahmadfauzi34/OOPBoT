# OOPBoT Android Workspace (`android/`)

## Purpose
Gradle multi-component Android workspace building the OOPBoT Jetpack Compose app (`:app`), bundling the TypeScript Termux helper assets (`android/host`), compiling native C/C++ libraries (`oopbot-syntax`, `oopbot-llama`, `oopbot-opencl-probe`), and packaging the Rust `liboopbot.so` JNI core (`arm64-v8a`).

## Ownership
- **Gradle Configuration & Quality Rules:** `android/settings.gradle.kts`, `android/gradle.properties`, `android/config/detekt.yml`, `android/.editorconfig`.
- **Application Module Build:** `android/app/build.gradle.kts`, `android/app/proguard-rules.pro`, `android/app/litertlm-rules.pro`.
- **Build Variants:** `android/app/src/debug/` (`.debug` suffix, `OOPBoT Dev`) vs `android/app/src/release/` (`TermuxBuild` isolation).
- **Release & Test Scripts:** `android/scripts/` (`emulator-llm.sh`, `sign-release-apk.sh`, `sign-play-bundle.sh`, `upload-play-bundle.sh`, `ssh-test-host/`).

## Local Contracts
- **Toolchain & SDK Pins:** Java 21 toolchain (`VERSION_21`), `compileSdk = 37`, `targetSdk = 37`, `minSdk = 29`, `ndkVersion = "30.0.16248370"`, `cmake = 4.1.2`, ABI filter `arm64-v8a`.
- **Debug vs Release Coexistence:** `debug` builds MUST use `applicationIdSuffix = ".debug"`, `isPseudoLocalesEnabled = true`, and separate `TermuxBuild` folder/port constants (`src/debug/.../TermuxBuild.kt` vs `src/release/.../TermuxBuild.kt`) so OOPBoT Dev never collides with a release install inside Termux.
- **R8 Full Mode & Reflection Keep Rules:** Release builds (`.github/workflows/build-termux.yml` -> `:app:assembleRelease` + `android/scripts/sign-release-apk.sh`) run `isMinifyEnabled = true`, `isShrinkResources = true`, and `android.enableR8.fullMode=true`. `proguard-rules.pro` strips `Log.v`/`Log.d` calls in release (`-assumenosideeffects`) and keeps C/C++ JNI bridges (`TreeSitter`, `OpenClProbe`, `LlamaServer`), while `litertlm-rules.pro` keeps `com.google.ai.edge.litertlm.**` and `OopbotBridge`.
- **Detekt Completeness Gate:** `detektDebug` MUST compile cleanly with zero `"compiler errors found during analysis"` (classpath explicitly includes `compileDebugKotlin` and `compileDebugJavaWithJavac` for AIDL types).
- **Static & Unit Gate Parity:** CI (`.github/workflows/android-test.yml`) requires `ktlintCheck`, `detektDebug`, `detektDebugUnitTest`, `testDebugUnitTest`, `:app:compileDebugKotlin` with `-Poopbot.composeReports=true` (verified by `android/scripts/check-compose-reports.py`), and `pnpm run check` in `android/host` to pass.

## Work Guidance
- Use `gradle -Poopbot.composeReports=true :app:compileReleaseKotlin` locally when diagnosing Compose stability or skippability regressions.
- Never hardcode user-facing UI strings in Kotlin; add localized string resources under `android/app/src/main/res/values/strings_*.xml` so debug pseudolocales catch leaks.
- Keep `pnpm-lock.yaml` and pinned SHA-256 source archives reproducible; never remove SHA-256 verification in `PinnedSources`.

## Verification
- `cd android && gradle :app:ktlintCheck :app:detektDebug :app:detektDebugUnitTest :app:testDebugUnitTest --no-daemon --stacktrace --continue`
- Compose stability check: `cd android && gradle -Poopbot.composeReports=true :app:compileDebugKotlin --no-daemon --stacktrace && python3 scripts/check-compose-reports.py app/build/compose_compiler`
- Host TS gate: `cd android/host && pnpm run check`

## Child graph Index
- [android/host/AGENTS.md](host/AGENTS.md) — Node.js/TypeScript Termux helper daemon & framing protocol bundled into APK assets.
- [android/app/src/main/cpp/AGENTS.md](app/src/main/cpp/AGENTS.md) — Native C/C++ CMake build (`oopbot-syntax`, `oopbot-llama`, `oopbot-opencl-probe`) & pinned archive licenses.
- [android/app/src/main/kotlin/io/jeph/oopbot/AGENTS.md](app/src/main/kotlin/io/jeph/oopbot/AGENTS.md) — Android Kotlin app packages (`AppGraph`, `agent`, `host`, `localmodel`, `ui`, etc.).
