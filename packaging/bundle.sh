#!/bin/zsh
# Build ViewMusic.app — release build + hand-rolled bundle assembly + codesign.
# (Hand-rolled assembly keeps the build dependency-free: this script IS the reliable
#  path and needs no extra tooling beyond cargo and the macOS code-signing tools.)
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
APP_NAME="ViewMusic"
BUNDLE_ID="io.github.acrive82.viewmusic"
OUT_DIR="$ROOT/target/release/bundle"
APP="$OUT_DIR/$APP_NAME.app"

echo "==> cargo build --release"
cargo build --release --manifest-path "$ROOT/Cargo.toml" -p viz-app

echo "==> assembling $APP"
rm -rf "$APP"
mkdir -p "$APP/Contents/MacOS" "$APP/Contents/Resources"
cp "$ROOT/target/release/viewmusic" "$APP/Contents/MacOS/viewmusic"
cp "$ROOT/packaging/Info.plist" "$APP/Contents/Info.plist"

echo "==> codesign"
# Prefer a stable Apple Development identity so the TCC audio grant survives rebuilds.
# Fall back to ad-hoc with a warning.
IDENTITY="$(security find-identity -v -p codesigning 2>/dev/null | awk -F'"' '/Apple Development/ {print $2; exit}')"
if [[ -n "${IDENTITY:-}" ]]; then
  codesign --force --options runtime --identifier "$BUNDLE_ID" --sign "$IDENTITY" "$APP"
  echo "    signed with: $IDENTITY"
else
  codesign --force --identifier "$BUNDLE_ID" --sign - "$APP"
  echo "    WARNING: ad-hoc signature — the System Audio Recording grant will NOT persist"
  echo "    across rebuilds. Create a free Apple Development certificate (see README.md)."
fi

echo "==> done: $APP"
echo "    launch with: open \"$APP\""
