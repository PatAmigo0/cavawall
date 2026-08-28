#!/usr/bin/env bash
set -euo pipefail

INSTALL_DIR="$HOME/.local"
CONFIG_DIR="$HOME/.config/cavalier"
SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"

# Build
echo "Installing..."
if [ -f "$SCRIPT_DIR/Cargo.toml" ] && [ "$SCRIPT_DIR" != "$SCRIPT_DIR" ]; then
  cp "$SCRIPT_DIR/Cargo.toml" "$SCRIPT_DIR/"
fi

cd "$SCRIPT_DIR"
cargo install --path . --force --root "$INSTALL_DIR"

# Config
mkdir -p "$CONFIG_DIR"
if [ -f "$SCRIPT_DIR/config.toml" ]; then
  cp "$SCRIPT_DIR/config.toml" "$CONFIG_DIR/"
fi

# Finalize
if [[ ":$PATH:" != *":$HOME/.local/bin:"* ]]; then
  echo -e "\nAdd to PATH:\n  echo 'export PATH=\"\$HOME/.local/bin:\$PATH\"' >> ~/.bashrc && source ~/.bashrc"
fi

echo -e "\nDone! Run with: cavalier"
