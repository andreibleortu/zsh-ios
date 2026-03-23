//! Runtime completion resolvers for expanded argument types.
//!
//! Each resolver queries the system at resolve-time and returns matching
//! entries for a given prefix. Used both for abbreviation resolution and
//! for `?` key completions.

use std::collections::HashMap;
use std::io::BufRead;
use std::sync::Mutex;

/// Cached results for expensive lookups.
static CACHE: Mutex<Option<ResolverCache>> = Mutex::new(None);

struct ResolverCache {
    signals: Vec<String>,
    ports: HashMap<String, u16>,
    users: Vec<String>,
    groups: Vec<String>,
    hosts: Vec<String>,
    net_ifaces: Vec<String>,
    locales: Vec<String>,
}

fn get_or_init_cache() -> std::sync::MutexGuard<'static, Option<ResolverCache>> {
    let mut guard = CACHE.lock().unwrap();
    if guard.is_none() {
        *guard = Some(ResolverCache {
            signals: load_signals(),
            ports: load_ports(),
            users: load_users(),
            groups: load_groups(),
            hosts: load_hosts(),
            net_ifaces: load_net_ifaces(),
            locales: load_locales(),
        });
    }
    guard
}

// --- Signal names (hardcoded, portable) ---

fn load_signals() -> Vec<String> {
    [
        "HUP", "INT", "QUIT", "ILL", "TRAP", "ABRT", "EMT", "FPE", "KILL", "BUS", "SEGV", "SYS",
        "PIPE", "ALRM", "TERM", "URG", "STOP", "TSTP", "CONT", "CHLD", "TTIN", "TTOU", "IO",
        "XCPU", "XFSZ", "VTALRM", "PROF", "WINCH", "INFO", "USR1", "USR2",
    ]
    .iter()
    .map(|s| s.to_string())
    .collect()
}

// --- Port names from /etc/services ---

fn load_ports() -> HashMap<String, u16> {
    let mut ports = HashMap::new();
    if let Ok(file) = std::fs::File::open("/etc/services") {
        for line in std::io::BufReader::new(file).lines().map_while(Result::ok) {
            let trimmed = line.trim();
            if trimmed.is_empty() || trimmed.starts_with('#') {
                continue;
            }
            let parts: Vec<&str> = trimmed.split_whitespace().collect();
            if parts.len() >= 2
                && let Some(num_str) = parts[1].split('/').next()
                && let Ok(num) = num_str.parse::<u16>()
            {
                ports.insert(parts[0].to_string(), num);
            }
        }
    }
    ports
}

// --- System users ---

fn load_users() -> Vec<String> {
    let mut users = Vec::new();
    // macOS: dscl . list /Users
    if let Ok(output) = std::process::Command::new("dscl")
        .args([".", "list", "/Users"])
        .output()
        && output.status.success()
    {
        for line in String::from_utf8_lossy(&output.stdout).lines() {
            let name = line.trim();
            if !name.is_empty() && !name.starts_with('_') {
                users.push(name.to_string());
            }
        }
    }
    // Fallback: /etc/passwd
    if users.is_empty()
        && let Ok(file) = std::fs::File::open("/etc/passwd")
    {
        for line in std::io::BufReader::new(file).lines().map_while(Result::ok) {
            if let Some(name) = line.split(':').next()
                && !name.starts_with('#')
                && !name.starts_with('_')
            {
                users.push(name.to_string());
            }
        }
    }
    users
}

// --- System groups ---

fn load_groups() -> Vec<String> {
    let mut groups = Vec::new();
    if let Ok(file) = std::fs::File::open("/etc/group") {
        for line in std::io::BufReader::new(file).lines().map_while(Result::ok) {
            if let Some(name) = line.split(':').next()
                && !name.starts_with('#')
                && !name.starts_with('_')
            {
                groups.push(name.to_string());
            }
        }
    }
    groups
}

// --- Hosts from /etc/hosts + ~/.ssh/known_hosts ---

fn load_hosts() -> Vec<String> {
    let mut hosts = Vec::new();
    // /etc/hosts
    if let Ok(file) = std::fs::File::open("/etc/hosts") {
        for line in std::io::BufReader::new(file).lines().map_while(Result::ok) {
            let trimmed = line.trim();
            if trimmed.is_empty() || trimmed.starts_with('#') {
                continue;
            }
            for name in trimmed.split_whitespace().skip(1) {
                if name != "localhost" && !name.starts_with('#') {
                    hosts.push(name.to_string());
                }
            }
        }
    }
    // ~/.ssh/known_hosts
    if let Some(home) = dirs::home_dir() {
        let kh = home.join(".ssh/known_hosts");
        if let Ok(file) = std::fs::File::open(kh) {
            for line in std::io::BufReader::new(file).lines().map_while(Result::ok) {
                let trimmed = line.trim();
                if trimmed.is_empty() || trimmed.starts_with('#') || trimmed.starts_with('|') {
                    continue; // skip hashed entries
                }
                if let Some(host_part) = trimmed.split_whitespace().next() {
                    for host in host_part.split(',') {
                        let host = host.trim_start_matches('[');
                        let host = host.split(']').next().unwrap_or(host);
                        if !host.is_empty() {
                            hosts.push(host.to_string());
                        }
                    }
                }
            }
        }
    }
    hosts.sort();
    hosts.dedup();
    hosts
}

// --- Network interfaces ---

fn load_net_ifaces() -> Vec<String> {
    let mut ifaces = Vec::new();
    if let Ok(output) = std::process::Command::new("ifconfig").arg("-l").output()
        && output.status.success()
    {
        for name in String::from_utf8_lossy(&output.stdout).split_whitespace() {
            ifaces.push(name.to_string());
        }
    }
    // Fallback: ls /sys/class/net (Linux)
    if ifaces.is_empty()
        && let Ok(entries) = std::fs::read_dir("/sys/class/net")
    {
        for entry in entries.flatten() {
            ifaces.push(entry.file_name().to_string_lossy().to_string());
        }
    }
    ifaces
}

// --- Locales ---

fn load_locales() -> Vec<String> {
    let mut locales = Vec::new();
    if let Ok(output) = std::process::Command::new("locale").arg("-a").output()
        && output.status.success()
    {
        for line in String::from_utf8_lossy(&output.stdout).lines() {
            let name = line.trim();
            if !name.is_empty() {
                locales.push(name.to_string());
            }
        }
    }
    locales
}

// --- Git queries (CWD-dependent, never cached) ---

fn git_query(args: &[&str]) -> Vec<String> {
    let output = match std::process::Command::new("git")
        .args(args)
        .stderr(std::process::Stdio::null())
        .output()
    {
        Ok(o) if o.status.success() => o,
        _ => return Vec::new(),
    };
    String::from_utf8_lossy(&output.stdout)
        .lines()
        .map(|l| l.trim().to_string())
        .filter(|l| !l.is_empty())
        .collect()
}

pub fn git_branches() -> Vec<String> {
    git_query(&["for-each-ref", "--format=%(refname:short)", "refs/heads"])
}

pub fn git_tags() -> Vec<String> {
    git_query(&["for-each-ref", "--format=%(refname:short)", "refs/tags"])
}

pub fn git_remotes() -> Vec<String> {
    git_query(&["remote"])
}

pub fn git_tracked_files() -> Vec<String> {
    let mut files = git_query(&["ls-files", "--cached", "--others", "--exclude-standard"]);
    // Also include modified files
    files.extend(git_query(&["diff", "--name-only"]));
    files.sort();
    files.dedup();
    files
}

// --- Public API ---

/// Resolve a prefix against the completions for a given arg type.
/// Returns the unique match if exactly one, or None if zero or ambiguous.
pub fn resolve_prefix(arg_type: u8, prefix: &str) -> Option<String> {
    let matches = list_matches(arg_type, prefix);
    if matches.len() == 1 {
        Some(matches[0].clone())
    } else {
        None
    }
}

/// List all entries matching a prefix for a given arg type.
pub fn list_matches(arg_type: u8, prefix: &str) -> Vec<String> {
    use crate::trie;
    let prefix_lower = prefix.to_lowercase();

    match arg_type {
        trie::ARG_MODE_SIGNALS => {
            let cache = get_or_init_cache();
            let c = cache.as_ref().unwrap();
            // Signals: match with or without SIG prefix
            let stripped = prefix.strip_prefix("SIG").unwrap_or(prefix);
            c.signals
                .iter()
                .filter(|s| {
                    s.starts_with(stripped)
                        || s.to_lowercase().starts_with(&stripped.to_lowercase())
                })
                .cloned()
                .collect()
        }
        trie::ARG_MODE_PORTS => {
            let cache = get_or_init_cache();
            let c = cache.as_ref().unwrap();
            c.ports
                .keys()
                .filter(|k| k.starts_with(prefix) || k.to_lowercase().starts_with(&prefix_lower))
                .cloned()
                .collect()
        }
        trie::ARG_MODE_USERS => {
            let cache = get_or_init_cache();
            let c = cache.as_ref().unwrap();
            c.users
                .iter()
                .filter(|u| u.starts_with(prefix) || u.to_lowercase().starts_with(&prefix_lower))
                .cloned()
                .collect()
        }
        trie::ARG_MODE_GROUPS => {
            let cache = get_or_init_cache();
            let c = cache.as_ref().unwrap();
            c.groups
                .iter()
                .filter(|g| g.starts_with(prefix) || g.to_lowercase().starts_with(&prefix_lower))
                .cloned()
                .collect()
        }
        trie::ARG_MODE_HOSTS => {
            let cache = get_or_init_cache();
            let c = cache.as_ref().unwrap();
            c.hosts
                .iter()
                .filter(|h| h.starts_with(prefix) || h.to_lowercase().starts_with(&prefix_lower))
                .cloned()
                .collect()
        }
        trie::ARG_MODE_NET_IFACES => {
            let cache = get_or_init_cache();
            let c = cache.as_ref().unwrap();
            c.net_ifaces
                .iter()
                .filter(|i| i.starts_with(prefix))
                .cloned()
                .collect()
        }
        trie::ARG_MODE_LOCALES => {
            let cache = get_or_init_cache();
            let c = cache.as_ref().unwrap();
            c.locales
                .iter()
                .filter(|l| l.starts_with(prefix) || l.to_lowercase().starts_with(&prefix_lower))
                .cloned()
                .collect()
        }
        trie::ARG_MODE_GIT_BRANCHES => git_branches()
            .into_iter()
            .filter(|b| b.starts_with(prefix) || b.to_lowercase().starts_with(&prefix_lower))
            .collect(),
        trie::ARG_MODE_GIT_TAGS => git_tags()
            .into_iter()
            .filter(|t| t.starts_with(prefix) || t.to_lowercase().starts_with(&prefix_lower))
            .collect(),
        trie::ARG_MODE_GIT_REMOTES => git_remotes()
            .into_iter()
            .filter(|r| r.starts_with(prefix) || r.to_lowercase().starts_with(&prefix_lower))
            .collect(),
        trie::ARG_MODE_GIT_FILES => git_tracked_files()
            .into_iter()
            .filter(|f| f.starts_with(prefix) || f.to_lowercase().starts_with(&prefix_lower))
            .collect(),
        _ => Vec::new(),
    }
}

/// Human-readable description of what this arg type expects.
pub fn type_hint(arg_type: u8) -> &'static str {
    use crate::trie;
    match arg_type {
        trie::ARG_MODE_PATHS => "<file>",
        trie::ARG_MODE_DIRS_ONLY => "<directory>",
        trie::ARG_MODE_EXECS_ONLY => "<command>",
        trie::ARG_MODE_USERS => "<user>",
        trie::ARG_MODE_GROUPS => "<group>",
        trie::ARG_MODE_HOSTS => "<host>",
        trie::ARG_MODE_PIDS => "<pid>",
        trie::ARG_MODE_SIGNALS => "<signal>",
        trie::ARG_MODE_PORTS => "<port>",
        trie::ARG_MODE_NET_IFACES => "<interface>",
        trie::ARG_MODE_GIT_BRANCHES => "<branch>",
        trie::ARG_MODE_GIT_TAGS => "<tag>",
        trie::ARG_MODE_GIT_REMOTES => "<remote>",
        trie::ARG_MODE_GIT_FILES => "<tracked-file>",
        trie::ARG_MODE_URLS => "<url>",
        trie::ARG_MODE_LOCALES => "<locale>",
        _ => "<arg>",
    }
}
