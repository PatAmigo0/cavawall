#!/usr/bin/env bash
# Build and install cavawall to ~/.local/bin and its helpers to
# ~/.local/lib/cavawall, off PATH,
# and give a first run a config to start from
set -euo pipefail

INSTALL_DIR="$HOME/.local"
CONFIG_DIR="${XDG_CONFIG_HOME:-$HOME/.config}/cavawall"
SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
cd "$SCRIPT_DIR"

echo "Installing..."
# .cargo/config.toml already asks for target-cpu=native; CAVAWALL_PORTABLE=1
# builds for the baseline instead, for a binary copied elsewhere
if [ -n "${CAVAWALL_PORTABLE:-}" ]; then
  RUSTFLAGS="${RUSTFLAGS:--C target-cpu=x86-64}" cargo install --path . --force --root "$INSTALL_DIR"
else
  cargo install --path . --force --root "$INSTALL_DIR"
fi
# `cavawall <command>` finds them here, so only cavawall completes on PATH
mkdir -p "$INSTALL_DIR/lib/cavawall"
for b in cavawallctl cavawall-tune; do
  mv -f "$INSTALL_DIR/bin/$b" "$INSTALL_DIR/lib/cavawall/$b"
done

# Only when there is none: an existing config is someone's settings, and may
# be a symlink into a dotfiles repo that cp would write straight through
if [ ! -e "$CONFIG_DIR/config.toml" ]; then
  mkdir -p "$CONFIG_DIR"
  cp config.toml "$CONFIG_DIR/"
  echo "Wrote $CONFIG_DIR/config.toml"
else
  echo "Kept the existing $CONFIG_DIR/config.toml"
fi

if [[ ":$PATH:" != *":$HOME/.local/bin:"* ]]; then
  echo -e "\nAdd to PATH:\n  echo 'export PATH=\"\$HOME/.local/bin:\$PATH\"' >> ~/.bashrc && source ~/.bashrc"
fi

echo -e "\nDone! Run with: cavawall"
