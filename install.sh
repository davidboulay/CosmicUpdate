#!/usr/bin/env bash
# Build and install the COSMIC updates applet for the current user.
set -euo pipefail

APP_ID="com.github.davidboulay.CosmicAppletUpdates"
ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"

PREFIX="${PREFIX:-$HOME/.local}"
BIN_DIR="$PREFIX/bin"
APP_DIR="$PREFIX/share/applications"

echo "==> Building (release)…"
cargo build --release --manifest-path "$ROOT/Cargo.toml"

echo "==> Installing binary to $BIN_DIR"
install -Dm755 "$ROOT/target/release/cosmic-applet-updates" "$BIN_DIR/cosmic-applet-updates"

echo "==> Installing desktop entry to $APP_DIR"
install -Dm644 "$ROOT/data/$APP_ID.desktop" "$APP_DIR/$APP_ID.desktop"

echo "==> Done."
echo "Open Settings → Desktop → Panel (or Dock) → Add Applet and pick \"Updates\"."
echo "If it doesn't appear, log out/in or run: cosmic-panel --replace &"
