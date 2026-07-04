#!/usr/bin/env bash
# Assemble "TrueNAS Apps Watcher.app" from a SwiftPM release build.
# Usage: scripts/make_app.sh [path-to-built-binary]
set -euo pipefail

BIN="${1:-.build/apple/Products/Release/TruenasAppsWatcher}"
[ -f "$BIN" ] || BIN=".build/release/TruenasAppsWatcher"
[ -f "$BIN" ] || { echo "error: built binary not found; run 'swift build -c release' first" >&2; exit 1; }

APP="dist/TrueNAS Apps Watcher.app"
rm -rf "$APP"
mkdir -p "$APP/Contents/MacOS"
cp Info.plist "$APP/Contents/Info.plist"
cp "$BIN" "$APP/Contents/MacOS/TruenasAppsWatcher"

# Ad-hoc signature: not notarized, but keeps Gatekeeper's messaging sane and
# lets the binary run after the quarantine flag is cleared.
codesign --force -s - "$APP"

echo "Built $APP"
