# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## What this is

`zsh-ios` is a Cisco-IOS-style command abbreviation engine for Zsh. A Rust binary (`zsh-ios`) builds a prefix trie from PATH/history/aliases/zsh completions and resolves abbreviated command lines; a Zsh plugin (`plugin/zsh-ios.zsh`) wires it into ZLE via widgets bound to Enter, Tab, and `?`.

## Common commands

```bash
cargo build                       # debug build
cargo build --release             # release build (lto, stripped)
cargo install --path .            # install to ~/.cargo/bin/
cargo test                        # run all unit tests (scanner, history, pins, trie, path_resolve, resolve, completions)
cargo test <name>                 # run a single test by name substring
cargo test -p zsh-ios <module>::  # run tests in one module, e.g. `resolve::`
./install.sh                      # full install (builds, copies plugin, edits ~/.zshrc)
./uninstall.sh                    # uninstall

# Exercising the binary during development:
alias | target/debug/zsh-ios build --aliases-stdin   # rebuild trie with current shell aliases
target/debug/zsh-ios resolve gi br                   # test abbreviation resolution
target/debug/zsh-ios complete 'git '                 # test ? key completion output
target/debug/zsh-ios status                          # show config paths & stats
```

There is no separate lint step; rely on `cargo build` / `cargo clippy` and `cargo fmt`.

## Architecture

### Two-process model

1. **Rust binary** (`src/`) does all resolution, completion, and learning. Fast enough (<10ms) to invoke synchronously per keystroke.
2. **Zsh plugin** (`plugin/zsh-ios.zsh`) installs three ZLE widgets — `accept-line` (Enter), `expand-or-complete` (Tab), and a `?` self-insert override — that shell out to the binary and `eval` its output. Communication is one-way: the binary prints shell-quoted `_zio_*` variable assignments, and `_zsh_ios_safe_eval` validates every line starts with `_zio_` before eval'ing it (security boundary — treat this invariant as load-bearing).

The binary exit codes are the plugin's signal: `0` = resolved (stdout is the new buffer), `1` = ambiguous command (stdout is `_zio_*` assignments for the picker), `2` = passthrough (run as-is), `3` = path ambiguous.

### Binary subcommands (src/main.rs)

`build`, `resolve`, `complete`, `learn`, `pin`, `unpin`, `pins`, `toggle`, `rebuild`, `status`. `build` and `rebuild` differ only in that `rebuild` re-execs via `zsh -c 'alias | zsh-ios build --aliases-stdin'` so aliases from the user's interactive shell are captured (aliases can't be read from a child process otherwise).

### Module responsibilities

- **`trie.rs`** — `CommandTrie` (prefix trie of command/subcommand words) plus `ArgSpec` per command (positional arg types + flag-value arg types). Serialized to disk as MessagePack (`tree.msgpack`). Defines 16 arg-type constants (DIRECTORY, FILE, EXECUTABLE, BRANCH, TAG, REMOTE, TRACKED_FILE, HOST, USER, GROUP, SIGNAL, PID, INTERFACE, PORT, LOCALE, NORMAL) — these are the contract between `completions.rs` (writer) and `resolve.rs`/`runtime_complete.rs` (readers).
- **`resolve.rs`** — the resolution engine. Walks the trie with prefix/suffix/contains matching, handles pins, does **deep disambiguation** (looks ahead at subsequent words to narrow an ambiguous prefix — e.g. `gi pu orig` resolves because only `git` has a `pu*` subcommand with `orig*` below it). Also implements the `?` completion path. Longest module; touch carefully.
- **`path_resolve.rs`** — filesystem path abbreviation with the same deep-disambiguation idea applied to directory components. Knows about `!` (suffix), `*` (contains), `**` (literal glob for shell), and `\!`/`\*` escapes.
- **`completions.rs`** — parses Zsh completion files (`_arguments` specs, `->state`, `_alternative`, `_regex_arguments`) to extract per-command subcommand lists and per-position/per-flag arg types. Also extracts subcommand descriptions for `?` help. This is where the arg-type intelligence for ~1400 commands comes from. Has a hardcoded override table for commands whose completion is too dynamic to parse.
- **`runtime_complete.rs`** — live resolvers invoked at `?`/`Tab` time: git branches/tags/remotes/tracked-files (shells out to `git`), users (`/etc/passwd` or `dscl`), hosts (`/etc/hosts` + `~/.ssh/known_hosts`), signals (hardcoded POSIX), interfaces, ports (`/etc/services`), locales.
- **`scanner.rs`** — PATH scan, Zsh builtin list, alias parser (teaches both the alias name *and* the expanded command chain as subcommand paths).
- **`history.rs`** — Zsh history parser; `split_command_segments` splits on `|`, `&&`, `||`, `;` so each segment learns independently.
- **`pins.rs`** — plain-text pin file (`abbrev -> expansion` lines), longest-prefix match at lookup time.
- **`config.rs`** — XDG / macOS Application Support path resolution.
- **`data/descriptions.yaml`** — bundled via `include_str!` at compile time; fallback subcommand descriptions when Zsh completion files don't provide one.

### Plugin internals worth knowing

- `_zsh_ios_precmd` learns the previous command *only if exit code was 0*. `_zsh_ios_save_retval` is force-inserted as the **first** `precmd_functions` entry so other hooks can't clobber `$?` before it's captured.
- The plugin also spawns a `zpty`-based **completion worker** (`_zsh_ios_worker_*`) that preloads `compinit` so generic Zsh completion can be queried cheaply from the `?` key. The worker sources this same plugin file but bails immediately via the `_ZSH_IOS_IS_WORKER` guard at the top — do not move that guard.
- A stale-trie rebuild runs in the background on shell startup if `tree.msgpack` is >1h old.

### Learning invariants

Commands are only added to the trie after they exit successfully, **after being resolved to their expanded form** — so the trie learns `git branch`, not `gi br`. `is_prefix_of_existing` prevents junk prefixes like `terr` from being inserted when `terraform` already exists. During `build`, history entries are also filtered against PATH/builtins/aliases.

## Conventions

- Edition 2024, no workspace — single crate.
- No custom error type; `Result<T, Box<dyn Error>>` / `io::Result` are used directly.
- Tests live inline in each module under `#[cfg(test)] mod tests`. Prefer adding tests next to the function under test rather than in a separate file.
