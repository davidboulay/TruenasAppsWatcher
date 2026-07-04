#!/usr/bin/env bash
# Build the styled installer disk image from an already-assembled .app
# (scripts/make_app.sh first). Requires dmgbuild:  pip3 install dmgbuild
# Run from the macos/ directory.
set -euo pipefail

APP="dist/TrueNAS Apps Watcher.app"
OUT="dist/TrueNAS-Apps-Watcher.dmg"
[ -d "$APP" ] || { echo "error: $APP not found; run scripts/make_app.sh first" >&2; exit 1; }

DMGBUILD=$(command -v dmgbuild || echo "$HOME/Library/Python/3.9/bin/dmgbuild")
command -v "$DMGBUILD" >/dev/null || DMGBUILD="python3 -m dmgbuild"

# Multi-resolution background so the window is crisp on retina displays.
tiffutil -cathidpicheck assets/dmg-background.png assets/dmg-background@2x.png \
    -out assets/dmg-background.tiff 2>/dev/null

rm -f "$OUT"
$DMGBUILD -s scripts/dmg-settings.py "TrueNAS Apps Watcher" "$OUT"
echo "Built $OUT"
