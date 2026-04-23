//! Built-in configuration presets for `zsh-ios preset`.

pub const DETERMINISTIC: &str = r#"# zsh-ios — deterministic / reproducible profile
# Resolution never depends on per-machine history; ties always surface as a
# picker rather than a silent pick.
disable_learning: true
disable_statistics: true
disable_sibling_context: true
disable_cwd_scoring: true
disable_arg_type_narrowing: false
disable_flag_matching: false
"#;

pub const PRIVACY: &str = r#"# zsh-ios — privacy-conscious profile
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
"#;

pub const POWER: &str = r#"# zsh-ios — power user profile
# Reduced dominance margin so the stats tiebreaker is more willing to
# auto-pick. Git branch/tag lists refreshed more frequently.
dominance_margin: 1.01
force_picker_at_candidates: 0
max_completions_shown: 500
tag_grouping: true
resolver_ttls:
  git-branches: 10
  git-tags: 60
"#;

pub fn cmd_preset(name: Option<&str>, show: bool, force: bool) {
    let Some(name) = name else {
        print_preset_list();
        return;
    };
    let yaml = match name {
        "deterministic" => DETERMINISTIC,
        "privacy" => PRIVACY,
        "power" => POWER,
        other => {
            eprintln!("Unknown preset: {other}");
            eprintln!("Available: deterministic, privacy, power");
            std::process::exit(1);
        }
    };
    if show {
        print!("{}", yaml);
        return;
    }
    let path = crate::config::user_config_path();
    if path.exists() && !force {
        let ts = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        let backup = path.with_extension(format!("yaml.bak.{ts}"));
        if let Err(e) = std::fs::copy(&path, &backup) {
            eprintln!("Could not back up existing config: {e}");
            std::process::exit(1);
        }
        eprintln!("Backed up existing config to {}", backup.display());
    }
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    if let Err(e) = std::fs::write(&path, yaml) {
        eprintln!("Could not write config: {e}");
        std::process::exit(1);
    }
    eprintln!("Applied preset '{name}' to {}", path.display());
}

fn print_preset_list() {
    println!("Available zsh-ios presets:");
    println!("  deterministic   reproducible resolution — no history-based ranking");
    println!("  privacy         no worker, no build-time shell exec");
    println!("  power           aggressive scoring + tuned resolver TTLs");
    println!();
    println!("Use:");
    println!("  zsh-ios preset <name>          apply (backs up existing config.yaml)");
    println!("  zsh-ios preset <name> --show   print the YAML without writing");
    println!("  zsh-ios preset <name> --force  overwrite without prompt");
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn all_presets_parse_as_valid_user_config() {
        for (name, yaml) in [
            ("deterministic", DETERMINISTIC),
            ("privacy", PRIVACY),
            ("power", POWER),
        ] {
            crate::user_config::UserConfig::parse(yaml)
                .unwrap_or_else(|e| panic!("preset {name} failed to parse: {e}"));
        }
    }

    #[test]
    fn preset_names_are_distinct() {
        assert_ne!(DETERMINISTIC, PRIVACY);
        assert_ne!(DETERMINISTIC, POWER);
        assert_ne!(PRIVACY, POWER);
    }

    #[test]
    fn deterministic_sets_disable_statistics() {
        let c = crate::user_config::UserConfig::parse(DETERMINISTIC).unwrap();
        assert!(c.disable_statistics);
    }

    #[test]
    fn privacy_sets_disable_worker() {
        let c = crate::user_config::UserConfig::parse(PRIVACY).unwrap();
        assert!(c.disable_worker);
    }

    #[test]
    fn power_sets_custom_ttls() {
        let c = crate::user_config::UserConfig::parse(POWER).unwrap();
        assert!(!c.resolver_ttls.is_empty());
    }
}
