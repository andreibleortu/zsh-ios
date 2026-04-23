use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::fs;
use std::path::Path;

/// A single parsed zstyle matcher rule derived from the `matcher-list` setting.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub enum MatcherRule {
    /// Case-insensitive match: fold both input and candidate to lowercase before
    /// comparing. Covers `m:{a-z}={A-Z}`, `m:{A-Z}={a-z}`, and `m:{a-zA-Z}={A-Za-z}`.
    CaseInsensitive,
    /// Match-in-the-middle on separator characters. `PartialOn("._-")` means
    /// `r:|[._-]=*`: the prefix can start a segment delimited by those chars.
    /// `PartialOn("")` means `r:|=*`: any position is a valid anchor.
    PartialOn(String),
    /// A spec form we recognised but chose not to implement; stored verbatim so
    /// callers can count it without silently dropping it.
    Unknown(String),
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct TrieNode {
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub children: BTreeMap<String, TrieNode>,
    #[serde(default, skip_serializing_if = "is_zero_u32")]
    pub count: u32,
    #[serde(default, skip_serializing_if = "is_zero_u32")]
    pub failures: u32,
    #[serde(default, skip_serializing_if = "is_zero_u64")]
    pub last_used: u64,
    #[serde(default, skip_serializing_if = "is_false")]
    pub is_leaf: bool,
    /// Frequency of use per-cwd. Limited to the top 8 cwds to keep the trie
    /// compact; once full, the least-used is evicted on the next distinct cwd.
    /// Populated by `cmd_learn --cwd PATH`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub cwd_hits: Vec<(String, u32)>,
}

fn is_zero_u32(n: &u32) -> bool {
    *n == 0
}
fn is_zero_u64(n: &u64) -> bool {
    *n == 0
}
fn is_false(b: &bool) -> bool {
    !*b
}

impl TrieNode {
    pub fn insert(&mut self, words: &[&str]) {
        self.insert_with_time(words, 0);
    }

    pub fn insert_with_time(&mut self, words: &[&str], unix_ts: u64) {
        if words.is_empty() {
            return;
        }
        let child = self.children.entry(words[0].to_string()).or_default();
        child.count += 1;
        if unix_ts > child.last_used {
            child.last_used = unix_ts;
        }
        if words.len() == 1 {
            child.is_leaf = true;
        }
        if words.len() > 1 {
            child.insert_with_time(&words[1..], unix_ts);
        }
    }

    /// Tally a failed (non-zero-exit) invocation along an existing trie path.
    /// Does NOT create new nodes — a command that doesn't exist in the trie
    /// is ignored (we don't learn junk from failures).
    /// Returns true if all path nodes existed and were tallied; false otherwise.
    pub fn record_failure(&mut self, words: &[&str], unix_ts: u64) -> bool {
        if words.is_empty() {
            return true;
        }
        match self.children.get_mut(words[0]) {
            Some(child) => {
                child.failures += 1;
                if unix_ts > child.last_used {
                    child.last_used = unix_ts;
                }
                if words.len() > 1 {
                    child.record_failure(&words[1..], unix_ts)
                } else {
                    true
                }
            }
            None => false,
        }
    }

    /// Heuristic success rate. Returns `None` if we have no data
    /// (`count == 0 && failures == 0`). Otherwise `count / (count + failures)`.
    pub fn success_rate(&self) -> Option<f32> {
        let total = self.count as u64 + self.failures as u64;
        if total == 0 {
            None
        } else {
            Some(self.count as f32 / total as f32)
        }
    }

    /// Seconds since last recorded use, relative to `now`. `None` if never used.
    pub fn age_seconds(&self, now: u64) -> Option<u64> {
        if self.last_used == 0 {
            None
        } else {
            Some(now.saturating_sub(self.last_used))
        }
    }

    /// Insert a single first-level command (executable name) without incrementing count
    /// from history. Marks it as a leaf so it's discoverable.
    pub fn insert_command(&mut self, name: &str) {
        let child = self.children.entry(name.to_string()).or_default();
        child.is_leaf = true;
    }

    /// Find all children whose names start with the given prefix.
    /// Returns (full_name, &child_node) pairs.
    pub fn prefix_search(&self, prefix: &str) -> Vec<(&str, &TrieNode)> {
        if prefix.is_empty() {
            return self.children.iter().map(|(k, v)| (k.as_str(), v)).collect();
        }
        // Use BTreeMap range for O(log n + m) lookup instead of O(n) full scan
        let start = prefix.to_string();
        self.children
            .range(start..)
            .take_while(|(k, _)| k.starts_with(prefix))
            .map(|(k, v)| (k.as_str(), v))
            .collect()
    }

    /// Like `prefix_search` but honours zstyle matcher rules.
    ///
    /// With no rules (or an empty prefix) it delegates straight to
    /// `prefix_search`.  Otherwise it layers up to two extra passes on top of
    /// the exact-prefix results:
    ///
    /// - `CaseInsensitive`: children whose lowercased name starts with the
    ///   lowercased prefix are added (deduped against the exact-prefix set).
    /// - `PartialOn(charset)`: children that contain the prefix at the start
    ///   of any segment delimited by `charset` characters are added.  An empty
    ///   charset (`r:|=*`) accepts any position as an anchor.
    pub fn matcher_aware_search<'a>(
        &'a self,
        prefix: &str,
        rules: &[MatcherRule],
    ) -> Vec<(&'a str, &'a TrieNode)> {
        if rules.is_empty() || prefix.is_empty() {
            return self.prefix_search(prefix);
        }

        // Always start with exact prefix matches.
        let mut results: Vec<(&'a str, &'a TrieNode)> = self.prefix_search(prefix);
        let exact_names: HashSet<&str> = results.iter().map(|(n, _)| *n).collect();

        // Case-insensitive pass.
        if rules.iter().any(|r| matches!(r, MatcherRule::CaseInsensitive)) {
            let lower = prefix.to_lowercase();
            for (name, node) in &self.children {
                if !exact_names.contains(name.as_str())
                    && name.to_lowercase().starts_with(&lower)
                {
                    results.push((name.as_str(), node));
                }
            }
        }

        // Partial-on-separator pass.
        if let Some(charset) = rules.iter().find_map(|r| match r {
            MatcherRule::PartialOn(cs) => Some(cs.as_str()),
            _ => None,
        }) {
            let sep_chars: Vec<char> = charset.chars().collect();
            for (name, node) in &self.children {
                // Skip names already included by an earlier pass.
                if results.iter().any(|(n, _)| *n == name.as_str()) {
                    continue;
                }
                let hit = if sep_chars.is_empty() {
                    // r:|=* — any position is a valid anchor.
                    name.contains(prefix)
                } else {
                    // r:|[CHARSET]=* — prefix must begin a separator-delimited segment.
                    name.split(|c: char| sep_chars.contains(&c))
                        .any(|seg| seg.starts_with(prefix))
                };
                if hit {
                    results.push((name.as_str(), node));
                }
            }
        }

        results
    }

    /// Exact lookup for a child by name.
    pub fn get_child(&self, name: &str) -> Option<&TrieNode> {
        self.children.get(name)
    }

    /// Total number of distinct first-level entries.
    pub fn len(&self) -> usize {
        self.children.len()
    }

    #[allow(dead_code)]
    pub fn is_empty(&self) -> bool {
        self.children.is_empty()
    }

    /// Record usage in the given cwd. Keeps at most 8 entries; evicts the
    /// least-used when full.
    pub fn record_cwd(&mut self, cwd: &str) {
        const MAX_CWDS: usize = 8;
        if let Some(entry) = self.cwd_hits.iter_mut().find(|(k, _)| k == cwd) {
            entry.1 += 1;
            return;
        }
        if self.cwd_hits.len() >= MAX_CWDS {
            // Evict the entry with the smallest count.
            if let Some(idx) = self
                .cwd_hits
                .iter()
                .enumerate()
                .min_by_key(|(_, (_, c))| *c)
                .map(|(i, _)| i)
            {
                self.cwd_hits.remove(idx);
            }
        }
        self.cwd_hits.push((cwd.to_string(), 1));
    }

    /// Fraction of uses that occurred in `cwd`, 0.0 if never seen there.
    pub fn cwd_score(&self, cwd: &str) -> f32 {
        let hits = self.cwd_hits.iter().find(|(k, _)| k == cwd).map(|(_, c)| *c).unwrap_or(0);
        if hits == 0 {
            return 0.0;
        }
        let total: u32 = self.cwd_hits.iter().map(|(_, c)| c).sum();
        if total == 0 {
            0.0
        } else {
            hits as f32 / total as f32
        }
    }

    /// Check whether `name` is a strict prefix of any existing child.
    /// Used to prevent learning abbreviated junk like "terr" when "terraform" exists.
    /// Uses BTreeMap range for O(log n) instead of O(n) full scan.
    pub fn is_prefix_of_existing(&self, name: &str) -> bool {
        // Range from `name` onward; the first entry >= name is either `name` itself
        // or something that starts with `name` (a longer command).
        self.children
            .range(name.to_string()..)
            .take_while(|(k, _)| k.starts_with(name))
            .any(|(k, _)| k.as_str() != name)
    }
}

/// Argument type constants for positions and flags.
/// Values 1-3 are the original modes; 4+ are extended types from Zsh completions.
pub const ARG_MODE_PATHS: u8 = 1;
pub const ARG_MODE_DIRS_ONLY: u8 = 2;
pub const ARG_MODE_EXECS_ONLY: u8 = 3;
pub const ARG_MODE_USERS: u8 = 4;
pub const ARG_MODE_HOSTS: u8 = 5;
pub const ARG_MODE_PIDS: u8 = 6;
pub const ARG_MODE_SIGNALS: u8 = 7;
pub const ARG_MODE_PORTS: u8 = 8;
pub const ARG_MODE_NET_IFACES: u8 = 9;
pub const ARG_MODE_GIT_BRANCHES: u8 = 10;
pub const ARG_MODE_GIT_TAGS: u8 = 11;
pub const ARG_MODE_GIT_REMOTES: u8 = 12;
pub const ARG_MODE_GIT_FILES: u8 = 13;
pub const ARG_MODE_URLS: u8 = 14;
pub const ARG_MODE_GROUPS: u8 = 15;
pub const ARG_MODE_LOCALES: u8 = 16;
/// Accepts either a user name or a group name (e.g. `chown owner:group`).
pub const ARG_MODE_USERS_GROUPS: u8 = 17;

// Git advanced
pub const ARG_MODE_GIT_STASH: u8 = 18;
pub const ARG_MODE_GIT_WORKTREE: u8 = 19;
pub const ARG_MODE_GIT_SUBMODULE: u8 = 20;
pub const ARG_MODE_GIT_CONFIG_KEY: u8 = 21;
pub const ARG_MODE_GIT_ALIAS: u8 = 22;
pub const ARG_MODE_GIT_COMMIT: u8 = 23;
pub const ARG_MODE_GIT_REFLOG: u8 = 24;

// Docker
pub const ARG_MODE_DOCKER_CONTAINER: u8 = 25;
pub const ARG_MODE_DOCKER_IMAGE: u8 = 26;
pub const ARG_MODE_DOCKER_NETWORK: u8 = 27;
pub const ARG_MODE_DOCKER_VOLUME: u8 = 28;
pub const ARG_MODE_DOCKER_COMPOSE_SERVICE: u8 = 29;

// Kubernetes
pub const ARG_MODE_K8S_CONTEXT: u8 = 30;
pub const ARG_MODE_K8S_NAMESPACE: u8 = 31;
pub const ARG_MODE_K8S_POD: u8 = 32;
pub const ARG_MODE_K8S_DEPLOYMENT: u8 = 33;
pub const ARG_MODE_K8S_SERVICE: u8 = 34;
pub const ARG_MODE_K8S_RESOURCE_KIND: u8 = 35;

// systemd
pub const ARG_MODE_SYSTEMD_UNIT: u8 = 36;
pub const ARG_MODE_SYSTEMD_SERVICE: u8 = 37;
pub const ARG_MODE_SYSTEMD_TIMER: u8 = 38;
pub const ARG_MODE_SYSTEMD_SOCKET: u8 = 39;

// Package managers
pub const ARG_MODE_BREW_FORMULA: u8 = 40;
pub const ARG_MODE_BREW_CASK: u8 = 41;
pub const ARG_MODE_APT_PACKAGE: u8 = 42;
pub const ARG_MODE_DNF_PACKAGE: u8 = 43;
pub const ARG_MODE_PACMAN_PACKAGE: u8 = 44;
pub const ARG_MODE_NPM_PACKAGE: u8 = 45;
pub const ARG_MODE_PIP_PACKAGE: u8 = 46;
pub const ARG_MODE_CARGO_CRATE: u8 = 47;

// Project scripts
pub const ARG_MODE_NPM_SCRIPT: u8 = 48;
pub const ARG_MODE_MAKE_TARGET: u8 = 49;
pub const ARG_MODE_JUST_RECIPE: u8 = 50;
pub const ARG_MODE_CARGO_TASK: u8 = 51;
pub const ARG_MODE_POETRY_SCRIPT: u8 = 52;
pub const ARG_MODE_COMPOSER_SCRIPT: u8 = 53;
pub const ARG_MODE_GRADLE_TASK: u8 = 54;
pub const ARG_MODE_RAKE_TASK: u8 = 55;

// Shell introspection
pub const ARG_MODE_SHELL_FUNCTION: u8 = 56;
pub const ARG_MODE_SHELL_ALIAS: u8 = 57;
pub const ARG_MODE_SHELL_VAR: u8 = 58;
pub const ARG_MODE_NAMED_DIR: u8 = 59;
pub const ARG_MODE_DIRSTACK_ENTRY: u8 = 60;
pub const ARG_MODE_JOB_SPEC: u8 = 61;
pub const ARG_MODE_HISTORY_ENTRY: u8 = 62;

// Session managers
pub const ARG_MODE_TMUX_SESSION: u8 = 63;
pub const ARG_MODE_TMUX_WINDOW: u8 = 64;
pub const ARG_MODE_TMUX_PANE: u8 = 65;
pub const ARG_MODE_SCREEN_SESSION: u8 = 66;

// Text/net types
pub const ARG_MODE_URL_SCHEME: u8 = 67;
pub const ARG_MODE_EMAIL: u8 = 68;
pub const ARG_MODE_IPV4: u8 = 69;
pub const ARG_MODE_IPV6: u8 = 70;
pub const ARG_MODE_MAC_ADDR: u8 = 71;
pub const ARG_MODE_TIMEZONE: u8 = 72;

// Git introspection (extended)
pub const ARG_MODE_GIT_HEAD: u8 = 73;
pub const ARG_MODE_GIT_BISECT: u8 = 74;
pub const ARG_MODE_GIT_REMOTE_REF: u8 = 75;

// Node/Python workspace
pub const ARG_MODE_PNPM_WORKSPACE: u8 = 76;
pub const ARG_MODE_LERNA_PACKAGE: u8 = 77;
pub const ARG_MODE_YARN_WORKSPACE: u8 = 78;
pub const ARG_MODE_PIPENV_SCRIPT: u8 = 79;

/// Returns a short human-readable label for an ARG_MODE_* constant.
/// Returns "?" for unknown values.
pub fn arg_mode_name(mode: u8) -> &'static str {
    match mode {
        ARG_MODE_PATHS => "path",
        ARG_MODE_DIRS_ONLY => "directory",
        ARG_MODE_EXECS_ONLY => "executable",
        ARG_MODE_USERS => "user",
        ARG_MODE_HOSTS => "host",
        ARG_MODE_PIDS => "pid",
        ARG_MODE_SIGNALS => "signal",
        ARG_MODE_PORTS => "port",
        ARG_MODE_NET_IFACES => "interface",
        ARG_MODE_GIT_BRANCHES => "git-branch",
        ARG_MODE_GIT_TAGS => "git-tag",
        ARG_MODE_GIT_REMOTES => "git-remote",
        ARG_MODE_GIT_FILES => "git-file",
        ARG_MODE_URLS => "url",
        ARG_MODE_GROUPS => "group",
        ARG_MODE_LOCALES => "locale",
        ARG_MODE_USERS_GROUPS => "user-or-group",
        ARG_MODE_GIT_STASH => "git-stash",
        ARG_MODE_GIT_WORKTREE => "git-worktree",
        ARG_MODE_GIT_SUBMODULE => "git-submodule",
        ARG_MODE_GIT_CONFIG_KEY => "git-config-key",
        ARG_MODE_GIT_ALIAS => "git-alias",
        ARG_MODE_GIT_COMMIT => "git-commit",
        ARG_MODE_GIT_REFLOG => "git-reflog",
        ARG_MODE_DOCKER_CONTAINER => "docker-container",
        ARG_MODE_DOCKER_IMAGE => "docker-image",
        ARG_MODE_DOCKER_NETWORK => "docker-network",
        ARG_MODE_DOCKER_VOLUME => "docker-volume",
        ARG_MODE_DOCKER_COMPOSE_SERVICE => "docker-compose-service",
        ARG_MODE_K8S_CONTEXT => "k8s-context",
        ARG_MODE_K8S_NAMESPACE => "k8s-namespace",
        ARG_MODE_K8S_POD => "k8s-pod",
        ARG_MODE_K8S_DEPLOYMENT => "k8s-deployment",
        ARG_MODE_K8S_SERVICE => "k8s-service",
        ARG_MODE_K8S_RESOURCE_KIND => "k8s-resource-kind",
        ARG_MODE_SYSTEMD_UNIT => "systemd-unit",
        ARG_MODE_SYSTEMD_SERVICE => "systemd-service",
        ARG_MODE_SYSTEMD_TIMER => "systemd-timer",
        ARG_MODE_SYSTEMD_SOCKET => "systemd-socket",
        ARG_MODE_BREW_FORMULA => "brew-formula",
        ARG_MODE_BREW_CASK => "brew-cask",
        ARG_MODE_APT_PACKAGE => "apt-package",
        ARG_MODE_DNF_PACKAGE => "dnf-package",
        ARG_MODE_PACMAN_PACKAGE => "pacman-package",
        ARG_MODE_NPM_PACKAGE => "npm-package",
        ARG_MODE_PIP_PACKAGE => "pip-package",
        ARG_MODE_CARGO_CRATE => "cargo-crate",
        ARG_MODE_NPM_SCRIPT => "npm-script",
        ARG_MODE_MAKE_TARGET => "make-target",
        ARG_MODE_JUST_RECIPE => "just-recipe",
        ARG_MODE_CARGO_TASK => "cargo-task",
        ARG_MODE_POETRY_SCRIPT => "poetry-script",
        ARG_MODE_COMPOSER_SCRIPT => "composer-script",
        ARG_MODE_GRADLE_TASK => "gradle-task",
        ARG_MODE_RAKE_TASK => "rake-task",
        ARG_MODE_SHELL_FUNCTION => "shell-function",
        ARG_MODE_SHELL_ALIAS => "shell-alias",
        ARG_MODE_SHELL_VAR => "shell-var",
        ARG_MODE_NAMED_DIR => "named-dir",
        ARG_MODE_DIRSTACK_ENTRY => "dirstack-entry",
        ARG_MODE_JOB_SPEC => "job-spec",
        ARG_MODE_HISTORY_ENTRY => "history-entry",
        ARG_MODE_TMUX_SESSION => "tmux-session",
        ARG_MODE_TMUX_WINDOW => "tmux-window",
        ARG_MODE_TMUX_PANE => "tmux-pane",
        ARG_MODE_SCREEN_SESSION => "screen-session",
        ARG_MODE_URL_SCHEME => "url-scheme",
        ARG_MODE_EMAIL => "email",
        ARG_MODE_IPV4 => "ipv4",
        ARG_MODE_IPV6 => "ipv6",
        ARG_MODE_MAC_ADDR => "mac-addr",
        ARG_MODE_TIMEZONE => "timezone",
        ARG_MODE_GIT_HEAD => "git-head",
        ARG_MODE_GIT_BISECT => "git-bisect",
        ARG_MODE_GIT_REMOTE_REF => "git-remote-ref",
        ARG_MODE_PNPM_WORKSPACE => "pnpm-workspace",
        ARG_MODE_LERNA_PACKAGE => "lerna-package",
        ARG_MODE_YARN_WORKSPACE => "yarn-workspace",
        ARG_MODE_PIPENV_SCRIPT => "pipenv-script",
        _ => "?",
    }
}

/// A context-sensitive completion rule evaluated at query time.
///
/// When any flag in `trigger_flags` is already present on the current command
/// line, the completion type for the next positional argument is overridden
/// with `override_type` instead of the default.
///
/// Parsed from Zsh `if [[ -n ${opt_args[(I)-b|-B|...]} ]]; then ACTION`
/// patterns inside `case $state in` arm bodies.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ContextRule {
    /// Any of these flags being present in the current words triggers the rule.
    pub trigger_flags: Vec<String>,
    /// The completion type (ARG_MODE_* constant) to use when triggered.
    pub override_type: u8,
}

/// Per-command argument specification, parsed from Zsh completion files.
/// Knows what type of argument each position and flag expects.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct ArgSpec {
    /// Argument type for specific positions (1-indexed: 1 = first arg after command).
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    pub positional: HashMap<u32, u8>,
    /// Argument type for all remaining/unspecified positions (from `*:...:_files`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rest: Option<u8>,
    /// Flags that consume the next word as a typed argument.
    /// e.g., "-o" → Paths means the word after -o is a file path.
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    pub flag_args: HashMap<String, u8>,
    /// Flags whose value is produced by running an external command.
    /// From Zsh `_call_program` specs: `'-c+:cipher:_call_program ciphers ssh -Q cipher'`.
    /// Maps flag → (tag, argv).  `tag` is the human label; `argv` is run to get completions.
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    pub flag_call_programs: HashMap<String, (String, Vec<String>)>,
    /// Same as `flag_call_programs` but for rest/positional arguments.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rest_call_program: Option<(String, Vec<String>)>,
    /// Flags whose value is a static enumeration of literal strings.
    /// From `compadd - yes no`, `_values 'mode' fast slow`, etc.
    /// Maps flag → sorted deduplicated completion items.
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    pub flag_static_lists: HashMap<String, Vec<String>>,
    /// Same as `flag_static_lists` but for rest/positional arguments.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rest_static_list: Option<Vec<String>>,
    /// Context-sensitive rules: when certain flags are present in the current
    /// command line, override what we complete for the next positional argument.
    /// Evaluated at completion time by checking the typed words.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub context_rules: Vec<ContextRule>,
    /// Flag alias groups: each inner Vec holds flags that refer to the same
    /// underlying option (e.g. `["-f", "--force"]`). Parsed from brace-expanded
    /// `{-f,--force}` forms in `_arguments` specs.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub flag_aliases: Vec<Vec<String>>,
    /// Flag exclusion groups: each inner Vec is a list of flags that are
    /// mutually exclusive. Parsed from the `(flag1 flag2)` prefix of
    /// `_arguments` spec strings.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub flag_exclusions: Vec<Vec<String>>,
}

impl ArgSpec {
    /// Get the argument type for a given position (1-indexed).
    pub fn type_at(&self, position: u32) -> Option<u8> {
        self.positional.get(&position).copied().or(self.rest)
    }

    /// Get the argument type expected after a flag.
    /// Also checks the flag with trailing `=` stripped (e.g., `--output=` → `--output`),
    /// then falls back to checking alias groups so `-f` resolves when only `--force` is stored.
    pub fn type_after_flag(&self, flag: &str) -> Option<u8> {
        if let Some(&t) = self.flag_args.get(flag) {
            return Some(t);
        }
        let stripped = flag.trim_end_matches('=');
        if stripped != flag && let Some(&t) = self.flag_args.get(stripped) {
            return Some(t);
        }
        // Alias lookup: if this flag is in an alias group, retry with each sibling.
        let canonical = self.canonical_flag(flag);
        if canonical != flag {
            if let Some(&t) = self.flag_args.get(canonical) {
                return Some(t);
            }
            let can_stripped = canonical.trim_end_matches('=');
            if can_stripped != canonical && let Some(&t) = self.flag_args.get(can_stripped) {
                return Some(t);
            }
        }
        // Try every sibling in the alias group.
        for group in &self.flag_aliases {
            if group.iter().any(|g| g == flag || g == stripped) {
                for sibling in group {
                    if sibling != flag && let Some(&t) = self.flag_args.get(sibling) {
                        return Some(t);
                    }
                }
            }
        }
        None
    }

    /// Return the canonical (first) flag name for any alias.
    /// Used by disambiguation engines to normalise flags before counting hits.
    pub fn canonical_flag<'a>(&'a self, flag: &'a str) -> &'a str {
        for group in &self.flag_aliases {
            if group.iter().any(|g| g == flag) {
                return group.first().map(String::as_str).unwrap_or(flag);
            }
        }
        flag
    }

    /// Convenience: is this spec non-empty?
    pub fn is_empty(&self) -> bool {
        self.positional.is_empty()
            && self.rest.is_none()
            && self.flag_args.is_empty()
            && self.flag_call_programs.is_empty()
            && self.rest_call_program.is_none()
            && self.flag_static_lists.is_empty()
            && self.rest_static_list.is_none()
            && self.context_rules.is_empty()
            && self.flag_aliases.is_empty()
            && self.flag_exclusions.is_empty()
    }

    /// Whether a flag consumes the next word (either via typed arg, call_program, or static list).
    /// Checks aliases so `-f` matches when only `--force` is stored.
    pub fn flag_takes_value(&self, flag: &str) -> bool {
        if self.type_after_flag(flag).is_some() {
            return true;
        }
        if self.flag_call_programs.contains_key(flag) || self.flag_static_lists.contains_key(flag) {
            return true;
        }
        // Check aliases.
        for group in &self.flag_aliases {
            if group.iter().any(|g| g == flag) {
                for sibling in group {
                    if self.flag_call_programs.contains_key(sibling)
                        || self.flag_static_lists.contains_key(sibling)
                    {
                        return true;
                    }
                }
            }
        }
        false
    }

    /// Merge another `ArgSpec` into this one (pure gap-fill).
    /// Only slots that are completely absent in `self` are filled from `other`;
    /// any existing value — even a generic one — is preserved.  This ensures
    /// that the primary function's explicit specs always take precedence over
    /// what a helper function infers.
    pub fn merge(&mut self, other: &ArgSpec) {
        for (&pos, &arg_type) in &other.positional {
            self.positional.entry(pos).or_insert(arg_type);
        }
        if self.rest.is_none() {
            self.rest = other.rest;
        }
        for (flag, arg_type) in &other.flag_args {
            self.flag_args.entry(flag.clone()).or_insert(*arg_type);
        }
        for (flag, entry) in &other.flag_call_programs {
            self.flag_call_programs
                .entry(flag.clone())
                .or_insert_with(|| entry.clone());
        }
        if self.rest_call_program.is_none() {
            self.rest_call_program = other.rest_call_program.clone();
        }
        for (flag, list) in &other.flag_static_lists {
            self.flag_static_lists
                .entry(flag.clone())
                .or_insert_with(|| list.clone());
        }
        if self.rest_static_list.is_none() {
            self.rest_static_list = other.rest_static_list.clone();
        }
        // Gap-fill context rules: add any rules from other whose trigger_flags
        // are not already covered by an existing rule in self.
        for other_rule in &other.context_rules {
            let already_covered = self.context_rules.iter().any(|r| {
                r.trigger_flags
                    .iter()
                    .any(|f| other_rule.trigger_flags.contains(f))
            });
            if !already_covered {
                self.context_rules.push(other_rule.clone());
            }
        }
        // Gap-fill alias groups: only add groups whose flags are not yet represented.
        'outer_alias: for other_group in &other.flag_aliases {
            for self_group in &self.flag_aliases {
                if other_group.iter().any(|f| self_group.contains(f)) {
                    continue 'outer_alias;
                }
            }
            self.flag_aliases.push(other_group.clone());
        }
        // Gap-fill exclusion groups.
        'outer_excl: for other_excl in &other.flag_exclusions {
            for self_excl in &self.flag_exclusions {
                if other_excl.iter().any(|f| self_excl.contains(f)) {
                    continue 'outer_excl;
                }
            }
            self.flag_exclusions.push(other_excl.clone());
        }
    }
}

/// Maps command path (e.g., "git add", "cp") to its argument spec.
pub type ArgSpecMap = HashMap<String, ArgSpec>;

/// Legacy flat map kept for backward compat during deserialization.
pub type ArgModeMap = HashMap<String, u8>;

/// Maps parent command (e.g., "git", "docker compose") to
/// subcommand -> description pairs for IOS-style `?` help.
pub type DescriptionMap = HashMap<String, HashMap<String, String>>;

pub const TREE_SCHEMA_VERSION: u32 = 2;

/// The full command trie with serialization.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct CommandTrie {
    pub root: TrieNode,
    /// Per-position argument specs from Zsh completion files.
    #[serde(default)]
    pub arg_specs: ArgSpecMap,
    /// Legacy flat arg modes (kept for backward compat with old tree files).
    #[serde(default)]
    pub arg_modes: ArgModeMap,
    /// Subcommand descriptions for IOS-style `?` help.
    /// Key = parent command (e.g. "git"), value = subcommand -> description.
    #[serde(default)]
    pub descriptions: DescriptionMap,
    /// Schema version stamped at save time. 0 means pre-versioned (legacy).
    #[serde(default)]
    pub schema_version: u32,
    /// User-defined named directories from `hash -d` (or .zshrc equivalents).
    /// Maps name -> absolute path.  Populated by `zsh-ios ingest` from the
    /// plugin worker's `hash -d` dump.
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    pub named_dirs: HashMap<String, String>,
    /// Directory stack, PWD first, pushd entries after. Consulted by
    /// path_resolve for `~N` references. Populated by `zsh-ios ingest` via
    /// the plugin worker's `dirstack` dump.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub dir_stack: Vec<String>,
    /// Live-state metadata captured by the worker ingest. Key = section
    /// name (jobs, parameters, options, widgets, modules, commands),
    /// value = the raw dump body. Purely informational — surfaced in
    /// `zsh-ios status` for introspection, not consumed by resolution.
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    pub live_state: HashMap<String, String>,
    /// Global aliases from `alias -g NAME=VALUE`. Substituted anywhere on
    /// the command line (not just at command position) by `resolve_line`
    /// before the trie walk.  Ingested from the plugin worker's
    /// `dump-galiases` request.
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    pub galiases: HashMap<String, String>,
    /// Parsed zstyle matcher-list rules from the `zstyle` live-state dump.
    /// Empty = use the default strict prefix matcher.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub matcher_rules: Vec<MatcherRule>,
}

impl CommandTrie {
    pub fn new() -> Self {
        Self {
            schema_version: TREE_SCHEMA_VERSION,
            ..Default::default()
        }
    }

    pub fn insert(&mut self, words: &[&str]) {
        self.root.insert(words);
    }

    pub fn insert_command(&mut self, name: &str) {
        self.root.insert_command(name);
    }

    /// Serialize to MessagePack and write to file atomically.
    /// Writes to a sibling tempfile and renames into place so concurrent
    /// `learn` processes (spawned in the background by the Zsh plugin)
    /// cannot observe or produce a truncated file.
    pub fn save(&self, path: &Path) -> Result<(), Box<dyn std::error::Error>> {
        let mut t = self.clone();
        t.schema_version = TREE_SCHEMA_VERSION;
        let data = rmp_serde::to_vec_named(&t)?;
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        let tmp = match path.file_name() {
            Some(name) => {
                let mut s = name.to_os_string();
                s.push(format!(".tmp.{}", std::process::id()));
                path.with_file_name(s)
            }
            None => return Err("invalid tree path".into()),
        };
        fs::write(&tmp, data)?;
        // rename is atomic on the same filesystem on Unix.
        if let Err(e) = fs::rename(&tmp, path) {
            let _ = fs::remove_file(&tmp);
            return Err(Box::new(e));
        }
        Ok(())
    }

    /// Load from MessagePack file.
    // TODO: check schema_version and trigger rebuild on mismatch (future phase).
    pub fn load(path: &Path) -> Result<Self, Box<dyn std::error::Error>> {
        let data = fs::read(path)?;
        let trie: Self = rmp_serde::from_slice(&data)?;
        Ok(trie)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_insert_and_search() {
        let mut trie = CommandTrie::new();
        trie.insert(&["git", "checkout", "main"]);
        trie.insert(&["git", "commit", "-m"]);
        trie.insert(&["grep", "-r", "pattern"]);

        let matches = trie.root.prefix_search("gi");
        assert_eq!(matches.len(), 1);
        assert_eq!(matches[0].0, "git");

        let matches = trie.root.prefix_search("g");
        assert_eq!(matches.len(), 2); // git, grep

        let git_node = trie.root.get_child("git").unwrap();
        let matches = git_node.prefix_search("ch");
        assert_eq!(matches.len(), 1);
        assert_eq!(matches[0].0, "checkout");

        let matches = git_node.prefix_search("c");
        assert_eq!(matches.len(), 2); // checkout, commit
    }

    #[test]
    fn test_insert_command() {
        let mut trie = CommandTrie::new();
        trie.insert_command("terraform");
        trie.insert_command("telnet");

        let matches = trie.root.prefix_search("ter");
        assert_eq!(matches.len(), 1);
        assert_eq!(matches[0].0, "terraform");

        let matches = trie.root.prefix_search("te");
        assert_eq!(matches.len(), 2);
    }

    #[test]
    fn test_serialize_roundtrip() {
        let mut trie = CommandTrie::new();
        trie.insert(&["git", "checkout"]);
        trie.insert(&["terraform", "apply"]);

        let data = rmp_serde::to_vec_named(&trie).unwrap();
        let loaded: CommandTrie = rmp_serde::from_slice(&data).unwrap();

        assert_eq!(loaded.root.children.len(), 2);
        assert!(loaded.root.get_child("git").is_some());
        assert!(loaded.root.get_child("terraform").is_some());
    }

    #[test]
    fn test_is_prefix_of_existing() {
        let mut trie = CommandTrie::new();
        trie.insert_command("terraform");
        trie.insert_command("telnet");

        assert!(trie.root.is_prefix_of_existing("terr")); // prefix of "terraform"
        assert!(trie.root.is_prefix_of_existing("te")); // prefix of both
        assert!(!trie.root.is_prefix_of_existing("terraform")); // exact, not strict prefix
        assert!(!trie.root.is_prefix_of_existing("xyz")); // prefix of nothing
    }

    #[test]
    fn test_insert_empty_is_noop() {
        let mut trie = CommandTrie::new();
        trie.insert(&[]);
        assert_eq!(trie.root.children.len(), 0);
    }

    #[test]
    fn test_prefix_search_empty_prefix_returns_all() {
        let mut trie = CommandTrie::new();
        trie.insert_command("alpha");
        trie.insert_command("beta");
        trie.insert_command("gamma");
        let all = trie.root.prefix_search("");
        assert_eq!(all.len(), 3);
    }

    #[test]
    fn test_insert_marks_terminal_not_intermediate() {
        let mut trie = CommandTrie::new();
        trie.insert(&["git", "checkout"]);
        let git = trie.root.get_child("git").unwrap();
        assert!(!git.is_leaf, "intermediate 'git' should not be marked leaf by bare insert");
        let checkout = git.get_child("checkout").unwrap();
        assert!(checkout.is_leaf);
    }

    #[test]
    fn test_insert_command_then_subcommand() {
        let mut trie = CommandTrie::new();
        trie.insert_command("git");
        trie.insert(&["git", "checkout"]);
        let git = trie.root.get_child("git").unwrap();
        assert!(git.is_leaf, "insert_command marks top-level leaf");
        assert!(git.get_child("checkout").unwrap().is_leaf);
    }

    #[test]
    fn test_insert_increments_count() {
        let mut trie = CommandTrie::new();
        trie.insert(&["foo"]);
        trie.insert(&["foo"]);
        trie.insert(&["foo"]);
        assert_eq!(trie.root.get_child("foo").unwrap().count, 3);
    }

    #[test]
    fn test_arg_spec_type_at_positional_and_rest() {
        let mut spec = ArgSpec::default();
        spec.positional.insert(1, ARG_MODE_EXECS_ONLY);
        spec.rest = Some(ARG_MODE_PATHS);
        assert_eq!(spec.type_at(1), Some(ARG_MODE_EXECS_ONLY));
        // Falls back to `rest` for unspecified positions.
        assert_eq!(spec.type_at(2), Some(ARG_MODE_PATHS));
    }

    #[test]
    fn test_arg_spec_type_after_flag_with_equals() {
        let mut spec = ArgSpec::default();
        spec.flag_args.insert("--output".into(), ARG_MODE_PATHS);
        assert_eq!(spec.type_after_flag("--output"), Some(ARG_MODE_PATHS));
        // `--output=` should strip the trailing `=` and match.
        assert_eq!(spec.type_after_flag("--output="), Some(ARG_MODE_PATHS));
        assert_eq!(spec.type_after_flag("--unknown"), None);
    }

    #[test]
    fn test_arg_spec_flag_takes_value_sources() {
        let mut spec = ArgSpec::default();
        spec.flag_args.insert("-a".into(), ARG_MODE_PATHS);
        spec.flag_call_programs
            .insert("-b".into(), ("tag".into(), vec!["echo".into()]));
        spec.flag_static_lists.insert("-c".into(), vec!["one".into()]);
        assert!(spec.flag_takes_value("-a"));
        assert!(spec.flag_takes_value("-b"));
        assert!(spec.flag_takes_value("-c"));
        assert!(!spec.flag_takes_value("-z"));
    }

    #[test]
    fn test_arg_spec_is_empty() {
        let spec = ArgSpec::default();
        assert!(spec.is_empty());
        let spec = ArgSpec {
            rest: Some(ARG_MODE_PATHS),
            ..Default::default()
        };
        assert!(!spec.is_empty());
    }

    #[test]
    fn test_arg_spec_merge_gap_fills_only() {
        let mut a = ArgSpec {
            rest: Some(ARG_MODE_PATHS),
            ..Default::default()
        };
        a.flag_args.insert("-x".into(), ARG_MODE_HOSTS);

        let mut b = ArgSpec {
            rest: Some(ARG_MODE_DIRS_ONLY), // should be ignored (a has rest)
            ..Default::default()
        };
        b.flag_args.insert("-x".into(), ARG_MODE_USERS); // ignored
        b.flag_args.insert("-y".into(), ARG_MODE_GROUPS); // filled
        b.positional.insert(1, ARG_MODE_EXECS_ONLY); // filled (a had none)

        a.merge(&b);
        assert_eq!(a.rest, Some(ARG_MODE_PATHS));
        assert_eq!(a.flag_args.get("-x"), Some(&ARG_MODE_HOSTS));
        assert_eq!(a.flag_args.get("-y"), Some(&ARG_MODE_GROUPS));
        assert_eq!(a.positional.get(&1), Some(&ARG_MODE_EXECS_ONLY));
    }

    #[test]
    fn test_arg_spec_merge_context_rules() {
        let mut a = ArgSpec {
            context_rules: vec![ContextRule {
                trigger_flags: vec!["-b".into()],
                override_type: ARG_MODE_GIT_BRANCHES,
            }],
            ..Default::default()
        };
        let b = ArgSpec {
            context_rules: vec![
                ContextRule {
                    trigger_flags: vec!["-b".into()],
                    override_type: ARG_MODE_HOSTS,
                }, // duplicate trigger → dropped
                ContextRule {
                    trigger_flags: vec!["-u".into()],
                    override_type: ARG_MODE_USERS,
                }, // unique trigger → kept
            ],
            ..Default::default()
        };
        a.merge(&b);
        assert_eq!(a.context_rules.len(), 2);
        assert_eq!(a.context_rules[0].override_type, ARG_MODE_GIT_BRANCHES);
        assert_eq!(a.context_rules[1].override_type, ARG_MODE_USERS);
    }

    #[test]
    fn save_writes_atomically_and_round_trips() {
        let td = tempfile::tempdir().unwrap();
        let path = td.path().join("nested").join("tree.msgpack");

        let mut trie = CommandTrie::new();
        trie.insert(&["git", "checkout"]);
        trie.arg_modes.insert("cat".into(), ARG_MODE_PATHS);

        trie.save(&path).expect("save");
        // Atomic rename leaves no .tmp file behind.
        let parent = path.parent().unwrap();
        let leftovers: Vec<_> = std::fs::read_dir(parent)
            .unwrap()
            .filter_map(|e| e.ok())
            .filter(|e| e.file_name().to_string_lossy().contains(".tmp"))
            .collect();
        assert!(leftovers.is_empty(), "tempfile leaked: {:?}", leftovers);

        let loaded = CommandTrie::load(&path).expect("load");
        assert!(loaded.root.get_child("git").is_some());
        assert_eq!(loaded.arg_modes.get("cat"), Some(&ARG_MODE_PATHS));
    }

    #[test]
    fn load_errors_on_missing_file() {
        let td = tempfile::tempdir().unwrap();
        let err = CommandTrie::load(&td.path().join("does-not-exist.msgpack"));
        assert!(err.is_err());
    }

    #[test]
    fn load_errors_on_garbage() {
        let td = tempfile::tempdir().unwrap();
        let p = td.path().join("garbage.msgpack");
        std::fs::write(&p, b"not messagepack").unwrap();
        assert!(CommandTrie::load(&p).is_err());
    }

    #[test]
    fn save_overwrites_existing_file() {
        let td = tempfile::tempdir().unwrap();
        let path = td.path().join("t.msgpack");

        let mut t1 = CommandTrie::new();
        t1.insert_command("first");
        t1.save(&path).unwrap();

        let mut t2 = CommandTrie::new();
        t2.insert_command("second");
        t2.save(&path).unwrap();

        let loaded = CommandTrie::load(&path).unwrap();
        assert!(loaded.root.get_child("second").is_some());
        assert!(loaded.root.get_child("first").is_none());
    }

    #[test]
    fn arg_mode_name_covers_all_modes() {
        for mode in 1u8..=72 {
            assert_ne!(
                arg_mode_name(mode),
                "?",
                "arg_mode_name returned '?' for mode {mode}"
            );
        }
    }

    #[test]
    fn arg_mode_name_unknown_returns_placeholder() {
        assert_eq!(arg_mode_name(0), "?");
        assert_eq!(arg_mode_name(200), "?");
    }

    #[test]
    fn arg_mode_names_unique() {
        use std::collections::HashSet;
        let labels: Vec<&str> = (1u8..=72).map(arg_mode_name).collect();
        let unique: HashSet<&str> = labels.iter().copied().collect();
        assert_eq!(
            unique.len(),
            labels.len(),
            "duplicate label found among modes 1..=72"
        );
    }

    #[test]
    fn insert_does_not_set_last_used_without_ts() {
        let mut root = TrieNode::default();
        root.insert(&["git"]);
        assert_eq!(root.children["git"].last_used, 0);
    }

    #[test]
    fn insert_with_time_sets_last_used() {
        let mut root = TrieNode::default();
        root.insert_with_time(&["git"], 12345);
        assert_eq!(root.children["git"].last_used, 12345);
    }

    #[test]
    fn insert_with_time_keeps_max_ts() {
        let mut root = TrieNode::default();
        root.insert_with_time(&["git"], 100);
        root.insert_with_time(&["git"], 50);
        assert_eq!(root.children["git"].last_used, 100);
    }

    #[test]
    fn record_failure_increments_existing() {
        let mut root = TrieNode::default();
        root.insert(&["git", "push"]);
        let ok = root.record_failure(&["git", "push"], 500);
        assert!(ok);
        let git = &root.children["git"];
        assert_eq!(git.failures, 1);
        assert_eq!(git.last_used, 500);
        let push = &git.children["push"];
        assert_eq!(push.failures, 1);
        assert_eq!(push.last_used, 500);
    }

    #[test]
    fn record_failure_missing_returns_false() {
        let mut root = TrieNode::default();
        let ok = root.record_failure(&["nope", "there"], 0);
        assert!(!ok);
    }

    #[test]
    fn record_failure_partial_path_returns_false() {
        let mut root = TrieNode::default();
        root.insert(&["git"]);
        let ok = root.record_failure(&["git", "notasub"], 0);
        assert!(!ok);
        let git = &root.children["git"];
        assert_eq!(git.count, 1);
        assert_eq!(git.failures, 1);
    }

    #[test]
    fn success_rate_handles_empty() {
        let node = TrieNode::default();
        assert_eq!(node.success_rate(), None);
    }

    #[test]
    fn success_rate_zero_failures_is_one() {
        let node = TrieNode {
            count: 5,
            ..Default::default()
        };
        assert_eq!(node.success_rate(), Some(1.0));
    }

    #[test]
    fn success_rate_mixed() {
        let node = TrieNode {
            count: 3,
            failures: 1,
            ..Default::default()
        };
        assert_eq!(node.success_rate(), Some(0.75));
    }

    #[test]
    fn age_seconds_never_used() {
        let node = TrieNode::default();
        assert_eq!(node.age_seconds(9999), None);
    }

    #[test]
    fn age_seconds_basic() {
        let node = TrieNode {
            last_used: 100,
            ..Default::default()
        };
        assert_eq!(node.age_seconds(150), Some(50));
        assert_eq!(node.age_seconds(50), Some(0));
    }

    #[test]
    fn serde_roundtrip_preserves_new_fields() {
        let mut root = TrieNode::default();
        root.insert_with_time(&["git", "push"], 9999);
        root.record_failure(&["git", "push"], 8000);

        let data = rmp_serde::to_vec_named(&root).unwrap();
        let loaded: TrieNode = rmp_serde::from_slice(&data).unwrap();

        let git = &loaded.children["git"];
        assert_eq!(git.last_used, 9999);
        assert_eq!(git.failures, 1);
        let push = &git.children["push"];
        assert_eq!(push.last_used, 9999);
        assert_eq!(push.failures, 1);
    }

    #[test]
    fn old_tree_deserializes_with_zero_defaults() {
        use serde::Serialize;

        #[derive(Serialize)]
        struct LegacyNode {
            children: BTreeMap<String, LegacyNode>,
            count: u32,
            is_leaf: bool,
        }

        let legacy = LegacyNode {
            children: BTreeMap::new(),
            count: 7,
            is_leaf: true,
        };
        let mut buf = Vec::new();
        let mut se = rmp_serde::Serializer::new(&mut buf).with_struct_map();
        legacy.serialize(&mut se).unwrap();

        let node: TrieNode = rmp_serde::from_slice(&buf).unwrap();
        assert_eq!(node.count, 7);
        assert!(node.is_leaf);
        assert_eq!(node.failures, 0);
        assert_eq!(node.last_used, 0);
    }

    #[test]
    fn schema_version_stamped_on_save() {
        let td = tempfile::tempdir().unwrap();
        let path = td.path().join("tree.msgpack");

        let mut trie = CommandTrie::new();
        trie.schema_version = 0;
        trie.save(&path).unwrap();

        let loaded = CommandTrie::load(&path).unwrap();
        assert_eq!(loaded.schema_version, TREE_SCHEMA_VERSION);
    }

    #[test]
    fn old_tree_deserializes_with_default_version() {
        // Serialize a struct without the schema_version field, simulating a tree
        // written before versioning was added. #[serde(default)] should produce
        // schema_version == 0 on deserialization.
        use serde::Serialize;
        use std::collections::HashMap;

        #[derive(Serialize)]
        struct OldTree {
            root: TrieNode,
            arg_specs: ArgSpecMap,
            arg_modes: ArgModeMap,
            descriptions: DescriptionMap,
        }

        let mut ser_buf = Vec::new();
        let mut se = rmp_serde::Serializer::new(&mut ser_buf).with_struct_map();
        OldTree {
            root: TrieNode::default(),
            arg_specs: HashMap::new(),
            arg_modes: HashMap::new(),
            descriptions: HashMap::new(),
        }
        .serialize(&mut se)
        .unwrap();

        let loaded: CommandTrie = rmp_serde::from_slice(&ser_buf).unwrap();
        assert_eq!(
            loaded.schema_version, 0,
            "missing field should default to 0"
        );
    }

    #[test]
    fn canonical_flag_returns_primary() {
        let mut spec = ArgSpec::default();
        spec.flag_aliases.push(vec!["-f".to_string(), "--force".to_string()]);
        assert_eq!(spec.canonical_flag("-f"), "-f");
        assert_eq!(spec.canonical_flag("--force"), "-f");
        assert_eq!(spec.canonical_flag("--other"), "--other");
    }

    #[test]
    fn type_after_flag_resolves_via_alias() {
        let mut spec = ArgSpec::default();
        spec.flag_args.insert("-f".to_string(), ARG_MODE_PATHS);
        spec.flag_aliases.push(vec!["-f".to_string(), "--file".to_string()]);
        // Direct hit
        assert_eq!(spec.type_after_flag("-f"), Some(ARG_MODE_PATHS));
        // Via alias group
        assert_eq!(spec.type_after_flag("--file"), Some(ARG_MODE_PATHS));
    }

    #[test]
    fn flag_takes_value_respects_aliases() {
        let mut spec = ArgSpec::default();
        spec.flag_args.insert("--force".to_string(), ARG_MODE_PATHS);
        spec.flag_aliases.push(vec!["-f".to_string(), "--force".to_string()]);
        assert!(spec.flag_takes_value("-f"), "-f should take value via alias");
        assert!(spec.flag_takes_value("--force"), "--force should take value directly");
    }

    #[test]
    fn arg_spec_is_empty_respects_new_fields() {
        let mut spec = ArgSpec::default();
        assert!(spec.is_empty());
        spec.flag_aliases.push(vec!["-f".to_string(), "--force".to_string()]);
        assert!(!spec.is_empty());

        let mut spec2 = ArgSpec::default();
        spec2.flag_exclusions.push(vec!["-a".to_string(), "-v".to_string()]);
        assert!(!spec2.is_empty());
    }

    #[test]
    fn merge_gap_fills_aliases_and_exclusions() {
        let mut a = ArgSpec::default();
        a.flag_aliases.push(vec!["-f".to_string(), "--force".to_string()]);
        a.flag_exclusions.push(vec!["-a".to_string(), "-v".to_string()]);

        let mut b = ArgSpec::default();
        // Overlapping alias group — should not be duplicated
        b.flag_aliases.push(vec!["-f".to_string(), "--file".to_string()]);
        // New alias group
        b.flag_aliases.push(vec!["-q".to_string(), "--quiet".to_string()]);
        // Overlapping exclusion — should not be duplicated
        b.flag_exclusions.push(vec!["-a".to_string(), "-b".to_string()]);
        // New exclusion
        b.flag_exclusions.push(vec!["-x".to_string(), "-y".to_string()]);

        a.merge(&b);
        // The overlapping alias group from b (containing -f) should be dropped
        assert_eq!(a.flag_aliases.len(), 2);
        assert!(a.flag_aliases.iter().any(|g| g.contains(&"-q".to_string())));
        // The overlapping exclusion group from b (containing -a) should be dropped
        assert_eq!(a.flag_exclusions.len(), 2);
        assert!(a.flag_exclusions.iter().any(|g| g.contains(&"-x".to_string())));
    }

    #[test]
    fn record_cwd_inserts_and_bumps() {
        let mut node = TrieNode::default();
        node.record_cwd("/home/user/proj");
        assert_eq!(node.cwd_hits.len(), 1);
        assert_eq!(node.cwd_hits[0], ("/home/user/proj".to_string(), 1));

        node.record_cwd("/home/user/proj");
        assert_eq!(node.cwd_hits.len(), 1);
        assert_eq!(node.cwd_hits[0].1, 2);

        node.record_cwd("/tmp");
        assert_eq!(node.cwd_hits.len(), 2);
    }

    #[test]
    fn record_cwd_evicts_when_full() {
        let mut node = TrieNode::default();
        // Fill to 8 entries with counts 1..=8 in distinct cwds
        for i in 1u32..=8 {
            let path = format!("/dir/{}", i);
            for _ in 0..i {
                node.record_cwd(&path);
            }
        }
        assert_eq!(node.cwd_hits.len(), 8);

        // Insert a 9th — the entry with count 1 (/dir/1) must be evicted.
        node.record_cwd("/dir/new");
        assert_eq!(node.cwd_hits.len(), 8);
        // /dir/1 had the smallest count and should be gone.
        assert!(!node.cwd_hits.iter().any(|(k, _)| k == "/dir/1"));
        // The new entry should be present.
        assert!(node.cwd_hits.iter().any(|(k, c)| k == "/dir/new" && *c == 1));
    }

    #[test]
    fn cwd_score_zero_when_unseen() {
        let node = TrieNode::default();
        assert_eq!(node.cwd_score("/anywhere"), 0.0);
    }

    #[test]
    fn cwd_score_fraction_of_total() {
        let mut node = TrieNode::default();
        // 3 uses in /a, 1 use in /b → /a should score 0.75
        node.record_cwd("/a");
        node.record_cwd("/a");
        node.record_cwd("/a");
        node.record_cwd("/b");
        let score_a = node.cwd_score("/a");
        assert!((score_a - 0.75).abs() < 1e-6, "expected 0.75 got {}", score_a);
        let score_b = node.cwd_score("/b");
        assert!((score_b - 0.25).abs() < 1e-6, "expected 0.25 got {}", score_b);
        assert_eq!(node.cwd_score("/unseen"), 0.0);
    }

    // ── matcher_aware_search ──────────────────────────────────────────────────

    #[test]
    fn matcher_aware_search_empty_rules_behaves_like_prefix() {
        let mut trie = CommandTrie::new();
        trie.insert_command("git");
        trie.insert_command("grep");
        // No rules → identical to prefix_search.
        let res = trie.root.matcher_aware_search("gi", &[]);
        assert_eq!(res.len(), 1);
        assert_eq!(res[0].0, "git");
    }

    #[test]
    fn matcher_aware_search_case_insensitive() {
        let mut trie = CommandTrie::new();
        trie.insert_command("Git");
        trie.insert_command("grep");
        let rules = vec![MatcherRule::CaseInsensitive];
        // "gi" (lower) should fold-match "Git" (mixed case).
        let res = trie.root.matcher_aware_search("gi", &rules);
        let names: Vec<&str> = res.iter().map(|(n, _)| *n).collect();
        assert!(names.contains(&"Git"), "expected 'Git' in {:?}", names);
        // "grep" does not start with "gi" in any case folding.
        assert!(!names.contains(&"grep"));
    }

    #[test]
    fn matcher_aware_search_partial_on_separator() {
        let mut trie = CommandTrie::new();
        trie.insert_command("web-server");
        trie.insert_command("db-backup");
        let rules = vec![MatcherRule::PartialOn("-".to_string())];
        // "server" starts the second segment of "web-server".
        let res = trie.root.matcher_aware_search("server", &rules);
        let names: Vec<&str> = res.iter().map(|(n, _)| *n).collect();
        assert!(names.contains(&"web-server"), "expected 'web-server' in {:?}", names);
        assert!(!names.contains(&"db-backup"));
    }

    #[test]
    fn matcher_aware_search_unknown_rule_falls_back() {
        let mut trie = CommandTrie::new();
        trie.insert_command("git");
        trie.insert_command("grep");
        // An Unknown rule alone should not add any extra matches beyond prefix_search.
        let rules = vec![MatcherRule::Unknown("b:=*".to_string())];
        let res = trie.root.matcher_aware_search("gi", &rules);
        assert_eq!(res.len(), 1);
        assert_eq!(res[0].0, "git");
    }
}
