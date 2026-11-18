#!/usr/bin/env bash
# Signs an unsigned release APK with OOPBoT's release key and checks that the result carries the release certificate.
# .github/workflows/release.yml runs it in a job that holds the signing secrets and nothing else.
#
# Usage: sign-release-apk.sh <unsigned.apk> <signed.apk>
#
# Environment:
#   RELEASE_KEYSTORE_BASE64    the PKCS12 keystore, base64-encoded
#   RELEASE_KEYSTORE_PASSWORD  the keystore's password, which PKCS12 also uses for the key
#   RELEASE_KEY_ALIAS          the key's alias in the keystore
#   RELEASE_CERT_SHA256        the SHA-256 digest of the release certificate, in hex; colons are optional
#   BUILD_TOOLS                the Android build-tools folder holding zipalign and apksigner
set -euo pipefail

verify_only=false
if [ $# -eq 1 ] && [ "$1" = "--verify-only" ]; then
  verify_only=true
elif [ $# -ne 2 ]; then
  echo "Usage: $0 <unsigned.apk> <signed.apk> | --verify-only" >&2
  exit 2
else
  unsigned=$1
  signed=$2
fi

: "${RELEASE_KEYSTORE_BASE64:?is not set}"
: "${RELEASE_KEYSTORE_PASSWORD:?is not set}"
: "${RELEASE_KEY_ALIAS:?is not set}"
: "${RELEASE_CERT_SHA256:?is not set}"

work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT
keystore="$work/release.p12"
(
  umask 077
  printf '%s' "$RELEASE_KEYSTORE_BASE64" | tr -d '[:space:]' | base64 --decode >"$keystore"
)
key_alias=$(printf '%s' "$RELEASE_KEY_ALIAS" | tr -d '[:space:]')

trimmed_pw=$(printf '%s' "$RELEASE_KEYSTORE_PASSWORD" | tr -d '\r\n' | sed 's/^[[:space:]]*//;s/[[:space:]]*$//')
lower_first_pw="${trimmed_pw,}"
upper_first_pw="${trimmed_pw^}"

resolved_pw=""
for candidate in "$RELEASE_KEYSTORE_PASSWORD" "$trimmed_pw" "$lower_first_pw" "$upper_first_pw"; do
  [ -z "$candidate" ] && continue
  if CANDIDATE_PW="$candidate" keytool -list -keystore "$keystore" -storetype PKCS12 -alias "$key_alias" -storepass:env CANDIDATE_PW >/dev/null 2>&1; then
    resolved_pw="$candidate"
    break
  fi
done

if [ -z "$resolved_pw" ]; then
  echo "Failed to unlock PKCS12 keystore with RELEASE_KEYSTORE_PASSWORD (also tried trimming whitespace and toggling first-letter case)." >&2
  exit 2
fi
export RELEASE_KEYSTORE_PASSWORD="$resolved_pw"

if [ "$verify_only" = true ]; then
  echo "Keystore and password verified for alias '$key_alias'."
  exit 0
fi

: "${BUILD_TOOLS:?is not set}"

# 16 KB page alignment keeps the APK installable on devices with 16 KB memory pages.
"$BUILD_TOOLS/zipalign" -P 16 -f 4 "$unsigned" "$work/aligned.apk"
# The password reaches apksigner through the environment, never the command line.
"$BUILD_TOOLS/apksigner" sign \
  --ks "$keystore" \
  --ks-type PKCS12 \
  --ks-pass env:RELEASE_KEYSTORE_PASSWORD \
  --ks-key-alias "$key_alias" \
  --v4-signing-enabled false \
  --out "$signed" \
  "$work/aligned.apk"

certificates=$("$BUILD_TOOLS/apksigner" verify --print-certs "$signed")
# apksigner prints one digest per signature scheme; they must all name the one release certificate.
actual=$(sed -n 's/^.*certificate SHA-256 digest: //p' <<<"$certificates" | sort -u)
expected=$(printf '%s' "$RELEASE_CERT_SHA256" | sed 's/^[[:space:]]*SHA256:[[:space:]]*//I' | tr -d ':[:space:]' | tr 'A-F' 'a-f')
if [ "$actual" != "$expected" ]; then
  echo "$signed is signed by $actual, not the release certificate $expected" >&2
  exit 1
fi
