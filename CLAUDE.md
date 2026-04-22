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
bats tests/plugin/                # run Zsh plugin bats suite (picker, safe_eval, widget bypasses)
bats tests/plugin/picker.bats     # run one bats file
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

### Crate layout

Split lib + bin: `src/lib.rs` re-exports all modules publicly so tests and future library consumers can link against the core, and `src/main.rs` is a thin CLI on top (`use zsh_ios::*;`, clap parser, subcommand dispatch, `cmd_*` handlers, lock helpers, and the `_zio_*` shell-serialization functions). The `test_util::CWD_LOCK` mutex lives in `lib.rs` under `#[cfg(test)]`.

### Binary subcommands (src/main.rs)

`build`, `resolve`, `complete`, `learn`, `pin`, `unpin`, `pins`, `toggle`, `rebuild`, `status`, `explain`. `build` and `rebuild` differ only in that `rebuild` re-execs via `zsh -c 'alias | zsh-ios build --aliases-stdin'` so aliases from the user's interactive shell are captured (aliases can't be read from a child process otherwise). `explain` is a debugging tool — `resolve::explain` walks the same primitives as `resolve_line` (wrapper detect → pin lookup → trie prefix search → deep disambiguation → arg-spec) and narrates each step, then prints the actual `resolve_line` result so any drift between narrator and engine is visible.

### Module responsibilities

- **`trie.rs`** — `CommandTrie` (prefix trie of command/subcommand words) plus `ArgSpec` per command (positional arg types + flag-value arg types). Serialized to disk as MessagePack (`tree.msgpack`). Defines 16 arg-type constants (DIRECTORY, FILE, EXECUTABLE, BRANCH, TAG, REMOTE, TRACKED_FILE, HOST, USER, GROUP, SIGNAL, PID, INTERFACE, PORT, LOCALE, NORMAL) — these are the contract between `completions.rs` (writer) and `resolve.rs`/`runtime_complete.rs` (readers).
- **`resolve/`** — the resolution subsystem, split across three files that share a parent `mod.rs` re-exporting the public API. Callers still use `crate::resolve::resolve_line` / `resolve::complete` / `resolve::explain` — the internal split is invisible.
  - **`resolve/engine.rs`** — the trie walk, pin lookup, wrapper-command drill-in (`sudo`, `env`, `nice`, `doas`, `watch`), **deep disambiguation** (looks ahead at subsequent words to narrow an ambiguous prefix — e.g. `gi pu orig` resolves because only `git` has a `pu*` subcommand with `orig*` below it), arg-spec application (path/executable/runtime-type), and the `explain` narrator. The 100-test `mod tests` block is at the bottom of this file — it intentionally tests across the split (it calls into `complete` and `escape` via `pub(super)` + `use super::super::...` at the top of the test mod) rather than being split by feature.
  - **`resolve/complete.rs`** — the `?` key path (`complete`, `complete_segment`, `complete_flags`, `format_columns`, `terminal_width`, `show_type_completions`, `complete_filesystem`, `resolve_first_word`). Reads engine helpers (arg-spec lookup, context rules, `split_on_operators`, `starts_with_bang`) via `use super::engine::*;`.
  - **`resolve/escape.rs`** — 3 short helpers (`shell_escape_path`, `shell_escape_path_glob`, `escape_resolved_path`) called by engine when splicing a resolved path back into the output buffer. All three are `pub(super)` so engine reaches them via `super::escape::...` without leaking beyond `resolve`.
- **`path_resolve.rs`** — filesystem path abbreviation with the same deep-disambiguation idea applied to directory components. Knows about `!` (suffix), `*` (contains), `**` (literal glob for shell), and `\!`/`\*` escapes.
- **`completions.rs`** — parses Zsh completion files (`_arguments` specs, `->state`, `_alternative`, `_regex_arguments`) to extract per-command subcommand lists and per-position/per-flag arg types. Also extracts subcommand descriptions for `?` help. This is where the arg-type intelligence for ~1400 commands comes from. Has a hardcoded override table for commands whose completion is too dynamic to parse.
- **`runtime_complete.rs`** — live resolvers invoked at `?`/`Tab` time: git branches/tags/remotes/tracked-files (shells out to `git`), users (`/etc/passwd` or `dscl`), hosts (`/etc/hosts` + `~/.ssh/known_hosts`), signals (hardcoded POSIX), interfaces, ports (`/etc/services`), locales.
- **`scanner.rs`** — PATH scan, Zsh builtin list, alias parser (teaches both the alias name *and* the expanded command chain as subcommand paths).
- **`history.rs`** — Zsh history parser; `split_command_segments` splits on `|`, `&&`, `||`, `;` so each segment learns independently.
- **`pins.rs`** — plain-text pin file (`abbrev -> expansion` lines), longest-prefix match at lookup time.
- **`config.rs`** — XDG / macOS Application Support path resolution.
- **`data/descriptions.yaml`** — bundled via `include_str!` at compile time; fallback subcommand descriptions when Zsh completion files don't provide one.
- **`user_config.rs`** — optional `$config_dir/config.yaml`. Knobs: `stale_threshold_seconds` (Zsh plugin parses this out of `zsh-ios status` → `Stale threshold: Ns`), `disable_learning` (short-circuits `cmd_learn`), `command_blocklist` (checked in `cmd_resolve` both literally on the typed first word *and* on the resolved first word — so blocking `kubectl` catches `kub ...` too; blocklist hits print the original input and exit 2). Uses `serde(deny_unknown_fields)` so typos in field names error instead of silently defaulting. Invalid YAML prints a warning and falls back to defaults — it must never wedge the shell.

### Plugin internals worth knowing

- Retval capture runs in the `precmd` magic function (which zsh invokes **before** `precmd_functions`), chaining through any user-defined `precmd` so third-party hooks can't displace it. `_zsh_ios_precmd` then learns the previous command only if `$?` was 0.
- The plugin also spawns a `zpty`-based **completion worker** (`_zsh_ios_worker_*`) that preloads `compinit` so generic Zsh completion can be queried cheaply from the `?` key. The worker sources this same plugin file but bails immediately via the `_ZSH_IOS_IS_WORKER` guard at the top — do not move that guard.
- A stale-trie rebuild runs in the background on shell startup if `tree.msgpack` is >1h old.
- **Leading-`!` bypass**: if `BUFFER` starts with `!`, Enter/Tab/`?` fall through to native zsh (history expansion, literal run). The Rust side mirrors this: `resolve_line` and `complete` short-circuit via `starts_with_bang` so the binary is safe to call directly on such input too.
- **Ambiguous picker** is keystroke-driven: reads one char at a time. Digits auto-commit the moment the typed number uniquely identifies an option (no longer number `<=N` extends the buffered digits); out-of-range digits are silently dropped. `\t` cycles a highlight through options (wraps at the end, redraws the `> ` line as `> [N] choice`); Enter commits digits if any, else the cycle highlight, else cancels. Backspace erases one digit or steps the cycle back. Any other key cancels. Used by both Enter and Tab via `_zsh_ios_handle_ambiguity <output> <mode>` — mode `accept` runs the selection (Enter path), mode `expand` populates BUFFER and `zle reset-prompt`s so the user can edit or re-Enter (Tab path). Both modes pin the chosen mapping.
- **Tab is two-stage on ambiguity**: first Tab does the LCP extend + one-per-line hint (old behavior, cheap). The post-LCP buffer is stashed in `_zsh_ios_last_tab_buffer`; a second Tab on the unchanged buffer escalates to `_zsh_ios_handle_ambiguity ... expand` (the picker above). Any edit to the buffer — or any non-ambiguous Tab result — clears the stash so the next Tab starts fresh. This preserves the free LCP progress (`doc` → `docker`) while still giving the user a proper picker when they actually want to disambiguate.

### Learning invariants

Commands are only added to the trie after they exit successfully, **after being resolved to their expanded form** — so the trie learns `git branch`, not `gi br`. `is_prefix_of_existing` prevents junk prefixes like `terr` from being inserted when `terraform` already exists. During `build`, history entries are also filtered against PATH/builtins/aliases.

### Concurrency safety

The plugin spawns `zsh-ios learn` in the background after every command, which means multiple load-mutate-save cycles can overlap. Two invariants protect `tree.msgpack`:
- `CommandTrie::save` writes to `tree.msgpack.tmp.$pid` then renames — readers never see a partial file.
- `cmd_build`, `cmd_learn`, `cmd_pin`, `cmd_unpin` hold an exclusive `fs2` advisory flock on a sibling `.lock` file across the full load-mutate-save cycle.

`cmd_toggle` uses `O_CREAT|O_EXCL` to avoid an analogous race on the `disabled` marker file. `load_trie` distinguishes missing-file from decode-failure and prints the error with a rebuild hint; do not silence it — that was a real regression we fixed.

## Conventions

- Edition 2024, no workspace — single crate.
- No custom error type; `Result<T, Box<dyn Error>>` / `io::Result` are used directly.
- YAML parsing uses `serde_yaml_ng` (drop-in for the now-unmaintained `serde_yaml`).
- Tests live inline in each module under `#[cfg(test)] mod tests`. Prefer adding tests next to the function under test rather than in a separate file. End-to-end CLI tests live in `tests/cli.rs` and spawn the actual binary with an isolated `HOME`/`XDG_CONFIG_HOME` tempdir. Zsh plugin tests live in `tests/plugin/*.bats`; they source the plugin with ZLE stubbed (`zle` → records into `_zle_calls`), the binary replaced by `tests/plugin/helpers/zsh-ios-stub` (shapable via `ZSH_IOS_STUB_*` env vars), and the picker's `read`s redirected from `$_ZSH_IOS_TEST_INPUT_FD` instead of `/dev/tty`. Driver lives at `tests/plugin/helpers/run-in-zsh`; `tests/plugin/helpers/test_helper.bash` gives tests `keystrokes '...'` + `zsh_run '<snippet>'`.
- Any test that mutates process-global state (`cwd`, `$PATH`) must take `crate::test_util::CWD_LOCK` for the duration of its work — the default parallel test runner will race otherwise. This is why `CommandTrie::save`-related tests use tempdirs but the cwd-touching ones explicitly lock.
