#!/usr/bin/env bash
# CosmicUpdate installer.
#
# Run it any of these ways:
#   curl -fsSL https://raw.githubusercontent.com/davidboulay/CosmicUpdate/main/install.sh | bash
#   ./install.sh                       # from a checkout
#   PREFIX=/usr/local sudo -E ./install.sh   # system-wide
#
# It downloads a prebuilt binary from the latest GitHub release when one is
# available for your architecture, and otherwise builds from source (needs a
# Rust toolchain).
set -euo pipefail

REPO="davidboulay/CosmicUpdate"
APP_ID="com.github.davidboulay.CosmicAppletUpdates"
BIN="cosmic-applet-updates"

PREFIX="${PREFIX:-$HOME/.local}"
BIN_DIR="$PREFIX/bin"
APP_DIR="$PREFIX/share/applications"
mkdir -p "$BIN_DIR" "$APP_DIR"

msg() { printf '\033[1;34m==>\033[0m %s\n' "$*"; }
err() { printf '\033[1;31merror:\033[0m %s\n' "$*" >&2; }

install_prebuilt() {
    # Prebuilt binaries are published for x86_64 only.
    [ "$(uname -m)" = "x86_64" ] || return 1
    command -v curl >/dev/null || return 1

    local tag
    tag=$(curl -fsSL "https://api.github.com/repos/$REPO/releases/latest" 2>/dev/null \
        | grep -oP '"tag_name":\s*"\K[^"]+' || true)
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
    if [ -f "Cargo.toml" ] && grep -q "name = \"$BIN\"" Cargo.toml 2>/dev/null; then
        src="$(pwd)"            # already inside a checkout
    else
        command -v git >/dev/null || { err "git is required to fetch the source."; exit 1; }
        msg "Cloning $REPO…"
        src=$(mktemp -d)
        cleanup="$src"
        git clone --depth 1 "https://github.com/$REPO.git" "$src"
    fi

    msg "Building from source (this takes a few minutes the first time)…"
    cargo build --release --locked --manifest-path "$src/Cargo.toml"
    install -Dm755 "$src/target/release/$BIN" "$BIN_DIR/$BIN"
    install -Dm644 "$src/data/$APP_ID.desktop" "$APP_DIR/$APP_ID.desktop"
    [ -n "$cleanup" ] && rm -rf "$cleanup"
}

msg "Installing CosmicUpdate to $PREFIX"
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

✅ CosmicUpdate installed to $BIN_DIR/$BIN

Add it to the panel:
  Settings → Desktop → Panel (or Dock) → Add Applet → "Updates"

If it doesn't appear right away, restart the panel:
  cosmic-panel --replace &
EOF
