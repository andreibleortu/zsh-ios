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
        serde_yaml_ng::from_str(yaml_str).unwrap_or_default();

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
/// ```zsh
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
        ("git mv", &[(1, ARG_MODE_GIT_FILES), (2, ARG_MODE_PATHS)], None, &[]),
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
        // --- ip (Linux iproute2 — uses _regex_arguments, subcommands are static) ---
        ("ip", &[], None, &[]),
        ("ip addr", &[], None, &[]),
        ("ip link", &[], None, &[]),
        ("ip route", &[], None, &[]),
        ("ip neigh", &[], None, &[]),
        ("ip rule", &[], None, &[]),
        ("ip monitor", &[], None, &[]),
        ("ip netns", &[], None, &[]),

        // --- Docker (rich arg types) ---
        ("docker run", &[(1, ARG_MODE_DOCKER_IMAGE)], None, &[
            ("--network", ARG_MODE_DOCKER_NETWORK),
            ("--volumes-from", ARG_MODE_DOCKER_CONTAINER),
        ]),
        ("docker exec", &[(1, ARG_MODE_DOCKER_CONTAINER)], None, &[]),
        ("docker start", &[(1, ARG_MODE_DOCKER_CONTAINER)], Some(ARG_MODE_DOCKER_CONTAINER), &[]),
        ("docker stop", &[(1, ARG_MODE_DOCKER_CONTAINER)], Some(ARG_MODE_DOCKER_CONTAINER), &[]),
        ("docker restart", &[(1, ARG_MODE_DOCKER_CONTAINER)], Some(ARG_MODE_DOCKER_CONTAINER), &[]),
        ("docker kill", &[(1, ARG_MODE_DOCKER_CONTAINER)], Some(ARG_MODE_DOCKER_CONTAINER), &[]),
        ("docker rm", &[(1, ARG_MODE_DOCKER_CONTAINER)], Some(ARG_MODE_DOCKER_CONTAINER), &[]),
        ("docker logs", &[(1, ARG_MODE_DOCKER_CONTAINER)], None, &[]),
        ("docker inspect", &[(1, ARG_MODE_DOCKER_CONTAINER)], Some(ARG_MODE_DOCKER_CONTAINER), &[]),
        ("docker attach", &[(1, ARG_MODE_DOCKER_CONTAINER)], None, &[]),
        ("docker top", &[(1, ARG_MODE_DOCKER_CONTAINER)], None, &[]),
        ("docker pause", &[(1, ARG_MODE_DOCKER_CONTAINER)], Some(ARG_MODE_DOCKER_CONTAINER), &[]),
        ("docker unpause", &[(1, ARG_MODE_DOCKER_CONTAINER)], Some(ARG_MODE_DOCKER_CONTAINER), &[]),
        ("docker rmi", &[(1, ARG_MODE_DOCKER_IMAGE)], Some(ARG_MODE_DOCKER_IMAGE), &[]),
        ("docker pull", &[(1, ARG_MODE_DOCKER_IMAGE)], None, &[]),
        ("docker push", &[(1, ARG_MODE_DOCKER_IMAGE)], None, &[]),
        ("docker tag", &[(1, ARG_MODE_DOCKER_IMAGE), (2, ARG_MODE_DOCKER_IMAGE)], None, &[]),
        ("docker network inspect", &[(1, ARG_MODE_DOCKER_NETWORK)], Some(ARG_MODE_DOCKER_NETWORK), &[]),
        ("docker network rm", &[(1, ARG_MODE_DOCKER_NETWORK)], Some(ARG_MODE_DOCKER_NETWORK), &[]),
        ("docker network connect", &[(1, ARG_MODE_DOCKER_NETWORK), (2, ARG_MODE_DOCKER_CONTAINER)], None, &[]),
        ("docker network disconnect", &[(1, ARG_MODE_DOCKER_NETWORK), (2, ARG_MODE_DOCKER_CONTAINER)], None, &[]),
        ("docker volume rm", &[(1, ARG_MODE_DOCKER_VOLUME)], Some(ARG_MODE_DOCKER_VOLUME), &[]),
        ("docker volume inspect", &[(1, ARG_MODE_DOCKER_VOLUME)], Some(ARG_MODE_DOCKER_VOLUME), &[]),
        ("docker compose up", &[], Some(ARG_MODE_DOCKER_COMPOSE_SERVICE), &[]),
        ("docker compose down", &[], None, &[]),
        ("docker compose logs", &[], Some(ARG_MODE_DOCKER_COMPOSE_SERVICE), &[]),
        ("docker compose exec", &[(1, ARG_MODE_DOCKER_COMPOSE_SERVICE)], None, &[]),
        ("docker compose run", &[(1, ARG_MODE_DOCKER_COMPOSE_SERVICE)], None, &[]),
        ("docker compose restart", &[], Some(ARG_MODE_DOCKER_COMPOSE_SERVICE), &[]),
        ("docker compose start", &[], Some(ARG_MODE_DOCKER_COMPOSE_SERVICE), &[]),
        ("docker compose stop", &[], Some(ARG_MODE_DOCKER_COMPOSE_SERVICE), &[]),

        // --- Kubernetes (kubectl) with rich arg types ---
        ("kubectl config use-context", &[(1, ARG_MODE_K8S_CONTEXT)], None, &[]),
        ("kubectl config delete-context", &[(1, ARG_MODE_K8S_CONTEXT)], None, &[]),
        ("kubectl config rename-context", &[(1, ARG_MODE_K8S_CONTEXT), (2, ARG_MODE_K8S_CONTEXT)], None, &[]),
        ("kubectl config set-context", &[(1, ARG_MODE_K8S_CONTEXT)], None, &[]),
        // kind-then-name forms
        ("kubectl get pod", &[], Some(ARG_MODE_K8S_POD), &[("-n", ARG_MODE_K8S_NAMESPACE), ("--namespace", ARG_MODE_K8S_NAMESPACE)]),
        ("kubectl get pods", &[], Some(ARG_MODE_K8S_POD), &[("-n", ARG_MODE_K8S_NAMESPACE), ("--namespace", ARG_MODE_K8S_NAMESPACE)]),
        ("kubectl get deployment", &[], Some(ARG_MODE_K8S_DEPLOYMENT), &[("-n", ARG_MODE_K8S_NAMESPACE)]),
        ("kubectl get deployments", &[], Some(ARG_MODE_K8S_DEPLOYMENT), &[("-n", ARG_MODE_K8S_NAMESPACE)]),
        ("kubectl get service", &[], Some(ARG_MODE_K8S_SERVICE), &[("-n", ARG_MODE_K8S_NAMESPACE)]),
        ("kubectl get services", &[], Some(ARG_MODE_K8S_SERVICE), &[("-n", ARG_MODE_K8S_NAMESPACE)]),
        ("kubectl get namespace", &[], Some(ARG_MODE_K8S_NAMESPACE), &[]),
        ("kubectl get namespaces", &[], Some(ARG_MODE_K8S_NAMESPACE), &[]),
        ("kubectl describe pod", &[(1, ARG_MODE_K8S_POD)], None, &[("-n", ARG_MODE_K8S_NAMESPACE)]),
        ("kubectl describe deployment", &[(1, ARG_MODE_K8S_DEPLOYMENT)], None, &[("-n", ARG_MODE_K8S_NAMESPACE)]),
        ("kubectl describe service", &[(1, ARG_MODE_K8S_SERVICE)], None, &[("-n", ARG_MODE_K8S_NAMESPACE)]),
        ("kubectl describe namespace", &[(1, ARG_MODE_K8S_NAMESPACE)], None, &[]),
        ("kubectl delete pod", &[(1, ARG_MODE_K8S_POD)], Some(ARG_MODE_K8S_POD), &[("-n", ARG_MODE_K8S_NAMESPACE)]),
        ("kubectl delete deployment", &[(1, ARG_MODE_K8S_DEPLOYMENT)], Some(ARG_MODE_K8S_DEPLOYMENT), &[("-n", ARG_MODE_K8S_NAMESPACE)]),
        ("kubectl delete service", &[(1, ARG_MODE_K8S_SERVICE)], Some(ARG_MODE_K8S_SERVICE), &[("-n", ARG_MODE_K8S_NAMESPACE)]),
        ("kubectl delete namespace", &[(1, ARG_MODE_K8S_NAMESPACE)], None, &[]),
        ("kubectl logs", &[(1, ARG_MODE_K8S_POD)], None, &[("-n", ARG_MODE_K8S_NAMESPACE)]),
        ("kubectl exec", &[(1, ARG_MODE_K8S_POD)], None, &[("-n", ARG_MODE_K8S_NAMESPACE)]),
        ("kubectl port-forward", &[(1, ARG_MODE_K8S_POD)], Some(ARG_MODE_PORTS), &[("-n", ARG_MODE_K8S_NAMESPACE)]),

        // --- systemctl with SYSTEMD_UNIT ---
        ("systemctl start", &[(1, ARG_MODE_SYSTEMD_UNIT)], Some(ARG_MODE_SYSTEMD_UNIT), &[]),
        ("systemctl stop", &[(1, ARG_MODE_SYSTEMD_UNIT)], Some(ARG_MODE_SYSTEMD_UNIT), &[]),
        ("systemctl restart", &[(1, ARG_MODE_SYSTEMD_UNIT)], Some(ARG_MODE_SYSTEMD_UNIT), &[]),
        ("systemctl reload", &[(1, ARG_MODE_SYSTEMD_UNIT)], Some(ARG_MODE_SYSTEMD_UNIT), &[]),
        ("systemctl status", &[(1, ARG_MODE_SYSTEMD_UNIT)], Some(ARG_MODE_SYSTEMD_UNIT), &[]),
        ("systemctl enable", &[(1, ARG_MODE_SYSTEMD_UNIT)], Some(ARG_MODE_SYSTEMD_UNIT), &[]),
        ("systemctl disable", &[(1, ARG_MODE_SYSTEMD_UNIT)], Some(ARG_MODE_SYSTEMD_UNIT), &[]),
        ("systemctl mask", &[(1, ARG_MODE_SYSTEMD_UNIT)], Some(ARG_MODE_SYSTEMD_UNIT), &[]),
        ("systemctl unmask", &[(1, ARG_MODE_SYSTEMD_UNIT)], Some(ARG_MODE_SYSTEMD_UNIT), &[]),
        ("systemctl cat", &[(1, ARG_MODE_SYSTEMD_UNIT)], Some(ARG_MODE_SYSTEMD_UNIT), &[]),
        ("systemctl edit", &[(1, ARG_MODE_SYSTEMD_UNIT)], Some(ARG_MODE_SYSTEMD_UNIT), &[]),

        // --- Homebrew with BREW_FORMULA / BREW_CASK ---
        ("brew install", &[(1, ARG_MODE_BREW_FORMULA)], Some(ARG_MODE_BREW_FORMULA), &[]),
        ("brew uninstall", &[(1, ARG_MODE_BREW_FORMULA)], Some(ARG_MODE_BREW_FORMULA), &[]),
        ("brew upgrade", &[(1, ARG_MODE_BREW_FORMULA)], Some(ARG_MODE_BREW_FORMULA), &[]),
        ("brew info", &[(1, ARG_MODE_BREW_FORMULA)], Some(ARG_MODE_BREW_FORMULA), &[]),
        ("brew remove", &[(1, ARG_MODE_BREW_FORMULA)], Some(ARG_MODE_BREW_FORMULA), &[]),
        ("brew reinstall", &[(1, ARG_MODE_BREW_FORMULA)], Some(ARG_MODE_BREW_FORMULA), &[]),
        ("brew pin", &[(1, ARG_MODE_BREW_FORMULA)], Some(ARG_MODE_BREW_FORMULA), &[]),
        ("brew unpin", &[(1, ARG_MODE_BREW_FORMULA)], Some(ARG_MODE_BREW_FORMULA), &[]),

        // --- Package managers with rich types ---
        ("apt install", &[(1, ARG_MODE_APT_PACKAGE)], Some(ARG_MODE_APT_PACKAGE), &[]),
        ("apt remove", &[(1, ARG_MODE_APT_PACKAGE)], Some(ARG_MODE_APT_PACKAGE), &[]),
        ("apt purge", &[(1, ARG_MODE_APT_PACKAGE)], Some(ARG_MODE_APT_PACKAGE), &[]),
        ("apt show", &[(1, ARG_MODE_APT_PACKAGE)], Some(ARG_MODE_APT_PACKAGE), &[]),
        ("apt search", &[(1, ARG_MODE_APT_PACKAGE)], None, &[]),
        ("dnf install", &[(1, ARG_MODE_DNF_PACKAGE)], Some(ARG_MODE_DNF_PACKAGE), &[]),
        ("dnf remove", &[(1, ARG_MODE_DNF_PACKAGE)], Some(ARG_MODE_DNF_PACKAGE), &[]),
        ("dnf info", &[(1, ARG_MODE_DNF_PACKAGE)], Some(ARG_MODE_DNF_PACKAGE), &[]),
        ("pacman -S", &[(1, ARG_MODE_PACMAN_PACKAGE)], Some(ARG_MODE_PACMAN_PACKAGE), &[]),
        ("pacman -R", &[(1, ARG_MODE_PACMAN_PACKAGE)], Some(ARG_MODE_PACMAN_PACKAGE), &[]),

        // --- tmux / screen ---
        ("tmux attach", &[], None, &[("-t", ARG_MODE_TMUX_SESSION)]),
        ("tmux attach-session", &[], None, &[("-t", ARG_MODE_TMUX_SESSION)]),
        ("tmux kill-session", &[], None, &[("-t", ARG_MODE_TMUX_SESSION)]),
        ("tmux switch-client", &[], None, &[("-t", ARG_MODE_TMUX_SESSION)]),
        ("tmux has-session", &[], None, &[("-t", ARG_MODE_TMUX_SESSION)]),
        ("screen -r", &[(1, ARG_MODE_SCREEN_SESSION)], None, &[]),
        ("screen -x", &[(1, ARG_MODE_SCREEN_SESSION)], None, &[]),

        // --- npm / yarn / pnpm with project-scoped types ---
        ("npm install", &[(1, ARG_MODE_NPM_PACKAGE)], Some(ARG_MODE_NPM_PACKAGE), &[]),
        ("npm uninstall", &[(1, ARG_MODE_NPM_PACKAGE)], Some(ARG_MODE_NPM_PACKAGE), &[]),
        ("npm run", &[(1, ARG_MODE_NPM_SCRIPT)], None, &[]),
        ("npm run-script", &[(1, ARG_MODE_NPM_SCRIPT)], None, &[]),
        ("yarn add", &[(1, ARG_MODE_NPM_PACKAGE)], Some(ARG_MODE_NPM_PACKAGE), &[]),
        ("yarn remove", &[(1, ARG_MODE_NPM_PACKAGE)], Some(ARG_MODE_NPM_PACKAGE), &[]),
        ("yarn run", &[(1, ARG_MODE_NPM_SCRIPT)], None, &[]),
        ("pnpm add", &[(1, ARG_MODE_NPM_PACKAGE)], Some(ARG_MODE_NPM_PACKAGE), &[]),
        ("pnpm remove", &[(1, ARG_MODE_NPM_PACKAGE)], Some(ARG_MODE_NPM_PACKAGE), &[]),
        ("pnpm run", &[(1, ARG_MODE_NPM_SCRIPT)], None, &[]),

        // --- pip ---
        ("pip install", &[(1, ARG_MODE_PIP_PACKAGE)], Some(ARG_MODE_PIP_PACKAGE), &[("-r", ARG_MODE_PATHS)]),
        ("pip uninstall", &[(1, ARG_MODE_PIP_PACKAGE)], Some(ARG_MODE_PIP_PACKAGE), &[]),
        ("pip show", &[(1, ARG_MODE_PIP_PACKAGE)], Some(ARG_MODE_PIP_PACKAGE), &[]),
        ("pip3 install", &[(1, ARG_MODE_PIP_PACKAGE)], Some(ARG_MODE_PIP_PACKAGE), &[("-r", ARG_MODE_PATHS)]),
        ("pip3 uninstall", &[(1, ARG_MODE_PIP_PACKAGE)], Some(ARG_MODE_PIP_PACKAGE), &[]),

        // --- make / just ---
        ("make", &[], Some(ARG_MODE_MAKE_TARGET), &[("-f", ARG_MODE_PATHS), ("-C", ARG_MODE_DIRS_ONLY)]),
        ("just", &[], Some(ARG_MODE_JUST_RECIPE), &[("-f", ARG_MODE_PATHS)]),

        // --- git-advanced positional slots ---
        ("git stash show", &[(1, ARG_MODE_GIT_STASH)], None, &[]),
        ("git stash apply", &[(1, ARG_MODE_GIT_STASH)], None, &[]),
        ("git stash pop", &[(1, ARG_MODE_GIT_STASH)], None, &[]),
        ("git stash drop", &[(1, ARG_MODE_GIT_STASH)], None, &[]),
        ("git worktree remove", &[(1, ARG_MODE_GIT_WORKTREE)], None, &[]),
        ("git worktree move", &[(1, ARG_MODE_GIT_WORKTREE), (2, ARG_MODE_DIRS_ONLY)], None, &[]),
        ("git cherry-pick", &[(1, ARG_MODE_GIT_COMMIT)], Some(ARG_MODE_GIT_COMMIT), &[]),
        ("git revert", &[(1, ARG_MODE_GIT_COMMIT)], Some(ARG_MODE_GIT_COMMIT), &[]),
        ("git show", &[(1, ARG_MODE_GIT_COMMIT)], None, &[]),
        ("git bisect good", &[(1, ARG_MODE_GIT_COMMIT)], None, &[]),
        ("git bisect bad", &[(1, ARG_MODE_GIT_COMMIT)], None, &[]),
    ];

    // Commands whose rest/positional completions come from an external program.
    // These mirror `_call_program` in Zsh completion files but bypass the
    // `_regex_arguments` grammar that prevents static extraction.
    // Each entry: (command, tag, argv for call_program).
    // Used for package managers, `ip` object-type completions, etc.
    type CallProg<'a> = (&'a str, &'a str, &'a [&'a str]);
    let call_prog_rest: &[CallProg] = &[
        // Debian/Ubuntu package managers — apt-cache pkgnames is fast and prefix-aware
        ("apt install",    "package", &["apt-cache", "pkgnames"]),
        ("apt remove",     "package", &["apt-cache", "pkgnames"]),
        ("apt purge",      "package", &["apt-cache", "pkgnames"]),
        ("apt reinstall",  "package", &["apt-cache", "pkgnames"]),
        ("apt show",       "package", &["apt-cache", "pkgnames"]),
        ("apt-get install","package", &["apt-cache", "pkgnames"]),
        ("apt-get remove", "package", &["apt-cache", "pkgnames"]),
        ("apt-get purge",  "package", &["apt-cache", "pkgnames"]),
        ("aptitude install","package",&["apt-cache", "pkgnames"]),
        ("aptitude remove", "package",&["apt-cache", "pkgnames"]),
        // RPM-based — dnf repoquery is equivalent to apt-cache pkgnames
        ("dnf install",    "package", &["dnf", "repoquery", "--available", "--qf", "%{name}"]),
        ("dnf remove",     "package", &["rpm", "-qa", "--qf", "%{name}\n"]),
        // Arch pacman — packaged in core repos
        ("pacman -S",      "package", &["pacman", "-Ssq"]),
        ("pacman -R",      "package", &["pacman", "-Qq"]),
    ];

    // Commands with static-list rest completions (object types for `ip`, etc.)
    type StaticRest<'a> = (&'a str, &'a [&'a str]);
    let static_list_rest: &[StaticRest] = &[
        ("ip",         &["addr", "link", "route", "neigh", "rule", "monitor", "netns", "maddr",
                         "mroute", "mrule", "tunnel", "tuntap", "l2tp", "fou", "xfrm",
                         "vrf", "sr", "nexthop", "macsec"]),
        ("ip addr",    &["add", "del", "show", "flush", "save", "restore"]),
        ("ip link",    &["add", "set", "show", "delete", "up", "down"]),
        ("ip route",   &["add", "del", "show", "flush", "get", "save", "restore"]),
        ("ip neigh",   &["add", "del", "show", "flush"]),
        ("ip rule",    &["add", "del", "list", "flush"]),
        ("ip netns",   &["add", "del", "list", "exec", "identify", "pids", "monitor"]),
    ];

    for &(cmd, positional, rest, flags) in overrides {
        let base_cmd = cmd.split_whitespace().next().unwrap_or(cmd);
        let has_completions = cmds_with_completions.contains(base_cmd);

        // For commands with a Zsh completion file, the parser handles positional
        // and rest specs. Flag specs are still applied as a supplement because
        // some completion functions put branch/file specs inside runtime
        // variables rather than literal _arguments specs. For example, in
        // `_git-branch`, `-d`/`-D` are defined as boolean flags (no arg spec),
        // and the subsequent branch names live in `$dependent_deletion_args`
        // populated by `if (( words[(I)-d] ))` — a variable the parser never
        // resolves. The parser CAN extract `-b → branches` from `_git-checkout`
        // because that spec is a literal string inside `_arguments`.
        // Positional overrides are gap-fill for all commands (with or without
        // completions). The parser stores bare `:desc:action` positionals as
        // `spec.rest` rather than numbered slots, so per-position well-known
        // specs are needed to fill in what the parser can't distinguish.
        {
            let spec = specs.entry(cmd.to_string()).or_default();
            for &(pos, arg_type) in positional {
                let existing = spec.positional.get(&pos).copied();
                if existing.is_none() || existing == Some(ARG_MODE_PATHS) || existing == Some(0) {
                    spec.positional.insert(pos, arg_type);
                }
            }
            // Rest override only for commands without a completion file —
            // the parser handles rest for commands that have one.
            if !has_completions
                && let Some(r) = rest
                    && (spec.rest.is_none() || spec.rest == Some(ARG_MODE_PATHS))
                {
                    spec.rest = Some(r);
                }
        }

        // Flag specs are always applied (gap-fill only — won't overwrite a
        // type already detected by the parser).
        if !flags.is_empty() {
            let spec = specs.entry(cmd.to_string()).or_default();
            for &(flag, arg_type) in flags {
                let existing = spec.flag_args.get(flag).copied();
                if existing.is_none() || existing == Some(ARG_MODE_PATHS) || existing == Some(0) {
                    spec.flag_args.insert(flag.to_string(), arg_type);
                }
            }
        }
    }

    // Apply _call_program rest completions (gap-fill only).
    for &(cmd, tag, argv_refs) in call_prog_rest {
        let spec = specs.entry(cmd.to_string()).or_default();
        if spec.rest_call_program.is_none() && spec.rest_static_list.is_none() {
            let argv: Vec<String> = argv_refs.iter().map(|s| s.to_string()).collect();
            spec.rest_call_program = Some((tag.to_string(), argv));
        }
    }

    // Apply static-list rest completions (gap-fill only).
    for &(cmd, items_refs) in static_list_rest {
        let spec = specs.entry(cmd.to_string()).or_default();
        if spec.rest_static_list.is_none() && spec.rest_call_program.is_none() {
            let items: Vec<String> = items_refs.iter().map(|s| s.to_string()).collect();
            spec.rest_static_list = Some(items);
        }
    }
}

fn completion_dirs() -> Vec<String> {
    let mut dirs = Vec::new();
    let add = |d: String, out: &mut Vec<String>| {
        if !d.is_empty() && !out.contains(&d) {
            out.push(d);
        }
    };

    // Standard Zsh completion directories
    for pattern in &[
        "/usr/share/zsh/*/functions",
        "/usr/local/share/zsh/site-functions",
        "/opt/homebrew/share/zsh/site-functions",
    ] {
        if let Ok(entries) = glob_simple(pattern) {
            for e in entries {
                add(e, &mut dirs);
            }
        }
    }

    // Plugin framework trees — each is a glob of directories that tend to
    // contain `_cmd` completion functions.  We don't care which framework
    // the user actually has; we try every known location and skip ones
    // that don't exist.
    let home = dirs::home_dir();
    if let Some(h) = home.as_ref() {
        let h = h.to_string_lossy().into_owned();
        for pattern in &[
            // Oh-My-Zsh
            "{h}/.oh-my-zsh/plugins/*",
            "{h}/.oh-my-zsh/custom/plugins/*",
            "{h}/.oh-my-zsh/completions",
            // Prezto
            "{h}/.zprezto/modules/*/functions",
            // zinit
            "{h}/.local/share/zinit/plugins/*",
            "{h}/.local/share/zinit/completions",
            "{h}/.zinit/plugins/*",
            "{h}/.zinit/completions",
            // antidote
            "{h}/.cache/antidote/*",
            "{h}/.antidote/*",
            // antibody
            "{h}/.cache/antibody/*",
            // znap
            "{h}/Git/*/plugins/*",
            "{h}/.znap/*",
            // zplug
            "{h}/.zplug/repos/*",
            // Misc XDG locations
            "{h}/.config/zsh/plugins/*",
            "{h}/.config/zsh/completions",
        ] {
            let expanded = pattern.replace("{h}", &h);
            if let Ok(entries) = glob_simple(&expanded) {
                for e in entries {
                    add(e, &mut dirs);
                }
            }
            // Also include the path verbatim for non-glob literals so a
            // pattern like `~/.oh-my-zsh/completions` is added even if
            // glob_simple expects a * somewhere.
            if !expanded.contains('*') && std::path::Path::new(&expanded).is_dir() {
                add(expanded, &mut dirs);
            }
        }
    }

    // Also check $fpath from environment if available
    if let Ok(fpath) = std::env::var("FPATH") {
        for dir in fpath.split(':') {
            add(dir.to_string(), &mut dirs);
        }
    }

    dirs
}

/// Result of scanning completion directories: (subcommands-per-command,
/// arg-spec-per-command, commands-that-had-a-completion-file).
type ExtractedCompletions = (
    HashMap<String, Vec<String>>,
    HashMap<String, ArgSpec>,
    HashSet<String>,
);

/// Extract subcommands and per-position argument specs from completion files.
fn extract_from_dirs(dirs: &[String]) -> ExtractedCompletions {
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
                let commands = parse_compdef_commands(&content);

                // Only add `cmd` (filename-derived) to the covered set if the
                // `#compdef` header either doesn't list anything (implicit
                // coverage) or explicitly includes `cmd`.  If `#compdef` names
                // other commands but NOT `cmd` (e.g. `_go` covers `gccgo gofmt`
                // but not the modern `go` CLI), `cmd` must NOT block its
                // hardcoded well-known spec.
                if commands.is_empty() || commands.iter().any(|c| c == cmd) {
                    cmds_with_completions.insert(cmd.to_string());
                }
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
                // Drop flags (-P, -K, etc.) and glob/regex patterns ([, *, #, ?)
                .filter(|w| {
                    !w.starts_with('-')
                        && !w.contains(['[', ']', '*', '#', '?', '(', ')'])
                })
                // Strip `alias=realcmd` forms; keep only the real command name
                .map(|w| w.split('=').next().unwrap_or(w).to_string())
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
        || action.contains("__git_remote_branch_names")
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
    // Commits, tree-ishs, revisions — resolve as branches (closest approximation).
    if action.contains("__git_commits")
        || action.contains("__git_committishs")
        || action.contains("__git_revisions")
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

    // systemd — `_wanted systemd-units`, `__systemctl list-units`
    if action.contains("systemd-units") || action.contains("__systemctl list-units") {
        return Some(trie::ARG_MODE_SYSTEMD_UNIT);
    }
    if action.contains("__systemctl list-timers") {
        return Some(trie::ARG_MODE_SYSTEMD_TIMER);
    }

    // Docker — completion functions use `__docker_complete_*` / `__docker_*` prefixes
    if action.contains("__docker_complete_containers")
        || action.contains("__docker_containers_all")
        || action.contains("__docker_complete_running_containers")
        || action.contains("__docker_complete_stopped_containers")
    {
        return Some(trie::ARG_MODE_DOCKER_CONTAINER);
    }
    if action.contains("__docker_complete_images") || action.contains("__docker_images") {
        return Some(trie::ARG_MODE_DOCKER_IMAGE);
    }
    if action.contains("__docker_complete_networks") || action.contains("__docker_networks") {
        return Some(trie::ARG_MODE_DOCKER_NETWORK);
    }
    if action.contains("__docker_complete_volumes") || action.contains("__docker_volumes") {
        return Some(trie::ARG_MODE_DOCKER_VOLUME);
    }
    if action.contains("__docker_complete_services") || action.contains("__docker_compose_services") {
        return Some(trie::ARG_MODE_DOCKER_COMPOSE_SERVICE);
    }

    // kubectl — the generated completion uses `__kubectl_*` helpers.
    if action.contains("__kubectl_get_pods") || action.contains("__kubectl_pod_names") {
        return Some(trie::ARG_MODE_K8S_POD);
    }
    if action.contains("__kubectl_get_namespaces") || action.contains("__kubectl_namespaces") {
        return Some(trie::ARG_MODE_K8S_NAMESPACE);
    }
    if action.contains("__kubectl_get_contexts") || action.contains("__kubectl_config_get-contexts") {
        return Some(trie::ARG_MODE_K8S_CONTEXT);
    }
    if action.contains("__kubectl_get_deployments") {
        return Some(trie::ARG_MODE_K8S_DEPLOYMENT);
    }
    if action.contains("__kubectl_get_services") {
        return Some(trie::ARG_MODE_K8S_SERVICE);
    }

    // Homebrew
    if action.contains("_brew_formulae") || action.contains("__brew_installed_formulae") {
        return Some(trie::ARG_MODE_BREW_FORMULA);
    }
    if action.contains("_brew_casks") || action.contains("__brew_installed_casks") {
        return Some(trie::ARG_MODE_BREW_CASK);
    }

    // tmux — session list helpers
    if action.contains("__tmux-sessions") || action.contains("__tmux_session_names") {
        return Some(trie::ARG_MODE_TMUX_SESSION);
    }
    if action.contains("__tmux-windows") {
        return Some(trie::ARG_MODE_TMUX_WINDOW);
    }
    if action.contains("__tmux-panes") {
        return Some(trie::ARG_MODE_TMUX_PANE);
    }

    // Git — additional helpers
    if action.contains("__git_stashes") || action.contains("__git_recent_stashes") {
        return Some(trie::ARG_MODE_GIT_STASH);
    }
    if action.contains("__git_worktrees") {
        return Some(trie::ARG_MODE_GIT_WORKTREE);
    }
    if action.contains("__git_submodules") {
        return Some(trie::ARG_MODE_GIT_SUBMODULE);
    }
    if action.contains("__git_config_vars") || action.contains("__git_config_get-regexp") {
        return Some(trie::ARG_MODE_GIT_CONFIG_KEY);
    }
    if action.contains("__git_aliases") {
        return Some(trie::ARG_MODE_GIT_ALIAS);
    }

    // Package managers — apt/dpkg
    if action.contains("_apt_packages") || action.contains("_deb_packages") {
        return Some(trie::ARG_MODE_APT_PACKAGE);
    }
    if action.contains("_dnf_packages") || action.contains("_rpm_packages") {
        return Some(trie::ARG_MODE_DNF_PACKAGE);
    }
    if action.contains("_pacman_packages") {
        return Some(trie::ARG_MODE_PACMAN_PACKAGE);
    }
    if action.contains("_npm_packages") {
        return Some(trie::ARG_MODE_NPM_PACKAGE);
    }
    if action.contains("_pip_packages") {
        return Some(trie::ARG_MODE_PIP_PACKAGE);
    }

    None
}

/// The resolved completion for a single Zsh case-state arm.
#[derive(Debug, Clone)]
enum StateAction {
    /// A typed arg mode (e.g. ARG_MODE_HOSTS for _ssh_hosts).
    ArgType(u8),
    /// Run an external command and use its output as completions.
    CallProgram(String, Vec<String>),
    /// A fixed enumeration of literal completion items.
    StaticList(Vec<String>),
}

/// Parse `case $state in` / `case "$lstate" in` blocks from a Zsh completion
/// function body and return a map of state-name → completion action.
///
/// Each case arm is scanned with a priority order:
///   1. `_call_program tag cmd ...` → CallProgram
///   2. `compadd - items` / `_values 'desc' items` → StaticList
///   3. Known function names (`_ssh_hosts`, `__git_branch_names`, …) → ArgType
///   4. `_files` / `_directories` → ArgType(PATHS / DIRS_ONLY)
///
/// Only the first recognisable completion in each arm is recorded.
fn extract_state_handlers(body: &str) -> HashMap<String, StateAction> {
    let mut result = HashMap::new();

    // Find `case $state in` or `case "$lstate" in` or `case "${state}" in` etc.
    let case_pat = ["case $state in", "case \"$state\" in",
                    "case $lstate in", "case \"$lstate\" in",
                    "case ${state} in", "case \"${state}\" in"];
    let case_start = case_pat.iter()
        .filter_map(|p| body.find(p).map(|pos| pos + p.len()))
        .min();

    let Some(case_body_start) = case_start else { return result; };

    // Find `esac` that closes this case block (simplified: first one after case_body_start)
    let case_body_end = body[case_body_start..].find("\nesac")
        .map(|p| case_body_start + p)
        .unwrap_or(body.len());

    let case_body = &body[case_body_start..case_body_end];

    // Split on arm terminators `;;` to get individual arms
    for arm_text in case_body.split(";;") {
        let arm = arm_text.trim();
        if arm.is_empty() {
            continue;
        }

        // Extract arm name: first non-empty line that matches `(name)` or `name)`
        let name = arm.lines()
            .map(str::trim)
            .find(|l| !l.is_empty() && !l.starts_with('#'))
            .and_then(|l| {
                // Forms: `(name)`, `name)`, `(name1|name2)` (take first)
                let stripped = l.trim_start_matches('(');
                let end = stripped.find(')')?;
                let raw = &stripped[..end];
                // Take the first alternative in `name1|name2`
                Some(raw.split('|').next()?.trim().to_string())
            });

        let Some(name) = name else { continue };
        if name.is_empty() || name == "*" {
            continue;
        }

        // Scan the arm body for a recognisable completion
        let action = extract_state_arm_action(arm);
        if let Some(action) = action {
            result.entry(name).or_insert(action);
        }
    }

    result
}

/// Given the raw text of a `case` arm, return the first completion action found.
fn extract_state_arm_action(arm: &str) -> Option<StateAction> {
    // Priority 1: _call_program
    if arm.contains("_call_program")
        && let Some((tag, argv)) = parse_call_program(arm)
    {
        return Some(StateAction::CallProgram(tag, argv));
    }

    // Priority 2: static list via compadd / _values
    if arm.contains("compadd") || arm.contains("_values") {
        // Find the compadd / _values call — look for the line that contains it
        for line in arm.lines() {
            let line = line.trim();
            if (line.contains("compadd") || line.starts_with("_values"))
                && let Some(items) = action_to_static_list(line)
                && !items.is_empty()
            {
                return Some(StateAction::StaticList(items));
            }
        }
    }

    // Priority 3: detect_type_in_block scans every line and combines all types
    // (handles _alternative blocks with mixed __git_cached_files + _directories → PATHS,
    // plain _files/_directories, direct function calls, quoted action specs, etc.)
    if let Some(t) = detect_type_in_block(arm) {
        return Some(StateAction::ArgType(t));
    }

    None
}

/// Extract context-sensitive completion rules from the body of a Zsh function.
///
/// Scans for patterns like:
/// ```zsh
/// if [[ -n ${opt_args[(I)-b|-B|--orphan]} ]]; then
///     _alternative branches::__git_branch_names
/// ```
/// and builds `ContextRule` entries: "when any of [-b, -B, --orphan] are in
/// the current words, override the completion type with GIT_BRANCHES".
///
/// Also handles `if (( $+opt_args[-f] )); then` (single flag, present test).
pub fn extract_state_context_rules(body: &str) -> Vec<trie::ContextRule> {
    let mut rules = Vec::new();
    let lines: Vec<&str> = body.lines().collect();
    let mut i = 0;

    while i < lines.len() {
        let line = lines[i].trim();

        // Match patterns:
        //   if [[ -n ${opt_args[(I)-b|-B|--flag]} ]]
        //   if (( $+opt_args[-f] ))
        let flags = if line.contains("opt_args[(I)") {
            extract_opt_args_flags(line)
        } else if line.contains("$+opt_args[") || line.contains("$+opt_args [") {
            extract_plus_opt_args_flag(line)
        } else {
            i += 1;
            continue;
        };

        if flags.is_empty() {
            i += 1;
            continue;
        }

        // Skip negated conditions: -z, == 0, != 1, < 1, <= 0
        if line.contains("-z ") || line.contains("== 0") || line.contains("!= 1")
            || line.contains("< 1") || line.contains("<= 0")
        {
            i += 1;
            continue;
        }

        // Collect the "then" block (stop at elif/else/fi at the same nesting depth)
        i += 1;
        let then_start = i;
        let mut depth = 0usize;
        while i < lines.len() {
            let l = lines[i].trim();
            // Nesting: `if`/`case` open a new level, `fi`/`esac` close
            if l.starts_with("if ") || l == "if" || l.starts_with("case ") {
                depth += 1;
            }
            if l == "fi" || l == "esac" {
                if depth == 0 {
                    break;
                }
                depth -= 1;
            }
            // At depth 0, elif/else ends the then-block
            if depth == 0 && (l.starts_with("elif ") || l == "else") {
                break;
            }
            i += 1;
        }

        let then_block = lines[then_start..i].join("\n");
        if let Some(StateAction::ArgType(t)) = extract_state_arm_action(&then_block) {
            rules.push(trie::ContextRule {
                trigger_flags: flags,
                override_type: t,
            });
        }
        // Don't increment i here — we want to process the elif/else too
    }

    rules
}

/// Parse `${opt_args[(I)-a|-b|--long]}` flags from a line.
fn extract_opt_args_flags(line: &str) -> Vec<String> {
    let marker = "opt_args[(I)";
    let Some(pos) = line.find(marker) else {
        return vec![];
    };
    let after = &line[pos + marker.len()..];
    let end = after.find("]}").unwrap_or(after.find(']').unwrap_or(0));
    if end == 0 {
        return vec![];
    }
    after[..end]
        .split('|')
        .map(str::trim)
        .filter(|f| f.starts_with('-'))
        .map(str::to_string)
        .collect()
}

/// Parse `$+opt_args[-f]` single-flag from a line.
fn extract_plus_opt_args_flag(line: &str) -> Vec<String> {
    let marker = "$+opt_args[";
    let Some(pos) = line.find(marker) else {
        return vec![];
    };
    let after = &line[pos + marker.len()..];
    let end = after.find(']').unwrap_or(0);
    if end == 0 {
        return vec![];
    }
    let flag = after[..end].trim();
    if flag.starts_with('-') {
        vec![flag.to_string()]
    } else {
        vec![]
    }
}

/// Split a shell command string into tokens, respecting single- and double-quoted
/// strings so that e.g. `compadd -M 'r:|.=* r:|=*'` yields `["-M", "r:|.=* r:|=*"]`
/// rather than splitting the quoted matcher spec on the interior space.
fn shell_tokenize(s: &str) -> Vec<String> {
    let mut tokens = Vec::new();
    let mut current = String::new();
    let mut chars = s.chars().peekable();
    while let Some(&c) = chars.peek() {
        if c.is_ascii_whitespace() {
            if !current.is_empty() {
                tokens.push(std::mem::take(&mut current));
            }
            chars.next();
        } else if c == '\'' {
            chars.next(); // skip opening quote
            while let Some(&ch) = chars.peek() {
                if ch == '\'' {
                    chars.next();
                    break;
                }
                current.push(ch);
                chars.next();
            }
        } else if c == '"' {
            chars.next();
            while let Some(&ch) = chars.peek() {
                if ch == '"' {
                    chars.next();
                    break;
                }
                current.push(ch);
                chars.next();
            }
        } else {
            current.push(c);
            chars.next();
        }
    }
    if !current.is_empty() {
        tokens.push(current);
    }
    tokens
}

/// Extract a static list of literal completion items from a Zsh action string.
///
/// Recognises:
/// - `compadd - item1 item2 ...`
/// - `compadd item1 item2 ...` (no `-` separator)
/// - `_sequence compadd - item1 item2 ...` (the `_sequence` wrapper is ignored)
/// - `_wanted tag expl desc compadd - item1 item2 ...`
/// - `_values 'desc' item1 item2 ...`
///
/// Returns `None` if the action does not produce a static list.
pub fn action_to_static_list(action: &str) -> Option<Vec<String>> {
    let action = action.trim();

    // Strip _sequence / _wanted prefix wrappers to get at the compadd
    let inner = if let Some(pos) = action.find("compadd") {
        &action[pos..]
    } else if action.starts_with("_values") {
        action
    } else {
        return None;
    };

    if inner.starts_with("compadd") {
        // compadd [opts] - item1 item2 ...
        // Skip flags/options (words starting with -), then collect items after
        // the `--` or `-` separator (or directly after compadd if no separator).
        //
        // Quote-aware tokenizer: respect single-quoted strings so that e.g.
        // `-M 'r:|.=* r:|=*'` is treated as one token, not two.
        let tokens = shell_tokenize(inner);
        let tokens: Vec<&str> = tokens.iter().map(String::as_str).collect();
        // tokens[0] == "compadd"
        let mut items = Vec::new();
        let mut after_sep = false;
        let mut array_mode = false;
        let mut i = 1usize;
        while i < tokens.len() {
            let t = tokens[i];
            // The `-` or `--` separator marks the end of compadd options.
            // Only honour the first occurrence; after `after_sep` is set,
            // a bare `-` is a literal completion item (e.g. `compadd - + -`).
            if !after_sep && (t == "-" || t == "--") {
                after_sep = true;
                i += 1;
                continue;
            }
            // Skip compadd option flags and their arguments.
            if !after_sep && t.starts_with('-') {
                // `-a` / `-k`: array mode — items after separator are array
                // names, not literal strings.  We can't expand arrays at parse
                // time, so bail out entirely.
                if t == "-a" || t == "-k" {
                    array_mode = true;
                }
                i += 1;
                // Flags that consume the next token as a value:
                //   -J group  -V group  -X expl  -x msg  -M matcher
                //   -P prefix -S suffix -p hpfx  -s hsfx -I sep
                //   -W fpfx   -F array  -r rchars -R rfunc
                //   -D array  -O array  -A array  -E num  -d array
                if matches!(
                    t,
                    "-J" | "-V" | "-X" | "-x" | "-M"
                    | "-P" | "-S" | "-p" | "-s" | "-I"
                    | "-W" | "-F" | "-r" | "-R"
                    | "-D" | "-O" | "-A" | "-E" | "-d"
                ) {
                    i += 1; // skip option argument
                }
                continue;
            }
            // Shell operators end the compadd argument list
            if matches!(t, "&&" | "||" | ";" | "&" | "|") {
                break;
            }
            // In array mode, words after the separator are array names, not items.
            if array_mode {
                i += 1;
                continue;
            }
            // Skip shell expansions (items starting with $, {, or () )
            if t.starts_with('$') || t.starts_with('{') || t.starts_with('(') {
                i += 1;
                continue;
            }
            // Valid completion item
            let item = t.trim_matches('\'').trim_matches('"');
            if !item.is_empty() && !item.starts_with('$') {
                items.push(item.to_string());
            }
            i += 1;
        }
        if items.is_empty() {
            return None;
        }
        return Some(items);
    }

    if inner.starts_with("_values") {
        // _values 'description' item1 item2 ...
        // or _values -s sep 'description' item1 item2 ...
        let tokens: Vec<&str> = inner.split_whitespace().collect();
        let mut items = Vec::new();
        let mut i = 1usize;
        // Skip _values options (-s, -w, -C, -S, -O)
        let mut desc_done = false;
        while i < tokens.len() {
            let t = tokens[i];
            if t.starts_with('-') {
                i += 1;
                // -s and -O take a separator/variable argument
                if t == "-s" || t == "-O" {
                    i += 1;
                }
                continue;
            }
            // First non-flag token is the description (possibly multi-word, quoted).
            // Consume tokens until we find the closing quote.
            if !desc_done && (t.starts_with('\'') || t.starts_with('"')) {
                let quote = if t.starts_with('\'') { '\'' } else { '"' };
                // If the opening token is also the closing token, done in one step
                let already_closed = t.len() > 1 && t.ends_with(quote);
                i += 1;
                if !already_closed {
                    // Consume more tokens until the closing quote
                    while i < tokens.len() && !tokens[i].ends_with(quote) {
                        i += 1;
                    }
                    if i < tokens.len() {
                        i += 1; // skip the closing token
                    }
                }
                desc_done = true;
                continue;
            }
            // A bare unquoted first token is also the description — skip it
            if !desc_done {
                i += 1;
                desc_done = true;
                continue;
            }
            // Shell operators end the argument list
            if matches!(t, "&&" | "||" | ";" | "&" | "|") {
                break;
            }
            // Collect remaining items (may have 'item:description' form)
            let item = t.split(':').next().unwrap_or(t);
            let item = item.trim_matches('\'').trim_matches('"');
            if !item.is_empty() && !item.starts_with('$') {
                items.push(item.to_string());
            }
            i += 1;
        }
        if items.is_empty() {
            return None;
        }
        return Some(items);
    }

    None
}

/// Extract argument specs from subcommand function bodies within a completion file.
///
/// Finds functions like `_git-add () { ... }` and parses each body for
/// `_arguments` specs. Returns a map of "cmd subcmd" → ArgSpec.
///
/// Also performs a helper-function cross-reference pass: functions like
/// `__git_setup_log_options` / `__git_setup_revision_options` populate spec
/// arrays (`log_options`, `revision_options`) that are passed as `$varname`
/// to `_arguments` in subcommand functions.  We parse those helpers and merge
/// their specs into each subcommand that calls them.
fn extract_subcommand_arg_specs(cmd: &str, content: &str) -> HashMap<String, ArgSpec> {
    let mut specs = HashMap::new();
    let prefix = format!("_{}-", cmd);

    // --- Pre-pass: build a map of helper function name → ArgSpec ---
    // Helpers are `__<cmd>_*` functions whose names suggest they populate spec
    // arrays (e.g. `__git_setup_log_options`, `__git_setup_diff_options`).
    let helper_specs = extract_helper_function_specs(cmd, content);

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
        let mut spec = parse_arg_spec(&body);

        // Merge specs from any helper functions called in this body
        if !helper_specs.is_empty() {
            for body_line in body.lines() {
                let t = body_line.trim();
                // A bare helper call: just the function name on a line, no args
                if let Some(helper_spec) = helper_specs.get(t) {
                    spec.merge(helper_spec);
                }
            }
        }

        if !spec.is_empty() {
            let key = format!("{} {}", cmd, subcmd);
            specs.insert(key, spec);
        }
    }

    specs
}

/// Scan the completion file for helper functions that populate spec arrays.
///
/// Targets functions matching `__<cmd>_setup_*` or `__<cmd>_*_options` —
/// the naming convention used in `_git` for `__git_setup_log_options`,
/// `__git_setup_diff_options`, `__git_setup_revision_options`, etc.
///
/// Returns a map of `function_name → ArgSpec` parsed from the function body.
fn extract_helper_function_specs(cmd: &str, content: &str) -> HashMap<String, ArgSpec> {
    let mut result = HashMap::new();
    // Match `__<cmd>_setup_*` and `__<cmd>_*_options` naming conventions
    let setup_prefix = format!("__{}_setup_", cmd);
    let options_suffix = "_options";

    let lines: Vec<&str> = content.lines().collect();
    let n = lines.len();

    // Collect (start_line_idx, function_name) for matching helpers
    let mut helpers: Vec<(usize, String)> = Vec::new();
    for (i, line) in lines.iter().enumerate() {
        let trimmed = line.trim();
        // Function definition lines look like:
        //   __git_setup_log_options () {
        // or preceded by a guard:
        //   (( $+functions[__git_setup_log_options] )) ||
        //   __git_setup_log_options () {
        let name = trimmed
            .split_whitespace()
            .next()
            .unwrap_or("")
            .trim_end_matches("()");
        if (name.starts_with(&setup_prefix)
            || (name.starts_with(&format!("__{}_", cmd)) && name.ends_with(options_suffix)))
            && (trimmed.contains('(') || trimmed.ends_with('{'))
        {
            helpers.push((i, name.to_string()));
        }
    }

    if helpers.is_empty() {
        return result;
    }

    // Parse each helper's body (lines from its definition to the next helper or EOF)
    for (idx, (start, name)) in helpers.iter().enumerate() {
        let end = if idx + 1 < helpers.len() {
            helpers[idx + 1].0
        } else {
            n
        };
        let body = lines[*start..end].join("\n");
        let spec = parse_arg_spec(&body);
        if !spec.is_empty() {
            result.insert(name.clone(), spec);
        }
    }

    result
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

        // Extract single-quoted and double-quoted strings containing ->
        let mut chars = trimmed.chars().peekable();
        while let Some(&ch) = chars.peek() {
            if ch == '\'' {
                chars.next();
                let mut s = String::new();
                while let Some(&c) = chars.peek() {
                    if c == '\'' { chars.next(); break; }
                    s.push(c);
                    chars.next();
                }
                if s.contains("->") && let Some(r) = parse_state_ref(&s) {
                    refs.push(r);
                }
            } else if ch == '"' {
                chars.next();
                let mut s = String::new();
                let mut escaped = false;
                while let Some(&c) = chars.peek() {
                    chars.next();
                    if escaped { s.push(c); escaped = false; continue; }
                    if c == '\\' { escaped = true; continue; }
                    if c == '"' { break; }
                    s.push(c);
                }
                if s.contains("->") && let Some(r) = parse_state_ref(&s) {
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

/// Extract flag names from `words[(I)flag]` patterns in a Zsh condition string.
///
/// Handles all common variants:
///   `words[(I)-d]`                 → `["-d"]`
///   `words[(I)-d] || words[(I)-D]` → `["-d", "-D"]`
///   `words[(I)(-d|-D)]`            → `["-d", "-D"]`
///   `words[(I)(-r|--remotes)]`     → `["-r", "--remotes"]`
fn extract_words_flags(condition: &str) -> Vec<String> {
    let mut flags = Vec::new();
    let mut s = condition;
    let pattern = "words[(I)";
    while let Some(pos) = s.find(pattern) {
        let after = &s[pos + pattern.len()..];
        if let Some(close) = after.find(']') {
            let inner = &after[..close];
            // Strip enclosing parens: (-d|-D) → -d|-D
            let inner = inner.trim_matches('(').trim_matches(')');
            for part in inner.split('|') {
                let flag = part.trim().trim_matches('(').trim_matches(')');
                if flag.starts_with('-') {
                    // Strip trailing = or + that indicate the flag takes a value
                    let flag = flag.trim_end_matches('=').trim_end_matches('+');
                    if !flag.is_empty() {
                        flags.push(flag.to_string());
                    }
                }
            }
            s = &after[close..];
        } else {
            break;
        }
    }
    flags
}

/// Extract arg type associations from conditional variable blocks.
///
/// Many complex Zsh completion functions (notably `_git-branch`) place their
/// `_arguments` specs inside variables that are conditionally assigned based on
/// which flags are already in the command line, e.g.:
///
/// ```zsh
/// if (( words[(I)-d] || words[(I)-D] )); then
///   dependent_deletion_args=(
///     '*: :__git_ignore_line_inside_arguments __git_branch_names'
///   )
/// fi
/// # ... later ...
/// _arguments ... $dependent_deletion_args
/// ```
///
/// Zsh evaluates this dynamically at completion time. We replicate the intent
/// statically: when flag X appears in a `words[(I)X]` condition and the block
/// body contains a positional spec with a known action, record that association
/// in `spec.flag_args` (gap-fill only).
/// Scan a Zsh completion function body for array variable declarations and their
/// spec strings.  Handles patterns like:
///
/// ```zsh
/// declare -a opts
/// local -a opts
/// opts=( '-f+[file]:file:_files' '--flag=:label:_users' )
/// opts+=( '...' )
/// if (( words[(I)-d] )); then
///   opts+=( '*: :__git_branch_names' )
/// fi
/// _arguments ... "$opts[@]"
/// ```
///
/// Returns a map from array variable name to all spec strings collected across all
/// branches (conservative union — we include specs from every `if`/`else` branch).
fn extract_array_specs(content: &str) -> HashMap<String, Vec<String>> {
    let mut arrays: HashMap<String, Vec<String>> = HashMap::new();
    let lines: Vec<&str> = content.lines().collect();

    // First pass: find all declared array variables (`declare -a`, `local -a`, `typeset -a`)
    let mut tracked: std::collections::HashSet<String> = std::collections::HashSet::new();
    for line in &lines {
        let t = line.trim();
        for kw in &["declare -a ", "local -a ", "typeset -a "] {
            if let Some(rest) = t.strip_prefix(kw) {
                // May be `declare -a foo bar` or `declare -a foo`
                for name in rest.split_whitespace() {
                    let name = name.trim_end_matches('=');
                    if !name.is_empty() && name.chars().all(|c| c.is_alphanumeric() || c == '_') {
                        tracked.insert(name.to_string());
                    }
                }
            }
        }
    }

    if tracked.is_empty() {
        return arrays;
    }

    // Second pass: scan all assignment lines `varname=(...)` and `varname+=(...)`
    let mut i = 0;
    while i < lines.len() {
        let line = lines[i].trim();

        // Match `varname=(` or `varname+=(`
        for var in &tracked {
            let assign_pat = format!("{var}=(");
            let append_pat = format!("{var}+=(");
            let is_assign = line.starts_with(&assign_pat) || line == format!("{var}=").as_str();
            let is_append = line.starts_with(&append_pat);
            if !is_assign && !is_append {
                continue;
            }

            // Collect lines until closing `)` at column 0 (or line-end `)`)
            let mut collected = String::new();
            // Start from first `(` in the line
            let paren_start = line.find('(').unwrap_or(line.len());
            collected.push_str(&line[paren_start..]);

            // Count parens to find the end of the assignment
            let mut depth: i32 = 0;
            let mut j = i;
            'outer: loop {
                let chunk = if j == i { &line[paren_start..] } else { lines[j].trim() };
                for ch in chunk.chars() {
                    match ch {
                        '(' => depth += 1,
                        ')' => {
                            depth -= 1;
                            if depth <= 0 {
                                break 'outer;
                            }
                        }
                        _ => {}
                    }
                }
                j += 1;
                if j >= lines.len() { break; }
                if j > i {
                    collected.push('\n');
                    collected.push_str(lines[j].trim());
                }
            }

            // Extract spec strings from the collected block
            let entry = arrays.entry(var.clone()).or_default();
            for spec_line in collected.lines() {
                for spec_str in extract_argument_specs(spec_line) {
                    if !entry.contains(&spec_str) {
                        entry.push(spec_str);
                    }
                }
            }
        }

        i += 1;
    }

    arrays
}

fn extract_conditional_variable_specs(content: &str, spec: &mut ArgSpec) {
    let lines: Vec<&str> = content.lines().collect();
    let n = lines.len();
    let mut i = 0;

    while i < n {
        let line = lines[i].trim();

        // Match `if` lines that inspect `words[(I)FLAG]`
        if (line.starts_with("if ") || line.starts_with("if("))
            && line.contains("words[(I)")
        {
            let all_flags = extract_words_flags(line);
            if all_flags.is_empty() {
                i += 1;
                continue;
            }

            // Skip negated conditions: `words[(I)FLAG] == 0` means the flag is
            // *absent*.  Associating the flag with the block's arg type would be
            // wrong — the block fires when the flag is NOT present.
            if line.contains("== 0")
                || line.contains("!= 1")
                || line.contains("< 1")
                || line.contains("<= 0")
            {
                i += 1;
                continue;
            }

            // For AND conditions (&&), be conservative: only use flags from OR
            // clusters. If the condition is `words[(I)-a] && words[(I)-b]`, we
            // can't know which flag is "responsible" for the completion type, so
            // we take only the first flag.
            // For OR conditions (||), all flags map to the same type.
            let flags: Vec<String> = if line.contains("&&") && !line.contains("||") {
                // Pure AND: take only the first flag as the trigger
                all_flags.into_iter().take(1).collect()
            } else {
                // OR or single flag: all flags trigger the same type
                all_flags
            };

            // Walk forward to the matching `fi`, respecting nested `if`/`fi`
            let body_start = i + 1;
            let mut depth: i32 = 1;
            let mut j = body_start;
            while j < n && depth > 0 {
                let l = lines[j].trim();
                if l.starts_with("if ") || l == "if" || l.starts_with("if(") {
                    depth += 1;
                } else if l == "fi"
                    || l.starts_with("fi;")
                    || l.starts_with("fi ")
                    || l.starts_with("fi\t")
                {
                    depth -= 1;
                }
                j += 1;
            }
            let body_end = j;

            // Scan the block body (including nested blocks) for quoted spec
            // strings with known actions.  Skip specs that start with `-`
            // (those are flag definitions, not positional arg specs).
            let mut found_type: Option<u8> = None;
            'scan: for line in &lines[body_start..body_end] {
                for spec_str in extract_argument_specs(line.trim()) {
                    let s = spec_str.trim();
                    if s.starts_with('-') {
                        continue;
                    }
                    if let Some(action) = find_action_in_spec(s)
                        && let Some(arg_type) = action_to_arg_type(&action)
                    {
                        found_type = Some(arg_type);
                        break 'scan;
                    }
                }
            }

            if let Some(arg_type) = found_type {
                for flag in &flags {
                    spec.flag_args.entry(flag.clone()).or_insert(arg_type);
                }
            }

            i = body_end;
        } else {
            i += 1;
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

    // Pre-scan: build a map of array variable name → spec strings collected from
    // all assignment sites (including conditional branches).
    let array_specs = extract_array_specs(content);

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

        // For `_arguments` calls that reference array variables ($opts[@], $args[@], etc.),
        // process the specs collected from those arrays.
        if trimmed.contains("_arguments") && trimmed.contains("$") {
            for (var, var_specs) in &array_specs {
                // Check if this _arguments line references $var or "$var[@]" etc.
                if trimmed.contains(&format!("${var}"))
                    || trimmed.contains(&format!("\"${var}"))
                {
                    for spec_str in var_specs {
                        process_spec_string(spec_str, &mut spec);
                    }
                }
            }
        }
    }

    // Extract flag→arg-type associations from conditional variable blocks, e.g.
    //   if (( words[(I)-d] )); then ...'*: :__git_branch_names'... fi
    // This handles completion functions that build _arguments specs dynamically
    // rather than embedding them as literals.
    extract_conditional_variable_specs(content, &mut spec);

    // Resolve ->state references: connect _arguments `->statename` specs
    // to the types detected in `case $state` handler bodies.
    // `extract_state_handlers` handles ArgType, CallProgram, and StaticList.
    // `extract_state_types` is kept as a fallback for plain u8 types.
    let state_refs = extract_state_refs(content);
    if !state_refs.is_empty() {
        let state_actions = extract_state_handlers(content);
        let state_types = if state_actions.is_empty() {
            extract_state_types(content)
        } else {
            HashMap::new()
        };

        for (kind, state_name) in state_refs {
            // Try the richer handler first
            if let Some(action) = state_actions.get(&state_name) {
                match (kind, action.clone()) {
                    (StateRefKind::Rest, StateAction::ArgType(t)) => {
                        // `*:: :->state` is an explicit rest spec — always override.
                        // A bare `:desc:action` may have wrongly pre-populated spec.rest;
                        // the state handler is the authoritative rest type.
                        spec.rest = Some(t);
                    }
                    (StateRefKind::Rest, StateAction::CallProgram(tag, argv)) => {
                        spec.rest_call_program.get_or_insert((tag, argv));
                    }
                    (StateRefKind::Rest, StateAction::StaticList(items)) => {
                        spec.rest_static_list.get_or_insert(items);
                    }
                    (StateRefKind::Positional(pos), StateAction::ArgType(t)) => {
                        spec.positional.entry(pos).or_insert(t);
                    }
                    (StateRefKind::Positional(_), StateAction::CallProgram(tag, argv)) => {
                        // No per-position call_program field; fall through to rest
                        spec.rest_call_program.get_or_insert((tag, argv));
                    }
                    (StateRefKind::Positional(_), StateAction::StaticList(items)) => {
                        spec.rest_static_list.get_or_insert(items);
                    }
                    (StateRefKind::Flag(flag), StateAction::ArgType(t)) => {
                        spec.flag_args.entry(flag).or_insert(t);
                    }
                    (StateRefKind::Flag(flag), StateAction::CallProgram(tag, argv)) => {
                        spec.flag_call_programs.entry(flag)
                            .or_insert_with(|| (tag, argv));
                    }
                    (StateRefKind::Flag(flag), StateAction::StaticList(items)) => {
                        spec.flag_static_lists.entry(flag).or_insert(items);
                    }
                }
            } else if let Some(&arg_type) = state_types.get(&state_name) {
                // Fallback: plain u8 type from detect_type_in_block
                match kind {
                    StateRefKind::Rest => {
                        spec.rest = Some(arg_type);
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

    // Extract context-sensitive rules from opt_args[(I)...] conditions.
    // These are evaluated at completion time by checking which flags the user
    // has already typed on the current command line.
    let context_rules = extract_state_context_rules(content);
    for rule in context_rules {
        let already_covered = spec.context_rules.iter().any(|r| {
            r.trigger_flags
                .iter()
                .any(|f| rule.trigger_flags.contains(f))
        });
        if !already_covered {
            spec.context_rules.push(rule);
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
///
/// Handles three forms:
/// 1. Single-quoted:  `'specifier:desc:action'`
/// 2. Double-quoted:  `"($vars): :action"` — common in `_git-branch` etc.; the
///    `$var` expansions in exclusion lists don't affect action extraction since
///    the action is always the last colon-delimited field.
/// 3. Brace-expanded: `{-f,--flag=}'[desc]:label:_action'`
fn extract_argument_specs(line: &str) -> Vec<String> {
    let mut specs = Vec::new();
    let mut chars = line.chars().peekable();

    while let Some(&ch) = chars.peek() {
        match ch {
            '\'' => {
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
                if s.contains(':') && has_known_action(&s) {
                    specs.push(s);
                }
            }
            '"' => {
                chars.next(); // consume opening quote
                let mut s = String::new();
                let mut escaped = false;
                while let Some(&c) = chars.peek() {
                    chars.next();
                    if escaped {
                        s.push(c);
                        escaped = false;
                    } else if c == '\\' {
                        escaped = true;
                    } else if c == '"' {
                        break;
                    } else {
                        s.push(c);
                    }
                }
                if s.contains(':') && has_known_action(&s) {
                    specs.push(s);
                }
            }
            _ => {
                chars.next();
            }
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

/// Parse a `_call_program tag cmd [args...]` action string.
/// Returns `(tag, argv)` where argv is the command to run.
///
/// Handles two forms:
/// - Direct:   `_call_program ciphers ssh -Q cipher`
/// - Embedded: `compadd - $(_call_program ciphers ssh -Q cipher)`
fn parse_call_program(action: &str) -> Option<(String, Vec<String>)> {
    // Locate `_call_program` anywhere in the action string
    let cp_pos = action.find("_call_program")?;
    let after = &action[cp_pos + "_call_program".len()..];

    // If embedded in $(...), stop at the closing paren; otherwise take the whole tail
    let end = after.find(')').unwrap_or(after.len());
    let inner = &after[..end];

    let mut parts = inner.split_whitespace();
    let tag = parts.next()?.to_string();
    let argv: Vec<String> = parts
        .take_while(|s| !s.starts_with('$') && !s.starts_with('{'))
        .map(|s| s.to_string())
        .collect();
    if argv.is_empty() {
        return None;
    }
    Some((tag, argv))
}

/// Store a call_program association in `spec` based on the spec string prefix.
fn store_call_program(spec_str: &str, tag: String, argv: Vec<String>, spec: &mut ArgSpec) {
    let s = spec_str.trim();
    let entry = (tag, argv);

    // Rest or bare positional
    if s.starts_with('*') {
        let after_star = s.get(1..).unwrap_or("").trim_start();
        if !after_star.starts_with('-') {
            spec.rest_call_program.get_or_insert(entry);
            return;
        }
        // *--flag= : repeatable flag with call_program
        let flags = extract_flags_from_spec(after_star);
        for flag in flags {
            spec.flag_call_programs.entry(flag).or_insert_with(|| entry.clone());
        }
        return;
    }
    if s.starts_with(':') {
        spec.rest_call_program.get_or_insert(entry);
        return;
    }

    // Exclusion-group prefix: (excl)rest_or_flag
    if s.starts_with('(') && let Some(close) = s.find(')') {
        let after = s[close + 1..].trim_start();
        if after.starts_with('*') {
            let after_star = after.get(1..).unwrap_or("").trim_start();
            if after_star.starts_with('-') {
                let flags = extract_flags_from_spec(after_star);
                for flag in flags {
                    spec.flag_call_programs.entry(flag).or_insert_with(|| entry.clone());
                }
            } else {
                spec.rest_call_program.get_or_insert(entry);
            }
            return;
        }
        if after.starts_with(':') {
            spec.rest_call_program.get_or_insert(entry);
            return;
        }
        if after.starts_with('-') {
            let flags = extract_flags_from_spec(after);
            for flag in flags {
                spec.flag_call_programs.entry(flag).or_insert_with(|| entry.clone());
            }
            return;
        }
    }

    // Flag spec
    if s.starts_with('-') {
        let flags = extract_flags_from_spec(s);
        for flag in flags {
            spec.flag_call_programs.entry(flag).or_insert_with(|| entry.clone());
        }
    }
}

/// Store a static list of literal completion items in `spec` based on the spec string prefix.
/// Follows the same routing logic as `store_call_program`.
fn store_static_list(spec_str: &str, items: Vec<String>, spec: &mut ArgSpec) {
    let s = spec_str.trim();

    if s.starts_with('*') {
        let after_star = s.get(1..).unwrap_or("").trim_start();
        if !after_star.starts_with('-') {
            spec.rest_static_list.get_or_insert(items);
            return;
        }
        let flags = extract_flags_from_spec(after_star);
        for flag in flags {
            spec.flag_static_lists.entry(flag).or_insert_with(|| items.clone());
        }
        return;
    }
    if s.starts_with(':') {
        spec.rest_static_list.get_or_insert(items);
        return;
    }

    if s.starts_with('(') && let Some(close) = s.find(')') {
        let after = s[close + 1..].trim_start();
        if after.starts_with('*') {
            let after_star = after.get(1..).unwrap_or("").trim_start();
            if after_star.starts_with('-') {
                let flags = extract_flags_from_spec(after_star);
                for flag in flags {
                    spec.flag_static_lists.entry(flag).or_insert_with(|| items.clone());
                }
            } else {
                spec.rest_static_list.get_or_insert(items);
            }
            return;
        }
        if after.starts_with(':') {
            spec.rest_static_list.get_or_insert(items);
            return;
        }
        if after.starts_with('-') {
            let flags = extract_flags_from_spec(after);
            for flag in flags {
                spec.flag_static_lists.entry(flag).or_insert_with(|| items.clone());
            }
            return;
        }
    }

    if s.starts_with('-') {
        let flags = extract_flags_from_spec(s);
        for flag in flags {
            spec.flag_static_lists.entry(flag).or_insert_with(|| items.clone());
        }
    }
}

/// Process a single _arguments spec string and add to the ArgSpec.
fn process_spec_string(spec_str: &str, spec: &mut ArgSpec) {
    // Find the action: it's after the last colon that isn't inside brackets
    let action = match find_action_in_spec(spec_str) {
        Some(a) => a,
        None => return,
    };

    // _call_program: run an external command at completion time to get values
    if action.contains("_call_program") {
        if let Some((tag, argv)) = parse_call_program(&action) {
            store_call_program(spec_str, tag, argv, spec);
        }
        return;
    }

    // Static list: compadd/compadd/_values with literal items
    if let Some(items) = action_to_static_list(&action) {
        store_static_list(spec_str, items, spec);
        return;
    }

    let arg_type = match action_to_arg_type(&action) {
        Some(t) => t,
        None => return,
    };

    let s = spec_str.trim();

    // `*` prefix: either rest (`*:desc:action`, `*::action`) or a repeatable
    // flag (`*-f+:value:action`, `*--flag=:value:action`).
    if s.starts_with('*') {
        let after_star = s.get(1..).unwrap_or("").trim_start();
        if after_star.starts_with('-') {
            // Repeatable flag: *--flag= or *-f+ — treat like a regular flag spec
            let flags = extract_flags_from_spec(after_star);
            for flag in flags {
                spec.flag_args.insert(flag, arg_type);
            }
        } else {
            // Rest spec: *:desc:action or *::action
            spec.rest = Some(arg_type);
        }
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
    // The ( ... ) is a mutual-exclusion list; after it comes the position digit,
    // `*`, `:` (bare positional), or a flag name.
    if s.starts_with('(') && let Some(close) = s.find(')') {
        let after = s[close + 1..].trim_start();
        if after.starts_with('*') {
            let after_star = after.get(1..).unwrap_or("").trim_start();
            if after_star.starts_with('-') {
                // Repeatable flag: (excl)*--flag= …
                let flags = extract_flags_from_spec(after_star);
                for flag in flags {
                    spec.flag_args.insert(flag, arg_type);
                }
            } else {
                spec.rest = Some(arg_type);
            }
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
        // '(excl): :action' — bare positional (no number), treat as rest
        if after.starts_with(':') {
            spec.rest = Some(arg_type);
            return;
        }
        // '(excl)--flag=:desc:action' — flag with exclusion group
        if after.starts_with('-') {
            let flags = extract_flags_from_spec(after);
            for flag in flags {
                spec.flag_args.insert(flag, arg_type);
            }
            return;
        }
    }

    // Bare positional: ':desc:action' or '::desc:action'
    // In _arguments syntax, `:desc:action` means "next positional argument"
    // and `::desc:action` means "optional next positional".  Both map to rest.
    if s.starts_with(':') {
        spec.rest = Some(arg_type);
        return;
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
        "__git_branch_names",
        "__git_remote_branch_names",
        "__git_heads",
        "__git_tags",
        "__git_remotes",
        "__git_files",
        "__git_cached_files",
        "__git_modified_files",
        "__git_other_files",
        "__git_commit_tags",
        "__git_commits",
        "__git_committishs",
        "__git_revisions",
        "_call_program",
        "compadd",
        "_values",
        "_sequence",
        "_wanted",
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

        // Positional gap-fill IS now applied even for covered commands.
        // The parser stores bare `:desc:action` specs as `spec.rest` instead of
        // numbered slots, so per-position well-known specs are needed to correct
        // cases like `git mv` where pos-2 is a destination path, not a git file.
        // git mv: pos-1 = GIT_FILES (source), pos-2 = PATHS (destination).
        let mv_spec = specs.get("git mv").expect("git mv spec should be added");
        assert_eq!(
            mv_spec.positional.get(&1).copied(),
            Some(trie::ARG_MODE_GIT_FILES),
            "git mv pos-1 should be GIT_FILES"
        );
        assert_eq!(
            mv_spec.positional.get(&2).copied(),
            Some(trie::ARG_MODE_PATHS),
            "git mv pos-2 should be PATHS (destination)"
        );

        // Rest is still NOT injected for covered commands — the parser owns that.
        if let Some(spec) = specs.get("git checkout") {
            assert!(spec.rest.is_none(), "rest must not be injected for covered 'git'");
        }

        // Flag specs ARE applied even for covered commands (parser misses them).
        // git branch: -d/-D/-m/-M should be present
        let branch_spec = specs.get("git branch").expect("git branch flag specs should be added");
        assert!(branch_spec.flag_args.contains_key("-d"), "-d should be supplemented");
        assert!(branch_spec.flag_args.contains_key("-D"), "-D should be supplemented");
        // git checkout: -b should be present
        let co_spec = specs.get("git checkout").expect("git checkout flag specs should be added");
        assert!(co_spec.flag_args.contains_key("-b"), "-b should be supplemented");

        // kill: flag and positional overrides are applied; rest is NOT injected for covered.
        let kill_spec = specs.get("kill").expect("kill -s spec should be added");
        assert!(kill_spec.flag_args.contains_key("-s"), "kill -s should be supplemented");
        assert_eq!(kill_spec.positional.get(&1).copied(), Some(trie::ARG_MODE_PIDS),
            "kill pos-1 should be PIDS");
        assert!(kill_spec.rest.is_none(), "kill rest must not be injected for covered");

        // Commands without completion files are fully covered as before
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
        assert!(!result.contains_key("git"));
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
    fn test_parse_compdef_commands_glob_filtered() {
        // _pip: #compdef -P pip[0-9.]# — should produce no commands (all glob)
        let content = "#compdef -P pip[0-9.]#\n";
        assert!(parse_compdef_commands(content).is_empty());

        // _go: #compdef gccgo gofmt 5l ... — should not include "go"
        let content = "#compdef gccgo gofmt 5l 6l 8l 5g 6g 8g\n";
        let cmds = parse_compdef_commands(content);
        assert!(!cmds.contains(&"go".to_string()));
        assert!(cmds.contains(&"gccgo".to_string()));
        assert!(cmds.contains(&"gofmt".to_string()));
    }

    #[test]
    fn test_go_not_in_cmds_with_completions() {
        let path = "/usr/share/zsh/5.9/functions/_go";
        let Ok(content) = std::fs::read_to_string(path) else {
            return;
        };
        let commands = parse_compdef_commands(&content);
        // The _go file covers gccgo/gofmt etc. but NOT the modern `go` CLI
        assert!(!commands.contains(&"go".to_string()), 
            "_go compdef should not list 'go', got: {:?}", commands);
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
    fn test_extract_words_flags_basic() {
        assert_eq!(extract_words_flags("if (( words[(I)-d] ))"), vec!["-d"]);
        assert_eq!(
            extract_words_flags("if (( words[(I)-d] || words[(I)-D] ))"),
            vec!["-d", "-D"]
        );
        assert_eq!(
            extract_words_flags("if (( words[(I)(-d|-D)] ))"),
            vec!["-d", "-D"]
        );
        assert_eq!(
            extract_words_flags("if (( words[(I)(-r|--remotes)] == 0 ))"),
            vec!["-r", "--remotes"]
        );
        // Condition without words[(I)...] → empty
        assert!(extract_words_flags("if [[ -n $foo ]]").is_empty());
    }

    #[test]
    fn test_conditional_variable_specs_synthetic() {
        let content = r#"
_test-cmd () {
  declare -a deletion_args
  if (( words[(I)-d] || words[(I)-D] )); then
    deletion_args=(
      '*: :__git_branch_names'
    )
  fi
  declare -a modification_args
  if (( words[(I)-m] )); then
    modification_args=(
      ':old branch:__git_branch_names'
      '::new branch:__git_branch_names'
    )
  fi
  _arguments $deletion_args $modification_args
}
"#;
        let spec = parse_arg_spec(content);
        assert_eq!(
            spec.flag_args.get("-d"),
            Some(&trie::ARG_MODE_GIT_BRANCHES),
            "-d should map to GIT_BRANCHES"
        );
        assert_eq!(
            spec.flag_args.get("-D"),
            Some(&trie::ARG_MODE_GIT_BRANCHES),
            "-D should map to GIT_BRANCHES"
        );
        assert_eq!(
            spec.flag_args.get("-m"),
            Some(&trie::ARG_MODE_GIT_BRANCHES),
            "-m should map to GIT_BRANCHES"
        );
    }

    #[test]
    fn test_git_branch_spec_from_completion_file() {
        let path = "/usr/share/zsh/5.9/functions/_git";
        let Ok(content) = std::fs::read_to_string(path) else {
            return;
        };
        let sub_specs = extract_subcommand_arg_specs("git", &content);
        let spec = sub_specs
            .get("git branch")
            .expect("git branch spec missing");
        assert_eq!(
            spec.flag_args.get("-d"),
            Some(&trie::ARG_MODE_GIT_BRANCHES),
            "git branch -d should complete branches, got {:?}",
            spec.flag_args.get("-d")
        );
        assert_eq!(
            spec.flag_args.get("-D"),
            Some(&trie::ARG_MODE_GIT_BRANCHES),
            "git branch -D should complete branches"
        );
        assert_eq!(
            spec.flag_args.get("-m"),
            Some(&trie::ARG_MODE_GIT_BRANCHES),
            "git branch -m should complete branches"
        );
        assert_eq!(
            spec.flag_args.get("-M"),
            Some(&trie::ARG_MODE_GIT_BRANCHES),
            "git branch -M should complete branches"
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

    // --- Gap 1: Double-quoted spec strings ---

    #[test]
    fn test_double_quoted_spec_extraction_rest() {
        // "($l $m $d): :__git_branch_names" — exclusion-group bare positional
        let content = r#"
_test-cmd () {
  _arguments \
    "($l $m $d): :__git_branch_names" \
    "($l $m $d)*--contains=[only list branches that contain commit]: :__git_committishs"
}
"#;
        let spec = parse_arg_spec(content);
        assert_eq!(
            spec.rest,
            Some(trie::ARG_MODE_GIT_BRANCHES),
            "double-quoted bare positional should give rest=GIT_BRANCHES"
        );
        assert!(
            spec.flag_args.contains_key("--contains"),
            "--contains flag should be extracted from double-quoted spec"
        );
        assert_eq!(
            spec.flag_args.get("--contains"),
            Some(&trie::ARG_MODE_GIT_BRANCHES)
        );
    }

    #[test]
    fn test_double_quoted_flag_spec_extraction() {
        // Double-quoted flag spec: "--merged=[...]: :__git_committishs"
        let content = r#"
_test-cmd () {
  _arguments \
    "($l $m $d)--merged=[only list branches that are fully contained by HEAD]: :__git_committishs"
}
"#;
        let spec = parse_arg_spec(content);
        assert_eq!(
            spec.flag_args.get("--merged"),
            Some(&trie::ARG_MODE_GIT_BRANCHES),
            "--merged should map to GIT_BRANCHES via double-quoted spec"
        );
    }

    #[test]
    fn test_git_branch_double_quoted_from_file() {
        let path = "/usr/share/zsh/5.9/functions/_git";
        let Ok(content) = std::fs::read_to_string(path) else {
            return;
        };
        let sub_specs = extract_subcommand_arg_specs("git", &content);
        let spec = sub_specs.get("git branch").expect("git branch spec missing");
        // --contains and --merged come from double-quoted specs in _git-branch
        assert_eq!(
            spec.flag_args.get("--contains"),
            Some(&trie::ARG_MODE_GIT_BRANCHES),
            "--contains should be extracted from double-quoted spec"
        );
        assert_eq!(
            spec.flag_args.get("--merged"),
            Some(&trie::ARG_MODE_GIT_BRANCHES),
            "--merged should be extracted from double-quoted spec"
        );
        // rest should come from "($l $m $d): :__git_branch_names"
        assert_eq!(
            spec.rest,
            Some(trie::ARG_MODE_GIT_BRANCHES),
            "git branch positional (new branch name) should resolve as GIT_BRANCHES"
        );
    }

    // --- Gap 2: Negated conditions ---

    #[test]
    fn test_negated_condition_skipped() {
        let content = r#"
_test-cmd () {
  if (( words[(I)(-r|--remotes)] == 0 )); then
    creation_args=(
      '*: :__git_branch_names'
    )
  fi
  _arguments $creation_args
}
"#;
        let spec = parse_arg_spec(content);
        // The positional spec IS extracted (from the single-quoted string inside
        // the body, via the main line scanner — gap 1+3 handle this).
        // Crucially, `-r` / `--remotes` must NOT be associated with GIT_BRANCHES.
        assert!(
            !spec.flag_args.contains_key("-r"),
            "-r must not be associated with GIT_BRANCHES (negated condition)"
        );
        assert!(
            !spec.flag_args.contains_key("--remotes"),
            "--remotes must not be associated with GIT_BRANCHES (negated condition)"
        );
    }

    // --- Gap 3: Bare :desc:action positional specs ---

    #[test]
    fn test_bare_positional_spec_single_colon() {
        let content = r#"
_test-cmd () {
  _arguments \
    ':old branch name:__git_branch_names'
}
"#;
        let spec = parse_arg_spec(content);
        assert_eq!(
            spec.rest,
            Some(trie::ARG_MODE_GIT_BRANCHES),
            "bare ':desc:action' should set rest=GIT_BRANCHES"
        );
    }

    #[test]
    fn test_bare_positional_spec_double_colon() {
        // '::optional arg:_files' — optional positional, also maps to rest
        let content = r#"
_test-cmd () {
  _arguments \
    '::optional file:_files'
}
"#;
        let spec = parse_arg_spec(content);
        assert_eq!(
            spec.rest,
            Some(trie::ARG_MODE_PATHS),
            "bare '::desc:action' should set rest=PATHS"
        );
    }

    #[test]
    fn test_exclusion_group_bare_positional() {
        // "($l $m $d): :__git_branch_names" after stripping (excl) leaves ": ..."
        let content = r#"
_test-cmd () {
  _arguments "($excl): :__git_branch_names"
}
"#;
        let spec = parse_arg_spec(content);
        assert_eq!(
            spec.rest,
            Some(trie::ARG_MODE_GIT_BRANCHES),
            "'(excl): :action' should set rest=GIT_BRANCHES"
        );
    }

    // --- Gap 4: Helper function cross-reference ---

    #[test]
    fn test_helper_function_spec_merge() {
        let content = r#"
(( $+functions[__test_setup_options] )) ||
__test_setup_options () {
  test_options=(
    '--author=[limit by author]:author'
    '--format=[output format]:format'
    '-u+[track remote]:remote:__git_remotes'
  )
}

(( $+functions[_test-log] )) ||
_test-log () {
  __test_setup_options
  _arguments \
    $test_options \
    '*: :__git_branch_names'
}
"#;
        let sub_specs = extract_subcommand_arg_specs("test", content);
        let spec = sub_specs.get("test log").expect("test log spec missing");
        // rest from '*: :__git_branch_names' in _test-log
        assert_eq!(spec.rest, Some(trie::ARG_MODE_GIT_BRANCHES));
        // -u from __test_setup_options (merged via helper cross-reference)
        assert_eq!(
            spec.flag_args.get("-u"),
            Some(&trie::ARG_MODE_GIT_REMOTES),
            "-u should be merged from helper function"
        );
    }

    #[test]
    fn test_git_log_revision_options_from_file() {
        let path = "/usr/share/zsh/5.9/functions/_git";
        let Ok(content) = std::fs::read_to_string(path) else {
            return;
        };
        let sub_specs = extract_subcommand_arg_specs("git", &content);
        let spec = sub_specs.get("git log").expect("git log spec missing");
        // __git_setup_revision_options provides many flag specs; check that the
        // helper cross-reference wired them in (the spec should be non-empty)
        assert!(
            !spec.flag_args.is_empty(),
            "git log should have flag specs from __git_setup_revision_options"
        );
    }

    // --- ArgSpec::merge ---

    #[test]
    fn test_argspec_merge_gap_fill() {
        let mut base = trie::ArgSpec::default();
        base.positional.insert(1, trie::ARG_MODE_PATHS);
        base.rest = Some(trie::ARG_MODE_PATHS);

        let mut other = trie::ArgSpec::default();
        other.positional.insert(1, trie::ARG_MODE_GIT_BRANCHES); // should NOT overwrite PATHS
        other.positional.insert(2, trie::ARG_MODE_GIT_REMOTES);  // should fill in
        other.rest = Some(trie::ARG_MODE_GIT_BRANCHES);           // should NOT overwrite PATHS
        other.flag_args.insert("-u".into(), trie::ARG_MODE_GIT_REMOTES);

        base.merge(&other);

        assert_eq!(base.positional.get(&1), Some(&trie::ARG_MODE_PATHS), "pos 1 not overwritten");
        assert_eq!(base.positional.get(&2), Some(&trie::ARG_MODE_GIT_REMOTES), "pos 2 filled in");
        assert_eq!(base.rest, Some(trie::ARG_MODE_PATHS), "rest not overwritten");
        assert_eq!(base.flag_args.get("-u"), Some(&trie::ARG_MODE_GIT_REMOTES), "-u filled in");
    }

    // --- _call_program parsing ---

    #[test]
    fn test_call_program_direct_action() {
        // _call_program as the direct spec action
        let content = r#"
_test-cmd () {
  _arguments \
    '-c+[cipher]:cipher:_call_program ciphers ssh -Q cipher'
}
"#;
        let spec = parse_arg_spec(content);
        let (tag, argv) = spec
            .flag_call_programs
            .get("-c")
            .expect("-c should have a call_program");
        assert_eq!(tag, "ciphers");
        assert_eq!(argv, &["ssh", "-Q", "cipher"]);
    }

    #[test]
    fn test_call_program_embedded_in_compadd() {
        // _call_program embedded inside a compadd call
        let content = r#"
_test-cmd () {
  _arguments \
    '-Z+[cipher]:cipher:compadd - $(_call_program ciphers ssh -Q cipher)'
}
"#;
        let spec = parse_arg_spec(content);
        let (tag, argv) = spec
            .flag_call_programs
            .get("-Z")
            .expect("-Z should have a call_program");
        assert_eq!(tag, "ciphers");
        assert_eq!(argv, &["ssh", "-Q", "cipher"]);
    }

    #[test]
    fn test_call_program_rest_positional() {
        // _call_program for a rest/positional argument
        let content = r#"
_test-cmd () {
  _arguments \
    '*:module:_call_program modules myapp list'
}
"#;
        let spec = parse_arg_spec(content);
        let (tag, argv) = spec
            .rest_call_program
            .as_ref()
            .expect("rest_call_program should be set");
        assert_eq!(tag, "modules");
        assert_eq!(argv, &["myapp", "list"]);
    }

    #[test]
    fn test_call_program_parse_helper_direct() {
        // Unit test for parse_call_program function directly
        let direct = parse_call_program("_call_program macs ssh -Q mac");
        assert_eq!(direct, Some(("macs".into(), vec!["ssh".into(), "-Q".into(), "mac".into()])));

        let embedded = parse_call_program("compadd - $(_call_program macs ssh -Q mac)");
        assert_eq!(embedded, Some(("macs".into(), vec!["ssh".into(), "-Q".into(), "mac".into()])));

        let none = parse_call_program("_files -/");
        assert_eq!(none, None);
    }

    #[test]
    fn test_call_program_from_ssh_file() {
        let path = "/usr/share/zsh/5.9/functions/_ssh";
        let Ok(content) = std::fs::read_to_string(path) else {
            return; // skip if not available
        };
        let sub_specs = extract_subcommand_arg_specs("ssh", &content);
        // The -c flag in _ssh completions is handled via state machine, so the
        // spec may be empty for ssh itself; just verify parsing doesn't crash
        let _ = sub_specs;
    }

    // --- State machine (->state) resolution ---

    #[test]
    fn test_state_machine_call_program() {
        let content = r#"
_test-cmd () {
  _arguments \
    '-c+[select cipher]:cipher:->ciphers' \
    ':host:->userhost'

  case $state in
    ciphers)
      _wanted ciphers expl cipher _sequence compadd - $(_call_program ciphers ssh -Q cipher)
      return
      ;;
    userhost)
      _wanted hosts expl host _ssh_hosts
      return
      ;;
  esac
}
"#;
        let spec = parse_arg_spec(content);
        // -c should resolve to call_program via the ciphers state
        let entry = spec.flag_call_programs.get("-c")
            .expect("-c should have a call_program from state machine");
        assert_eq!(entry.0, "ciphers");
        assert_eq!(entry.1, &["ssh", "-Q", "cipher"]);
        // :host should resolve to HOSTS via userhost state
        assert_eq!(spec.rest, None, "rest should not be set (positional 1 covers it)");
    }

    #[test]
    fn test_state_machine_static_list() {
        let content = r#"
_test-mode () {
  _arguments \
    '-m+[compression mode]:mode:->mode'

  case $state in
    mode)
      compadd - fast slow best
      return
      ;;
  esac
}
"#;
        let spec = parse_arg_spec(content);
        let items = spec.flag_static_lists.get("-m")
            .expect("-m should have a static list from state machine");
        assert!(items.contains(&"fast".to_string()), "fast should be in list");
        assert!(items.contains(&"slow".to_string()), "slow should be in list");
        assert!(items.contains(&"best".to_string()), "best should be in list");
    }

    #[test]
    fn test_state_machine_arg_type() {
        let content = r#"
_test-copy () {
  _arguments \
    ':source file:->srcfile' \
    ':dest dir:->destdir'

  case $state in
    srcfile)
      _files
      return
      ;;
    destdir)
      _directories
      return
      ;;
  esac
}
"#;
        let spec = parse_arg_spec(content);
        // srcfile state → PATHS, destdir state → DIRS_ONLY
        // Both map to positional or rest
        assert!(
            spec.rest == Some(trie::ARG_MODE_PATHS)
                || spec.positional.values().any(|&v| v == trie::ARG_MODE_PATHS),
            "should have PATHS from srcfile state"
        );
    }

    #[test]
    fn test_state_machine_from_ssh_file() {
        let path = "/usr/share/zsh/5.9/functions/_ssh";
        let Ok(content) = std::fs::read_to_string(path) else {
            return; // skip if not available
        };
        let spec = parse_arg_spec(&content);
        // -c+[cipher]:->ciphers state → ciphers) arm → _call_program ciphers ssh -Q cipher
        let cipher_entry = spec.flag_call_programs.get("-c");
        assert!(
            cipher_entry.is_some(),
            "-c should have a call_program (via ciphers state). Got flag_call_programs: {:?}",
            spec.flag_call_programs.keys().collect::<Vec<_>>()
        );
        if let Some((tag, argv)) = cipher_entry {
            assert_eq!(tag, "ciphers");
            assert_eq!(argv, &["ssh", "-Q", "cipher"]);
        }
        // -m+[mac]:->macs state → macs) arm → _call_program macs ssh -Q mac
        let mac_entry = spec.flag_call_programs.get("-m");
        assert!(mac_entry.is_some(), "-m should have a call_program (via macs state)");
    }

    // --- Static list direct action ---

    #[test]
    fn test_static_list_compadd_direct() {
        let content = r#"
_test-cmd () {
  _arguments \
    '-m+[mode]:mode:compadd - yes no maybe'
}
"#;
        let spec = parse_arg_spec(content);
        let items = spec.flag_static_lists.get("-m")
            .expect("-m should have a static list");
        assert_eq!(items, &["yes", "no", "maybe"]);
    }

    #[test]
    fn test_static_list_values() {
        let content = r#"
_test-cmd () {
  _arguments \
    '-t+[type]:type:_values "compression type" fast slow best'
}
"#;
        let spec = parse_arg_spec(content);
        let items = spec.flag_static_lists.get("-t")
            .expect("-t should have a static list from _values");
        assert!(items.contains(&"fast".to_string()));
        assert!(items.contains(&"slow".to_string()));
    }

    #[test]
    fn test_static_list_sequence() {
        let content = r#"
_test-cmd () {
  _arguments \
    '-k+[key type]:key type:_sequence compadd - rsa ecdsa ed25519'
}
"#;
        let spec = parse_arg_spec(content);
        let items = spec.flag_static_lists.get("-k")
            .expect("-k should have a static list from _sequence compadd");
        assert!(items.contains(&"rsa".to_string()));
        assert!(items.contains(&"ed25519".to_string()));
    }

    #[test]
    fn test_static_list_compadd_matcher_spec_not_leaked() {
        // compadd -M 'matcher' should not leak the matcher spec as an item.
        // This is a direct call to action_to_static_list (the real _git uses
        // compadd -M inside function bodies / ->state arms, not _arguments actions,
        // because the matcher spec contains colons that would break spec parsing).
        let result = action_to_static_list(
            "compadd -M 'r:|.=* r:|=*' - foo bar baz"
        );
        let items = result.expect("should produce a static list");
        assert_eq!(items, &["foo", "bar", "baz"]);
        // Matcher specs must NOT appear as items
        assert!(!items.iter().any(|i| i.contains("r:|")));
    }

    #[test]
    fn test_static_list_compadd_array_mode_returns_none() {
        // compadd -a - arrayname  →  items come from array expansion, not literals
        let result = action_to_static_list("compadd -M 'r:|.=* r:|=*' -a - git_present_options");
        assert!(result.is_none(), "array-mode compadd should not produce static items");
    }

    #[test]
    fn test_static_list_compadd_k_array_mode_returns_none() {
        // compadd -k - assoc_array  →  keys from assoc array, not literals
        let result = action_to_static_list("compadd -k - my_hash");
        assert!(result.is_none(), "-k array-mode compadd should not produce static items");
    }

    // --- OR conditions ---

    #[test]
    fn test_or_condition_both_flags_get_type() {
        let content = r#"
_git-branch () {
  declare -a dependent_deletion_args
  if (( words[(I)-d] || words[(I)-D] )); then
    dependent_deletion_args=(
      '*: :__git_ignore_line_inside_arguments __git_branch_names'
    )
  fi
  _arguments -S "$dependent_deletion_args[@]"
}
"#;
        let spec = parse_arg_spec(content);
        // Both -d and -D should be associated with GIT_BRANCHES
        assert_eq!(
            spec.flag_args.get("-d"),
            Some(&trie::ARG_MODE_GIT_BRANCHES),
            "-d should map to GIT_BRANCHES from OR condition"
        );
        assert_eq!(
            spec.flag_args.get("-D"),
            Some(&trie::ARG_MODE_GIT_BRANCHES),
            "-D should map to GIT_BRANCHES from OR condition"
        );
    }

    // --- Dynamic array construction ---

    #[test]
    fn test_dynamic_array_construction_basic() {
        let content = r#"
_test-cmd () {
  declare -a args
  args=(
    '-f+[file]:file:_files'
    '-u+[user]:user:_users'
  )
  _arguments "$args[@]"
}
"#;
        let spec = parse_arg_spec(content);
        assert_eq!(
            spec.flag_args.get("-f"),
            Some(&trie::ARG_MODE_PATHS),
            "-f should be PATHS from dynamic array"
        );
        assert_eq!(
            spec.flag_args.get("-u"),
            Some(&trie::ARG_MODE_USERS),
            "-u should be USERS from dynamic array"
        );
    }

    #[test]
    fn test_dynamic_array_construction_append() {
        let content = r#"
_test-cmd () {
  local -a opts
  opts=( '--output=[output file]:file:_files' )
  opts+=( '--user=[user]:user:_users' )
  _arguments $opts[@]
}
"#;
        let spec = parse_arg_spec(content);
        assert_eq!(
            spec.flag_args.get("--output"),
            Some(&trie::ARG_MODE_PATHS),
            "--output should be PATHS"
        );
        assert_eq!(
            spec.flag_args.get("--user"),
            Some(&trie::ARG_MODE_USERS),
            "--user should be USERS"
        );
    }

    // ── Context rules ──────────────────────────────────────────────────────────

    #[test]
    fn test_context_rules_basic_opt_args() {
        // Simulate a function body with opt_args[(I)...] conditions
        let content = r#"
_git-checkout() {
  _arguments -C -s \
    '(-b -B)-b+[create new branch]:branch:->branch' \
    '*: :->branch-or-file'

  case $state in
    branch-or-file)
      if [[ -n ${opt_args[(I)-b|-B|--orphan]} ]]; then
        __git_branch_names
      else
        _files
      fi
      ;;
  esac
}
"#;
        let rules = extract_state_context_rules(content);
        assert!(
            !rules.is_empty(),
            "should extract at least one context rule"
        );
        let rule = &rules[0];
        assert!(
            rule.trigger_flags.contains(&"-b".to_string()),
            "rule should trigger on -b"
        );
        assert!(
            rule.trigger_flags.contains(&"-B".to_string()),
            "rule should trigger on -B"
        );
        assert_eq!(
            rule.override_type,
            trie::ARG_MODE_GIT_BRANCHES,
            "override should be GIT_BRANCHES"
        );
    }

    #[test]
    fn test_context_rules_plus_opt_args() {
        // $+opt_args[-f] single-flag form
        let content = r#"
_my_cmd() {
  case $state in
    arg)
      if (( $+opt_args[-l] )); then
        _ssh_users
      else
        _ssh_hosts
      fi
      ;;
  esac
}
"#;
        let rules = extract_state_context_rules(content);
        assert!(!rules.is_empty(), "should extract rule from $+opt_args[-l]");
        let rule = &rules[0];
        assert!(rule.trigger_flags.contains(&"-l".to_string()));
        assert_eq!(rule.override_type, trie::ARG_MODE_USERS);
    }

    #[test]
    fn test_context_rules_negated_skipped() {
        // -z (negated) conditions should be skipped — they trigger when flag is ABSENT
        let content = r#"
_my_cmd() {
  case $state in
    arg)
      if [[ -z ${opt_args[(I)-l]} ]]; then
        _ssh_hosts
      fi
      ;;
  esac
}
"#;
        let rules = extract_state_context_rules(content);
        assert!(
            rules.is_empty(),
            "negated condition (-z) should not produce context rules"
        );
    }

    #[test]
    fn test_context_rules_stored_in_argspec() {
        // Full round-trip: parse_arg_spec should populate context_rules
        let content = r#"
_git-checkout() {
  _arguments -C \
    '(-b -B)-b+[create]:branch:->branch' \
    '*: :->branch-or-file'

  case $state in
    branch-or-file)
      if [[ -n ${opt_args[(I)-b|-B]} ]]; then
        __git_branch_names
      else
        _files
      fi
      ;;
  esac
}
"#;
        let spec = parse_arg_spec(content);
        assert!(
            !spec.context_rules.is_empty(),
            "parse_arg_spec should propagate context rules"
        );
        assert!(
            spec.context_rules[0].trigger_flags.contains(&"-b".to_string())
        );
    }

    // ── @ prefix splitting ─────────────────────────────────────────────────────

    #[test]
    fn test_at_prefix_splitting_in_arg_spec() {
        // Ensure ssh positional[1] = ARG_MODE_HOSTS so @ splitting activates
        let mut specs: HashMap<String, trie::ArgSpec> = HashMap::new();
        let cmds: std::collections::HashSet<String> = std::collections::HashSet::new();
        apply_well_known_specs(&mut specs, &cmds);
        let ssh_spec = specs.get("ssh").expect("ssh should have well-known spec");
        assert_eq!(
            ssh_spec.positional.get(&1).copied(),
            Some(trie::ARG_MODE_HOSTS),
            "ssh positional[1] should be HOSTS so @ splitting activates"
        );
    }

    // ── _call_program well-known specs ─────────────────────────────────────────

    #[test]
    fn test_apt_install_call_program() {
        let mut specs: HashMap<String, trie::ArgSpec> = HashMap::new();
        let cmds: std::collections::HashSet<String> = std::collections::HashSet::new();
        apply_well_known_specs(&mut specs, &cmds);

        let spec = specs.get("apt install").expect("apt install should have well-known spec");
        let (tag, argv) = spec.rest_call_program.as_ref().expect("apt install should have rest_call_program");
        assert_eq!(tag, "package");
        assert_eq!(argv[0], "apt-cache");
        assert_eq!(argv[1], "pkgnames");
    }

    #[test]
    fn test_ip_static_list() {
        let mut specs: HashMap<String, trie::ArgSpec> = HashMap::new();
        let cmds: std::collections::HashSet<String> = std::collections::HashSet::new();
        apply_well_known_specs(&mut specs, &cmds);

        let spec = specs.get("ip").expect("ip should have well-known spec");
        let items = spec.rest_static_list.as_ref().expect("ip should have rest_static_list");
        assert!(items.contains(&"addr".to_string()), "ip should complete addr");
        assert!(items.contains(&"route".to_string()), "ip should complete route");
        assert!(items.contains(&"link".to_string()), "ip should complete link");
    }

    #[test]
    fn test_apt_get_install_call_program() {
        let mut specs: HashMap<String, trie::ArgSpec> = HashMap::new();
        let cmds: std::collections::HashSet<String> = std::collections::HashSet::new();
        apply_well_known_specs(&mut specs, &cmds);

        let spec = specs.get("apt-get install").expect("apt-get install should have spec");
        let (tag, argv) = spec.rest_call_program.as_ref().expect("apt-get install should have rest_call_program");
        assert_eq!(tag, "package");
        assert_eq!(argv[0], "apt-cache");
    }

    // ── Shell-operator truncation in static list extraction ────────────────────

    #[test]
    fn test_static_list_stops_at_shell_operators() {
        // compadd - + - && ret=0  →  items should be ["+", "-"], NOT ["&&", "ret=0"]
        let items = action_to_static_list("compadd - + - && ret=0");
        let items = items.expect("should parse items");
        assert!(items.contains(&"+".to_string()), "should contain +");
        assert!(items.contains(&"-".to_string()), "should contain -");
        assert!(
            !items.contains(&"&&".to_string()),
            "&& should NOT be an item"
        );
        assert!(
            !items.iter().any(|i| i.contains("ret")),
            "ret=0 should NOT be an item"
        );
    }

    #[test]
    fn test_static_list_values_stops_at_operator() {
        // _values 'truth value' yes no && ret=0  →  items are [yes, no]
        let items = action_to_static_list("_values 'truth value' yes no && ret=0");
        let items = items.expect("should parse items");
        assert_eq!(items, vec!["yes".to_string(), "no".to_string()]);
        assert!(!items.contains(&"&&".to_string()));
    }

    #[test]
    fn test_ssh_option_state_arm_no_garbage() {
        // The _ssh option state arm contains `compadd - + - && ret=0`.
        // After the fix, the static list for -o should be ["+", "-"] only.
        let content = std::fs::read_to_string("/usr/share/zsh/5.9/functions/_ssh").unwrap_or_default();
        if content.is_empty() {
            return; // skip on systems without this file
        }
        let spec = parse_arg_spec(&content);
        // -o → flag_static_lists["-o"] should exist (it's a string option with literals)
        // OR flag_args["-o"] → some type, either way no garbage items
        if let Some(items) = spec.flag_static_lists.get("-o") {
            for item in items {
                assert!(
                    !item.contains("ret") && item != "&&" && item != "||",
                    "flag_static_lists[\"-o\"] contains Zsh syntax garbage: {:?}",
                    item
                );
            }
        }
        // Also check the flat rest_static_list
        if let Some(items) = &spec.rest_static_list {
            for item in items {
                assert!(
                    !item.contains("ret") && item != "&&" && item != "||",
                    "rest_static_list contains Zsh syntax garbage: {:?}",
                    item
                );
            }
        }
    }

    #[test]
    fn completion_dirs_is_deduplicated() {
        let dirs = completion_dirs();
        let mut sorted = dirs.clone();
        sorted.sort();
        sorted.dedup();
        assert_eq!(
            dirs.len(),
            sorted.len(),
            "completion_dirs must not contain duplicates",
        );
    }
}
