//! Runtime tuning values threaded in from `UserConfig` at CLI entry.
//!
//! Callers get a cheap clone via [`get()`]; the writer holds a `RwLock` briefly.
//! All values have compiled-in defaults so callers that never call [`set()`]
//! (unit tests, benchmarks) see sensible behaviour.

use std::collections::HashMap;
use std::sync::{OnceLock, RwLock};

/// Tuning knobs derived from the user's `config.yaml`.
///
/// Fields are grouped by concern: resolution determinism, privacy/attack
/// surface, performance, retention, and display.
#[derive(Debug, Clone)]
pub struct RuntimeConfig {
    // Determinism / scoring
    /// Minimum length (in characters) that the first word of a command must
    /// have before abbreviation-resolution is attempted. Shorter words are
    /// passed through unchanged. Default: 2 (single-letter commands still
    /// passthrough; `ls` is the shortest useful prefix you'd want to expand).
    pub min_resolve_prefix_length: u32,
    /// When the candidate pool at disambiguation time reaches this size, skip
    /// the statistics tiebreaker and show the picker immediately. 0 = never
    /// force the picker this way (the engine always tries stats first).
    pub force_picker_at_candidates: u32,
    /// The winner of the stats tiebreaker must score at least this many times
    /// the runner-up to be accepted. Lower values = more aggressive auto-pick;
    /// higher values = more picker prompts. Default 1.05 (5 % margin).
    pub dominance_margin: f32,
    /// When true, the per-directory usage-frequency multiplier is not applied
    /// during scoring. Useful when your trie is shared across machines with
    /// different working-directory layouts.
    pub disable_cwd_scoring: bool,
    /// When true, the `ZSH_IOS_LAST_CMD` sibling-context boost is not applied.
    /// Useful for fully reproducible resolution independent of shell history.
    pub disable_sibling_context: bool,
    /// When true, `narrow_by_arg_type` is skipped during disambiguation.
    pub disable_arg_type_narrowing: bool,
    /// When true, `narrow_by_flag_match` is skipped during disambiguation.
    pub disable_flag_matching: bool,

    // Privacy / attack surface
    /// When true, the ZLE background worker is not started. Disables the
    /// `complete-word`, `_approximate`, and `alias-expand` worker tiers.
    pub disable_worker: bool,
    /// Runtime resolvers whose `id()` appears in this list are short-circuited
    /// to return an empty candidate list. Example: `["git-branches"]` prevents
    /// zsh-ios from shelling out to `git` during completion.
    pub disable_runtime_resolvers: Vec<String>,
    /// Directories to exclude from the Zsh $fpath scan during `build`. Entries
    /// are matched as prefixes after `~` expansion so a single entry can exclude
    /// a whole plugin manager's subtree.
    pub excluded_fpath_dirs: Vec<String>,
    /// When true, the `import_shell_functions` step in `cmd_build` is skipped.
    /// Prevents zsh-ios from launching an interactive `zsh -ic` sub-shell to
    /// enumerate user-defined functions.
    pub disable_build_time_shell_exec: bool,

    // Performance
    /// Per-resolver TTL overrides (seconds). Key is the resolver `id()`. When
    /// present this overrides `resolver.cache_ttl()`.
    pub resolver_ttls: HashMap<String, u64>,
    /// How long (milliseconds) to wait for the ZLE worker before giving up on
    /// a single completion request. Parsed back into the plugin via
    /// `zsh-ios status`.
    pub worker_timeout_ms: u32,
    /// Maximum number of live resolver calls per `resolve` / `complete`
    /// invocation. Once exceeded, remaining resolver calls return empty instead
    /// of shelling out. 0 = no cap.
    pub resolve_max_runtime_calls: u32,

    // Retention
    /// Prune trie nodes that have not been used in this many days AND whose
    /// use count is below 3. 0 = never prune (default).
    pub forget_unused_after_days: u32,
    /// Cap the total number of trie nodes after each `build`. Nodes are dropped
    /// in ascending (count, last_used) order until within the cap. 0 = no cap.
    pub max_trie_size: u32,

    // Display
    /// The prefix character(s) printed before each section header in `?` output
    /// and the ambiguity picker. The plugin parses this from `zsh-ios status`.
    pub picker_header_prefix: String,
    /// When true, ANSI colour codes are suppressed in `?` output.
    pub disable_list_colors: bool,
    /// Maximum number of completions shown by `format_columns`. Default 200.
    pub max_completions_shown: u32,
    /// When true, tag-grouped display is used for commands with tag_groups in
    /// the trie. When false, always use the flat subcommand list.
    pub tag_grouping: bool,
}

impl Default for RuntimeConfig {
    fn default() -> Self {
        Self {
            min_resolve_prefix_length: 1,
            force_picker_at_candidates: 0,
            dominance_margin: 1.05,
            disable_cwd_scoring: false,
            disable_sibling_context: false,
            disable_arg_type_narrowing: false,
            disable_flag_matching: false,
            disable_worker: false,
            disable_runtime_resolvers: Vec::new(),
            excluded_fpath_dirs: Vec::new(),
            disable_build_time_shell_exec: false,
            resolver_ttls: HashMap::new(),
            worker_timeout_ms: 500,
            resolve_max_runtime_calls: 0,
            forget_unused_after_days: 0,
            max_trie_size: 0,
            picker_header_prefix: "%".into(),
            disable_list_colors: false,
            max_completions_shown: 200,
            tag_grouping: true,
        }
    }
}

static CONFIG: OnceLock<RwLock<RuntimeConfig>> = OnceLock::new();

/// Replace the current config. Called once at CLI entry after reading
/// `UserConfig`.
pub fn set(c: RuntimeConfig) {
    let lock = CONFIG.get_or_init(|| RwLock::new(RuntimeConfig::default()));
    *lock.write().unwrap() = c;
}

/// Return a clone of the current config. Cheap enough for hot paths.
pub fn get() -> RuntimeConfig {
    CONFIG
        .get()
        .and_then(|l| l.read().ok())
        .map(|r| r.clone())
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_values_are_sane() {
        let c = RuntimeConfig::default();
        assert_eq!(c.min_resolve_prefix_length, 1);
        assert!((c.dominance_margin - 1.05).abs() < 1e-5);
        assert_eq!(c.picker_header_prefix, "%");
        assert_eq!(c.max_completions_shown, 200);
        assert!(c.tag_grouping);
        assert!(!c.disable_cwd_scoring);
        assert!(!c.disable_arg_type_narrowing);
    }

    #[test]
    fn set_and_get_roundtrip() {
        // Build a config value and verify field assignments without relying on
        // the global singleton (which is shared across parallel test threads.
        let c = RuntimeConfig {
            disable_arg_type_narrowing: true,
            picker_header_prefix: "#".into(),
            ..RuntimeConfig::default()
        };
        assert_eq!(c.min_resolve_prefix_length, 1);
        assert!(c.disable_arg_type_narrowing);
        assert_eq!(c.picker_header_prefix, "#");
    }
}
