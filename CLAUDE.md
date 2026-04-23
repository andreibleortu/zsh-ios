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

The plugin also maintains a `zpty`-based **completion worker** with an extended request protocol. The worker understands: `complete-word` | `approximate` | `correct` | `expand_alias` | `history_complete_word` (completion tiers), and `dump-aliases` | `dump-galiases` | `dump-saliases` | `dump-functions` | `dump-nameddirs` | `dump-zstyle` | `dump-history` | `dump-dirstack` | `dump-jobs` | `dump-commands` | `dump-parameters` | `dump-options` | `dump-widgets` | `dump-modules` | `dump-regex-args <funcname>` (live shell state harvesting).

### Crate layout

Split lib + bin: `src/lib.rs` re-exports all modules publicly so tests and future library consumers can link against the core, and `src/main.rs` is a thin CLI on top (`use zsh_ios::*;`, clap parser, subcommand dispatch, `cmd_*` handlers, lock helpers, and the `_zio_*` shell-serialization functions). The `test_util::CWD_LOCK` mutex lives in `lib.rs` under `#[cfg(test)]`.

### Binary subcommands (src/main.rs)

`build`, `resolve`, `complete`, `learn`, `pin`, `unpin`, `pins`, `toggle`, `rebuild`, `status`, `explain`, `ingest`, `regex-args-ingest`, `preset`.

- `resolve` / `complete` take `--context` / `--quote` / `--param-context` flags for shell-context inference threaded in by the plugin.
- `learn` takes `--exit-code` (default 0) and `--cwd` for per-directory frequency tracking.
- `ingest` reads a sectioned `@<kind>` payload from stdin and applies it to the trie (aliases, galiases, saliases, functions, named dirs, history, dirstack, jobs, commands, parameters, options, widgets, modules, zstyle).
- `regex-args-ingest` folds a `_regex_arguments` harvest capture from stdin into the trie's arg specs.
- `preset` lists, shows (`--show`), or applies (`--force` skips backup) one of three named YAML presets: `deterministic`, `privacy`, `power`.
- `build` and `rebuild` differ only in that `rebuild` re-execs via `zsh -c 'alias | zsh-ios build --aliases-stdin'` so aliases from the user's interactive shell are captured.
- `explain` is a debugging tool — `resolve::explain` walks the same primitives as `resolve_line` and narrates each step, then prints the actual `resolve_line` result so any drift is visible.

### Module responsibilities

- **`trie.rs`** — `CommandTrie` (prefix trie of command/subcommand words) plus `ArgSpec` per command (positional arg types + flag-value arg types). Serialized to disk as MessagePack (`tree.msgpack`). Defines 83 `ARG_MODE_*` constants (filesystem, POSIX, git, docker, k8s, systemd, tmux, package managers, project scripts, shell state, live session, format-validated types) — these are the contract between `completions.rs` (writer) and `resolve/`/`runtime_complete.rs` (readers). Also stores `galiases`, `named_dirs`, `dir_stack`, `matcher_rules`, `completion_styles`, `live_state`, and `tag_groups`.
- **`resolve/`** — the resolution subsystem, split across three files that share a parent `mod.rs` re-exporting the public API. Callers still use `crate::resolve::resolve_line` / `resolve::complete` / `resolve::explain` — the internal split is invisible.
  - **`resolve/engine.rs`** — the trie walk, pin lookup, wrapper-command drill-in (`sudo`, `env`, `nice`, `doas`, `watch`), **deep disambiguation** (looks ahead at subsequent words to narrow an ambiguous prefix), arg-spec application, `narrow_by_arg_type`, `narrow_by_flag_match`, `score_candidates_stats` (frequency × recency × success_rate × cwd × sibling boost, gated by dominance_margin), and the `explain` narrator. The 100-test `mod tests` block lives at the bottom.
  - **`resolve/complete.rs`** — the `?` key path (`complete`, `complete_segment`, `complete_flags`, `format_columns`, `terminal_width`, `show_type_completions`, `complete_filesystem`, `resolve_first_word`). Reads engine helpers via `use super::engine::*;`.
  - **`resolve/escape.rs`** — 3 short helpers (`shell_escape_path`, `shell_escape_path_glob`, `escape_resolved_path`) called by engine when splicing a resolved path back into the output buffer. All three are `pub(super)`.
- **`path_resolve.rs`** — filesystem path abbreviation with the same deep-disambiguation idea applied to directory components. Knows about `!` (suffix), `*` (contains), `**` (literal glob for shell), and `\!`/`\*` escapes. Also expands named-dir references (`proj:/proj/`) and dirstack references (`~N`, `~+N`, `~-N`).
- **`completions.rs`** — parses Zsh completion files to extract per-command subcommand lists, per-position/per-flag arg types, and subcommand descriptions. Grammar forms handled: `_arguments` with positional/flag/rest specs, `->state` blocks with case-body evaluation, `local -a`/`typeset -a`/`declare -a` arrays referenced by `_describe`/`compadd -a`, `_values 'tag' 'item[desc]'` with per-item descriptions, `_alternative` with exclusion groups, `{-f,--force}` brace-expanded flag groups, `_sequence`/`_guard`/`_call_function` wrappers, handwritten `case $words[2] in` completers, `_regex_arguments` DSL (static parse + dynamic harvest via worker), `_wanted`/`_description` tag groups, and ~100 hardcoded helper recognitions (`__git_*`, `__docker_*`, `__kubectl_*`, `_wanted systemd-units`, etc.). Also has a hardcoded override table for commands whose completion is too dynamic to parse.
- **`runtime_complete.rs`** — 71 live resolvers invoked at `?`/`Tab` time, covering: POSIX users/groups/hosts/signals/ports/locales/net-ifaces; 12 git resolvers (branches, tags, remotes, files, stash, worktree, submodule, config-key, alias, commit, reflog, head, bisect, remote-ref); docker (container, image, network, volume, compose-service); k8s (context, namespace, pod, deployment, service, resource-kind); systemd (unit, service, timer, socket); tmux/screen sessions/windows/panes; package managers (brew formula/cask, apt, dnf, pacman, npm, pip, cargo); project scripts (npm, make, just, cargo-task, poetry, composer, gradle, rake, pnpm-workspace, lerna, yarn-workspace, pipenv); shell state (function, alias, var, named-dir, history-entry, job-spec, zsh-widget, zsh-keymap, zsh-module, hashed-command). Format-validated types (IPV4, IPV6, EMAIL, MAC_ADDR, URL_SCHEME, TIMEZONE) use regex/stdlib validators instead of enumeration.
- **`scanner.rs`** — PATH scan, Zsh builtin list, alias parser (teaches both the alias name *and* the expanded command chain as subcommand paths).
- **`history.rs`** — Zsh history parser; `split_command_segments` splits on `|`, `&&`, `||`, `;` so each segment learns independently.
- **`pins.rs`** — plain-text pin file (`abbrev -> expansion` lines), longest-prefix match at lookup time.
- **`config.rs`** — XDG / macOS Application Support path resolution.
- **`user_config.rs`** — optional `$config_dir/config.yaml`. 29 config knobs across six groups: core behaviour, resolution determinism, privacy/attack surface, performance, retention, and display/ghost-preview. Uses `serde(deny_unknown_fields)` so typos in field names error instead of silently defaulting. Invalid YAML prints a warning and falls back to defaults — it must never wedge the shell. See `docs/config.md` for the full reference.
- **`runtime_config.rs`** — `RuntimeConfig` struct mirroring `UserConfig` knobs, published via `OnceLock<RwLock<RuntimeConfig>>`. Hot paths call `runtime_config::get()` for a cheap clone; `runtime_config::set()` is called once at CLI entry.
- **`type_resolver.rs`** — `TypeResolver` trait (`list`, `cache_ttl`, `id`) and `Registry` (map of `ARG_MODE_*` → `Box<dyn TypeResolver>`). Used by `runtime_complete.rs`.
- **`runtime_cache.rs`** — on-disk MessagePack TTL cache for `TypeResolver` results. Writes via sibling tempfile + atomic rename. TTL checked against file mtime; `Duration::ZERO` disables caching for a resolver.
- **`bash_completions.rs`** — scans standard Bash completion directories (`/etc/bash_completion.d`, `/usr/share/bash-completion/completions`, etc.) and parses `complete -F` / `complete -W` stanzas to supplement the trie. Zsh and Fish data win on conflicts.
- **`fish_completions.rs`** — scans Fish completion directories (`/usr/share/fish/completions`, `~/.config/fish/completions`, etc.) and parses `.fish` completion files to supplement the trie.
- **`galiases.rs`** — `expand_galiases` rewrites global aliases token-by-token before the trie walk. Not recursive. Skips tokens inside single/double quotes, `$(...)`, `` `...` ``, and `${...}`.
- **`ingest.rs`** — `cmd_ingest` reads a sectioned `@<kind>` payload from stdin and folds it into the trie. `apply_ingest` is exported for unit tests. Handles all worker dump types.
- **`locks.rs`** — `lock_for(path)` acquires an exclusive `fs2` advisory flock on a sibling `.lock` file. Called by `cmd_build`, `cmd_learn`, `cmd_pin`, `cmd_unpin`, `cmd_ingest`, and `cmd_regex_args_ingest`.
- **`presets.rs`** — three bundled YAML preset strings (`DETERMINISTIC`, `PRIVACY`, `POWER`) and `cmd_preset` dispatch. Presets are applied by writing to the user config path with optional backup.
- **`regex_args.rs`** — tokenizes and walks `_regex_arguments` DSL bodies to extract per-positional arg types and static enumerations. Output feeds `trie::ArgSpec` via `completions::scan_completions`.
- **`data/descriptions.yaml`** — bundled via `include_str!` at compile time; fallback subcommand descriptions when Zsh completion files don't provide one.

### Plugin internals worth knowing

- Retval capture runs in the `precmd` hook, chaining through any user-defined `precmd` so third-party hooks can't displace it. `_zsh_ios_precmd` then learns the previous command only if `$?` was 0.
- The plugin spawns a `zpty`-based **completion worker** that preloads `compinit` so generic Zsh completion can be queried cheaply from the `?` key. The worker sources this same plugin file but bails immediately via the `_ZSH_IOS_IS_WORKER` guard at the top — do not move that guard.
- A stale-trie rebuild runs in the background on shell startup if `tree.msgpack` is >1h old.
- **Ghost-text preview**: `_zsh_ios_ghost_preview_widget` is hooked into `line-pre-redraw`. It sets `POSTDISPLAY` to the resolved command (prefixed by `_zsh_ios_ghost_prefix`, default two spaces) and paints it with a `region_highlight` range past the end of BUFFER using the `_zsh_ios_ghost_style` spec (default `fg=240`). Cache: if BUFFER hasn't changed, the previous POSTDISPLAY is replayed without calling the binary. Suppressed when the buffer is empty, starts with `!`, contains a newline, or when the resolved form equals the buffer.
- **Shell-context inference**: `_zsh_ios_infer_context` (math/condition/redirection/argument), `_zsh_ios_infer_quote` (none/single/double/backtick/dollar), and `_zsh_ios_infer_param_context` (inside `${…}`) thread `--context`, `--quote`, and `--param-context` flags to `resolve` and `complete` calls so the engine can apply context-appropriate behaviour.
- **First-precmd background ingest**: on the first `precmd` after the worker becomes ready, `(_zsh_ios_ingest_worker_state; _zsh_ios_harvest_regex_args) &>/dev/null &|` is disowned in the background. This dumps all 13 live-state kinds plus regex-args harvest into the trie without blocking the shell.
- **Leading-`!` bypass**: if `BUFFER` starts with `!`, Enter/Tab/`?` fall through to native zsh. The Rust side mirrors this via `starts_with_bang`.
- **Ambiguous picker** is keystroke-driven. Digits auto-commit the moment typed number uniquely identifies an option. Tab/arrow keys (`ESC [ A/B/C/D`) cycle the highlight (Up/Left = previous, Down/Right = next). Enter commits digits if any, else cycle highlight, else cancels. Backspace erases one digit or steps cycle back. Any other key cancels. Used by both Enter and Tab via `_zsh_ios_handle_ambiguity <output> <mode>`.
- **Tab is two-stage on ambiguity**: first Tab does the LCP extend + one-per-line hint. The post-LCP buffer is stashed in `_zsh_ios_last_tab_buffer`; a second Tab on the unchanged buffer escalates to the full picker. Any edit clears the stash.
- **`?` key five-tier fallback**: (1) Rust `complete` binary, (2) worker `complete-word` (full Zsh completion via compadd intercept), (3) worker `_approximate` (fuzzy/typo-tolerant), (4) worker `_correct`, (5) worker `_expand_alias` → `_history-complete-word`. Each tier is tried only if the previous yielded nothing.

### Learning invariants

Commands are only added to the trie after they exit successfully, **after being resolved to their expanded form** — so the trie learns `git branch`, not `gi br`. `is_prefix_of_existing` prevents junk prefixes like `terr` from being inserted when `terraform` already exists. During `build`, history entries are also filtered against PATH/builtins/aliases.

### Concurrency safety

The plugin spawns `zsh-ios learn` in the background after every command, which means multiple load-mutate-save cycles can overlap. Two invariants protect `tree.msgpack`:
- `CommandTrie::save` writes to `tree.msgpack.tmp.$pid` then renames — readers never see a partial file.
- `cmd_build`, `cmd_learn`, `cmd_pin`, `cmd_unpin`, `cmd_ingest`, and `cmd_regex_args_ingest` hold an exclusive `fs2` advisory flock via `locks::lock_for` across the full load-mutate-save cycle.

`cmd_toggle` uses `O_CREAT|O_EXCL` to avoid an analogous race on the `disabled` marker file. `load_trie` distinguishes missing-file from decode-failure and prints the error with a rebuild hint; do not silence it — that was a real regression we fixed.

## Conventions

- Edition 2024, no workspace — single crate.
- No custom error type; `Result<T, Box<dyn Error>>` / `io::Result` are used directly.
- YAML parsing uses `serde_yaml_ng` (drop-in for the now-unmaintained `serde_yaml`). The `regex` crate (1.x) is a dependency used in several parsers.
- Hot paths read tuning knobs via `runtime_config::get()` (cheap clone from `OnceLock<RwLock<RuntimeConfig>>`); do not re-read `UserConfig` from disk in hot paths.
- Tests live inline in each module under `#[cfg(test)] mod tests`. Prefer adding tests next to the function under test rather than in a separate file. End-to-end CLI tests live in `tests/cli.rs` and spawn the actual binary with an isolated `HOME`/`XDG_CONFIG_HOME` tempdir. Zsh plugin tests live in `tests/plugin/*.bats`; they source the plugin with ZLE stubbed (`zle` → records into `_zle_calls`), the binary replaced by `tests/plugin/helpers/zsh-ios-stub` (shapable via `ZSH_IOS_STUB_*` env vars), and the picker's `read`s redirected from `$_ZSH_IOS_TEST_INPUT_FD` instead of `/dev/tty`. Driver lives at `tests/plugin/helpers/run-in-zsh`; `tests/plugin/helpers/test_helper.bash` gives tests `keystrokes '...'` + `zsh_run '<snippet>'`.
- Any test that mutates process-global state (`cwd`, `$PATH`) must take `crate::test_util::CWD_LOCK` for the duration of its work — the default parallel test runner will race otherwise.
