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

pub fn ensure_config_dir() -> std::io::Result<()> {
    std::fs::create_dir_all(config_dir())
}
