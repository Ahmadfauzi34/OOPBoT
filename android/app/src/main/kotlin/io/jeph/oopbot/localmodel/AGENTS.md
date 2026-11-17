# On-Device Local Model Runtime (`io.jeph.oopbot.localmodel`)

## Purpose
Runs local on-device LLMs via an isolated `:inference` AIDL child process (`InferenceProcess`), exposes a local OpenAI-compatible HTTP endpoint (`LocalModelServer`), schedules single-model memory leases (`LocalModelScheduler`), and manages downloadable catalogs and runtime engines (`LiteRT-LM` and `llama.cpp`).

## Ownership
- **Orchestration & Scheduling:** `LocalModels.kt`, `LocalModelRuntime.kt`, `LocalModelScheduler.kt`, `LocalModelServer.kt`, `LocalModelTypes.kt`, `InferenceProcess.kt`, `StreamProgress.kt`, and `aidl/io/jeph/oopbot/localmodel/*`.
- **Catalog & Downloads (`catalog/`):** `LocalModelCatalog`, `InstalledModels`, `ModelDownloader`, `EngineChoice`.
- **Inference Engines (`engine/`):**
  - `engine/litert/` (`.litertlm` runner in `:inference` process, `ChatChunks`, `ChatRequest`, `LiteRtMessages`, `TurnPlan`).
  - `engine/llama/` (GGUF server via native `oopbot-llama` JNI + `oopbot-opencl-probe`, `ServerArguments`).
- **Loopback HTTP Serving (`http/`):** OpenAI-compatible local inference API server.

## Local Contracts
- **Single-Loaded-Model & Turn Lease Invariant:** Only ONE heavy local model may be resident in RAM at a time (`ModelHost`). `LocalModelScheduler` serializes conflicting model loads with time-bound session leases (`leaseMillis`) released early via `endLease(session)` when `AgentProcess.busy` drops to `false` (wired in `AppGraph.endLocalModelLeases()`), preventing OOMs and subagent-to-parent deadlocks.
- **`:inference` Process & R8 Protection:** LiteRT-LM runs inside a separate `:inference` process communicating over AIDL; `android/app/litertlm-rules.pro` MUST NOT be weakened because native LiteRT code invokes Kotlin callbacks by class/method name under R8 full mode.
- **Loopback-Only Serving:** `LocalModelServer` (`http/`) MUST bind strictly to localhost/loopback (`127.0.0.1`) so local models are never exposed to external network interfaces.
- **Storage & Download Safety:** `ModelDownloader` must stream large weight downloads with cancellation support (`StreamProgress`) and validate destinations inside app-managed model storage without blocking the main thread.

## Work Guidance
- Engine-specific prompt formatting belongs inside `engine/litert/` or `engine/llama/`, never in `ui/` or `agent/`.
- Use `kotlinx-coroutines-test` virtual time when testing `LocalModelScheduler` lease/idle-unload timing (`LocalModelSchedulerTest.kt`).

## Verification
- `cd android && gradle :app:testDebugUnitTest --tests "io.jeph.oopbot.localmodel.*"`
- Release R8 sanity: verify `litertlm-rules.pro` + CMake targets (`oopbot-llama`, `oopbot-opencl-probe`) remain intact on any engine refactor.

## Child graph Index
- None (leaf domain; native backend lives in `android/app/src/main/cpp/AGENTS.md`).
