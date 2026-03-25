# zsh-ios

Command abbreviation engine for Zsh inspired by Cisco IOS. Type abbreviated commands and let the shell figure out the rest.

```
$ ter ap         →  terraform apply
$ gi br          →  git branch
$ cd ~/Lib/App/zsh-  →  cd ~/Library/Application\ Support/zsh-ios
$ cd te/!5       →  cd tests/test-5
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

**Deep disambiguation** looks ahead at subsequent words to narrow things down. `gi pu orig main` resolves to `git push origin main` because `git` is the only `gi*` command with a `pu*` subcommand. The same technique works for filesystem paths -- `cd ~/Lib/Applic/zsh-` resolves through `Application Support` (not `Application Scripts`) because only `Application Support` has a `zsh-*` child.

**Pins** are saved abbreviation rules. When you disambiguate interactively, the resolution is saved to a plain text file so the same abbreviation resolves instantly next time. Pins use longest-prefix matching: `gi ch` -> `git checkout` won't interfere with `gi` alone being ambiguous.

## Features

- **Command abbreviation** -- prefix-match any command or subcommand in your trie
- **Path abbreviation** -- `cd Des/Fo` -> `cd Desktop/Folder`, with deep disambiguation across path components
- **Suffix matching** -- `!` prefix matches by suffix: `cd te/!5` -> `cd tests/test-5` (matches entries ending with `5`)
- **Contains matching** -- `*` prefix matches by substring: `cd *prod` -> `cd app-config-prod` (matches entries containing `prod`)
- **Shell glob passthrough** -- `**` passes a literal `*` to the shell: `chmod +x **.py` -> `chmod +x *.py` (shell expands the glob)
- **Pipe/chain resolution** -- commands joined by `|`, `&&`, `||`, `;` are each resolved independently
- **Context-aware argument resolution** -- commands like `cd` and `ls` resolve arguments against the filesystem (not the trie); commands like `which` and `man` resolve against executables only
- **Deep disambiguation** -- subsequent words narrow ambiguous prefixes automatically
- **Tab expansion** -- Tab expands to the longest common prefix, then falls through to native Zsh completion for cycling
- **`?` key** -- show all available completions for the current prefix (IOS-style help)
- **Incremental learning** -- every successful command is added to the trie (failed commands are ignored)
- **Pins** -- persistent user-defined abbreviation rules stored in a plain text file
- **Interactive clarifier** -- ambiguous commands on Enter show a numbered menu with full command paths; selecting saves a pin
- **Path disambiguation** -- when multiple directories match, shows a numbered picker with single-keypress selection
- **Toggle on/off** -- `zsh-ios toggle` to disable without uninstalling
- **Fast** -- Rust binary with MessagePack-serialized trie; resolution takes < 10ms

## Data sources

The command trie is built from:

1. **PATH** -- all executable files in your `$PATH`
2. **Zsh builtins** -- `cd`, `echo`, `source`, etc.
3. **Aliases** -- both the alias name and the commands inside the alias value (e.g., `tfa='terraform apply'` teaches `terraform -> apply`)
4. **Shell history** -- commands from `~/.zsh_history` that correspond to known executables (typos and gone scripts are filtered out)
5. **Zsh completions** -- subcommand patterns and per-position argument specs from completion files. Parses `_arguments` specs, `->state` resolution, `_alternative` blocks, and `_regex_arguments`. Recognizes completion action functions (`__git_branch_names`, `_users`, `_hosts`, `_signals`, etc.) to determine what type of argument each position expects.

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
| **?** | IOS-style help. Show completions for the current position — subcommands, flags with expected argument type, or live argument values (branches, hosts, signals, users, tracked files, ...). |

The `?` key is position and context-aware:

```
git ?              →  all git subcommands (multi-column, auto-sized to terminal width)
git checkout ?     →  Expects: <branch>  /  main  feature-x  ...
git checkout -?    →  -b <branch>   -B <branch>   --orphan <branch>  ...
git checkout -b?   →  -b expects: <branch>  /  main  feature-x  ...
git add ?          →  Expects: <tracked-file>  /  src/main.rs  Cargo.toml  ...
git push ?         →  Expects: <remote>  /  origin
kill -s ?          →  Expects: <signal>  /  HUP  INT  KILL  TERM  ...
kill -?            →  -s <signal>
chown ?            →  Expects: <user>  /  andrei  root  daemon  ...
ping ?             →  Expects: <host>  /  (from /etc/hosts + ~/.ssh/known_hosts)
ssh -?             →  -l <user>   -i <file>   -R   -o   ...
```

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
4. **Path resolution** -- arguments are checked against the real filesystem; if a matching file or directory exists, the abbreviation is expanded
5. **Result**: `terraform apply --auto-approve`

The engine is context-aware about what kind of arguments each command and subcommand takes. Arg specs are extracted from Zsh completion files for 1400+ commands, supplemented by hardcoded overrides for commands with complex dynamic completions:

| Argument type | Examples | Resolved from |
|---------------|----------|---------------|
| **Directory** | `cd`, `pushd`, `mkdir` | Filesystem (dirs only) |
| **File path** | `ls`, `rm`, `cat`, `vim`, `cp`, `mv` | Filesystem |
| **Executable** | `which`, `type`, `man`, `sudo -u` | Command trie |
| **Branch** | `git checkout`, `git merge`, `git rebase` | `git for-each-ref refs/heads` |
| **Tag** | `git tag`, `git push <remote> <tag>` | `git for-each-ref refs/tags` |
| **Remote** | `git push`, `git pull`, `git fetch` | `git remote` |
| **Tracked file** | `git add`, `git rm`, `git restore` | `git ls-files` |
| **Host** | `ssh`, `ping`, `dig`, `traceroute` | `/etc/hosts` + `~/.ssh/known_hosts` |
| **User** | `chown`, `su`, `sudo -u` | `/etc/passwd` (or `dscl` on macOS) |
| **Group** | `chgrp` | `/etc/group` |
| **Signal** | `kill -s` | Hardcoded POSIX signal names |
| **PID** | `kill` | Numeric (not resolved) |
| **Network interface** | `ifconfig`, `ping -I` | `ifconfig -l` (or `/sys/class/net`) |
| **Port** | port-related flags | `/etc/services` |
| **Locale** | locale flags | `locale -a` |
| **Normal** | everything else | Trie first; path-like args (`./foo`, `~/bar`) resolve against filesystem |

Arg type detection is per-position and per-flag: `git push <remote> <branch>` knows position 1 is a remote and position 2 is a branch. `ssh -l <user>` knows `-l` expects a username.

### Suffix matching

Prefix `!` on a path component to match by **suffix** instead of prefix:

```
$ cd te/!5        →  cd tests/test-5       (ends with "5")
$ cat !results    →  cat parse_results.py  (ends with "results")
$ ls !.md         →  ls README.md          (ends with ".md")
```

Suffix matching works anywhere in a path and combines freely with prefix matching. Case-insensitive fallback applies just like prefix matching.

### Contains matching

Prefix `*` on a path component to match by **substring**:

```
$ cd *prod        →  cd app-config-prod    (contains "prod")
$ ls *config      →  ls app-config.yaml    (contains "config")
```

### Shell glob passthrough

Prefix `**` to pass a literal `*` through to the shell for glob expansion:

```
$ chmod +x **.py        →  chmod +x *.py         (shell expands to all .py files)
$ chmod +x ./src/**.rs  →  chmod +x ./src/*.rs
$ rm **.log             →  rm *.log
```

Without `**`, a bare `*` would be interpreted as contains-matching mode. Use `**` whenever you want the shell to expand the glob rather than zsh-ios resolving the path.

### Escaping `!` and `*`

If a filename literally starts with `!` or `*`, escape with `\` to match as a prefix:

```
$ cd \!imp        →  cd !important         (literal !, not suffix mode)
$ ls \*star       →  ls *starred           (literal *, not contains mode)
```

### Pipes and chains

Each segment of a pipeline or chain is resolved independently:

```
$ gi st | gr main    →  git status | grep main
$ cd src && ls *.rs  →  cd src && ls *.rs
$ mak clean; mak     →  make clean; make
```

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

Before learning, abbreviations are resolved to their full form -- typing `gi br` and having it resolve to `git branch` means the trie learns `git branch`, not `gi br`. If resolution is ambiguous, nothing is learned at all.

The trie is further protected from junk:
- **Prefix guard** -- abbreviated prefixes like `terr` are never learned when `terraform` already exists
- **Existence check** -- during `build`, history entries are only learned if the command is a known executable (on PATH, a builtin, or an alias)

Aliases are also learned by value: if you have `alias tfa='terraform apply -auto-approve'`, the trie learns both `tfa` as a command and `terraform apply` as a subcommand path.

## Uninstall

```bash
./uninstall.sh
```

Removes the binary, offers to delete config directories (with confirmation), and cleans `~/.zshrc`.

## Project structure

```
src/
  main.rs               CLI entry point (clap subcommands)
  trie.rs               Prefix trie with MessagePack serialization; 16 arg type constants
  resolve.rs            Core abbreviation resolution engine; ? key completion
  path_resolve.rs       Filesystem path abbreviation with deep disambiguation
  runtime_complete.rs   Runtime resolvers: git branches/tags/remotes/files, users,
                        groups, hosts, signals, ports, network interfaces, locales
  history.rs            Zsh history parser
  scanner.rs            PATH scanner, builtins, alias parser
  completions.rs        Zsh completion file parser (→state, _alternative, _regex_arguments);
                        also extracts subcommand descriptions from completion arrays
  pins.rs               Pin storage (load/save/match)
  config.rs             Config directory paths
data/
  descriptions.yaml     Fallback subcommand descriptions for IOS-style ? help
                        (bundled at compile time; parsed Zsh descriptions override these)
plugin/
  zsh-ios.zsh           Zsh plugin (ZLE widgets, key bindings, preexec/precmd hooks)
install.sh              Installer
uninstall.sh            Uninstaller
```

## License

AGPL-3.0-only
