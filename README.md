# OOPBoT — Agent Harness Rust untuk Termux

Eksperimen *agent harness* berbasis Gemini API dengan **tool calling**, ditulis
dalam Rust. Binary di-*cross-compile* oleh GitHub Actions untuk
`aarch64-linux-android`; di sisi Termux kamu **hanya mengunduh binary jadi**
(tidak perlu memasang Rust/cargo di HP).

```
┌──────────── GitHub ─────────────┐        ┌──────── Termux (HP) ────────┐
│ push tag v* ─► Actions (NDK)    │        │ install.sh                  │
│   cargo build --target          │ ─────► │  ├ unduh oopbot-aarch64...  │
│   aarch64-linux-android         │Release │  ├ cek sha256               │
│   └► Release: oopbot-* + .sha256│        │  └ $PREFIX/bin/oopbot       │
└─────────────────────────────────┘        └─────────────────────────────┘
```

## Pasang di Termux

```sh
pkg install -y curl
curl -fsSL https://raw.githubusercontent.com/Ahmadfauzi34/OOPBoT/main/install.sh | sh
```

Versi tertentu: `... | VERSION=v0.1.0 sh`. Untuk update, jalankan ulang perintah yang sama.

Lalu isi API key:

```sh
nano ~/.config/oopbot/.env     # GEMINI_API_KEY=xxxx
oopbot
```

## Pemakaian

```
oopbot [-m MODEL] [-y] [--max-steps N]
```

| Opsi | Keterangan |
|---|---|
| `-m, --model` | Model Gemini (default `$GEMINI_MODEL` atau `gemini-3.5-flash-lite`) |
| `-y, --yes` | Setujui semua tool otomatis (**hati-hati**) |
| `--max-steps N` | Batas iterasi tool per pesan (default 25) |

Di dalam chat: `keluar`, `reset` (hapus memori), `tools`.

### Tool yang tersedia

| Tool | Konfirmasi | Fungsi |
|---|---|---|
| `run_shell` | ✅ | `sh -c <command>` dengan timeout, kembalikan exit code/stdout/stderr |
| `write_file` | ✅ | Buat/timpa file (direktori induk dibuat otomatis) |
| `read_file` | – | Baca file teks |
| `list_dir` | – | Daftar isi direktori |

Saat diminta konfirmasi: `y` = izinkan sekali, `a` = izinkan semua sampai keluar, lainnya = tolak.

## Arsitektur kode

| File | Isi |
|---|---|
| [`src/main.rs`](src/main.rs) | CLI, parsing argumen, REPL |
| [`src/agent.rs`](src/agent.rs) | Trait `Agent`, `GeminiAgent` (loop model ⇄ tool), trait `Approver` |
| [`src/tools.rs`](src/tools.rs) | Trait `Tool`, `ToolRegistry`, implementasi tool |
| [`src/gemini.rs`](src/gemini.rs) | Klien `generateContent` + function calling |

Menambah tool baru: buat struct yang mengimplementasikan `Tool`, lalu
`registry.register(Box::new(ToolBaru))` di `ToolRegistry::with_defaults()`.

## Rilis binary baru

```sh
git tag v0.1.0
git push origin v0.1.0
```

Workflow [`build-termux.yml`](.github/workflows/build-termux.yml) akan menjalankan
test, build `aarch64-linux-android` (API 24) memakai Android NDK bawaan runner,
lalu mengunggah `oopbot-aarch64-linux-android` + `.sha256` ke GitHub Release.
Setiap push/PR juga menghasilkan *artifact* build untuk dicoba.

## Pengembangan lokal (opsional, di PC)

```sh
cp .env.example .env   # isi GEMINI_API_KEY
cargo run
cargo test
```
