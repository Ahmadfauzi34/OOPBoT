# OOPBoT — Hybrid Rust Agent Harness (Termux CLI + `liboopbot.so`)

Fondasi *agent harness* berbasis Gemini API dalam Rust yang mengadopsi pola arsitektur **Pipkin** (multi-tab sesi, pemilih model dinamis per-tab, lipatan *step stream* `Thinking` + `ToolCall`, serta antarmuka hybrid **CLI Binary + Android Shared Library `.so`**).

Setiap rilis di GitHub Actions otomatis menghasilkan **2 artefak `aarch64-linux-android`**:
1. **`oopbot-aarch64-linux-android`** — Binary CLI mandiri untuk dijalankan langsung di Termux.
2. **`liboopbot-aarch64-linux-android.so`** — Shared library (`cdylib`) C-ABI siap dipasang ke proyek APK Android (`jniLibs/arm64-v8a/liboopbot.so`).

---

## Pasang di Termux

### Jika Repo Public:
```sh
pkg install -y curl
curl -fsSL https://raw.githubusercontent.com/Ahmadfauzi34/OOPBoT/main/install.sh | sh
```

### Jika Repo Private (via `gh` CLI):
```sh
pkg install -y gh
gh auth login
gh api repos/Ahmadfauzi34/OOPBoT/contents/install.sh -H "Accept: application/vnd.github.raw" | sh
```

Lalu isi API key:
```sh
nano ~/.config/oopbot/.env     # GEMINI_API_KEY=xxxx
oopbot
```

---

## Pemakaian & Perintah Interaktif (Ala Pipkin)

```
oopbot [-m MODEL] [-p "PROMPT"] [-y] [--max-steps N]
```

Di dalam sesi chat interaktif, tersedia perintah manajemen **Model**, **Tab Sesi**, dan **Step Timeline**:

| Perintah | Fungsi |
|---|---|
| `/models` | Tampilkan katalog model & model aktif di tab saat ini |
| `/model <nomor\|id>` | Ganti model untuk tab aktif (misal `/model 2` atau `/model gemini-2.5-pro`) |
| `/tabs` | Lihat daftar semua tab sesi percakapan beserta model & jumlah langkahnya |
| `/new [model]` | Buat tab sesi baru (opsional langsung menentukan model) |
| `/tab <nomor\|id>` | Pindah antar-tab sesi |
| `/close <nomor\|id>` | Tutup tab sesi |
| `/steps` | Lihat lipatan proses kerja agen (`StepsFold`: `Thinking` + `ToolCall` beserta durasi ms) |
| `tools` | Lihat daftar tool aktif |
| `reset` | Bersihkan memori tab saat ini |
| `keluar` / `exit` | Keluar dari program |

### Tool Bawaan

| Tool | Konfirmasi | Fungsi |
|---|---|---|
| `run_shell` | ✅ | `sh -c <command>` dengan timeout, kembalikan exit code/stdout/stderr |
| `write_file` | ✅ | Buat/timpa file (direktori induk dibuat otomatis) |
| `read_file` | – | Baca file teks |
| `list_dir` | – | Daftar isi direktori |

---

## Arsitektur Hybrid OOP & Modul (`src/`)

| File | Peran |
|---|---|
| [`src/lib.rs`](src/lib.rs) | Entry point library (`rlib` + `cdylib` untuk `liboopbot.so`) |
| [`src/main.rs`](src/main.rs) | Entry point binary CLI Termux (`oopbot`) |
| [`src/agent.rs`](src/agent.rs) | Trait `Agent`, `GeminiAgent`, trait `Approver` |
| [`src/events.rs`](src/events.rs) | `AgentEvent`, trait `EventSink`, `TimelineEntry::StepsFold` (lipatan step ala `StepStream.kt` Pipkin) |
| [`src/models.rs`](src/models.rs) | `ModelCatalog` & `ModelInfo` (manajemen daftar model & pemilihan model per tab) |
| [`src/session.rs`](src/session.rs) | `WorkspaceTabs` & `ChatSession` (manajemen multi-tab + persistensi JSON di `~/.local/share/oopbot/tabs.json`) |
| [`src/tools.rs`](src/tools.rs) | Trait `Tool`, `ToolRegistry`, dan implementasi tool |
| [`src/gemini.rs`](src/gemini.rs) | Klien HTTP `generateContent` + function calling |
| [`src/ffi.rs`](src/ffi.rs) | Ekspor simbol C-ABI (`oopbot_engine_new`, `oopbot_engine_request`, `oopbot_string_free`, `oopbot_engine_free`) di `liboopbot.so` |

### Kontrak FFI `liboopbot.so` (Untuk Integrasi UI Android)
UI Android dapat memanggil `oopbot_engine_request(handle, json)` dengan operasi:
- `{"op": "models"}` / `{"op": "set_model", "model": "gemini-2.5-flash"}`
- `{"op": "tabs"}` / `{"op": "new_tab", "model": "..."}` / `{"op": "switch_tab", "tab": "1"}` / `{"op": "close_tab", "tab": "2"}`
- `{"op": "timeline"}` → mengembalikan struktur `TimelineEntry` (`User`, `StepsFold`, `Assistant`)
- `{"op": "chat", "prompt": "..."}` → menjalankan loop agen dan mengembalikan balasan + timeline terbaru
