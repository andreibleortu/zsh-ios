# zsh-ios data sources

zsh-ios consumes data from four broadly-distinct places:

1. **Static discovery at build** — what `zsh-ios build` reads once and stores in `tree.msgpack`.
2. **Runtime resolvers** — what the `?` / Tab path queries live at completion time.
3. **Live worker ingest** — what the background zpty worker dumps after first shell startup.
4. **Scoring signals** — metadata the trie tracks per node to rank candidates.

Together these feed the same `CommandTrie` and the same resolution pipeline.

---

## 1. Static discovery at build time

### PATH scan (`scanner.rs`)

`scan_path` iterates every directory in `$PATH` and inserts all executable, non-hidden regular files as first-level commands. Symlinks are followed (`fs::metadata` rather than `DirEntry::metadata`) so `cargo -> rustup` chains resolve correctly. Duplicates across PATH entries are de-duplicated; first occurrence wins.

### Zsh builtins (`scanner.rs`)

A hardcoded list of ~50 Zsh builtins (`cd`, `echo`, `source`, `alias`, `autoload`, `bindkey`, `compdef`, `zle`, etc.) is inserted at build time so they are always resolvable, even if they don't appear in PATH.

### Alias stream (`scanner.rs`, `ingest.rs`)

Aliases are fed to `build` via `--aliases-stdin` (piped from the shell's `alias` output). `parse_aliases` inserts:
- The alias name itself as a first-level command.
- The expanded command chain as a subcommand path, split on `|`, `&&`, `||`, `;`. For example `tfa='terraform apply'` teaches both `tfa` and the path `terraform → apply`.

### Shell history (`history.rs`)

`~/.zsh_history` (or `$HISTFILE`) is parsed; extended history format (`#timestamp\n`) is handled. Each entry is split into operator-separated segments, and each segment is inserted into the trie only if its first word is already known (on PATH, a builtin, or an alias). This prevents typos and stale scripts from polluting the trie.

### Shell functions (import at build, `main.rs`)

`import_shell_functions` runs `zsh -ic 'print -l ${(k)functions}'` to enumerate user-defined functions from the interactive `.zshrc`. Functions whose names start with `_` (private/completion helpers) or contain whitespace are skipped. Functions whose names already exist in the trie (as a PATH binary, for example) are also skipped. This step is skipped when `disable_build_time_shell_exec: true`.

### Zsh completion files (`completions.rs`)

Directories scanned (in priority order):

- System: `/usr/share/zsh/*/functions`, `/usr/local/share/zsh/site-functions`, `/opt/homebrew/share/zsh/site-functions`, `/usr/share/zsh/vendor-completions`, and equivalents.
- Plugin frameworks: Oh-My-Zsh (`~/.oh-my-zsh/plugins/*`, `~/.oh-my-zsh/completions`), Prezto (`~/.zprezto/modules/*/functions`), zinit (`~/.local/share/zinit/plugins/*`, `~/.local/share/zinit/completions`, `~/.zinit/…`), antidote (`~/.cache/antidote/*`, `~/.antidote/*`), antibody (`~/.cache/antibody/*`), znap (`~/.znap/*`), zplug (`~/.zplug/repos/*`), miscellaneous XDG locations (`~/.config/zsh/plugins/*`, `~/.config/zsh/completions`).
- `$FPATH` entries from the environment (used when `build` is called from an interactive shell via `--aliases-stdin`).
- Entries matching any `excluded_fpath_dirs` prefix are removed after expansion.

Grammar forms handled during parsing:

- `_arguments` with positional (`N:label:action`), flag (`-f:label:action`), and rest (`*:label:action`) specs.
- `->state` blocks with full case-body evaluation: the action strings inside each state are mapped to arg types via `action_to_arg_type` (which recognises `_files`, `_dirs`, `_executables`, `_users`, `_groups`, `_hosts`, `_signals`, `_interfaces`, `compadd`, `_describe`, `_alternative`, and ~100 named helpers such as `__git_branch_names`, `__docker_containers`, `__kubectl_pods`, `_wanted systemd-units`, etc.).
- `local -a VAR=(...)` / `typeset -a VAR=(...)` / `declare -a VAR=(...)` array literals referenced by downstream `_describe VAR` or `compadd -a VAR` calls.
- `_values 'tag' 'item[desc]'` with per-item description extraction.
- `_alternative '(excl) -a[desc]:...'` plus leading `(-a -b)` exclusion groups.
- `{-f,--force}` brace-expanded flag alias groups (all aliases map to the same spec entry).
- `_sequence` / `_guard` / `_call_function` wrappers (treated as pass-through; the wrapped action is resolved).
- Handwritten `case $words[2] in` completers — when no `_arguments` call is found, the parser falls back to scanning `case` arms for subcommand names.
- `_regex_arguments` DSL — static parse via `regex_args.rs` plus dynamic harvest at build time via the worker's `dump-regex-args <funcname>` protocol (see section 3).
- `_wanted TAG expl 'label' compadd …` → tag groups stored in `trie.tag_groups`.
- `_description TAG expl 'label'` → tag labels.
- Hardcoded override table for commands whose completion is too dynamic to parse statically (e.g., commands whose subcommand list is built by shelling out).

The result is arg specs for ~1400 commands, covering positional types and per-flag argument types.

(~83 `ARG_MODE_*` constants as of this writing — `grep '^pub const ARG_MODE_' src/trie.rs | wc -l` to confirm.)

### Fish completion files (`fish_completions.rs`)

Directories scanned: `/usr/share/fish/completions`, `/usr/share/fish/vendor_completions.d`, `/usr/local/share/fish/completions`, `/opt/homebrew/share/fish/completions`, `~/.config/fish/completions`, `~/.local/share/fish/vendor_completions.d`.

Each `.fish` file is parsed for `complete -c <cmd>` stanzas. The parser extracts:
- Subcommand conditions (`-n '__fish_seen_subcommand_from X'`) to associate flags with subcommands.
- `-l`/`-s` (long/short flag names) with `-d` (description) and `-r`/`-F` (requires argument / is a file).
- Argument type hints from descriptions and condition patterns.

Existing Zsh data wins on conflicts (Fish data supplements, not overwrites).

### Bash completion files (`bash_completions.rs`)

Directories scanned: `/etc/bash_completion.d`, `/usr/share/bash-completion/completions`, `/usr/local/share/bash-completion/completions`, `/opt/homebrew/share/bash-completion/completions`, `/opt/homebrew/etc/bash_completion.d`, `~/.local/share/bash-completion/completions`, `~/.bash_completion.d`.

Files with `.bash` extension or no extension are parsed. The parser extracts:
- `complete -F <func> <cmd>` — records the command name (function body not evaluated statically).
- `complete -W '<wordlist>' <cmd>` — word lists used as subcommand/argument enumerations.

Existing Zsh and Fish data wins on conflicts.

### carapace-spec YAML files (`carapace_completions.rs`)

Directories scanned: `~/.config/carapace/specs/`, `/usr/share/carapace/specs/`, `/usr/local/share/carapace/specs/`.

If the `carapace` binary is on PATH and `disable_build_time_shell_exec` is false, the scanner additionally shells to `carapace _list` to enumerate every builtin completer, then `carapace <cmd> _spec` to dump each as YAML. Dumps cache under `$XDG_CACHE_HOME/zsh-ios/carapace-specs/<name>.yaml` keyed by `carapace --version` so upgrades invalidate automatically. The cached files are then read the same way as user-authored specs.

Each spec's `name` / `description` / `flags` / `persistentflags` / `commands[]` / `completion.positional[N]` / `completion.flag[flag]` / `completion.positionalany` are recursively folded into `trie.root`, `trie.descriptions`, and `trie.arg_specs`. Action strings are resolved via:

- `$files` / `$files(pattern)` → `ARG_MODE_PATHS`
- `$directories` → `ARG_MODE_DIRS_ONLY`
- `$list(sep,items)` → static list (delimiter from first character)
- `$(cmd args)` → `call_program` with argv from `shlex::split`
- `["foo", "bar"]` / `["foo\tdescription"]` → static list (descriptions captured separately)

Comma-separated flag aliases (`-v, --verbose`) populate `flag_aliases` groups. Persistent flags inherit through the subcommand tree. Existing Zsh / Fish / Bash data wins on conflict.

### withfig/autocomplete (Fig) JSON specs (`fig_completions.rs` + `data/fig_dump.js`)

Fig's 500+ completion specs are written in TypeScript, so the pipeline is split:

1. **One-time fetch** (`zsh-ios fig-fetch`): clones `withfig/autocomplete`, runs `pnpm install && pnpm build` to produce compiled `.js` specs, then runs a bundled Node scriptlet (`data/fig_dump.js`, embedded via `include_str!`) that `require()`s every spec, replaces JS functions with the `"__FN__"` sentinel so `JSON.stringify` survives, and writes one JSON per spec to `$XDG_CACHE_HOME/zsh-ios/fig-json/`.

2. **Every build**: Rust reads those JSONs via `serde_json`, deserializes `FigSpec { name, description, subcommands[], options[], args }`, and folds into the trie:
   - `template: "filepaths"` → `ARG_MODE_PATHS`
   - `template: "folders"` → `ARG_MODE_DIRS_ONLY`
   - `template: "hosts"` → `ARG_MODE_HOSTS`
   - `template: "history"` / `"help"` → skipped (no resolver mapping)
   - `suggestions: [items]` → static list
   - `generators.script` (string or argv array) → `call_program`
   - `name: ["-v", "--verbose"]` → `flag_aliases` + individual `flag_args`
   - Subcommands nest recursively to unbounded depth

The `"__FN__"` sentinel prevents ghost `call_program` entries when a generator's logic was a JS closure we can't execute — only generators with an explicit `script` argv are usable by the resolver.

Users who skip `fig-fetch` stay completely dep-free; `scan_fig_completions` returns `(0, 0, 0)` silently when the JSON cache is absent. Node + pnpm (or npm) is required only for `fig-fetch` itself.

### Project manifests (resolved at both build and runtime)

Manifest resolvers walk up from the current working directory to find the nearest manifest file. At build time these are not scanned; they are queried live when `?` / Tab fires (see section 2). The manifest types and their parsed content are:

- `package.json` — `scripts` section (task names); `workspaces` globs; `dependencies` / `devDependencies` keys (package names for `npm-package`).
- `Makefile` / `makefile` / `GNUmakefile` — phony and concrete targets (regex on `^<target>:` lines, skipping variable assignments and internal targets).
- `justfile` / `Justfile` / `.justfile` — recipe names (regex on `^<name>:` and `^@<name>:` lines, excluding variable and setting lines).
- `Cargo.toml` — `[alias]` section keys (cargo task names); `[dependencies]` / `[dev-dependencies]` keys (crate names).
- `.cargo/config.toml` — additional `[alias]` section keys.
- `pyproject.toml` — `[tool.poetry.scripts]` and `[project.scripts]` section keys.
- `composer.json` — `scripts` section keys (same JSON shape as `package.json`).
- `build.gradle` / `build.gradle.kts` — task names via `task <name>` / `tasks.register("<name>")` regex.
- `Rakefile` / `Rakefile.rb` — `task :<name>` regex.
- `Pipfile` — `[scripts]` section keys.
- `pnpm-workspace.yaml` — `packages:` glob patterns (expanded to workspace package directories, then their `package.json` scripts).
- `lerna.json` — `packages` array patterns (same expansion).

---

## 2. Runtime resolvers (listed by arg type)

Runtime resolvers implement the `TypeResolver` trait from `type_resolver.rs`: a `list(ctx)` method that returns candidate strings, a `cache_ttl()` for on-disk caching, and an `id()` string for cache keys and `disable_runtime_resolvers` config. Results are cached via `runtime_cache.rs` (MessagePack files, atomic tempfile+rename writes, freshness by file mtime).

(71 resolvers registered as of this writing — `grep 'r\.register(' src/runtime_complete.rs | wc -l` to confirm.)

### POSIX / system

| `ARG_MODE_*` | id | Source | TTL |
|---|---|---|---|
| `USERS` | `users` | `dscl . list /Users` (macOS), then `/etc/passwd` | 3600 s |
| `GROUPS` | `groups` | `/etc/group` | 3600 s |
| `USERS_GROUPS` | `users-groups` | Union of users + groups, sorted + deduped | 3600 s |
| `HOSTS` | `hosts` | `/etc/hosts`, `~/.ssh/known_hosts`, `~/.ssh/config` (`Host` lines) | 3600 s |
| `SIGNALS` | `signals` | Hardcoded POSIX set: HUP INT QUIT ILL TRAP ABRT EMT FPE KILL BUS SEGV SYS PIPE ALRM TERM URG STOP TSTP CONT CHLD TTIN TTOU IO XCPU XFSZ VTALRM PROF WINCH INFO USR1 USR2 | 3600 s |
| `PORTS` | `ports` | `/etc/services` (name → port number mapping) | 3600 s |
| `NET_IFACES` | `net-ifaces` | `ifconfig -l` (macOS/BSD), `/sys/class/net` (Linux) | 3600 s |
| `LOCALES` | `locales` | `locale -a` | 3600 s |

### Filesystem

The `PATHS` / `DIRS_ONLY` / `EXECS_ONLY` types are handled directly by `complete_filesystem` in `resolve/complete.rs` rather than through the resolver registry. They walk the real filesystem relative to cwd.

### Git (12 resolvers, all CWD-sensitive)

| `ARG_MODE_*` | id | Source | TTL |
|---|---|---|---|
| `GIT_BRANCHES` | `git-branches` | `git for-each-ref --format=%(refname:short) refs/heads refs/remotes` | 5 s |
| `GIT_TAGS` | `git-tags` | `git for-each-ref --format=%(refname:short) refs/tags` | 5 s |
| `GIT_REMOTES` | `git-remotes` | `git remote` | 5 s |
| `GIT_FILES` | `git-files` | `git ls-files --cached --others --exclude-standard` | 5 s |
| `GIT_STASH` | `git-stash` | `git stash list` | 5 s |
| `GIT_WORKTREE` | `git-worktree` | `git worktree list` | 5 s |
| `GIT_SUBMODULE` | `git-submodule` | `git submodule status`; falls back to parsing `.gitmodules` via `git config --file .gitmodules --get-regexp path` | 300 s |
| `GIT_CONFIG_KEY` | `git-config-key` | `git config --list --name-only` | 60 s |
| `GIT_ALIAS` | `git-alias` | `git config --get-regexp ^alias\.` | 60 s |
| `GIT_COMMIT` | `git-commit` | `git log --oneline -50` | 10 s |
| `GIT_REFLOG` | `git-reflog` | `git reflog --oneline -50` | 10 s |
| `GIT_HEAD` | `git-head` | `git for-each-ref refs/heads refs/tags refs/remotes` + HEAD | 10 s |
| `GIT_BISECT` | `git-bisect` | Reads `.git/BISECT_TERMS` and `.git/BISECT_LOG`; static `start good bad skip reset` when no bisect active | 30 s |
| `GIT_REMOTE_REF` | `git-remote-ref` | `git ls-remote --refs <remote>` (remote inferred from prior words via `-n`/`--namespace` flag extraction) | 120 s |

### Docker

| `ARG_MODE_*` | id | Source | TTL |
|---|---|---|---|
| `DOCKER_CONTAINER` | `docker-container` | `docker ps --all --format {{.Names}}` | 5 s |
| `DOCKER_IMAGE` | `docker-image` | `docker images --format {{.Repository}}:{{.Tag}}` (also bare repo) | 30 s |
| `DOCKER_NETWORK` | `docker-network` | `docker network ls --format {{.Name}}` | 30 s |
| `DOCKER_VOLUME` | `docker-volume` | `docker volume ls --format {{.Name}}` | 30 s |
| `DOCKER_COMPOSE_SERVICE` | `docker-compose-service` | `docker compose ps --services` in nearest compose dir; falls back to parsing `services:` keys from compose YAML | 10 s |

### Kubernetes

| `ARG_MODE_*` | id | Source | TTL |
|---|---|---|---|
| `K8S_CONTEXT` | `k8s-context` | `kubectl config get-contexts -o name` | 60 s |
| `K8S_NAMESPACE` | `k8s-namespace` | `kubectl get namespaces -o name` | 30 s |
| `K8S_POD` | `k8s-pod` | `kubectl get pods -o name` (with `-n <namespace>` when present in prior words) | 5 s |
| `K8S_DEPLOYMENT` | `k8s-deployment` | `kubectl get deployments -o name` | 10 s |
| `K8S_SERVICE` | `k8s-service` | `kubectl get services -o name` | 10 s |
| `K8S_RESOURCE_KIND` | `k8s-resource-kind` | `kubectl api-resources --verbs=get -o name` | 300 s |

### systemd

| `ARG_MODE_*` | id | Source | TTL |
|---|---|---|---|
| `SYSTEMD_UNIT` | `systemd-unit` | `systemctl list-units --all --no-legend --no-pager` | 10 s |
| `SYSTEMD_SERVICE` | `systemd-service` | `systemctl list-units --type=service --all --no-legend --no-pager` | 10 s |
| `SYSTEMD_TIMER` | `systemd-timer` | `systemctl list-units --type=timer --all --no-legend --no-pager` | 10 s |
| `SYSTEMD_SOCKET` | `systemd-socket` | `systemctl list-units --type=socket --all --no-legend --no-pager` | 10 s |

### Session managers

| `ARG_MODE_*` | id | Source | TTL |
|---|---|---|---|
| `TMUX_SESSION` | `tmux-session` | `tmux list-sessions -F #S` | 5 s |
| `TMUX_WINDOW` | `tmux-window` | `tmux list-windows -a -F #S:#I:#W` | 5 s |
| `TMUX_PANE` | `tmux-pane` | `tmux list-panes -a -F #S:#I.#P` | 5 s |
| `SCREEN_SESSION` | `screen-session` | `screen -ls` (parses session lines) | 10 s |

### Package managers

| `ARG_MODE_*` | id | Source | TTL |
|---|---|---|---|
| `BREW_FORMULA` | `brew-formula` | `brew list --formula` | 300 s |
| `BREW_CASK` | `brew-cask` | `brew list --cask` | 300 s |
| `APT_PACKAGE` | `apt-package` | `/var/lib/dpkg/status` (installed) + `/var/lib/apt/lists/*_Packages` cache | 3600 s |
| `DNF_PACKAGE` | `dnf-package` | `dnf list installed` | 3600 s |
| `PACMAN_PACKAGE` | `pacman-package` | `pacman -Qq` | 3600 s |
| `NPM_PACKAGE` | `npm-package` | `package.json` `dependencies` + `devDependencies` keys (nearest ancestor); falls back to `npm list --json --depth=0` | 60 s |
| `PIP_PACKAGE` | `pip-package` | `pip list --format=columns` | 3600 s |
| `CARGO_CRATE` | `cargo-crate` | `Cargo.toml` `[dependencies]` + `[dev-dependencies]` keys (nearest ancestor) | 60 s |

### Project scripts

| `ARG_MODE_*` | id | Source | TTL |
|---|---|---|---|
| `NPM_SCRIPT` | `npm-script` | `package.json` `scripts` section (nearest ancestor) | 10 s |
| `MAKE_TARGET` | `make-target` | `Makefile` / `GNUmakefile` targets (nearest ancestor) | 10 s |
| `JUST_RECIPE` | `just-recipe` | `justfile` / `Justfile` recipes (nearest ancestor) | 10 s |
| `CARGO_TASK` | `cargo-task` | `Cargo.toml` `[alias]` + `.cargo/config.toml` `[alias]` (nearest ancestor) | 60 s |
| `POETRY_SCRIPT` | `poetry-script` | `pyproject.toml` `[tool.poetry.scripts]` + `[project.scripts]` (nearest ancestor) | 60 s |
| `COMPOSER_SCRIPT` | `composer-script` | `composer.json` `scripts` section (nearest ancestor) | 60 s |
| `GRADLE_TASK` | `gradle-task` | `build.gradle` / `build.gradle.kts` task names (nearest ancestor; also reads `gradle/wrapper/gradle-wrapper.properties` for project hints) | 30 s |
| `RAKE_TASK` | `rake-task` | `Rakefile` / `Rakefile.rb` `task :name` entries (nearest ancestor) | 30 s |
| `PNPM_WORKSPACE` | `pnpm-workspace` | `pnpm-workspace.yaml` `packages:` globs → expands to package dirs → reads each `package.json` name (nearest ancestor) | 60 s |
| `LERNA_PACKAGE` | `lerna-package` | `lerna.json` `packages` array → expands globs → reads package dirs (nearest ancestor) | 60 s |
| `YARN_WORKSPACE` | `yarn-workspace` | `package.json` `workspaces` field → glob expansion → reads each workspace `package.json` name | 60 s |
| `PIPENV_SCRIPT` | `pipenv-script` | `Pipfile` `[scripts]` section (nearest ancestor) | 60 s |

### Live shell state

| `ARG_MODE_*` | id | Source | TTL |
|---|---|---|---|
| `SHELL_FUNCTION` | `shell-function` | `trie.live_state["functions"]` (populated by worker ingest) | 0 (no cache) |
| `SHELL_ALIAS` | `shell-alias` | `trie.live_state["commands"]` / alias table from ingest | 0 |
| `SHELL_VAR` | `shell-var` | `trie.live_state["parameters"]` (populated by worker ingest) | 0 |
| `NAMED_DIR` | `named-dir` | `trie.named_dirs` (populated by `dump-nameddirs` ingest) | 0 |
| `HISTORY_ENTRY` | `history-entry` | `trie.live_state["history"]` | 0 |
| `JOB_SPEC` | `job-spec` | `trie.live_state["jobs"]` | 0 |
| `ZSH_WIDGET` | `zsh-widget` | `trie.live_state["widgets"]` | 0 |
| `ZSH_KEYMAP` | `zsh-keymap` | Hardcoded Zsh keymap names (emacs, viins, vicmd, viopp, visual, isearch, command, .safe) | 3600 s |
| `ZSH_MODULE` | `zsh-module` | `trie.live_state["modules"]` | 0 |
| `HASHED_COMMAND` | `hashed-command` | `trie.live_state["commands"]` (hash table entries) | 0 |

### Format-validated types (no enumeration)

These types are used for disambiguation and arg-type narrowing — when a typed word matches the format, it counts as evidence that the current command takes that type. They never produce completion lists.

| `ARG_MODE_*` | Validation |
|---|---|
| `IPV4` | `std::net::Ipv4Addr::parse` |
| `IPV6` | `std::net::Ipv6Addr::parse` |
| `EMAIL` | Regex: `[^@\s]+@[^@\s]+\.[^@\s]+` |
| `URL_SCHEME` | Regex: starts with `https?://` or `ftp://` etc. |
| `MAC_ADDR` | Regex: `[0-9a-fA-F]{2}([:-])` pattern |
| `TIMEZONE` | `/usr/share/zoneinfo` walk |

---

## 3. Live worker ingest

On first `precmd` after the worker becomes ready, the plugin runs:

```
( _zsh_ios_ingest_worker_state; _zsh_ios_harvest_regex_args ) &>/dev/null &|
```

This is disowned in the background and never blocks the shell.

### Worker request types

The worker (a `zpty`-named Zsh process) receives requests via `_ZIO_REQUEST` and responds to a named pipe / done-file protocol. The full request set:

| Request | Worker action | ingest target |
|---|---|---|
| `complete-word` | `zle complete-word` with compadd intercept | Completion list returned to caller (not stored in trie) |
| `approximate` | `zle _approximate` (completer chain overridden) | Completion list returned |
| `correct` | `zle _correct` | Completion list returned |
| `expand_alias` | `zle _expand_alias` (captures BUFFER delta) | Completion list returned |
| `history_complete_word` | `zle _history-complete-word` | Completion list returned |
| `dump-aliases` | `alias` output | `@aliases` section → `apply_aliases` |
| `dump-galiases` | `alias -g` output | `@galiases` section → `apply_galiases` → `trie.galiases` |
| `dump-saliases` | `alias -s` output | `@saliases` section → `apply_aliases` |
| `dump-functions` | `print -l ${(k)functions}` | `@functions` section → `apply_functions` → trie leaves |
| `dump-nameddirs` | `hash -d` output | `@nameddirs` section → `apply_nameddirs` → `trie.named_dirs` |
| `dump-zstyle` | `zstyle -L` output | `@zstyle` section → `parse_zstyle_output` → `trie.matcher_rules` + `trie.completion_styles` |
| `dump-history` | `$history` array | `@history` section → `apply_history` → trie via `history::parse_history` |
| `dump-dirstack` | `$PWD` + `$dirstack` | `@dirstack` section → `apply_dirstack` → `trie.dir_stack` |
| `dump-jobs` | jobs output | `trie.live_state["jobs"]` |
| `dump-commands` | hashed command table | `trie.live_state["commands"]` |
| `dump-parameters` | parameter names | `trie.live_state["parameters"]` |
| `dump-options` | option names | `trie.live_state["options"]` |
| `dump-widgets` | ZLE widget names | `trie.live_state["widgets"]` |
| `dump-modules` | loaded module names | `trie.live_state["modules"]` |
| `dump-regex-args <funcname>` | Overrides `_regex_words`/`_regex_arguments` sinks, autoloads + calls the function, captures specs | Fed to `zsh-ios regex-args-ingest` → `trie.arg_specs` (additive) |

The `complete-word` / `approximate` / `correct` / `expand_alias` / `history_complete_word` requests are used by the five-tier `?` fallback ladder in `_zsh_ios_handle_help` — each tier is tried only if the previous returned nothing. These do not modify the trie.

The `dump-*` requests are used once per session by `_zsh_ios_ingest_worker_state`, which concatenates all 13 dump types into a single `@<kind>` sectioned payload and pipes it to `zsh-ios ingest`.

### What each ingest section populates

- **`@aliases`** — `apply_aliases`: inserts alias names as commands and their expanded command chains as subcommand paths (same logic as `scanner::parse_aliases`).
- **`@galiases`** — `apply_galiases`: stores `name → value` pairs in `trie.galiases` (a `HashMap<String, String>`); consulted by `galiases::expand_galiases` before every trie walk.
- **`@saliases`** — treated identically to regular aliases (suffix aliases teach command names).
- **`@functions`** — `apply_functions`: inserts function names as trie leaves, skipping `_`-prefixed names.
- **`@nameddirs`** — `apply_nameddirs`: populates `trie.named_dirs` (a `HashMap<String, String>`); consulted by `path_resolve` when a path token contains `:` to expand `proj:/abs/path` references.
- **`@history`** — `apply_history`: runs `history::parse_history` on the ingest body, filtering and inserting entries as with the static build.
- **`@dirstack`** — `apply_dirstack`: populates `trie.dir_stack` (a `Vec<String>` with `$PWD` at index 0); consulted by `path_resolve` for `~N`, `~+N`, `~-N` dirstack references.
- **`@zstyle`** — `parse_zstyle_output`: parses `zstyle -L` output and extracts:
  - `matcher-list` → `trie.matcher_rules` (CaseInsensitive / PartialOn / BeginningAnchor / EndAnchor / Unknown variants).
  - `format`, `group-name`, `list-colors`, `menu`, `completer` → `trie.completion_styles`.
- **`@jobs`**, **`@commands`**, **`@parameters`**, **`@options`**, **`@widgets`**, **`@modules`** — stored verbatim in `trie.live_state` keyed by kind name. Read at completion time by the corresponding shell-state resolvers (JobSpec, ShellVar, ZshWidget, ZshModule, HashedCommand).

### `_regex_arguments` harvest

`_zsh_ios_harvest_regex_args` runs separately from the main ingest. It searches system fpath directories for completion files that contain `_regex_arguments` calls (checked via `grep`), skips files already processed at the same mtime (using a `~/.cache/zsh-ios/regex-harvest.cache` file), and for each new file:

1. Sends a `dump-regex-args <funcname>` request to the worker.
2. The worker overrides `_regex_words` and `_regex_arguments` to capture their spec arguments, then autoloads and calls the function.
3. The captured spec is sent to `zsh-ios regex-args-ingest`, which parses it via `regex_args::parse_regex_arguments` and merges the positional arg types into the existing `trie.arg_specs` for that command (additive — existing entries are not overwritten).

This harvest is mtime-gated and cache-tracked so it runs only for files that have changed since the last shell startup.

---

## 4. Scoring signals (ranking in `score_candidates_stats`)

When multiple candidates survive prefix matching and the narrowing layers, the statistical tiebreaker (`score_candidates_stats` in `resolve/engine.rs`) produces a composite score. A candidate must beat the runner-up by at least `dominance_margin` (default 1.05 = 5%) to be auto-selected; otherwise the picker is shown.

### Per-TrieNode signals

Each `TrieNode` stores:

| Field | Type | Meaning |
|---|---|---|
| `count` | `u32` | Times the command/path has been executed (incremented by `learn`) |
| `failures` | `u32` | Times the command exited non-zero (tracked via `learn --exit-code`) |
| `last_used` | `u64` | Unix timestamp of most recent use |
| `cwd_hits` | `Vec<(String, u32)>` | Per-directory usage frequency; capped at 8 entries (least-used evicted when full) |

Derived from these:

- **Frequency** (`count`): log-scaled so a heavily-used command doesn't completely dominate a rarely-used one.
- **Recency** (`last_used`): exponential decay with a 14-day half-life. Nodes never used score 0.5 (baseline) so they can still win against empty candidates.
- **Success rate**: `count / (count + failures)`, defaulting to 1.0 when no failure data exists.
- **cwd boost**: up to 1.5× when `cwd_hits` contains an entry matching the current working directory and `disable_cwd_scoring` is false.

### Environment signals

- **`_ZSH_IOS_LAST_CMD`**: exported by `_zsh_ios_precmd` after each command. The engine applies a 1.3× sibling-context boost to candidates whose root command matches this value, so the most recently used command family is more likely to auto-win on the next ambiguous input. Controlled by `disable_sibling_context`.

### Context hints (from buffer inference)

`ContextHint` variants inferred by `_zsh_ios_infer_context`, `_zsh_ios_infer_quote`, and `_zsh_ios_infer_param_context` are threaded via `--context`, `--quote`, `--param-context` flags:

- `Redirection` — after `>` / `>>` / `<`; arg resolution switches to file mode.
- `Math` / `Condition` — inside `(( ))` or `[[ ]]`; resolution is suppressed.
- `Array` — inside `( )` array literal context.
- `SingleQuoted` / `DoubleQuoted` / `Backticked` — inside a quoted string; expansion may be suppressed or limited.
- `ParameterName` — inside `${…}`; completion switches to parameter names.
- `Unknown` / `Command` / `Argument` — normal resolution.

### Narrowing layers applied before scoring

Scoring only operates on the set of candidates that survive all three narrowing layers:

1. **`deep_disambiguate`**: uses subsequent typed words as lookahead — only candidates that have a matching subcommand path for the next word survive. For example, `gi pu orig` eliminates all `gi*` candidates except `git` because only `git` has both a `pu*` subcommand and an `orig*` sub-subcommand.

2. **`narrow_by_arg_type`**: checks whether the next typed word is consistent with the positional-1 arg type in each candidate's `ArgSpec`. Uses `word_matches_type` which dispatches: filesystem checks for directory/file/executable types; registry lookups (with live resolver calls) for git/docker/etc. types; format validators for IPV4/EMAIL/etc. types.

3. **`narrow_by_flag_match`**: counts how many flags typed on the command line are known to each candidate's `ArgSpec`. Also checks alias siblings: if `-r` is an alias for `--recursive` in a spec, a typed `-r` counts as a hit for that spec's command. Candidates with the most flag hits survive.

Statistical scoring runs only if at least two candidates survive all three layers and `disable_statistics` is false. If the winner's score does not exceed the runner-up by `dominance_margin`, the picker is shown. If `force_picker_at_candidates` is set and the candidate pool is larger than that threshold, the picker is shown immediately without consulting stats.

---

## 5. Config knobs summary

All 29 knobs live in `$XDG_CONFIG_HOME/zsh-ios/config.yaml`. Full reference: `docs/config.md`.

| Knob | Default | Group | One-line description |
|---|---|---|---|
| `stale_threshold_seconds` | 3600 | Core | Seconds before auto-rebuild on shell startup |
| `disable_learning` | false | Core | Make `zsh-ios learn` a no-op |
| `command_blocklist` | [] | Core | Commands never touched (literal + resolved match) |
| `disable_statistics` | false | Core | Always show picker instead of auto-picking |
| `disable_galiases` | false | Core | Skip global-alias expansion before trie walk |
| `disable_dynamic_harvest` | false | Core | Skip `_regex_arguments` dynamic harvest at startup |
| `min_resolve_prefix_length` | 1 | Determinism | Minimum first-word length before resolution is attempted |
| `force_picker_at_candidates` | 0 | Determinism | Show picker immediately when candidate count reaches this |
| `dominance_margin` | 1.05 | Determinism | Stats winner must beat runner-up by this multiple |
| `disable_cwd_scoring` | false | Determinism | Skip per-directory usage-frequency boost |
| `disable_sibling_context` | false | Determinism | Skip `_ZSH_IOS_LAST_CMD` sibling boost |
| `disable_arg_type_narrowing` | false | Determinism | Skip arg-type narrowing layer |
| `disable_flag_matching` | false | Determinism | Skip flag-match narrowing layer |
| `disable_worker` | false | Privacy | Do not start the zpty background worker |
| `disable_runtime_resolvers` | [] | Privacy | Resolver ids to short-circuit to empty |
| `excluded_fpath_dirs` | [] | Privacy | Fpath dirs to exclude from `build` scan (prefix match) |
| `disable_build_time_shell_exec` | false | Privacy | Skip `zsh -ic` shell-function enumeration at build |
| `resolver_ttls` | {} | Performance | Per-resolver TTL overrides in seconds |
| `worker_timeout_ms` | 500 | Performance | How long to wait for worker per request |
| `resolve_max_runtime_calls` | 0 | Performance | Cap live resolver calls per invocation (0 = no cap) |
| `forget_unused_after_days` | 0 | Retention | Prune nodes unused for N days with count < 3 during build |
| `max_trie_size` | 0 | Retention | Cap total node count after build (0 = no cap) |
| `picker_header_prefix` | `%` | Display | Prefix character for section headers in `?` / picker output |
| `disable_list_colors` | false | Display | Suppress ANSI colour in `?` output |
| `max_completions_shown` | 200 | Display | Max items shown by `?` formatter |
| `tag_grouping` | true | Display | Use tag-grouped display when available |
| `disable_ghost_preview` | false | Ghost | Suppress the live resolved-command preview |
| `ghost_preview_style` | `fg=240` | Ghost | `region_highlight` style spec for ghost text |
| `ghost_preview_prefix` | `"  "` | Ghost | Text inserted between buffer and ghost text |

---

## 6. Links

- `docs/config.md` — full config reference with examples and per-knob prose
- `README.md` — user-facing overview
- `CLAUDE.md` — contributor guidance and architecture notes
