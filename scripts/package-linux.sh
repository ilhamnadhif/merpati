#!/usr/bin/env bash
# Packages the Linux build for the host architecture (x86_64 or aarch64):
#
#   cargo build --profile dist --locked -p merpati-app -p merpati-cli
#   scripts/package-linux.sh            # into target/dist-pkg (MERPATI_DIST_DIR)
#
#   merpati-<v>-linux-<arch>.tar.gz     any distribution (unpack and run)
#   merpati_<v>_<amd64|arm64>.deb       Debian, Ubuntu, Mint, Pop!_OS, ...
#   merpati-<v>-1.<arch>.rpm            Fedora, RHEL, Rocky, openSUSE, ...
#   Merpati-<v>-<arch>.AppImage         most others (Arch, Manjaro, ...)
#
# Needs dpkg-dev (dpkg-deb, dpkg-shlibdeps), rpm (rpmbuild) and, for the
# AppImage, appimagetool (downloaded into target/ unless APPIMAGETOOL points
# to one; it brings its own mksquashfs). Package dependencies are computed from the
# binaries (dpkg-shlibdeps, rpm's automatic requires), so build on the oldest
# distribution you want to support (CI uses Ubuntu 22.04).
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
cd "$ROOT"
VERSION="$(sed -n 's/^version = "\(.*\)"/\1/p' Cargo.toml | head -1)"
ARCH="$(uname -m)"
case "$ARCH" in
  x86_64) DEB_ARCH=amd64 ;;
  aarch64) DEB_ARCH=arm64 ;;
  *) echo "unsupported architecture: $ARCH" >&2; exit 1 ;;
esac
BIN="$ROOT/target/dist"
OUT="${MERPATI_DIST_DIR:-$ROOT/target/dist-pkg}"
WORK="$ROOT/target/linux-pkg"
REPO="https://github.com/ilhamnadhif/merpati"
SUMMARY="Lightweight native API client"
DESCRIPTION="Merpati is a fast, native API client written in Rust: HTTP, GraphQL, gRPC,
WebSocket, Socket.IO, MQTT, MCP and AI requests, Postman-compatible collections
and scripts, a collection runner, mock server and a newman-compatible CLI
(merpati). No account, no telemetry; workspaces are plain files."

for f in merpati-app merpati; do
  [[ -x "$BIN/$f" ]] || { echo "missing $BIN/$f: build with --profile dist first" >&2; exit 1; }
done
rm -rf "$WORK"
mkdir -p "$WORK" "$OUT"

echo "== file tree"
TREE="$WORK/tree"
install -Dm755 "$BIN/merpati-app" "$TREE/usr/bin/merpati-app"
install -Dm755 "$BIN/merpati" "$TREE/usr/bin/merpati"
install -Dm644 packaging/linux/merpati.desktop "$TREE/usr/share/applications/merpati.desktop"
for icon in packaging/linux/icons/hicolor/*/apps/merpati.png; do
  install -Dm644 "$icon" "$TREE/usr/share/icons/${icon#packaging/linux/icons/}"
done
install -Dm644 README.md "$TREE/usr/share/doc/merpati/README.md"
cat LICENSE-MIT LICENSE-APACHE > "$WORK/copyright"
install -Dm644 "$WORK/copyright" "$TREE/usr/share/doc/merpati/copyright"

echo "== tar.gz"
TAR_DIR="$WORK/merpati-$VERSION"
mkdir -p "$TAR_DIR"
cp "$BIN/merpati-app" "$BIN/merpati" packaging/linux/merpati.desktop README.md LICENSE-MIT LICENSE-APACHE "$TAR_DIR/"
cp packaging/linux/icons/hicolor/256x256/apps/merpati.png "$TAR_DIR/merpati.png"
tar -C "$WORK" -czf "$OUT/merpati-$VERSION-linux-$ARCH.tar.gz" "merpati-$VERSION"

echo "== deb"
DEB="$WORK/deb"
cp -a "$TREE" "$DEB"
mkdir -p "$DEB/DEBIAN"
# dpkg-shlibdeps wants a debian/control next to it.
mkdir -p "$WORK/shlibs/debian"
printf 'Source: merpati\n\nPackage: merpati\nArchitecture: any\n' > "$WORK/shlibs/debian/control"
DEPENDS="$(cd "$WORK/shlibs" && dpkg-shlibdeps -O "$DEB/usr/bin/merpati-app" "$DEB/usr/bin/merpati" 2>/dev/null |
  sed -n 's/^shlibs:Depends=//p')"
{
  echo "Package: merpati"
  echo "Version: $VERSION"
  echo "Architecture: $DEB_ARCH"
  echo "Maintainer: Merpati contributors <ilhamnadhif@users.noreply.github.com>"
  echo "Installed-Size: $(du -sk "$TREE" | cut -f1)"
  echo "Depends: $DEPENDS"
  echo "Recommends: xdg-desktop-portal, gnome-keyring | kwalletmanager"
  echo "Section: devel"
  echo "Priority: optional"
  echo "Homepage: $REPO"
  echo "Description: $SUMMARY"
  echo "$DESCRIPTION" | sed 's/^/ /'
} > "$DEB/DEBIAN/control"
dpkg-deb --root-owner-group -Zxz --build "$DEB" "$OUT/merpati_${VERSION}_${DEB_ARCH}.deb" >/dev/null

echo "== rpm"
RPM="$WORK/rpm"
mkdir -p "$RPM"/{BUILD,RPMS,SOURCES,SPECS,SRPMS}
{
  echo "Name: merpati"
  echo "Version: $VERSION"
  echo "Release: 1"
  echo "Summary: $SUMMARY"
  echo "License: MIT OR Apache-2.0"
  echo "URL: $REPO"
  echo "Recommends: xdg-desktop-portal"
  # Prebuilt, already stripped binaries: no debug package, no rebuilding.
  echo "%define debug_package %{nil}"
  echo "%define __os_install_post %{nil}"
  echo "%define _build_id_links none"
  echo "%description"
  echo "$DESCRIPTION"
  echo "%install"
  echo "mkdir -p %{buildroot}"
  echo "cp -a $TREE/. %{buildroot}/"
  echo "%files"
  echo "/usr/bin/merpati-app"
  echo "/usr/bin/merpati"
  echo "/usr/share/applications/merpati.desktop"
  echo "/usr/share/icons/hicolor/*/apps/merpati.png"
  echo "%doc /usr/share/doc/merpati/README.md"
  echo "%license /usr/share/doc/merpati/copyright"
} > "$RPM/SPECS/merpati.spec"
rpmbuild -bb --quiet --define "_topdir $RPM" --target "$ARCH" "$RPM/SPECS/merpati.spec"
cp "$RPM/RPMS/$ARCH/merpati-$VERSION-1.$ARCH.rpm" "$OUT/"

echo "== AppImage"
APPDIR="$WORK/Merpati.AppDir"
cp -a "$TREE" "$APPDIR"
ln -s usr/bin/merpati-app "$APPDIR/AppRun"
cp packaging/linux/merpati.desktop "$APPDIR/merpati.desktop"
cp packaging/linux/icons/hicolor/256x256/apps/merpati.png "$APPDIR/merpati.png"
TOOL="${APPIMAGETOOL:-$ROOT/target/appimagetool-$ARCH.AppImage}"
if [[ ! -x "$TOOL" ]]; then
  curl --proto '=https' --tlsv1.2 -fsSL -o "$TOOL" \
    "https://github.com/AppImage/appimagetool/releases/download/continuous/appimagetool-$ARCH.AppImage"
  chmod +x "$TOOL"
fi
# Extract-and-run: CI machines have no FUSE.
ARCH="$ARCH" APPIMAGE_EXTRACT_AND_RUN=1 "$TOOL" --no-appstream "$APPDIR" "$OUT/Merpati-$VERSION-$ARCH.AppImage" >/dev/null

echo "== checksums"
(cd "$OUT" && sha256sum "merpati-$VERSION-linux-$ARCH.tar.gz" "merpati_${VERSION}_${DEB_ARCH}.deb" \
  "merpati-$VERSION-1.$ARCH.rpm" "Merpati-$VERSION-$ARCH.AppImage" > "SHA256SUMS-linux-$ARCH.txt")
ls -la "$OUT"
