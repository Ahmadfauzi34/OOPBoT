#!/data/data/com.termux/files/usr/bin/sh
# Installer OOPBoT untuk Termux: unduh binary hasil build GitHub Actions
# dari GitHub Releases, verifikasi SHA-256, pasang ke $PREFIX/bin/oopbot.
#
# Pemakaian:
#   curl -fsSL https://raw.githubusercontent.com/Ahmadfauzi34/OOPBoT/main/install.sh | sh
#   # versi tertentu:
#   curl -fsSL https://raw.githubusercontent.com/Ahmadfauzi34/OOPBoT/main/install.sh | VERSION=v0.1.0 sh
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
if [ "$VERSION" = "latest" ]; then
  BASE="https://github.com/$REPO/releases/latest/download"
else
  BASE="https://github.com/$REPO/releases/download/$VERSION"
fi

command -v curl >/dev/null 2>&1 || pkg install -y curl

TMP="$(mktemp -d)"
trap 'rm -rf "$TMP"' EXIT

echo "⬇️  Mengunduh $ASSET ($VERSION) dari $REPO ..."
curl -fL --progress-bar -o "$TMP/$ASSET" "$BASE/$ASSET"
curl -fsSL -o "$TMP/$ASSET.sha256" "$BASE/$ASSET.sha256"

echo "🔐 Verifikasi SHA-256 ..."
( cd "$TMP" && sha256sum -c "$ASSET.sha256" )

install -m 755 "$TMP/$ASSET" "$PREFIX/bin/$BIN_NAME"

CONF_DIR="$HOME/.config/oopbot"
mkdir -p "$CONF_DIR"
if [ ! -f "$CONF_DIR/.env" ]; then
  printf 'GEMINI_API_KEY=\n# GEMINI_MODEL=gemini-3.5-flash-lite\n' > "$CONF_DIR/.env"
  chmod 600 "$CONF_DIR/.env"
  echo "📝 Isi API key di: $CONF_DIR/.env"
fi

echo "✅ Terpasang: $("$PREFIX/bin/$BIN_NAME" --version)"
echo "   Jalankan: $BIN_NAME"
