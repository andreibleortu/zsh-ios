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

Selection is keystroke-driven — the moment your digits uniquely identify an option, it fires without Enter. So in a 3-option menu, typing `2` picks immediately; with 20 options, typing `1` waits (because 10-19 are still reachable) but `13` fires on the `3`. Enter force-commits the current digits; Enter on an empty buffer cancels. Arrow keys and Tab cycle the highlight.

## How it works

zsh-ios builds a **prefix trie** from your PATH executables, shell history, aliases, Zsh builtins, Zsh/Fish/Bash/carapace/fig completion definitions, and live shell state. When you press Enter or Tab, every word is resolved against this trie using prefix matching. If a prefix uniquely identifies a command or subcommand, it expands. If it's ambiguous, you're prompted to pick.

**Deep disambiguation** looks ahead at subsequent words to narrow things down. `gi pu orig main` resolves to `git push origin main` because `git` is the only `gi*` command with a `pu*` subcommand. The same technique works for filesystem paths -- `cd ~/Lib/Applic/zsh-` resolves through `Application Support` (not `Application Scripts`) because only `Application Support` has a `zsh-*` child.

**Pins** are saved abbreviation rules. When you disambiguate interactively, the resolution is saved to a plain text file so the same abbreviation resolves instantly next time. Pins use longest-prefix matching: `gi ch` -> `git checkout` won't interfere with `gi` alone being ambiguous.

## Features

- **Command abbreviation** -- prefix-match any command or subcommand in your trie
- **Path abbreviation** -- `cd Des/Fo` -> `cd Desktop/Folder`, with deep disambiguation across path components
- **Suffix matching** -- `!` prefix matches by suffix: `cd te/!5` -> `cd tests/test-5` (matches entries ending with `5`)
- **Contains matching** -- `*` prefix matches by substring: `cd *prod` -> `cd app-config-prod` (matches entries containing `prod`)
- **Shell glob passthrough** -- `**` passes a literal `*` to the shell: `chmod +x **.py` -> `chmod +x *.py` (shell expands the glob)
- **Named directory expansion** -- `proj:/file` expands via Zsh `hash -d` named dirs; `~2/file` expands via dirstack
- **Global alias expansion** -- global aliases (`alias -g`) are expanded token-by-token before the trie walk
- **Pipe/chain resolution** -- commands joined by `|`, `&&`, `||`, `;` are each resolved independently
- **Context-aware argument resolution** -- commands like `cd` and `ls` resolve arguments against the filesystem (not the trie); commands like `which` and `man` resolve against executables only
- **Deep disambiguation** -- subsequent words narrow ambiguous prefixes automatically
- **Tab expansion** -- Tab expands to the longest common prefix, then falls through to native Zsh completion for cycling
- **`?` key** -- show all available completions for the current prefix (IOS-style help) with a five-tier fallback ladder: Rust resolver → worker `complete-word` → worker `_approximate` → worker `_correct` → worker `_expand_alias` / `_history-complete-word`
- **Ghost-text preview** -- live resolved command rendered faintly after the cursor as you type (via `POSTDISPLAY` + `region_highlight`), updated on every redraw without re-invoking the binary when the buffer hasn't changed
- **Incremental learning** -- every successful command is added to the trie (failed commands are ignored)
- **Pins** -- persistent user-defined abbreviation rules stored in a plain text file
- **Interactive clarifier** -- ambiguous commands on Enter show a numbered menu; arrow keys and Tab cycle, digits jump directly, selecting saves a pin
- **Path disambiguation** -- when multiple directories match, shows a numbered picker with single-keypress selection
- **Toggle on/off** -- `zsh-ios toggle` to disable without uninstalling
- **Config presets** -- `zsh-ios preset` to apply `deterministic`, `privacy`, or `power` profiles
- **Fast** -- Rust binary with MessagePack-serialized trie; resolution takes < 10ms
- **Over 70 runtime resolvers** -- live data at `?`/Tab time: git (branches/tags/remotes/files/stash/worktree/submodule/commit/reflog/bisect), docker, k8s, systemd, tmux, screen, brew, apt, dnf, pacman, npm, pip, cargo, and project scripts from Makefile/justfile/package.json/Cargo.toml/pyproject.toml/composer.json/build.gradle/Rakefile/Pipfile/pnpm-workspace.yaml/lerna.json
- **External spec catalogs** -- carapace-spec YAML (user + system + `carapace _spec` dumps) and withfig/autocomplete (500+ TypeScript specs compiled via `zsh-ios fig-fetch`) supplement Zsh/Fish/Bash parsing with the long tail of commands they don't cover

## Data sources

The command trie is built from:

1. **PATH** -- all executable files in your `$PATH`
2. **Zsh builtins** -- `cd`, `echo`, `source`, etc.
3. **Aliases** -- both the alias name and the commands inside the alias value (e.g., `tfa='terraform apply'` teaches `terraform -> apply`)
4. **Global aliases** -- `alias -g` entries stored separately; expanded before every trie walk
5. **Shell history** -- commands from `~/.zsh_history` that correspond to known executables (typos and gone scripts are filtered out)
6. **Shell functions** -- user-defined functions enumerated via `zsh -ic 'print -l ${(k)functions}'` at build time (skipped when `disable_build_time_shell_exec` is set)
7. **Zsh completion files** -- subcommand patterns and per-position argument specs from `$fpath` directories, including system paths and plugin-framework trees (Oh-My-Zsh, Prezto, zinit, antidote, antibody, znap, zplug, `~/.config/zsh`)
8. **Fish completion files** -- `.fish` completion files from standard Fish completion directories (`/usr/share/fish/completions`, `~/.config/fish/completions`, etc.)
9. **Bash completion files** -- `complete -F` / `complete -W` stanzas from `/etc/bash_completion.d`, `/usr/share/bash-completion/completions`, and equivalents
10. **carapace-spec YAML** -- three disk locations are scanned on every `build`:
    - `~/.config/carapace/specs/*.yaml` (user-authored)
    - `/usr/share/carapace/specs/*.yaml` (distro packages)
    - `/usr/local/share/carapace/specs/*.yaml` (Homebrew and local installs)

    When the `carapace` binary is also on PATH and `disable_build_time_shell_exec` is off, zsh-ios additionally shells to `carapace _list` to enumerate every builtin completer, then `carapace <cmd> _spec` to dump each as YAML. The dumps cache under `$XDG_CACHE_HOME/zsh-ios/carapace-specs/<cmd>.yaml` keyed by `carapace --version`, so subsequent builds read the cache until `carapace` itself upgrades.

    A third path — for users who do not want to install carapace system-wide — is `zsh-ios carapace-fetch`: it downloads the latest carapace-bin release tarball (via `curl | tar`) into `$XDG_CACHE_HOME/zsh-ios/carapace-bin/` and dumps every builtin completer's YAML into the same `carapace-specs/` directory that `build` already reads. Run it once and the downloaded specs persist across rebuilds; rerun when you want the latest upstream completers.

11. **Fig / withfig/autocomplete** -- 500+ TypeScript specs fetched from [`github.com/withfig/autocomplete`](https://github.com/withfig/autocomplete). The pipeline is a one-time `zsh-ios fig-fetch`:
    - `git clone` (or `git pull`) into `$XDG_CACHE_HOME/zsh-ios/fig-autocomplete/`
    - `pnpm install` + `pnpm build` (falls back to `npm` if `pnpm` isn't installed; both require Node on PATH)
    - a bundled Node scriptlet (`data/fig_dump.js`, embedded via `include_str!`) walks the compiled `build/*.js` specs, replaces JS functions with a `"__FN__"` sentinel so `JSON.stringify` survives, and writes one JSON per spec to `$XDG_CACHE_HOME/zsh-ios/fig-json/<name>.json`

    Every subsequent `zsh-ios rebuild` reads those JSON files via Rust's `serde_json` and folds templates / suggestions / generator scripts into the trie. If `fig-json/` is absent the scan silently returns zero — users who never run `fig-fetch` stay completely dep-free. Rerun `zsh-ios fig-fetch` whenever you want the latest upstream updates.
12. **Project-local manifests** -- `package.json`, `Makefile`, `justfile`, `Cargo.toml`, `pyproject.toml`, `composer.json`, `build.gradle`, `Rakefile`, `Pipfile`, `pnpm-workspace.yaml`, `lerna.json` (scripts and targets resolved by walking up from cwd)
13. **Live worker state** -- on first shell startup the background zpty worker dumps: aliases, galiases, saliases, functions, named dirs, history, dirstack, jobs, commands, parameters, options, widgets, modules, and zstyle settings; all folded into the trie via `zsh-ios ingest`
14. **Runtime resolvers** -- live data queried when `?` / Tab fires: git (`git for-each-ref`, `git log`, `git remote`, `git ls-files`, …), docker (`docker ps`, `docker images`, …), k8s (`kubectl get`), systemd (`systemctl list-units`), tmux (`tmux list-sessions`), package managers (brew, apt, dnf, pacman, npm, pip, cargo), and project scripts from manifests found by walking up from cwd

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
| **Enter** | Resolve abbreviations and execute. If ambiguous, show a keystroke-driven picker (auto-accepts the moment your digits uniquely identify an option; arrow keys and Tab cycle the highlight). |
| **Tab** | First press: expand to longest common prefix and show the candidate list (one per line). Second Tab on the unchanged buffer opens the numbered picker; inside it, Tab cycles the highlight, arrows cycle too, and a number jumps directly. Picking populates the buffer without executing, so you can edit or Enter. |
| **?** | IOS-style help. Show completions for the current position — subcommands, flags with expected argument type, or live argument values (branches, hosts, signals, users, tracked files, …). Falls back through five tiers: Rust resolver → worker complete-word → approximate → correct → expand_alias / history. |
| **Leading `!`** | Bypass zsh-ios entirely — the buffer runs exactly as typed (history expansion, literal-run). |
| **Ghost preview** | As you type, the resolved command appears faintly two spaces after the cursor. Nothing is committed; pressing Enter runs the resolution. |

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
zsh-ios explain "gi br" # Trace step-by-step how an input would resolve
zsh-ios ingest          # Read a sectioned @<kind> state payload from stdin
zsh-ios regex-args-ingest  # Fold a _regex_arguments harvest capture from stdin
zsh-ios preset          # List available config presets
zsh-ios preset power    # Apply the power-user preset (backs up existing config)
zsh-ios preset deterministic --show   # Print preset YAML without writing
zsh-ios fig-fetch       # Clone + build withfig/autocomplete, dump specs to JSON cache
                        # (one-time; requires Node + pnpm/npm; re-run after upstream updates)
zsh-ios carapace-fetch  # Download carapace-bin and dump every builtin completer's YAML spec
                        # (one-time; requires curl + tar; no carapace system install needed)
```

### Debugging resolution

When `zsh-ios` expands something unexpectedly (or fails to), `zsh-ios explain` prints the full decision tree: pin lookup, trie walk per word, deep-disambiguation candidates and winner, arg-spec hits, and the final result. Example:

```
$ zsh-ios explain "gi br"

Command: "gi br"
  Pin lookup: no longest-prefix match
  Trie: "gi" is ambiguous — 5 candidates: gids-tool, gio, git, git-shell, ...
    Deep-disambiguate with next word "br":
      git: branch
      (4 other candidates had no "br" subcommand)
    → winner: git

Final: Resolved → git branch
```

### Pins file

Pins are stored in plain text at `~/Library/Application Support/zsh-ios/pins.txt` (macOS) or `~/.config/zsh-ios/pins.txt` (Linux). Format:

```
g ch -> git checkout
tf -> terraform
k -> kubectl
```

Edit by hand anytime. Longest-prefix match applies: `g ch` takes priority over `g` when the input starts with `g ch`.

### Config file

Optional YAML at `~/.config/zsh-ios/config.yaml` (or `~/Library/Application Support/zsh-ios/config.yaml` on macOS). Every field is optional; missing file or missing field falls back to the compiled-in default. An invalid config prints a warning and uses defaults — it can never wedge the shell.

```yaml
# How many seconds old tree.msgpack can be before the plugin auto-rebuilds
stale_threshold_seconds: 3600

# Set true to make `zsh-ios learn` a no-op
disable_learning: false

# Suppress the ghost-text preview after the cursor
disable_ghost_preview: false

# Skip statistical tiebreaker — ties always surface as a picker
disable_statistics: false

# Commands zsh-ios must never touch (matched literally and on resolution)
command_blocklist:
  - kubectl
  - docker
```

There are 29 config knobs covering behaviour, resolution determinism, privacy/attack surface, performance, retention, and display. See `docs/config.md` for the full reference. Use `zsh-ios preset` as a shortcut to apply a named profile.

Run `zsh-ios status` to see which values are in effect.

### How resolution works

Given input `ter ap --auto-approve`:

1. **Pin check** -- longest-prefix match against saved pins
2. **Global alias expansion** -- any `alias -g` tokens are substituted before the trie walk
3. **Trie walk** -- `ter` prefix-matches `terraform` (unique), then `ap` prefix-matches `apply` (unique)
4. **Deep disambiguation** -- if still ambiguous after the trie walk, subsequent words are used to narrow candidates
5. **Arg-type narrowing** -- positional type evidence (e.g. a directory path favours `cd` over `cat`) narrows candidates further
6. **Flag-match narrowing** -- flags typed on the command line are counted against each candidate's known flag set
7. **Statistical tiebreaker** -- frequency × recency (14-day half-life) × success-rate × cwd-frequency boost × sibling-command boost, with a configurable dominance margin
8. **Flags** -- `--auto-approve` starts with `-`, passed through as-is (flags are never prefix-expanded)
9. **Path resolution** -- arguments are checked against the real filesystem; if a matching file or directory exists, the abbreviation is expanded
10. **Result**: `terraform apply --auto-approve`

The engine is context-aware about what kind of arguments each command and subcommand takes. Arg specs are extracted from Zsh, Fish, Bash, carapace-spec, and Fig completion sources for 2000+ commands (depending on which catalogs you've populated). See `docs/DATA-SOURCES.md` for the full arg-type list and resolver inventory.

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
| **Docker container** | `docker exec`, `docker stop` | `docker ps --all` |
| **k8s pod** | `kubectl exec`, `kubectl logs` | `kubectl get pods` |
| **systemd unit** | `systemctl start`, `systemctl status` | `systemctl list-units` |
| **npm script** | `npm run` | `package.json` scripts section |
| **make target** | `make` | Makefile targets |
| **…and 63 more** | | See `docs/DATA-SOURCES.md` |

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

### Bypass with `!`

A leading `!` means "don't touch this line." Enter, Tab, and `?` all fall through to native Zsh so history expansion (`!!`, `!$`, `!string`) and explicit literal-run semantics work untouched. zsh-ios never resolves, completes, or learns anything about a `!`-prefixed buffer. A `!` that follows a `/` (as in `cd te/!5`) is still the path suffix-match operator — only a leading `!` triggers bypass.

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

## Tests

```bash
cargo test              # Rust unit + integration tests (src/*.rs, tests/cli.rs)
bats tests/plugin/      # Zsh plugin tests (requires bats-core)
```

The bats suite drives `_zsh_ios_handle_ambiguity` and the ZLE widgets directly, using stubs for the `zsh-ios` binary and ZLE primitives — so you can iterate on the picker or a widget without a real interactive shell. The `_ZSH_IOS_TEST_INPUT_FD` hook lets tests feed keystrokes through a file descriptor instead of `/dev/tty`. Install bats with `brew install bats-core` (mac) or `apt install bats` (linux).

## Uninstall

```bash
./uninstall.sh
```

Removes the binary, offers to delete config directories (with confirmation), and cleans `~/.zshrc`.

## Project structure

```
src/
  main.rs               CLI entry point (clap subcommands)
  lib.rs                Re-exports all modules; houses test_util::CWD_LOCK
  trie.rs               Prefix trie, 83 ARG_MODE_* constants, MessagePack serialization
  resolve/              Abbreviation resolution subsystem:
    engine.rs             Core trie walk, deep disambiguation, scoring, explain
    complete.rs           `?` key completion path
    escape.rs             Shell-quoting helpers for resolved paths
  path_resolve.rs       Filesystem path abbreviation; named-dir + dirstack expansion
  runtime_complete.rs   71 runtime resolvers (git, docker, k8s, systemd, tmux,
                        brew, apt, npm, pip, cargo, project scripts, shell state, …)
  completions.rs        Zsh completion file parser (→state, _alternative,
                        _regex_arguments, _values, tag groups, …); ~1400 commands
  bash_completions.rs   Bash completion file parser (complete -F / -W)
  fish_completions.rs   Fish completion file parser (.fish files)
  carapace_completions.rs
                        carapace-spec YAML ingester + `carapace _spec` dumper
  fig_completions.rs    withfig/autocomplete JSON ingester + `zsh-ios fig-fetch`
  scanner.rs            PATH scanner, builtins, alias parser
  history.rs            Zsh history parser
  pins.rs               Pin storage (load/save/match)
  config.rs             Config directory paths (XDG / macOS Application Support)
  user_config.rs        29-knob config.yaml; serde(deny_unknown_fields)
  runtime_config.rs     RuntimeConfig under OnceLock<RwLock>; runtime_config::get()
  type_resolver.rs      TypeResolver trait + Registry
  runtime_cache.rs      On-disk MessagePack TTL cache for resolver results
  galiases.rs           Global-alias buffer rewrite before trie walk
  ingest.rs             `zsh-ios ingest` — sectioned live-state ingest
  locks.rs              Shared fs2 advisory flock helper
  presets.rs            Three built-in YAML presets (deterministic/privacy/power)
  regex_args.rs         _regex_arguments DSL parser + dynamic harvest support
data/
  descriptions.yaml     Fallback subcommand descriptions (bundled at compile time)
  fig_dump.js           Node scriptlet bundled via include_str! — used by
                        `zsh-ios fig-fetch` to convert compiled fig specs to JSON
plugin/
  zsh-ios.zsh           Zsh plugin (ZLE widgets, ghost preview, zpty worker,
                        key bindings, preexec/precmd hooks, context inference)
docs/
  config.md             Full config knob reference (29 fields with examples)
  DATA-SOURCES.md       Exhaustive reference of every data source and resolver
install.sh              Installer
uninstall.sh            Uninstaller
```

## License

AGPL-3.0-only
