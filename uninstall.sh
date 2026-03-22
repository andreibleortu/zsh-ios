#!/usr/bin/env bash
set -euo pipefail

ZSHRC="$HOME/.zshrc"
BIN_PATH="$HOME/.cargo/bin/zsh-ios"

echo "=== zsh-ios uninstaller ==="
echo ""

# 1. Remove binary
if [[ -f "$BIN_PATH" ]]; then
    rm "$BIN_PATH"
    echo "Removed binary: $BIN_PATH"
else
    echo "Binary not found at $BIN_PATH (already removed or installed elsewhere)"
fi

# 2. Find and remove config/data directories
#    The Rust binary uses dirs::config_dir() which is ~/Library/Application Support on macOS.
#    The plugin may be installed at $XDG_CONFIG_HOME/zsh-ios or ~/.config/zsh-ios.
declare -a config_dirs=()

# macOS native config dir
[[ -d "$HOME/Library/Application Support/zsh-ios" ]] && config_dirs+=("$HOME/Library/Application Support/zsh-ios")

# XDG config dir (if different)
xdg_dir="${XDG_CONFIG_HOME:-$HOME/.config}/zsh-ios"
if [[ -d "$xdg_dir" ]] && [[ "$xdg_dir" != "$HOME/Library/Application Support/zsh-ios" ]]; then
    config_dirs+=("$xdg_dir")
fi

for dir in "${config_dirs[@]}"; do
    echo ""
    echo "Found config directory: $dir"
    ls -la "$dir" 2>/dev/null | tail -n +2 | sed 's/^/  /'
    echo ""
    read -r -p "Delete $dir? [y/N] " choice
    if [[ "$choice" == "y" || "$choice" == "Y" ]]; then
        rm -rf "$dir"
        echo "Removed."
    else
        echo "Kept."
    fi
done

if [[ ${#config_dirs[@]} -eq 0 ]]; then
    echo "No config directories found."
fi

# 3. Remove source line from .zshrc
if [[ -f "$ZSHRC" ]]; then
    if grep -qF "zsh-ios" "$ZSHRC"; then
        cp "$ZSHRC" "$ZSHRC.bak-zsh-ios"
        grep -vF "zsh-ios" "$ZSHRC" > "$ZSHRC.tmp" && mv "$ZSHRC.tmp" "$ZSHRC"
        echo ""
        echo "Removed zsh-ios lines from $ZSHRC (backup: $ZSHRC.bak-zsh-ios)"
    else
        echo "No zsh-ios references found in $ZSHRC"
    fi
fi

echo ""
echo "=== Uninstall complete ==="
echo "Restart your shell or run: source $ZSHRC"
