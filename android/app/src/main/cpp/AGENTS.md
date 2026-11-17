# Native C/C++ JNI Libraries (`android/app/src/main/cpp/`)

## Purpose
CMake NDK build producing three native `arm64-v8a` shared libraries bundled in the Android APK:
1. `oopbot-syntax`: Tree-sitter parser + 25 language grammars for fast in-app syntax highlighting.
2. `oopbot-llama`: `llama.cpp` server/runtime for local GGUF model execution (with KleidiAI CPU kernels & OpenCL support).
3. `oopbot-opencl-probe`: Lightweight OpenCL device/GPU capability probe.

## Ownership
- **Root CMake & Tree-Sitter JNI:** `android/app/src/main/cpp/CMakeLists.txt`, `android/app/src/main/cpp/syntax.c`.
- **Llama.cpp Engine Wrapper:** `android/app/src/main/cpp/llama/`.
- **OpenCL Probe:** `android/app/src/main/cpp/opencl/`.
- **Gradle Source Pins & Licenses:** `syntaxSources` and `llamaSources` in `android/app/build.gradle.kts`; third-party license assets in `android/app/src/main/assets/licenses/`.

## Local Contracts
- **Strict SHA-256 Source Pinning:** All external archives (`tree-sitter`, language grammars, `llama.cpp`, `kleidiai`, `opencl-headers`) MUST be fetched via `PinnedSources` tasks (`syntaxSources` / `llamaSources`) with verified SHA-256 digests. Never fetch unpinned network dependencies inside CMakeLists directly.
- **License Notice Lockstep:** Updating any archive version or pin in `build.gradle.kts` MUST update the corresponding license file under `android/app/src/main/assets/licenses/` (e.g., `llama-cpp.txt`, `kleidiai.txt`, `opencl-headers.txt`, and vendored dependency notices).
- **Hidden Symbol Visibility:** `oopbot-syntax` uses `CMAKE_C_VISIBILITY_PRESET hidden` and `-Wall -Wextra -Werror -Wno-unused-parameter` on first-party `syntax.c`; only explicit `JNIEXPORT` symbols matching `io.jeph.oopbot.syntax.TreeSitter` (and llama/opencl Kotlin bindings) may be exported.
- **Streaming Parse Performance:** Third-party parser/grammar C files compile at `-O2` even in debug builds so live streaming markdown/code rendering does not lag on device.

## Work Guidance
- When adding a Tree-sitter grammar: add its pin to `syntaxSourcePins`, add its highlight queries in `syntaxSources.queries` (ordered cleanly so later patterns override earlier ones where needed), add its parser directory to `GRAMMAR_DIRS` in `CMakeLists.txt`, register its JNI entrypoint in `syntax.c`, and wire it in `io.jeph.oopbot.syntax.Grammar`.

## Verification
- Compiled automatically during Android native builds (`configureCMakeDebug` / `buildCMakeDebug` / `assembleDebug` in Gradle with NDK `30.0.16248370` and CMake `4.1.2`).
- Kotlin JNI contract tests: `gradle :app:testDebugUnitTest --tests "io.jeph.oopbot.syntax.*"` and `io.jeph.oopbot.localmodel.engine.llama.*`.

## Child graph Index
- None (leaf domain; Kotlin wrappers reside in `io.jeph.oopbot.syntax` and `io.jeph.oopbot.localmodel.engine.llama`).
