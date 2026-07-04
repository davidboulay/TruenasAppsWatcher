#!/usr/bin/env bash
# TrueNAS Apps Watcher installer — detects the platform and installs the
# right build:
#   - macOS: the menu bar app (from the release zip)
#   - Linux (COSMIC desktop): the panel applet (prebuilt binary, or built
#     from source when no release matches)
#
#   curl -fsSL https://raw.githubusercontent.com/davidboulay/TruenasAppsWatcher/main/install.sh | bash
set -euo pipefail

REPO="davidboulay/TruenasAppsWatcher"

msg() { printf '\033[1;34m==>\033[0m %s\n' "$*"; }
err() { printf '\033[1;31merror:\033[0m %s\n' "$*" >&2; }

latest_tag() {
    curl -fsSL "https://api.github.com/repos/$REPO/releases/latest" 2>/dev/null \
        | grep -o '"tag_name": *"[^"]*"' | head -1 | sed 's/.*"tag_name": *"\([^"]*\)".*/\1/'
}

install_macos() {
    local ZIP="TrueNAS-Apps-Watcher-macOS.zip"
    local APP="TrueNAS Apps Watcher.app"

    local tag
    tag=$(latest_tag)
    [ -n "$tag" ] || { err "no release found for $REPO"; exit 1; }

    msg "Found release $tag — downloading…"
    local tmp
    tmp=$(mktemp -d)
    trap 'rm -rf "$tmp"' EXIT
    curl -fsSL "https://github.com/$REPO/releases/download/$tag/$ZIP" -o "$tmp/$ZIP"

    msg "Installing to /Applications…"
    ditto -x -k "$tmp/$ZIP" "$tmp"
    rm -rf "/Applications/$APP"
    mv "$tmp/$APP" "/Applications/$APP"
    # The app is ad-hoc signed, not notarized.
    xattr -dr com.apple.quarantine "/Applications/$APP" 2>/dev/null || true

    msg "Launching…"
    open "/Applications/$APP"

    cat <<EOF

✅ TrueNAS Apps Watcher installed.

Look for the blue NAS icon in the menu bar (no Dock icon — it's a menu bar
app). Click it, open Settings (⚙) and enter your TrueNAS address and an API
key (TrueNAS UI → ⚙ top-right → API Keys).
EOF
}

install_linux() {
    local APP_ID="com.github.davidboulay.CosmicAppletTruenasApps"
    local BIN="cosmic-applet-truenas-apps"

    local PREFIX="${PREFIX:-$HOME/.local}"
    local BIN_DIR="$PREFIX/bin"
    local APP_DIR="$PREFIX/share/applications"
    mkdir -p "$BIN_DIR" "$APP_DIR"

    install_prebuilt() {
        # Prebuilt binaries are published for x86_64 only.
        [ "$(uname -m)" = "x86_64" ] || return 1

        local tag
        tag=$(latest_tag)
        [ -n "$tag" ] || return 1

        msg "Found release $tag — downloading prebuilt binary…"
        local base="https://github.com/$REPO/releases/download/$tag"
        local tmp
        tmp=$(mktemp -d)
        trap 'rm -rf "$tmp"' RETURN
        if curl -fsSL "$base/$BIN" -o "$tmp/$BIN" \
            && curl -fsSL "$base/$APP_ID.desktop" -o "$tmp/$APP_ID.desktop"; then
            install -Dm755 "$tmp/$BIN" "$BIN_DIR/$BIN"
            install -Dm644 "$tmp/$APP_ID.desktop" "$APP_DIR/$APP_ID.desktop"
            return 0
        fi
        return 1
    }

    build_from_source() {
        if ! command -v cargo >/dev/null; then
            err "A prebuilt binary isn't available and 'cargo' was not found."
            err "Install Rust from https://rustup.rs and re-run, or use an x86_64 machine."
            exit 1
        fi

        local src cleanup=""
        if [ -f "cosmic/Cargo.toml" ] && grep -q "name = \"$BIN\"" cosmic/Cargo.toml 2>/dev/null; then
            src="$(pwd)"            # already inside a checkout
        else
            command -v git >/dev/null || { err "git is required to fetch the source."; exit 1; }
            msg "Cloning $REPO…"
            src=$(mktemp -d)
            cleanup="$src"
            git clone --depth 1 "https://github.com/$REPO.git" "$src"
        fi

        msg "Building from source (this takes a few minutes the first time)…"
        cargo build --release --locked --manifest-path "$src/cosmic/Cargo.toml"
        install -Dm755 "$src/cosmic/target/release/$BIN" "$BIN_DIR/$BIN"
        install -Dm644 "$src/cosmic/data/$APP_ID.desktop" "$APP_DIR/$APP_ID.desktop"
        # Plain `[ -n ... ] &&` would return 1 here when there is nothing to
        # clean up (a checkout build), and `set -e` would abort the script.
        if [ -n "$cleanup" ]; then rm -rf "$cleanup"; fi
    }

    msg "Installing TrueNAS Apps Watcher to $PREFIX"
    if install_prebuilt; then
        msg "Installed prebuilt binary."
    else
        msg "No prebuilt binary for this system — building from source."
        build_from_source
    fi

    # Refresh the desktop database so the panel sees the new applet promptly.
    command -v update-desktop-database >/dev/null 2>&1 && \
        update-desktop-database "$APP_DIR" >/dev/null 2>&1 || true

    cat <<EOF

✅ TrueNAS Apps Watcher installed to $BIN_DIR/$BIN

Add it to the panel:
  Settings → Desktop → Panel (or Dock) → Add Applet → "TrueNAS Apps Watcher"

Then click the applet, open Settings (⚙) and enter your TrueNAS address
and an API key (TrueNAS UI → Settings (top-right) → API Keys).

If it doesn't appear right away, restart the panel:
  cosmic-panel --replace &
EOF
}

case "$(uname -s)" in
    Darwin) install_macos ;;
    Linux)  install_linux ;;
    *)      err "unsupported platform: $(uname -s)"; exit 1 ;;
esac
