# zsh-ios configuration reference

All options live in `$XDG_CONFIG_HOME/zsh-ios/config.yaml` (usually
`~/.config/zsh-ios/config.yaml`). On macOS the path follows Application
Support conventions; run `zsh-ios status` to see the exact path in use.

Every field is optional. A missing file or a field left out falls back to
the compiled-in default. An invalid YAML file prints a warning to stderr
and falls back silently — an invalid config will never wedge your shell.

## Minimal example

```yaml
disable_learning: false
stale_threshold_seconds: 3600
```

## Full reference

### Core behaviour

#### `stale_threshold_seconds: u64` (default 3600)

How old `tree.msgpack` must be (in seconds) before the plugin auto-rebuilds
on shell startup. Set to a larger value on slow machines or when you manage
the trie manually via `zsh-ios rebuild`. The plugin parses this from
`zsh-ios status` so both the binary and the plugin always agree on the value.

#### `disable_learning: bool` (default false)

When true, `zsh-ios learn` is a no-op — the trie never grows from
interactive use. Useful on shared servers or CI boxes where you want a fixed,
deterministic trie built from `config.yaml` + explicit `zsh-ios rebuild` calls.

#### `command_blocklist: [string]` (default [])

Commands that zsh-ios must not touch. When the first word of the typed input
matches any entry here — either literally as typed or as the resolved first
word — `resolve` returns passthrough (exit 2) so the buffer runs exactly as
typed. Matching is case-sensitive and exact.

Example: `command_blocklist: [kubectl, terraform]` prevents accidental
abbreviation of `kubectl` even when `kub apply` would otherwise expand.

#### `disable_statistics: bool` (default false)

Skip the statistical tiebreaker (frequency x recency x success-rate). When
true, the engine never silently picks between candidates based on local
history — the picker appears instead. Use this for reproducible resolution
across machines and sessions.

#### `disable_galiases: bool` (default false)

When true, global aliases (`alias -g`) are not expanded before resolution.
Some users prefer the literal buffer to remain intact.

#### `disable_dynamic_harvest: bool` (default false)

When true, the build-time harvest of `_regex_arguments` specs via the zpty
worker is skipped. Saves roughly one second of shell startup time at the cost
of less complete arg-spec data for commands like `ip` and `iptables` whose
completion functions build their spec at runtime.

---

### Resolution determinism

#### `min_resolve_prefix_length: u32` (default 1)

Minimum character length of the first typed word before resolution is
attempted. Words shorter than this pass through unchanged. The default of 1
means all non-empty words are candidates for resolution. Set to 2 or 3 to
prevent single- or double-letter inputs from being expanded.

Example: with `min_resolve_prefix_length: 2`, typing `l` runs `l` literally
instead of expanding to `ls` (or whatever `l` would resolve to).

#### `force_picker_at_candidates: u32` (default 0 — disabled)

When the candidate pool at disambiguation time reaches this count, skip the
statistical tiebreaker and show the picker immediately. 0 disables this
behaviour (the engine always tries stats first). Useful when you prefer an
explicit choice over the engine's automatic selection once there are many
alternatives.

#### `dominance_margin: f32` (default 1.05)

The winning stats candidate must score at least this multiple above the
runner-up to be accepted automatically. A value of 1.0 means any score
advantage wins. Values above 1.0 require progressively clearer margins.
Lower values produce more aggressive auto-pick; higher values produce more
picker prompts.

#### `disable_cwd_scoring: bool` (default false)

When true, the per-directory usage-frequency multiplier is not applied during
scoring. The cwd multiplier gives up to a 1.5x boost to candidates that have
been used in the current directory before. Disable this when the trie is
shared across machines with different directory layouts where cwd signals
would be misleading.

#### `disable_sibling_context: bool` (default false)

When true, the `ZSH_IOS_LAST_CMD` sibling-context boost is not applied.
This boost gives a 1.3x nudge to candidates that match the command the
user most recently ran. Disable this for fully reproducible resolution that
does not depend on interactive history.

#### `disable_arg_type_narrowing: bool` (default false)

When true, arg-type narrowing is skipped during disambiguation. Arg-type
narrowing checks whether a word typed after the ambiguous command matches the
expected positional type of each candidate (for example, a directory path
would favour `cd` over `cat`). Disabling this removes one disambiguation
layer and leaves more decisions to the picker.

#### `disable_flag_matching: bool` (default false)

When true, flag-match narrowing is skipped during disambiguation.
Flag-match narrowing counts how many flags typed in the command line are
known to each candidate's arg spec. For example, `-r` is known to `grep`
but not to `git`, so typing `g -r` would ordinarily resolve to `grep`.
Disabling this removes that signal.

---

### Privacy and attack surface

#### `disable_worker: bool` (default false)

When true, the ZLE background worker (a `zpty`-based completion helper) is
not started. This disables the `complete-word`, `_approximate`, and
`alias-expand` worker tiers that give generic Zsh completion as a fallback.
The worker runs as a separate process inside a `zpty`; disabling it reduces
the shell's attack surface and eliminates the latency of starting a second
Zsh instance.

#### `disable_runtime_resolvers: [string]` (default [])

Runtime resolver ids to disable. When a resolver's id appears in this list,
`list_matches` returns an empty list instead of calling the resolver. Example:
`disable_runtime_resolvers: [git-branches, hosts]` prevents zsh-ios from
shelling out to `git` or reading `/etc/hosts` during completion.

Run `zsh-ios status` to see how many resolvers are registered. Resolver ids
are visible in the source under `fn id()` in each resolver's `TypeResolver`
implementation.

#### `excluded_fpath_dirs: [string]` (default [])

Directories to exclude from the Zsh `$fpath` scan during `build`. Entries
are matched as path prefixes after `~` expansion, so a single entry can
exclude a whole plugin manager's subtree. Example:
`excluded_fpath_dirs: [~/.local/share/zinit]` excludes every completion file
installed by zinit.

#### `disable_build_time_shell_exec: bool` (default false)

When true, the step that launches `zsh -ic` to enumerate user-defined shell
functions is skipped during `build`. This prevents an interactive-shell
subprocess and avoids any side-effects from `.zshrc` evaluation. The tradeoff
is that custom functions defined in `.zshrc` are not added to the trie.

---

### Performance

#### `resolver_ttls: {string: u64}` (default {})

Per-resolver cache TTL overrides in seconds. The key is the resolver `id()`.
When present this overrides the compiled-in `cache_ttl()` for that resolver.
Example: `resolver_ttls: {git-branches: 30}` re-fetches branches every 30
seconds instead of the compiled-in default (typically 5 seconds).

Setting a TTL of 0 disables caching for that resolver — every call goes
to the live source.

#### `worker_timeout_ms: u32` (default 500)

How long (in milliseconds) to wait for the ZLE worker before giving up on
a single completion request. The plugin parses this value from
`zsh-ios status`. Increase this on slow machines where the worker takes
longer to produce results; decrease it to fail faster and fall through to
the native shell.

#### `resolve_max_runtime_calls: u32` (default 0 — no cap)

Maximum number of live resolver calls per `resolve` / `complete` invocation.
Once the cap is reached, further resolver calls return empty so the response
stays fast. 0 means no cap. This is useful on low-latency targets where
even a handful of shell-out calls per keypress is unacceptable.

---

### Retention

#### `forget_unused_after_days: u32` (default 0 — disabled)

Prune trie nodes that have not been used in this many days AND whose total
use count is below 3. Applied during `build`. The count threshold of 3 is
hardcoded: nodes used more than 3 times are kept regardless of age — they
represent long-term utility. Set to 90 or 180 to gradually reclaim space
from commands you have stopped using.

#### `max_trie_size: u32` (default 0 — no cap)

Cap the total number of trie nodes after each `build`. The least-used and
oldest nodes are dropped (in ascending order of `(count, last_used)`) until
the trie is within the cap. 0 means no cap. This is a blunt instrument;
prefer `forget_unused_after_days` for selective pruning.

---

### Display

#### `picker_header_prefix: string` (default `%`)

Prefix character(s) printed before section headers in `?` output and the
ambiguity picker. The plugin parses this from `zsh-ios status` and uses
`${_zsh_ios_picker_prefix:-%}` in its output so both sides agree.

Changing this is mostly cosmetic. Some users prefer `>` or `#` to match
their prompt style.

#### `disable_list_colors: bool` (default false)

When true, ANSI colour codes are suppressed in `?` output even when stdout
is a TTY. By default, directory entries are coloured cyan when the
`list-colors` zstyle is configured.

#### `max_completions_shown: u32` (default 200)

Maximum number of items shown by the `?` completion formatter. Items beyond
this count are elided with an `... and N more` line. Lower values keep
output concise; higher values show everything (at the cost of more
screen space).

#### `tag_grouping: bool` (default true)

When false, tag-grouped display is never used even for commands that have
tag groups in the trie. The flat subcommand list is always shown instead.
Tag groups come from zstyle `_alternative` specs in Zsh completion functions
(for example, `kill` groups processes and jobs separately). Set to false for
a simpler, consistent output format.

---

## Configuration profiles

Apply any of these with the `zsh-ios preset` subcommand:

```
zsh-ios preset                 # list presets
zsh-ios preset power --show    # print the YAML without writing
zsh-ios preset power           # back up existing and write
zsh-ios preset power --force   # skip the backup
```

### Deterministic / reproducible (for CI, shared servers)

```yaml
# zsh-ios — deterministic / reproducible profile
# Resolution never depends on per-machine history; ties always surface as a
# picker rather than a silent pick.
disable_learning: true
disable_statistics: true
disable_sibling_context: true
disable_cwd_scoring: true
disable_arg_type_narrowing: false
disable_flag_matching: false
```

Learning is disabled so the trie stays fixed after an explicit `rebuild`.
Statistics and context boosts are turned off so the same input always
produces the same resolution regardless of who ran what before.

### Privacy-conscious (no worker, no build-time shell exec)

```yaml
# zsh-ios — privacy-conscious profile
# No background worker, no build-time shell exec, no dynamic harvest.
# Live `?` completion via the Rust binary's own subprocess calls
# (git, docker, etc.) still works.
disable_worker: true
disable_build_time_shell_exec: true
disable_runtime_resolvers:
  - git-branches
  - git-tags
  - git-remotes
  - hosts
  - users
```

No zpty worker is spawned, no interactive Zsh is launched during build,
and the resolvers that shell out to `git` or read system files are disabled.
Resolution still works fully; completion hints are less rich.

### Power user (all defaults, aggressive statistics)

```yaml
# zsh-ios — power user profile
# Reduced dominance margin so the stats tiebreaker is more willing to
# auto-pick. Git branch/tag lists refreshed more frequently.
dominance_margin: 1.01
force_picker_at_candidates: 0
max_completions_shown: 500
tag_grouping: true
resolver_ttls:
  git-branches: 10
  git-tags: 60
```

The dominance margin is reduced so the stats tiebreaker is more willing to
auto-pick. Git branch/tag lists are refreshed more frequently. All completion
items are shown up to 500. This profile is for interactive power users who
trust the engine to make good choices.
