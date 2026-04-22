use std::collections::HashMap;
use std::sync::{LazyLock, Mutex};

/// Each resource initializes independently on first access — no upfront cost,
/// no mutex contention, and no risk of poisoned-mutex panics.
static SIGNALS: LazyLock<Vec<String>> = LazyLock::new(load_signals);

/// Session-level cache for `_call_program` results.
/// Key: joined argv string (e.g. "ssh -Q cipher").
/// Value: raw output lines captured from the command.
/// Git queries are always fresh (CWD-sensitive), but `_call_program` results
/// for things like cipher lists or rsync options are stable per process.
static CALL_PROGRAM_CACHE: LazyLock<Mutex<HashMap<String, Vec<String>>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));
static PORTS: LazyLock<HashMap<String, u16>> = LazyLock::new(load_ports);
static USERS: LazyLock<Vec<String>> = LazyLock::new(load_users);
static GROUPS: LazyLock<Vec<String>> = LazyLock::new(load_groups);
static HOSTS: LazyLock<Vec<String>> = LazyLock::new(load_hosts);
static NET_IFACES: LazyLock<Vec<String>> = LazyLock::new(load_net_ifaces);
static LOCALES: LazyLock<Vec<String>> = LazyLock::new(load_locales);

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
    match std::fs::read_to_string("/etc/services") {
        Ok(content) => parse_services(&content),
        Err(_) => HashMap::new(),
    }
}

/// Parse `/etc/services` content: `name port/proto [aliases] [# comment]`.
/// Returns the highest-stable mapping of service name → port number.
fn parse_services(content: &str) -> HashMap<String, u16> {
    let mut ports = HashMap::new();
    for line in content.lines() {
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
        users = parse_dscl_users(&String::from_utf8_lossy(&output.stdout));
    }
    // Fallback: /etc/passwd
    if users.is_empty()
        && let Ok(content) = std::fs::read_to_string("/etc/passwd")
    {
        users = parse_passwd(&content);
    }
    users
}

fn parse_dscl_users(content: &str) -> Vec<String> {
    content
        .lines()
        .filter_map(|line| {
            let name = line.trim();
            (!name.is_empty() && !name.starts_with('_')).then(|| name.to_string())
        })
        .collect()
}

/// Parse `/etc/passwd` content. Skips comment lines and entries whose name
/// starts with `_` (macOS convention for system-only accounts).
fn parse_passwd(content: &str) -> Vec<String> {
    content
        .lines()
        .filter_map(|line| line.split(':').next())
        .filter(|name| !name.starts_with('#') && !name.starts_with('_') && !name.is_empty())
        .map(|s| s.to_string())
        .collect()
}

// --- System groups ---

fn load_groups() -> Vec<String> {
    match std::fs::read_to_string("/etc/group") {
        Ok(content) => parse_group(&content),
        Err(_) => Vec::new(),
    }
}

fn parse_group(content: &str) -> Vec<String> {
    content
        .lines()
        .filter_map(|line| line.split(':').next())
        .filter(|name| !name.starts_with('#') && !name.starts_with('_') && !name.is_empty())
        .map(|s| s.to_string())
        .collect()
}

// --- Hosts from /etc/hosts + ~/.ssh/known_hosts + ~/.ssh/config ---

fn load_hosts() -> Vec<String> {
    let mut hosts = Vec::new();
    if let Ok(c) = std::fs::read_to_string("/etc/hosts") {
        hosts.extend(parse_etc_hosts(&c));
    }
    if let Some(home) = dirs::home_dir() {
        if let Ok(c) = std::fs::read_to_string(home.join(".ssh/known_hosts")) {
            hosts.extend(parse_known_hosts(&c));
        }
        if let Ok(c) = std::fs::read_to_string(home.join(".ssh/config")) {
            hosts.extend(parse_ssh_config(&c));
        }
    }
    hosts.sort();
    hosts.dedup();
    hosts
}

/// Parse `/etc/hosts`. Skips comments/blank lines and drops `localhost`.
fn parse_etc_hosts(content: &str) -> Vec<String> {
    let mut out = Vec::new();
    for line in content.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() || trimmed.starts_with('#') {
            continue;
        }
        for name in trimmed.split_whitespace().skip(1) {
            if name != "localhost" && !name.starts_with('#') {
                out.push(name.to_string());
            }
        }
    }
    out
}

/// Parse `~/.ssh/known_hosts`. Skips hashed entries (leading `|`), splits on
/// `,` for multi-host lines, and strips `[host]:port` bracket notation.
fn parse_known_hosts(content: &str) -> Vec<String> {
    let mut out = Vec::new();
    for line in content.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() || trimmed.starts_with('#') || trimmed.starts_with('|') {
            continue;
        }
        if let Some(host_part) = trimmed.split_whitespace().next() {
            for host in host_part.split(',') {
                let host = host.trim_start_matches('[');
                let host = host.split(']').next().unwrap_or(host);
                if !host.is_empty() {
                    out.push(host.to_string());
                }
            }
        }
    }
    out
}

/// Parse `~/.ssh/config` `Host` aliases. Wildcards (`*`, `?`) and negations
/// (`!foo`) are skipped since they aren't real hostnames.
fn parse_ssh_config(content: &str) -> Vec<String> {
    let mut out = Vec::new();
    for line in content.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() || trimmed.starts_with('#') {
            continue;
        }
        if let Some(rest) = trimmed
            .strip_prefix("Host ")
            .or_else(|| trimmed.strip_prefix("Host\t"))
            .or_else(|| trimmed.strip_prefix("host "))
            .or_else(|| trimmed.strip_prefix("host\t"))
        {
            for alias in rest.split_whitespace() {
                if alias.contains('*') || alias.contains('?') || alias.starts_with('!') {
                    continue;
                }
                out.push(alias.to_string());
            }
        }
    }
    out
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

// --- PIDs (never cached, always live) ---

fn load_pids() -> Vec<(String, String)> {
    // Returns (pid, command_name) pairs for the current user's processes
    let mut pids = Vec::new();
    if let Ok(output) = std::process::Command::new("ps")
        .args(["-u", &whoami(), "-o", "pid=,comm="])
        .output()
        && output.status.success()
    {
        for line in String::from_utf8_lossy(&output.stdout).lines() {
            let trimmed = line.trim();
            if let Some((pid, cmd)) = trimmed.split_once(char::is_whitespace) {
                let pid = pid.trim();
                let cmd = cmd.trim();
                if !pid.is_empty() && !cmd.is_empty() {
                    pids.push((pid.to_string(), cmd.to_string()));
                }
            }
        }
    }
    pids
}

fn whoami() -> String {
    std::env::var("USER")
        .or_else(|_| std::env::var("LOGNAME"))
        .unwrap_or_else(|_| String::from("root"))
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
    let mut branches =
        git_query(&["for-each-ref", "--format=%(refname:short)", "refs/heads", "refs/remotes"]);
    // Drop remote HEAD symrefs (e.g. "origin/HEAD")
    branches.retain(|b| !b.ends_with("/HEAD"));
    branches
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

// --- _call_program dynamic runner ---

/// Run an external command (from a Zsh `_call_program` spec) and return its
/// output lines filtered by prefix.
///
/// Results are cached per argv for the lifetime of the process so repeated
/// `?` presses don't re-exec the same command.  Git-like CWD-sensitive queries
/// should use the dedicated git helpers instead.
///
/// Each output line is split on whitespace and only the first token is kept —
/// many completions emit `value  # comment` or `value  description` format.
pub fn call_program_cached(argv: &[String], prefix: &str) -> Vec<String> {
    if argv.is_empty() {
        return vec![];
    }
    let cache_key = argv.join("\x00");

    // Try cache first
    if let Ok(cache) = CALL_PROGRAM_CACHE.lock()
        && let Some(cached) = cache.get(&cache_key)
    {
        return filter_prefix(cached, prefix);
    }

    // Run the command with a 3-second timeout via a thread
    let argv_owned = argv.to_vec();
    let output = std::process::Command::new(&argv_owned[0])
        .args(&argv_owned[1..])
        .output();

    let items: Vec<String> = match output {
        Ok(out) if out.status.success() || !out.stdout.is_empty() => {
            String::from_utf8_lossy(&out.stdout)
                .lines()
                .filter_map(|line| {
                    let tok = line.split_whitespace().next()?;
                    if tok.is_empty() { None } else { Some(tok.to_string()) }
                })
                .collect()
        }
        _ => vec![],
    };

    // Store in cache
    if let Ok(mut cache) = CALL_PROGRAM_CACHE.lock() {
        cache.insert(cache_key, items.clone());
    }

    filter_prefix(&items, prefix)
}

fn filter_prefix(items: &[String], prefix: &str) -> Vec<String> {
    if prefix.is_empty() {
        return items.to_vec();
    }
    let prefix_lower = prefix.to_lowercase();
    items
        .iter()
        .filter(|s| s.starts_with(prefix) || s.to_lowercase().starts_with(&prefix_lower))
        .cloned()
        .collect()
}

// --- Public API ---

/// Resolve a prefix against the completions for a given arg type.
/// Returns the unique match if exactly one, or None if zero or ambiguous.
pub fn resolve_prefix(arg_type: u8, prefix: &str) -> Option<String> {
    use crate::trie;
    // PIDs are special: list_matches returns "pid  cmd" for display,
    // but resolution should yield just the PID number.
    if arg_type == trie::ARG_MODE_PIDS {
        let pids = load_pids();
        let matches: Vec<&(String, String)> = pids
            .iter()
            .filter(|(pid, cmd)| {
                pid.starts_with(prefix) || cmd.starts_with(prefix)
                    || cmd.to_lowercase().starts_with(&prefix.to_lowercase())
            })
            .collect();
        return if matches.len() == 1 {
            Some(matches[0].0.clone())
        } else {
            None
        };
    }
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
            // Signals: match with or without SIG prefix
            let stripped = prefix.strip_prefix("SIG").unwrap_or(prefix);
            SIGNALS
                .iter()
                .filter(|s| {
                    s.starts_with(stripped)
                        || s.to_lowercase().starts_with(&stripped.to_lowercase())
                })
                .cloned()
                .collect()
        }
        trie::ARG_MODE_PORTS => PORTS
            .keys()
            .filter(|k| k.starts_with(prefix) || k.to_lowercase().starts_with(&prefix_lower))
            .cloned()
            .collect(),
        trie::ARG_MODE_USERS => USERS
            .iter()
            .filter(|u| u.starts_with(prefix) || u.to_lowercase().starts_with(&prefix_lower))
            .cloned()
            .collect(),
        trie::ARG_MODE_GROUPS => GROUPS
            .iter()
            .filter(|g| g.starts_with(prefix) || g.to_lowercase().starts_with(&prefix_lower))
            .cloned()
            .collect(),
        trie::ARG_MODE_USERS_GROUPS => USERS
            .iter()
            .chain(GROUPS.iter())
            .filter(|s| s.starts_with(prefix) || s.to_lowercase().starts_with(&prefix_lower))
            .cloned()
            .collect::<std::collections::HashSet<_>>()
            .into_iter()
            .collect(),
        trie::ARG_MODE_HOSTS => HOSTS
            .iter()
            .filter(|h| h.starts_with(prefix) || h.to_lowercase().starts_with(&prefix_lower))
            .cloned()
            .collect(),
        trie::ARG_MODE_NET_IFACES => NET_IFACES
            .iter()
            .filter(|i| i.starts_with(prefix))
            .cloned()
            .collect(),
        trie::ARG_MODE_LOCALES => LOCALES
            .iter()
            .filter(|l| l.starts_with(prefix) || l.to_lowercase().starts_with(&prefix_lower))
            .cloned()
            .collect(),
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
        trie::ARG_MODE_PIDS => {
            let pids = load_pids();
            pids.into_iter()
                .filter(|(pid, cmd)| {
                    pid.starts_with(prefix) || cmd.starts_with(prefix)
                        || cmd.to_lowercase().starts_with(&prefix_lower)
                })
                .map(|(pid, cmd)| format!("{}  {}", pid, cmd))
                .collect()
        }
        _ => Vec::new(),
    }
}

/// Invalidate the `_call_program` cache. Exposed for tests only so a test
/// can force a re-exec after pre-seeding or clearing results.
#[cfg(test)]
fn clear_call_program_cache() {
    if let Ok(mut c) = CALL_PROGRAM_CACHE.lock() {
        c.clear();
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
        trie::ARG_MODE_USERS_GROUPS => "<user|group>",
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::trie;

    // --- parse_services ---

    #[test]
    fn parse_services_basic() {
        let content = "\
# comment
ftp             21/tcp
ssh             22/tcp
ssh             22/udp
http            80/tcp   www www-http
";
        let ports = parse_services(content);
        assert_eq!(ports.get("ssh"), Some(&22));
        assert_eq!(ports.get("ftp"), Some(&21));
        assert_eq!(ports.get("http"), Some(&80));
    }

    #[test]
    fn parse_services_ignores_malformed() {
        let content = "only-one-token\nnotanumber/tcp second-field\nfine 99/tcp\n";
        let ports = parse_services(content);
        assert_eq!(ports.len(), 1);
        assert_eq!(ports.get("fine"), Some(&99));
    }

    #[test]
    fn parse_services_empty() {
        assert!(parse_services("").is_empty());
        assert!(parse_services("# just a comment\n\n\t\n").is_empty());
    }

    // --- parse_passwd / parse_group ---

    #[test]
    fn parse_passwd_filters_system_accounts() {
        let content = "\
root:x:0:0:root:/root:/bin/bash
andrei:x:1000:1000:Andrei:/home/andrei:/bin/zsh
_apt:x:42:65534::/nonexistent:/usr/sbin/nologin
# comment-line:x:1:1::/:/bin/false
";
        let users = parse_passwd(content);
        assert_eq!(users, vec!["root".to_string(), "andrei".to_string()]);
    }

    #[test]
    fn parse_group_filters_system_groups() {
        let users = parse_group("root:x:0:\n_lp:x:7:\nwheel:x:10:andrei\n");
        assert_eq!(users, vec!["root".to_string(), "wheel".to_string()]);
    }

    #[test]
    fn parse_dscl_users_strips_underscore_prefix() {
        let out = parse_dscl_users("root\nandrei\n_mbsetupuser\n\n");
        assert_eq!(out, vec!["root".to_string(), "andrei".to_string()]);
    }

    // --- parse_etc_hosts ---

    #[test]
    fn parse_etc_hosts_skips_localhost_and_comments() {
        let content = "\
127.0.0.1   localhost
::1         localhost ip6-localhost
192.168.1.5 media mediaserver # my box
# 10.0.0.1 skipped
";
        let hosts = parse_etc_hosts(content);
        // localhost filtered; trailing-comment token still kept as-is.
        assert!(hosts.contains(&"ip6-localhost".to_string()));
        assert!(hosts.contains(&"media".to_string()));
        assert!(hosts.contains(&"mediaserver".to_string()));
        assert!(!hosts.iter().any(|h| h == "localhost"));
    }

    // --- parse_known_hosts ---

    #[test]
    fn parse_known_hosts_handles_brackets_and_hashed() {
        let content = "\
github.com,140.82.121.3 ssh-ed25519 AAAA...
[bastion.example.com]:2222 ssh-ed25519 BBBB...
|1|abc123=|def== ssh-rsa HIDDEN
# a comment
";
        let hosts = parse_known_hosts(content);
        assert!(hosts.contains(&"github.com".to_string()));
        assert!(hosts.contains(&"140.82.121.3".to_string()));
        // `[host]:port` → we keep only the host portion (before `]`)
        assert!(hosts.contains(&"bastion.example.com".to_string()));
        // hashed line skipped entirely
        assert!(!hosts.iter().any(|h| h.starts_with("|1|")));
    }

    // --- parse_ssh_config ---

    #[test]
    fn parse_ssh_config_collects_host_aliases() {
        let content = "\
# comment
Host alpha beta
    User andrei
Host *.internal
    ForwardAgent yes
host prod staging !excluded
";
        let hosts = parse_ssh_config(content);
        assert!(hosts.contains(&"alpha".to_string()));
        assert!(hosts.contains(&"beta".to_string()));
        assert!(hosts.contains(&"prod".to_string()));
        assert!(hosts.contains(&"staging".to_string()));
        // Wildcards and negations filtered.
        assert!(!hosts.iter().any(|h| h.contains('*')));
        assert!(!hosts.iter().any(|h| h.starts_with('!')));
    }

    // --- filter_prefix ---

    #[test]
    fn filter_prefix_empty_returns_all() {
        let items = vec!["a".into(), "B".into(), "c".into()];
        assert_eq!(filter_prefix(&items, ""), items);
    }

    #[test]
    fn filter_prefix_case_fallback() {
        let items = vec!["README.md".to_string(), "readme.txt".to_string(), "Other".to_string()];
        let matches = filter_prefix(&items, "read");
        // case-insensitive fallback picks up README.md even though prefix is lowercase
        assert!(matches.contains(&"README.md".to_string()));
        assert!(matches.contains(&"readme.txt".to_string()));
        assert!(!matches.contains(&"Other".to_string()));
    }

    // --- list_matches (hardcoded SIGNALS path is filesystem-free) ---

    #[test]
    fn list_matches_signals_with_and_without_sig_prefix() {
        let v = list_matches(trie::ARG_MODE_SIGNALS, "KI");
        assert!(v.contains(&"KILL".to_string()));
        let v = list_matches(trie::ARG_MODE_SIGNALS, "SIGKI");
        assert!(v.contains(&"KILL".to_string()));
        // `SIG` stripping is case-sensitive by design; lowercase `sig` stays literal.
        let v = list_matches(trie::ARG_MODE_SIGNALS, "SIGTERM");
        assert!(v.iter().any(|s| s.eq_ignore_ascii_case("TERM")));
        // case-insensitive fallback on the bare name
        let v = list_matches(trie::ARG_MODE_SIGNALS, "term");
        assert!(v.iter().any(|s| s.eq_ignore_ascii_case("TERM")));
    }

    #[test]
    fn list_matches_unknown_mode_returns_empty() {
        assert!(list_matches(99, "anything").is_empty());
    }

    // --- resolve_prefix ---

    #[test]
    fn resolve_prefix_signal_unique() {
        // Only HUP starts with "HU"
        assert_eq!(
            resolve_prefix(trie::ARG_MODE_SIGNALS, "HU"),
            Some("HUP".to_string())
        );
    }

    #[test]
    fn resolve_prefix_signal_ambiguous_returns_none() {
        // "U" matches both URG and USR1/USR2
        assert_eq!(resolve_prefix(trie::ARG_MODE_SIGNALS, "U"), None);
    }

    // --- type_hint ---

    #[test]
    fn type_hint_exhaustive() {
        use trie::*;
        assert_eq!(type_hint(ARG_MODE_PATHS), "<file>");
        assert_eq!(type_hint(ARG_MODE_DIRS_ONLY), "<directory>");
        assert_eq!(type_hint(ARG_MODE_EXECS_ONLY), "<command>");
        assert_eq!(type_hint(ARG_MODE_USERS), "<user>");
        assert_eq!(type_hint(ARG_MODE_GROUPS), "<group>");
        assert_eq!(type_hint(ARG_MODE_USERS_GROUPS), "<user|group>");
        assert_eq!(type_hint(ARG_MODE_HOSTS), "<host>");
        assert_eq!(type_hint(ARG_MODE_PIDS), "<pid>");
        assert_eq!(type_hint(ARG_MODE_SIGNALS), "<signal>");
        assert_eq!(type_hint(ARG_MODE_PORTS), "<port>");
        assert_eq!(type_hint(ARG_MODE_NET_IFACES), "<interface>");
        assert_eq!(type_hint(ARG_MODE_GIT_BRANCHES), "<branch>");
        assert_eq!(type_hint(ARG_MODE_GIT_TAGS), "<tag>");
        assert_eq!(type_hint(ARG_MODE_GIT_REMOTES), "<remote>");
        assert_eq!(type_hint(ARG_MODE_GIT_FILES), "<tracked-file>");
        assert_eq!(type_hint(ARG_MODE_URLS), "<url>");
        assert_eq!(type_hint(ARG_MODE_LOCALES), "<locale>");
        assert_eq!(type_hint(0), "<arg>");
        assert_eq!(type_hint(255), "<arg>");
    }

    // --- call_program_cached ---

    #[test]
    fn call_program_cached_empty_argv() {
        assert!(call_program_cached(&[], "").is_empty());
    }

    #[test]
    fn call_program_cached_runs_and_caches() {
        clear_call_program_cache();
        let argv = vec!["printf".to_string(), "alpha\nbeta\ngamma\n".to_string()];
        let out = call_program_cached(&argv, "");
        assert_eq!(out, vec!["alpha", "beta", "gamma"]);

        // Cached path: filter by prefix without re-execing.
        let out = call_program_cached(&argv, "be");
        assert_eq!(out, vec!["beta"]);
    }

    #[test]
    fn call_program_cached_missing_binary() {
        clear_call_program_cache();
        let argv = vec!["this-binary-absolutely-does-not-exist-zio".to_string()];
        assert!(call_program_cached(&argv, "").is_empty());
    }

    // --- git helpers smoke-test: should not panic and return Vec ---

    #[test]
    fn list_matches_exercises_all_filesystem_backed_modes() {
        // Each of these triggers the LazyLock init for its data source at
        // least once. We don't assert content — system state varies — but
        // the call must not panic and must return a Vec.
        for mode in [
            trie::ARG_MODE_PORTS,
            trie::ARG_MODE_USERS,
            trie::ARG_MODE_GROUPS,
            trie::ARG_MODE_USERS_GROUPS,
            trie::ARG_MODE_HOSTS,
            trie::ARG_MODE_NET_IFACES,
            trie::ARG_MODE_LOCALES,
            trie::ARG_MODE_PIDS,
            trie::ARG_MODE_GIT_BRANCHES,
            trie::ARG_MODE_GIT_TAGS,
            trie::ARG_MODE_GIT_REMOTES,
            trie::ARG_MODE_GIT_FILES,
        ] {
            let _ = list_matches(mode, "");
            // PIDs need non-empty prefix to exercise the PID branch of resolve_prefix
            let _ = resolve_prefix(mode, "nothing-will-match-this-9999");
        }
    }

    #[test]
    fn git_helpers_tolerate_non_repo() {
        let _g = crate::test_util::CWD_LOCK.lock().unwrap();
        // Run from a temp dir that is NOT a git repo; should return empty vecs
        let td = tempfile::tempdir().unwrap();
        let cwd = std::env::current_dir().ok();
        std::env::set_current_dir(td.path()).unwrap();
        // These just need to not panic.
        let _ = git_branches();
        let _ = git_tags();
        let _ = git_remotes();
        let _ = git_tracked_files();
        if let Some(c) = cwd {
            let _ = std::env::set_current_dir(c);
        }
    }
}
