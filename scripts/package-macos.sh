#!/usr/bin/env bash
# Builds Merpati.app, a DMG, the app archive the in-app updater installs
# (Merpati-<version>-macos-<arch>.zip), a CLI tarball and a Homebrew cask
# (macOS).
#
#   scripts/package-macos.sh              # universal (arm64 + x86_64), ad-hoc signed
#   scripts/package-macos.sh --native     # host architecture only (faster)
#
# Signing and notarization (optional, needs an Apple Developer account):
#   MERPATI_SIGN_IDENTITY="Developer ID Application: Name (TEAMID)"
#   MERPATI_NOTARY_PROFILE=profile   # created once with `xcrun notarytool store-credentials`
# Without MERPATI_SIGN_IDENTITY the app is signed ad hoc: it runs on this Mac,
# but Gatekeeper on other Macs asks the user to allow it.
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
OUT="${MERPATI_DIST_DIR:-$ROOT/target/dist-pkg}"
VERSION="$(sed -n 's/^version = "\(.*\)"/\1/p' "$ROOT/Cargo.toml" | head -1)"
BUILD="$(date +%Y%m%d%H%M)"
NATIVE=0
[[ "${1:-}" == "--native" ]] && NATIVE=1

cd "$ROOT"
mkdir -p "$OUT"
if [[ $NATIVE == 1 ]]; then
  echo "== building (dist profile, host architecture)"
  cargo build --profile dist -p merpati-app -p merpati-cli
  APP_BIN="$ROOT/target/dist/merpati-app"
  CLI_BIN="$ROOT/target/dist/merpati"
  ARCH="$(uname -m)"
else
  echo "== building (dist profile, arm64 + x86_64)"
  for t in aarch64-apple-darwin x86_64-apple-darwin; do
    cargo build --profile dist --target "$t" -p merpati-app -p merpati-cli
  done
  APP_BIN="$OUT/merpati-app"
  CLI_BIN="$OUT/merpati"
  lipo -create -output "$APP_BIN" target/aarch64-apple-darwin/dist/merpati-app target/x86_64-apple-darwin/dist/merpati-app
  lipo -create -output "$CLI_BIN" target/aarch64-apple-darwin/dist/merpati target/x86_64-apple-darwin/dist/merpati
  ARCH=universal
fi

echo "== Merpati.app"
APP="$OUT/Merpati.app"
rm -rf "$OUT/Merpati.app"
mkdir -p "$APP/Contents/MacOS" "$APP/Contents/Resources"
cp "$APP_BIN" "$APP/Contents/MacOS/merpati-app"
sed -e "s/__VERSION__/$VERSION/" -e "s/__BUILD__/$BUILD/" packaging/macos/Info.plist > "$APP/Contents/Info.plist"
ICONSET="$OUT/Merpati.iconset"
rm -rf "$OUT/Merpati.iconset"
mkdir -p "$ICONSET"
for s in 16 32 128 256 512; do
  sips -z $s $s packaging/macos/icon-1024.png --out "$ICONSET/icon_${s}x${s}.png" >/dev/null
  sips -z $((s * 2)) $((s * 2)) packaging/macos/icon-1024.png --out "$ICONSET/icon_${s}x${s}@2x.png" >/dev/null
done
iconutil -c icns "$ICONSET" -o "$APP/Contents/Resources/Merpati.icns"
rm -rf "$OUT/Merpati.iconset"

echo "== signing"
if [[ -n "${MERPATI_SIGN_IDENTITY:-}" ]]; then
  SIGN=(--sign "$MERPATI_SIGN_IDENTITY" --options runtime --timestamp --entitlements packaging/macos/entitlements.plist)
else
  SIGN=(--sign -)
fi
codesign --force "${SIGN[@]}" "$CLI_BIN"
codesign --force "${SIGN[@]}" "$APP"
codesign --verify --strict --verbose=1 "$APP"

echo "== DMG"
DMG="$OUT/Merpati-$VERSION-$ARCH.dmg"
STAGE="$OUT/dmg-stage"
rm -rf "$OUT/dmg-stage"
mkdir -p "$STAGE"
cp -R "$APP" "$STAGE/"
ln -s /Applications "$STAGE/Applications"
rm -f "$DMG"
hdiutil create -volname "Merpati $VERSION" -srcfolder "$STAGE" -ov -format UDZO "$DMG" >/dev/null
rm -rf "$OUT/dmg-stage"
[[ -n "${MERPATI_SIGN_IDENTITY:-}" ]] && codesign --force --sign "$MERPATI_SIGN_IDENTITY" --timestamp "$DMG"

if [[ -n "${MERPATI_NOTARY_PROFILE:-}" ]]; then
  echo "== notarizing"
  xcrun notarytool submit "$DMG" --keychain-profile "$MERPATI_NOTARY_PROFILE" --wait
  xcrun stapler staple "$DMG"
fi

echo "== app archive (in-app updates)"
# The signed app, zipped the way macOS expects: "Install and restart" in the
# app downloads it, checks it and puts it in place of the running copy.
ZIP="$OUT/Merpati-$VERSION-macos-$ARCH.zip"
rm -f "$ZIP"
ditto -c -k --sequesterRsrc --keepParent "$APP" "$ZIP"

echo "== CLI tarball"
CLI_TGZ="$OUT/merpati-cli-$VERSION-macos-$ARCH.tar.gz"
tar -C "$(dirname "$CLI_BIN")" -czf "$CLI_TGZ" "$(basename "$CLI_BIN")"

echo "== Homebrew cask"
SHA="$(shasum -a 256 "$DMG" | cut -d' ' -f1)"
sed -e "s/__VERSION__/$VERSION/g" -e "s/__SHA256__/$SHA/" -e "s/__ARCH__/$ARCH/g" \
  packaging/homebrew/merpati.rb.in > "$OUT/merpati.rb"

printf "\napp  %s (%.1f MB binary)\ndmg  %s (%.1f MB)\nzip  %s (%.1f MB)\ncli  %s\ncask %s\n" \
  "$APP" "$(echo "$(stat -f%z "$APP/Contents/MacOS/merpati-app") / 1048576" | bc -l)" \
  "$DMG" "$(echo "$(stat -f%z "$DMG") / 1048576" | bc -l)" \
  "$ZIP" "$(echo "$(stat -f%z "$ZIP") / 1048576" | bc -l)" "$CLI_TGZ" "$OUT/merpati.rb"
