use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::Path;

use crate::trie::{self, ArgSpec, CommandTrie};

/// Scan Zsh completion files for subcommand definitions and argument modes,
/// adding them to the trie.
///
/// Parses `_cmd-subcmd` function patterns from completion files (e.g.,
/// `_git-checkout` in `_git` means `checkout` is a subcommand of `git`).
///
/// Also extracts argument type actions (`_files`, `_directories`,
/// `_command_names`, etc.) to determine what kind of arguments each
/// command expects (paths, directories only, executables, etc.).
pub fn scan_completions(trie: &mut CommandTrie) -> u32 {
    let fpath_dirs = completion_dirs();
    let mut total = 0u32;

    let (subcmds, arg_specs, cmds_with_completions) = extract_from_dirs(&fpath_dirs);
    for (cmd, subs) in &subcmds {
        for sub in subs {
            trie.insert(&[cmd.as_str(), sub.as_str()]);
            total += 1;
        }
    }

    // Populate both the new arg_specs and the legacy arg_modes (for compat)
    for (cmd, spec) in &arg_specs {
        if let Some(mode) = spec.rest {
            trie.arg_modes.insert(cmd.clone(), mode);
        }
    }
    trie.arg_specs.extend(arg_specs);

    // Apply well-known hardcoded specs ONLY for commands without a Zsh completion file.
    // Commands with completion files are fully handled by the parser — no hardcoding.
    apply_well_known_specs(&mut trie.arg_specs, &cmds_with_completions);

    // Seed well-known deep subcommand hierarchies (docker compose, git subcommands, etc.)
    // These are commands where the Zsh completion files might not have _cmd-subcmd patterns.
    total += seed_well_known_subcommands(trie);

    // Load subcommand descriptions: YAML fallbacks first, then parsed overrides
    load_descriptions(trie, &fpath_dirs);

    total
}

/// Load subcommand descriptions into the trie.
/// 1. Load fallback descriptions from the bundled YAML file.
/// 2. Parse `command:'description'` pairs from Zsh completion files.
/// 3. Parsed descriptions override YAML fallbacks.
fn load_descriptions(trie: &mut CommandTrie, fpath_dirs: &[String]) {
    // Step 1: Load YAML fallback descriptions (bundled at compile time)
    let yaml_str = include_str!("../data/descriptions.yaml");
    let yaml_map: HashMap<String, HashMap<String, String>> =
        serde_yaml::from_str(yaml_str).unwrap_or_default();

    // Insert YAML descriptions as the base layer
    for (parent, subs) in yaml_map {
        let entry = trie.descriptions.entry(parent).or_default();
        for (sub, desc) in subs {
            entry.entry(sub).or_insert(desc);
        }
    }

    // Step 2: Parse descriptions from Zsh completion files (override YAML)
    let parsed = extract_descriptions_from_dirs(fpath_dirs);
    for (parent, subs) in parsed {
        let entry = trie.descriptions.entry(parent).or_default();
        for (sub, desc) in subs {
            // Parsed descriptions always win over YAML fallbacks
            entry.insert(sub, desc);
        }
    }
}

/// Extract `command:'description'` pairs from Zsh completion files.
///
/// Looks for patterns like:
///   `add:'add file contents to index'`
/// inside array assignments (e.g. `main_porcelain_commands=(...)`)
/// and `_describe` / `_arguments` subcommand lists.
fn extract_descriptions_from_dirs(dirs: &[String]) -> HashMap<String, HashMap<String, String>> {
    let mut all_descs: HashMap<String, HashMap<String, String>> = HashMap::new();

    for dir in dirs {
        let entries = match fs::read_dir(dir) {
            Ok(e) => e,
            Err(_) => continue,
        };

        for entry in entries.flatten() {
            let path = entry.path();
            if !path.is_file() {
                continue;
            }
            let filename = entry.file_name().to_string_lossy().to_string();
            // Only process completion files (start with _)
            if !filename.starts_with('_') || filename.starts_with("_.") {
                continue;
            }

            let cmd_name = &filename[1..]; // strip leading _
            if cmd_name.is_empty() {
                continue;
            }

            let content = match fs::read_to_string(&path) {
                Ok(c) => c,
                Err(_) => continue,
            };

            let descs = extract_descriptions_from_content(&content, cmd_name);
            for (parent, subs) in descs {
                let entry = all_descs.entry(parent).or_default();
                for (sub, desc) in subs {
                    entry.insert(sub, desc);
                }
            }
        }
    }

    all_descs
}

/// Parse subcommand descriptions from the content of a single Zsh completion file.
///
/// Recognizes the `command:'description'` pattern used in Zsh arrays like:
/// ```
/// commands=(
///   add:'add file contents to index'
///   commit:'record changes to repository'
/// )
/// ```
fn extract_descriptions_from_content(
    content: &str,
    cmd_name: &str,
) -> HashMap<String, HashMap<String, String>> {
    let mut result: HashMap<String, HashMap<String, String>> = HashMap::new();
    let mut in_array = false;
    let mut current_parent = cmd_name.to_string();

    for line in content.lines() {
        let trimmed = line.trim();

        // Detect array assignment start: `varname=(`
        if trimmed.contains("=(") && !trimmed.starts_with('#') {
            in_array = true;
            // Try to determine parent from variable name
            // e.g., `main_porcelain_commands=(` → parent is the file's command
            // For subcommand arrays like in _git-stash, derive parent
            if cmd_name.contains('-') {
                // e.g., _git-stash → parent = "git stash"
                current_parent = cmd_name.replacen('-', " ", 1);
            } else {
                current_parent = cmd_name.to_string();
            }

            // Check if there are entries on the same line as =(
            let after_paren = trimmed.split_once("=(").map(|(_, r)| r).unwrap_or("");
            extract_desc_entries(after_paren, &current_parent, &mut result);
            continue;
        }

        // Detect array end
        if in_array && trimmed.contains(')') {
            // There might be entries before the closing paren on this line
            let before_paren = trimmed.split(')').next().unwrap_or("");
            extract_desc_entries(before_paren, &current_parent, &mut result);
            in_array = false;
            continue;
        }

        // Inside an array: look for command:'description' entries
        if in_array {
            extract_desc_entries(trimmed, &current_parent, &mut result);
        }
    }

    result
}

/// Extract `command:'description'` entries from a line of text.
fn extract_desc_entries(
    line: &str,
    parent: &str,
    result: &mut HashMap<String, HashMap<String, String>>,
) {
    // Match patterns like: word:'description text'
    // The command part is a bare word (alphanumeric + hyphens),
    // followed by :'...' (single-quoted description)
    let mut i = 0;
    let bytes = line.as_bytes();

    while i < bytes.len() {
        // Skip whitespace
        while i < bytes.len() && bytes[i].is_ascii_whitespace() {
            i += 1;
        }
        if i >= bytes.len() {
            break;
        }

        // Try to find a word followed by :'
        let word_start = i;
        while i < bytes.len()
            && (bytes[i].is_ascii_alphanumeric() || bytes[i] == b'-' || bytes[i] == b'_')
        {
            i += 1;
        }

        let word_end = i;
        if word_end == word_start {
            i += 1;
            continue;
        }

        // Check for :' immediately after the word
        if i + 1 < bytes.len() && bytes[i] == b':' && bytes[i + 1] == b'\'' {
            let cmd = &line[word_start..word_end];
            i += 2; // skip :'

            // Read until closing quote
            let desc_start = i;
            while i < bytes.len() && bytes[i] != b'\'' {
                i += 1;
            }
            let desc = &line[desc_start..i];
            if i < bytes.len() {
                i += 1; // skip closing '
            }

            if !cmd.is_empty() && !desc.is_empty() {
                result
                    .entry(parent.to_string())
                    .or_default()
                    .insert(cmd.to_string(), desc.to_string());
            }
        }
    }
}

/// Seed the trie with well-known deep subcommand hierarchies that Zsh completion
/// files may not expose via the standard `_cmd-subcmd` pattern.
fn seed_well_known_subcommands(trie: &mut CommandTrie) -> u32 {
    let hierarchies: &[(&[&str], &[&str])] = &[
        // git deep subcommands
        (&["git", "stash"], &[
            "push", "pop", "list", "show", "apply", "drop", "clear", "branch",
        ]),
        (&["git", "remote"], &[
            "add", "remove", "rename", "get-url", "set-url", "show", "prune",
        ]),
        (&["git", "worktree"], &[
            "add", "list", "lock", "move", "prune", "remove", "repair", "unlock",
        ]),
        // docker compose subcommands
        (&["docker", "compose"], &[
            "build", "config", "create", "down", "events", "exec", "images",
            "kill", "logs", "ls", "pause", "port", "ps", "pull", "push",
            "restart", "rm", "run", "start", "stop", "top", "unpause", "up",
            "version", "watch",
        ]),
        // kubectl resource types (commonly used with get/describe/delete)
        (&["kubectl", "get"], &[
            "pods", "services", "deployments", "nodes", "namespaces",
            "configmaps", "secrets", "ingress", "jobs", "cronjobs",
            "statefulsets", "daemonsets", "replicasets", "pv", "pvc",
            "events", "endpoints", "serviceaccounts", "roles",
            "rolebindings", "clusterroles", "clusterrolebindings",
        ]),
        (&["kubectl", "describe"], &[
            "pods", "services", "deployments", "nodes", "namespaces",
            "configmaps", "secrets", "ingress",
        ]),
        // systemctl subcommands
        (&["systemctl"], &[
            "start", "stop", "restart", "reload", "status", "enable", "disable",
            "mask", "unmask", "daemon-reload", "is-active", "is-enabled", "is-failed",
            "list-units", "list-unit-files", "show", "cat", "edit", "kill",
        ]),
        // tmux common subcommands
        (&["tmux"], &[
            "new-session", "attach-session", "list-sessions", "kill-session",
            "new-window", "list-windows", "kill-window", "select-window",
            "split-window", "select-pane", "kill-pane", "resize-pane",
            "send-keys", "source-file", "set-option", "show-options",
            "list-keys", "list-clients", "switch-client", "rename-session",
            "rename-window", "detach-client", "display-message",
        ]),
        // helm subcommands
        (&["helm"], &[
            "completion", "create", "dependency", "env", "get", "history",
            "install", "lint", "list", "package", "plugin", "pull", "push",
            "registry", "repo", "rollback", "search", "show", "status",
            "template", "test", "uninstall", "upgrade", "verify", "version",
        ]),
        // ip subcommands
        (&["ip"], &[
            "addr", "addrlabel", "link", "maddr", "monitor", "neighbour",
            "netns", "route", "rule", "tunnel",
        ]),
        // journalctl is single-level (flags), no deep hierarchy needed
        // podman subcommands (parallel to docker)
        (&["podman"], &[
            "build", "compose", "container", "cp", "create", "exec",
            "image", "images", "inspect", "kill", "logs", "network",
            "pod", "ps", "pull", "push", "rm", "rmi", "run", "start",
            "stop", "volume",
        ]),
        // rustup subcommands
        (&["rustup"], &[
            "component", "default", "doc", "man", "override", "run",
            "self", "set", "show", "target", "toolchain", "update", "which",
        ]),
        // apt subcommands
        (&["apt"], &[
            "autoremove", "clean", "depends", "download", "full-upgrade",
            "install", "list", "purge", "rdepends", "reinstall", "remove",
            "search", "show", "update", "upgrade",
        ]),
        // dnf subcommands
        (&["dnf"], &[
            "autoremove", "check-update", "clean", "distro-sync", "downgrade",
            "group", "history", "info", "install", "list", "makecache",
            "provides", "reinstall", "remove", "repolist", "search",
            "update", "upgrade",
        ]),
        // yarn subcommands
        (&["yarn"], &[
            "add", "build", "cache", "config", "dedupe", "dlx", "info",
            "init", "install", "link", "pack", "plugin", "rebuild", "remove",
            "run", "search", "set", "start", "test", "up", "why", "workspace",
        ]),
        // pnpm subcommands
        (&["pnpm"], &[
            "add", "audit", "build", "create", "dedupe", "dlx", "exec",
            "fetch", "install", "link", "list", "outdated", "publish",
            "rebuild", "remove", "run", "start", "store", "test", "update", "why",
        ]),
    ];

    let mut count = 0u32;
    for (prefix, subcommands) in hierarchies {
        for sub in *subcommands {
            let mut words: Vec<&str> = prefix.to_vec();
            words.push(sub);
            trie.insert(&words);
            count += 1;
        }
    }
    count
}

/// Hardcoded arg specs for commands where Zsh completions use runtime-conditional
/// logic that static parsing can't resolve. These only fill in gaps — they won't
/// overwrite a position if the parser already detected a non-Paths type.
fn apply_well_known_specs(specs: &mut HashMap<String, ArgSpec>, cmds_with_completions: &HashSet<String>) {
    use trie::*;

    type Override<'a> = (&'a str, &'a [(u32, u8)], Option<u8>, &'a [(&'a str, u8)]);
    let overrides: &[Override] = &[
        // git subcommands — branches, tags, remotes
        (
            "git checkout",
            &[(1, ARG_MODE_GIT_BRANCHES)],
            Some(ARG_MODE_PATHS),
            &[("-b", ARG_MODE_GIT_BRANCHES), ("-B", ARG_MODE_GIT_BRANCHES)],
        ),
        (
            "git switch",
            &[(1, ARG_MODE_GIT_BRANCHES)],
            None,
            &[("-c", ARG_MODE_GIT_BRANCHES), ("-C", ARG_MODE_GIT_BRANCHES)],
        ),
        (
            "git branch",
            &[(1, ARG_MODE_GIT_BRANCHES)],
            None,
            &[
                ("-d", ARG_MODE_GIT_BRANCHES),
                ("-D", ARG_MODE_GIT_BRANCHES),
                ("-m", ARG_MODE_GIT_BRANCHES),
                ("-M", ARG_MODE_GIT_BRANCHES),
            ],
        ),
        ("git merge", &[(1, ARG_MODE_GIT_BRANCHES)], None, &[]),
        (
            "git rebase",
            &[(1, ARG_MODE_GIT_BRANCHES), (2, ARG_MODE_GIT_BRANCHES)],
            None,
            &[("--onto", ARG_MODE_GIT_BRANCHES)],
        ),
        ("git log", &[], Some(ARG_MODE_PATHS), &[]),
        ("git diff", &[], Some(ARG_MODE_PATHS), &[]),
        (
            "git push",
            &[(1, ARG_MODE_GIT_REMOTES), (2, ARG_MODE_GIT_BRANCHES)],
            None,
            &[],
        ),
        (
            "git pull",
            &[(1, ARG_MODE_GIT_REMOTES), (2, ARG_MODE_GIT_BRANCHES)],
            None,
            &[],
        ),
        ("git fetch", &[(1, ARG_MODE_GIT_REMOTES)], None, &[]),
        ("git tag", &[(1, ARG_MODE_GIT_TAGS)], None, &[]),
        ("git stash", &[], None, &[]),
        ("git rm", &[], Some(ARG_MODE_GIT_FILES), &[]),
        (
            "git restore",
            &[],
            Some(ARG_MODE_GIT_FILES),
            &[("--source", ARG_MODE_GIT_BRANCHES)],
        ),
        (
            "git reset",
            &[(1, ARG_MODE_GIT_BRANCHES)],
            Some(ARG_MODE_PATHS),
            &[],
        ),
        // kill — signals
        (
            "kill",
            &[],
            Some(ARG_MODE_PIDS),
            &[("-s", ARG_MODE_SIGNALS)],
        ),
        // ssh/scp — hosts
        (
            "ssh",
            &[(1, ARG_MODE_HOSTS)],
            None,
            &[("-l", ARG_MODE_USERS), ("-i", ARG_MODE_PATHS)],
        ),
        ("scp", &[], Some(ARG_MODE_PATHS), &[("-i", ARG_MODE_PATHS)]),
        // user/group commands
        ("chown", &[(1, ARG_MODE_USERS)], Some(ARG_MODE_PATHS), &[]),
        ("chgrp", &[(1, ARG_MODE_GROUPS)], Some(ARG_MODE_PATHS), &[]),
        ("su", &[(1, ARG_MODE_USERS)], None, &[]),
        ("sudo", &[], None, &[("-u", ARG_MODE_USERS)]),
        // network
        (
            "ping",
            &[(1, ARG_MODE_HOSTS)],
            None,
            &[("-I", ARG_MODE_NET_IFACES)],
        ),
        ("traceroute", &[(1, ARG_MODE_HOSTS)], None, &[]),
        ("dig", &[(1, ARG_MODE_HOSTS)], None, &[]),
        ("host", &[(1, ARG_MODE_HOSTS)], None, &[]),
        ("nslookup", &[(1, ARG_MODE_HOSTS)], None, &[]),
        ("ifconfig", &[(1, ARG_MODE_NET_IFACES)], None, &[]),
        ("ip", &[], None, &[]),
        // --- Docker ---
        ("docker build", &[], None, &[("-t", ARG_MODE_PATHS), ("-f", ARG_MODE_PATHS)]),
        ("docker run", &[], None, &[
            ("-v", ARG_MODE_PATHS), ("--volume", ARG_MODE_PATHS),
            ("-w", ARG_MODE_PATHS), ("--workdir", ARG_MODE_PATHS),
            ("--env-file", ARG_MODE_PATHS),
            ("-u", ARG_MODE_USERS), ("--user", ARG_MODE_USERS),
        ]),
        ("docker exec", &[], None, &[("-u", ARG_MODE_USERS), ("--user", ARG_MODE_USERS)]),
        ("docker cp", &[], Some(ARG_MODE_PATHS), &[]),
        ("docker compose", &[], None, &[("-f", ARG_MODE_PATHS), ("--file", ARG_MODE_PATHS)]),
        ("docker compose up", &[], None, &[]),
        ("docker compose down", &[], None, &[]),
        ("docker compose build", &[], None, &[]),
        ("docker compose logs", &[], None, &[]),
        ("docker compose exec", &[], None, &[]),
        ("docker compose run", &[], None, &[]),
        ("docker compose ps", &[], None, &[]),
        // --- Kubernetes (kubectl) ---
        ("kubectl apply", &[], None, &[
            ("-f", ARG_MODE_PATHS), ("--filename", ARG_MODE_PATHS),
        ]),
        ("kubectl create", &[], None, &[("-f", ARG_MODE_PATHS), ("--filename", ARG_MODE_PATHS)]),
        ("kubectl delete", &[], None, &[("-f", ARG_MODE_PATHS), ("--filename", ARG_MODE_PATHS)]),
        ("kubectl logs", &[], None, &[]),
        ("kubectl exec", &[], None, &[]),
        ("kubectl get", &[], None, &[("-o", ARG_MODE_PATHS)]),
        ("kubectl describe", &[], None, &[]),
        ("kubectl edit", &[], None, &[]),
        // --- systemctl ---
        ("systemctl start", &[], None, &[]),
        ("systemctl stop", &[], None, &[]),
        ("systemctl restart", &[], None, &[]),
        ("systemctl status", &[], None, &[]),
        ("systemctl enable", &[], None, &[]),
        ("systemctl disable", &[], None, &[]),
        ("journalctl", &[], None, &[("-u", ARG_MODE_PATHS)]),
        // --- Cargo (Rust) ---
        ("cargo build", &[], None, &[("--manifest-path", ARG_MODE_PATHS)]),
        ("cargo test", &[], None, &[("--manifest-path", ARG_MODE_PATHS)]),
        ("cargo run", &[], None, &[("--manifest-path", ARG_MODE_PATHS)]),
        ("cargo add", &[], None, &[]),
        ("cargo install", &[], None, &[("--path", ARG_MODE_PATHS)]),
        ("cargo clippy", &[], None, &[("--manifest-path", ARG_MODE_PATHS)]),
        // --- Node / npm / yarn ---
        ("npm install", &[], None, &[]),
        ("npm run", &[], None, &[]),
        ("npm test", &[], None, &[]),
        ("npx", &[(1, ARG_MODE_EXECS_ONLY)], None, &[]),
        ("yarn add", &[], None, &[]),
        ("yarn run", &[], None, &[]),
        // --- Python ---
        ("pip install", &[], None, &[("-r", ARG_MODE_PATHS)]),
        ("pip3 install", &[], None, &[("-r", ARG_MODE_PATHS)]),
        ("python", &[(1, ARG_MODE_PATHS)], None, &[("-m", ARG_MODE_EXECS_ONLY)]),
        ("python3", &[(1, ARG_MODE_PATHS)], None, &[("-m", ARG_MODE_EXECS_ONLY)]),
        // --- Homebrew ---
        ("brew install", &[], None, &[]),
        ("brew uninstall", &[], None, &[]),
        ("brew upgrade", &[], None, &[]),
        ("brew info", &[], None, &[]),
        ("brew search", &[], None, &[]),
        // --- Package managers ---
        ("apt install", &[], None, &[]),
        ("apt remove", &[], None, &[]),
        ("apt search", &[], None, &[]),
        ("dnf install", &[], None, &[]),
        ("dnf remove", &[], None, &[]),
        ("pacman", &[], None, &[]),
        // --- tmux ---
        ("tmux", &[], None, &[("-f", ARG_MODE_PATHS)]),
        // --- Make ---
        ("make", &[], None, &[("-f", ARG_MODE_PATHS), ("-C", ARG_MODE_DIRS_ONLY)]),
        // --- curl / wget ---
        ("curl", &[(1, ARG_MODE_URLS)], None, &[
            ("-o", ARG_MODE_PATHS), ("--output", ARG_MODE_PATHS),
            ("-d", ARG_MODE_PATHS), ("--data", ARG_MODE_PATHS),
            ("--cacert", ARG_MODE_PATHS), ("--cert", ARG_MODE_PATHS),
            ("--key", ARG_MODE_PATHS),
        ]),
        ("wget", &[(1, ARG_MODE_URLS)], None, &[
            ("-O", ARG_MODE_PATHS), ("--output-document", ARG_MODE_PATHS),
            ("-P", ARG_MODE_DIRS_ONLY), ("--directory-prefix", ARG_MODE_DIRS_ONLY),
        ]),
        // --- rsync ---
        ("rsync", &[], Some(ARG_MODE_PATHS), &[]),
        // --- awk/sed on files ---
        ("awk", &[], None, &[("-f", ARG_MODE_PATHS)]),
        ("sed", &[], None, &[("-f", ARG_MODE_PATHS), ("-i", ARG_MODE_PATHS)]),
        // --- Go ---
        ("go build", &[], Some(ARG_MODE_PATHS), &[("-o", ARG_MODE_PATHS)]),
        ("go test", &[], Some(ARG_MODE_PATHS), &[]),
        ("go run", &[], Some(ARG_MODE_PATHS), &[]),
        ("go install", &[], None, &[]),
        ("go get", &[], None, &[]),
        // --- Terraform ---
        ("terraform apply", &[], None, &[("-var-file", ARG_MODE_PATHS)]),
        ("terraform plan", &[], None, &[("-var-file", ARG_MODE_PATHS)]),
        ("terraform import", &[], None, &[]),
        ("terraform destroy", &[], None, &[("-var-file", ARG_MODE_PATHS)]),
        // --- Ansible ---
        ("ansible-playbook", &[(1, ARG_MODE_PATHS)], None, &[
            ("-i", ARG_MODE_PATHS), ("--inventory", ARG_MODE_PATHS),
            ("-e", ARG_MODE_PATHS), ("--extra-vars", ARG_MODE_PATHS),
        ]),
        ("ansible", &[(1, ARG_MODE_HOSTS)], None, &[
            ("-i", ARG_MODE_PATHS), ("--inventory", ARG_MODE_PATHS),
        ]),
        // --- journalctl ---
        ("journalctl", &[], None, &[
            ("-u", ARG_MODE_EXECS_ONLY), ("--unit", ARG_MODE_EXECS_ONLY),
        ]),
        // --- kill/pkill/killall ---
        ("kill", &[(1, ARG_MODE_PIDS)], Some(ARG_MODE_PIDS), &[
            ("-s", ARG_MODE_SIGNALS),
        ]),
        ("pkill", &[], None, &[
            ("-signal", ARG_MODE_SIGNALS), ("-U", ARG_MODE_USERS),
        ]),
        ("killall", &[(1, ARG_MODE_SIGNALS)], None, &[]),
        // --- ssh/scp ---
        ("ssh", &[(1, ARG_MODE_HOSTS)], None, &[
            ("-i", ARG_MODE_PATHS), ("-F", ARG_MODE_PATHS),
            ("-l", ARG_MODE_USERS), ("-p", ARG_MODE_PORTS),
        ]),
        ("scp", &[], Some(ARG_MODE_PATHS), &[
            ("-i", ARG_MODE_PATHS), ("-F", ARG_MODE_PATHS), ("-P", ARG_MODE_PORTS),
        ]),
        // --- chown/chgrp ---
        ("chown", &[(1, ARG_MODE_USERS)], Some(ARG_MODE_PATHS), &[]),
        ("chgrp", &[(1, ARG_MODE_GROUPS)], Some(ARG_MODE_PATHS), &[]),
        // --- helm ---
        ("helm install", &[], None, &[("-f", ARG_MODE_PATHS), ("--values", ARG_MODE_PATHS)]),
        ("helm upgrade", &[], None, &[("-f", ARG_MODE_PATHS), ("--values", ARG_MODE_PATHS)]),
        // --- podman (mirrors docker specs) ---
        ("podman build", &[], None, &[("-f", ARG_MODE_PATHS), ("--file", ARG_MODE_PATHS)]),
        ("podman run", &[], None, &[("-v", ARG_MODE_PATHS), ("--volume", ARG_MODE_PATHS)]),
        ("podman exec", &[], None, &[("-u", ARG_MODE_USERS)]),
        ("podman cp", &[], Some(ARG_MODE_PATHS), &[]),
    ];

    for &(cmd, positional, rest, flags) in overrides {
        // Skip entirely if the base command has a Zsh completion file —
        // everything must come from the parser in that case.
        let base_cmd = cmd.split_whitespace().next().unwrap_or(cmd);
        if cmds_with_completions.contains(base_cmd) {
            continue;
        }

        let spec = specs.entry(cmd.to_string()).or_default();
        for &(pos, arg_type) in positional {
            let existing = spec.positional.get(&pos).copied();
            if existing.is_none() || existing == Some(ARG_MODE_PATHS) || existing == Some(0) {
                spec.positional.insert(pos, arg_type);
            }
        }
        if let Some(r) = rest
            && (spec.rest.is_none() || spec.rest == Some(ARG_MODE_PATHS))
        {
            spec.rest = Some(r);
        }
        for &(flag, arg_type) in flags {
            let existing = spec.flag_args.get(flag).copied();
            if existing.is_none() || existing == Some(ARG_MODE_PATHS) || existing == Some(0) {
                spec.flag_args.insert(flag.to_string(), arg_type);
            }
        }
    }
}

fn completion_dirs() -> Vec<String> {
    let mut dirs = Vec::new();

    // Standard Zsh completion directories
    for pattern in &[
        "/usr/share/zsh/*/functions",
        "/usr/local/share/zsh/site-functions",
        "/opt/homebrew/share/zsh/site-functions",
    ] {
        if let Ok(entries) = glob_simple(pattern) {
            dirs.extend(entries);
        }
    }

    // Also check $fpath from environment if available
    if let Ok(fpath) = std::env::var("FPATH") {
        for dir in fpath.split(':') {
            if !dir.is_empty() && !dirs.contains(&dir.to_string()) {
                dirs.push(dir.to_string());
            }
        }
    }

    dirs
}

/// Extract subcommands and per-position argument specs from completion files.
/// Returns (command -> subcommands, command -> ArgSpec).
fn extract_from_dirs(
    dirs: &[String],
) -> (
    HashMap<String, Vec<String>>,
    HashMap<String, ArgSpec>,
    HashSet<String>,
) {
    let mut subcmds: HashMap<String, Vec<String>> = HashMap::new();
    let mut arg_specs: HashMap<String, ArgSpec> = HashMap::new();
    let mut cmds_with_completions: HashSet<String> = HashSet::new();

    for dir in dirs {
        let dir_path = Path::new(dir);
        let entries = match fs::read_dir(dir_path) {
            Ok(e) => e,
            Err(_) => continue,
        };

        for entry in entries.flatten() {
            let name = entry.file_name();
            let name = name.to_string_lossy();

            if !name.starts_with('_') || name.starts_with("__") {
                continue;
            }

            let cmd = &name[1..];
            if cmd.is_empty() || cmd.contains('.') {
                continue;
            }

            if is_internal_completion(cmd) {
                continue;
            }

            let file_path = entry.path();
            let real_path = match fs::canonicalize(&file_path) {
                Ok(p) => p,
                Err(_) => file_path,
            };

            if let Ok(content) = fs::read_to_string(&real_path) {
                // Track every command name this file covers.
                cmds_with_completions.insert(cmd.to_string());
                let commands = parse_compdef_commands(&content);
                for c in &commands {
                    cmds_with_completions.insert(c.clone());
                }

                let subs = extract_subcommands_from_content(cmd, &content);
                if !subs.is_empty() {
                    subcmds.entry(cmd.to_string()).or_default().extend(subs);
                }

                let spec = parse_arg_spec(&content);
                if !spec.is_empty() {
                    for c in &commands {
                        arg_specs.insert(c.clone(), spec.clone());
                    }
                    if commands.is_empty() {
                        arg_specs.insert(cmd.to_string(), spec);
                    }
                }

                // Parse subcommand function bodies for per-subcommand arg specs.
                // e.g., _git-add () { _arguments ... '*:file:_files' } → "git add" → Paths
                let sub_specs = extract_subcommand_arg_specs(cmd, &content);
                arg_specs.extend(sub_specs);
            }
        }
    }

    for subs in subcmds.values_mut() {
        subs.sort();
        subs.dedup();
    }

    (subcmds, arg_specs, cmds_with_completions)
}

/// Parse the `#compdef` header to get the list of commands this file covers.
/// e.g., `#compdef cd chdir pushd` → ["cd", "chdir", "pushd"]
fn parse_compdef_commands(content: &str) -> Vec<String> {
    for line in content.lines() {
        let trimmed = line.trim();
        if let Some(rest) = trimmed.strip_prefix("#compdef ") {
            return rest
                .split_whitespace()
                .filter(|w| !w.starts_with('-'))
                .map(String::from)
                .collect();
        }
        // #compdef must be in the first few lines
        if !trimmed.is_empty() && !trimmed.starts_with('#') {
            break;
        }
    }
    vec![]
}

/// Detect the argument type from a Zsh completion action string.
/// Recognizes all standard Zsh completion helpers, not just files/dirs/execs.
/// Check if `s` contains `func` as a standalone identifier (word-boundary match).
/// Prevents "_files" from matching "__git_tree_files".
fn contains_func_name(s: &str, func: &str) -> bool {
    let bytes = s.as_bytes();
    let flen = func.len();
    let mut start = 0;
    while let Some(pos) = s[start..].find(func).map(|p| p + start) {
        let before_ok = pos == 0
            || !matches!(bytes.get(pos - 1), Some(b) if b.is_ascii_alphanumeric() || *b == b'_');
        let after_ok =
            !matches!(bytes.get(pos + flen), Some(b) if b.is_ascii_alphanumeric() || *b == b'_');
        if before_ok && after_ok {
            return true;
        }
        start = pos + 1;
    }
    false
}

fn action_to_arg_type(action: &str) -> Option<u8> {
    let action = action.trim().trim_matches('\'').trim_matches('"');

    // Commands / executables
    if action.contains("_command_names")
        || action.contains("_path_commands")
        || action.contains(":_commands")
    {
        return Some(trie::ARG_MODE_EXECS_ONLY);
    }

    // Directories — must test before generic _files to catch "_files -/"
    if action.contains("_directories")
        || action.contains("_files -/")
        || action.contains("_path_files -/")
        || (action.contains("_path_files") && action.contains("-/"))
    {
        return Some(trie::ARG_MODE_DIRS_ONLY);
    }

    // Files (general) — word-boundary check so "__git_tree_files" doesn't match
    if contains_func_name(action, "_files") || contains_func_name(action, "_path_files") {
        return Some(trie::ARG_MODE_PATHS);
    }

    // Git-specific (checked before generic resources to avoid false positives)
    if action.contains("__git_branch_names")
        || action.contains("__git_heads")
        || action.contains("_git_branch")
    {
        return Some(trie::ARG_MODE_GIT_BRANCHES);
    }
    if action.contains("__git_tags") || action.contains("__git_commit_tags") {
        return Some(trie::ARG_MODE_GIT_TAGS);
    }
    if action.contains("__git_remotes") {
        return Some(trie::ARG_MODE_GIT_REMOTES);
    }
    // Git tree files and cached/modified/indexed files → GIT_FILES
    if action.contains("__git_files")
        || action.contains("__git_cached_files")
        || action.contains("__git_modified_files")
        || action.contains("__git_tree_files")
    {
        return Some(trie::ARG_MODE_GIT_FILES);
    }
    // Untracked/other working-tree files are just regular filesystem files.
    if action.contains("__git_other_files") {
        return Some(trie::ARG_MODE_PATHS);
    }
    // Commits, tree-ishs — resolve as branches (closest approximation).
    if action.contains("__git_commits")
        || action.contains("__git_tree_ish")
        || action.contains("__git_recent")
    {
        return Some(trie::ARG_MODE_GIT_BRANCHES);
    }

    // System resources
    if action.contains("_users") || action.contains("_ssh_users") {
        return Some(trie::ARG_MODE_USERS);
    }
    if action.contains("_groups") {
        return Some(trie::ARG_MODE_GROUPS);
    }
    if action.contains("_hosts") || action.contains("_ssh_hosts") {
        return Some(trie::ARG_MODE_HOSTS);
    }
    if action.contains("_pids") {
        return Some(trie::ARG_MODE_PIDS);
    }
    if action.contains("_signals") {
        return Some(trie::ARG_MODE_SIGNALS);
    }
    if action.contains("_ports") {
        return Some(trie::ARG_MODE_PORTS);
    }
    if action.contains("_net_interfaces") {
        return Some(trie::ARG_MODE_NET_IFACES);
    }
    if action.contains("_urls") {
        return Some(trie::ARG_MODE_URLS);
    }
    if action.contains("_locales") {
        return Some(trie::ARG_MODE_LOCALES);
    }

    None
}

/// Extract argument specs from subcommand function bodies within a completion file.
///
/// Finds functions like `_git-add () { ... }` and parses each body for
/// `_arguments` specs. Returns a map of "cmd subcmd" → ArgSpec.
///
/// Uses a simple approach: split the file at function definition lines
/// and parse the content between each pair.
fn extract_subcommand_arg_specs(cmd: &str, content: &str) -> HashMap<String, ArgSpec> {
    let mut specs = HashMap::new();
    let prefix = format!("_{}-", cmd);

    // Find all function definition positions and their subcmd names
    let mut funcs: Vec<(usize, String)> = Vec::new();
    for (line_idx, line) in content.lines().enumerate() {
        let trimmed = line.trim();
        if !trimmed.starts_with(&prefix) {
            continue;
        }
        let after_prefix = &trimmed[prefix.len()..];
        let subcmd: String = after_prefix
            .chars()
            .take_while(|c| c.is_alphanumeric() || *c == '-' || *c == '_')
            .collect();
        if subcmd.is_empty() || subcmd.len() >= 40 {
            continue;
        }
        // Verify it looks like a function definition (has parens)
        let rest = &after_prefix[subcmd.len()..];
        if rest.contains('(') || rest.trim_start().starts_with("()") {
            funcs.push((line_idx, subcmd));
        }
    }

    if funcs.is_empty() {
        return specs;
    }

    // Extract the content between consecutive function definitions
    let lines: Vec<&str> = content.lines().collect();
    for (idx, (start_line, subcmd)) in funcs.iter().enumerate() {
        let end_line = if idx + 1 < funcs.len() {
            funcs[idx + 1].0
        } else {
            lines.len()
        };

        let body: String = lines[*start_line..end_line].join("\n");
        let spec = parse_arg_spec(&body);
        if !spec.is_empty() {
            let key = format!("{} {}", cmd, subcmd);
            specs.insert(key, spec);
        }
    }

    specs
}

/// Describes where a `->state` reference appears in an `_arguments` spec.
enum StateRefKind {
    /// `'*:desc:->state'` or `'*::args:->state'` — remaining positional args.
    Rest,
    /// `'N:desc:->state'` — specific positional argument.
    Positional(u32),
    /// `'-f+:desc:->state'` — flag that consumes a typed value.
    Flag(String),
}

/// Scan `_arguments` spec strings for `->state` references.
/// These are specs where the action part is `->statename` instead of a
/// completion function, delegating to a `case $state` dispatch block.
fn extract_state_refs(content: &str) -> Vec<(StateRefKind, String)> {
    let mut refs = Vec::new();

    for line in content.lines() {
        let trimmed = line.trim().trim_end_matches('\\').trim();
        if trimmed.starts_with('#') {
            continue;
        }

        // Extract single-quoted strings containing ->
        let mut chars = trimmed.chars().peekable();
        while let Some(&ch) = chars.peek() {
            if ch == '\'' {
                chars.next();
                let mut s = String::new();
                while let Some(&c) = chars.peek() {
                    if c == '\'' {
                        chars.next();
                        break;
                    }
                    s.push(c);
                    chars.next();
                }
                if s.contains("->")
                    && let Some(r) = parse_state_ref(&s)
                {
                    refs.push(r);
                }
            } else {
                chars.next();
            }
        }
    }

    refs
}

/// Parse a `->state` reference from a single `_arguments` spec string.
/// Determines what kind of argument (rest, positional, flag) the state applies to.
fn parse_state_ref(spec: &str) -> Option<(StateRefKind, String)> {
    // Split on colons (respecting brackets) to find the action
    let mut parts: Vec<String> = Vec::new();
    let mut current = String::new();
    let mut bracket_depth: u32 = 0;
    for ch in spec.chars() {
        match ch {
            '[' => {
                bracket_depth += 1;
                current.push(ch);
            }
            ']' => {
                bracket_depth = bracket_depth.saturating_sub(1);
                current.push(ch);
            }
            ':' if bracket_depth == 0 => {
                parts.push(std::mem::take(&mut current));
            }
            _ => current.push(ch),
        }
    }
    parts.push(current);

    let action = parts.last()?.trim();
    let state_name = action.strip_prefix("->")?;
    let state_name = state_name.trim();
    if state_name.is_empty() {
        return None;
    }

    let s = spec.trim();
    let kind = if s.starts_with('*') {
        StateRefKind::Rest
    } else if s.starts_with('-') || s.starts_with('(') {
        let after_excl = if s.starts_with('(') {
            s.find(')').map(|end| s[end + 1..].trim()).unwrap_or(s)
        } else {
            s
        };
        let flag: String = after_excl
            .chars()
            .take_while(|c| !matches!(*c, '[' | ':' | ' '))
            .collect();
        let flag = flag.trim_end_matches('+').trim_end_matches('=').to_string();
        if flag.starts_with('-') && flag.len() > 1 {
            StateRefKind::Flag(flag)
        } else {
            StateRefKind::Rest
        }
    } else if s.chars().next().is_some_and(|c| c.is_ascii_digit()) {
        let pos_str: String = s.chars().take_while(|c| c.is_ascii_digit()).collect();
        match pos_str.parse::<u32>() {
            Ok(n) => StateRefKind::Positional(n),
            Err(_) => StateRefKind::Rest,
        }
    } else if s.starts_with(':') {
        // Bare ':desc:->state' — means "next positional argument" (position 1)
        StateRefKind::Positional(1)
    } else {
        StateRefKind::Rest
    };

    Some((kind, state_name.to_string()))
}

/// Find `case $state`/`case "$state"`/`case "$lstate"` blocks and determine
/// the argument type for each state handler by scanning for _files/_directories
/// calls and _alternative action specs.
fn extract_state_types(content: &str) -> HashMap<String, u8> {
    let mut types = HashMap::new();
    let lines: Vec<&str> = content.lines().collect();
    let mut i = 0;

    while i < lines.len() {
        let trimmed = lines[i].trim();
        // Match: case $state in / case "$state" in / case $lstate in
        if trimmed.starts_with("case ")
            && trimmed.ends_with(" in")
            && (trimmed.contains("$state") || trimmed.contains("$lstate"))
        {
            i += 1;
            let mut case_depth: u32 = 1;
            let mut current_state: Option<String> = None;
            let mut current_body = String::new();

            while i < lines.len() && case_depth > 0 {
                let line = lines[i].trim();

                // Track nested case/esac
                if line.starts_with("case ") && line.ends_with(" in") {
                    case_depth += 1;
                    current_body.push_str(lines[i]);
                    current_body.push('\n');
                    i += 1;
                    continue;
                }
                if line == "esac"
                    || line.starts_with("esac ")
                    || line.starts_with("esac;")
                    || line.starts_with("esac)")
                {
                    case_depth -= 1;
                    if case_depth == 0 {
                        if let Some(state) = current_state.take()
                            && let Some(t) = detect_type_in_block(&current_body)
                        {
                            types.insert(state, t);
                        }
                        break;
                    }
                    current_body.push_str(lines[i]);
                    current_body.push('\n');
                    i += 1;
                    continue;
                }

                // At top level of our case block, check for new case arms
                if case_depth == 1 {
                    if let Some(name) = extract_case_arm_name(line) {
                        if let Some(prev) = current_state.take()
                            && let Some(t) = detect_type_in_block(&current_body)
                        {
                            types.insert(prev, t);
                        }
                        current_state = Some(name);
                        current_body.clear();
                    } else {
                        current_body.push_str(lines[i]);
                        current_body.push('\n');
                    }
                } else {
                    current_body.push_str(lines[i]);
                    current_body.push('\n');
                }

                i += 1;
            }
        }
        i += 1;
    }

    types
}

/// Extract a state name from a `case` arm line.
/// Matches `(statename)` or `statename)` but not wildcards or OR patterns.
fn extract_case_arm_name(line: &str) -> Option<String> {
    let line = line.trim();
    if !line.ends_with(')') {
        return None;
    }
    let inner = if line.starts_with('(') {
        &line[1..line.len() - 1]
    } else {
        &line[..line.len() - 1]
    };
    if inner.is_empty() || inner.contains('|') || inner.contains('*') || inner.contains(' ') {
        return None;
    }
    if inner
        .chars()
        .all(|c| c.is_alphanumeric() || c == '-' || c == '_')
    {
        Some(inner.to_string())
    } else {
        None
    }
}

/// Detect the dominant argument type in a block of shell code.
/// Checks for _files/_directories/_command_names in direct calls
/// and within `_alternative` / `_values` / `_regex_words` action specs.
/// Also detects runtime types (_users, _hosts, _pids, _signals, etc.).
fn detect_type_in_block(body: &str) -> Option<u8> {
    let mut has_files = false;
    let mut has_dirs = false;
    let mut has_execs = false;
    let mut runtime_type: Option<u8> = None;

    for line in body.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with('#') {
            continue;
        }

        // Direct calls — filesystem types
        if trimmed.contains("_directories")
            || (trimmed.contains("_path_files") && trimmed.contains("-/"))
            || trimmed.contains("_files -/")
        {
            has_dirs = true;
        }
        if (contains_func_name(trimmed, "_files") || contains_func_name(trimmed, "_path_files"))
            && !trimmed.contains("-/")
        {
            has_files = true;
        }
        if trimmed.contains("_command_names")
            || trimmed.contains("_path_commands")
            || trimmed.contains(":_commands")
        {
            has_execs = true;
        }

        // Direct calls — runtime types (bare function calls in state handler bodies)
        // e.g. `_pids`, `_ssh_hosts`, `_wanted hosts ... _ssh_hosts`
        if let Some(t) = action_to_arg_type(trimmed) {
            accumulate_runtime_type(t, &mut has_files, &mut has_dirs, &mut has_execs, &mut runtime_type);
        }

        // Parse single-quoted 'tag:desc:action' specs from _alternative / _values
        scan_quoted_action_specs(trimmed, &mut has_files, &mut has_dirs, &mut has_execs, &mut runtime_type);
    }

    if has_execs && !has_files && !has_dirs && runtime_type.is_none() {
        Some(trie::ARG_MODE_EXECS_ONLY)
    } else if has_dirs && !has_files && runtime_type.is_none() {
        Some(trie::ARG_MODE_DIRS_ONLY)
    } else if has_files || has_dirs {
        Some(trie::ARG_MODE_PATHS)
    } else {
        runtime_type
    }
}

/// Merge a newly detected type into the running accumulators.
/// Filesystem types set the has_* flags; runtime types are stored separately.
/// If two different runtime types appear in the same block, fall back to PATHS.
fn accumulate_runtime_type(
    t: u8,
    has_files: &mut bool,
    has_dirs: &mut bool,
    has_execs: &mut bool,
    runtime_type: &mut Option<u8>,
) {
    match t {
        trie::ARG_MODE_DIRS_ONLY => *has_dirs = true,
        trie::ARG_MODE_PATHS => *has_files = true,
        trie::ARG_MODE_EXECS_ONLY => *has_execs = true,
        other => {
            if let Some(existing) = *runtime_type {
                if existing != other {
                    // Users + Groups → combined type (e.g. chown/chgrp shared state body)
                    let combined = match (existing, other) {
                        (trie::ARG_MODE_USERS, trie::ARG_MODE_GROUPS)
                        | (trie::ARG_MODE_GROUPS, trie::ARG_MODE_USERS) => {
                            Some(trie::ARG_MODE_USERS_GROUPS)
                        }
                        // All other conflicts: keep the first type seen
                        _ => None,
                    };
                    if let Some(t) = combined {
                        *runtime_type = Some(t);
                    }
                }
            } else {
                *runtime_type = Some(other);
            }
        }
    }
}

/// Scan single-quoted strings on a line for `tag:desc:action` patterns
/// (used by `_alternative`, `_values`) and `:tag:desc:action` patterns
/// (used by `_regex_arguments`, `_regex_words`).
fn scan_quoted_action_specs(
    line: &str,
    has_files: &mut bool,
    has_dirs: &mut bool,
    has_execs: &mut bool,
    runtime_type: &mut Option<u8>,
) {
    let mut chars = line.chars().peekable();
    while let Some(&ch) = chars.peek() {
        if ch == '\'' {
            chars.next();
            let mut s = String::new();
            while let Some(&c) = chars.peek() {
                if c == '\'' {
                    chars.next();
                    break;
                }
                s.push(c);
                chars.next();
            }

            // _alternative / _values format: 'tag:desc:action'
            let colon_parts: Vec<&str> = s.splitn(3, ':').collect();
            if colon_parts.len() >= 3
                && let Some(t) = action_to_arg_type(colon_parts[2])
            {
                accumulate_runtime_type(t, has_files, has_dirs, has_execs, runtime_type);
            }

            // _regex_arguments / _regex_words format: ':tag:desc:action'
            if let Some(stripped) = s.strip_prefix(':') {
                let parts: Vec<&str> = stripped.splitn(3, ':').collect();
                if parts.len() >= 3
                    && let Some(t) = action_to_arg_type(parts[2])
                {
                    accumulate_runtime_type(t, has_files, has_dirs, has_execs, runtime_type);
                }
            }
        } else {
            chars.next();
        }
    }
}

/// Parse per-position and per-flag argument specs from a completion file.
///
/// Extracts from `_arguments` specs:
/// - `'N:desc:_files'` → position N expects files
/// - `'*:desc:_files'` → all remaining args expect files
/// - `'-f+:desc:_files'` → flag -f takes a file argument
/// - `'--flag=:desc:_files'` → flag --flag takes a file argument
fn parse_arg_spec(content: &str) -> ArgSpec {
    let mut spec = ArgSpec::default();

    for line in content.lines() {
        let trimmed = line.trim().trim_end_matches('\\').trim();
        if trimmed.starts_with('#') {
            continue;
        }

        // We're looking for _arguments spec strings.
        // These are single-quoted or double-quoted strings with colons separating
        // the spec parts: 'specifier:description:action'
        // The action (after the last colon) tells us what type of completion.

        // Extract quoted argument specs from the line
        for spec_str in extract_argument_specs(trimmed) {
            process_spec_string(&spec_str, &mut spec);
        }

        // Also catch bare _files/_directories calls used as direct actions
        // in non-_arguments style completions (e.g., `_diff_options ... ':file:_files'`)
        if !trimmed.contains("_arguments")
            && (trimmed.contains(":_files")
                || trimmed.contains(":_directories")
                || trimmed.contains(":_command"))
        {
            // Try to parse colon-separated specs in the line
            for part in trimmed.split_whitespace() {
                let part = part.trim_matches('\'').trim_matches('"');
                if part.contains(':') {
                    process_spec_string(part, &mut spec);
                }
            }
        }
    }

    // Resolve ->state references: connect _arguments `->statename` specs
    // to the types detected in `case $state` handler bodies.
    let state_refs = extract_state_refs(content);
    if !state_refs.is_empty() {
        let state_types = extract_state_types(content);
        for (kind, state_name) in state_refs {
            if let Some(&arg_type) = state_types.get(&state_name) {
                match kind {
                    StateRefKind::Rest => {
                        if spec.rest.is_none() {
                            spec.rest = Some(arg_type);
                        }
                    }
                    StateRefKind::Positional(pos) => {
                        spec.positional.entry(pos).or_insert(arg_type);
                    }
                    StateRefKind::Flag(flag) => {
                        spec.flag_args.entry(flag).or_insert(arg_type);
                    }
                }
            }
        }
    }

    // Fallback: if we found no structured specs, scan for bare action calls,
    // _alternative specs, _regex_arguments actions, etc.
    if spec.is_empty()
        && let Some(mode) = detect_dominant_action(content)
    {
        spec.rest = Some(mode);
    }

    spec
}

/// Scan a completion file for the dominant action when no structured
/// _arguments specs were found. Delegates to `detect_type_in_block` which
/// handles direct calls, `_alternative` specs, and `_regex_arguments` actions.
fn detect_dominant_action(content: &str) -> Option<u8> {
    detect_type_in_block(content)
}

/// Extract argument spec strings from a line.
/// Looks for single-quoted strings that contain colons (argument specs).
fn extract_argument_specs(line: &str) -> Vec<String> {
    let mut specs = Vec::new();
    let mut chars = line.chars().peekable();

    while let Some(&ch) = chars.peek() {
        if ch == '\'' {
            chars.next(); // consume opening quote
            let mut s = String::new();
            while let Some(&c) = chars.peek() {
                if c == '\'' {
                    chars.next();
                    break;
                }
                s.push(c);
                chars.next();
            }
            // Include strings that look like argument specs (contain colons
            // and a completion action we recognize)
            if s.contains(':') && has_known_action(&s) {
                specs.push(s);
            }
        } else {
            chars.next();
        }
    }

    // Also handle brace-expanded flag specs:
    // '(excl)'{-f,--flag=}'[desc]:label:_action'
    // The flag names are in the unquoted brace group; the action is in the
    // subsequent quoted description string.
    specs.extend(extract_brace_expanded_specs(line));

    specs
}

/// Extract synthesized flag specs from brace-expanded `_arguments` patterns.
///
/// Pattern: (optional quoted exclusion) `{-f,--flag=}` `'[desc]:label:_action'`
///
/// Produces: `-f[desc]:label:_action`, `--flag[desc]:label:_action`, ...
/// so that `process_spec_string` can classify the flag type.
fn extract_brace_expanded_specs(line: &str) -> Vec<String> {
    let mut result = Vec::new();
    let bytes = line.as_bytes();
    let len = bytes.len();
    let mut i = 0;

    while i < len {
        if bytes[i] != b'{' {
            i += 1;
            continue;
        }

        // Find the matching closing brace
        let brace_start = i;
        let mut depth = 0u32;
        let mut j = i;
        while j < len {
            match bytes[j] {
                b'{' => depth += 1,
                b'}' => {
                    depth -= 1;
                    if depth == 0 {
                        break;
                    }
                }
                _ => {}
            }
            j += 1;
        }
        if j >= len {
            i += 1;
            continue;
        }
        let brace_end = j; // position of closing '}'
        let brace_content = &line[brace_start + 1..brace_end];

        // Must look like a flag list (contains '-')
        if !brace_content.contains('-') {
            i = brace_end + 1;
            continue;
        }

        // Skip optional whitespace after '}'
        let mut k = brace_end + 1;
        while k < len && bytes[k] == b' ' {
            k += 1;
        }

        // Must be followed by a single-quoted description+action string
        if k >= len || bytes[k] != b'\'' {
            i = brace_end + 1;
            continue;
        }

        // Extract the quoted description+action
        k += 1; // skip opening quote
        let mut desc_action = String::new();
        while k < len && bytes[k] != b'\'' {
            desc_action.push(bytes[k] as char);
            k += 1;
        }

        // Only if it starts with '[' (description-only form) and has a known action
        if !desc_action.starts_with('[') || !desc_action.contains(':') || !has_known_action(&desc_action) {
            i = brace_end + 1;
            continue;
        }

        // Expand brace: parse comma-separated flags, trim + and = suffixes
        for flag_raw in brace_content.split(',') {
            let flag = flag_raw
                .trim()
                .trim_end_matches('+')
                .trim_end_matches('=');
            if flag.starts_with('-') && flag.len() > 1 {
                // Synthesize: "-f[desc]:label:_action"
                result.push(format!("{}{}", flag, desc_action));
            }
        }

        i = brace_end + 1;
    }

    result
}

/// Process a single _arguments spec string and add to the ArgSpec.
fn process_spec_string(spec_str: &str, spec: &mut ArgSpec) {
    // Find the action: it's after the last colon that isn't inside brackets
    let action = match find_action_in_spec(spec_str) {
        Some(a) => a,
        None => return,
    };

    let arg_type = match action_to_arg_type(&action) {
        Some(t) => t,
        None => return,
    };

    let s = spec_str.trim();

    // Positional: starts with a digit or *
    if s.starts_with('*') {
        spec.rest = Some(arg_type);
        return;
    }

    if let Some(first_char) = s.chars().next()
        && first_char.is_ascii_digit()
        && let Ok(pos) = s
            .chars()
            .take_while(|c| c.is_ascii_digit())
            .collect::<String>()
            .parse::<u32>()
    {
        spec.positional.insert(pos, arg_type);
        return;
    }

    // Positional with exclusion group: '(-a -b)N:desc:action'
    // The ( ... ) is a mutual-exclusion list; after it comes the position digit.
    if s.starts_with('(') {
        if let Some(close) = s.find(')') {
            let after = s[close + 1..].trim_start();
            if after.starts_with('*') {
                spec.rest = Some(arg_type);
                return;
            }
            if let Some(fc) = after.chars().next()
                && fc.is_ascii_digit()
                && let Ok(pos) = after
                    .chars()
                    .take_while(|c| c.is_ascii_digit())
                    .collect::<String>()
                    .parse::<u32>()
            {
                spec.positional.insert(pos, arg_type);
                return;
            }
        }
    }

    // Flag spec: starts with -
    if s.starts_with('-') {
        let flags = extract_flags_from_spec(s);
        for flag in flags {
            spec.flag_args.insert(flag, arg_type);
        }
    }
}

/// Check whether a string contains any known Zsh completion action function.
fn has_known_action(s: &str) -> bool {
    const KNOWN_ACTIONS: &[&str] = &[
        "_files",
        "_directories",
        "_command",
        "_path_files",
        "_path_commands",
        "_command_names",
        "_users",
        "_groups",
        "_hosts",
        "_ssh_hosts",
        "_ssh_users",
        "_pids",
        "_signals",
        "_ports",
        "_net_interfaces",
        "_urls",
        "_locales",
        "__git_branch",
        "__git_heads",
        "__git_tags",
        "__git_remotes",
        "__git_files",
        "__git_cached_files",
        "__git_modified_files",
        "__git_other_files",
        "__git_commit_tags",
    ];
    KNOWN_ACTIONS.iter().any(|a| s.contains(a))
}

/// Find the action (completion function) in a spec string.
/// The action is after the last `:` that's part of the argument description,
/// not inside brackets `[...]`.
fn find_action_in_spec(spec: &str) -> Option<String> {
    // Strategy: split on colons, but skip content inside []
    let mut parts: Vec<String> = Vec::new();
    let mut current = String::new();
    let mut bracket_depth: u32 = 0;

    for ch in spec.chars() {
        match ch {
            '[' => {
                bracket_depth += 1;
                current.push(ch);
            }
            ']' => {
                bracket_depth = bracket_depth.saturating_sub(1);
                current.push(ch);
            }
            ':' if bracket_depth == 0 => {
                parts.push(current.clone());
                current.clear();
            }
            _ => current.push(ch),
        }
    }
    parts.push(current);

    // The action is the last part
    let last = parts.last()?;
    let last = last.trim();
    if has_known_action(last) {
        Some(last.to_string())
    } else {
        None
    }
}

/// Extract flag names from a spec string.
fn extract_flags_from_spec(spec: &str) -> Vec<String> {
    let mut flags = Vec::new();
    let s = spec.trim();

    // Strip leading exclusion group: (...)
    let s = if s.starts_with('(') {
        if let Some(end) = s.find(')') {
            s[end + 1..].trim()
        } else {
            s
        }
    } else {
        s
    };

    // The flag is at the start, up to the first [ or :
    let flag_part: String = s.chars().take_while(|c| *c != '[' && *c != ':').collect();
    let flag_part = flag_part.trim();

    // Handle comma-separated alternatives inside braces: {-f,--flag}
    if flag_part.contains('{')
        && flag_part.contains('}')
        && let Some(start) = flag_part.find('{')
        && let Some(end) = flag_part.find('}')
    {
        let inner = &flag_part[start + 1..end];
        for part in inner.split(',') {
            let f = part.trim().trim_end_matches('+').trim_end_matches('=');
            if f.starts_with('-') {
                flags.push(f.to_string());
            }
        }
        return flags;
    }

    // Single flag: strip trailing + or =
    let flag = flag_part.trim_end_matches('+').trim_end_matches('=');
    if flag.starts_with('-') && !flag.is_empty() {
        flags.push(flag.to_string());
    }

    flags
}

/// Extract subcommands from a completion file's content.
/// Looks for patterns like `_git-checkout`, `_docker-build`, etc.
fn extract_subcommands_from_content(cmd: &str, content: &str) -> Vec<String> {
    let mut subs = Vec::new();
    let prefix = format!("_{}-", cmd);

    for line in content.lines() {
        // Pattern: (( $+functions[_cmd-subcmd] ))
        if let Some(start) = line.find(&prefix) {
            let after = &line[start + prefix.len()..];
            // Extract the subcmd name (alphanumeric, hyphens, underscores)
            let subcmd: String = after
                .chars()
                .take_while(|c| c.is_alphanumeric() || *c == '-' || *c == '_')
                .collect();
            if !subcmd.is_empty() && subcmd.len() < 40 {
                subs.push(subcmd);
            }
        }
    }

    subs.sort();
    subs.dedup();
    subs
}

fn is_internal_completion(name: &str) -> bool {
    matches!(
        name,
        "arguments"
            | "values"
            | "alternative"
            | "describe"
            | "all_labels"
            | "all_matches"
            | "approximate"
            | "cache_invalid"
            | "call_function"
            | "combination"
            | "command_names"
            | "complete"
            | "completion"
            | "configure"
            | "default"
            | "dispatch"
            | "equal"
            | "expand"
            | "extensions"
            | "file_descriptors"
            | "files"
            | "guard"
            | "have_glob_qual"
            | "history"
            | "ignored"
            | "list"
            | "main_complete"
            | "message"
            | "multi_parts"
            | "next_label"
            | "normal"
            | "oldlist"
            | "parameters"
            | "path_files"
            | "pick_variant"
            | "prefix"
            | "regex_arguments"
            | "regex_words"
            | "requested"
            | "retrieve_cache"
            | "sep_parts"
            | "sequence"
            | "set_command"
            | "setup"
            | "store_cache"
            | "style"
            | "sub_command"
            | "suffix"
            | "tags"
            | "user_expand"
            | "wanted"
    )
}

/// Simple glob that expands `*` in a single path component.
fn glob_simple(pattern: &str) -> Result<Vec<String>, std::io::Error> {
    let mut results = Vec::new();

    if let Some(star_pos) = pattern.find('*') {
        let parent = &pattern[..pattern[..star_pos].rfind('/').unwrap_or(0)];
        let suffix = &pattern[pattern[star_pos..]
            .find('/')
            .map(|p| star_pos + p)
            .unwrap_or(pattern.len())..];

        if let Ok(entries) = fs::read_dir(parent) {
            for entry in entries.flatten() {
                let path = entry.path();
                if path.is_dir() {
                    let candidate = format!("{}{}", path.display(), suffix);
                    if Path::new(&candidate).exists() {
                        results.push(candidate);
                    }
                }
            }
        }
    } else if Path::new(pattern).exists() {
        results.push(pattern.to_string());
    }

    Ok(results)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_extract_subcommands() {
        let content = r#"
(( $+functions[_git-add] )) ||
_git-add () {
  local curcontext=$curcontext state line ret=1
}

(( $+functions[_git-checkout] )) ||
_git-checkout () {
}

(( $+functions[_git-commit] )) ||
_git-commit () {
}
"#;
        let subs = extract_subcommands_from_content("git", content);
        assert_eq!(subs, vec!["add", "checkout", "commit"]);
    }

    #[test]
    fn test_extract_no_match() {
        let content = "some random content\n_arguments -S\n";
        let subs = extract_subcommands_from_content("foo", content);
        assert!(subs.is_empty());
    }

    #[test]
    fn test_parse_compdef_commands() {
        assert_eq!(
            parse_compdef_commands("#compdef cd chdir pushd\n"),
            vec!["cd", "chdir", "pushd"]
        );
        assert_eq!(
            parse_compdef_commands("#compdef rm grm zf_rm\n"),
            vec!["rm", "grm", "zf_rm"]
        );
        assert_eq!(
            parse_compdef_commands("# just a comment\nsome code\n"),
            Vec::<String>::new()
        );
    }

    #[test]
    fn test_parse_arg_spec_rest_files() {
        let content = "#compdef cat\n_arguments '*: :_files'\n";
        let spec = parse_arg_spec(content);
        assert_eq!(spec.rest, Some(trie::ARG_MODE_PATHS));
    }

    #[test]
    fn test_parse_arg_spec_rest_dirs() {
        let content = "#compdef rmdir\n_arguments '*: :_directories'\n";
        let spec = parse_arg_spec(content);
        assert_eq!(spec.rest, Some(trie::ARG_MODE_DIRS_ONLY));
    }

    #[test]
    fn test_parse_arg_spec_rest_execs() {
        let content = "#compdef which\n_arguments '*:command:_command_names'\n";
        let spec = parse_arg_spec(content);
        assert_eq!(spec.rest, Some(trie::ARG_MODE_EXECS_ONLY));
    }

    #[test]
    fn test_parse_arg_spec_flag_with_file() {
        // -t takes a directory argument, * takes files
        let content = r#"#compdef cp
_arguments \
  '-t+[target directory]:target directory:_files -/' \
  '*:file or directory:_files'
"#;
        let spec = parse_arg_spec(content);
        assert_eq!(spec.rest, Some(trie::ARG_MODE_PATHS));
        assert_eq!(
            spec.flag_args.get("-t"),
            Some(&trie::ARG_MODE_DIRS_ONLY),
            "flag -t should expect directories"
        );
    }

    #[test]
    fn test_parse_arg_spec_positional() {
        let content = r#"#compdef diff
_arguments '1:original file:_files' '2:new file:_files'
"#;
        let spec = parse_arg_spec(content);
        assert_eq!(spec.positional.get(&1), Some(&trie::ARG_MODE_PATHS));
        assert_eq!(spec.positional.get(&2), Some(&trie::ARG_MODE_PATHS));
    }

    #[test]
    fn test_parse_arg_spec_empty() {
        let content = "#compdef git\n_arguments -S\n";
        let spec = parse_arg_spec(content);
        assert!(spec.is_empty());
    }

    #[test]
    fn test_parse_arg_spec_gcc_output_flag() {
        let content = r#"#compdef gcc
_arguments \
  '-o+:output file:_files' \
  '*:input file:_files'
"#;
        let spec = parse_arg_spec(content);
        assert_eq!(spec.flag_args.get("-o"), Some(&trie::ARG_MODE_PATHS));
        assert_eq!(spec.rest, Some(trie::ARG_MODE_PATHS));
    }

    #[test]
    fn test_state_ref_rest_files() {
        let content = r#"
_arguments -C \
  '*:: :->file' && return

case $state in
  (file)
    _alternative \
      'files:file:_files' \
      'hosts:host:_ssh_hosts' && ret=0
    ;;
esac
"#;
        let spec = parse_arg_spec(content);
        assert_eq!(spec.rest, Some(trie::ARG_MODE_PATHS));
    }

    #[test]
    fn test_state_ref_rest_execs() {
        let content = r#"
_arguments -C \
  '*:: :->command' && return

case $state in
  (command)
    _command_names && ret=0
    ;;
esac
"#;
        let spec = parse_arg_spec(content);
        assert_eq!(spec.rest, Some(trie::ARG_MODE_EXECS_ONLY));
    }

    #[test]
    fn test_state_ref_flag() {
        let content = r#"
_arguments -C \
  '-o+[output]:output file:->outfile' \
  '*:input:_files' && return

case $state in
  (outfile)
    _files && ret=0
    ;;
esac
"#;
        let spec = parse_arg_spec(content);
        assert_eq!(spec.flag_args.get("-o"), Some(&trie::ARG_MODE_PATHS));
        assert_eq!(spec.rest, Some(trie::ARG_MODE_PATHS));
    }

    #[test]
    fn test_state_ref_positional() {
        let content = r#"
_arguments -C \
  '1:source:->src' \
  '2:dest:->dst' && return

case $state in
  (src)
    _files && ret=0
    ;;
  (dst)
    _directories && ret=0
    ;;
esac
"#;
        let spec = parse_arg_spec(content);
        assert_eq!(spec.positional.get(&1), Some(&trie::ARG_MODE_PATHS));
        assert_eq!(spec.positional.get(&2), Some(&trie::ARG_MODE_DIRS_ONLY));
    }

    #[test]
    fn test_nested_case_esac() {
        let content = r#"
case $state in
  (outer)
    case $line[1] in
      (sub)
        _files
        ;;
    esac
    ;;
  (other)
    _directories
    ;;
esac
"#;
        let types = extract_state_types(content);
        assert_eq!(types.get("outer"), Some(&trie::ARG_MODE_PATHS));
        assert_eq!(types.get("other"), Some(&trie::ARG_MODE_DIRS_ONLY));
    }

    #[test]
    fn test_alternative_fallback_detection() {
        let content = r#"
_alternative \
  'files:file:_files' \
  'urls:url:_urls' && ret=0
"#;
        let spec = parse_arg_spec(content);
        assert_eq!(spec.rest, Some(trie::ARG_MODE_PATHS));
    }

    #[test]
    fn test_regex_args_detection() {
        let content = r#"
_regex_arguments _mycommand \
  ':files:file:_files -S ""' \
  ':dirs:directory:_directories'
"#;
        let mode = detect_dominant_action(content);
        assert_eq!(mode, Some(trie::ARG_MODE_PATHS));
    }

    #[test]
    fn test_state_ref_with_lstate() {
        let content = r#"
_arguments -C \
  ':host:->userhost' \
  '*::args:->command' && ret=0

case "$lstate" in
  (userhost)
    _ssh_hosts && ret=0
    ;;
  (command)
    _command_names && ret=0
    ;;
esac
"#;
        let spec = parse_arg_spec(content);
        // ':host:->userhost' is positional 1 → HOSTS
        assert_eq!(spec.positional.get(&1), Some(&trie::ARG_MODE_HOSTS));
        // '*::args:->command' is rest → EXECS_ONLY
        assert_eq!(spec.rest, Some(trie::ARG_MODE_EXECS_ONLY));
    }

    #[test]
    fn test_alternative_with_dirs_only() {
        let content = r#"
case $state in
  (dest)
    _alternative \
      'directories:directory:_directories' && ret=0
    ;;
esac
"#;
        let types = extract_state_types(content);
        assert_eq!(types.get("dest"), Some(&trie::ARG_MODE_DIRS_ONLY));
    }

    #[test]
    fn test_has_known_action_recognizes_runtime_types() {
        assert!(has_known_action("_users"));
        assert!(has_known_action("_hosts"));
        assert!(has_known_action("_signals"));
        assert!(has_known_action("_ports"));
        assert!(has_known_action("_pids"));
        assert!(has_known_action("_groups"));
        assert!(has_known_action("_locales"));
        assert!(has_known_action("_net_interfaces"));
        assert!(has_known_action("_urls"));
        assert!(has_known_action("__git_branch_names"));
        assert!(has_known_action("__git_tags"));
        assert!(has_known_action("__git_remotes"));
        assert!(has_known_action("__git_files"));
        assert!(!has_known_action("_something_random"));
        assert!(!has_known_action("just text"));
    }

    #[test]
    fn test_parse_arg_spec_users_hosts() {
        let content = r#"#compdef ssh
_arguments \
  '1:host:_hosts' \
  '-l+:login name:_users'
"#;
        let spec = parse_arg_spec(content);
        assert_eq!(spec.positional.get(&1), Some(&trie::ARG_MODE_HOSTS));
        assert_eq!(spec.flag_args.get("-l"), Some(&trie::ARG_MODE_USERS));
    }

    #[test]
    fn test_parse_arg_spec_signals() {
        let content = r#"#compdef kill
_arguments \
  '-s+:signal:_signals' \
  '*:pid:_pids'
"#;
        let spec = parse_arg_spec(content);
        assert_eq!(spec.flag_args.get("-s"), Some(&trie::ARG_MODE_SIGNALS));
        assert_eq!(spec.rest, Some(trie::ARG_MODE_PIDS));
    }

    fn no_completions() -> HashSet<String> {
        HashSet::new()
    }

    #[test]
    fn test_well_known_specs_not_empty() {
        let mut specs = HashMap::new();
        apply_well_known_specs(&mut specs, &no_completions());
        // Should have added specs for git checkout, docker, kubectl, etc.
        assert!(specs.contains_key("git checkout"));
        assert!(specs.contains_key("git push"));
        assert!(specs.contains_key("docker run"));
        assert!(specs.contains_key("kubectl apply"));
        assert!(specs.contains_key("cargo build"));
        assert!(specs.contains_key("npm install"));
        assert!(specs.contains_key("curl"));
        assert!(specs.contains_key("ssh"));
        assert!(specs.contains_key("kill"));
    }

    #[test]
    fn test_well_known_specs_skipped_for_completion_commands() {
        let mut specs = HashMap::new();
        let mut covered = HashSet::new();
        covered.insert("git".to_string());
        covered.insert("ssh".to_string());
        covered.insert("kill".to_string());
        apply_well_known_specs(&mut specs, &covered);
        // These have completion files — overrides must not be applied
        assert!(!specs.contains_key("git checkout"));
        assert!(!specs.contains_key("git push"));
        assert!(!specs.contains_key("ssh"));
        assert!(!specs.contains_key("kill"));
        // These don't have completion files — overrides should still apply
        assert!(specs.contains_key("docker run"));
        assert!(specs.contains_key("curl"));
    }

    #[test]
    fn test_well_known_docker_specs() {
        let mut specs = HashMap::new();
        apply_well_known_specs(&mut specs, &no_completions());
        let docker_run = specs.get("docker run").expect("docker run should have specs");
        assert!(docker_run.flag_args.contains_key("-v"));
        assert!(docker_run.flag_args.contains_key("-u"));
    }

    #[test]
    fn test_well_known_curl_specs() {
        let mut specs = HashMap::new();
        apply_well_known_specs(&mut specs, &no_completions());
        let curl = specs.get("curl").expect("curl should have specs");
        assert_eq!(curl.positional.get(&1), Some(&trie::ARG_MODE_URLS));
        assert_eq!(curl.flag_args.get("-o"), Some(&trie::ARG_MODE_PATHS));
    }

    #[test]
    fn test_well_known_kill_specs() {
        let mut specs = HashMap::new();
        apply_well_known_specs(&mut specs, &no_completions());
        let kill = specs.get("kill").expect("kill should have specs");
        assert_eq!(kill.positional.get(&1), Some(&trie::ARG_MODE_PIDS));
        assert_eq!(kill.rest, Some(trie::ARG_MODE_PIDS));
        assert_eq!(kill.flag_args.get("-s"), Some(&trie::ARG_MODE_SIGNALS));
    }

    #[test]
    fn test_well_known_ssh_specs() {
        let mut specs = HashMap::new();
        apply_well_known_specs(&mut specs, &no_completions());
        let ssh = specs.get("ssh").expect("ssh should have specs");
        assert_eq!(ssh.positional.get(&1), Some(&trie::ARG_MODE_HOSTS));
        assert_eq!(ssh.flag_args.get("-i"), Some(&trie::ARG_MODE_PATHS));
        assert_eq!(ssh.flag_args.get("-l"), Some(&trie::ARG_MODE_USERS));
        assert_eq!(ssh.flag_args.get("-p"), Some(&trie::ARG_MODE_PORTS));
    }

    #[test]
    fn test_well_known_chown_specs() {
        let mut specs = HashMap::new();
        apply_well_known_specs(&mut specs, &no_completions());
        let chown = specs.get("chown").expect("chown should have specs");
        assert_eq!(chown.positional.get(&1), Some(&trie::ARG_MODE_USERS));
        assert_eq!(chown.rest, Some(trie::ARG_MODE_PATHS));
    }

    // --- Tests for extract_desc_entries ---

    #[test]
    fn test_extract_desc_entries_basic() {
        let mut result = HashMap::new();
        extract_desc_entries("add:'Add file contents to index'", "git", &mut result);
        let git = result.get("git").unwrap();
        assert_eq!(git.get("add").unwrap(), "Add file contents to index");
    }

    #[test]
    fn test_extract_desc_entries_multiple() {
        let mut result = HashMap::new();
        extract_desc_entries(
            "add:'Add file contents' commit:'Record changes'",
            "git",
            &mut result,
        );
        let git = result.get("git").unwrap();
        assert_eq!(git.len(), 2);
        assert_eq!(git.get("add").unwrap(), "Add file contents");
        assert_eq!(git.get("commit").unwrap(), "Record changes");
    }

    #[test]
    fn test_extract_desc_entries_hyphenated_command() {
        let mut result = HashMap::new();
        extract_desc_entries("cherry-pick:'Apply changes'", "git", &mut result);
        let git = result.get("git").unwrap();
        assert_eq!(git.get("cherry-pick").unwrap(), "Apply changes");
    }

    #[test]
    fn test_extract_desc_entries_empty_desc_skipped() {
        let mut result = HashMap::new();
        extract_desc_entries("add:''", "git", &mut result);
        // Empty description should be skipped
        assert!(result.get("git").is_none());
    }

    #[test]
    fn test_extract_desc_entries_no_colon_quote() {
        let mut result = HashMap::new();
        extract_desc_entries("just a plain word", "git", &mut result);
        assert!(result.is_empty());
    }

    // --- Tests for extract_descriptions_from_content ---

    #[test]
    fn test_extract_descriptions_basic_array() {
        let content = r#"
commands=(
  add:'Add file contents to index'
  commit:'Record changes to repository'
)
"#;
        let result = extract_descriptions_from_content(content, "git");
        let git = result.get("git").unwrap();
        assert_eq!(git.len(), 2);
        assert_eq!(git.get("add").unwrap(), "Add file contents to index");
        assert_eq!(git.get("commit").unwrap(), "Record changes to repository");
    }

    #[test]
    fn test_extract_descriptions_hyphenated_cmd() {
        // _git-stash should derive parent "git stash"
        let content = r#"
subcmds=(
  apply:'Apply a stash'
  pop:'Pop a stash'
)
"#;
        let result = extract_descriptions_from_content(content, "git-stash");
        let parent = result.get("git stash").unwrap();
        assert_eq!(parent.len(), 2);
        assert_eq!(parent.get("apply").unwrap(), "Apply a stash");
    }

    #[test]
    fn test_extract_descriptions_inline_with_paren() {
        // Entries on the same line as =(
        let content = "commands=(add:'Add files' commit:'Record changes')\n";
        let result = extract_descriptions_from_content(content, "git");
        let git = result.get("git").unwrap();
        assert_eq!(git.len(), 2);
    }

    #[test]
    fn test_extract_descriptions_entries_before_close() {
        // Entries on the same line as closing )
        let content = "commands=(\n  add:'Add files'\n  commit:'Record changes')\n";
        let result = extract_descriptions_from_content(content, "git");
        let git = result.get("git").unwrap();
        assert_eq!(git.len(), 2);
    }

    #[test]
    fn test_extract_descriptions_no_arrays() {
        let content = "just some random code\nno arrays here\n";
        let result = extract_descriptions_from_content(content, "foo");
        assert!(result.is_empty());
    }

    #[test]
    fn test_extract_descriptions_comment_line_skipped() {
        let content = "# commands=(\nreal_var=(\n  add:'Add files'\n)\n";
        let result = extract_descriptions_from_content(content, "git");
        let git = result.get("git").unwrap();
        assert_eq!(git.len(), 1);
    }

    // --- Tests for extract_case_arm_name ---

    #[test]
    fn test_extract_case_arm_name_parenthesized() {
        assert_eq!(extract_case_arm_name("  (files)"), Some("files".into()));
    }

    #[test]
    fn test_extract_case_arm_name_bare() {
        assert_eq!(extract_case_arm_name("  files)"), Some("files".into()));
    }

    #[test]
    fn test_extract_case_arm_name_with_hyphens() {
        assert_eq!(
            extract_case_arm_name("  (remote-tracking)"),
            Some("remote-tracking".into())
        );
    }

    #[test]
    fn test_extract_case_arm_name_wildcard_rejected() {
        assert_eq!(extract_case_arm_name("  (*)"), None);
    }

    #[test]
    fn test_extract_case_arm_name_or_pattern_rejected() {
        assert_eq!(extract_case_arm_name("  (a|b)"), None);
    }

    #[test]
    fn test_extract_case_arm_name_space_rejected() {
        assert_eq!(extract_case_arm_name("  (a b)"), None);
    }

    #[test]
    fn test_extract_case_arm_name_no_paren() {
        assert_eq!(extract_case_arm_name("  files"), None);
    }

    #[test]
    fn test_extract_case_arm_name_empty_inner() {
        assert_eq!(extract_case_arm_name("  ()"), None);
    }

    // --- Tests for detect_type_in_block ---

    #[test]
    fn test_detect_type_files() {
        assert_eq!(
            detect_type_in_block("  _files\n"),
            Some(trie::ARG_MODE_PATHS)
        );
    }

    #[test]
    fn test_detect_type_directories() {
        assert_eq!(
            detect_type_in_block("  _directories\n"),
            Some(trie::ARG_MODE_DIRS_ONLY)
        );
    }

    #[test]
    fn test_detect_type_dirs_slash() {
        assert_eq!(
            detect_type_in_block("  _path_files -/\n"),
            Some(trie::ARG_MODE_DIRS_ONLY)
        );
        assert_eq!(
            detect_type_in_block("  _files -/\n"),
            Some(trie::ARG_MODE_DIRS_ONLY)
        );
    }

    #[test]
    fn test_detect_type_execs() {
        assert_eq!(
            detect_type_in_block("  _command_names\n"),
            Some(trie::ARG_MODE_EXECS_ONLY)
        );
        assert_eq!(
            detect_type_in_block("  _path_commands\n"),
            Some(trie::ARG_MODE_EXECS_ONLY)
        );
    }

    #[test]
    fn test_detect_type_files_and_dirs() {
        // When both files and dirs present → Paths
        assert_eq!(
            detect_type_in_block("  _files\n  _directories\n"),
            Some(trie::ARG_MODE_PATHS)
        );
    }

    #[test]
    fn test_detect_type_comment_ignored() {
        assert_eq!(
            detect_type_in_block("  # _files\n"),
            None
        );
    }

    #[test]
    fn test_detect_type_empty() {
        assert_eq!(detect_type_in_block(""), None);
    }

    // --- Tests for is_internal_completion ---

    #[test]
    fn test_is_internal_completion() {
        assert!(is_internal_completion("arguments"));
        assert!(is_internal_completion("values"));
        assert!(is_internal_completion("files"));
        assert!(is_internal_completion("path_files"));
        assert!(is_internal_completion("command_names"));
        assert!(is_internal_completion("regex_arguments"));
        assert!(!is_internal_completion("git"));
        assert!(!is_internal_completion("docker"));
        assert!(!is_internal_completion("ssh"));
    }

    #[test]
    fn test_make_flag_f_from_completion_file() {
        let path = "/usr/share/zsh/5.9/functions/_make";
        let Ok(content) = std::fs::read_to_string(path) else {
            return;
        };
        let spec = parse_arg_spec(&content);
        assert_eq!(
            spec.flag_args.get("-f"),
            Some(&trie::ARG_MODE_PATHS),
            "make -f should be PATHS, got {:?}",
            spec.flag_args.get("-f")
        );
    }

    #[test]
    fn test_sudo_flag_u_from_completion_file() {
        let path = "/usr/share/zsh/5.9/functions/_sudo";
        let Ok(content) = std::fs::read_to_string(path) else {
            return;
        };
        let spec = parse_arg_spec(&content);
        assert_eq!(
            spec.flag_args.get("-u"),
            Some(&trie::ARG_MODE_USERS),
            "sudo -u should be USERS, got {:?}",
            spec.flag_args.get("-u")
        );
    }

    #[test]
    fn test_git_checkout_spec_from_completion_file() {
        let path = "/usr/share/zsh/5.9/functions/_git";
        let Ok(content) = std::fs::read_to_string(path) else {
            return; // skip if file not present
        };
        let sub_specs = extract_subcommand_arg_specs("git", &content);
        let spec = sub_specs.get("git checkout").expect("git checkout spec missing");
        // The rest arg should resolve as branches (commits), not files
        assert_eq!(
            spec.rest,
            Some(trie::ARG_MODE_GIT_BRANCHES),
            "git checkout rest should be GIT_BRANCHES, got {:?}",
            spec.rest
        );
    }
}
