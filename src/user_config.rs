//! Optional user-facing config at `$config_dir/config.yaml`.
//!
//! Every knob here is optional; a missing file or a field left out just falls
//! back to the compiled-in default. Parse failures are surfaced on stderr but
//! are *never* fatal — an invalid config must not wedge the shell.

use serde::Deserialize;
use std::fs;
use std::path::Path;

/// Default stale-trie rebuild threshold, in seconds. The Zsh plugin reads
/// this value out of `zsh-ios status` so both sides agree.
pub const DEFAULT_STALE_THRESHOLD_SECS: u64 = 3600;

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct UserConfig {
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
    /// success-rate) is skipped — if narrowing by subcommand prefix,
    /// arg-type, and flag match still leaves >1 candidate, the user sees
    /// the picker instead of the engine silently picking the historical
    /// favorite. Users who want reproducible resolution across machines
    /// and sessions turn this on.
    pub disable_statistics: bool,

    /// When true, global aliases (alias -g) are NOT expanded before
    /// resolution. Some users prefer the literal buffer intact.
    pub disable_galiases: bool,
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
}
