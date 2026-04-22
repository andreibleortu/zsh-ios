use std::path::PathBuf;

pub fn config_dir() -> PathBuf {
    let base = dirs::config_dir().unwrap_or_else(|| PathBuf::from("."));
    base.join("zsh-ios")
}

pub fn tree_path() -> PathBuf {
    config_dir().join("tree.msgpack")
}

pub fn pins_path() -> PathBuf {
    config_dir().join("pins.txt")
}

/// Location of the optional user config (`config.yaml`). Absent by default.
pub fn user_config_path() -> PathBuf {
    config_dir().join("config.yaml")
}

pub fn ensure_config_dir() -> std::io::Result<()> {
    std::fs::create_dir_all(config_dir())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `dirs::config_dir()` consults `$XDG_CONFIG_HOME` on Linux and
    /// `$HOME/Library/Application Support` on macOS. To avoid cross-platform
    /// assumptions in a single test, we just verify the returned path ends
    /// with `zsh-ios` and the relative helpers extend it correctly.
    #[test]
    fn config_dir_ends_with_zsh_ios() {
        let dir = config_dir();
        assert_eq!(dir.file_name().and_then(|s| s.to_str()), Some("zsh-ios"));
    }

    #[test]
    fn tree_path_lives_under_config_dir() {
        let tree = tree_path();
        assert_eq!(tree.file_name().and_then(|s| s.to_str()), Some("tree.msgpack"));
        assert_eq!(tree.parent().unwrap(), config_dir());
    }

    #[test]
    fn pins_path_lives_under_config_dir() {
        let pins = pins_path();
        assert_eq!(pins.file_name().and_then(|s| s.to_str()), Some("pins.txt"));
        assert_eq!(pins.parent().unwrap(), config_dir());
    }

    #[test]
    fn user_config_path_lives_under_config_dir() {
        let cfg = user_config_path();
        assert_eq!(
            cfg.file_name().and_then(|s| s.to_str()),
            Some("config.yaml")
        );
        assert_eq!(cfg.parent().unwrap(), config_dir());
    }

    #[test]
    fn ensure_config_dir_creates_directory() {
        // We can't override HOME globally in a test without races, so we just
        // verify the function is idempotent against the real config dir.
        let dir = config_dir();
        ensure_config_dir().expect("first call");
        assert!(dir.exists());
        ensure_config_dir().expect("second call is idempotent");
    }
}
