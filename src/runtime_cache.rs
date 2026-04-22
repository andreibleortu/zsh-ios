use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use serde::{Deserialize, Serialize};

/// Cache format version. Stored in every entry; a mismatch causes a miss so
/// old on-disk entries are automatically discarded after code updates.
const CACHE_FORMAT_VERSION: u32 = 1;

/// A single on-disk cache entry.  `version` guards against format changes;
/// `items` is the opaque payload returned by the resolver.
///
/// We use file mtime rather than embedding a timestamp here.  The file system
/// mtime is sufficient for our TTL needs (resolution to 1 second is fine for
/// lists of git branches or system users), and keeping the struct small avoids
/// any serialization drift between "recorded at" and "file written at".
#[derive(Serialize, Deserialize)]
struct CacheEntry {
    version: u32,
    items: Vec<String>,
}

/// On-disk key→value cache for `TypeResolver` results.  Values are stored as
/// MessagePack; freshness is determined by file mtime compared to a per-entry
/// TTL supplied at read time.  Safe to use concurrently from multiple
/// processes — writes go through a sibling tempfile + atomic rename.
pub struct RuntimeCache {
    dir: PathBuf,
}

impl RuntimeCache {
    /// Create a cache rooted at `dir`.  The directory is created on first
    /// write; callers should not expect it to exist just because `new`
    /// succeeded.
    pub fn new(dir: PathBuf) -> Self {
        Self { dir }
    }

    /// Default cache location.
    ///
    /// Resolution order:
    /// 1. `$ZSH_IOS_RUNTIME_CACHE_DIR` — lets tests (and power users) redirect
    ///    the cache without modifying `$HOME` or `$XDG_CACHE_HOME`.
    /// 2. `$XDG_CACHE_HOME/zsh-ios/runtime/`
    /// 3. `~/.cache/zsh-ios/runtime/`
    ///
    /// Returns `None` if none of the above can be determined.
    pub fn default_location() -> Option<Self> {
        // 1. Test / override env var.
        if let Ok(dir) = std::env::var("ZSH_IOS_RUNTIME_CACHE_DIR")
            && !dir.is_empty()
        {
            return Some(Self::new(PathBuf::from(dir)));
        }
        // 2. XDG_CACHE_HOME.
        if let Ok(xdg) = std::env::var("XDG_CACHE_HOME")
            && !xdg.is_empty()
        {
            return Some(Self::new(PathBuf::from(xdg).join("zsh-ios").join("runtime")));
        }
        // 3. ~/.cache fallback.
        dirs::home_dir()
            .map(|h| Self::new(h.join(".cache").join("zsh-ios").join("runtime")))
    }

    /// Path for a given key's entry file.
    fn entry_path(&self, key: &str) -> PathBuf {
        self.dir.join(format!("{}.mpk", key))
    }

    /// Read an entry if it exists and is fresher than `ttl`.
    ///
    /// Returns `None` for: file missing, expired, malformed, or any I/O
    /// error.  Never panics; all errors are treated as cache misses.
    pub fn get(&self, key: &str, ttl: Duration) -> Option<Vec<String>> {
        let path = self.entry_path(key);
        let meta = fs::metadata(&path).ok()?;
        // Freshness check on the file's mtime.
        let modified = meta.modified().ok()?;
        let age = SystemTime::now().duration_since(modified).unwrap_or(Duration::MAX);
        if age > ttl {
            return None;
        }
        let data = fs::read(&path).ok()?;
        let entry: CacheEntry = rmp_serde::from_slice(&data).ok()?;
        if entry.version != CACHE_FORMAT_VERSION {
            return None;
        }
        Some(entry.items)
    }

    /// Atomically write an entry.  Errors are returned so callers can ignore
    /// them with `let _ = cache.put(...)` — failing to cache must not break
    /// resolution.
    pub fn put(&self, key: &str, items: &[String]) -> io::Result<()> {
        fs::create_dir_all(&self.dir)?;
        let entry = CacheEntry { version: CACHE_FORMAT_VERSION, items: items.to_vec() };
        let data = rmp_serde::to_vec_named(&entry).map_err(io::Error::other)?;
        // Write to a per-process tempfile then rename atomically.
        let tmp_name = format!(".tmp.{}.{}", std::process::id(), key);
        let tmp_path = self.dir.join(tmp_name);
        fs::write(&tmp_path, &data)?;
        if let Err(e) = fs::rename(&tmp_path, self.entry_path(key)) {
            let _ = fs::remove_file(&tmp_path);
            return Err(e);
        }
        Ok(())
    }

    /// Remove all cached entries.  Used by `zsh-ios rebuild` and tests.
    pub fn clear(&self) -> io::Result<()> {
        if !self.dir.exists() {
            return Ok(());
        }
        for entry in fs::read_dir(&self.dir)? {
            let entry = entry?;
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) == Some("mpk") {
                fs::remove_file(path)?;
            }
        }
        Ok(())
    }

    /// Return `(entries_on_disk, total_bytes)` for status/debugging output.
    pub fn stats(&self) -> (usize, u64) {
        if !self.dir.exists() {
            return (0, 0);
        }
        let Ok(entries) = fs::read_dir(&self.dir) else {
            return (0, 0);
        };
        let mut count = 0usize;
        let mut bytes = 0u64;
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) == Some("mpk") {
                count += 1;
                if let Ok(meta) = fs::metadata(&path) {
                    bytes += meta.len();
                }
            }
        }
        (count, bytes)
    }
}

/// Build a stable cache key from a resolver id, the current working directory
/// (or `"/"` if none), and the resolver-specific inputs (prior words).
///
/// We use `std::hash::DefaultHasher` (no external dep) with a 16-hex-char
/// suffix, which gives 64 bits of collision resistance — more than enough for
/// a per-user on-disk cache of runtime type lists.
///
/// Key format: `<sanitized_id>_<16-hex-hash>.mpk`
/// — `sanitized_id`: resolver id with non-alphanumeric chars replaced by `_`.
/// — hash input: `id | cwd | inputs` each separated by a `\0` byte.
pub fn make_key(resolver_id: &str, cwd: Option<&Path>, inputs: &[&str]) -> String {
    use std::hash::{Hash, Hasher};

    let sanitized: String = resolver_id
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '_' })
        .collect();

    let cwd_str = cwd.and_then(|p| p.to_str()).unwrap_or("/");

    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    resolver_id.hash(&mut hasher);
    '\0'.hash(&mut hasher);
    cwd_str.hash(&mut hasher);
    for input in inputs {
        '\0'.hash(&mut hasher);
        input.hash(&mut hasher);
    }
    let h = hasher.finish();

    format!("{}_{:016x}", sanitized, h)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;
    use tempfile::tempdir;

    #[test]
    fn put_then_get_roundtrip() {
        let td = tempdir().unwrap();
        let cache = RuntimeCache::new(td.path().to_path_buf());
        let items = vec!["a".to_string(), "b".to_string(), "c".to_string()];
        cache.put("test-key", &items).unwrap();
        let got = cache.get("test-key", Duration::from_secs(60));
        assert_eq!(got, Some(items));
    }

    #[test]
    fn get_expired_returns_none() {
        let td = tempdir().unwrap();
        let cache = RuntimeCache::new(td.path().to_path_buf());
        let items = vec!["x".to_string()];
        cache.put("exp-key", &items).unwrap();
        // TTL of 1 ms; sleep 10 ms so the entry is definitely stale.
        std::thread::sleep(Duration::from_millis(10));
        let got = cache.get("exp-key", Duration::from_millis(1));
        assert!(got.is_none(), "expected None for expired entry, got {:?}", got);
    }

    #[test]
    fn get_missing_returns_none() {
        let td = tempdir().unwrap();
        let cache = RuntimeCache::new(td.path().to_path_buf());
        assert!(cache.get("never-written", Duration::from_secs(60)).is_none());
    }

    #[test]
    fn clear_wipes_all() {
        let td = tempdir().unwrap();
        let cache = RuntimeCache::new(td.path().to_path_buf());
        cache.put("key1", &["a".to_string()]).unwrap();
        cache.put("key2", &["b".to_string()]).unwrap();
        assert_eq!(cache.stats().0, 2);
        cache.clear().unwrap();
        assert!(cache.get("key1", Duration::from_secs(60)).is_none());
        assert!(cache.get("key2", Duration::from_secs(60)).is_none());
        assert_eq!(cache.stats().0, 0);
    }

    #[test]
    fn make_key_stable() {
        let k1 = make_key("git-branches", Some(Path::new("/home/user/repo")), &["git"]);
        let k2 = make_key("git-branches", Some(Path::new("/home/user/repo")), &["git"]);
        assert_eq!(k1, k2, "same inputs must yield the same key");

        // Different cwd → different key.
        let k3 = make_key("git-branches", Some(Path::new("/other/repo")), &["git"]);
        assert_ne!(k1, k3, "different cwd must produce different key");

        // Different prior_words → different key.
        let k4 = make_key("git-branches", Some(Path::new("/home/user/repo")), &["git", "extra"]);
        assert_ne!(k1, k4, "different inputs must produce different key");

        // No cwd → uses "/" sentinel, still stable.
        let k5 = make_key("git-branches", None, &[]);
        let k6 = make_key("git-branches", None, &[]);
        assert_eq!(k5, k6);
    }

    #[test]
    fn cache_version_mismatch_returns_none() {
        let td = tempdir().unwrap();
        let cache = RuntimeCache::new(td.path().to_path_buf());
        // Write a valid entry first so the directory exists.
        cache.put("ver-key", &["ok".to_string()]).unwrap();
        assert!(cache.get("ver-key", Duration::from_secs(60)).is_some());

        // Overwrite with a mismatched version directly.
        #[derive(serde::Serialize)]
        struct BadEntry {
            version: u32,
            items: Vec<String>,
        }
        let bad = BadEntry { version: 999, items: vec!["x".to_string()] };
        let data = rmp_serde::to_vec_named(&bad).unwrap();
        fs::write(td.path().join("ver-key.mpk"), data).unwrap();

        assert!(
            cache.get("ver-key", Duration::from_secs(60)).is_none(),
            "version mismatch must return None"
        );
    }

    #[test]
    fn stats_empty_dir() {
        let td = tempdir().unwrap();
        let cache = RuntimeCache::new(td.path().to_path_buf());
        assert_eq!(cache.stats(), (0, 0));
    }

    #[test]
    fn stats_counts_entries() {
        let td = tempdir().unwrap();
        let cache = RuntimeCache::new(td.path().to_path_buf());
        cache.put("a", &["one".to_string()]).unwrap();
        cache.put("b", &["two".to_string(), "three".to_string()]).unwrap();
        let (count, bytes) = cache.stats();
        assert_eq!(count, 2);
        assert!(bytes > 0);
    }

    #[test]
    fn atomic_write_no_partial_reads() {
        // Verify that a concurrent reader never sees a corrupt/partial file.
        // We hammer `get` from a reader thread while a writer loop does puts,
        // and assert that `get` always returns either None (before first write
        // or an expired entry) or a well-formed Vec — never an error that
        // leaked through (rmp_serde errors are caught as None inside `get`).
        let td = tempdir().unwrap();
        let dir = td.path().to_path_buf();
        let cache = RuntimeCache::new(dir.clone());
        let key = "atomic-test";
        let expected: Vec<String> = (0..50).map(|i| format!("item-{}", i)).collect();

        let reader_dir = dir.clone();
        let reader_expected = expected.clone();
        let reader_key = key.to_string();
        let handle = std::thread::spawn(move || {
            let rc = RuntimeCache::new(reader_dir);
            for _ in 0..200 {
                let result = rc.get(&reader_key, Duration::from_secs(60));
                if let Some(items) = result {
                    // Must be either the empty sentinel or the full expected list.
                    assert!(
                        items.is_empty() || items == reader_expected,
                        "unexpected intermediate value: {:?}",
                        items
                    );
                }
            }
        });

        // Writer: repeatedly put either an empty list or the full list.
        for i in 0..100 {
            let data: Vec<String> = if i % 2 == 0 { vec![] } else { expected.clone() };
            let _ = cache.put(key, &data);
        }
        handle.join().unwrap();
    }
}
