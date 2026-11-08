#!/data/data/com.termux/files/usr/bin/sh
# Installer OOPBoT untuk Termux: unduh binary hasil build GitHub Actions
# dari GitHub Releases, verifikasi SHA-256, pasang ke $PREFIX/bin/oopbot.
#
# Pemakaian (repo public):
#   curl -fsSL https://raw.githubusercontent.com/Ahmadfauzi34/OOPBoT/main/install.sh | sh
#   # versi tertentu:
#   curl -fsSL https://raw.githubusercontent.com/Ahmadfauzi34/OOPBoT/main/install.sh | VERSION=v0.1.0 sh
#
# Pemakaian (repo private, butuh gh login):
#   gh api repos/Ahmadfauzi34/OOPBoT/contents/install.sh -H "Accept: application/vnd.github.raw" | sh
set -eu

REPO="${REPO:-Ahmadfauzi34/OOPBoT}"
VERSION="${VERSION:-latest}"
BIN_NAME="oopbot"
PREFIX="${PREFIX:-/data/data/com.termux/files/usr}"

case "$(uname -m)" in
  aarch64|arm64) TARGET="aarch64-linux-android" ;;
  *) echo "❌ Arsitektur $(uname -m) belum didukung (hanya aarch64)." >&2; exit 1 ;;
esac

ASSET="$BIN_NAME-$TARGET"

TMP="$(mktemp -d)"
trap 'rm -rf "$TMP"' EXIT

download_public() {
  command -v curl >/dev/null 2>&1 || pkg install -y curl
  if [ "$VERSION" = "latest" ]; then
    BASE="https://github.com/$REPO/releases/latest/download"
  else
    BASE="https://github.com/$REPO/releases/download/$VERSION"
  fi
  curl -fL --progress-bar -o "$TMP/$ASSET" "$BASE/$ASSET" &&
    curl -fsSL -o "$TMP/$ASSET.sha256" "$BASE/$ASSET.sha256"
}

download_gh() {
  command -v gh >/dev/null 2>&1 || return 1
  if [ "$VERSION" = "latest" ]; then
    gh release download -R "$REPO" -p "$ASSET" -p "$ASSET.sha256" -D "$TMP"
  else
    gh release download "$VERSION" -R "$REPO" -p "$ASSET" -p "$ASSET.sha256" -D "$TMP"
  fi
}

echo "⬇️  Mengunduh $ASSET ($VERSION) dari $REPO ..."
if ! download_public 2>/dev/null; then
  echo "ℹ️  Unduhan publik gagal (repo mungkin private), mencoba via 'gh' CLI ..."
  if ! download_gh; then
    echo "❌ Gagal mengunduh rilis. Jika repo private, jalankan: pkg install -y gh && gh auth login" >&2
    exit 1
  fi
fi

echo "🔐 Verifikasi SHA-256 ..."
( cd "$TMP" && sha256sum -c "$ASSET.sha256" )

install -m 755 "$TMP/$ASSET" "$PREFIX/bin/$BIN_NAME"

CONF_DIR="$HOME/.config/oopbot"
mkdir -p "$CONF_DIR"
if [ ! -f "$CONF_DIR/.env" ]; then
  cat <<'EOF' > "$CONF_DIR/.env"
# ==============================================================================
# OOPBoT Provider Configuration (Pilih salah satu provider berikut)
# ==============================================================================

# 1. OpenRouter (Akses ratusan model: DeepSeek V3/R1, Claude 3.5, Llama, dsb)
# OPENROUTER_API_KEY=sk-or-v1-...
# OOPBOT_MODEL=deepseek/deepseek-chat

# 2. DeepSeek API Langsung
# DEEPSEEK_API_KEY=sk-...
# OOPBOT_MODEL=deepseek-chat

# 3. Google Gemini API
# GEMINI_API_KEY=...
# GEMINI_MODEL=gemini-2.5-flash

# 4. OpenAI API
# OPENAI_API_KEY=sk-...
# OOPBOT_MODEL=gpt-4o-mini

# 5. Anthropic Claude API
# ANTHROPIC_API_KEY=sk-ant-...
# OOPBOT_MODEL=claude-3-5-haiku-20241022

# 6. Provider Kustom / Server Lokal (Ollama, vLLM, LMStudio, Groq, Together)
# OOPBOT_API_KEY=...
# OOPBOT_BASE_URL=http://localhost:11434/v1
# OOPBOT_MODEL=llama3.2
EOF
  chmod 600 "$CONF_DIR/.env"
  echo "📝 Template konfigurasi multi-provider dibuat di: $CONF_DIR/.env"
fi

echo "✅ Terpasang: $("$PREFIX/bin/$BIN_NAME" --version)"
echo "   Jalankan: $BIN_NAME"

