#!/usr/bin/env bash
# Installs (or updates) Merpati from its GitHub releases into /Applications.
#
#   curl -fsSL https://raw.githubusercontent.com/ilhamnadhif/merpati/master/scripts/install.sh | bash
#   ... | bash -s -- v0.1.0          # a specific version
#
# The app is downloaded with curl, so macOS does not quarantine it and it opens without the
# "Apple could not verify" prompt. Everything is fetched over HTTPS and checked (the SHA-256
# GitHub publishes, the bundle id, the version and the code signature) before the installed copy
# is touched. The old copy is moved aside and put back if the new one can't be moved in. sudo is
# used only when /Applications or the installed app is not writable.
#
# Environment:
#   MERPATI_REPO      GitHub repository to install from (default ilhamnadhif/merpati)
#   MERPATI_APP_DIR   folder to install Merpati.app into (default /Applications)
set -euo pipefail

REPO="${MERPATI_REPO:-ilhamnadhif/merpati}"
APP_DIR="${MERPATI_APP_DIR:-/Applications}"
APP="$APP_DIR/Merpati.app"
BUNDLE_ID="dev.merpati.app"
EXE="merpati-app"

TMP=""
MNT=""
SUDO=""
STAGE=""
OLD=""
VERSION=""
INSTALLED_TEAM=""

say() { printf '\033[1m==>\033[0m %s\n' "$*"; }
die() { printf 'Error: %s\n' "$*" >&2; exit 1; }
# HTTPS only, redirects included.
fetch() { curl --proto '=https' --proto-redir '=https' --tlsv1.2 --retry 2 --connect-timeout 20 -fL "$@"; }
plist() { plutil -extract "$2" raw -o - "$1/Contents/Info.plist" 2>/dev/null || true; }
team() { codesign -dv "$1" 2>&1 | sed -n 's/^TeamIdentifier=//p' | grep -v '^not set$' || true; }
# plutil reads JSON too.
json() { plutil -extract "$1" raw -o - "$TMP/release.json" 2>/dev/null; }
running() { pgrep -f -- "$APP/Contents/MacOS/$EXE" >/dev/null 2>&1; }

cleanup() {
    # Interrupted between moving the old app aside and moving the new one in: put it back.
    if [ -n "$OLD" ] && [ -e "$OLD" ] && [ ! -e "$APP" ]; then
        $SUDO mv -h "$OLD" "$APP" || true
    fi
    if [ -n "$STAGE" ]; then $SUDO rm -rf "$STAGE" || true; fi
    if [ -n "$MNT" ]; then hdiutil detach -quiet "$MNT" 2>/dev/null || true; fi
    if [ -n "$TMP" ]; then rm -rf "$TMP"; fi
}

# Looks up the release and picks its app archive: the zip the in-app updater uses, else the
# disk image (universal first, then this Mac's architecture). Sets VERSION, NAME, URL and DIGEST.
resolve_release() {
    local tag="$1" api name i arch zip="" arch_zip="" dmg="" arch_dmg="" sums="" asset
    if [ "$tag" = "latest" ]; then
        api="https://api.github.com/repos/$REPO/releases/latest"
    else
        api="https://api.github.com/repos/$REPO/releases/tags/$tag"
    fi
    say "Looking up the release ($tag)"
    fetch -sS -H "Accept: application/vnd.github+json" -o "$TMP/release.json" "$api" ||
        die "release $tag of $REPO not found"
    VERSION="$(json tag_name)" || die "unexpected answer from GitHub"
    VERSION="${VERSION#v}"
    case "$(uname -m)" in arm64) arch=arm64 ;; *) arch=x86_64 ;; esac
    i=0
    while name="$(json "assets.$i.name")"; do
        case "$name" in
            "Merpati-$VERSION-macos-universal.zip") zip=$i ;;
            "Merpati-$VERSION-macos-$arch.zip") arch_zip=$i ;;
            "Merpati-$VERSION-universal.dmg") dmg=$i ;;
            "Merpati-$VERSION-$arch.dmg") arch_dmg=$i ;;
            "SHA256SUMS-macos.txt") sums=$i ;;
        esac
        i=$((i + 1))
    done
    asset="${zip:-${arch_zip:-${dmg:-$arch_dmg}}}"
    [ -n "$asset" ] || die "release $tag of $REPO has no Merpati-$VERSION-macos-universal.zip or .dmg"
    NAME="$(json "assets.$asset.name")"
    URL="$(json "assets.$asset.browser_download_url")"
    DIGEST="$(json "assets.$asset.digest" || true)"
    case "$DIGEST" in sha256:*) DIGEST="${DIGEST#sha256:}" ;; *) DIGEST="" ;; esac
    case "$URL" in
        "https://github.com/$REPO/releases/download/"*) ;;
        *) die "unexpected download address: $URL" ;;
    esac
    # Releases uploaded before GitHub computed digests: use the checksum file of the release.
    if [ -z "$DIGEST" ] && [ -n "$sums" ]; then
        local sums_url
        sums_url="$(json "assets.$sums.browser_download_url")"
        case "$sums_url" in "https://github.com/$REPO/releases/download/"*) ;; *) die "unexpected download address: $sums_url" ;; esac
        fetch -sS -o "$TMP/SHA256SUMS" "$sums_url" || die "could not download the checksums of the release"
        DIGEST="$(awk -v n="$NAME" '$2 == n || $2 == "*" n { print $1; exit }' "$TMP/SHA256SUMS")"
    fi
    [[ "$DIGEST" =~ ^[0-9A-Fa-f]{64}$ ]] ||
        die "the release publishes no SHA-256 for $NAME; not installing an unverified download"
}

# verify_checksum FILE EXPECTED: EXPECTED is a hex SHA-256, optionally prefixed with `sha256:`.
verify_checksum() {
    local expected actual
    expected="$(printf '%s' "${2#sha256:}" | tr 'A-F' 'a-f')"
    actual="$(shasum -a 256 "$1" | awk '{print $1}')"
    [ "$actual" = "$expected" ] || die "the download does not match the SHA-256 GitHub published"
    say "Checksum verified"
}

# unpack FILE: extracts a .zip or mounts a .dmg; sets SRC to the Merpati.app inside.
unpack() {
    case "$1" in
        *.zip)
            mkdir "$TMP/unpacked"
            ditto -x -k "$1" "$TMP/unpacked" || die "the download could not be unpacked"
            SRC="$TMP/unpacked/Merpati.app"
            # Tolerate an archive that wraps the app in one folder.
            [ -d "$SRC" ] || SRC="$(find "$TMP/unpacked" -maxdepth 2 -type d -name Merpati.app | head -n 1)"
            ;;
        *.dmg)
            MNT="$TMP/mnt"
            mkdir "$MNT"
            hdiutil attach -quiet -nobrowse -readonly -noautoopen -mountpoint "$MNT" "$1" ||
                die "the disk image could not be opened"
            SRC="$MNT/Merpati.app"
            ;;
        *) die "unknown archive: $1" ;;
    esac
    if [ ! -d "$SRC" ] || [ -L "$SRC" ]; then die "the download does not contain Merpati.app"; fi
}

# check_app APP: the signature, bundle id and version, and the team of the installed copy.
check_app() {
    $SUDO codesign --verify --deep --strict "$1" 2>/dev/null || die "the downloaded app's signature is not valid"
    [ "$(plist "$1" CFBundleIdentifier)" = "$BUNDLE_ID" ] || die "the download is not Merpati"
    [ "$(plist "$1" CFBundleShortVersionString)" = "$VERSION" ] || die "the downloaded version does not match the release"
    [ "$(plist "$1" CFBundleExecutable)" = "$EXE" ] || die "the download is not Merpati"
    if [ -n "$INSTALLED_TEAM" ] && [ "$(team "$1")" != "$INSTALLED_TEAM" ]; then
        die "the download is signed by someone else than the installed Merpati; remove $APP first to install it anyway"
    fi
}

# install_app SRC: replaces $APP with a checked copy of SRC; the old copy comes back on failure.
install_app() {
    local src="$1" owner leftover
    INSTALLED_TEAM=""
    [ ! -e "$APP" ] || INSTALLED_TEAM="$(team "$APP")"
    check_app "$src"

    if [ ! -w "$APP_DIR" ] || { [ -e "$APP" ] && { [ ! -w "$APP" ] || [ ! -w "$APP/Contents" ]; }; }; then
        SUDO="sudo"
        say "$APP_DIR or the installed Merpati.app is not writable by $(id -un)."
        say "sudo will ask for your password to replace it."
        sudo -v || die "Merpati was not installed: administrator rights are needed"
    fi

    if running; then
        say "Quitting the running Merpati"
        osascript -e "tell application id \"$BUNDLE_ID\" to quit" >/dev/null 2>&1 || true
        for _ in $(seq 1 20); do running || break; sleep 0.5; done
        ! running || die "Merpati is still running; quit it (check for unsaved tabs) and run this again."
    fi

    # The copy that gets installed is made in a new folder next to the app (same volume, so
    # moving it in is a rename) that only this script (or root) can write, and checked again
    # there. As root it gets the owner of the copy it replaces.
    STAGE="$($SUDO mktemp -d "$APP_DIR/.merpati-install.XXXXXXXX")"
    local new="$STAGE/Merpati.app"
    $SUDO ditto --norsrc --noextattr --noacl "$src" "$new"
    if [ -n "$SUDO" ]; then
        owner="root:wheel"
        [ ! -e "$APP" ] || owner="$(stat -f %u:%g "$APP")"
        $SUDO chown -R "$owner" "$new"
    fi
    $SUDO chmod -R u+rwX,go+rX,go-w,a-st "$new"
    check_app "$new"

    if [ -e "$APP" ] || [ -L "$APP" ]; then
        say "Replacing Merpati $(plist "$APP" CFBundleShortVersionString) in $APP_DIR with $VERSION"
        OLD="$(mktemp -u "$APP_DIR/.Merpati.app.old-XXXXXXXX")"
        # -h: never move into a directory a link points to.
        $SUDO mv -h "$APP" "$OLD" || die "the installed app could not be moved aside; nothing was changed"
    else
        say "Installing Merpati $VERSION into $APP_DIR"
    fi
    if ! $SUDO mv -h "$new" "$APP"; then
        if [ -n "$OLD" ]; then
            $SUDO rm -rf "$APP"
            $SUDO mv -h "$OLD" "$APP" && OLD=""
        fi
        die "the new version could not be moved into place; the previous one was kept"
    fi
    OLD=""
    # The old copy, and any an interrupted update left behind (the new app is in place and checked).
    for leftover in "$APP_DIR"/.Merpati.app.old-*; do
        if [ -e "$leftover" ] || [ -L "$leftover" ]; then
            $SUDO rm -rf "$leftover" 2>/dev/null || say "Could not remove $leftover; delete it yourself."
        fi
    done
}

main() {
    local tag="${1:-latest}"
    [ "$(uname -s)" = "Darwin" ] || die "this installer is for macOS; Linux and Windows builds are on the releases page."
    [[ "$REPO" =~ ^[A-Za-z0-9._-]+/[A-Za-z0-9._-]+$ ]] || die "not a GitHub repository: $REPO"
    case "$tag" in
        latest) ;;
        v[0-9]*) ;;
        [0-9]*) tag="v$tag" ;;
        *) die "not a version: $tag" ;;
    esac
    [[ "$tag" =~ ^(latest|v[0-9][0-9A-Za-z.+-]*)$ ]] || die "not a version: $tag"
    [ -d "$APP_DIR" ] || die "$APP_DIR does not exist"

    TMP="$(mktemp -d)"
    trap cleanup EXIT

    resolve_release "$tag"
    say "Downloading $NAME"
    fetch --progress-bar -o "$TMP/$NAME" "$URL"
    verify_checksum "$TMP/$NAME" "$DIGEST"
    unpack "$TMP/$NAME"
    install_app "$SRC"

    say "Done. Merpati $(plist "$APP" CFBundleShortVersionString) is in $APP_DIR."
    open "$APP" || true
}

# Run unless sourced (tests source this file to call the functions above). Piped into bash,
# BASH_SOURCE is empty.
if [ -z "${BASH_SOURCE[0]:-}" ] || [ "${BASH_SOURCE[0]}" = "$0" ]; then
    main "$@"
fi
