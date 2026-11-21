#!/usr/bin/env bash
# Build Mercury-<version>-x86_64.AppImage.
#
#   packaging/build-appimage.sh            # uses cargo-zigbuild (glibc 2.31 floor) when available
#   GLIBC=2.28 packaging/build-appimage.sh # pick a different floor
#   NATIVE=1 packaging/build-appimage.sh   # plain cargo build (links against this machine's glibc)
#   APPIMAGETOOL=/path/to/appimagetool     # otherwise looked up on PATH
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
cd "$ROOT"
VERSION="$(grep -m1 '^version' Cargo.toml | cut -d'"' -f2)"
GLIBC="${GLIBC:-2.31}"
OUT="${OUT:-$ROOT/dist}"

if [[ -z "${NATIVE:-}" ]] && command -v cargo-zigbuild >/dev/null; then
    cargo zigbuild --release --target "x86_64-unknown-linux-gnu.$GLIBC"
    BIN="target/x86_64-unknown-linux-gnu/release/mercury"
else
    cargo build --release
    BIN="target/release/mercury"
fi

APPDIR="$(mktemp -d)/Mercury.AppDir"
mkdir -p "$APPDIR/usr/bin" "$APPDIR/usr/share/applications" "$APPDIR/usr/share/icons/hicolor/256x256/apps"
install -m755 "$BIN" "$APPDIR/usr/bin/mercury"
install -m755 packaging/AppRun "$APPDIR/AppRun"
install -m644 packaging/mercury.desktop "$APPDIR/mercury.desktop"
install -m644 packaging/mercury.desktop "$APPDIR/usr/share/applications/mercury.desktop"
install -m644 packaging/mercury.png "$APPDIR/mercury.png"
install -m644 packaging/mercury.png "$APPDIR/usr/share/icons/hicolor/256x256/apps/mercury.png"
ln -sf mercury.png "$APPDIR/.DirIcon"

TOOL="${APPIMAGETOOL:-$(command -v appimagetool || true)}"
[[ -n "$TOOL" ]] || { echo "appimagetool not found; set APPIMAGETOOL" >&2; exit 1; }
mkdir -p "$OUT"
TARGET="$OUT/Mercury-$VERSION-x86_64.AppImage"
ARCH=x86_64 "$TOOL" --no-appstream "$APPDIR" "$TARGET"
echo "built $TARGET"
