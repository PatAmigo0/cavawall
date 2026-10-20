#!/usr/bin/env bash
# Build and install cavawall to ~/.local/bin and its helpers to
# ~/.local/lib/cavawall, off PATH,
# and give a first run a config to start from
#
# Runs from a clone, or piped from curl: then it clones the source to
# ~/.cache/cavawall/src first, and a later run pulls it there and rebuilds
set -euo pipefail

REPO="https://github.com/PatAmigo0/cavawall"
INSTALL_DIR="$HOME/.local"
CONFIG_DIR="${XDG_CONFIG_HOME:-$HOME/.config}/cavawall"
SRC_DIR="${XDG_CACHE_HOME:-$HOME/.cache}/cavawall/src"

# The whole script is a function so bash has read all of it before anything
# runs: piped from curl, a cut-off download would otherwise run half a script
main() {
  for tool in cargo git; do
    if ! command -v "$tool" >/dev/null; then
      echo "cavawall needs $tool to build; install it first" >&2
      exit 1
    fi
  done

  # Piped, $0 is the shell and dirname gives the current directory, which
  # is only the source when it holds cavawall's Cargo.toml
  local dir
  dir="$(cd "$(dirname "$0")" && pwd)"
  if grep -qs '^name = "cavawall"' "$dir/Cargo.toml"; then
    cd "$dir"
  else
    if [ -d "$SRC_DIR/.git" ]; then
      echo "Updating the source in $SRC_DIR..."
      git -C "$SRC_DIR" pull --ff-only --quiet </dev/null
    else
      echo "Fetching the source to $SRC_DIR..."
      mkdir -p "$(dirname "$SRC_DIR")"
      git clone --depth 1 --quiet "$REPO" "$SRC_DIR" </dev/null
    fi
    cd "$SRC_DIR"
  fi

  echo "Installing..."
  # .cargo/config.toml already asks for target-cpu=native; CAVAWALL_PORTABLE=1
  # builds for the baseline instead, for a binary copied elsewhere
  if [ -n "${CAVAWALL_PORTABLE:-}" ]; then
    RUSTFLAGS="${RUSTFLAGS:--C target-cpu=x86-64}" cargo install --path . --force --root "$INSTALL_DIR" </dev/null
  else
    cargo install --path . --force --root "$INSTALL_DIR" </dev/null
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
  echo "Start it with the session: cavawall service install (or your compositor's autostart)"
}

main "$@"
