#!/usr/bin/env bash
set -euo pipefail

INSTALL_DIR="$HOME/.local"
CONFIG_DIR="$HOME/.config/cavawall"
SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"

# Build
echo "Installing..."
if [ -f "$SCRIPT_DIR/Cargo.toml" ] && [ "$SCRIPT_DIR" != "$SCRIPT_DIR" ]; then
  cp "$SCRIPT_DIR/Cargo.toml" "$SCRIPT_DIR/"
fi

cd "$SCRIPT_DIR"
# .cargo/config.toml already asks for target-cpu=native; this makes it explicit
# and lets a packager opt out with CAVAWALL_PORTABLE=1.
if [ -n "${CAVAWALL_PORTABLE:-}" ]; then
  RUSTFLAGS="${RUSTFLAGS:-}" cargo install --path . --force --root "$INSTALL_DIR"
else
  RUSTFLAGS="${RUSTFLAGS:--C target-cpu=native}" cargo install --path . --force --root "$INSTALL_DIR"
fi

# Config
mkdir -p "$CONFIG_DIR"
if [ -f "$SCRIPT_DIR/config.toml" ]; then
  cp "$SCRIPT_DIR/config.toml" "$CONFIG_DIR/"
fi

# Finalize
if [[ ":$PATH:" != *":$HOME/.local/bin:"* ]]; then
  echo -e "\nAdd to PATH:\n  echo 'export PATH=\"\$HOME/.local/bin:\$PATH\"' >> ~/.bashrc && source ~/.bashrc"
fi

echo -e "\nDone! Run with: cavawall"
