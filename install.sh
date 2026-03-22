#!/usr/bin/env bash
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
CONFIG_DIR="${XDG_CONFIG_HOME:-$HOME/.config}/zsh-ios"

echo "=== zsh-ios installer ==="
echo ""

# 1. Check for Rust toolchain
if ! command -v cargo &>/dev/null; then
    echo "Rust toolchain not found. Installing via rustup..."
    curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y
    source "$HOME/.cargo/env"
    echo ""
fi

echo "Using: $(rustc --version)"
echo ""

# 2. Build and install the binary
echo "Building zsh-ios (release mode)..."
cd "$SCRIPT_DIR"
cargo install --path . --force
echo ""

# Verify installation
if ! command -v zsh-ios &>/dev/null; then
    echo "Warning: zsh-ios not found in PATH after install."
    echo "Make sure ~/.cargo/bin is in your PATH."
    echo ""
fi

# 3. Set up config directory and plugin
echo "Setting up config directory: $CONFIG_DIR"
mkdir -p "$CONFIG_DIR"
cp "$SCRIPT_DIR/plugin/zsh-ios.zsh" "$CONFIG_DIR/zsh-ios.zsh"
echo ""

# 4. Add to .zshrc if not already present
ZSHRC="$HOME/.zshrc"
SOURCE_LINE="source \"$CONFIG_DIR/zsh-ios.zsh\""

if grep -qF "zsh-ios.zsh" "$ZSHRC" 2>/dev/null; then
    echo "zsh-ios already sourced in $ZSHRC"
else
    echo "" >> "$ZSHRC"
    echo "# zsh-ios: Cisco IOS-style command abbreviation" >> "$ZSHRC"
    echo "$SOURCE_LINE" >> "$ZSHRC"
    echo "Added source line to $ZSHRC"
fi
echo ""

# 5. Run initial build
echo "Building initial command tree..."
zsh -c "source $ZSHRC 2>/dev/null; alias" | zsh-ios build --aliases-stdin
echo ""

echo "=== Installation complete! ==="
echo ""
echo "Usage:"
echo "  - Type abbreviated commands and press Enter to resolve+execute"
echo "  - Press Tab to expand abbreviations without executing"
echo "  - Type ? after a space to see available completions"
echo "  - zsh-ios toggle        enable/disable"
echo "  - zsh-ios rebuild       rebuild the command tree"
echo "  - zsh-ios status        show config and tree stats"
echo "  - zsh-ios pin/unpin     manage abbreviation pins"
echo "  - zsh-ios pins          list all pins"
echo ""
echo "Uninstall: ./uninstall.sh"
echo ""
echo "Restart your shell or run: source $ZSHRC"
