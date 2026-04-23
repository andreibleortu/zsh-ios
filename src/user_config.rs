//! Optional user-facing config at `$config_dir/config.yaml`.
//!
//! Every knob here is optional; a missing file or a field left out just falls
//! back to the compiled-in default. Parse failures are surfaced on stderr but
//! are *never* fatal — an invalid config must not wedge the shell.

use crate::runtime_config::RuntimeConfig;
use serde::Deserialize;
use std::collections::HashMap;
use std::fs;
use std::path::Path;

/// Default stale-trie rebuild threshold, in seconds. The Zsh plugin reads
/// this value out of `zsh-ios status` so both sides agree.
pub const DEFAULT_STALE_THRESHOLD_SECS: u64 = 3600;

// ── serde default helpers ──────────────────────────────────────────────────

fn default_dominance_margin() -> f32 { 1.05 }
fn default_min_resolve_prefix_length() -> u32 { 1 }
fn default_worker_timeout_ms() -> u32 { 500 }
fn default_max_completions_shown() -> u32 { 200 }
fn default_tag_grouping() -> bool { true }
fn default_picker_header_prefix() -> String { "%".into() }

#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct UserConfig {
    // ── Existing fields ───────────────────────────────────────────────────────

    /// How old (in seconds) `tree.msgpack` must be before the plugin auto-
    /// rebuilds on shell startup. `None` = use the compiled-in default.
    pub stale_threshold_seconds: Option<u64>,

    /// If true, `zsh-ios learn` is a no-op — the trie never grows from
    /// interactive use. Users who manage their trie explicitly via `build` /
    /// `rebuild` can turn this off to keep the trie deterministic.
    pub disable_learning: bool,

    /// Commands that zsh-ios must not touch. When the first word of the
    /// input matches any entry here (either literally as typed *or* as the
    /// resolved first word), `resolve` returns passthrough (exit 2) so the
    /// buffer runs exactly as typed.
    pub command_blocklist: Vec<String>,

    /// When true, the statistical tiebreaker (frequency × recency ×
    /// success-rate) is skipped. Users who want reproducible resolution
    /// across machines and sessions turn this on.
    pub disable_statistics: bool,

    /// When true, global aliases (alias -g) are NOT expanded before
    /// resolution. Some users prefer the literal buffer intact.
    pub disable_galiases: bool,

    /// When true, the build-time harvest of `_regex_arguments` specs via the
    /// zpty worker is skipped.
    pub disable_dynamic_harvest: bool,

    // ── Resolution determinism ────────────────────────────────────────────────

    /// Minimum character length of the first word before resolution is
    /// attempted. Words shorter than this are passed through unchanged.
    /// Default: 2.
    #[serde(default = "default_min_resolve_prefix_length")]
    pub min_resolve_prefix_length: u32,

    /// When the candidate pool reaches this count, skip the statistical
    /// tiebreaker and show the picker directly. 0 = disabled (default).
    pub force_picker_at_candidates: u32,

    /// The winning stats candidate must score at least this multiple above the
    /// runner-up. Default: 1.05.
    #[serde(default = "default_dominance_margin")]
    pub dominance_margin: f32,

    /// When true, the per-directory usage-frequency multiplier is not applied
    /// during scoring.
    pub disable_cwd_scoring: bool,

    /// When true, the `ZSH_IOS_LAST_CMD` sibling-context boost is not applied.
    pub disable_sibling_context: bool,

    /// When true, arg-type narrowing is skipped during disambiguation.
    pub disable_arg_type_narrowing: bool,

    /// When true, flag-match narrowing is skipped during disambiguation.
    pub disable_flag_matching: bool,

    // ── Privacy / attack surface ──────────────────────────────────────────────

    /// When true, the ZLE background worker is not started.
    pub disable_worker: bool,

    /// Runtime resolver ids to disable. Example: `["git-branches", "hosts"]`.
    pub disable_runtime_resolvers: Vec<String>,

    /// Fpath directories to exclude from `build` scans. Matched as path
    /// prefixes after `~` expansion.
    pub excluded_fpath_dirs: Vec<String>,

    /// When true, the `zsh -ic` shell function enumeration step in `build` is
    /// skipped.
    pub disable_build_time_shell_exec: bool,

    // ── Performance ───────────────────────────────────────────────────────────

    /// Per-resolver TTL overrides in seconds. Key is `resolver.id()`.
    pub resolver_ttls: HashMap<String, u64>,

    /// How long (ms) to wait for the ZLE worker. Parsed by plugin from
    /// `zsh-ios status`. Default: 500.
    #[serde(default = "default_worker_timeout_ms")]
    pub worker_timeout_ms: u32,

    /// Max live resolver calls per invocation. 0 = no cap.
    pub resolve_max_runtime_calls: u32,

    // ── Retention ─────────────────────────────────────────────────────────────

    /// Prune nodes unused for this many days with count < 3 during `build`.
    /// 0 = disabled.
    pub forget_unused_after_days: u32,

    /// Cap total trie node count during `build`. 0 = no cap.
    pub max_trie_size: u32,

    // ── Display ───────────────────────────────────────────────────────────────

    /// Prefix printed before section headers in `?` output and the picker.
    /// Parsed by plugin from `zsh-ios status`. Default: `%`.
    #[serde(default = "default_picker_header_prefix")]
    pub picker_header_prefix: String,

    /// When true, ANSI colour codes are suppressed in `?` output.
    pub disable_list_colors: bool,

    /// Maximum items shown by `format_columns`. Default: 200.
    #[serde(default = "default_max_completions_shown")]
    pub max_completions_shown: u32,

    /// When false, tag-grouped display is never used. Default: true.
    #[serde(default = "default_tag_grouping")]
    pub tag_grouping: bool,

    // ── Ghost preview ─────────────────────────────────────────────────────────

    /// When true, the live resolved-command preview is suppressed. Default
    /// is to show it in the configured style two spaces after the cursor
    /// (via POSTDISPLAY — see docs/config.md).
    pub disable_ghost_preview: bool,

    /// region_highlight style spec for the ghost text. Passed verbatim to
    /// `region_highlight=("P0 N <style>")`. Examples: `fg=240`, `fg=#888`,
    /// `fg=blue,italic`. Default: `"fg=240"` (256-color gray).
    pub ghost_preview_style: Option<String>,

    /// Literal bytes inserted between the user's buffer and the ghost text.
    /// Default: `"  "` (two spaces). Set to the empty string for a tight
    /// render, or ` -> ` for an arrow separator.
    pub ghost_preview_prefix: Option<String>,
}

impl Default for UserConfig {
    fn default() -> Self {
        Self {
            stale_threshold_seconds: None,
            disable_learning: false,
            command_blocklist: Vec::new(),
            disable_statistics: false,
            disable_galiases: false,
            disable_dynamic_harvest: false,
            min_resolve_prefix_length: 1,
            force_picker_at_candidates: 0,
            dominance_margin: default_dominance_margin(),
            disable_cwd_scoring: false,
            disable_sibling_context: false,
            disable_arg_type_narrowing: false,
            disable_flag_matching: false,
            disable_worker: false,
            disable_runtime_resolvers: Vec::new(),
            excluded_fpath_dirs: Vec::new(),
            disable_build_time_shell_exec: false,
            resolver_ttls: HashMap::new(),
            worker_timeout_ms: default_worker_timeout_ms(),
            resolve_max_runtime_calls: 0,
            forget_unused_after_days: 0,
            max_trie_size: 0,
            picker_header_prefix: default_picker_header_prefix(),
            disable_list_colors: false,
            max_completions_shown: default_max_completions_shown(),
            tag_grouping: default_tag_grouping(),
            disable_ghost_preview: false,
            ghost_preview_style: None,
            ghost_preview_prefix: None,
        }
    }
}

impl UserConfig {
    /// Read and parse the config file. Missing file → defaults (silent).
    /// Parse error → warn on stderr, defaults (so the shell still works).
    pub fn load(path: &Path) -> Self {
        let content = match fs::read_to_string(path) {
            Ok(s) => s,
            Err(_) => return Self::default(),
        };
        Self::parse(&content).unwrap_or_else(|e| {
            eprintln!(
                "zsh-ios: ignoring invalid config at {}: {}",
                path.display(),
                e
            );
            Self::default()
        })
    }

    /// Parse a YAML string into a UserConfig.
    pub fn parse(s: &str) -> Result<Self, serde_yaml_ng::Error> {
        if s.trim().is_empty() {
            return Ok(Self::default());
        }
        serde_yaml_ng::from_str(s)
    }

    /// Effective stale-trie threshold in seconds (applies default when unset).
    pub fn stale_threshold(&self) -> u64 {
        self.stale_threshold_seconds
            .unwrap_or(DEFAULT_STALE_THRESHOLD_SECS)
    }

    /// True if `first_word` is literally on the blocklist.
    pub fn is_blocklisted(&self, first_word: &str) -> bool {
        !first_word.is_empty() && self.command_blocklist.iter().any(|b| b == first_word)
    }

    /// Convert to the runtime representation published via [`runtime_config::set`].
    pub fn to_runtime_config(&self) -> RuntimeConfig {
        RuntimeConfig {
            min_resolve_prefix_length: self.min_resolve_prefix_length,
            force_picker_at_candidates: self.force_picker_at_candidates,
            dominance_margin: self.dominance_margin,
            disable_cwd_scoring: self.disable_cwd_scoring,
            disable_sibling_context: self.disable_sibling_context,
            disable_arg_type_narrowing: self.disable_arg_type_narrowing,
            disable_flag_matching: self.disable_flag_matching,
            disable_worker: self.disable_worker,
            disable_runtime_resolvers: self.disable_runtime_resolvers.clone(),
            excluded_fpath_dirs: self.excluded_fpath_dirs.clone(),
            disable_build_time_shell_exec: self.disable_build_time_shell_exec,
            resolver_ttls: self.resolver_ttls.clone(),
            worker_timeout_ms: self.worker_timeout_ms,
            resolve_max_runtime_calls: self.resolve_max_runtime_calls,
            forget_unused_after_days: self.forget_unused_after_days,
            max_trie_size: self.max_trie_size,
            picker_header_prefix: self.picker_header_prefix.clone(),
            disable_list_colors: self.disable_list_colors,
            max_completions_shown: self.max_completions_shown,
            tag_grouping: self.tag_grouping,
            disable_ghost_preview: self.disable_ghost_preview,
            ghost_preview_style: self
                .ghost_preview_style
                .clone()
                .unwrap_or_else(|| "fg=240".into()),
            ghost_preview_prefix: self
                .ghost_preview_prefix
                .clone()
                .unwrap_or_else(|| "  ".into()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_is_permissive() {
        let c = UserConfig::default();
        assert_eq!(c.stale_threshold(), DEFAULT_STALE_THRESHOLD_SECS);
        assert!(!c.disable_learning);
        assert!(!c.disable_statistics);
        assert!(!c.disable_galiases);
        assert!(c.command_blocklist.is_empty());
        assert!(!c.is_blocklisted("git"));
    }

    #[test]
    fn parse_disable_statistics() {
        let c = UserConfig::parse("disable_statistics: true").unwrap();
        assert!(c.disable_statistics);
        assert!(!c.disable_learning);
    }

    #[test]
    fn parse_empty_is_default() {
        let c = UserConfig::parse("").unwrap();
        assert_eq!(c.stale_threshold(), DEFAULT_STALE_THRESHOLD_SECS);
        assert!(!c.disable_learning);
    }

    #[test]
    fn parse_missing_fields_use_defaults() {
        let c = UserConfig::parse("disable_learning: true").unwrap();
        assert!(c.disable_learning);
        assert_eq!(c.stale_threshold(), DEFAULT_STALE_THRESHOLD_SECS);
        assert!(c.command_blocklist.is_empty());
    }

    #[test]
    fn parse_full_config() {
        let yaml = r#"
stale_threshold_seconds: 7200
disable_learning: true
command_blocklist:
  - kubectl
  - docker
"#;
        let c = UserConfig::parse(yaml).unwrap();
        assert_eq!(c.stale_threshold(), 7200);
        assert!(c.disable_learning);
        assert_eq!(c.command_blocklist, vec!["kubectl", "docker"]);
        assert!(c.is_blocklisted("kubectl"));
        assert!(c.is_blocklisted("docker"));
        assert!(!c.is_blocklisted("git"));
    }

    #[test]
    fn blocklist_is_case_sensitive_and_exact() {
        let c = UserConfig {
            command_blocklist: vec!["kubectl".into()],
            ..UserConfig::default()
        };
        assert!(c.is_blocklisted("kubectl"));
        assert!(!c.is_blocklisted("Kubectl"));
        assert!(!c.is_blocklisted("kub"));
        assert!(!c.is_blocklisted("kubectl-more"));
        assert!(!c.is_blocklisted(""));
    }

    #[test]
    fn parse_unknown_field_errors() {
        // deny_unknown_fields catches typos so users don't silently get
        // defaults when they meant to set something.
        let err = UserConfig::parse("disabel_learning: true").unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("disabel_learning") || msg.contains("unknown field"));
    }

    #[test]
    fn load_missing_file_returns_default() {
        let td = tempfile::tempdir().unwrap();
        let p = td.path().join("does-not-exist.yaml");
        let c = UserConfig::load(&p);
        assert!(!c.disable_learning);
        assert_eq!(c.stale_threshold(), DEFAULT_STALE_THRESHOLD_SECS);
    }

    #[test]
    fn load_invalid_file_returns_default_and_doesnt_panic() {
        let td = tempfile::tempdir().unwrap();
        let p = td.path().join("bad.yaml");
        fs::write(&p, "stale_threshold_seconds: not-a-number\n").unwrap();
        let c = UserConfig::load(&p);
        assert_eq!(c.stale_threshold(), DEFAULT_STALE_THRESHOLD_SECS);
    }

    #[test]
    fn load_valid_file_roundtrips() {
        let td = tempfile::tempdir().unwrap();
        let p = td.path().join("config.yaml");
        fs::write(
            &p,
            "stale_threshold_seconds: 600\ncommand_blocklist:\n  - ansible\n",
        )
        .unwrap();
        let c = UserConfig::load(&p);
        assert_eq!(c.stale_threshold(), 600);
        assert!(c.is_blocklisted("ansible"));
    }

    #[test]
    fn parse_every_new_field() {
        let yaml = r#"
min_resolve_prefix_length: 3
force_picker_at_candidates: 5
dominance_margin: 1.2
disable_cwd_scoring: true
disable_sibling_context: true
disable_arg_type_narrowing: true
disable_flag_matching: true
disable_worker: true
disable_runtime_resolvers:
  - git-branches
  - hosts
excluded_fpath_dirs:
  - ~/.local/share/zinit
disable_build_time_shell_exec: true
resolver_ttls:
  git-branches: 30
  hosts: 3600
worker_timeout_ms: 1000
resolve_max_runtime_calls: 16
forget_unused_after_days: 90
max_trie_size: 5000
picker_header_prefix: ">"
disable_list_colors: true
max_completions_shown: 100
tag_grouping: false
"#;
        let c = UserConfig::parse(yaml).unwrap();
        assert_eq!(c.min_resolve_prefix_length, 3);
        assert_eq!(c.force_picker_at_candidates, 5);
        assert!((c.dominance_margin - 1.2).abs() < 1e-4);
        assert!(c.disable_cwd_scoring);
        assert!(c.disable_sibling_context);
        assert!(c.disable_arg_type_narrowing);
        assert!(c.disable_flag_matching);
        assert!(c.disable_worker);
        assert_eq!(c.disable_runtime_resolvers, vec!["git-branches", "hosts"]);
        assert_eq!(c.excluded_fpath_dirs, vec!["~/.local/share/zinit"]);
        assert!(c.disable_build_time_shell_exec);
        assert_eq!(c.resolver_ttls.get("git-branches"), Some(&30u64));
        assert_eq!(c.resolver_ttls.get("hosts"), Some(&3600u64));
        assert_eq!(c.worker_timeout_ms, 1000);
        assert_eq!(c.resolve_max_runtime_calls, 16);
        assert_eq!(c.forget_unused_after_days, 90);
        assert_eq!(c.max_trie_size, 5000);
        assert_eq!(c.picker_header_prefix, ">");
        assert!(c.disable_list_colors);
        assert_eq!(c.max_completions_shown, 100);
        assert!(!c.tag_grouping);

        // Verify to_runtime_config round-trip
        let rc = c.to_runtime_config();
        assert_eq!(rc.min_resolve_prefix_length, 3);
        assert!(rc.disable_arg_type_narrowing);
        assert_eq!(rc.picker_header_prefix, ">");
        assert_eq!(rc.max_completions_shown, 100);
        assert!(!rc.tag_grouping);
    }

    #[test]
    fn new_fields_have_correct_defaults() {
        let c = UserConfig::default();
        assert_eq!(c.min_resolve_prefix_length, 1);
        assert_eq!(c.force_picker_at_candidates, 0);
        assert!((c.dominance_margin - 1.05).abs() < 1e-4);
        assert!(!c.disable_cwd_scoring);
        assert!(!c.disable_sibling_context);
        assert!(!c.disable_arg_type_narrowing);
        assert!(!c.disable_flag_matching);
        assert!(!c.disable_worker);
        assert!(c.disable_runtime_resolvers.is_empty());
        assert!(c.excluded_fpath_dirs.is_empty());
        assert!(!c.disable_build_time_shell_exec);
        assert!(c.resolver_ttls.is_empty());
        assert_eq!(c.worker_timeout_ms, 500);
        assert_eq!(c.resolve_max_runtime_calls, 0);
        assert_eq!(c.forget_unused_after_days, 0);
        assert_eq!(c.max_trie_size, 0);
        assert_eq!(c.picker_header_prefix, "%");
        assert!(!c.disable_list_colors);
        assert_eq!(c.max_completions_shown, 200);
        assert!(c.tag_grouping);
        assert!(!c.disable_ghost_preview);
        assert!(c.ghost_preview_style.is_none());
        assert!(c.ghost_preview_prefix.is_none());
    }

    #[test]
    fn parse_ghost_preview_fields() {
        let yaml = r#"
disable_ghost_preview: true
ghost_preview_style: "fg=blue,italic"
ghost_preview_prefix: " -> "
"#;
        let c = UserConfig::parse(yaml).unwrap();
        assert!(c.disable_ghost_preview);
        assert_eq!(c.ghost_preview_style.as_deref(), Some("fg=blue,italic"));
        assert_eq!(c.ghost_preview_prefix.as_deref(), Some(" -> "));

        let rc = c.to_runtime_config();
        assert!(rc.disable_ghost_preview);
        assert_eq!(rc.ghost_preview_style, "fg=blue,italic");
        assert_eq!(rc.ghost_preview_prefix, " -> ");
    }

    #[test]
    fn ghost_preview_runtime_config_defaults_when_unset() {
        let c = UserConfig::default();
        let rc = c.to_runtime_config();
        assert!(!rc.disable_ghost_preview);
        assert_eq!(rc.ghost_preview_style, "fg=240");
        assert_eq!(rc.ghost_preview_prefix, "  ");
    }
}
