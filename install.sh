#!/usr/bin/env bash
set -e

echo "Installing cava-wallpaper..."

# Directories and repo
INSTALL_DIR="$HOME/.local/bin"
REPO_URL="https://github.com/rs-pro0/wallpaper-cava.git"
BUILD_DIR="$HOME/cava-wallpaper-src"
CONFIG_DIR="$HOME/.config/cava-wallpaper"
SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"

# Clone or update the wallpaper-cava repo
if [ -d "$BUILD_DIR" ]; then
  echo "Updating existing repository at $BUILD_DIR..."
  cd "$BUILD_DIR"
  git pull
else
  echo "Cloning repository..."
  git clone "$REPO_URL" "$BUILD_DIR"
fi

# Copy the Cargo.toml with binary name configuration if not the same file
echo "Updating Cargo configuration..."
if [ "$BUILD_DIR/Cargo.toml" != "$SCRIPT_DIR/Cargo.toml" ]; then
  cp "$SCRIPT_DIR/Cargo.toml" "$BUILD_DIR/"
fi

# Build and install using cargo
cd "$BUILD_DIR"
echo "Building and installing project..."
cargo install --path . --force

# Create config directory and copy config file
mkdir -p "$CONFIG_DIR"
if [ -f "$SCRIPT_DIR/config.toml" ]; then
  cp "$SCRIPT_DIR/config.toml" "$CONFIG_DIR/"
  echo "Config file copied to $CONFIG_DIR/config.toml"
fi

# Ensure ~/.local/bin is in PATH
if ! echo "$PATH" | grep -q "$HOME/.local/bin"; then
  echo 'export PATH="$HOME/.local/bin:$PATH"' >> "$HOME/.bashrc"
  echo "~/.local/bin has been added to your PATH in ~/.bashrc"
  echo "Please run: source ~/.bashrc"
fi

echo ""
echo "Installation complete!"
echo "The binary 'cava-wallpaper' is now installed globally."
echo "You can run it from anywhere with:"
echo "    cava-wallpaper"
echo ""
echo "Config location: $CONFIG_DIR/config.toml"
echo ""
echo "To update later, simply re-run this script."
