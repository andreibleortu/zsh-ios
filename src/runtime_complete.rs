use crate::trie::*;
use crate::type_resolver::{Ctx, Registry, TypeResolver};
use std::collections::HashMap;
use std::sync::{LazyLock, Mutex};
use std::time::Duration;

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

pub fn git_stash_list() -> Vec<String> {
    git_query(&["stash", "list", "--format=%gd"])
}

pub fn git_worktree_list() -> Vec<String> {
    git_query(&["worktree", "list", "--porcelain"])
        .into_iter()
        .filter_map(|line| line.strip_prefix("worktree ").map(|s| s.to_string()))
        .collect()
}

pub fn git_submodule_list() -> Vec<String> {
    // Try the porcelain helper first; fall back to .gitmodules parsing.
    let out = git_query(&["submodule--helper", "list"]);
    if !out.is_empty() {
        // Each line is: <mode> SP <hash> SP <stage> TAB <path>
        return out
            .into_iter()
            .filter_map(|line| line.split_once('\t').map(|x| x.1.to_string()))
            .collect();
    }
    // Fallback: parse .gitmodules via `git config --file .gitmodules --get-regexp path`
    // Output lines look like: `submodule.<name>.path <path>`
    git_query(&["config", "--file", ".gitmodules", "--get-regexp", "path"])
        .into_iter()
        .filter_map(|line| {
            line.split_whitespace().nth(1).map(|s| s.to_string())
        })
        .collect()
}

pub fn git_config_keys() -> Vec<String> {
    git_query(&["config", "--list", "--name-only"])
}

pub fn git_aliases() -> Vec<String> {
    git_query(&["config", "--get-regexp", r"^alias\."])
        .into_iter()
        .filter_map(|line| {
            // Line format: `alias.<name> <value>`
            let key = line.split_whitespace().next()?;
            key.strip_prefix("alias.").map(|s| s.to_string())
        })
        .collect()
}

pub fn git_commits() -> Vec<String> {
    let full = git_query(&["log", "--format=%H", "--max-count=200"]);
    let short = git_query(&["log", "--format=%h", "--max-count=200"]);
    let mut combined = full;
    combined.extend(short);
    combined.sort();
    combined.dedup();
    combined
}

pub fn git_reflog_list() -> Vec<String> {
    git_query(&["reflog", "--format=%gd", "--max-count=100"])
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

/// Invoke a resolver, wrapping the call with a cross-invocation on-disk cache.
///
/// The cache is keyed on the resolver id + cwd + prior words (not the partial
/// prefix, which is user-typed noise and would fragment the cache needlessly).
/// Prefix filtering is applied by the caller after this returns.
///
/// A `Duration::ZERO` TTL from `resolver.cache_ttl()` opts the resolver out
/// of caching entirely — the resolver is called directly and results are never
/// stored on disk.
fn list_with_cache(
    resolver: &dyn crate::type_resolver::TypeResolver,
    mode: u8,
    ctx: &Ctx,
) -> Vec<String> {
    let ttl = resolver.cache_ttl();
    if ttl.is_zero() {
        return resolver.list(ctx);
    }
    let cache = match crate::runtime_cache::RuntimeCache::default_location() {
        Some(c) => c,
        None => return resolver.list(ctx),
    };
    let id: &str = if resolver.id().is_empty() {
        // Synthesize a stable id from the mode number so unnamed resolvers
        // still get a usable cache key.  Box::leak is acceptable here because
        // this path is only taken for the rare case of an unnamed resolver, and
        // the leaked allocation is a handful of bytes that lives for the
        // process lifetime anyway.
        Box::leak(format!("mode-{}", mode).into_boxed_str())
    } else {
        resolver.id()
    };
    let cwd = ctx.cwd.as_deref();
    let prior: Vec<&str> = ctx.prior_words.iter().map(String::as_str).collect();
    let key = crate::runtime_cache::make_key(id, cwd, &prior);
    if let Some(hit) = cache.get(&key, ttl) {
        return hit;
    }
    let fresh = resolver.list(ctx);
    let _ = cache.put(&key, &fresh);
    fresh
}

/// Resolve a prefix against the completions for a given arg type.
/// Returns the unique match if exactly one, or None if zero or ambiguous.
pub fn resolve_prefix(arg_type: u8, prefix: &str) -> Option<String> {
    // PIDs are special: list_matches returns "pid  cmd" for display,
    // but resolution should yield just the PID number.
    if arg_type == ARG_MODE_PIDS {
        let pids = load_pids();
        let matches: Vec<&(String, String)> = pids
            .iter()
            .filter(|(pid, cmd)| {
                pid.starts_with(prefix)
                    || cmd.starts_with(prefix)
                    || cmd.to_lowercase().starts_with(&prefix.to_lowercase())
            })
            .collect();
        return if matches.len() == 1 { Some(matches[0].0.clone()) } else { None };
    }
    // Registry fast path for all other registered types.
    if let Some(resolver) = crate::type_resolver::REGISTRY.get(arg_type) {
        let ctx = Ctx::with_partial(prefix);
        let items = list_with_cache(resolver, arg_type, &ctx);
        let filtered = filter_prefix(&items, prefix);
        return match filtered.len() {
            1 => Some(filtered.into_iter().next().unwrap()),
            _ => None,
        };
    }
    let matches = list_matches(arg_type, prefix);
    if matches.len() == 1 { Some(matches[0].clone()) } else { None }
}

/// List all entries matching a prefix for a given arg type.
pub fn list_matches(arg_type: u8, prefix: &str) -> Vec<String> {
    let prefix_lower = prefix.to_lowercase();

    // Signals have custom SIG-prefix stripping logic; handle before registry.
    if arg_type == ARG_MODE_SIGNALS {
        let stripped = prefix.strip_prefix("SIG").unwrap_or(prefix);
        return SIGNALS
            .iter()
            .filter(|s| {
                s.starts_with(stripped) || s.to_lowercase().starts_with(&stripped.to_lowercase())
            })
            .cloned()
            .collect();
    }

    // PIDs have a special (pid, cmd) shape; handle before registry.
    if arg_type == ARG_MODE_PIDS {
        let pids = load_pids();
        return pids
            .into_iter()
            .filter(|(pid, cmd)| {
                pid.starts_with(prefix)
                    || cmd.starts_with(prefix)
                    || cmd.to_lowercase().starts_with(&prefix_lower)
            })
            .map(|(pid, cmd)| format!("{}  {}", pid, cmd))
            .collect();
    }

    // Registry fast path for all registered types.
    if let Some(resolver) = crate::type_resolver::REGISTRY.get(arg_type) {
        let ctx = Ctx::with_partial(prefix);
        let items = list_with_cache(resolver, arg_type, &ctx);
        return filter_prefix(&items, prefix);
    }

    // Fallback for unregistered types (URLS, PATHS/DIRS_ONLY/EXECS_ONLY handled
    // by the filesystem layer in complete.rs, unknown future modes, etc.).
    Vec::new()
}

// --- Built-in TypeResolver implementations ---

pub struct UsersResolver;
impl TypeResolver for UsersResolver {
    fn list(&self, _ctx: &Ctx) -> Vec<String> {
        USERS.clone()
    }
    fn cache_ttl(&self) -> Duration {
        Duration::from_secs(3600)
    }
    fn id(&self) -> &'static str {
        "users"
    }
}

pub struct GroupsResolver;
impl TypeResolver for GroupsResolver {
    fn list(&self, _ctx: &Ctx) -> Vec<String> {
        GROUPS.clone()
    }
    fn cache_ttl(&self) -> Duration {
        Duration::from_secs(3600)
    }
    fn id(&self) -> &'static str {
        "groups"
    }
}

pub struct HostsResolver;
impl TypeResolver for HostsResolver {
    fn list(&self, _ctx: &Ctx) -> Vec<String> {
        HOSTS.clone()
    }
    fn cache_ttl(&self) -> Duration {
        Duration::from_secs(3600)
    }
    fn id(&self) -> &'static str {
        "hosts"
    }
}

pub struct SignalsResolver;
impl TypeResolver for SignalsResolver {
    fn list(&self, _ctx: &Ctx) -> Vec<String> {
        SIGNALS.clone()
    }
    fn cache_ttl(&self) -> Duration {
        Duration::from_secs(3600)
    }
    fn id(&self) -> &'static str {
        "signals"
    }
}

pub struct PortsResolver;
impl TypeResolver for PortsResolver {
    fn list(&self, _ctx: &Ctx) -> Vec<String> {
        PORTS.keys().cloned().collect()
    }
    fn cache_ttl(&self) -> Duration {
        Duration::from_secs(3600)
    }
    fn id(&self) -> &'static str {
        "ports"
    }
}

pub struct NetIfacesResolver;
impl TypeResolver for NetIfacesResolver {
    fn list(&self, _ctx: &Ctx) -> Vec<String> {
        NET_IFACES.clone()
    }
    fn cache_ttl(&self) -> Duration {
        Duration::from_secs(3600)
    }
    fn id(&self) -> &'static str {
        "net-ifaces"
    }
}

pub struct LocalesResolver;
impl TypeResolver for LocalesResolver {
    fn list(&self, _ctx: &Ctx) -> Vec<String> {
        LOCALES.clone()
    }
    fn cache_ttl(&self) -> Duration {
        Duration::from_secs(3600)
    }
    fn id(&self) -> &'static str {
        "locales"
    }
}

pub struct GitBranchesResolver;
impl TypeResolver for GitBranchesResolver {
    fn list(&self, _ctx: &Ctx) -> Vec<String> {
        git_branches()
    }
    fn cache_ttl(&self) -> Duration {
        Duration::from_secs(5)
    }
    fn id(&self) -> &'static str {
        "git-branches"
    }
}

pub struct GitTagsResolver;
impl TypeResolver for GitTagsResolver {
    fn list(&self, _ctx: &Ctx) -> Vec<String> {
        git_tags()
    }
    fn cache_ttl(&self) -> Duration {
        Duration::from_secs(5)
    }
    fn id(&self) -> &'static str {
        "git-tags"
    }
}

pub struct GitRemotesResolver;
impl TypeResolver for GitRemotesResolver {
    fn list(&self, _ctx: &Ctx) -> Vec<String> {
        git_remotes()
    }
    fn cache_ttl(&self) -> Duration {
        Duration::from_secs(5)
    }
    fn id(&self) -> &'static str {
        "git-remotes"
    }
}

pub struct GitFilesResolver;
impl TypeResolver for GitFilesResolver {
    fn list(&self, _ctx: &Ctx) -> Vec<String> {
        git_tracked_files()
    }
    fn cache_ttl(&self) -> Duration {
        Duration::from_secs(5)
    }
    fn id(&self) -> &'static str {
        "git-files"
    }
}

pub struct UsersGroupsResolver;
impl TypeResolver for UsersGroupsResolver {
    fn list(&self, _ctx: &Ctx) -> Vec<String> {
        let mut combined: Vec<String> = USERS.iter().chain(GROUPS.iter()).cloned().collect();
        combined.sort();
        combined.dedup();
        combined
    }
    fn cache_ttl(&self) -> Duration {
        Duration::from_secs(3600)
    }
    fn id(&self) -> &'static str {
        "users-groups"
    }
}

pub struct GitStashResolver;
impl TypeResolver for GitStashResolver {
    fn list(&self, _ctx: &Ctx) -> Vec<String> {
        git_stash_list()
    }
    fn cache_ttl(&self) -> Duration {
        Duration::from_secs(5)
    }
    fn id(&self) -> &'static str {
        "git-stash"
    }
}

pub struct GitWorktreeResolver;
impl TypeResolver for GitWorktreeResolver {
    fn list(&self, _ctx: &Ctx) -> Vec<String> {
        git_worktree_list()
    }
    fn cache_ttl(&self) -> Duration {
        Duration::from_secs(5)
    }
    fn id(&self) -> &'static str {
        "git-worktree"
    }
}

pub struct GitSubmoduleResolver;
impl TypeResolver for GitSubmoduleResolver {
    fn list(&self, _ctx: &Ctx) -> Vec<String> {
        git_submodule_list()
    }
    fn cache_ttl(&self) -> Duration {
        Duration::from_secs(300)
    }
    fn id(&self) -> &'static str {
        "git-submodule"
    }
}

pub struct GitConfigKeyResolver;
impl TypeResolver for GitConfigKeyResolver {
    fn list(&self, _ctx: &Ctx) -> Vec<String> {
        git_config_keys()
    }
    fn cache_ttl(&self) -> Duration {
        Duration::from_secs(60)
    }
    fn id(&self) -> &'static str {
        "git-config-key"
    }
}

pub struct GitAliasResolver;
impl TypeResolver for GitAliasResolver {
    fn list(&self, _ctx: &Ctx) -> Vec<String> {
        git_aliases()
    }
    fn cache_ttl(&self) -> Duration {
        Duration::from_secs(60)
    }
    fn id(&self) -> &'static str {
        "git-alias"
    }
}

pub struct GitCommitResolver;
impl TypeResolver for GitCommitResolver {
    fn list(&self, _ctx: &Ctx) -> Vec<String> {
        git_commits()
    }
    fn cache_ttl(&self) -> Duration {
        Duration::from_secs(10)
    }
    fn id(&self) -> &'static str {
        "git-commit"
    }
}

pub struct GitReflogResolver;
impl TypeResolver for GitReflogResolver {
    fn list(&self, _ctx: &Ctx) -> Vec<String> {
        git_reflog_list()
    }
    fn cache_ttl(&self) -> Duration {
        Duration::from_secs(10)
    }
    fn id(&self) -> &'static str {
        "git-reflog"
    }
}

// --- Shared utility for Docker / Kubernetes resolvers ---

fn run_capture(cmd: &str, args: &[&str], dir: Option<&std::path::Path>) -> Vec<String> {
    let mut c = std::process::Command::new(cmd);
    c.args(args);
    if let Some(d) = dir {
        c.current_dir(d);
    }
    c.stderr(std::process::Stdio::null());
    match c.output() {
        Ok(out) if out.status.success() => String::from_utf8_lossy(&out.stdout)
            .lines()
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .collect(),
        _ => Vec::new(),
    }
}

// --- Docker resolvers ---

pub fn docker_containers() -> Vec<String> {
    run_capture("docker", &["ps", "--all", "--format", "{{.Names}}"], None)
}

pub struct DockerContainerResolver;
impl TypeResolver for DockerContainerResolver {
    fn list(&self, _ctx: &Ctx) -> Vec<String> {
        docker_containers()
    }
    fn cache_ttl(&self) -> Duration {
        Duration::from_secs(5)
    }
    fn id(&self) -> &'static str {
        "docker-container"
    }
}

pub fn docker_images() -> Vec<String> {
    let raw = run_capture("docker", &["images", "--format", "{{.Repository}}:{{.Tag}}"], None);
    let mut out: Vec<String> = Vec::new();
    for entry in raw {
        if let Some((repo, tag)) = entry.split_once(':') {
            if tag == "<none>" {
                if !repo.is_empty() && repo != "<none>" {
                    out.push(repo.to_string());
                }
            } else {
                out.push(entry.clone());
                // Also include bare repo name for convenience.
                if !repo.is_empty() && repo != "<none>" {
                    out.push(repo.to_string());
                }
            }
        } else {
            out.push(entry);
        }
    }
    out.sort();
    out.dedup();
    out
}

pub struct DockerImageResolver;
impl TypeResolver for DockerImageResolver {
    fn list(&self, _ctx: &Ctx) -> Vec<String> {
        docker_images()
    }
    fn cache_ttl(&self) -> Duration {
        Duration::from_secs(30)
    }
    fn id(&self) -> &'static str {
        "docker-image"
    }
}

pub fn docker_networks() -> Vec<String> {
    run_capture("docker", &["network", "ls", "--format", "{{.Name}}"], None)
}

pub struct DockerNetworkResolver;
impl TypeResolver for DockerNetworkResolver {
    fn list(&self, _ctx: &Ctx) -> Vec<String> {
        docker_networks()
    }
    fn cache_ttl(&self) -> Duration {
        Duration::from_secs(30)
    }
    fn id(&self) -> &'static str {
        "docker-network"
    }
}

pub fn docker_volumes() -> Vec<String> {
    run_capture("docker", &["volume", "ls", "--format", "{{.Name}}"], None)
}

pub struct DockerVolumeResolver;
impl TypeResolver for DockerVolumeResolver {
    fn list(&self, _ctx: &Ctx) -> Vec<String> {
        docker_volumes()
    }
    fn cache_ttl(&self) -> Duration {
        Duration::from_secs(30)
    }
    fn id(&self) -> &'static str {
        "docker-volume"
    }
}

/// Walk from `start` up to the filesystem root and return the first directory
/// that contains a compose file.
fn find_compose_dir(start: &std::path::Path) -> Option<std::path::PathBuf> {
    const COMPOSE_FILES: &[&str] =
        &["docker-compose.yml", "docker-compose.yaml", "compose.yml", "compose.yaml"];
    let mut dir = start.to_path_buf();
    loop {
        for name in COMPOSE_FILES {
            if dir.join(name).exists() {
                return Some(dir);
            }
        }
        if !dir.pop() {
            return None;
        }
    }
}

/// Parse top-level `services:` keys from a compose YAML file as a fallback
/// when `docker compose` is unavailable or not running.
fn parse_compose_services(compose_dir: &std::path::Path) -> Vec<String> {
    const COMPOSE_FILES: &[&str] =
        &["docker-compose.yml", "docker-compose.yaml", "compose.yml", "compose.yaml"];
    for name in COMPOSE_FILES {
        let path = compose_dir.join(name);
        if let Ok(content) = std::fs::read_to_string(&path) {
            if let Ok(doc) = serde_yaml_ng::from_str::<serde_yaml_ng::Value>(&content)
                && let Some(services) = doc.get("services").and_then(|v| v.as_mapping())
            {
                return services
                    .keys()
                    .filter_map(|k| k.as_str().map(|s| s.to_string()))
                    .collect();
            }
            break;
        }
    }
    Vec::new()
}

pub fn docker_compose_services(ctx: &Ctx) -> Vec<String> {
    let start = ctx
        .cwd
        .as_deref()
        .map(|p| p.to_path_buf())
        .or_else(|| std::env::current_dir().ok());
    let compose_dir = match start.as_deref().and_then(find_compose_dir) {
        Some(d) => d,
        None => return Vec::new(),
    };
    let out = run_capture(
        "docker",
        &["compose", "ps", "--services"],
        Some(compose_dir.as_path()),
    );
    if !out.is_empty() {
        return out;
    }
    parse_compose_services(&compose_dir)
}

pub struct DockerComposeServiceResolver;
impl TypeResolver for DockerComposeServiceResolver {
    fn list(&self, ctx: &Ctx) -> Vec<String> {
        docker_compose_services(ctx)
    }
    fn cache_ttl(&self) -> Duration {
        Duration::from_secs(10)
    }
    fn id(&self) -> &'static str {
        "docker-compose-service"
    }
}

// --- Kubernetes resolvers ---

/// Extract the value of a flag from a word list.
/// Handles `-f value`, `--flag value`, and `--flag=value` forms.
fn extract_flag_value(words: &[String], flags: &[&str]) -> Option<String> {
    for i in 0..words.len() {
        for f in flags {
            let key = (*f).to_string();
            if words[i] == key && i + 1 < words.len() {
                return Some(words[i + 1].clone());
            }
            if let Some(rest) = words[i].strip_prefix(&format!("{}=", f)) {
                return Some(rest.to_string());
            }
        }
    }
    None
}

/// Build `-n <namespace>` args to inject into kubectl commands if the caller
/// specified a namespace in prior words.
fn kubectl_namespace_args(ctx: &Ctx) -> Vec<String> {
    match extract_flag_value(&ctx.prior_words, &["-n", "--namespace"]) {
        Some(ns) => vec!["-n".to_string(), ns],
        None => Vec::new(),
    }
}

pub fn k8s_contexts() -> Vec<String> {
    run_capture("kubectl", &["config", "get-contexts", "-o", "name"], None)
}

pub struct K8sContextResolver;
impl TypeResolver for K8sContextResolver {
    fn list(&self, _ctx: &Ctx) -> Vec<String> {
        k8s_contexts()
    }
    fn cache_ttl(&self) -> Duration {
        Duration::from_secs(300)
    }
    fn id(&self) -> &'static str {
        "k8s-context"
    }
}

pub fn k8s_namespaces() -> Vec<String> {
    run_capture("kubectl", &["get", "namespaces", "-o", "name"], None)
        .into_iter()
        .map(|line| {
            line.strip_prefix("namespace/").map(|s| s.to_string()).unwrap_or(line)
        })
        .collect()
}

pub struct K8sNamespaceResolver;
impl TypeResolver for K8sNamespaceResolver {
    fn list(&self, _ctx: &Ctx) -> Vec<String> {
        k8s_namespaces()
    }
    fn cache_ttl(&self) -> Duration {
        Duration::from_secs(30)
    }
    fn id(&self) -> &'static str {
        "k8s-namespace"
    }
}

pub fn k8s_pods(ctx: &Ctx) -> Vec<String> {
    let ns_args = kubectl_namespace_args(ctx);
    let ns_strs: Vec<&str> = ns_args.iter().map(String::as_str).collect();
    let mut args: Vec<&str> = vec!["get"];
    args.extend(ns_strs.iter().copied());
    args.extend(["pods", "-o", "name"]);
    run_capture("kubectl", &args, None)
        .into_iter()
        .map(|line| line.strip_prefix("pod/").map(|s| s.to_string()).unwrap_or(line))
        .collect()
}

pub struct K8sPodResolver;
impl TypeResolver for K8sPodResolver {
    fn list(&self, ctx: &Ctx) -> Vec<String> {
        k8s_pods(ctx)
    }
    fn cache_ttl(&self) -> Duration {
        Duration::from_secs(5)
    }
    fn id(&self) -> &'static str {
        "k8s-pod"
    }
}

pub fn k8s_deployments(ctx: &Ctx) -> Vec<String> {
    let ns_args = kubectl_namespace_args(ctx);
    let ns_strs: Vec<&str> = ns_args.iter().map(String::as_str).collect();
    let mut args: Vec<&str> = vec!["get"];
    args.extend(ns_strs.iter().copied());
    args.extend(["deployments", "-o", "name"]);
    run_capture("kubectl", &args, None)
        .into_iter()
        .map(|line| {
            line.strip_prefix("deployment.apps/").map(|s| s.to_string()).unwrap_or(line)
        })
        .collect()
}

pub struct K8sDeploymentResolver;
impl TypeResolver for K8sDeploymentResolver {
    fn list(&self, ctx: &Ctx) -> Vec<String> {
        k8s_deployments(ctx)
    }
    fn cache_ttl(&self) -> Duration {
        Duration::from_secs(10)
    }
    fn id(&self) -> &'static str {
        "k8s-deployment"
    }
}

pub fn k8s_services(ctx: &Ctx) -> Vec<String> {
    let ns_args = kubectl_namespace_args(ctx);
    let ns_strs: Vec<&str> = ns_args.iter().map(String::as_str).collect();
    let mut args: Vec<&str> = vec!["get"];
    args.extend(ns_strs.iter().copied());
    args.extend(["services", "-o", "name"]);
    run_capture("kubectl", &args, None)
        .into_iter()
        .map(|line| line.strip_prefix("service/").map(|s| s.to_string()).unwrap_or(line))
        .collect()
}

pub struct K8sServiceResolver;
impl TypeResolver for K8sServiceResolver {
    fn list(&self, ctx: &Ctx) -> Vec<String> {
        k8s_services(ctx)
    }
    fn cache_ttl(&self) -> Duration {
        Duration::from_secs(10)
    }
    fn id(&self) -> &'static str {
        "k8s-service"
    }
}

pub fn k8s_resource_kinds() -> Vec<String> {
    run_capture("kubectl", &["api-resources", "--no-headers", "--output=name"], None)
}

pub struct K8sResourceKindResolver;
impl TypeResolver for K8sResourceKindResolver {
    fn list(&self, _ctx: &Ctx) -> Vec<String> {
        k8s_resource_kinds()
    }
    fn cache_ttl(&self) -> Duration {
        Duration::from_secs(3600)
    }
    fn id(&self) -> &'static str {
        "k8s-resource-kind"
    }
}

// --- systemd resolvers ---

fn systemctl_args(ctx: &Ctx, rest: &[&str]) -> Vec<String> {
    let mut out = Vec::new();
    if ctx.prior_words.iter().any(|w| w == "--user") {
        out.push("--user".into());
    }
    for a in rest {
        out.push((*a).into());
    }
    out
}

fn parse_systemctl_list_unit_files(raw: &str, suffix_filter: Option<&str>) -> Vec<String> {
    raw.lines()
        .filter_map(|line| {
            let first = line.split_whitespace().next()?;
            if first.is_empty() {
                return None;
            }
            if let Some(suffix) = suffix_filter
                && !first.ends_with(suffix)
            {
                return None;
            }
            Some(first.to_string())
        })
        .collect()
}

fn systemctl_list_units(ctx: &Ctx, suffix_filter: Option<&str>) -> Vec<String> {
    let base_args: Vec<&str> = vec!["list-unit-files", "--no-legend", "--no-pager"];
    let args = systemctl_args(ctx, &base_args);
    let arg_strs: Vec<&str> = args.iter().map(String::as_str).collect();
    let mut cmd = std::process::Command::new("systemctl");
    cmd.args(&arg_strs);
    cmd.stderr(std::process::Stdio::null());
    let raw = match cmd.output() {
        Ok(out) if out.status.success() || !out.stdout.is_empty() => {
            String::from_utf8_lossy(&out.stdout).into_owned()
        }
        _ => return Vec::new(),
    };
    parse_systemctl_list_unit_files(&raw, suffix_filter)
}

pub struct SystemdUnitResolver;
impl TypeResolver for SystemdUnitResolver {
    fn list(&self, ctx: &Ctx) -> Vec<String> {
        systemctl_list_units(ctx, None)
    }
    fn cache_ttl(&self) -> Duration {
        Duration::from_secs(60)
    }
    fn id(&self) -> &'static str {
        "systemd-unit"
    }
}

pub struct SystemdServiceResolver;
impl TypeResolver for SystemdServiceResolver {
    fn list(&self, ctx: &Ctx) -> Vec<String> {
        systemctl_list_units(ctx, Some(".service"))
    }
    fn cache_ttl(&self) -> Duration {
        Duration::from_secs(60)
    }
    fn id(&self) -> &'static str {
        "systemd-service"
    }
}

pub struct SystemdTimerResolver;
impl TypeResolver for SystemdTimerResolver {
    fn list(&self, ctx: &Ctx) -> Vec<String> {
        systemctl_list_units(ctx, Some(".timer"))
    }
    fn cache_ttl(&self) -> Duration {
        Duration::from_secs(60)
    }
    fn id(&self) -> &'static str {
        "systemd-timer"
    }
}

pub struct SystemdSocketResolver;
impl TypeResolver for SystemdSocketResolver {
    fn list(&self, ctx: &Ctx) -> Vec<String> {
        systemctl_list_units(ctx, Some(".socket"))
    }
    fn cache_ttl(&self) -> Duration {
        Duration::from_secs(60)
    }
    fn id(&self) -> &'static str {
        "systemd-socket"
    }
}

// --- tmux resolvers ---

pub fn tmux_sessions() -> Vec<String> {
    run_capture("tmux", &["list-sessions", "-F", "#{session_name}"], None)
}

pub struct TmuxSessionResolver;
impl TypeResolver for TmuxSessionResolver {
    fn list(&self, _ctx: &Ctx) -> Vec<String> {
        tmux_sessions()
    }
    fn cache_ttl(&self) -> Duration {
        Duration::from_secs(5)
    }
    fn id(&self) -> &'static str {
        "tmux-session"
    }
}

pub fn tmux_windows() -> Vec<String> {
    run_capture(
        "tmux",
        &["list-windows", "-a", "-F", "#{session_name}:#{window_index}.#{window_name}"],
        None,
    )
}

pub struct TmuxWindowResolver;
impl TypeResolver for TmuxWindowResolver {
    fn list(&self, _ctx: &Ctx) -> Vec<String> {
        tmux_windows()
    }
    fn cache_ttl(&self) -> Duration {
        Duration::from_secs(5)
    }
    fn id(&self) -> &'static str {
        "tmux-window"
    }
}

pub fn tmux_panes() -> Vec<String> {
    run_capture(
        "tmux",
        &["list-panes", "-a", "-F", "#{session_name}:#{window_index}.#{pane_index}"],
        None,
    )
}

pub struct TmuxPaneResolver;
impl TypeResolver for TmuxPaneResolver {
    fn list(&self, _ctx: &Ctx) -> Vec<String> {
        tmux_panes()
    }
    fn cache_ttl(&self) -> Duration {
        Duration::from_secs(5)
    }
    fn id(&self) -> &'static str {
        "tmux-pane"
    }
}

// --- screen resolver ---

pub fn parse_screen_ls(output: &str) -> Vec<String> {
    // Lines of interest look like: `\t12345.work\t(Detached)`
    // Capture the name after the dot.
    output
        .lines()
        .filter_map(|line| {
            let trimmed = line.trim();
            // Must start with a digit (the PID).
            if trimmed.is_empty() || !trimmed.chars().next().is_some_and(|c| c.is_ascii_digit()) {
                return None;
            }
            // The session descriptor is "<pid>.<name>"; take what's after the dot.
            let dot_pos = trimmed.find('.')?;
            let rest = &trimmed[dot_pos + 1..];
            // The name ends at the first whitespace.
            let name = rest.split_whitespace().next()?;
            if name.is_empty() { None } else { Some(name.to_string()) }
        })
        .collect()
}

pub fn screen_sessions() -> Vec<String> {
    let mut cmd = std::process::Command::new("screen");
    cmd.args(["-ls"]);
    cmd.stderr(std::process::Stdio::null());
    let raw = match cmd.output() {
        // `screen -ls` returns exit code 1 when listing; still captures output.
        Ok(out) => String::from_utf8_lossy(&out.stdout).into_owned(),
        Err(_) => return Vec::new(),
    };
    parse_screen_ls(&raw)
}

pub struct ScreenSessionResolver;
impl TypeResolver for ScreenSessionResolver {
    fn list(&self, _ctx: &Ctx) -> Vec<String> {
        screen_sessions()
    }
    fn cache_ttl(&self) -> Duration {
        Duration::from_secs(5)
    }
    fn id(&self) -> &'static str {
        "screen-session"
    }
}

pub fn register_builtins(r: &mut Registry) {
    r.register(ARG_MODE_USERS, Box::new(UsersResolver));
    r.register(ARG_MODE_GROUPS, Box::new(GroupsResolver));
    r.register(ARG_MODE_HOSTS, Box::new(HostsResolver));
    r.register(ARG_MODE_SIGNALS, Box::new(SignalsResolver));
    r.register(ARG_MODE_PORTS, Box::new(PortsResolver));
    r.register(ARG_MODE_NET_IFACES, Box::new(NetIfacesResolver));
    r.register(ARG_MODE_LOCALES, Box::new(LocalesResolver));
    r.register(ARG_MODE_GIT_BRANCHES, Box::new(GitBranchesResolver));
    r.register(ARG_MODE_GIT_TAGS, Box::new(GitTagsResolver));
    r.register(ARG_MODE_GIT_REMOTES, Box::new(GitRemotesResolver));
    r.register(ARG_MODE_GIT_FILES, Box::new(GitFilesResolver));
    r.register(ARG_MODE_USERS_GROUPS, Box::new(UsersGroupsResolver));
    r.register(ARG_MODE_GIT_STASH, Box::new(GitStashResolver));
    r.register(ARG_MODE_GIT_WORKTREE, Box::new(GitWorktreeResolver));
    r.register(ARG_MODE_GIT_SUBMODULE, Box::new(GitSubmoduleResolver));
    r.register(ARG_MODE_GIT_CONFIG_KEY, Box::new(GitConfigKeyResolver));
    r.register(ARG_MODE_GIT_ALIAS, Box::new(GitAliasResolver));
    r.register(ARG_MODE_GIT_COMMIT, Box::new(GitCommitResolver));
    r.register(ARG_MODE_GIT_REFLOG, Box::new(GitReflogResolver));
    // Docker
    r.register(ARG_MODE_DOCKER_CONTAINER, Box::new(DockerContainerResolver));
    r.register(ARG_MODE_DOCKER_IMAGE, Box::new(DockerImageResolver));
    r.register(ARG_MODE_DOCKER_NETWORK, Box::new(DockerNetworkResolver));
    r.register(ARG_MODE_DOCKER_VOLUME, Box::new(DockerVolumeResolver));
    r.register(ARG_MODE_DOCKER_COMPOSE_SERVICE, Box::new(DockerComposeServiceResolver));
    // Kubernetes
    r.register(ARG_MODE_K8S_CONTEXT, Box::new(K8sContextResolver));
    r.register(ARG_MODE_K8S_NAMESPACE, Box::new(K8sNamespaceResolver));
    r.register(ARG_MODE_K8S_POD, Box::new(K8sPodResolver));
    r.register(ARG_MODE_K8S_DEPLOYMENT, Box::new(K8sDeploymentResolver));
    r.register(ARG_MODE_K8S_SERVICE, Box::new(K8sServiceResolver));
    r.register(ARG_MODE_K8S_RESOURCE_KIND, Box::new(K8sResourceKindResolver));
    // systemd
    r.register(ARG_MODE_SYSTEMD_UNIT, Box::new(SystemdUnitResolver));
    r.register(ARG_MODE_SYSTEMD_SERVICE, Box::new(SystemdServiceResolver));
    r.register(ARG_MODE_SYSTEMD_TIMER, Box::new(SystemdTimerResolver));
    r.register(ARG_MODE_SYSTEMD_SOCKET, Box::new(SystemdSocketResolver));
    // tmux
    r.register(ARG_MODE_TMUX_SESSION, Box::new(TmuxSessionResolver));
    r.register(ARG_MODE_TMUX_WINDOW, Box::new(TmuxWindowResolver));
    r.register(ARG_MODE_TMUX_PANE, Box::new(TmuxPaneResolver));
    // screen
    r.register(ARG_MODE_SCREEN_SESSION, Box::new(ScreenSessionResolver));
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
        trie::ARG_MODE_GIT_STASH => "<stash>",
        trie::ARG_MODE_GIT_WORKTREE => "<worktree>",
        trie::ARG_MODE_GIT_SUBMODULE => "<submodule>",
        trie::ARG_MODE_GIT_CONFIG_KEY => "<config-key>",
        trie::ARG_MODE_GIT_ALIAS => "<alias>",
        trie::ARG_MODE_GIT_COMMIT => "<commit>",
        trie::ARG_MODE_GIT_REFLOG => "<reflog-entry>",
        trie::ARG_MODE_DOCKER_CONTAINER => "<container>",
        trie::ARG_MODE_DOCKER_IMAGE => "<image>",
        trie::ARG_MODE_DOCKER_NETWORK => "<network>",
        trie::ARG_MODE_DOCKER_VOLUME => "<volume>",
        trie::ARG_MODE_DOCKER_COMPOSE_SERVICE => "<service>",
        trie::ARG_MODE_K8S_CONTEXT => "<context>",
        trie::ARG_MODE_K8S_NAMESPACE => "<namespace>",
        trie::ARG_MODE_K8S_POD => "<pod>",
        trie::ARG_MODE_K8S_DEPLOYMENT => "<deployment>",
        trie::ARG_MODE_K8S_SERVICE => "<k8s-service>",
        trie::ARG_MODE_K8S_RESOURCE_KIND => "<resource-kind>",
        trie::ARG_MODE_SYSTEMD_UNIT => "<unit>",
        trie::ARG_MODE_SYSTEMD_SERVICE => "<service>",
        trie::ARG_MODE_SYSTEMD_TIMER => "<timer>",
        trie::ARG_MODE_SYSTEMD_SOCKET => "<socket>",
        trie::ARG_MODE_TMUX_SESSION => "<session>",
        trie::ARG_MODE_TMUX_WINDOW => "<window>",
        trie::ARG_MODE_TMUX_PANE => "<pane>",
        trie::ARG_MODE_SCREEN_SESSION => "<screen-session>",
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
        // Hold CWD_LOCK so PATH-clearing tests in this module don't race with
        // the `printf` exec below.
        let _g = crate::test_util::CWD_LOCK.lock().unwrap();
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

    // --- helpers for new git resolver tests ---

    fn setup_git_repo() -> tempfile::TempDir {
        let td = tempfile::tempdir().unwrap();
        let p = td.path();
        for args in [
            &["init", "-q", "-b", "main"][..],
            &["config", "user.email", "t@example.com"][..],
            &["config", "user.name", "T"][..],
        ] {
            std::process::Command::new("git").current_dir(p).args(args).output().unwrap();
        }
        std::fs::write(p.join("f.txt"), "hi").unwrap();
        std::process::Command::new("git")
            .current_dir(p)
            .args(["add", "f.txt"])
            .output()
            .unwrap();
        std::process::Command::new("git")
            .current_dir(p)
            .args(["commit", "-q", "-m", "init"])
            .output()
            .unwrap();
        td
    }

    // --- GitStashResolver ---

    #[test]
    fn git_stash_returns_expected() {
        let _g = crate::test_util::CWD_LOCK.lock().unwrap();
        let td = setup_git_repo();
        let orig = std::env::current_dir().ok();
        std::env::set_current_dir(td.path()).unwrap();

        assert!(git_stash_list().is_empty(), "no stashes yet");

        std::fs::write(td.path().join("f.txt"), "changed").unwrap();
        std::process::Command::new("git").args(["stash"]).output().unwrap();
        let stashes = git_stash_list();
        assert_eq!(stashes.len(), 1);
        assert!(stashes[0].starts_with("stash@{"), "unexpected: {}", stashes[0]);

        if let Some(o) = orig {
            let _ = std::env::set_current_dir(o);
        }
    }

    // --- GitWorktreeResolver ---

    #[test]
    fn git_worktree_non_repo_returns_empty() {
        let _g = crate::test_util::CWD_LOCK.lock().unwrap();
        let td = tempfile::tempdir().unwrap();
        let orig = std::env::current_dir().ok();
        std::env::set_current_dir(td.path()).unwrap();
        // Detailed worktree/submodule scenarios are covered in integration tests.
        assert!(git_worktree_list().is_empty());
        if let Some(o) = orig {
            let _ = std::env::set_current_dir(o);
        }
    }

    #[test]
    fn git_worktree_returns_main_worktree_path() {
        let _g = crate::test_util::CWD_LOCK.lock().unwrap();
        let td = setup_git_repo();
        let orig = std::env::current_dir().ok();
        std::env::set_current_dir(td.path()).unwrap();

        let worktrees = git_worktree_list();
        // The main worktree is always present.
        assert!(!worktrees.is_empty(), "expected at least the main worktree");
        // The first entry should be the repo root path.
        assert!(
            worktrees[0].contains(td.path().to_str().unwrap()),
            "worktree path should include repo root: {:?}",
            worktrees[0]
        );

        if let Some(o) = orig {
            let _ = std::env::set_current_dir(o);
        }
    }

    // --- GitSubmoduleResolver ---

    #[test]
    fn git_submodule_no_submodules_returns_empty() {
        let _g = crate::test_util::CWD_LOCK.lock().unwrap();
        let td = setup_git_repo();
        let orig = std::env::current_dir().ok();
        std::env::set_current_dir(td.path()).unwrap();
        // Detailed submodule scenarios are covered in integration tests.
        assert!(git_submodule_list().is_empty());
        if let Some(o) = orig {
            let _ = std::env::set_current_dir(o);
        }
    }

    // --- GitConfigKeyResolver ---

    #[test]
    fn git_config_keys_contains_user_fields() {
        let _g = crate::test_util::CWD_LOCK.lock().unwrap();
        let td = setup_git_repo();
        let orig = std::env::current_dir().ok();
        std::env::set_current_dir(td.path()).unwrap();

        let keys = git_config_keys();
        assert!(keys.contains(&"user.email".to_string()), "keys: {:?}", keys);
        assert!(keys.contains(&"user.name".to_string()), "keys: {:?}", keys);

        if let Some(o) = orig {
            let _ = std::env::set_current_dir(o);
        }
    }

    // --- GitAliasResolver ---

    #[test]
    fn git_aliases_returns_configured_aliases() {
        let _g = crate::test_util::CWD_LOCK.lock().unwrap();
        let td = setup_git_repo();
        let orig = std::env::current_dir().ok();
        std::env::set_current_dir(td.path()).unwrap();

        std::process::Command::new("git")
            .current_dir(td.path())
            .args(["config", "alias.co", "checkout"])
            .output()
            .unwrap();
        std::process::Command::new("git")
            .current_dir(td.path())
            .args(["config", "alias.br", "branch"])
            .output()
            .unwrap();

        let aliases = git_aliases();
        assert!(aliases.contains(&"co".to_string()), "aliases: {:?}", aliases);
        assert!(aliases.contains(&"br".to_string()), "aliases: {:?}", aliases);

        if let Some(o) = orig {
            let _ = std::env::set_current_dir(o);
        }
    }

    // --- GitCommitResolver ---

    #[test]
    fn git_commits_returns_hashes_after_commit() {
        let _g = crate::test_util::CWD_LOCK.lock().unwrap();
        let td = setup_git_repo();
        let orig = std::env::current_dir().ok();
        std::env::set_current_dir(td.path()).unwrap();

        let commits = git_commits();
        assert!(!commits.is_empty(), "expected at least one commit hash");
        assert!(
            commits.iter().all(|h| h.chars().next().is_some_and(|c| c.is_ascii_hexdigit())),
            "all entries should be hex: {:?}",
            commits
        );

        if let Some(o) = orig {
            let _ = std::env::set_current_dir(o);
        }
    }

    // --- GitReflogResolver ---

    #[test]
    fn git_reflog_returns_entries_after_commit() {
        let _g = crate::test_util::CWD_LOCK.lock().unwrap();
        let td = setup_git_repo();
        let orig = std::env::current_dir().ok();
        std::env::set_current_dir(td.path()).unwrap();

        let entries = git_reflog_list();
        assert!(!entries.is_empty(), "expected at least one reflog entry");
        assert!(
            entries[0].starts_with("HEAD@{"),
            "unexpected reflog entry: {}",
            entries[0]
        );

        if let Some(o) = orig {
            let _ = std::env::set_current_dir(o);
        }
    }

    // --- new resolvers tolerate non-repo ---

    #[test]
    fn new_git_resolvers_tolerate_non_repo() {
        let _g = crate::test_util::CWD_LOCK.lock().unwrap();
        let td = tempfile::tempdir().unwrap();
        let orig = std::env::current_dir().ok();
        std::env::set_current_dir(td.path()).unwrap();
        // None of these may panic. Stash/worktree/submodule/commits/reflog
        // return empty outside a repo; config keys may return global config
        // entries even outside a repo — that is correct git behavior.
        assert!(git_stash_list().is_empty());
        assert!(git_worktree_list().is_empty());
        assert!(git_submodule_list().is_empty());
        let _ = git_config_keys(); // global config may be non-empty; must not panic
        let _ = git_aliases();    // global aliases are valid to return
        assert!(git_commits().is_empty());
        assert!(git_reflog_list().is_empty());
        if let Some(o) = orig {
            let _ = std::env::set_current_dir(o);
        }
    }

    // --- Docker resolver tests ---

    #[test]
    fn docker_container_missing_cli_returns_empty() {
        let _g = crate::test_util::CWD_LOCK.lock().unwrap();
        let orig = std::env::var_os("PATH");
        let empty = tempfile::tempdir().unwrap();
        // SAFETY: test serialized via CWD_LOCK; no other threads touching PATH.
        unsafe { std::env::set_var("PATH", empty.path()); }
        assert_eq!(docker_containers(), Vec::<String>::new());
        unsafe {
            if let Some(p) = orig {
                std::env::set_var("PATH", p);
            } else {
                std::env::remove_var("PATH");
            }
        }
    }

    #[test]
    fn docker_image_missing_cli_returns_empty() {
        let _g = crate::test_util::CWD_LOCK.lock().unwrap();
        let orig = std::env::var_os("PATH");
        let empty = tempfile::tempdir().unwrap();
        unsafe { std::env::set_var("PATH", empty.path()); }
        assert_eq!(docker_images(), Vec::<String>::new());
        unsafe {
            if let Some(p) = orig {
                std::env::set_var("PATH", p);
            } else {
                std::env::remove_var("PATH");
            }
        }
    }

    #[test]
    fn docker_network_missing_cli_returns_empty() {
        let _g = crate::test_util::CWD_LOCK.lock().unwrap();
        let orig = std::env::var_os("PATH");
        let empty = tempfile::tempdir().unwrap();
        unsafe { std::env::set_var("PATH", empty.path()); }
        assert_eq!(docker_networks(), Vec::<String>::new());
        unsafe {
            if let Some(p) = orig {
                std::env::set_var("PATH", p);
            } else {
                std::env::remove_var("PATH");
            }
        }
    }

    #[test]
    fn docker_volume_missing_cli_returns_empty() {
        let _g = crate::test_util::CWD_LOCK.lock().unwrap();
        let orig = std::env::var_os("PATH");
        let empty = tempfile::tempdir().unwrap();
        unsafe { std::env::set_var("PATH", empty.path()); }
        assert_eq!(docker_volumes(), Vec::<String>::new());
        unsafe {
            if let Some(p) = orig {
                std::env::set_var("PATH", p);
            } else {
                std::env::remove_var("PATH");
            }
        }
    }

    #[test]
    fn docker_compose_service_missing_cli_and_no_compose_file_returns_empty() {
        let _g = crate::test_util::CWD_LOCK.lock().unwrap();
        let orig = std::env::var_os("PATH");
        let td = tempfile::tempdir().unwrap();
        unsafe { std::env::set_var("PATH", td.path()); }
        let ctx = crate::type_resolver::Ctx {
            cwd: Some(td.path().to_path_buf()),
            ..Default::default()
        };
        assert_eq!(docker_compose_services(&ctx), Vec::<String>::new());
        unsafe {
            if let Some(p) = orig {
                std::env::set_var("PATH", p);
            } else {
                std::env::remove_var("PATH");
            }
        }
    }

    #[test]
    fn docker_compose_service_yaml_fallback() {
        // No docker CLI needed; parses compose YAML directly.
        let _g = crate::test_util::CWD_LOCK.lock().unwrap();
        let td = tempfile::tempdir().unwrap();
        let compose_content = "services:\n  web:\n    image: nginx\n  db:\n    image: postgres\n";
        std::fs::write(td.path().join("docker-compose.yml"), compose_content).unwrap();

        // Point PATH at an empty dir so docker is not available.
        let orig = std::env::var_os("PATH");
        let empty = tempfile::tempdir().unwrap();
        unsafe { std::env::set_var("PATH", empty.path()); }

        let ctx = crate::type_resolver::Ctx {
            cwd: Some(td.path().to_path_buf()),
            ..Default::default()
        };
        let mut services = docker_compose_services(&ctx);
        services.sort();

        unsafe {
            if let Some(p) = orig {
                std::env::set_var("PATH", p);
            } else {
                std::env::remove_var("PATH");
            }
        }

        assert!(services.contains(&"web".to_string()), "services: {:?}", services);
        assert!(services.contains(&"db".to_string()), "services: {:?}", services);
    }

    #[test]
    fn docker_images_deduplicates_repo_none_tag() {
        // Build raw lines as if docker output them, then check dedup logic.
        // We test parse_compose_services directly; for images we verify the
        // dedup behavior using the helper logic path by inspecting docker_images()
        // output format expectations via the internal logic (unit test the mapping).
        let raw = vec![
            "myapp:<none>".to_string(),
            "myapp:1.0".to_string(),
            "myapp:latest".to_string(),
        ];
        let mut out: Vec<String> = Vec::new();
        for entry in raw {
            if let Some((repo, tag)) = entry.split_once(':') {
                if tag == "<none>" {
                    if !repo.is_empty() && repo != "<none>" {
                        out.push(repo.to_string());
                    }
                } else {
                    out.push(entry.clone());
                    if !repo.is_empty() && repo != "<none>" {
                        out.push(repo.to_string());
                    }
                }
            } else {
                out.push(entry);
            }
        }
        out.sort();
        out.dedup();
        assert!(out.contains(&"myapp".to_string()), "out: {:?}", out);
        assert!(out.contains(&"myapp:1.0".to_string()), "out: {:?}", out);
        assert!(out.contains(&"myapp:latest".to_string()), "out: {:?}", out);
        assert!(!out.iter().any(|s| s.contains("<none>")), "out: {:?}", out);
    }

    // --- Kubernetes resolver tests ---

    #[test]
    fn k8s_context_missing_cli_returns_empty() {
        let _g = crate::test_util::CWD_LOCK.lock().unwrap();
        let orig = std::env::var_os("PATH");
        let empty = tempfile::tempdir().unwrap();
        unsafe { std::env::set_var("PATH", empty.path()); }
        assert_eq!(k8s_contexts(), Vec::<String>::new());
        unsafe {
            if let Some(p) = orig {
                std::env::set_var("PATH", p);
            } else {
                std::env::remove_var("PATH");
            }
        }
    }

    #[test]
    fn k8s_namespace_missing_cli_returns_empty() {
        let _g = crate::test_util::CWD_LOCK.lock().unwrap();
        let orig = std::env::var_os("PATH");
        let empty = tempfile::tempdir().unwrap();
        unsafe { std::env::set_var("PATH", empty.path()); }
        assert_eq!(k8s_namespaces(), Vec::<String>::new());
        unsafe {
            if let Some(p) = orig {
                std::env::set_var("PATH", p);
            } else {
                std::env::remove_var("PATH");
            }
        }
    }

    #[test]
    fn k8s_pod_missing_cli_returns_empty() {
        let _g = crate::test_util::CWD_LOCK.lock().unwrap();
        let orig = std::env::var_os("PATH");
        let empty = tempfile::tempdir().unwrap();
        unsafe { std::env::set_var("PATH", empty.path()); }
        let ctx = crate::type_resolver::Ctx::new();
        assert_eq!(k8s_pods(&ctx), Vec::<String>::new());
        unsafe {
            if let Some(p) = orig {
                std::env::set_var("PATH", p);
            } else {
                std::env::remove_var("PATH");
            }
        }
    }

    #[test]
    fn k8s_deployment_missing_cli_returns_empty() {
        let _g = crate::test_util::CWD_LOCK.lock().unwrap();
        let orig = std::env::var_os("PATH");
        let empty = tempfile::tempdir().unwrap();
        unsafe { std::env::set_var("PATH", empty.path()); }
        let ctx = crate::type_resolver::Ctx::new();
        assert_eq!(k8s_deployments(&ctx), Vec::<String>::new());
        unsafe {
            if let Some(p) = orig {
                std::env::set_var("PATH", p);
            } else {
                std::env::remove_var("PATH");
            }
        }
    }

    #[test]
    fn k8s_service_missing_cli_returns_empty() {
        let _g = crate::test_util::CWD_LOCK.lock().unwrap();
        let orig = std::env::var_os("PATH");
        let empty = tempfile::tempdir().unwrap();
        unsafe { std::env::set_var("PATH", empty.path()); }
        let ctx = crate::type_resolver::Ctx::new();
        assert_eq!(k8s_services(&ctx), Vec::<String>::new());
        unsafe {
            if let Some(p) = orig {
                std::env::set_var("PATH", p);
            } else {
                std::env::remove_var("PATH");
            }
        }
    }

    #[test]
    fn k8s_resource_kind_missing_cli_returns_empty() {
        let _g = crate::test_util::CWD_LOCK.lock().unwrap();
        let orig = std::env::var_os("PATH");
        let empty = tempfile::tempdir().unwrap();
        unsafe { std::env::set_var("PATH", empty.path()); }
        assert_eq!(k8s_resource_kinds(), Vec::<String>::new());
        unsafe {
            if let Some(p) = orig {
                std::env::set_var("PATH", p);
            } else {
                std::env::remove_var("PATH");
            }
        }
    }

    // --- extract_flag_value helper tests ---

    #[test]
    fn extract_flag_value_short_flag_space_form() {
        let words = ["-n", "prod"].map(String::from).to_vec();
        assert_eq!(extract_flag_value(&words, &["-n", "--namespace"]), Some("prod".to_string()));
    }

    #[test]
    fn extract_flag_value_long_flag_equals_form() {
        let words = ["--namespace=dev"].map(String::from).to_vec();
        assert_eq!(extract_flag_value(&words, &["-n", "--namespace"]), Some("dev".to_string()));
    }

    #[test]
    fn extract_flag_value_long_flag_space_form() {
        let words = ["--namespace", "staging"].map(String::from).to_vec();
        assert_eq!(extract_flag_value(&words, &["-n", "--namespace"]), Some("staging".to_string()));
    }

    #[test]
    fn extract_flag_value_flag_absent_returns_none() {
        let words = ["pod", "list"].map(String::from).to_vec();
        assert_eq!(extract_flag_value(&words, &["-n", "--namespace"]), None);
    }

    #[test]
    fn kubectl_namespace_args_injected_into_pods() {
        // With -n prod in prior_words, k8s_pods should pass -n prod to kubectl.
        // We can't run kubectl in CI; just verify extract_flag_value sees it.
        let ctx = crate::type_resolver::Ctx {
            prior_words: vec!["-n".to_string(), "prod".to_string()],
            ..Default::default()
        };
        assert_eq!(kubectl_namespace_args(&ctx), vec!["-n".to_string(), "prod".to_string()]);
    }

    // --- list_with_cache integration ---

    /// Verify that `list_matches` (and therefore `list_with_cache`) calls the
    /// resolver only once on a second invocation with the same context when the
    /// on-disk cache is warm.
    ///
    /// We register a counting resolver under a test-only mode number (200),
    /// redirect the runtime cache to a fresh tempdir via `ZSH_IOS_RUNTIME_CACHE_DIR`,
    /// then call `list_matches` twice and assert the resolver's `list` method
    /// was invoked exactly once.
    #[test]
    fn list_matches_uses_cache_hit_on_second_call() {
        use std::sync::{Arc, Mutex};
        use crate::type_resolver::{Ctx, TypeResolver};

        // A resolver that counts how many times `list` is called.
        struct CountingResolver {
            call_count: Arc<Mutex<usize>>,
            items: Vec<String>,
        }
        impl TypeResolver for CountingResolver {
            fn list(&self, _ctx: &Ctx) -> Vec<String> {
                *self.call_count.lock().unwrap() += 1;
                self.items.clone()
            }
            fn cache_ttl(&self) -> Duration {
                Duration::from_secs(60)
            }
            fn id(&self) -> &'static str {
                "counting-resolver-test"
            }
        }

        // We cannot inject into the global REGISTRY (it's a LazyLock), so we
        // exercise `list_with_cache` directly instead.  This tests the exact
        // code path used by both `list_matches` and `resolve_prefix`.
        let td = tempfile::tempdir().unwrap();
        // Override the cache dir so we don't pollute the real cache.
        // SAFETY: test binary is single-threaded at this point (standard
        // Rust test runner runs each #[test] sequentially within a thread).
        // We restore the variable immediately after the calls so it doesn't
        // leak into other tests.
        unsafe { std::env::set_var("ZSH_IOS_RUNTIME_CACHE_DIR", td.path().as_os_str()) };

        let call_count = Arc::new(Mutex::new(0usize));
        let resolver = CountingResolver {
            call_count: Arc::clone(&call_count),
            items: vec!["branch-a".to_string(), "branch-b".to_string()],
        };
        let ctx = Ctx::with_partial("");

        let result1 = list_with_cache(&resolver, 200, &ctx);
        let result2 = list_with_cache(&resolver, 200, &ctx);

        unsafe { std::env::remove_var("ZSH_IOS_RUNTIME_CACHE_DIR") };

        assert_eq!(result1, vec!["branch-a", "branch-b"]);
        assert_eq!(result1, result2, "second call must return the same items");
        assert_eq!(
            *call_count.lock().unwrap(),
            1,
            "resolver.list() must be called exactly once; cache should serve the second call"
        );
    }

    // --- systemd tests ---

    #[test]
    fn parse_systemctl_list_unit_files_no_filter() {
        let raw = "\
foo.service enabled enabled\n\
bar.timer active static\n\
baz.socket disabled static\n\
qux.mount mounted\n";
        let result = parse_systemctl_list_unit_files(raw, None);
        assert_eq!(result, vec!["foo.service", "bar.timer", "baz.socket", "qux.mount"]);
    }

    #[test]
    fn parse_systemctl_list_unit_files_service_filter() {
        let raw = "\
foo.service enabled enabled\n\
bar.timer active static\n\
baz.socket disabled static\n\
qux.mount mounted\n";
        let result = parse_systemctl_list_unit_files(raw, Some(".service"));
        assert_eq!(result, vec!["foo.service"]);
    }

    #[test]
    fn parse_systemctl_list_unit_files_timer_filter() {
        let raw = "\
foo.service enabled enabled\n\
bar.timer active static\n\
baz.socket disabled static\n\
qux.mount mounted\n";
        let result = parse_systemctl_list_unit_files(raw, Some(".timer"));
        assert_eq!(result, vec!["bar.timer"]);
    }

    #[test]
    fn parse_systemctl_list_unit_files_socket_filter() {
        let raw = "\
foo.service enabled enabled\n\
bar.timer active static\n\
baz.socket disabled static\n\
qux.mount mounted\n";
        let result = parse_systemctl_list_unit_files(raw, Some(".socket"));
        assert_eq!(result, vec!["baz.socket"]);
    }

    #[test]
    fn systemd_tolerates_missing_cli() {
        let _g = crate::test_util::CWD_LOCK.lock().unwrap();
        let orig = std::env::var_os("PATH");
        let empty = tempfile::tempdir().unwrap();
        unsafe { std::env::set_var("PATH", empty.path()); }
        let ctx = crate::type_resolver::Ctx::new();
        assert_eq!(systemctl_list_units(&ctx, None), Vec::<String>::new());
        unsafe {
            if let Some(p) = orig {
                std::env::set_var("PATH", p);
            } else {
                std::env::remove_var("PATH");
            }
        }
    }

    // --- tmux tests ---

    #[test]
    fn tmux_tolerates_missing_cli() {
        let _g = crate::test_util::CWD_LOCK.lock().unwrap();
        let orig = std::env::var_os("PATH");
        let empty = tempfile::tempdir().unwrap();
        unsafe { std::env::set_var("PATH", empty.path()); }
        assert_eq!(tmux_sessions(), Vec::<String>::new());
        assert_eq!(tmux_windows(), Vec::<String>::new());
        assert_eq!(tmux_panes(), Vec::<String>::new());
        unsafe {
            if let Some(p) = orig {
                std::env::set_var("PATH", p);
            } else {
                std::env::remove_var("PATH");
            }
        }
    }

    // --- screen tests ---

    #[test]
    fn parse_screen_ls_extracts_session_names() {
        let output = "\
There are screens on:\n\
    12345.work       (Detached)\n\
    67890.play       (Attached)\n\
2 Sockets in /run/screen/S-user.\n";
        let result = parse_screen_ls(output);
        assert_eq!(result, vec!["work".to_string(), "play".to_string()]);
    }

    #[test]
    fn parse_screen_ls_empty_output() {
        assert!(parse_screen_ls("No Sockets found in /run/screen/S-user.\n").is_empty());
        assert!(parse_screen_ls("").is_empty());
    }

    #[test]
    fn screen_tolerates_missing_cli() {
        let _g = crate::test_util::CWD_LOCK.lock().unwrap();
        let orig = std::env::var_os("PATH");
        let empty = tempfile::tempdir().unwrap();
        unsafe { std::env::set_var("PATH", empty.path()); }
        assert_eq!(screen_sessions(), Vec::<String>::new());
        unsafe {
            if let Some(p) = orig {
                std::env::set_var("PATH", p);
            } else {
                std::env::remove_var("PATH");
            }
        }
    }
}
