use crate::path_resolve;
use crate::pins::Pins;
use crate::trie::{CommandTrie, TrieNode};

/// Result of resolving an abbreviated command line.
#[derive(Debug)]
pub enum ResolveResult {
    /// Fully resolved -- the expanded command line.
    Resolved(String),
    /// Ambiguous at some word position.
    Ambiguous(AmbiguityInfo),
    /// A path argument is ambiguous -- multiple fully-resolved commands.
    PathAmbiguous(Vec<String>),
    /// Nothing to resolve -- input is empty or already fully expanded.
    Passthrough(String),
}

#[derive(Debug)]
pub struct AmbiguityInfo {
    /// The abbreviated word that was ambiguous.
    pub word: String,
    /// Position (0-indexed) in the input where ambiguity occurred.
    pub position: usize,
    /// All candidate expansions at this level.
    pub candidates: Vec<String>,
    /// Longest common prefix of all candidates -- Tab can expand to this.
    pub lcp: String,
    /// Candidates narrowed by looking at subsequent words (deep disambiguation).
    /// Each entry is (command_path, subcommand_matches).
    pub deep_candidates: Vec<DeepCandidate>,
    /// Words resolved so far (before the ambiguous position).
    pub resolved_prefix: Vec<String>,
    /// Words remaining after the ambiguous word.
    pub remaining: Vec<String>,
}

#[derive(Debug)]
pub struct DeepCandidate {
    pub command: String,
    pub subcommand_matches: Vec<String>,
}

/// Resolve an abbreviated command line against the trie and pins.
pub fn resolve(input: &str, trie: &CommandTrie, pins: &Pins) -> ResolveResult {
    let words: Vec<&str> = input.split_whitespace().collect();
    if words.is_empty() {
        return ResolveResult::Passthrough(input.to_string());
    }

    // Check pins first (longest-prefix match)
    let pin_result = pins.longest_match(&words);

    let (pin_consumed, expanded_prefix) = match pin_result {
        Some((consumed, expanded)) => (consumed, expanded),
        None => (0, vec![]),
    };

    if pin_consumed > 0 {
        // Pin matched some prefix words. Now resolve the rest against the trie.
        let remaining_words = &words[pin_consumed..];

        // Walk the trie to the node corresponding to the expanded prefix
        let mut node = &trie.root;
        for expanded_word in &expanded_prefix {
            match node.get_child(expanded_word) {
                Some(child) => node = child,
                None => {
                    let mut result_words = expanded_prefix;
                    result_words.extend(remaining_words.iter().map(|s| s.to_string()));
                    return finalize_with_paths(input, result_words);
                }
            }
        }

        let mut result_words = expanded_prefix;
        match resolve_from_node(remaining_words, node, &mut result_words) {
            Ok(()) => finalize_with_paths(input, result_words),
            Err(ambiguity) => ResolveResult::Ambiguous(*ambiguity),
        }
    } else {
        let mut result_words: Vec<String> = Vec::new();
        match resolve_from_node(&words, &trie.root, &mut result_words) {
            Ok(()) => finalize_with_paths(input, result_words),
            Err(ambiguity) => ResolveResult::Ambiguous(*ambiguity),
        }
    }
}

fn finalize_with_paths(input: &str, words: Vec<String>) -> ResolveResult {
    let mode = words.first().map(|w| arg_mode(w)).unwrap_or(ArgMode::Normal);
    match resolve_paths_in_words(&words, mode) {
        PathsResult::Resolved(result) => {
            if result == input {
                ResolveResult::Passthrough(input.to_string())
            } else {
                ResolveResult::Resolved(result)
            }
        }
        PathsResult::Ambiguous(candidates) => ResolveResult::PathAmbiguous(candidates),
    }
}

/// Walk the trie resolving each word. On success, appends to `result`.
/// On ambiguity, returns an AmbiguityInfo error.
fn resolve_from_node(
    words: &[&str],
    start_node: &TrieNode,
    result: &mut Vec<String>,
) -> Result<(), Box<AmbiguityInfo>> {
    if words.is_empty() {
        return Ok(());
    }

    // For path/dir commands, skip trie resolution for arguments --
    // they'll be resolved against the filesystem later.
    if !result.is_empty() {
        let mode = arg_mode(&result[0]);
        if mode == ArgMode::DirsOnly || mode == ArgMode::Paths {
            for w in words {
                result.push(w.to_string());
            }
            return Ok(());
        }
    }

    let word = words[0];
    let rest = &words[1..];

    // Flags (start with -) are never prefix-expanded -- pass through as-is.
    // We still walk into the trie if the flag is an EXACT match (so words
    // after the flag can still be resolved), but we never expand a flag.
    if word.starts_with('-') {
        if let Some(exact_node) = start_node.get_child(word) {
            result.push(word.to_string());
            if !rest.is_empty() && !exact_node.children.is_empty() {
                return resolve_from_node(rest, exact_node, result);
            }
        } else {
            result.push(word.to_string());
        }
        for w in rest {
            result.push(w.to_string());
        }
        return Ok(());
    }

    // Words with explicit path syntax (/, ~, .) always skip trie matching.
    if word.contains('/') || word.starts_with('~') || word.starts_with('.') {
        result.push(word.to_string());
        for w in rest {
            result.push(w.to_string());
        }
        return Ok(());
    }

    // Exact match handling: if the word is an exact child, use it --
    // UNLESS the exact node is a childless leaf that's also a strict prefix
    // of a longer sibling (ghost from history like "reb" alongside "rebuild").
    if let Some(exact_node) = start_node.get_child(word) {
        let dominated = exact_node.children.is_empty()
            && start_node
                .children
                .keys()
                .any(|k| k != word && k.starts_with(word));
        if !dominated {
            result.push(word.to_string());
            if !rest.is_empty() && !exact_node.children.is_empty() {
                return resolve_from_node(rest, exact_node, result);
            }
            for w in rest {
                result.push(w.to_string());
            }
            return Ok(());
        }
    }

    // For arguments (not the command itself): if this word matches a real
    // file or directory, skip trie prefix-matching and let the path resolver
    // handle it later.  This avoids expanding `te` to `terraform` when
    // there is a `tests/` directory right here.
    // Skip this for exec-only commands (which, man) -- their args are commands.
    let mode = if result.is_empty() {
        ArgMode::Normal
    } else {
        arg_mode(&result[0])
    };
    if !result.is_empty() && mode != ArgMode::ExecsOnly && has_filesystem_prefix_match(word) {
        result.push(word.to_string());
        for w in rest {
            result.push(w.to_string());
        }
        return Ok(());
    }

    let matches = start_node.prefix_search(word);

    match matches.len() {
        0 => {
            // No match in trie -- pass through (it's an argument, filename, etc.)
            result.push(word.to_string());
            for w in rest {
                result.push(w.to_string());
            }
            Ok(())
        }
        1 => {
            let (full_name, child_node) = matches[0];
            result.push(full_name.to_string());

            if !rest.is_empty() && !child_node.children.is_empty() {
                resolve_from_node(rest, child_node, result)
            } else {
                for w in rest {
                    result.push(w.to_string());
                }
                Ok(())
            }
        }
        _ => {
            // Ambiguous -- but try deep disambiguation first
            if !rest.is_empty() && !rest[0].starts_with('-') {
                let deep = deep_disambiguate(&matches, rest[0]);

                if deep.len() == 1 {
                    // Deep disambiguation resolved it
                    let (full_name, child_node) = deep[0];
                    result.push(full_name.to_string());

                    if !child_node.children.is_empty() {
                        return resolve_from_node(rest, child_node, result);
                    } else {
                        for w in rest {
                            result.push(w.to_string());
                        }
                        return Ok(());
                    }
                }

                // Build deep candidate info for the ambiguity report
                let deep_candidates: Vec<DeepCandidate> = matches
                    .iter()
                    .filter_map(|(name, node)| {
                        let sub_matches: Vec<String> = node
                            .prefix_search(rest[0])
                            .iter()
                            .map(|(s, _)| s.to_string())
                            .collect();
                        if sub_matches.is_empty() {
                            None
                        } else {
                            Some(DeepCandidate {
                                command: name.to_string(),
                                subcommand_matches: sub_matches,
                            })
                        }
                    })
                    .collect();

                let cands: Vec<String> = matches.iter().map(|(s, _)| s.to_string()).collect();
                let lcp = longest_common_prefix(&cands);
                Err(Box::new(AmbiguityInfo {
                    word: word.to_string(),
                    position: result.len(),
                    candidates: cands,
                    lcp,
                    deep_candidates,
                    resolved_prefix: result.clone(),
                    remaining: rest.iter().map(|s| s.to_string()).collect(),
                }))
            } else {
                let cands: Vec<String> = matches.iter().map(|(s, _)| s.to_string()).collect();
                let lcp = longest_common_prefix(&cands);
                Err(Box::new(AmbiguityInfo {
                    word: word.to_string(),
                    position: result.len(),
                    candidates: cands,
                    lcp,
                    deep_candidates: vec![],
                    resolved_prefix: result.clone(),
                    remaining: rest.iter().map(|s| s.to_string()).collect(),
                }))
            }
        }
    }
}

/// Compute the longest common prefix of a list of strings.
fn longest_common_prefix(strings: &[String]) -> String {
    if strings.is_empty() {
        return String::new();
    }
    let first = &strings[0];
    let mut len = first.len();
    for s in &strings[1..] {
        len = len.min(s.len());
        for (i, (a, b)) in first.bytes().zip(s.bytes()).enumerate() {
            if a != b {
                len = len.min(i);
                break;
            }
        }
    }
    first[..len].to_string()
}

/// Given multiple matches for a word, check which ones have children matching
/// the next word. Returns the filtered matches.
fn deep_disambiguate<'a>(
    matches: &[(&'a str, &'a TrieNode)],
    next_word: &str,
) -> Vec<(&'a str, &'a TrieNode)> {
    matches
        .iter()
        .filter(|(_, node)| !node.prefix_search(next_word).is_empty())
        .copied()
        .collect()
}

enum PathsResult {
    Resolved(String),
    Ambiguous(Vec<String>),
}

/// After command resolution, resolve any path arguments against the filesystem.
fn resolve_paths_in_words(words: &[String], mode: ArgMode) -> PathsResult {
    let mut result: Vec<String> = Vec::new();
    for (i, word) in words.iter().enumerate() {
        // Skip path resolution for exec-only commands (which, man, etc.)
        if i > 0 && !word.starts_with('-') && mode != ArgMode::ExecsOnly {
            let path_result = match mode {
                ArgMode::DirsOnly => path_resolve::resolve_path_dirs_only(word),
                _ => path_resolve::resolve_path(word),
            };
            match path_result {
                path_resolve::PathResult::Resolved(resolved) => {
                    result.push(shell_escape_path(&resolved));
                }
                path_resolve::PathResult::Ambiguous(candidates) => {
                    let prefix: Vec<String> = result.clone();
                    let suffix: Vec<String> = words[i + 1..].to_vec();
                    let full_cmds: Vec<String> = candidates
                        .into_iter()
                        .map(|c| {
                            let mut parts = prefix.clone();
                            parts.push(shell_escape_path(&c));
                            parts.extend(suffix.clone());
                            parts.join(" ")
                        })
                        .collect();
                    return PathsResult::Ambiguous(full_cmds);
                }
                path_resolve::PathResult::Unchanged => {
                    result.push(word.clone());
                }
            }
        } else {
            result.push(word.clone());
        }
    }
    PathsResult::Resolved(result.join(" "))
}

/// Escape spaces (and other shell-sensitive chars) in resolved paths.
fn shell_escape_path(path: &str) -> String {
    if !path.contains(' ') && !path.contains('(') && !path.contains(')') {
        return path.to_string();
    }
    path.replace(' ', "\\ ")
        .replace('(', "\\(")
        .replace(')', "\\)")
}

/// How a command's arguments should be resolved.
#[derive(Clone, Copy, PartialEq, Eq)]
enum ArgMode {
    /// Trie resolution + filesystem path resolution (default).
    Normal,
    /// Arguments are filesystem paths (files and directories).
    /// Skips trie, resolves against the filesystem.
    Paths,
    /// Arguments are directory paths only (e.g. cd, pushd).
    /// Skips trie, resolves against directories on the filesystem.
    DirsOnly,
    /// Arguments are command / executable names (e.g. which, man).
    /// Keeps trie resolution, skips filesystem path resolution.
    ExecsOnly,
}

/// Classify a command by how its arguments should be resolved.
/// Add new entries here to teach zsh-ios about more commands.
fn arg_mode(cmd: &str) -> ArgMode {
    match cmd {
        // Directory-only arguments
        "cd" | "pushd" => ArgMode::DirsOnly,

        // Filesystem path arguments
        "ls" | "rm" | "rmdir" | "mkdir" | "cp" | "mv" | "ln"
        | "cat" | "less" | "more" | "head" | "tail" | "wc"
        | "touch" | "chmod" | "chown" | "chgrp" | "stat" | "file"
        | "readlink" | "realpath" | "basename" | "dirname"
        | "du" | "find" | "diff" | "patch"
        | "tar" | "zip" | "unzip" | "gzip" | "gunzip" | "bzip2" | "xz"
        | "source" | "open"
        | "nano" | "vim" | "vi" | "nvim" | "emacs" | "code" | "bat"
        => ArgMode::Paths,

        // Executable / command-name arguments
        "which" | "type" | "whence" | "where" | "command" | "man" | "rehash"
        => ArgMode::ExecsOnly,

        _ => ArgMode::Normal,
    }
}

/// Check the actual filesystem: does any entry in cwd start with this word?
fn has_filesystem_prefix_match(word: &str) -> bool {
    if word.is_empty() {
        return false;
    }
    let cwd = match std::env::current_dir() {
        Ok(d) => d,
        Err(_) => return false,
    };
    match std::fs::read_dir(&cwd) {
        Ok(entries) => entries.flatten().any(|e| {
            let name = e.file_name();
            name.to_string_lossy().starts_with(word)
        }),
        Err(_) => false,
    }
}

/// Generate completions for the `?` command.
/// Returns a formatted list of matching commands/subcommands.
pub fn complete(input: &str, trie: &CommandTrie, pins: &Pins) -> String {
    let words: Vec<&str> = input.split_whitespace().collect();
    let mut output = String::new();

    if words.is_empty() {
        // Show top-level commands sorted by usage count
        let mut entries: Vec<(&str, &TrieNode)> = trie.root.prefix_search("");
        entries.sort_by(|a, b| b.1.count.cmp(&a.1.count));
        for (name, node) in entries.iter().take(40) {
            if node.count > 0 {
                output.push_str(&format!("  {:<24} ({} uses)\n", name, node.count));
            } else {
                output.push_str(&format!("  {}\n", name));
            }
        }
        if entries.len() > 40 {
            output.push_str(&format!("  ... and {} more\n", entries.len() - 40));
        }
        return output;
    }

    // Check pins first
    let pin_result = pins.longest_match(&words);
    let (pin_consumed, expanded_prefix) = match pin_result {
        Some((consumed, expanded)) => (consumed, expanded),
        None => (0, vec![]),
    };

    if pin_consumed >= words.len() {
        // All words consumed by pin -- show subcommands of the expanded command
        let mut node = &trie.root;
        for w in &expanded_prefix {
            match node.get_child(w) {
                Some(child) => node = child,
                None => return format!("  {} (pin, no subcommands in tree)\n", expanded_prefix.join(" ")),
            }
        }
        output.push_str(&format!("  {} (pinned)\n", expanded_prefix.join(" ")));
        if !node.children.is_empty() {
            output.push_str("  Subcommands:\n");
            for (name, child) in &node.children {
                if child.count > 0 {
                    output.push_str(&format!("    {:<20} ({} uses)\n", name, child.count));
                } else {
                    output.push_str(&format!("    {}\n", name));
                }
            }
        }
        return output;
    }

    // Walk the trie to the current position
    let resolve_start = if pin_consumed > 0 { pin_consumed } else { 0 };
    let mut node = &trie.root;

    // If pin matched, walk the trie to the expanded prefix position
    if pin_consumed > 0 {
        for w in &expanded_prefix {
            match node.get_child(w) {
                Some(child) => node = child,
                None => {
                    return format!("  {} (pin target not in tree)\n", expanded_prefix.join(" "));
                }
            }
        }
    }

    // Resolve words before the last one, drilling into the trie
    for word in &words[resolve_start..words.len().saturating_sub(1)] {
        // Prefer exact match, then unique prefix match
        if let Some(child) = node.get_child(word) {
            node = child;
            continue;
        }
        let matches = node.prefix_search(word);
        match matches.len() {
            1 => node = matches[0].1,
            0 => {
                output.push_str(&format!("  No commands matching \"{}\"\n", word));
                return output;
            }
            _ => {
                for (name, child) in &matches {
                    if child.count > 0 {
                        output.push_str(&format!("  {:<24} ({} uses)\n", name, child.count));
                    } else {
                        output.push_str(&format!("  {}\n", name));
                    }
                }
                return output;
            }
        }
    }

    // Show matches for the last word (or all if the last word is the query)
    let last_word = words.last().unwrap_or(&"");
    let matches = node.prefix_search(last_word);

    if matches.is_empty() {
        output.push_str(&format!("  No commands matching \"{}\"\n", last_word));
    } else {
        for (name, child) in &matches {
            if child.count > 0 {
                output.push_str(&format!("  {:<24} ({} uses)\n", name, child.count));
            } else {
                output.push_str(&format!("  {}\n", name));
            }
            // Show subcommands if available
            if !child.children.is_empty() {
                let subs: Vec<&str> = child.children.keys().map(|s| s.as_str()).collect();
                if subs.len() <= 8 {
                    output.push_str(&format!("    -> {}\n", subs.join("  ")));
                } else {
                    let shown: Vec<&str> = subs.iter().take(8).copied().collect();
                    output.push_str(&format!(
                        "    -> {}  ... +{} more\n",
                        shown.join("  "),
                        subs.len() - 8
                    ));
                }
            }
        }
    }

    output
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pins::Pin;

    fn build_test_trie() -> CommandTrie {
        let mut trie = CommandTrie::new();
        trie.insert(&["git", "checkout", "main"]);
        trie.insert(&["git", "checkout", "develop"]);
        trie.insert(&["git", "commit", "-m"]);
        trie.insert(&["git", "push"]);
        trie.insert(&["grep", "-r", "pattern"]);
        trie.insert(&["go", "build"]);
        trie.insert(&["terraform", "apply"]);
        trie.insert(&["terraform", "destroy"]);
        trie.insert(&["terraform", "init"]);
        trie.insert(&["terraform", "plan"]);
        trie.insert_command("gzip");
        trie
    }

    #[test]
    fn test_unambiguous_resolve() {
        let trie = build_test_trie();
        let pins = Pins::default();

        match resolve("ter ap", &trie, &pins) {
            ResolveResult::Resolved(s) => assert_eq!(s, "terraform apply"),
            other => panic!("Expected Resolved, got {:?}", other),
        }

        match resolve("ter pl", &trie, &pins) {
            ResolveResult::Resolved(s) => assert_eq!(s, "terraform plan"),
            other => panic!("Expected Resolved, got {:?}", other),
        }
    }

    #[test]
    fn test_ambiguous_first_word() {
        let trie = build_test_trie();
        let pins = Pins::default();

        // Bare "g" with no further words to disambiguate
        match resolve("g", &trie, &pins) {
            ResolveResult::Ambiguous(info) => {
                assert_eq!(info.word, "g");
                assert!(info.candidates.contains(&"git".to_string()));
                assert!(info.candidates.contains(&"grep".to_string()));
                assert!(info.candidates.contains(&"go".to_string()));
                assert!(info.candidates.contains(&"gzip".to_string()));
            }
            other => panic!("Expected Ambiguous, got {:?}", other),
        }
    }

    #[test]
    fn test_deep_disambig_resolves_g_push() {
        let trie = build_test_trie();
        let pins = Pins::default();

        // "g push" -- only git has a "push" subcommand, so deep disambig resolves it
        match resolve("g push", &trie, &pins) {
            ResolveResult::Resolved(s) => assert_eq!(s, "git push"),
            other => panic!("Expected Resolved via deep disambig, got {:?}", other),
        }
    }

    #[test]
    fn test_deep_disambiguation() {
        let trie = build_test_trie();
        let pins = Pins::default();

        // "g ch" -- only git has a subcommand starting with "ch" (checkout)
        match resolve("g ch main", &trie, &pins) {
            ResolveResult::Resolved(s) => assert_eq!(s, "git checkout main"),
            ResolveResult::Ambiguous(info) => {
                // Deep disambiguation should narrow to git
                assert!(!info.deep_candidates.is_empty());
                assert_eq!(info.deep_candidates.len(), 1);
                assert_eq!(info.deep_candidates[0].command, "git");
                panic!("Should have resolved via deep disambiguation");
            }
            other => panic!("Expected Resolved via deep disambig, got {:?}", other),
        }
    }

    #[test]
    fn test_pin_resolution() {
        let trie = build_test_trie();
        let pins = Pins {
            entries: vec![
                Pin {
                    abbrev: vec!["g".into(), "ch".into()],
                    expanded: vec!["git".into(), "checkout".into()],
                },
            ],
        };

        match resolve("g ch develop", &trie, &pins) {
            ResolveResult::Resolved(s) => assert_eq!(s, "git checkout develop"),
            other => panic!("Expected Resolved, got {:?}", other),
        }
    }

    #[test]
    fn test_passthrough() {
        let trie = build_test_trie();
        let pins = Pins::default();

        match resolve("terraform apply", &trie, &pins) {
            ResolveResult::Passthrough(s) => assert_eq!(s, "terraform apply"),
            ResolveResult::Resolved(s) => {
                // Also acceptable if it resolves to the same thing
                assert_eq!(s, "terraform apply");
            }
            other => panic!("Expected Passthrough, got {:?}", other),
        }
    }

    #[test]
    fn test_flags_passthrough() {
        let trie = build_test_trie();
        let pins = Pins::default();

        match resolve("ter ap --auto-approve", &trie, &pins) {
            ResolveResult::Resolved(s) => assert_eq!(s, "terraform apply --auto-approve"),
            other => panic!("Expected Resolved, got {:?}", other),
        }
    }

    #[test]
    fn test_cd_skips_trie() {
        let mut trie = CommandTrie::new();
        trie.insert_command("cd");
        trie.insert(&["cd", "terraform"]);
        trie.insert(&["cd", "tests"]);
        trie.insert_command("terraform");
        let pins = Pins::default();

        // "cd te" should NOT return trie-level Ambiguous with executables
        match resolve("cd te", &trie, &pins) {
            ResolveResult::Ambiguous(info) => {
                panic!(
                    "cd args should skip trie resolution, got ambiguous: {:?}",
                    info.candidates
                );
            }
            _ => {} // Passthrough, Resolved, or PathAmbiguous are all acceptable
        }
    }

    #[test]
    fn test_pushd_skips_trie() {
        let mut trie = CommandTrie::new();
        trie.insert_command("pushd");
        trie.insert(&["pushd", "projects"]);
        trie.insert(&["pushd", "pictures"]);
        let pins = Pins::default();

        match resolve("pushd pro", &trie, &pins) {
            ResolveResult::Ambiguous(_) => {
                panic!("pushd args should skip trie resolution");
            }
            _ => {}
        }
    }

    #[test]
    fn test_ls_skips_trie() {
        let mut trie = CommandTrie::new();
        trie.insert_command("ls");
        trie.insert(&["ls", "terraform"]);
        trie.insert(&["ls", "tests"]);
        trie.insert_command("terraform");
        let pins = Pins::default();

        // "ls te" should NOT produce trie-level Ambiguous
        match resolve("ls te", &trie, &pins) {
            ResolveResult::Ambiguous(info) => {
                panic!(
                    "ls args should skip trie, got ambiguous: {:?}",
                    info.candidates
                );
            }
            _ => {}
        }
    }

    #[test]
    fn test_which_keeps_trie() {
        let mut trie = CommandTrie::new();
        trie.insert_command("which");
        trie.insert(&["which", "terraform"]);
        trie.insert(&["which", "git"]);
        let pins = Pins::default();

        // "which ter" should resolve via the trie to "which terraform"
        match resolve("which ter", &trie, &pins) {
            ResolveResult::Resolved(s) => assert_eq!(s, "which terraform"),
            ResolveResult::Passthrough(s) => assert_eq!(s, "which terraform"),
            other => panic!("Expected which to resolve via trie, got {:?}", other),
        }
    }

    #[test]
    fn test_ambiguity_lcp() {
        let trie = build_test_trie();
        let pins = Pins::default();

        match resolve("g", &trie, &pins) {
            ResolveResult::Ambiguous(info) => {
                // candidates: git, grep, go, gzip -- LCP is "g"
                assert_eq!(info.lcp, "g");
            }
            other => panic!("Expected Ambiguous, got {:?}", other),
        }

        // Add ansible-like commands for a more meaningful LCP
        let mut trie2 = CommandTrie::new();
        trie2.insert_command("ansible");
        trie2.insert_command("ansible-community");
        trie2.insert_command("ansible-config");
        trie2.insert_command("ansible-console");
        trie2.insert_command("ansible-doc");
        trie2.insert_command("ansible-galaxy");
        trie2.insert_command("ansible-inventory");
        trie2.insert_command("ansible-playbook");
        trie2.insert_command("ansible-pull");
        trie2.insert_command("ansible-test");
        trie2.insert_command("ansible-vault");

        match resolve("ansib", &trie2, &pins) {
            ResolveResult::Ambiguous(info) => {
                assert_eq!(info.lcp, "ansible");
            }
            other => panic!("Expected Ambiguous, got {:?}", other),
        }
    }
}
