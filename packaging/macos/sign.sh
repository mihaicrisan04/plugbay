#!/usr/bin/env bash
# Signs a macOS release binary with the Developer ID certificate and notarizes it.
# A stable signature lets macOS keep "Always Allow" keychain access across upgrades.
#
# Usage: sign.sh <binary>
# Needs the Actions secrets listed in docs/releasing.md; without them it warns and leaves the binary ad-hoc signed.
set -euo pipefail

binary=$1
# Part of the designated requirement the keychain remembers; never change it.
identifier=io.github.mihaicrisan04.plugbay

required=(MACOS_CERTIFICATE_P12 MACOS_CERTIFICATE_PASSWORD APPLE_API_KEY_P8 APPLE_API_KEY_ID APPLE_API_ISSUER_ID)
for name in "${required[@]}"; do
  if [ -z "${!name:-}" ]; then
    echo "::warning::$name is not set, so $binary stays ad-hoc signed; see docs/releasing.md"
    exit 0
  fi
done

work=$(mktemp -d)
keychain="$work/signing.keychain-db"
trap 'security delete-keychain "$keychain" 2>/dev/null || true; rm -rf "$work"' EXIT

keychain_password=$(openssl rand -hex 24)
security create-keychain -p "$keychain_password" "$keychain"
security set-keychain-settings -lut 3600 "$keychain"
security unlock-keychain -p "$keychain_password" "$keychain"
printf '%s' "$MACOS_CERTIFICATE_P12" | base64 --decode > "$work/certificate.p12"
security import "$work/certificate.p12" -k "$keychain" -P "$MACOS_CERTIFICATE_PASSWORD" -T /usr/bin/codesign
security set-key-partition-list -S apple-tool:,apple: -s -k "$keychain_password" "$keychain" > /dev/null

identity=$(security find-identity -v -p codesigning "$keychain" | awk '/Developer ID Application/ { print $2; exit }')
if [ -z "$identity" ]; then
  echo "::error::MACOS_CERTIFICATE_P12 holds no Developer ID Application identity"
  exit 1
fi

codesign --force --options runtime --timestamp --identifier "$identifier" \
  --keychain "$keychain" --sign "$identity" "$binary"
codesign --verify --strict --verbose=2 "$binary"
codesign --display --verbose=2 "$binary" 2>&1 | grep -E '^(Identifier|Authority|TeamIdentifier)='

printf '%s' "$APPLE_API_KEY_P8" > "$work/AuthKey.p8"
ditto -c -k "$binary" "$work/plugbay.zip"
submission=$(xcrun notarytool submit "$work/plugbay.zip" --wait --timeout 30m --output-format json \
  --key "$work/AuthKey.p8" --key-id "$APPLE_API_KEY_ID" --issuer "$APPLE_API_ISSUER_ID")
status=$(jq -r .status <<< "$submission")
echo "notarization: $status"
if [ "$status" != "Accepted" ]; then
  xcrun notarytool log "$(jq -r .id <<< "$submission")" \
    --key "$work/AuthKey.p8" --key-id "$APPLE_API_KEY_ID" --issuer "$APPLE_API_ISSUER_ID" || true
  exit 1
fi
