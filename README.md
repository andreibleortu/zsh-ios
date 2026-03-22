# zsh-ios

Command abbreviation engine for Zsh inspired by Cisco IOS. Type abbreviated commands and let the shell figure out the rest.

```
$ ter ap         →  terraform apply
$ gi ch main     →  git checkout main
$ cd ~/Lib/App/zsh-  →  cd ~/Library/Application\ Support/zsh-ios
```

If an abbreviation is ambiguous, you're told -- just like IOS. Pick a number and the mapping is saved for next time:

```
% Ambiguous command: "gi ch"
  Pick a number to save as shorthand (Enter to cancel):
    1) git check-attr
    2) git checkout
    3) git cherry-pick
  > 2
  Saved: "gi ch" → "git checkout"
  In ~/Library/Application Support/zsh-ios/pins.txt
```

## How it works

zsh-ios builds a **prefix trie** from your PATH executables, shell history, aliases, Zsh builtins, and Zsh completion definitions. When you press Enter or Tab, every word is resolved against this trie using prefix matching. If a prefix uniquely identifies a command or subcommand, it expands. If it's ambiguous, you're prompted to pick.

**Deep disambiguation** looks ahead at subsequent words to narrow things down. `gi ch main` resolves to `git checkout main` because `git` is the only `gi*` command with a `ch*` subcommand. The same technique works for filesystem paths -- `cd ~/Lib/Applic/zsh-` resolves through `Application Support` (not `Application Scripts`) because only `Application Support` has a `zsh-*` child.

**Pins** are saved abbreviation rules. When you disambiguate interactively, the resolution is saved to a plain text file so the same abbreviation resolves instantly next time. Pins use longest-prefix matching: `gi ch` -> `git checkout` won't interfere with `gi` alone being ambiguous.

## Features

- **Command abbreviation** -- prefix-match any command or subcommand in your trie
- **Path abbreviation** -- `cd Des/Fo` -> `cd Desktop/Folder`, with deep disambiguation across path components
- **Deep disambiguation** -- subsequent words narrow ambiguous prefixes automatically
- **Tab expansion** -- Tab expands to the longest common prefix, then falls through to native Zsh completion for cycling
- **`?` key** -- show all available completions for the current prefix (IOS-style help)
- **Incremental learning** -- every successful command is added to the trie (failed commands are ignored)
- **Pins** -- persistent user-defined abbreviation rules stored in a plain text file
- **Interactive clarifier** -- ambiguous commands on Enter show a numbered menu with full command paths; selecting saves a pin
- **Path disambiguation** -- when multiple directories match, shows a numbered picker with single-keypress selection
- **Ghost resistance** -- abbreviations learned from typos are automatically deprioritized against real commands
- **Toggle on/off** -- `zsh-ios toggle` to disable without uninstalling
- **Fast** -- Rust binary with MessagePack-serialized trie; resolution takes < 10ms

## Data sources

The command trie is built from:

1. **PATH** -- all executable files in your `$PATH`
2. **Zsh builtins** -- `cd`, `echo`, `source`, etc.
3. **Aliases** -- both the alias name and the commands inside the alias value (e.g., `tfa='terraform apply'` teaches `terraform -> apply`)
4. **Shell history** -- every command from `~/.zsh_history`, split on pipes/semicolons
5. **Zsh completions** -- subcommand patterns from completion files (e.g., `_git-checkout` -> `git checkout`)

## Installation

### Requirements

- macOS (tested) or Linux
- Zsh
- Rust toolchain (installed automatically if missing)

### Quick install

```bash
git clone https://github.com/alxrxs/zsh-ios.git
cd zsh-ios
./install.sh
```

The install script will:
1. Install Rust via rustup if needed
2. Build and install the `zsh-ios` binary to `~/.cargo/bin/`
3. Copy the Zsh plugin to your config directory
4. Add a source line to `~/.zshrc`
5. Build the initial command trie

### Manual install

```bash
cargo install --path .
mkdir -p "${XDG_CONFIG_HOME:-$HOME/.config}/zsh-ios"
cp plugin/zsh-ios.zsh "${XDG_CONFIG_HOME:-$HOME/.config}/zsh-ios/"
echo 'source "${XDG_CONFIG_HOME:-$HOME/.config}/zsh-ios/zsh-ios.zsh"' >> ~/.zshrc
alias | zsh-ios build --aliases-stdin
```

## Usage

### Key bindings

| Key | Action |
|-----|--------|
| **Enter** | Resolve abbreviations and execute. If ambiguous, show interactive picker. |
| **Tab** | Expand to longest common prefix. If already expanded, fall through to native Zsh completion. |
| **?** | After a space, show all matching commands for the current prefix. |

### Commands

```bash
zsh-ios status          # Show config paths, tree stats, enabled/disabled
zsh-ios toggle          # Enable/disable without uninstalling
zsh-ios rebuild         # Rebuild the command trie (captures current aliases)
zsh-ios pin "g ch" --to "git checkout"    # Save an abbreviation rule
zsh-ios unpin "g ch"    # Remove a pin
zsh-ios pins            # List all saved pins
```

### Pins file

Pins are stored in plain text at `~/Library/Application Support/zsh-ios/pins.txt` (macOS) or `~/.config/zsh-ios/pins.txt` (Linux). Format:

```
g ch -> git checkout
tf -> terraform
k -> kubectl
```

Edit by hand anytime. Longest-prefix match applies: `g ch` takes priority over `g` when the input starts with `g ch`.

### How resolution works

Given input `ter ap --auto-approve`:

1. **Pin check** -- longest-prefix match against saved pins
2. **Trie walk** -- `ter` prefix-matches `terraform` (unique), then `ap` prefix-matches `apply` (unique)
3. **Flags** -- `--auto-approve` starts with `-`, passed through as-is (flags are never expanded)
4. **Path check** -- any word containing `/` or starting with `~`/`.` is resolved against the filesystem
5. **Result**: `terraform apply --auto-approve`

### Self-abbreviation

`zsh-ios` knows its own subcommands, so abbreviations work on it too:

```
$ zsh-ios reb   →  zsh-ios rebuild
$ zsh-ios st    →  zsh-ios status
```

### Flags

Flags (words starting with `-`) are never prefix-expanded. `-H` stays `-H`, it won't become `-HUP`. If a flag is an exact match in the trie (e.g., `-m` under `git commit`), resolution continues for subsequent words.

### How learning works

When you run a command, `zsh-ios` waits for it to finish. If it exits successfully (exit code 0), the command is learned into the trie. Failed commands are silently ignored, so typos and `command not found` errors won't pollute your trie.

Before learning, abbreviations are resolved to their full form -- typing `gi ch main` and having it resolve to `git checkout main` means the trie learns `git checkout`, not `gi ch`.

Aliases are also learned by value: if you have `alias tfa='terraform apply -auto-approve'`, the trie learns both `tfa` as a command and `terraform apply` as a subcommand path.

## Uninstall

```bash
./uninstall.sh
```

Removes the binary, offers to delete config directories (with confirmation), and cleans `~/.zshrc`.

## Project structure

```
src/
  main.rs          CLI entry point (clap subcommands)
  trie.rs          Prefix trie with MessagePack serialization
  resolve.rs       Core abbreviation resolution engine
  path_resolve.rs  Filesystem path abbreviation with deep disambiguation
  history.rs       Zsh history parser
  scanner.rs       PATH scanner, builtins, alias parser
  completions.rs   Zsh completion file parser
  pins.rs          Pin storage (load/save/match)
  config.rs        Config directory paths
plugin/
  zsh-ios.zsh      Zsh plugin (ZLE widgets, key bindings, preexec/precmd hooks)
install.sh         Installer
uninstall.sh       Uninstaller
```

## License

AGPL-3.0-only
