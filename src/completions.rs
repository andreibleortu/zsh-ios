use std::collections::HashMap;
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

    let (subcmds, arg_specs) = extract_from_dirs(&fpath_dirs);
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

    // Apply well-known hardcoded specs for commands whose Zsh completions are
    // too dynamic to parse statically (runtime conditionals, _alternative, etc.)
    apply_well_known_specs(&mut trie.arg_specs);

    total
}

/// Hardcoded arg specs for commands where Zsh completions use runtime-conditional
/// logic that static parsing can't resolve. These only fill in gaps — they won't
/// overwrite a position if the parser already detected a non-Paths type.
fn apply_well_known_specs(specs: &mut HashMap<String, ArgSpec>) {
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
        ("git add", &[], Some(ARG_MODE_GIT_FILES), &[]),
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
    ];

    for &(cmd, positional, rest, flags) in overrides {
        let spec = specs.entry(cmd.to_string()).or_default();
        for &(pos, arg_type) in positional {
            // Overwrite if the parser only found a generic type (Paths/Normal)
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
            // Overwrite generic types for flags too
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
fn extract_from_dirs(dirs: &[String]) -> (HashMap<String, Vec<String>>, HashMap<String, ArgSpec>) {
    let mut subcmds: HashMap<String, Vec<String>> = HashMap::new();
    let mut arg_specs: HashMap<String, ArgSpec> = HashMap::new();

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
                let subs = extract_subcommands_from_content(cmd, &content);
                if !subs.is_empty() {
                    subcmds.entry(cmd.to_string()).or_default().extend(subs);
                }

                let commands = parse_compdef_commands(&content);
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

    (subcmds, arg_specs)
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
fn action_to_arg_type(action: &str) -> Option<u8> {
    let action = action.trim().trim_matches('\'').trim_matches('"');

    // Commands / executables
    if action.contains("_command_names")
        || action.contains("_path_commands")
        || action.contains(":_commands")
    {
        return Some(trie::ARG_MODE_EXECS_ONLY);
    }

    // Directories
    if action.contains("_directories")
        || action.contains("_files -/")
        || action.contains("_path_files -/")
        || (action.contains("_path_files") && action.contains("-/"))
    {
        return Some(trie::ARG_MODE_DIRS_ONLY);
    }

    // Files (general)
    if action.contains("_files") || action.contains("_path_files") {
        return Some(trie::ARG_MODE_PATHS);
    }

    // Git-specific
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
    if action.contains("__git_files")
        || action.contains("__git_cached_files")
        || action.contains("__git_modified_files")
        || action.contains("__git_other_files")
    {
        return Some(trie::ARG_MODE_GIT_FILES);
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
    } else {
        // Bare ':desc:->state' — treat as rest
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
fn detect_type_in_block(body: &str) -> Option<u8> {
    let mut has_files = false;
    let mut has_dirs = false;
    let mut has_execs = false;

    for line in body.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with('#') {
            continue;
        }

        // Direct calls
        if trimmed.contains("_directories")
            || (trimmed.contains("_path_files") && trimmed.contains("-/"))
            || trimmed.contains("_files -/")
        {
            has_dirs = true;
        }
        if (trimmed.contains("_files") || trimmed.contains("_path_files"))
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

        // Parse single-quoted 'tag:desc:action' specs from _alternative / _values
        scan_quoted_action_specs(trimmed, &mut has_files, &mut has_dirs, &mut has_execs);
    }

    if has_execs && !has_files && !has_dirs {
        Some(trie::ARG_MODE_EXECS_ONLY)
    } else if has_dirs && !has_files {
        Some(trie::ARG_MODE_DIRS_ONLY)
    } else if has_files || has_dirs {
        Some(trie::ARG_MODE_PATHS)
    } else {
        None
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
                match t {
                    trie::ARG_MODE_DIRS_ONLY => *has_dirs = true,
                    trie::ARG_MODE_PATHS => *has_files = true,
                    trie::ARG_MODE_EXECS_ONLY => *has_execs = true,
                    _ => {}
                }
            }

            // _regex_arguments / _regex_words format: ':tag:desc:action'
            if let Some(stripped) = s.strip_prefix(':') {
                let parts: Vec<&str> = stripped.splitn(3, ':').collect();
                if parts.len() >= 3
                    && let Some(t) = action_to_arg_type(parts[2])
                {
                    match t {
                        trie::ARG_MODE_DIRS_ONLY => *has_dirs = true,
                        trie::ARG_MODE_PATHS => *has_files = true,
                        trie::ARG_MODE_EXECS_ONLY => *has_execs = true,
                        _ => {}
                    }
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
            // Only include strings that look like argument specs (contain colons
            // and an action we care about)
            if s.contains(':')
                && (s.contains("_files")
                    || s.contains("_directories")
                    || s.contains("_command")
                    || s.contains("_path_files")
                    || s.contains("_path_commands"))
            {
                specs.push(s);
            }
        } else {
            chars.next();
        }
    }

    specs
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

    // Flag spec: starts with - or ( (exclusion group)
    // Extract flag names from patterns like:
    //   '-o+:desc:_files'  → flag "-o"
    //   '--output=:desc:_files'  → flag "--output"
    //   '(-f --flag)'{-f,--flag}':desc:_files' → flags "-f", "--flag"
    //   But we see the inner part after brace expansion, so we get:
    //   '-f:desc:_files' and '--flag:desc:_files'
    if s.starts_with('-') || s.starts_with('(') {
        let flags = extract_flags_from_spec(s);
        for flag in flags {
            spec.flag_args.insert(flag, arg_type);
        }
    }
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
    if last.contains("_files")
        || last.contains("_directories")
        || last.contains("_command")
        || last.contains("_path_files")
        || last.contains("_path_commands")
    {
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
}
