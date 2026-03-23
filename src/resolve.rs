use crate::path_resolve;
use crate::pins::Pins;
use crate::runtime_complete;
use crate::trie::{self, ArgModeMap, ArgSpec, CommandTrie, TrieNode};

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

/// Resolve a full command line, splitting on pipes and chain operators.
///
/// Handles `|`, `||`, `&&`, and `;`.  Each segment is resolved independently;
/// the first ambiguity encountered is returned so the caller can prompt.
pub fn resolve_line(input: &str, trie: &CommandTrie, pins: &Pins) -> ResolveResult {
    let parts = split_on_operators(input);

    // Fast path: no operators → resolve as a single command.
    let has_op = parts.iter().any(|p| matches!(p, LinePart::Operator(_)));
    if !has_op {
        return resolve(input, trie, pins);
    }

    let mut resolved: Vec<String> = Vec::new();
    let mut any_changed = false;
    let mut word_offset: usize = 0;

    for part in &parts {
        match part {
            LinePart::Operator(op) => {
                resolved.push(op.clone());
                word_offset += 1; // operators are one word in zsh's (z) split
            }
            LinePart::Command(cmd) => {
                let trimmed = cmd.trim();
                if trimmed.is_empty() {
                    resolved.push(String::new());
                    continue;
                }

                match resolve(trimmed, trie, pins) {
                    ResolveResult::Resolved(r) => {
                        word_offset += r.split_whitespace().count();
                        resolved.push(r);
                        any_changed = true;
                    }
                    ResolveResult::Passthrough(p) => {
                        word_offset += p.split_whitespace().count();
                        resolved.push(p);
                    }
                    ResolveResult::Ambiguous(mut info) => {
                        info.position += word_offset;
                        let prefix_so_far = resolved.join(" ");
                        if !prefix_so_far.trim().is_empty() {
                            let mut full_prefix: Vec<String> =
                                prefix_so_far.split_whitespace().map(String::from).collect();
                            full_prefix.extend(info.resolved_prefix);
                            info.resolved_prefix = full_prefix;
                        }
                        return ResolveResult::Ambiguous(info);
                    }
                    ResolveResult::PathAmbiguous(candidates) => {
                        let prefix_so_far = resolved.join(" ");
                        if prefix_so_far.trim().is_empty() {
                            return ResolveResult::PathAmbiguous(candidates);
                        }
                        let adjusted: Vec<String> = candidates
                            .into_iter()
                            .map(|c| format!("{} {}", prefix_so_far.trim(), c))
                            .collect();
                        return ResolveResult::PathAmbiguous(adjusted);
                    }
                }
            }
        }
    }

    let result = resolved.join(" ");
    if any_changed && result != input {
        ResolveResult::Resolved(result)
    } else {
        ResolveResult::Passthrough(input.to_string())
    }
}

/// Resolve a single command segment (no pipes/chains) against the trie and pins.
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
                    return finalize_with_paths(input, result_words, trie);
                }
            }
        }

        let mut result_words = expanded_prefix;
        match resolve_from_node(remaining_words, node, &mut result_words, &trie.arg_modes) {
            Ok(()) => finalize_with_paths(input, result_words, trie),
            Err(ambiguity) => ResolveResult::Ambiguous(*ambiguity),
        }
    } else {
        let mut result_words: Vec<String> = Vec::new();
        match resolve_from_node(&words, &trie.root, &mut result_words, &trie.arg_modes) {
            Ok(()) => finalize_with_paths(input, result_words, trie),
            Err(ambiguity) => ResolveResult::Ambiguous(*ambiguity),
        }
    }
}

// --- Pipe / chain splitting ---

enum LinePart {
    Command(String),
    Operator(String),
}

/// Split a command line on `|`, `||`, `&&`, `;` while respecting quotes.
fn split_on_operators(input: &str) -> Vec<LinePart> {
    let mut parts = Vec::new();
    let mut start = 0;
    let mut in_sq = false;
    let mut in_dq = false;
    let bytes = input.as_bytes();
    let mut i = 0;

    while i < bytes.len() {
        match bytes[i] {
            b'\'' if !in_dq => in_sq = !in_sq,
            b'"' if !in_sq => in_dq = !in_dq,
            b'\\' if !in_sq => {
                i += 1; // skip escaped char
            }
            b'|' if !in_sq && !in_dq => {
                parts.push(LinePart::Command(input[start..i].to_string()));
                if i + 1 < bytes.len() && bytes[i + 1] == b'|' {
                    parts.push(LinePart::Operator("||".to_string()));
                    i += 1;
                } else {
                    parts.push(LinePart::Operator("|".to_string()));
                }
                start = i + 1;
            }
            b'&' if !in_sq && !in_dq => {
                if i + 1 < bytes.len() && bytes[i + 1] == b'&' {
                    parts.push(LinePart::Command(input[start..i].to_string()));
                    parts.push(LinePart::Operator("&&".to_string()));
                    i += 1;
                    start = i + 1;
                }
            }
            b';' if !in_sq && !in_dq => {
                parts.push(LinePart::Command(input[start..i].to_string()));
                parts.push(LinePart::Operator(";".to_string()));
                start = i + 1;
            }
            _ => {}
        }
        i += 1;
    }

    parts.push(LinePart::Command(input[start..].to_string()));
    parts
}

fn finalize_with_paths(input: &str, words: Vec<String>, trie: &CommandTrie) -> ResolveResult {
    // Look up per-position ArgSpec: try "cmd subcmd" first, then "cmd"
    let (spec, cmd_words) = lookup_arg_spec(&words, &trie.arg_specs);
    let fallback_mode = words
        .first()
        .map(|w| arg_mode(w, &trie.arg_modes))
        .unwrap_or(ArgMode::Normal);
    match resolve_paths_in_words(&words, spec, fallback_mode, cmd_words) {
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
    modes: &ArgModeMap,
) -> Result<(), Box<AmbiguityInfo>> {
    if words.is_empty() {
        return Ok(());
    }

    // For path/dir/runtime commands, skip trie resolution for arguments --
    // they'll be resolved by the path resolver or runtime resolver later.
    if !result.is_empty() {
        let mode = arg_mode(&result[0], modes);
        if matches!(
            mode,
            ArgMode::DirsOnly | ArgMode::Paths | ArgMode::Runtime(_)
        ) {
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
                return resolve_from_node(rest, exact_node, result, modes);
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

    // Exact match always wins -- if the word is an exact child, use it.
    // Ghost prevention (e.g. "reb" alongside "rebuild") is handled upstream
    // by the prefix guard in history learning, so ghosts never enter the trie.
    if let Some(exact_node) = start_node.get_child(word) {
        result.push(word.to_string());
        if !rest.is_empty() && !exact_node.children.is_empty() {
            return resolve_from_node(rest, exact_node, result, modes);
        }
        for w in rest {
            result.push(w.to_string());
        }
        return Ok(());
    }

    // For arguments (not the command itself): if this word matches a real
    // file or directory, skip trie prefix-matching and let the path resolver
    // handle it later.  This avoids expanding `te` to `terraform` when
    // there is a `tests/` directory right here.
    // Skip this for exec-only commands (which, man) -- their args are commands.
    let mode = if result.is_empty() {
        ArgMode::Normal
    } else {
        arg_mode(&result[0], modes)
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
                resolve_from_node(rest, child_node, result, modes)
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
                        return resolve_from_node(rest, child_node, result, modes);
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
/// Check whether a word looks like an explicit path reference.
/// Used in Normal mode to avoid resolving bare words like "clau" against
/// the filesystem -- only words that syntactically reference a path.
fn looks_like_path(word: &str) -> bool {
    word.contains('/')
        || word.starts_with('~')
        || word.starts_with('.')
        || word.starts_with('!')
        || word.starts_with('*')
        || word.starts_with("\\!")
        || word.starts_with("\\*")
}

/// Look up an ArgSpec for the resolved command, trying the most specific
/// key first: "cmd subcmd", then "cmd".
/// Returns (spec, skip_words) where skip_words is how many words form the
/// command prefix (1 for "cmd", 2 for "cmd subcmd").
fn lookup_arg_spec<'a>(
    words: &[String],
    specs: &'a trie::ArgSpecMap,
) -> (Option<&'a ArgSpec>, usize) {
    // Try "cmd subcmd" (e.g., "git add")
    if words.len() >= 2 && !words[1].starts_with('-') {
        let key = format!("{} {}", words[0], words[1]);
        if let Some(spec) = specs.get(&key) {
            return (Some(spec), 2);
        }
    }
    // Fall back to "cmd"
    if !words.is_empty()
        && let Some(spec) = specs.get(&words[0])
    {
        return (Some(spec), 1);
    }
    (None, 1)
}

/// Determine the ArgMode for a specific argument word, given its position,
/// the preceding word (for flag-value detection), the ArgSpec, and the
/// fallback whole-command mode.
fn arg_type_for_word(
    arg_position: u32,
    prev_word: Option<&str>,
    spec: Option<&ArgSpec>,
    fallback: ArgMode,
) -> ArgMode {
    if let Some(spec) = spec {
        // Check if this word is the value of a flag from the previous word
        if let Some(prev) = prev_word
            && prev.starts_with('-')
            && let Some(flag_type) = spec.type_after_flag(prev)
        {
            return u8_to_arg_mode(flag_type);
        }

        // Check per-position spec
        if let Some(pos_type) = spec.type_at(arg_position) {
            return u8_to_arg_mode(pos_type);
        }
    }

    // Fall back to whole-command mode
    fallback
}

fn u8_to_arg_mode(val: u8) -> ArgMode {
    match val {
        0 => ArgMode::Normal,
        trie::ARG_MODE_PATHS => ArgMode::Paths,
        trie::ARG_MODE_DIRS_ONLY => ArgMode::DirsOnly,
        trie::ARG_MODE_EXECS_ONLY => ArgMode::ExecsOnly,
        other => ArgMode::Runtime(other),
    }
}

fn resolve_paths_in_words(
    words: &[String],
    spec: Option<&ArgSpec>,
    fallback_mode: ArgMode,
    cmd_words: usize,
) -> PathsResult {
    let mut result: Vec<String> = Vec::new();
    let mut arg_position: u32 = 0; // 1-indexed position of non-flag arguments after the command prefix

    for (i, word) in words.iter().enumerate() {
        // Skip command prefix words (e.g., "git" or "git add")
        if i < cmd_words {
            result.push(word.clone());
            continue;
        }

        if word.starts_with('-') {
            result.push(word.clone());
            continue;
        }

        arg_position += 1;
        let prev_word = if i > 0 {
            Some(words[i - 1].as_str())
        } else {
            None
        };
        let mode = arg_type_for_word(arg_position, prev_word, spec, fallback_mode);

        match mode {
            // Runtime-resolved types: users, hosts, signals, git branches, etc.
            ArgMode::Runtime(type_id) => {
                if let Some(resolved) = runtime_complete::resolve_prefix(type_id, word) {
                    result.push(resolved);
                } else {
                    result.push(word.clone());
                }
            }

            // Filesystem path resolution
            ArgMode::Paths | ArgMode::DirsOnly => {
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
            }

            // ExecsOnly: no path resolution, already handled by trie walk
            ArgMode::ExecsOnly => {
                result.push(word.clone());
            }

            // Normal: only resolve path-like words against the filesystem
            ArgMode::Normal => {
                if looks_like_path(word) {
                    match path_resolve::resolve_path(word) {
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
    Paths,
    /// Arguments are directory paths only (e.g. cd, pushd).
    DirsOnly,
    /// Arguments are command / executable names (e.g. which, man).
    ExecsOnly,
    /// Runtime-resolved type (users, hosts, signals, git branches, etc.).
    /// The u8 is the original arg type constant from trie.rs.
    Runtime(u8),
}

/// Classify a command by how its arguments should be resolved.
///
/// Checks the auto-detected arg modes from Zsh completions first,
/// then falls back to a hardcoded list for common commands.
fn arg_mode(cmd: &str, modes: &ArgModeMap) -> ArgMode {
    // Check auto-detected modes from Zsh completion files.
    // Only trust the basic three modes (1-3); Runtime types (4+) from Zsh
    // completions are per-position specs, not command-level arg modes — fall
    // through to the hardcoded list so ls/cat/nano still get ArgMode::Paths.
    if let Some(&mode) = modes.get(cmd) {
        match mode {
            trie::ARG_MODE_DIRS_ONLY => return ArgMode::DirsOnly,
            trie::ARG_MODE_PATHS => return ArgMode::Paths,
            trie::ARG_MODE_EXECS_ONLY => return ArgMode::ExecsOnly,
            _ => {} // Runtime types (4+): fall through to hardcoded list below
        }
    }

    // Hardcoded fallback for commands without Zsh completions
    match cmd {
        "cd" | "pushd" => ArgMode::DirsOnly,

        "ls" | "rm" | "rmdir" | "mkdir" | "cp" | "mv" | "ln" | "cat" | "less" | "more" | "head"
        | "tail" | "wc" | "touch" | "chmod" | "chown" | "chgrp" | "stat" | "file" | "readlink"
        | "realpath" | "basename" | "dirname" | "du" | "find" | "diff" | "patch" | "tar"
        | "zip" | "unzip" | "gzip" | "gunzip" | "bzip2" | "xz" | "source" | "open" | "nano"
        | "vim" | "vi" | "nvim" | "emacs" | "code" | "bat" => ArgMode::Paths,

        "which" | "type" | "whence" | "where" | "command" | "man" | "rehash" => ArgMode::ExecsOnly,

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
/// Splits on pipe/chain operators and completes only the last segment.
pub fn complete(input: &str, trie: &CommandTrie, pins: &Pins) -> String {
    // Use only the last segment after any pipe/chain operator.
    // Preserve trailing whitespace — it tells complete_segment whether the user
    // has finished the current word (trailing space) or is still typing it.
    let parts = split_on_operators(input);
    let last_cmd = parts
        .iter()
        .rev()
        .find_map(|p| match p {
            LinePart::Command(c) => Some(c.trim_start()),
            _ => None,
        })
        .unwrap_or(input.trim_start());

    complete_segment(last_cmd, trie, pins)
}

fn complete_segment(input: &str, trie: &CommandTrie, pins: &Pins) -> String {
    let words: Vec<&str> = input.split_whitespace().collect();
    // Trailing whitespace means the user has finished the current word and is
    // starting a new one — the completion prefix for the next word is empty.
    let completing_next = input.ends_with(' ') || input.ends_with('\t');
    let mut output = String::new();

    if words.is_empty() {
        // Show top-level commands sorted by usage count
        let mut entries: Vec<(&str, &TrieNode)> = trie.root.prefix_search("");
        entries.sort_by(|a, b| b.1.count.cmp(&a.1.count).then(a.0.cmp(b.0)));
        let names: Vec<&str> = entries.iter().map(|(n, _)| *n).collect();
        output.push_str(&format_columns(&names, 80));
        return output;
    }

    // Determine which words are "completed" (fully typed) vs the prefix being completed.
    // completed_words: words that are done (user has moved past them)
    // prefix: the partial word being completed (empty if user is starting a fresh word)
    let (completed_words, prefix): (Vec<&str>, &str) = if completing_next {
        (words.clone(), "")
    } else if words.len() == 1 {
        // Single word, no trailing space → completing at root level
        (vec![], words[0])
    } else {
        (words[..words.len() - 1].to_vec(), words[words.len() - 1])
    };

    // If no completed words, just search the root
    if completed_words.is_empty() {
        let mut matches = trie.root.prefix_search(prefix);
        if matches.is_empty() {
            output.push_str(&format!("  No commands matching \"{}\"\n", prefix));
        } else {
            matches.sort_by(|a, b| b.1.count.cmp(&a.1.count).then(a.0.cmp(b.0)));
            let names: Vec<&str> = matches.iter().map(|(n, _)| *n).collect();
            output.push_str(&format_columns(&names, 80));
        }
        return output;
    }

    // Resolve completed words to build the resolved command prefix and walk trie
    let mut resolved_words: Vec<String> = Vec::new();
    let resolved_cmd = resolve_first_word(completed_words[0], trie);
    resolved_words.push(resolved_cmd.clone());

    // Check pins first
    let pin_result = pins.longest_match(&completed_words);
    let (pin_consumed, expanded_prefix) = match pin_result {
        Some((consumed, expanded)) => (consumed, expanded),
        None => (0, vec![]),
    };

    // Walk the trie through completed words
    let mut node = &trie.root;
    let resolve_start;

    if pin_consumed > 0 {
        resolved_words = expanded_prefix.clone();
        for w in &expanded_prefix {
            match node.get_child(w) {
                Some(child) => node = child,
                None => break,
            }
        }
        resolve_start = pin_consumed;
    } else {
        if let Some(child) = node.get_child(&resolved_cmd) {
            node = child;
        }
        resolve_start = 1;
    }

    // Walk remaining completed words
    for word in &completed_words[resolve_start..] {
        if let Some(child) = node.get_child(word) {
            resolved_words.push(word.to_string());
            node = child;
            continue;
        }
        let matches = node.prefix_search(word);
        match matches.len() {
            1 => {
                resolved_words.push(matches[0].0.to_string());
                node = matches[0].1;
            }
            0 => {
                resolved_words.push(word.to_string());
                break;
            }
            _ => {
                // Intermediate word is ambiguous — show its completions
                let names: Vec<&str> = matches.iter().map(|(n, _)| *n).collect();
                output.push_str(&format_columns(&names, 80));
                return output;
            }
        }
    }

    // Determine the arg type at the current position
    let (spec, cmd_words) = lookup_arg_spec(
        &resolved_words.iter().map(String::from).collect::<Vec<_>>(),
        &trie.arg_specs,
    );
    let fallback_mode = arg_mode(&resolved_cmd, &trie.arg_modes);
    // Position of the word being completed (1-indexed)
    let total_words = completed_words.len() + 1; // completed + the word being typed
    let arg_position = total_words.saturating_sub(cmd_words).max(1) as u32;
    let prev_word = completed_words.last().copied();
    let current_mode = arg_type_for_word(arg_position, prev_word, spec, fallback_mode);

    // --- Flag completion mode ---
    // When typing a flag prefix (starts with '-'), show known flags + their expected arg types.
    if prefix.starts_with('-') {
        return complete_flags(prefix, spec, node, output);
    }

    // --- Trie-based completion (subcommands) ---
    let trie_matches = node.prefix_search(prefix);

    if trie_matches.is_empty() {
        // No trie matches — show type-aware completions based on arg spec
        show_type_completions(&mut output, current_mode, prefix, spec, arg_position);
    } else {
        // Separate subcommands from flags (flags from history are trie children too)
        let subcmds: Vec<(&str, &TrieNode)> = trie_matches
            .iter()
            .filter(|(n, _)| !n.starts_with('-'))
            .copied()
            .collect();
        let flag_matches: Vec<(&str, &TrieNode)> = trie_matches
            .iter()
            .filter(|(n, _)| n.starts_with('-'))
            .copied()
            .collect();

        if !subcmds.is_empty() {
            let mut sorted = subcmds.clone();
            sorted.sort_by(|a, b| b.1.count.cmp(&a.1.count).then(a.0.cmp(b.0)));
            let names: Vec<&str> = sorted.iter().map(|(n, _)| *n).collect();
            output.push_str(&format_columns(&names, 80));
        }

        if !flag_matches.is_empty() {
            output.push_str(&format_flags_from_trie(&flag_matches, spec));
        }

        // Type hint when completing next (empty prefix) and type is known
        if prefix.is_empty() && !matches!(current_mode, ArgMode::Normal | ArgMode::ExecsOnly) {
            let type_hint = match current_mode {
                ArgMode::DirsOnly => Some("<directory>"),
                ArgMode::Paths => Some("<file>"),
                ArgMode::Runtime(type_id) => Some(runtime_complete::type_hint(type_id)),
                _ => None,
            };
            if let Some(hint) = type_hint {
                output.push_str(&format!("  (also accepts: {})\n", hint));
            }
        }
    }

    output
}

/// Complete a flag prefix: show matching flags from spec and trie.
/// If the prefix exactly matches a single flag that takes an argument, show what it expects.
fn complete_flags(
    prefix: &str,
    spec: Option<&trie::ArgSpec>,
    node: &TrieNode,
    mut output: String,
) -> String {
    // Collect flags from ArgSpec (flags that take typed arguments)
    let mut known_flags: Vec<(String, Option<u8>)> = Vec::new();
    if let Some(spec) = spec {
        for (flag, &arg_type) in &spec.flag_args {
            if flag.starts_with(prefix) {
                known_flags.push((flag.clone(), Some(arg_type)));
            }
        }
    }

    // Collect flags from trie children (flags learned from history — may be boolean)
    let trie_flags: Vec<&str> = node
        .prefix_search(prefix)
        .into_iter()
        .filter(|(n, _)| n.starts_with('-'))
        .map(|(n, _)| n)
        .collect();
    for flag in &trie_flags {
        if !known_flags.iter().any(|(f, _)| f == flag) {
            known_flags.push((flag.to_string(), None));
        }
    }

    known_flags.sort_by(|a, b| a.0.cmp(&b.0));

    if known_flags.is_empty() {
        output.push_str(&format!("  No flags matching \"{}\"\n", prefix));
        return output;
    }

    // If exactly one match and it IS the prefix: flag is complete — show what it expects
    if known_flags.len() == 1 && known_flags[0].0 == prefix {
        if let Some(arg_type) = known_flags[0].1 {
            let hint = runtime_complete::type_hint(arg_type);
            output.push_str(&format!("  {} expects: {}\n", prefix, hint));
            let rt = runtime_complete::list_matches(arg_type, "");
            let names: Vec<&str> = rt.iter().map(String::as_str).collect();
            if !names.is_empty() {
                output.push_str(&format_columns(&names, 80));
            }
        } else {
            // Boolean flag, no argument
            output.push_str(&format!("  {} (no argument)\n", prefix));
        }
        return output;
    }

    // Multiple matches or partial: show flag names with their expected arg type
    let col_width = known_flags.iter().map(|(f, _)| f.len()).max().unwrap_or(0) + 2;
    for (flag, arg_type) in &known_flags {
        if let Some(at) = arg_type {
            let hint = runtime_complete::type_hint(*at);
            output.push_str(&format!("  {:<width$}{}\n", flag, hint, width = col_width));
        } else {
            output.push_str(&format!("  {}\n", flag));
        }
    }
    output
}

/// Format flags from trie (history-learned) with their spec-derived arg type hints.
fn format_flags_from_trie(flags: &[(&str, &TrieNode)], spec: Option<&trie::ArgSpec>) -> String {
    let col_width = flags.iter().map(|(n, _)| n.len()).max().unwrap_or(0) + 2;
    let mut out = String::new();
    for (name, _) in flags {
        let hint = spec
            .and_then(|s| s.type_after_flag(name))
            .map(runtime_complete::type_hint);
        if let Some(hint) = hint {
            out.push_str(&format!("  {:<width$}{}\n", name, hint, width = col_width));
        } else {
            out.push_str(&format!("  {}\n", name));
        }
    }
    out
}

/// Detect the current terminal width via ioctl(TIOCGWINSZ).
/// Falls back to $COLUMNS, then 80.
fn terminal_width() -> usize {
    // Try ioctl on stderr (fd 2) — most likely to be a real tty even when
    // stdout/stdin are redirected (e.g. in a pipeline).
    #[cfg(unix)]
    {
        use std::os::unix::io::RawFd;

        #[repr(C)]
        struct Winsize {
            ws_row: u16,
            ws_col: u16,
            _ws_xpixel: u16,
            _ws_ypixel: u16,
        }

        // TIOCGWINSZ varies by platform
        #[cfg(target_os = "macos")]
        const TIOCGWINSZ: u64 = 0x4008_7468;
        #[cfg(not(target_os = "macos"))]
        const TIOCGWINSZ: u64 = 0x5413;

        // Try stderr (2), then stdout (1), then stdin (0)
        for fd in [2i32, 1, 0] as [RawFd; 3] {
            let mut ws = Winsize {
                ws_row: 0,
                ws_col: 0,
                _ws_xpixel: 0,
                _ws_ypixel: 0,
            };
            let ret = unsafe { libc_ioctl(fd, TIOCGWINSZ, &mut ws as *mut Winsize as *mut u8) };
            if ret == 0 && ws.ws_col > 0 {
                return ws.ws_col as usize;
            }
        }

        // ioctl failed (not a tty) — try $COLUMNS
        if let Some(w) = std::env::var("COLUMNS")
            .ok()
            .and_then(|c| c.parse::<usize>().ok())
        {
            return w.clamp(40, 500);
        }
    }
    #[cfg(not(unix))]
    {
        if let Some(w) = std::env::var("COLUMNS")
            .ok()
            .and_then(|c| c.parse::<usize>().ok())
        {
            return w.clamp(40, 500);
        }
    }
    80
}

#[cfg(unix)]
unsafe fn libc_ioctl(fd: i32, request: u64, arg: *mut u8) -> i32 {
    unsafe extern "C" {
        fn ioctl(fd: i32, request: u64, ...) -> i32;
    }
    unsafe { ioctl(fd, request, arg) }
}

/// Format a list of names into columns, capped at `max_items`.
/// Uses terminal width from COLUMNS env (default 80). Short lists use a single column.
fn format_columns(names: &[&str], max_items: usize) -> String {
    if names.is_empty() {
        return String::new();
    }

    let term_width = terminal_width();

    let total = names.len();
    let visible_count = total.min(max_items);
    let shown = &names[..visible_count];

    // Single-column for small lists (≤ 12 items)
    if shown.len() <= 12 {
        let mut out = String::new();
        for name in shown {
            out.push_str(&format!("  {}\n", name));
        }
        if total > max_items {
            out.push_str(&format!("  ... and {} more\n", total - max_items));
        }
        return out;
    }

    // Multi-column for larger lists
    let max_name_len = shown.iter().map(|s| s.len()).max().unwrap_or(0);
    let col_width = max_name_len + 2; // 2-space gap between columns
    // Account for the 2-space indent
    let usable_width = term_width.saturating_sub(2);
    let num_cols = (usable_width / col_width).clamp(1, 6);

    let rows = shown.len().div_ceil(num_cols);
    let mut out = String::new();

    for row in 0..rows {
        out.push_str("  ");
        for col in 0..num_cols {
            let idx = col * rows + row; // column-major (like `ls`)
            if idx >= shown.len() {
                break;
            }
            let is_last_in_row = col == num_cols - 1 || (col + 1) * rows + row >= shown.len();
            if is_last_in_row {
                out.push_str(shown[idx]);
            } else {
                out.push_str(&format!("{:<width$}", shown[idx], width = col_width));
            }
        }
        out.push('\n');
    }

    if total > max_items {
        out.push_str(&format!("  ... and {} more\n", total - max_items));
    }

    out
}

/// Show type-aware completions for a given arg mode and prefix.
fn show_type_completions(
    output: &mut String,
    mode: ArgMode,
    prefix: &str,
    spec: Option<&trie::ArgSpec>,
    arg_position: u32,
) {
    match mode {
        ArgMode::DirsOnly => {
            output.push_str("  Expects: <directory>\n");
            output.push_str(&complete_filesystem(prefix, true));
        }
        ArgMode::Paths => {
            output.push_str("  Expects: <file>\n");
            output.push_str(&complete_filesystem(prefix, false));
        }
        ArgMode::Runtime(type_id) => {
            let hint = runtime_complete::type_hint(type_id);
            output.push_str(&format!("  Expects: {}\n", hint));
            let rt = runtime_complete::list_matches(type_id, prefix);
            let names: Vec<&str> = rt.iter().map(String::as_str).collect();
            if names.is_empty() {
                if !prefix.is_empty() {
                    output.push_str(&format!("  No matches for \"{}\"\n", prefix));
                }
            } else {
                output.push_str(&format_columns(&names, 80));
            }
        }
        _ => {
            // Check spec for type hint even in Normal/ExecsOnly mode
            if let Some(spec) = spec
                && let Some(pos_type) = spec.type_at(arg_position)
                && pos_type != 0
            {
                let hint = runtime_complete::type_hint(pos_type);
                output.push_str(&format!("  Expects: {}\n", hint));
                let rt = runtime_complete::list_matches(pos_type, prefix);
                let names: Vec<&str> = rt.iter().map(String::as_str).collect();
                if !names.is_empty() {
                    output.push_str(&format_columns(&names, 80));
                    return;
                }
            }
            if prefix.is_empty() {
                output.push_str("  <enter argument>\n");
            } else {
                output.push_str(&format!("  No commands matching \"{}\"\n", prefix));
            }
        }
    }
}

/// Resolve just the first word of a command against the trie root.
fn resolve_first_word(word: &str, trie: &CommandTrie) -> String {
    if trie.root.get_child(word).is_some() {
        return word.to_string();
    }
    let matches = trie.root.prefix_search(word);
    if matches.len() == 1 {
        return matches[0].0.to_string();
    }
    word.to_string()
}

/// List filesystem entries for `?` completion in dir/path commands.
fn complete_filesystem(word: &str, dirs_only: bool) -> String {
    let cwd = std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from("."));

    let (search_dir, pattern) = if let Some((dir_part, comp)) = word.rsplit_once('/') {
        let dir = if let Some(rest) = dir_part.strip_prefix('~') {
            let home = dirs::home_dir().unwrap_or_default();
            let rest = rest.strip_prefix('/').unwrap_or(rest);
            if rest.is_empty() {
                home
            } else {
                home.join(rest)
            }
        } else if dir_part.is_empty() {
            std::path::PathBuf::from("/")
        } else {
            cwd.join(dir_part)
        };
        (dir, comp)
    } else {
        (cwd, word)
    };

    let mut entries: Vec<String> = match std::fs::read_dir(&search_dir) {
        Ok(rd) => rd
            .flatten()
            .filter(|e| !dirs_only || e.path().is_dir())
            .map(|e| e.file_name().to_string_lossy().to_string())
            .collect(),
        Err(_) => return "  (cannot read directory)\n".to_string(),
    };
    entries.sort();

    let filtered: Vec<&String> = if pattern.is_empty() {
        entries.iter().collect()
    } else if let Some(suffix) = pattern.strip_prefix('!') {
        if suffix.is_empty() {
            entries.iter().collect()
        } else {
            let lower = suffix.to_lowercase();
            entries
                .iter()
                .filter(|e| e.ends_with(suffix) || e.to_lowercase().ends_with(&lower))
                .collect()
        }
    } else if let Some(sub) = pattern.strip_prefix('*') {
        if sub.is_empty() {
            entries.iter().collect()
        } else {
            let lower = sub.to_lowercase();
            entries
                .iter()
                .filter(|e| e.contains(sub) || e.to_lowercase().contains(&lower))
                .collect()
        }
    } else {
        let lower = pattern.to_lowercase();
        entries
            .iter()
            .filter(|e| e.starts_with(pattern) || e.to_lowercase().starts_with(&lower))
            .collect()
    };

    let mut output = String::new();
    if filtered.is_empty() {
        output.push_str(&format!("  No matches for \"{}\"\n", word));
    } else {
        for name in &filtered {
            let trailing = if search_dir.join(name).is_dir() {
                "/"
            } else {
                ""
            };
            output.push_str(&format!("  {}{}\n", name, trailing));
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
            entries: vec![Pin {
                abbrev: vec!["g".into(), "ch".into()],
                expanded: vec!["git".into(), "checkout".into()],
            }],
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
    fn test_pipe_resolution() {
        let trie = build_test_trie();
        let pins = Pins::default();

        // Both sides of a pipe should resolve
        match resolve_line("gi push | gr -r pattern", &trie, &pins) {
            ResolveResult::Resolved(s) => assert_eq!(s, "git push | grep -r pattern"),
            other => panic!("Expected Resolved, got {:?}", other),
        }
    }

    #[test]
    fn test_chain_resolution() {
        let trie = build_test_trie();
        let pins = Pins::default();

        match resolve_line("ter init && ter ap", &trie, &pins) {
            ResolveResult::Resolved(s) => assert_eq!(s, "terraform init && terraform apply"),
            other => panic!("Expected Resolved, got {:?}", other),
        }
    }

    #[test]
    fn test_semicolon_resolution() {
        let trie = build_test_trie();
        let pins = Pins::default();

        match resolve_line("ter init; ter pl", &trie, &pins) {
            ResolveResult::Resolved(s) => assert_eq!(s, "terraform init ; terraform plan"),
            other => panic!("Expected Resolved, got {:?}", other),
        }
    }

    #[test]
    fn test_pipe_ambiguity_in_second_segment() {
        let trie = build_test_trie();
        let pins = Pins::default();

        // First segment resolves; second is ambiguous (bare "g")
        match resolve_line("ter ap | g", &trie, &pins) {
            ResolveResult::Ambiguous(info) => {
                assert_eq!(info.word, "g");
                // Position should be offset by first segment (2 words) + operator (1)
                assert_eq!(info.position, 3);
                // Resolved prefix should include the first segment + operator
                assert_eq!(info.resolved_prefix, vec!["terraform", "apply", "|"]);
            }
            other => panic!("Expected Ambiguous, got {:?}", other),
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
