use std::fs::OpenOptions;
use std::path::Path;

/// Acquire an exclusive advisory lock on a sibling `.lock` file for the
/// given path.  The lock is released when the returned file handle drops.
///
/// Used to serialize concurrent `learn` / `build` / `pin` / `ingest` writers
/// that the Zsh plugin spawns in the background after every command.
pub fn lock_for(path: &Path) -> Option<std::fs::File> {
    let lock_path = {
        let mut s = path.as_os_str().to_os_string();
        s.push(".lock");
        std::path::PathBuf::from(s)
    };
    if let Some(parent) = lock_path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let file = OpenOptions::new()
        .create(true)
        .read(true)
        .write(true)
        .truncate(false)
        .open(&lock_path)
        .ok()?;
    file.lock().ok()?;
    Some(file)
}
