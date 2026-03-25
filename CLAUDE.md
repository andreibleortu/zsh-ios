# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Project

zsh-ios is a command abbreviation engine for Zsh inspired by Cisco IOS CLI. It builds a prefix trie from PATH executables, shell history, aliases, Zsh builtins, and completion definitions. Users type abbreviated commands and the shell resolves them. Written in Rust with a Zsh plugin frontend.

## Build & Test Commands

```bash
cargo build                    # Debug build
cargo build --release          # Release build (LTO, stripped)
cargo test                     # Run all tests
cargo test --lib               # Library tests only
cargo test <test_name>         # Run a single test by name
cargo clippy                   # Lint
cargo fmt --check              # Check formatting
cargo llvm-cov --no-cfg-coverage --summary-only   # Coverage report (needs gcc as cc, not zig cc)
```

## Architecture

**Resolution pipeline**: Pin check (longest-prefix match) → arg mode check → trie prefix walk per word → deep disambiguation (look-ahead at subsequent words) → filesystem path resolution → shell escaping.

**Key modules** (`src/`):
- `trie.rs` — BTreeMap-based prefix trie with counts; serialized to MessagePack (`tree.msgpack`). `CommandTrie` holds `TrieNode`, `ArgSpecMap`, `ArgModeMap`, and `DescriptionMap`. 16 `ARG_MODE_*` constants for typed argument positions.
- `resolve.rs` — Core resolution engine: `resolve_line()` splits on pipe/chain operators and resolves each segment via `resolve()`. Walks trie, handles flags as pass-through, deep disambiguation, `ArgMode` enum (DirsOnly/Paths/ExecsOnly/Runtime(u8)/Normal) for context-aware argument resolution, returns `ResolveResult` enum (Resolved/Ambiguous/PassThrough). `complete_segment()` powers the `?` key with type-aware position hints.
- `runtime_complete.rs` — Runtime completion resolvers for expanded arg types (git branches/tags/remotes/files, users, groups, hosts, signals, ports, network interfaces, locales). Each resource is a per-field `LazyLock` static (independent lazy init, no mutex contention); git queries are always fresh (CWD-dependent).
- `path_resolve.rs` — Filesystem path abbreviation: exact → case-sensitive prefix → case-insensitive prefix, with deep path disambiguation. Supports `!suffix` matching, `*contains` matching, `\` escaping, and dirs-only mode
- `scanner.rs` — PATH executable scanner, alias parser, hardcoded Zsh builtins list
- `history.rs` — Zsh history parser (plain text & extended `: timestamp:duration;command` format)
- `completions.rs` — Extracts subcommands and per-position arg specs from Zsh completion files. Parses `_arguments` specs, `->state` resolution, `_alternative` blocks, `_regex_arguments`. Well-known hardcoded overrides supplement dynamic completions (git, ssh, kill, chown, etc.) in `apply_well_known_specs()`. Also extracts `command:'description'` patterns from Zsh completion arrays and loads YAML fallback descriptions via `load_descriptions()`.
- `pins.rs` — Pin storage (`abbrev -> expanded` text format), longest-prefix matching
- `config.rs` — XDG-aware config directory resolution, macOS `~/Library/Application Support` fallback
- `main.rs` — Clap-derived CLI: `build`, `resolve`, `complete`, `learn`, `pin`, `unpin`, `toggle`, `rebuild`, `status`

**Description system** (`data/descriptions.yaml` + `completions.rs`):
- Two-layer: `data/descriptions.yaml` has fallback subcommand descriptions for ~30 commands, bundled at compile time via `include_str!`. Zsh completion files are parsed for `command:'description'` patterns (e.g. `_git` has 418 entries). Parsed descriptions always override YAML fallbacks.
- **Git descriptions are NOT in the YAML** — they come exclusively from `_git` completions. Other commands (docker, kubectl, cargo, terraform, etc.) use YAML fallbacks since their Zsh completion files lack description arrays.
- Descriptions are stored in `trie.descriptions: DescriptionMap` (`HashMap<String, HashMap<String, String>>`, parent command → subcommand → description) and used by `complete_segment()` for IOS-style `?` help output.

**Zsh plugin** (`plugin/zsh-ios.zsh`):
- ZLE widgets bound to Enter (resolve+disambiguate), Tab (LCP expand), ? (show completions)
- `_zsh_ios_save_retval()` captures `$?` first in `precmd_functions` (prepended, not appended) so other hooks cannot corrupt the exit code before learning
- `_zsh_ios_safe_eval()` validates Rust binary output contains only `_zio_*` variable assignments before `eval`'ing — injection guard
- Background trie rebuild when tree age > 1 hour (cross-platform: `stat -f %m` on macOS, `stat -c %Y` on Linux)

## Key Design Decisions

- **Context-aware arg resolution**: `ArgSpec` in `trie.rs` stores per-position and per-flag argument types for 1400+ commands (from Zsh completions + hardcoded overrides). `arg_type_for_word()` in `resolve.rs` checks flag-value, positional spec, then falls back to `arg_mode()`. Add well-known command overrides in `apply_well_known_specs()` in `completions.rs`. 16 arg types defined as constants in `trie.rs` (Paths, DirsOnly, ExecsOnly, Users, Hosts, Signals, GitBranches, etc.).
- **Filesystem-based path detection**: Arguments are checked against the real filesystem rather than syntactic heuristics. If a file/dir matching the prefix exists, it's treated as a path.
- **Suffix matching**: `!` prefix on path components matches by suffix (`!5` matches `test-5`). Handled in `resolve_component` in `path_resolve.rs`.
- **Contains matching**: `*` prefix on path components matches by substring (`*prod` matches `app-config-prod`). Same location.
- **Backslash escaping**: `\!` and `\*` treat `!`/`*` as literal prefix characters, not mode switches.
- **Pipe/chain/background resolution**: `resolve_line()` in `resolve.rs` splits on `|`, `&&`, `||`, `;`, and `&` (background operator, respecting quotes) and resolves each segment independently.
- **Deep disambiguation**: When prefix matching is ambiguous, the engine looks ahead at subsequent words to narrow candidates without prompting the user
- **Exact match priority**: If a word exactly matches a trie entry it always wins immediately, no prefix search needed. Ghost prevention is handled at learning time (prefix guard + unknown command check) so abbreviations never enter the trie.
- **Exit-code gating**: Only commands that succeed (exit code 0) are learned incrementally. Only fully resolved (non-ambiguous) commands are learned.
- **Learning guards**: Abbreviated prefixes (e.g. `terr` when `terraform` exists) and unknown commands (not on PATH/builtins/aliases) are never inserted into the trie. See `should_skip_command()` in `history.rs` and `is_prefix_of_existing()` in `trie.rs`.
- **File-based toggle**: Presence of a `disabled` file in config dir enables/disables the plugin without uninstalling

## Testing Conventions

- Tests live in `#[cfg(test)] mod tests` at the bottom of each file.
- Filesystem tests use `std::env::temp_dir().join("zsh-ios-test-*")` and clean up with `fs::remove_dir_all`.
- YAML description tests use `load_yaml_descriptions()` helper in resolve.rs.
- `main.rs`, `runtime_complete.rs`, `config.rs` are I/O-heavy and have 0% unit test coverage — they need a live shell/filesystem/process table.
- No clippy overrides. `cargo clippy` should pass clean.

## Rust Edition & Dependencies

Edition 2024. Key deps: `clap` 4.x (derive), `serde` + `rmp-serde` (MessagePack), `serde_yaml` 0.9 (description fallbacks), `dirs` 6.x (config dirs).
