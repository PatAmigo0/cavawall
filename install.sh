#!/usr/bin/env bash
set -e

INSTALL_DIR="$HOME/.local/bin"
REPO_URL="https://github.com/rs-pro0/wallpaper-cava.git"
BUILD_DIR="$HOME/cava-wallpaper-src"
CONFIG_DIR="$HOME/.config/cava-wallpape"
SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"

# Setup repo
if [ -d "$BUILD_DIR" ]; then
  echo "Updating repo..."
  cd "$BUILD_DIR" && git pull
else
  echo "Cloning repo..."
  git clone "$REPO_URL" "$BUILD_DIR"
fi

# Build
echo "Installing..."
[ "$BUILD_DIR/Cargo.toml" != "$SCRIPT_DIR/Cargo.toml" ] && cp "$SCRIPT_DIR/Cargo.toml" "$BUILD_DIR/"
cd "$BUILD_DIR" && cargo install --path . --force

# Config
mkdir -p "$CONFIG_DIR"
[ -f "$SCRIPT_DIR/config.toml" ] && cp "$SCRIPT_DIR/config.toml" "$CONFIG_DIR/"

# Finalize
if [[ ":$PATH:" != *":$HOME/.local/bin:"* ]]; then
  echo -e "\nAdd to PATH:\n  echo 'export PATH=\"\$HOME/.local/bin:\$PATH\"' >> ~/.bashrc && source ~/.bashrc"
fi

echo -e "\nDone! Run with: cava-wallpaper"
