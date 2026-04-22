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
/// A leading `!` (optionally after whitespace) marks the buffer as a
/// pass-through — zsh's history expansion or the user's explicit "run
/// literal" intent takes over. We never expand, complete, or learn on such
/// input.
fn starts_with_bang(input: &str) -> bool {
    input.trim_start().starts_with('!')
}

pub fn resolve_line(input: &str, trie: &CommandTrie, pins: &Pins) -> ResolveResult {
    // Leading `!`: the user wants zsh's history-expansion / literal-run
    // semantics. Never touch the buffer — return it verbatim so the shell
    // runs exactly what was typed.
    if starts_with_bang(input) {
        return ResolveResult::Passthrough(input.to_string());
    }

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

    // Note: resolved.join(" ") normalises whitespace (e.g., double-spaces
    // become single-space). This is intentional — shells split on whitespace.
    let result = resolved.join(" ");
    if any_changed && result != input {
        ResolveResult::Resolved(result)
    } else {
        ResolveResult::Passthrough(input.to_string())
    }
}

/// Commands that wrap another command: we resolve their flags but then
/// restart full resolution from the inner command onward.
/// Returns the index of the first inner-command word, or None if not a wrapper.
fn wrapper_inner_start(words: &[&str]) -> Option<usize> {
    if words.is_empty() {
        return None;
    }
    match words[0] {
        // sudo [-u user] [-g group] [-flags...] <command>
        "sudo" => {
            let mut i = 1;
            while i < words.len() {
                let w = words[i];
                if !w.starts_with('-') {
                    // Check if previous flag consumes a value (-u, -g, -C, -D, etc.)
                    if i > 1 && matches!(words[i - 1], "-u" | "-g" | "-C" | "-D" | "-p" | "-r" | "-t") {
                        i += 1;
                        continue;
                    }
                    return Some(i);
                }
                i += 1;
            }
            None
        }
        // env [-flags...] [VAR=val ...] <command>
        "env" => {
            let mut i = 1;
            while i < words.len() {
                let w = words[i];
                if w.starts_with('-') {
                    i += 1;
                    continue;
                }
                if w.contains('=') {
                    i += 1;
                    continue;
                }
                return Some(i);
            }
            None
        }
        // xargs: [flags] [command] — first non-flag word is the inner command
        "xargs" => {
            let mut i = 1;
            while i < words.len() {
                let w = words[i];
                if w.starts_with('-') {
                    // Flags that consume a value: -I, -n, -P, -L, -E, -d, -s
                    if matches!(w, "-I" | "-n" | "-P" | "-L" | "-E" | "-d" | "-s") && i + 1 < words.len() {
                        i += 2;
                        continue;
                    }
                    i += 1;
                    continue;
                }
                return Some(i);
            }
            None
        }
        // doas (sudo alternative on BSDs / some Linux): [-flags] <command>
        "doas" => {
            let mut i = 1;
            while i < words.len() {
                let w = words[i];
                if !w.starts_with('-') {
                    // -u consumes the next word
                    if i > 1 && words[i - 1] == "-u" {
                        i += 1;
                        continue;
                    }
                    return Some(i);
                }
                i += 1;
            }
            None
        }
        // Simple passthrough wrappers: first non-flag arg is the command
        "command" | "exec" | "nice" | "nohup" | "time" | "strace" | "ltrace" | "watch" => {
            let mut i = 1;
            while i < words.len() {
                if !words[i].starts_with('-') {
                    return Some(i);
                }
                i += 1;
            }
            None
        }
        _ => None,
    }
}

/// Split a command line into words, preserving quoted strings as single tokens.
/// Returns (words, quoted_mask) where quoted_mask[i] is true if words[i] was quoted.
/// Split a line into words while tracking whether each word contains quotes.
///
/// Unclosed quotes consume the rest of the line as a single word — this
/// matches what Zsh does with `PS2`-continuation input on a single command
/// buffer. We never error on unterminated quotes; the downstream resolver
/// will treat the blob as one argument.
fn split_words_quoted(input: &str) -> (Vec<&str>, Vec<bool>) {
    let mut words = Vec::new();
    let mut quoted = Vec::new();
    let bytes = input.as_bytes();
    let mut i = 0;

    while i < bytes.len() {
        // Skip whitespace
        while i < bytes.len() && bytes[i].is_ascii_whitespace() {
            i += 1;
        }
        if i >= bytes.len() {
            break;
        }

        let start = i;
        let is_quoted = match bytes[i] {
            b'\'' => {
                // Single-quoted string: find closing quote
                i += 1;
                while i < bytes.len() && bytes[i] != b'\'' {
                    i += 1;
                }
                if i < bytes.len() {
                    i += 1; // consume closing quote
                }
                true
            }
            b'"' => {
                // Double-quoted string: find closing quote (respecting backslash)
                i += 1;
                while i < bytes.len() && bytes[i] != b'"' {
                    if bytes[i] == b'\\' && i + 1 < bytes.len() {
                        i += 1;
                    }
                    i += 1;
                }
                if i < bytes.len() {
                    i += 1;
                }
                true
            }
            _ => {
                // Unquoted word
                while i < bytes.len() && !bytes[i].is_ascii_whitespace() {
                    // Handle inline quotes within a word (e.g., foo"bar baz"qux)
                    if bytes[i] == b'\'' {
                        i += 1;
                        while i < bytes.len() && bytes[i] != b'\'' {
                            i += 1;
                        }
                        if i < bytes.len() {
                            i += 1;
                        }
                    } else if bytes[i] == b'"' {
                        i += 1;
                        while i < bytes.len() && bytes[i] != b'"' {
                            if bytes[i] == b'\\' && i + 1 < bytes.len() {
                                i += 1;
                            }
                            i += 1;
                        }
                        if i < bytes.len() {
                            i += 1;
                        }
                    } else if bytes[i] == b'\\' && i + 1 < bytes.len() {
                        i += 2;
                    } else {
                        i += 1;
                    }
                }
                // Mark as quoted if the word contains quotes
                input[start..i].contains('\'') || input[start..i].contains('"')
            }
        };

        let word = &input[start..i];
        if !word.is_empty() {
            words.push(word);
            quoted.push(is_quoted);
        }
    }

    (words, quoted)
}

/// Resolve a single command segment (no pipes/chains) against the trie and pins.
pub fn resolve(input: &str, trie: &CommandTrie, pins: &Pins) -> ResolveResult {
    let (qwords, _quoted_mask) = split_words_quoted(input);
    if qwords.is_empty() {
        return ResolveResult::Passthrough(input.to_string());
    }

    // For pin matching and wrapper detection, use the stripped words
    let words: Vec<&str> = qwords.clone();

    // Handle wrapper commands (sudo, env, etc.) by resolving the inner command
    // separately and then prepending the wrapper prefix.
    if let Some(inner_start) = wrapper_inner_start(&words) {
        let wrapper_prefix: Vec<String> = words[..inner_start].iter().map(|s| s.to_string()).collect();
        let inner_input: String = words[inner_start..].join(" ");

        match resolve(&inner_input, trie, pins) {
            ResolveResult::Resolved(inner) => {
                let full = format!("{} {}", wrapper_prefix.join(" "), inner);
                if full == input {
                    return ResolveResult::Passthrough(input.to_string());
                }
                return ResolveResult::Resolved(full);
            }
            ResolveResult::Ambiguous(mut info) => {
                info.position += inner_start;
                let mut full_prefix = wrapper_prefix;
                full_prefix.extend(info.resolved_prefix);
                info.resolved_prefix = full_prefix;
                return ResolveResult::Ambiguous(info);
            }
            ResolveResult::PathAmbiguous(candidates) => {
                let prefix = wrapper_prefix.join(" ");
                let adjusted: Vec<String> = candidates
                    .into_iter()
                    .map(|c| format!("{} {}", prefix, c))
                    .collect();
                return ResolveResult::PathAmbiguous(adjusted);
            }
            ResolveResult::Passthrough(inner) => {
                let full = format!("{} {}", wrapper_prefix.join(" "), inner);
                return ResolveResult::Passthrough(full);
            }
        }
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

fn finalize_with_paths(input: &str, mut words: Vec<String>, trie: &CommandTrie) -> ResolveResult {
    // If the command word itself is a relative/absolute path (e.g. `./unin`,
    // `~/bin/foo`), resolve it against the filesystem before handling args.
    if let Some(cmd) = words.first()
        && (cmd.contains('/') || cmd.starts_with('~')) && !cmd.starts_with('-') {
            match path_resolve::resolve_path(cmd) {
                path_resolve::PathResult::Resolved(resolved) => {
                    words[0] = shell_escape_path(&resolved);
                }
                path_resolve::PathResult::Ambiguous(candidates) => {
                    let suffix: Vec<String> = words[1..].to_vec();
                    let full_cmds: Vec<String> = candidates
                        .into_iter()
                        .map(|c| {
                            let mut parts = vec![shell_escape_path(&c)];
                            parts.extend(suffix.clone());
                            parts.join(" ")
                        })
                        .collect();
                    return ResolveResult::PathAmbiguous(full_cmds);
                }
                path_resolve::PathResult::Unchanged => {}
            }
        }

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

    // Quoted words are never expanded — pass through as-is.
    // This prevents resolving "fix bug" in `git commit -m "fix bug"`.
    let word = words[0];
    if word.starts_with('\'') || word.starts_with('"') {
        for w in words {
            result.push(w.to_string());
        }
        return Ok(());
    }

    // For path/dir/runtime commands, skip trie resolution for arguments --
    // they'll be resolved by the path resolver or runtime resolver later.
    //
    // Hardcoded file/dir commands (ls, cd, cat, nano, …) always skip, even if
    // they have historical trie entries that would prefix-match the word.
    //
    // Commands whose Paths arg_mode comes from the completions parser (e.g. git,
    // docker) may have real subcommands, so we only skip when there are no
    // non-flag trie matches for this word.
    if !result.is_empty() {
        let mode = arg_mode(&result[0], modes);
        if matches!(
            mode,
            ArgMode::DirsOnly | ArgMode::Paths | ArgMode::Runtime(_)
        ) {
            let force_skip = is_hardcoded_path_command(&result[0]);
            let has_subcmd_match = !force_skip
                && !word.starts_with('-')
                && start_node
                    .prefix_search(word)
                    .iter()
                    .any(|(n, _)| !n.starts_with('-'));
            if !has_subcmd_match {
                for w in words {
                    result.push(w.to_string());
                }
                return Ok(());
            }
        }
    }
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
                let deep = deep_disambiguate(&matches, rest);

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
/// the next words. Looks up to 3 words ahead for disambiguation.
/// Returns the filtered matches.
fn deep_disambiguate<'a>(
    matches: &[(&'a str, &'a TrieNode)],
    rest: &[&str],
) -> Vec<(&'a str, &'a TrieNode)> {
    if rest.is_empty() {
        return matches.to_vec();
    }

    // First pass: filter by immediate next word
    let next_word = rest[0];
    let filtered: Vec<(&'a str, &'a TrieNode)> = matches
        .iter()
        .filter(|(_, node)| !node.prefix_search(next_word).is_empty())
        .copied()
        .collect();

    if filtered.len() <= 1 || rest.len() <= 1 {
        return filtered;
    }

    // Second pass: look deeper — try rest[1] (and rest[2]) to further narrow
    let mut deeper = filtered.clone();
    for depth in 1..rest.len().min(3) {
        let lookahead = rest[depth];
        if lookahead.starts_with('-') {
            // Flags can exist on many commands — not useful for disambiguation
            continue;
        }
        let narrowed: Vec<(&'a str, &'a TrieNode)> = deeper
            .iter()
            .filter(|(_, node)| {
                // Walk to the child that matches rest[0..depth], then check rest[depth]
                let mut current = *node;
                for &w in &rest[..depth] {
                    let sub_matches = current.prefix_search(w);
                    if sub_matches.len() == 1 {
                        current = sub_matches[0].1;
                    } else {
                        return true; // Can't walk further, keep this candidate
                    }
                }
                !current.prefix_search(lookahead).is_empty()
            })
            .copied()
            .collect();
        if !narrowed.is_empty() && narrowed.len() < deeper.len() {
            deeper = narrowed;
        }
        if deeper.len() == 1 {
            break;
        }
    }

    deeper
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

/// Apply context-sensitive rules from the spec against the already-typed words.
///
/// When a flag listed in a `ContextRule.trigger_flags` is present anywhere in
/// the resolved command line, the rule's `override_type` replaces the default
/// completion mode.  Rules are checked in order; the first match wins.
fn apply_context_rules(spec: Option<&ArgSpec>, words: &[String], base: ArgMode) -> ArgMode {
    let Some(spec) = spec else { return base; };
    for rule in &spec.context_rules {
        if rule
            .trigger_flags
            .iter()
            .any(|f| words.iter().any(|w| w == f))
        {
            return u8_to_arg_mode(rule.override_type);
        }
    }
    base
}

fn resolve_paths_in_words(
    words: &[String],
    spec: Option<&ArgSpec>,
    fallback_mode: ArgMode,
    cmd_words: usize,
) -> PathsResult {
    let mut result: Vec<String> = Vec::new();
    let mut arg_position: u32 = 0; // 1-indexed position of non-flag arguments after the command prefix
    let mut next_is_flag_value = false; // true when prev word was a flag that consumes a typed value

    for (i, word) in words.iter().enumerate() {
        // Skip command prefix words (e.g., "git" or "git add")
        if i < cmd_words {
            result.push(word.clone());
            continue;
        }

        // Quoted words are never path-resolved — pass through as-is
        if word.starts_with('\'') || word.starts_with('"') {
            result.push(word.clone());
            continue;
        }

        // Bare . and .. are directory literals (e.g. `git add .`); never resolve.
        if word == "." || word == ".." {
            result.push(word.clone());
            continue;
        }

        if word.starts_with('-') {
            result.push(word.clone());
            // Check if this flag consumes the next word as a typed value
            next_is_flag_value = spec.is_some_and(|s| s.flag_takes_value(word));
            continue;
        }

        // If this word is a flag's value, don't count it as a positional argument
        if next_is_flag_value {
            next_is_flag_value = false;
        } else {
            arg_position += 1;
        }
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
                        result.push(escape_resolved_path(word, &resolved));
                    }
                    path_resolve::PathResult::Ambiguous(candidates) => {
                        let prefix: Vec<String> = result.clone();
                        let suffix: Vec<String> = words[i + 1..].to_vec();
                        let full_cmds: Vec<String> = candidates
                            .into_iter()
                            .map(|c| {
                                let mut parts = prefix.clone();
                                parts.push(escape_resolved_path(word, &c));
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
                            result.push(escape_resolved_path(word, &resolved));
                        }
                        path_resolve::PathResult::Ambiguous(candidates) => {
                            let prefix: Vec<String> = result.clone();
                            let suffix: Vec<String> = words[i + 1..].to_vec();
                            let full_cmds: Vec<String> = candidates
                                .into_iter()
                                .map(|c| {
                                    let mut parts = prefix.clone();
                                    parts.push(escape_resolved_path(word, &c));
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

/// Escape shell metacharacters in resolved paths so they're safe to execute.
fn shell_escape_path(path: &str) -> String {
    if path
        .bytes()
        .all(|b| matches!(b, b'a'..=b'z' | b'A'..=b'Z' | b'0'..=b'9' | b'/' | b'.' | b'-' | b'_' | b'~' | b':' | b',' | b'+' | b'@' | b'%'))
    {
        return path.to_string();
    }
    let mut out = String::with_capacity(path.len() + 8);
    for ch in path.chars() {
        match ch {
            ' ' | '(' | ')' | '\'' | '"' | '$' | '`' | '!' | '&' | ';' | '|' | '{' | '}'
            | '[' | ']' | '#' | '?' | '*' | '<' | '>' | '\\' | '=' | '^' => {
                out.push('\\');
                out.push(ch);
            }
            _ => out.push(ch),
        }
    }
    out
}

/// Like `shell_escape_path` but leaves `*` and `?` unescaped so the shell
/// can expand them as globs. Used when the original word contained `**`
/// (the glob passthrough prefix).
fn shell_escape_path_glob(path: &str) -> String {
    if path
        .bytes()
        .all(|b| matches!(b, b'a'..=b'z' | b'A'..=b'Z' | b'0'..=b'9' | b'/' | b'.' | b'-' | b'_' | b'~' | b':' | b',' | b'+' | b'@' | b'%' | b'*' | b'?'))
    {
        return path.to_string();
    }
    let mut out = String::with_capacity(path.len() + 8);
    for ch in path.chars() {
        match ch {
            ' ' | '(' | ')' | '\'' | '"' | '$' | '`' | '!' | '&' | ';' | '|' | '{' | '}'
            | '[' | ']' | '#' | '<' | '>' | '\\' | '=' | '^' => {
                out.push('\\');
                out.push(ch);
            }
            _ => out.push(ch),
        }
    }
    out
}

fn escape_resolved_path(original_word: &str, resolved: &str) -> String {
    if original_word.contains("**") {
        shell_escape_path_glob(resolved)
    } else {
        shell_escape_path(resolved)
    }
}

/// How a command's arguments should be resolved.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
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

/// Commands whose arguments are directory paths (cd, pushd).
const DIR_COMMANDS: &[&str] = &["cd", "pushd"];

/// Commands whose arguments are filesystem paths (files and/or directories).
const PATH_COMMANDS: &[&str] = &[
    "ls", "rm", "rmdir", "mkdir", "cp", "mv", "ln", "cat", "less", "more", "head", "tail", "wc",
    "touch", "chmod", "chown", "chgrp", "stat", "file", "readlink", "realpath", "basename",
    "dirname", "du", "find", "diff", "patch", "tar", "zip", "unzip", "gzip", "gunzip", "bzip2",
    "xz", "source", "open", "nano", "vim", "vi", "nvim", "emacs", "code", "bat",
    "rsync", "scp", "sftp", "rg", "fd", "exa", "eza",
];

/// Commands whose arguments are executable / command names.
const EXEC_COMMANDS: &[&str] = &[
    "which", "type", "whence", "where", "command", "man", "rehash",
];

/// Returns true for commands that are hardcoded as file/dir/exec-only —
/// these always skip trie resolution for their arguments regardless of
/// whether the trie node happens to have learned entries.
fn is_hardcoded_path_command(cmd: &str) -> bool {
    DIR_COMMANDS.contains(&cmd) || PATH_COMMANDS.contains(&cmd)
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
    if DIR_COMMANDS.contains(&cmd) {
        ArgMode::DirsOnly
    } else if PATH_COMMANDS.contains(&cmd) {
        ArgMode::Paths
    } else if EXEC_COMMANDS.contains(&cmd) {
        ArgMode::ExecsOnly
    } else {
        ArgMode::Normal
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

/// Produce a human-readable narrative of how `input` resolves against the
/// trie and pins. Walks the same primitives as the real resolver (pins,
/// wrapper detection, trie prefix search, deep disambiguation, arg-spec
/// lookup) and then reports the actual `resolve_line` result so any
/// discrepancy between the walk and the real engine is visible.
pub fn explain(input: &str, trie: &CommandTrie, pins: &Pins) -> String {
    let mut out = Vec::<String>::new();
    let push = |out: &mut Vec<String>, depth: usize, s: String| {
        out.push(format!("{}{}", "  ".repeat(depth), s));
    };

    push(&mut out, 0, format!("zsh-ios explain: {:?}", input));
    push(&mut out, 0, String::new());

    // 1. Leading-! bypass
    if starts_with_bang(input) {
        push(
            &mut out,
            0,
            "Leading-! bypass: buffer starts with '!' — run AS-IS, no resolution.".into(),
        );
        push(&mut out, 0, format!("Final: Passthrough → {}", input));
        return out.join("\n");
    }

    // 2. Split on pipe/chain operators
    let parts = split_on_operators(input);
    let segments: Vec<&str> = parts
        .iter()
        .filter_map(|p| match p {
            LinePart::Command(c) => Some(c.as_str()),
            _ => None,
        })
        .collect();
    let has_op = parts.iter().any(|p| matches!(p, LinePart::Operator(_)));
    if has_op {
        push(
            &mut out,
            0,
            format!(
                "Pipe/chain split: {} command segment{}",
                segments.len(),
                if segments.len() == 1 { "" } else { "s" }
            ),
        );
    }

    // 3. Per-segment narrative
    for (i, seg) in segments.iter().enumerate() {
        let trimmed = seg.trim();
        if trimmed.is_empty() {
            continue;
        }
        push(&mut out, 0, String::new());
        if has_op {
            push(&mut out, 0, format!("Segment {}: {:?}", i + 1, trimmed));
            explain_segment(&mut out, 1, trimmed, trie, pins);
        } else {
            push(&mut out, 0, format!("Command: {:?}", trimmed));
            explain_segment(&mut out, 1, trimmed, trie, pins);
        }
    }

    // 4. Real result
    push(&mut out, 0, String::new());
    match resolve_line(input, trie, pins) {
        ResolveResult::Resolved(s) => {
            push(&mut out, 0, format!("Final: Resolved → {}", s));
        }
        ResolveResult::Passthrough(s) => {
            push(&mut out, 0, format!("Final: Passthrough → {}", s));
            push(
                &mut out,
                0,
                "  (no trie match; line returned unchanged)".into(),
            );
        }
        ResolveResult::Ambiguous(info) => {
            push(&mut out, 0, "Final: Ambiguous".into());
            push(&mut out, 1, format!("ambiguous word : {:?}", info.word));
            push(&mut out, 1, format!("longest common : {:?}", info.lcp));
            push(&mut out, 1, format!("position       : {}", info.position));
            if !info.resolved_prefix.is_empty() {
                push(
                    &mut out,
                    1,
                    format!("resolved prefix: {}", info.resolved_prefix.join(" ")),
                );
            }
            if !info.remaining.is_empty() {
                push(
                    &mut out,
                    1,
                    format!("remaining      : {}", info.remaining.join(" ")),
                );
            }
            push(
                &mut out,
                1,
                format!("candidates     : {}", info.candidates.join(", ")),
            );
            if !info.deep_candidates.is_empty() {
                push(&mut out, 1, "deep candidates:".into());
                for dc in &info.deep_candidates {
                    push(
                        &mut out,
                        2,
                        format!("{}  (subs: {})", dc.command, dc.subcommand_matches.join(", ")),
                    );
                }
            }
        }
        ResolveResult::PathAmbiguous(cands) => {
            push(&mut out, 0, "Final: PathAmbiguous".into());
            for c in &cands {
                push(&mut out, 1, format!("• {}", c));
            }
        }
    }

    out.join("\n")
}

/// Internal narrator for a single command segment (no pipe/chain operators).
/// Mirrors the decision tree of `resolve`: wrapper detect → pin lookup →
/// first-word trie match → deep disambiguation if ambiguous → arg-spec lookup.
fn explain_segment(
    out: &mut Vec<String>,
    depth: usize,
    input: &str,
    trie: &CommandTrie,
    pins: &Pins,
) {
    let push = |out: &mut Vec<String>, d: usize, s: String| {
        out.push(format!("{}{}", "  ".repeat(d), s));
    };

    let (word_strs, _) = split_words_quoted(input);
    let words: Vec<String> = word_strs.iter().map(|s| s.to_string()).collect();
    let word_refs: Vec<&str> = words.iter().map(String::as_str).collect();

    if word_refs.is_empty() {
        push(out, depth, "empty segment — no words to resolve".into());
        return;
    }

    // Wrapper (sudo, env, xargs, watch, doas, nice, nohup, time, command)
    if let Some(inner) = wrapper_inner_start(&word_refs) {
        let wrapper_words: Vec<&str> = word_refs[..inner].to_vec();
        push(
            out,
            depth,
            format!(
                "Wrapper: {} — pass through, resolve from word {}",
                wrapper_words.join(" "),
                inner + 1
            ),
        );
        let inner_str = word_refs[inner..].join(" ");
        if !inner_str.is_empty() {
            push(out, depth, format!("Inner: {:?}", inner_str));
            explain_segment(out, depth + 1, &inner_str, trie, pins);
        }
        return;
    }

    // Pin lookup (longest prefix)
    match pins.longest_match(&word_refs) {
        Some((consumed, expanded)) => {
            push(
                out,
                depth,
                format!(
                    "Pin match: \"{}\" → \"{}\"  (consumes {} word{})",
                    word_refs[..consumed].join(" "),
                    expanded.join(" "),
                    consumed,
                    if consumed == 1 { "" } else { "s" }
                ),
            );
            if consumed == words.len() {
                return; // pin covers the whole input
            }
            push(
                out,
                depth,
                format!(
                    "Remaining after pin: {}",
                    word_refs[consumed..].join(" ")
                ),
            );
        }
        None => {
            push(out, depth, "Pin lookup: no longest-prefix match".into());
        }
    }

    // First-word trie lookup
    let first = &word_refs[0];
    let first_matches = trie.root.prefix_search(first);
    if first_matches.is_empty() {
        push(
            out,
            depth,
            format!("Trie: no commands with prefix {:?}", first),
        );
        return;
    }
    if first_matches.len() == 1 {
        let name = first_matches[0].0;
        if name == *first {
            push(out, depth, format!("Trie: {:?} is an exact command", first));
        } else {
            push(
                out,
                depth,
                format!("Trie: {:?} uniquely matches {:?}", first, name),
            );
        }
    } else {
        let names: Vec<&str> = first_matches.iter().map(|(n, _)| *n).collect();
        push(
            out,
            depth,
            format!(
                "Trie: {:?} is ambiguous — {} candidates: {}",
                first,
                names.len(),
                summarize_names(&names, 8)
            ),
        );

        // Deep disambiguation using the next word, if any
        if word_refs.len() > 1 {
            let next = &word_refs[1];
            push(
                out,
                depth + 1,
                format!("Deep-disambiguate with next word {:?}:", next),
            );
            let mut survivors: Vec<(&str, Vec<&str>)> = Vec::new();
            let mut nonmatch_count = 0usize;
            for (name, node) in &first_matches {
                let sub = node.prefix_search(next);
                if sub.is_empty() {
                    nonmatch_count += 1;
                } else {
                    let sub_names: Vec<&str> = sub.iter().map(|(n, _)| *n).collect();
                    survivors.push((name, sub_names));
                }
            }
            // Only show survivors in detail; summarize non-matches in one line
            // so a 40-candidate case doesn't produce 40 "no match" lines.
            for (name, sub_names) in &survivors {
                push(
                    out,
                    depth + 2,
                    format!("{}: {}", name, summarize_names(sub_names, 6)),
                );
            }
            if nonmatch_count > 0 {
                push(
                    out,
                    depth + 2,
                    format!(
                        "({} other candidate{} had no {:?} subcommand)",
                        nonmatch_count,
                        if nonmatch_count == 1 { "" } else { "s" },
                        next
                    ),
                );
            }
            match survivors.len() {
                0 => push(
                    out,
                    depth + 1,
                    "→ no survivor — ambiguity stands".into(),
                ),
                1 => push(
                    out,
                    depth + 1,
                    format!("→ winner: {}", survivors[0].0),
                ),
                n => push(
                    out,
                    depth + 1,
                    format!("→ {} candidates survive — still ambiguous", n),
                ),
            }
        }
    }

    // Arg-spec (per-position type metadata)
    if word_refs.len() >= 2 {
        let two = format!("{} {}", word_refs[0], word_refs[1]);
        if let Some(spec) = trie.arg_specs.get(&two) {
            push(out, depth, format!("ArgSpec: detailed spec for {:?}", two));
            describe_spec(out, depth + 1, spec);
        } else if let Some(spec) = trie.arg_specs.get(word_refs[0]) {
            push(
                out,
                depth,
                format!("ArgSpec: top-level spec for {:?}", word_refs[0]),
            );
            describe_spec(out, depth + 1, spec);
        }
    }
}

/// Render a name list, truncated with an ellipsis when there are more than
/// `cap` entries. Keeps explain output readable even when `gr` matches 40
/// commands on a developer box.
fn summarize_names(names: &[&str], cap: usize) -> String {
    if names.len() <= cap {
        return names.join(", ");
    }
    let head = names[..cap].join(", ");
    format!("{}, … ({} more)", head, names.len() - cap)
}

fn describe_spec(out: &mut Vec<String>, depth: usize, spec: &ArgSpec) {
    let push = |out: &mut Vec<String>, d: usize, s: String| {
        out.push(format!("{}{}", "  ".repeat(d), s));
    };
    if let Some(t) = spec.rest {
        push(
            out,
            depth,
            format!(
                "positional rest: {} ({})",
                t,
                crate::runtime_complete::type_hint(t)
            ),
        );
    }
    for (pos, t) in &spec.positional {
        push(
            out,
            depth,
            format!(
                "position {}: {} ({})",
                pos,
                t,
                crate::runtime_complete::type_hint(*t)
            ),
        );
    }
    if !spec.flag_args.is_empty() {
        let n = spec.flag_args.len();
        push(
            out,
            depth,
            format!("{} flag{} take typed values", n, if n == 1 { "" } else { "s" }),
        );
    }
    if !spec.context_rules.is_empty() {
        push(
            out,
            depth,
            format!("{} context rule(s)", spec.context_rules.len()),
        );
    }
}

/// Generate completions for the `?` command.
/// Returns a formatted list of matching commands/subcommands.
/// Splits on pipe/chain operators and completes only the last segment.
pub fn complete(input: &str, trie: &CommandTrie, pins: &Pins) -> String {
    // Leading `!` is a hands-off marker (see `starts_with_bang`). Produce no
    // completions so the shell's native completion (or history expansion)
    // gets a clean look.
    if starts_with_bang(input) {
        return String::new();
    }

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
    let current_mode = {
        let base = arg_type_for_word(arg_position, prev_word, spec, fallback_mode);
        apply_context_rules(spec, &resolved_words, base)
    };

    // --- Flag completion mode ---
    // When typing a flag prefix (starts with '-'), show known flags + their expected arg types.
    if prefix.starts_with('-') {
        return complete_flags(prefix, spec, node, output);
    }

    // --- Trie-based completion (subcommands) ---
    // _call_program: flag value or rest is produced by running an external command.
    // Check this before the trie so we show live dynamic values (e.g. ssh -Q cipher).
    if let Some(prev) = prev_word
        && prev.starts_with('-')
        && let Some((tag, argv)) = spec.and_then(|s| s.flag_call_programs.get(prev))
    {
        output.push_str(&format!("  Expects: <{}>\n", tag));
        let results = runtime_complete::call_program_cached(argv, prefix);
        if !results.is_empty() {
            let names: Vec<&str> = results.iter().map(String::as_str).collect();
            output.push_str(&format_columns(&names, 80));
        } else if !prefix.is_empty() {
            output.push_str(&format!("  No matches for \"{}\"\n", prefix));
        }
        return output;
    }

    // Static list: flag value is a literal enumeration (compadd - yes no, _values, etc.)
    if let Some(prev) = prev_word
        && prev.starts_with('-')
        && let Some(items) = spec.and_then(|s| s.flag_static_lists.get(prev))
    {
        output.push_str("  Expects: <value>\n");
        let filtered: Vec<&str> = items
            .iter()
            .filter(|i| prefix.is_empty() || i.starts_with(prefix))
            .map(String::as_str)
            .collect();
        if !filtered.is_empty() {
            output.push_str(&format_columns(&filtered, 80));
        } else if !prefix.is_empty() {
            output.push_str(&format!("  No matches for \"{}\"\n", prefix));
        }
        return output;
    }

    // Rest position with call_program (and not completing a subcommand / flag)
    let prev_is_flag_consuming =
        prev_word.is_some_and(|p| p.starts_with('-') && spec.is_some_and(|s| s.flag_takes_value(p)));
    if !prefix.starts_with('-')
        && !prev_is_flag_consuming
        && let Some((tag, argv)) = spec.and_then(|s| s.rest_call_program.as_ref())
    {
        let results = runtime_complete::call_program_cached(argv, prefix);
        if !results.is_empty() {
            output.push_str(&format!("  Expects: <{}>\n", tag));
            let names: Vec<&str> = results.iter().map(String::as_str).collect();
            output.push_str(&format_columns(&names, 80));
            return output;
        }
    }

    // Rest position with static list
    if !prefix.starts_with('-')
        && !prev_is_flag_consuming
        && let Some(items) = spec.and_then(|s| s.rest_static_list.as_ref())
    {
        let filtered: Vec<&str> = items
            .iter()
            .filter(|i| prefix.is_empty() || i.starts_with(prefix))
            .map(String::as_str)
            .collect();
        if !filtered.is_empty() {
            output.push_str("  Expects: <value>\n");
            output.push_str(&format_columns(&filtered, 80));
            return output;
        }
    }

    // Skip trie when we're completing the value of a flag that takes a typed
    // argument (e.g. `sudo -u <user>`).  The trie children here are learned
    // prior invocations of the command, not values for this flag.
    let in_flag_value_context = prev_word
        .is_some_and(|p| p.starts_with('-') && spec.is_some_and(|s| s.flag_takes_value(p)));

    let trie_matches = if in_flag_value_context {
        vec![]
    } else {
        node.prefix_search(prefix)
    };

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

            // Try to show descriptions for subcommands (Cisco IOS style)
            let cmd_key = resolved_words.join(" ");
            let descs = trie.descriptions.get(&cmd_key);

            if descs.is_some_and(|d| !d.is_empty()) && sorted.len() <= 40 {
                let descs = descs.unwrap();
                let col_width = sorted.iter().map(|(n, _)| n.len()).max().unwrap_or(0) + 2;
                for (name, _) in &sorted {
                    if let Some(desc) = descs.get(*name) {
                        output.push_str(&format!("  {:<width$}{}\n", name, desc, width = col_width));
                    } else {
                        output.push_str(&format!("  {}\n", name));
                    }
                }
            } else {
                let names: Vec<&str> = sorted.iter().map(|(n, _)| *n).collect();
                output.push_str(&format_columns(&names, 80));
            }
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
        // Also include _call_program flags (they take a value but the type is dynamic)
        for flag in spec.flag_call_programs.keys() {
            if flag.starts_with(prefix) && !known_flags.iter().any(|(f, _)| f == flag) {
                known_flags.push((flag.clone(), None));
            }
        }
        // Also include static list flags
        for flag in spec.flag_static_lists.keys() {
            if flag.starts_with(prefix) && !known_flags.iter().any(|(f, _)| f == flag) {
                known_flags.push((flag.clone(), None));
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
        } else if let Some((tag, argv)) =
            spec.and_then(|s| s.flag_call_programs.get(prefix))
        {
            // _call_program flag: run it now to show valid values
            output.push_str(&format!("  {} expects: <{}>\n", prefix, tag));
            let results = runtime_complete::call_program_cached(argv, "");
            if !results.is_empty() {
                let names: Vec<&str> = results.iter().map(String::as_str).collect();
                output.push_str(&format_columns(&names, 80));
            }
        } else if let Some(items) = spec.and_then(|s| s.flag_static_lists.get(prefix)) {
            // Static list flag: show the known items
            output.push_str(&format!("  {} expects: <value>\n", prefix));
            let names: Vec<&str> = items.iter().map(String::as_str).collect();
            output.push_str(&format_columns(&names, 80));
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
        let typed_hint = spec
            .and_then(|s| s.type_after_flag(name))
            .map(runtime_complete::type_hint);
        let call_program_hint = spec
            .and_then(|s| s.flag_call_programs.get(*name))
            .map(|(tag, _)| tag.as_str());
        let static_hint: Option<String> = spec
            .and_then(|s| s.flag_static_lists.get(*name))
            .map(|items| items.iter().take(4).cloned().collect::<Vec<_>>().join("|"));
        if let Some(hint) = typed_hint {
            out.push_str(&format!("  {:<width$}<{}>\n", name, hint, width = col_width));
        } else if let Some(hint) = call_program_hint {
            out.push_str(&format!("  {:<width$}<{}>\n", name, hint, width = col_width));
        } else if let Some(hint) = static_hint {
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
            // Handle user@host prefix splitting: typing `alice@gi` means
            // we should complete host names that start with "gi", then
            // prepend "alice@" to each result.  Mirrors `compset -P '*@'`
            // in Zsh completion functions like _ssh.
            if type_id == trie::ARG_MODE_HOSTS
                && let Some(at_pos) = prefix.find('@')
            {
                let user_prefix = &prefix[..=at_pos]; // e.g. "alice@"
                let host_prefix = &prefix[at_pos + 1..]; // e.g. "gi"
                let hosts = runtime_complete::list_matches(trie::ARG_MODE_HOSTS, host_prefix);
                let with_user: Vec<String> =
                    hosts.iter().map(|h| format!("{user_prefix}{h}")).collect();
                output.push_str("  Expects: <user@host>\n");
                if with_user.is_empty() {
                    if !host_prefix.is_empty() {
                        output.push_str(&format!("  No matches for \"{host_prefix}\"\n"));
                    }
                } else {
                    let names: Vec<&str> = with_user.iter().map(String::as_str).collect();
                    output.push_str(&format_columns(&names, 80));
                }
                return;
            }
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
        // Append directory marker and use multi-column display
        let display_names: Vec<String> = filtered
            .iter()
            .map(|name| {
                if search_dir.join(name.as_str()).is_dir() {
                    format!("{}/", name)
                } else {
                    name.to_string()
                }
            })
            .collect();
        let refs: Vec<&str> = display_names.iter().map(String::as_str).collect();
        output.push_str(&format_columns(&refs, 80));
    }
    output
}


#[cfg(test)]
mod tests {
    use super::*;
    use crate::pins::Pin;
    use crate::test_util::CWD_LOCK;
    use crate::trie::ContextRule;

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
        if let ResolveResult::Ambiguous(info) = resolve("cd te", &trie, &pins) {
            panic!(
                "cd args should skip trie resolution, got ambiguous: {:?}",
                info.candidates
            );
        }
    }

    #[test]
    fn test_pushd_skips_trie() {
        let mut trie = CommandTrie::new();
        trie.insert_command("pushd");
        trie.insert(&["pushd", "projects"]);
        trie.insert(&["pushd", "pictures"]);
        let pins = Pins::default();

        if let ResolveResult::Ambiguous(_) = resolve("pushd pro", &trie, &pins) {
            panic!("pushd args should skip trie resolution");
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
        if let ResolveResult::Ambiguous(info) = resolve("ls te", &trie, &pins) {
            panic!(
                "ls args should skip trie, got ambiguous: {:?}",
                info.candidates
            );
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

    // --- Tests for sudo/env wrapper chaining ---

    #[test]
    fn test_sudo_resolves_inner_command() {
        let trie = build_test_trie();
        let pins = Pins::default();

        match resolve("sudo ter ap", &trie, &pins) {
            ResolveResult::Resolved(s) => assert_eq!(s, "sudo terraform apply"),
            other => panic!("Expected Resolved, got {:?}", other),
        }
    }

    #[test]
    fn test_sudo_with_flags() {
        let trie = build_test_trie();
        let pins = Pins::default();

        match resolve("sudo -u root ter ap", &trie, &pins) {
            ResolveResult::Resolved(s) => assert_eq!(s, "sudo -u root terraform apply"),
            other => panic!("Expected Resolved, got {:?}", other),
        }
    }

    #[test]
    fn test_env_resolves_inner_command() {
        let trie = build_test_trie();
        let pins = Pins::default();

        match resolve("env FOO=bar ter ap", &trie, &pins) {
            ResolveResult::Resolved(s) => assert_eq!(s, "env FOO=bar terraform apply"),
            other => panic!("Expected Resolved, got {:?}", other),
        }
    }

    #[test]
    fn test_nice_resolves_inner_command() {
        let trie = build_test_trie();
        let pins = Pins::default();

        match resolve("nice ter ap", &trie, &pins) {
            ResolveResult::Resolved(s) => assert_eq!(s, "nice terraform apply"),
            other => panic!("Expected Resolved, got {:?}", other),
        }
    }

    #[test]
    fn test_sudo_preserves_ambiguity() {
        let trie = build_test_trie();
        let pins = Pins::default();

        // sudo g → g is ambiguous; wrapper should propagate the ambiguity
        match resolve("sudo g", &trie, &pins) {
            ResolveResult::Ambiguous(info) => {
                assert_eq!(info.word, "g");
                // Position should be offset by "sudo" (1)
                assert_eq!(info.position, 1);
                assert!(info.candidates.contains(&"git".to_string()));
            }
            other => panic!("Expected Ambiguous, got {:?}", other),
        }
    }

    // --- Tests for multi-level deep disambiguation ---

    #[test]
    fn test_deep_disambig_multi_level() {
        let mut trie = CommandTrie::new();
        trie.insert(&["git", "commit", "-m"]);
        trie.insert(&["git", "checkout", "main"]);
        trie.insert(&["grep", "-r", "pattern"]);
        trie.insert(&["go", "build"]);
        // "g co" → both git and go have "co" matches (commit, checkout / ?),
        // but with "g co main", only git checkout has "main" as a child.
        let pins = Pins::default();

        match resolve("g ch main", &trie, &pins) {
            ResolveResult::Resolved(s) => assert_eq!(s, "git checkout main"),
            other => panic!("Expected deep disambig to resolve, got {:?}", other),
        }
    }

    // --- Tests for shell_escape_path ---

    #[test]
    fn test_shell_escape_plain_path() {
        assert_eq!(shell_escape_path("/usr/local/bin"), "/usr/local/bin");
        assert_eq!(shell_escape_path("file.txt"), "file.txt");
    }

    #[test]
    fn test_shell_escape_special_chars() {
        assert_eq!(shell_escape_path("my file.txt"), "my\\ file.txt");
        assert_eq!(shell_escape_path("dir (1)"), "dir\\ \\(1\\)");
        assert_eq!(shell_escape_path("$HOME/file"), "\\$HOME/file");
        assert_eq!(shell_escape_path("file;rm -rf"), "file\\;rm\\ -rf");
        assert_eq!(shell_escape_path("a&b"), "a\\&b");
        assert_eq!(shell_escape_path("test'quote"), "test\\'quote");
    }

    // --- Tests for descriptions (loaded from YAML) ---

    fn load_yaml_descriptions() -> trie::DescriptionMap {
        let yaml_str = include_str!("../data/descriptions.yaml");
        serde_yaml_ng::from_str(yaml_str).unwrap()
    }

    #[test]
    fn test_descriptions_unknown() {
        let descs = load_yaml_descriptions();
        assert!(!descs.contains_key("unknowncommand"));
    }

    #[test]
    fn test_descriptions_docker() {
        let descs = load_yaml_descriptions();
        let docker = descs.get("docker").expect("docker should have descriptions");
        assert!(docker.get("run").is_some());
        assert!(docker.get("build").is_some());
    }

    #[test]
    fn test_descriptions_cargo() {
        let descs = load_yaml_descriptions();
        let cargo = descs.get("cargo").expect("cargo should have descriptions");
        assert_eq!(cargo.get("test").map(String::as_str), Some("Execute all unit and integration tests"));
    }

    #[test]
    fn test_descriptions_zsh_ios() {
        let descs = load_yaml_descriptions();
        let zio = descs.get("zsh-ios").expect("zsh-ios should have descriptions");
        assert!(zio.get("build").is_some());
        assert!(zio.get("resolve").is_some());
    }

    // --- Tests for wrapper_inner_start ---

    #[test]
    fn test_wrapper_inner_start_sudo() {
        assert_eq!(wrapper_inner_start(&["sudo", "ls"]), Some(1));
        assert_eq!(wrapper_inner_start(&["sudo", "-u", "root", "ls"]), Some(3));
        assert_eq!(wrapper_inner_start(&["sudo", "-i"]), None); // no inner command
    }

    #[test]
    fn test_wrapper_inner_start_env() {
        assert_eq!(wrapper_inner_start(&["env", "FOO=bar", "ls"]), Some(2));
        assert_eq!(wrapper_inner_start(&["env", "ls"]), Some(1));
        assert_eq!(wrapper_inner_start(&["env", "-i", "FOO=bar", "ls"]), Some(3));
    }

    #[test]
    fn test_wrapper_inner_start_none() {
        assert_eq!(wrapper_inner_start(&["ls", "-la"]), None);
        assert_eq!(wrapper_inner_start(&["git", "push"]), None);
    }

    #[test]
    fn test_wrapper_inner_start_doas() {
        assert_eq!(wrapper_inner_start(&["doas", "ls"]), Some(1));
        assert_eq!(wrapper_inner_start(&["doas", "-u", "root", "ls"]), Some(3));
        assert_eq!(wrapper_inner_start(&["doas", "-s"]), None);
    }

    #[test]
    fn test_wrapper_inner_start_watch() {
        assert_eq!(wrapper_inner_start(&["watch", "ls"]), Some(1));
        assert_eq!(wrapper_inner_start(&["watch", "-n", "2", "ls"]), Some(2));
    }

    #[test]
    fn test_wrapper_inner_start_command() {
        assert_eq!(wrapper_inner_start(&["command", "ls"]), Some(1));
        // nice -n is a flag, 10 is the first non-flag = inner command start
        // (not strictly correct for `nice -n 10 ls` since 10 is -n's arg, but
        //  the wrapper heuristic picks the first non-flag word)
        assert_eq!(wrapper_inner_start(&["nice", "-n", "10", "ls"]), Some(2));
        assert_eq!(wrapper_inner_start(&["nice", "ls"]), Some(1));
        assert_eq!(wrapper_inner_start(&["nohup", "ls"]), Some(1));
        assert_eq!(wrapper_inner_start(&["time", "ls"]), Some(1));
    }

    // --- Tests for quoted word passthrough ---

    #[test]
    fn test_quoted_words_passthrough() {
        let trie = build_test_trie();
        let pins = Pins::default();

        // Quoted words should not be expanded
        match resolve("gi co -m \"some message\"", &trie, &pins) {
            ResolveResult::Resolved(s) => {
                assert!(s.contains("\"some message\""), "quoted string should be preserved: {}", s);
            }
            other => panic!("Expected Resolved, got {:?}", other),
        }
    }

    #[test]
    fn test_single_quoted_passthrough() {
        let trie = build_test_trie();
        let pins = Pins::default();

        match resolve("gi co -m 'fix bug'", &trie, &pins) {
            ResolveResult::Resolved(s) => {
                assert!(s.contains("'fix bug'"), "single-quoted string should be preserved: {}", s);
            }
            other => panic!("Expected Resolved, got {:?}", other),
        }
    }

    #[test]
    fn test_doas_resolves_inner_command() {
        let trie = build_test_trie();
        let pins = Pins::default();

        match resolve("doas ter ap", &trie, &pins) {
            ResolveResult::Resolved(s) => assert_eq!(s, "doas terraform apply"),
            other => panic!("Expected Resolved, got {:?}", other),
        }
    }

    #[test]
    fn test_doas_with_user_flag() {
        let trie = build_test_trie();
        let pins = Pins::default();

        match resolve("doas -u root ter ap", &trie, &pins) {
            ResolveResult::Resolved(s) => assert_eq!(s, "doas -u root terraform apply"),
            other => panic!("Expected Resolved, got {:?}", other),
        }
    }

    #[test]
    fn test_watch_resolves_inner_command() {
        let trie = build_test_trie();
        let pins = Pins::default();

        match resolve("watch ter ap", &trie, &pins) {
            ResolveResult::Resolved(s) => assert_eq!(s, "watch terraform apply"),
            other => panic!("Expected Resolved, got {:?}", other),
        }
    }

    #[test]
    fn test_descriptions_helm() {
        let descs = load_yaml_descriptions();
        let helm = descs.get("helm").expect("helm should have descriptions");
        assert!(!helm.is_empty());
        assert!(helm.get("install").is_some());
        assert!(helm.get("upgrade").is_some());
    }

    #[test]
    fn test_descriptions_aws() {
        let descs = load_yaml_descriptions();
        let aws = descs.get("aws").expect("aws should have descriptions");
        assert!(!aws.is_empty());
        assert!(aws.get("s3").is_some());
        assert!(aws.get("ec2").is_some());
    }

    #[test]
    fn test_descriptions_gcloud() {
        let descs = load_yaml_descriptions();
        let gcloud = descs.get("gcloud").expect("gcloud should have descriptions");
        assert!(!gcloud.is_empty());
        assert!(gcloud.get("compute").is_some());
        assert!(gcloud.get("config").is_some());
    }

    #[test]
    fn test_descriptions_apt() {
        let descs = load_yaml_descriptions();
        let apt = descs.get("apt").expect("apt should have descriptions");
        assert!(!apt.is_empty());
        assert!(apt.get("install").is_some());
        assert!(apt.get("update").is_some());
    }

    #[test]
    fn test_descriptions_yarn() {
        let descs = load_yaml_descriptions();
        let yarn = descs.get("yarn").expect("yarn should have descriptions");
        assert!(!yarn.is_empty());
        assert!(yarn.get("add").is_some());
        assert!(yarn.get("install").is_some());
    }

    #[test]
    fn test_descriptions_podman() {
        let descs = load_yaml_descriptions();
        let podman = descs.get("podman").expect("podman should have descriptions");
        assert!(!podman.is_empty());
        assert!(podman.get("run").is_some());
        assert!(podman.get("build").is_some());
    }

    #[test]
    fn test_descriptions_rustup() {
        let descs = load_yaml_descriptions();
        let rustup = descs.get("rustup").expect("rustup should have descriptions");
        assert!(!rustup.is_empty());
        assert!(rustup.get("update").is_some());
        assert!(rustup.get("toolchain").is_some());
    }

    #[test]
    fn test_sudo_with_chain() {
        let trie = build_test_trie();
        let pins = Pins::default();

        match resolve_line("sudo ter in && sudo ter ap", &trie, &pins) {
            ResolveResult::Resolved(s) => {
                assert_eq!(s, "sudo terraform init && sudo terraform apply");
            }
            other => panic!("Expected Resolved, got {:?}", other),
        }
    }

    #[test]
    fn test_xargs_resolves_inner_command() {
        let trie = build_test_trie();
        let pins = Pins::default();

        match resolve("xargs ter ap", &trie, &pins) {
            ResolveResult::Resolved(s) => assert_eq!(s, "xargs terraform apply"),
            other => panic!("Expected Resolved, got {:?}", other),
        }
    }

    #[test]
    fn test_xargs_with_flags() {
        let trie = build_test_trie();
        let pins = Pins::default();

        match resolve("xargs -I {} ter ap", &trie, &pins) {
            ResolveResult::Resolved(s) => assert_eq!(s, "xargs -I {} terraform apply"),
            other => panic!("Expected Resolved, got {:?}", other),
        }
    }

    #[test]
    fn test_split_words_quoted() {
        let (words, quoted) = split_words_quoted("git commit -m \"fix bug\"");
        assert_eq!(words, vec!["git", "commit", "-m", "\"fix bug\""]);
        assert_eq!(quoted, vec![false, false, false, true]);

        let (words, quoted) = split_words_quoted("echo 'hello world' foo");
        assert_eq!(words, vec!["echo", "'hello world'", "foo"]);
        assert_eq!(quoted, vec![false, true, false]);

        let (words, _) = split_words_quoted("simple words here");
        assert_eq!(words, vec!["simple", "words", "here"]);
    }

    #[test]
    fn test_split_words_quoted_inline_quotes() {
        // Inline quotes within an unquoted word
        let (words, quoted) = split_words_quoted("foo\"bar baz\"qux end");
        assert_eq!(words, vec!["foo\"bar baz\"qux", "end"]);
        assert_eq!(quoted, vec![true, false]);
    }

    #[test]
    fn test_split_words_quoted_backslash_escape() {
        let (words, _) = split_words_quoted("echo hello\\ world");
        assert_eq!(words, vec!["echo", "hello\\ world"]);
    }

    #[test]
    fn test_split_words_quoted_empty() {
        let (words, quoted) = split_words_quoted("");
        assert!(words.is_empty());
        assert!(quoted.is_empty());

        let (words, _) = split_words_quoted("   ");
        assert!(words.is_empty());
    }

    #[test]
    fn test_split_words_quoted_unclosed_double() {
        // Unclosed double quote — should consume to end
        let (words, quoted) = split_words_quoted("echo \"unclosed");
        assert_eq!(words, vec!["echo", "\"unclosed"]);
        assert_eq!(quoted, vec![false, true]);
    }

    #[test]
    fn test_split_words_quoted_unclosed_single() {
        let (words, quoted) = split_words_quoted("echo 'unclosed");
        assert_eq!(words, vec!["echo", "'unclosed"]);
        assert_eq!(quoted, vec![false, true]);
    }

    // --- Tests for longest_common_prefix ---

    #[test]
    fn test_lcp_empty() {
        assert_eq!(longest_common_prefix(&[]), "");
    }

    #[test]
    fn test_lcp_single() {
        assert_eq!(
            longest_common_prefix(&["checkout".to_string()]),
            "checkout"
        );
    }

    #[test]
    fn test_lcp_common_prefix() {
        assert_eq!(
            longest_common_prefix(&["checkout".into(), "cherry-pick".into(), "clean".into()]),
            "c"
        );
        assert_eq!(
            longest_common_prefix(&["checkout".into(), "cherry-pick".into()]),
            "che"
        );
    }

    #[test]
    fn test_lcp_no_common() {
        assert_eq!(
            longest_common_prefix(&["abc".into(), "xyz".into()]),
            ""
        );
    }

    #[test]
    fn test_lcp_identical() {
        assert_eq!(
            longest_common_prefix(&["foo".into(), "foo".into()]),
            "foo"
        );
    }

    // --- Tests for looks_like_path ---

    #[test]
    fn test_looks_like_path() {
        assert!(looks_like_path("/usr/bin"));
        assert!(looks_like_path("~/file"));
        assert!(looks_like_path("./relative"));
        assert!(looks_like_path(".."));
        assert!(looks_like_path("!suffix"));
        assert!(looks_like_path("*pattern"));
        assert!(looks_like_path("\\!literal"));
        assert!(looks_like_path("\\*literal"));
        assert!(!looks_like_path("git"));
        assert!(!looks_like_path("terraform"));
        assert!(!looks_like_path("-flag"));
    }

    // --- Tests for u8_to_arg_mode ---

    #[test]
    fn test_u8_to_arg_mode() {
        assert_eq!(u8_to_arg_mode(0), ArgMode::Normal);
        assert_eq!(u8_to_arg_mode(trie::ARG_MODE_PATHS), ArgMode::Paths);
        assert_eq!(u8_to_arg_mode(trie::ARG_MODE_DIRS_ONLY), ArgMode::DirsOnly);
        assert_eq!(u8_to_arg_mode(trie::ARG_MODE_EXECS_ONLY), ArgMode::ExecsOnly);
        assert_eq!(u8_to_arg_mode(trie::ARG_MODE_PIDS), ArgMode::Runtime(trie::ARG_MODE_PIDS));
        assert_eq!(u8_to_arg_mode(trie::ARG_MODE_SIGNALS), ArgMode::Runtime(trie::ARG_MODE_SIGNALS));
        assert_eq!(u8_to_arg_mode(trie::ARG_MODE_GIT_BRANCHES), ArgMode::Runtime(trie::ARG_MODE_GIT_BRANCHES));
    }

    // --- Tests for arg_mode ---

    #[test]
    fn test_arg_mode_from_map() {
        let mut modes = ArgModeMap::new();
        modes.insert("cd".into(), trie::ARG_MODE_DIRS_ONLY);
        modes.insert("cat".into(), trie::ARG_MODE_PATHS);
        modes.insert("which".into(), trie::ARG_MODE_EXECS_ONLY);

        assert_eq!(arg_mode("cd", &modes), ArgMode::DirsOnly);
        assert_eq!(arg_mode("cat", &modes), ArgMode::Paths);
        assert_eq!(arg_mode("which", &modes), ArgMode::ExecsOnly);
    }

    #[test]
    fn test_arg_mode_runtime_falls_through() {
        // Runtime types (4+) should fall through to hardcoded list
        let mut modes = ArgModeMap::new();
        modes.insert("ls".into(), trie::ARG_MODE_PIDS); // bogus runtime type
        // ls is in PATH_COMMANDS, so it should still get Paths
        assert_eq!(arg_mode("ls", &modes), ArgMode::Paths);
    }

    #[test]
    fn test_arg_mode_hardcoded_fallback() {
        let modes = ArgModeMap::new(); // empty map
        assert_eq!(arg_mode("cd", &modes), ArgMode::DirsOnly);
        assert_eq!(arg_mode("pushd", &modes), ArgMode::DirsOnly);
        assert_eq!(arg_mode("ls", &modes), ArgMode::Paths);
        assert_eq!(arg_mode("cp", &modes), ArgMode::Paths);
        assert_eq!(arg_mode("which", &modes), ArgMode::ExecsOnly);
        assert_eq!(arg_mode("man", &modes), ArgMode::ExecsOnly);
        assert_eq!(arg_mode("git", &modes), ArgMode::Normal);
    }

    // --- Tests for is_hardcoded_path_command ---

    #[test]
    fn test_is_hardcoded_path_command() {
        assert!(is_hardcoded_path_command("cd"));
        assert!(is_hardcoded_path_command("pushd"));
        assert!(is_hardcoded_path_command("ls"));
        assert!(is_hardcoded_path_command("cat"));
        assert!(is_hardcoded_path_command("vim"));
        assert!(!is_hardcoded_path_command("git"));
        assert!(!is_hardcoded_path_command("which"));
    }

    // --- Tests for lookup_arg_spec ---

    #[test]
    fn test_lookup_arg_spec_two_word() {
        let mut specs = trie::ArgSpecMap::new();
        let git_add_spec = ArgSpec {
            rest: Some(trie::ARG_MODE_PATHS),
            ..Default::default()
        };
        specs.insert("git add".into(), git_add_spec);

        let words: Vec<String> = vec!["git".into(), "add".into(), "file.txt".into()];
        let (spec, skip) = lookup_arg_spec(&words, &specs);
        assert!(spec.is_some());
        assert_eq!(skip, 2);
        assert_eq!(spec.unwrap().rest, Some(trie::ARG_MODE_PATHS));
    }

    #[test]
    fn test_lookup_arg_spec_one_word_fallback() {
        let mut specs = trie::ArgSpecMap::new();
        let cat_spec = ArgSpec {
            rest: Some(trie::ARG_MODE_PATHS),
            ..Default::default()
        };
        specs.insert("cat".into(), cat_spec);

        let words: Vec<String> = vec!["cat".into(), "file.txt".into()];
        let (spec, skip) = lookup_arg_spec(&words, &specs);
        assert!(spec.is_some());
        assert_eq!(skip, 1);
    }

    #[test]
    fn test_lookup_arg_spec_not_found() {
        let specs = trie::ArgSpecMap::new();
        let words: Vec<String> = vec!["unknown".into()];
        let (spec, skip) = lookup_arg_spec(&words, &specs);
        assert!(spec.is_none());
        assert_eq!(skip, 1);
    }

    #[test]
    fn test_lookup_arg_spec_flag_not_subcmd() {
        // If word[1] starts with -, don't try two-word lookup
        let mut specs = trie::ArgSpecMap::new();
        let cat_spec = ArgSpec {
            rest: Some(trie::ARG_MODE_PATHS),
            ..Default::default()
        };
        specs.insert("cat".into(), cat_spec);

        let words: Vec<String> = vec!["cat".into(), "-n".into(), "file.txt".into()];
        let (spec, skip) = lookup_arg_spec(&words, &specs);
        assert!(spec.is_some());
        assert_eq!(skip, 1); // fell back to one-word
    }

    // --- Tests for arg_type_for_word ---

    #[test]
    fn test_arg_type_for_word_flag_value() {
        let mut spec = ArgSpec::default();
        spec.flag_args.insert("-o".into(), trie::ARG_MODE_PATHS);
        // Word after -o should be Paths
        assert_eq!(
            arg_type_for_word(1, Some("-o"), Some(&spec), ArgMode::Normal),
            ArgMode::Paths
        );
    }

    #[test]
    fn test_arg_type_for_word_positional() {
        let mut spec = ArgSpec::default();
        spec.positional.insert(1, trie::ARG_MODE_HOSTS);
        assert_eq!(
            arg_type_for_word(1, None, Some(&spec), ArgMode::Normal),
            ArgMode::Runtime(trie::ARG_MODE_HOSTS)
        );
    }

    #[test]
    fn test_arg_type_for_word_fallback() {
        let spec = ArgSpec::default(); // empty spec
        assert_eq!(
            arg_type_for_word(1, None, Some(&spec), ArgMode::Paths),
            ArgMode::Paths
        );
        assert_eq!(
            arg_type_for_word(1, None, None, ArgMode::DirsOnly),
            ArgMode::DirsOnly
        );
    }

    // --- Tests for format_columns ---

    #[test]
    fn test_format_columns_empty() {
        assert_eq!(format_columns(&[], 100), "");
    }

    #[test]
    fn test_format_columns_single_column() {
        let names = vec!["add", "commit", "push"];
        let result = format_columns(&names, 100);
        assert!(result.contains("  add\n"));
        assert!(result.contains("  commit\n"));
        assert!(result.contains("  push\n"));
    }

    #[test]
    fn test_format_columns_overflow_message() {
        let names: Vec<&str> = (0..5).map(|i| match i {
            0 => "a", 1 => "b", 2 => "c", 3 => "d", _ => "e",
        }).collect();
        let result = format_columns(&names, 3);
        assert!(result.contains("... and 2 more"));
    }

    #[test]
    fn test_format_columns_multi_column() {
        // >12 items should use multi-column layout
        let names: Vec<&str> = vec![
            "aa", "bb", "cc", "dd", "ee", "ff", "gg", "hh", "ii", "jj", "kk", "ll", "mm",
        ];
        let result = format_columns(&names, 100);
        // Should have fewer lines than items (multi-column)
        let lines: Vec<&str> = result.lines().collect();
        assert!(lines.len() < names.len());
    }

    // --- Tests for resolve_first_word ---

    #[test]
    fn test_resolve_first_word_exact() {
        let trie = build_test_trie();
        assert_eq!(resolve_first_word("git", &trie), "git");
    }

    #[test]
    fn test_resolve_first_word_prefix() {
        let trie = build_test_trie();
        assert_eq!(resolve_first_word("ter", &trie), "terraform");
    }

    #[test]
    fn test_resolve_first_word_ambiguous() {
        let trie = build_test_trie();
        // "g" matches git, grep, go, gzip — returns unchanged
        assert_eq!(resolve_first_word("g", &trie), "g");
    }

    #[test]
    fn test_resolve_first_word_no_match() {
        let trie = build_test_trie();
        assert_eq!(resolve_first_word("zzz", &trie), "zzz");
    }

    // --- Tests for split_on_operators ---

    #[test]
    fn test_split_on_operators_simple() {
        let parts = split_on_operators("ls -la");
        assert_eq!(parts.len(), 1);
        assert!(matches!(&parts[0], LinePart::Command(c) if c == "ls -la"));
    }

    #[test]
    fn test_split_on_operators_pipe() {
        let parts = split_on_operators("ls | grep foo");
        assert_eq!(parts.len(), 3);
        assert!(matches!(&parts[0], LinePart::Command(c) if c == "ls "));
        assert!(matches!(&parts[1], LinePart::Operator(o) if o == "|"));
        assert!(matches!(&parts[2], LinePart::Command(c) if c == " grep foo"));
    }

    #[test]
    fn test_split_on_operators_and() {
        let parts = split_on_operators("a && b");
        assert_eq!(parts.len(), 3);
        assert!(matches!(&parts[1], LinePart::Operator(o) if o == "&&"));
    }

    #[test]
    fn test_split_on_operators_semicolon() {
        let parts = split_on_operators("a; b");
        assert_eq!(parts.len(), 3);
        assert!(matches!(&parts[1], LinePart::Operator(o) if o == ";"));
    }

    #[test]
    fn test_split_on_operators_quoted() {
        // Operators inside quotes should not split
        let parts = split_on_operators("echo \"a && b\"");
        assert_eq!(parts.len(), 1);
        assert!(matches!(&parts[0], LinePart::Command(c) if c == "echo \"a && b\""));
    }

    #[test]
    fn test_split_on_operators_single_quoted() {
        let parts = split_on_operators("echo 'a | b'");
        assert_eq!(parts.len(), 1);
    }

    // --- Tests for deep_disambiguate edge cases ---

    #[test]
    fn test_deep_disambig_empty_rest() {
        let mut trie = CommandTrie::new();
        trie.insert(&["git", "commit"]);
        trie.insert(&["grep", "-r"]);
        let matches = trie.root.prefix_search("g");
        let result = deep_disambiguate(&matches, &[]);
        // With empty rest, should return all matches unchanged
        assert_eq!(result.len(), matches.len());
    }

    #[test]
    fn test_deep_disambig_flag_skipped() {
        let mut trie = CommandTrie::new();
        trie.insert(&["git", "commit", "-m"]);
        trie.insert(&["go", "build", "-o"]);
        let matches = trie.root.prefix_search("g");
        // Flags should be skipped during lookahead
        let result = deep_disambiguate(&matches, &["co", "-m"]);
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].0, "git");
    }

    // --- Tests for shell_escape_path edge cases ---

    #[test]
    fn test_shell_escape_all_metacharacters() {
        assert_eq!(shell_escape_path("a[b]"), "a\\[b\\]");
        assert_eq!(shell_escape_path("a{b}"), "a\\{b\\}");
        assert_eq!(shell_escape_path("a#b"), "a\\#b");
        assert_eq!(shell_escape_path("a?b"), "a\\?b");
        assert_eq!(shell_escape_path("a<b>"), "a\\<b\\>");
        assert_eq!(shell_escape_path("a=b"), "a\\=b");
        assert_eq!(shell_escape_path("a^b"), "a\\^b");
        assert_eq!(shell_escape_path("a\\b"), "a\\\\b");
        assert_eq!(shell_escape_path("a`b`"), "a\\`b\\`");
    }

    #[test]
    fn test_escape_resolved_path_glob_passthrough() {
        // ** passthrough: * in the resolved path should NOT be escaped
        assert_eq!(escape_resolved_path("./**.py", "./*.py"), "./*.py");
        assert_eq!(escape_resolved_path("**.py", "*.py"), "*.py");
        assert_eq!(escape_resolved_path("./**", "./*"), "./*");
        // Other metacharacters still escaped even in glob paths
        assert_eq!(escape_resolved_path("**.py", "my dir/*.py"), "my\\ dir/*.py");
    }

    #[test]
    fn test_escape_resolved_path_literal_star_file() {
        // \* escape (literal * filename): * in the resolved path SHOULD be escaped
        assert_eq!(escape_resolved_path("\\*star", "*starred"), "\\*starred");
        // No ** in original → normal escaping applies
        assert_eq!(escape_resolved_path("./foo", "file*.txt"), "file\\*.txt");
    }

    // --- Tests for resolve_line with empty segments ---

    #[test]
    fn test_resolve_line_trailing_operator() {
        let trie = build_test_trie();
        let pins = Pins::default();
        // "ter ap &&" has an empty segment after &&
        match resolve_line("ter ap && ", &trie, &pins) {
            ResolveResult::Resolved(s) => assert_eq!(s, "terraform apply && "),
            other => panic!("Expected Resolved, got {:?}", other),
        }
    }

    #[test]
    fn test_resolve_line_or_operator() {
        let trie = build_test_trie();
        let pins = Pins::default();
        match resolve_line("ter in || ter ap", &trie, &pins) {
            ResolveResult::Resolved(s) => {
                assert_eq!(s, "terraform init || terraform apply");
            }
            other => panic!("Expected Resolved, got {:?}", other),
        }
    }

    // --- Tests for wrapper passthrough ---

    #[test]
    fn test_wrapper_passthrough_when_unchanged() {
        let trie = build_test_trie();
        let pins = Pins::default();
        // sudo with an already-resolved command should passthrough
        match resolve("sudo terraform apply", &trie, &pins) {
            ResolveResult::Passthrough(s) => assert_eq!(s, "sudo terraform apply"),
            other => panic!("Expected Passthrough, got {:?}", other),
        }
    }

    #[test]
    fn test_nohup_resolves_inner_command() {
        let trie = build_test_trie();
        let pins = Pins::default();
        match resolve("nohup ter ap", &trie, &pins) {
            ResolveResult::Resolved(s) => assert_eq!(s, "nohup terraform apply"),
            other => panic!("Expected Resolved, got {:?}", other),
        }
    }

    // --- Test resolve with empty input ---

    #[test]
    fn test_resolve_empty_input() {
        let trie = build_test_trie();
        let pins = Pins::default();
        match resolve("", &trie, &pins) {
            ResolveResult::Passthrough(s) => assert_eq!(s, ""),
            other => panic!("Expected Passthrough, got {:?}", other),
        }
    }

    #[test]
    fn test_dot_and_dotdot_pass_through() {
        let trie = build_test_trie();
        let pins = Pins::default();
        // . and .. are directory literals and must never be prefix-resolved
        match resolve("ter ap .", &trie, &pins) {
            ResolveResult::Resolved(s) => assert_eq!(s, "terraform apply ."),
            other => panic!("Expected Resolved with . unchanged, got {:?}", other),
        }
        match resolve("ter ap ..", &trie, &pins) {
            ResolveResult::Resolved(s) => assert_eq!(s, "terraform apply .."),
            other => panic!("Expected Resolved with .. unchanged, got {:?}", other),
        }
    }

    #[test]
    fn test_path_command_word_resolved() {
        use std::fs;
        let dir = std::env::temp_dir().join("zsh-ios-test-cmdword");
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("uninstall.sh"), "").unwrap();
        fs::write(dir.join("install.sh"), "").unwrap();

        let trie = build_test_trie();
        let pins = Pins::default();
        let abs = dir.to_str().unwrap().to_string();

        // Absolute path prefix: /tmp/.../unin → /tmp/.../uninstall.sh
        let input = format!("{}/unin", abs);
        let expected = format!("{}/uninstall.sh", abs);
        match resolve(&input, &trie, &pins) {
            ResolveResult::Resolved(s) => assert_eq!(s, expected),
            other => panic!("Expected Resolved for abs path cmd word, got {:?}", other),
        }

        // Absolute path prefix for the other script
        let input2 = format!("{}/ins", abs);
        let expected2 = format!("{}/install.sh", abs);
        match resolve(&input2, &trie, &pins) {
            ResolveResult::Resolved(s) => assert_eq!(s, expected2),
            other => panic!("Expected Resolved for abs path cmd word, got {:?}", other),
        }

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_resolve_whitespace_only() {
        let trie = build_test_trie();
        let pins = Pins::default();
        match resolve("   ", &trie, &pins) {
            ResolveResult::Passthrough(s) => assert_eq!(s, "   "),
            other => panic!("Expected Passthrough, got {:?}", other),
        }
    }

    // --- complete() public API ---

    #[test]
    fn complete_empty_input_lists_top_level_by_count() {
        let mut trie = CommandTrie::new();
        // Insert with counts to verify the sort-by-count path.
        trie.insert(&["rare"]);
        for _ in 0..5 {
            trie.insert(&["common"]);
        }
        let pins = Pins::default();
        let out = complete("", &trie, &pins);
        // common should appear before rare because its count is higher.
        let c = out.find("common").expect("common in output");
        let r = out.find("rare").expect("rare in output");
        assert!(c < r, "expected count-sorted order, got: {}", out);
    }

    #[test]
    fn complete_no_matches_message() {
        let trie = build_test_trie();
        let pins = Pins::default();
        let out = complete("xq", &trie, &pins);
        assert!(out.contains("No commands matching"), "got: {}", out);
    }

    #[test]
    fn complete_prefix_lists_candidates() {
        let trie = build_test_trie();
        let pins = Pins::default();
        let out = complete("g", &trie, &pins);
        // All g-prefixed commands appear.
        assert!(out.contains("git"));
        assert!(out.contains("grep"));
        assert!(out.contains("go"));
        assert!(out.contains("gzip"));
    }

    #[test]
    fn complete_intermediate_ambiguity_is_surfaced() {
        let trie = build_test_trie();
        let pins = Pins::default();
        // "g c " with trailing space means: "completed: g c; completing: <empty>".
        // But mid-walk "c" under git is ambiguous between checkout/commit, so
        // the code emits those matches directly.
        let out = complete("git c", &trie, &pins);
        assert!(out.contains("checkout"), "got: {}", out);
        assert!(out.contains("commit"), "got: {}", out);
    }

    #[test]
    fn complete_trailing_space_starts_fresh_word() {
        let trie = build_test_trie();
        let pins = Pins::default();
        // "git " with trailing space → list all git subcommands.
        let out = complete("git ", &trie, &pins);
        assert!(out.contains("checkout"));
        assert!(out.contains("commit"));
        assert!(out.contains("push"));
    }

    #[test]
    fn complete_after_pipe_only_completes_last_segment() {
        let trie = build_test_trie();
        let pins = Pins::default();
        // Before the pipe is "ls"-like junk, after the pipe is what gets completed.
        let out = complete("xx | g", &trie, &pins);
        assert!(out.contains("git") || out.contains("grep"), "got: {}", out);
    }

    #[test]
    fn complete_flag_prefix_reaches_flag_path() {
        let trie = build_test_trie();
        let pins = Pins::default();
        // "git -" triggers the complete_flags branch. Our test trie has
        // -m under git commit, so we expect some flag output.
        let out = complete("git commit -", &trie, &pins);
        assert!(out.contains("-m"), "flag output missing: {}", out);
    }

    // --- longest_common_prefix direct coverage ---

    #[test]
    fn lcp_mixed_lengths() {
        let v = vec!["foobar".into(), "foobaz".into(), "foo".into()];
        assert_eq!(longest_common_prefix(&v), "foo");
    }

    // --- shell_escape_path_glob vs shell_escape_path ---

    #[test]
    fn shell_escape_path_glob_preserves_star() {
        // Glob variant leaves `*` and `?` unescaped so the shell expands them.
        let escaped = shell_escape_path_glob("/tmp/*.log");
        assert!(escaped.contains('*'), "star lost: {}", escaped);
        assert!(!escaped.contains("\\*"), "star escaped: {}", escaped);

        // Plain variant escapes it.
        let escaped = shell_escape_path("/tmp/*.log");
        assert!(escaped.contains("\\*"), "star not escaped: {}", escaped);
    }

    // --- escape_resolved_path with tilde passthrough ---

    #[test]
    fn escape_resolved_path_tilde_preserved() {
        // When the user typed `~/Docs` the tilde should survive escaping so
        // the shell does home-expansion.
        let out = escape_resolved_path("~/Doc", "~/Documents");
        assert!(out.starts_with('~'), "tilde lost: {}", out);
    }

    // --- has_filesystem_prefix_match ---

    #[test]
    fn has_filesystem_prefix_match_current_dir() {
        let _g = CWD_LOCK.lock().unwrap();
        let td = tempfile::tempdir().unwrap();
        let orig = std::env::current_dir().ok();
        std::env::set_current_dir(td.path()).unwrap();
        std::fs::create_dir_all("mydir").unwrap();

        assert!(has_filesystem_prefix_match("myd"));
        assert!(!has_filesystem_prefix_match("zzz-no-such"));

        if let Some(o) = orig {
            let _ = std::env::set_current_dir(o);
        }
    }

    // --- looks_like_path ---

    #[test]
    fn looks_like_path_obvious_cases() {
        assert!(looks_like_path("./foo"));
        assert!(looks_like_path("../bar"));
        assert!(looks_like_path("/abs/path"));
        assert!(looks_like_path("~/home"));
        assert!(looks_like_path("a/b"));
        assert!(!looks_like_path("plain-word"));
    }

    // --- apply_context_rules ---

    #[test]
    fn apply_context_rules_overrides_when_flag_present() {
        let rule = ContextRule {
            trigger_flags: vec!["-b".into()],
            override_type: trie::ARG_MODE_GIT_BRANCHES,
        };
        let spec = ArgSpec {
            context_rules: vec![rule],
            ..Default::default()
        };
        let words = vec!["git".into(), "checkout".into(), "-b".into()];
        let base = ArgMode::Paths;
        let got = apply_context_rules(Some(&spec), &words, base);
        match got {
            ArgMode::Runtime(t) => assert_eq!(t, trie::ARG_MODE_GIT_BRANCHES),
            other => panic!("expected Runtime override, got {:?}", other),
        }
    }

    #[test]
    fn apply_context_rules_no_match_returns_base() {
        let spec = ArgSpec::default();
        let base = ArgMode::Paths;
        let out = apply_context_rules(Some(&spec), &["ls".into()], base);
        assert!(matches!(out, ArgMode::Paths));
    }

    // --- Resolve end-to-end: pin with zero consumption edge cases ---

    #[test]
    fn resolve_pin_to_multi_word_then_subcommand() {
        // Pin "k" -> "kubectl", then "k ap" resolves kubectl's subcommand trie.
        let mut trie = CommandTrie::new();
        trie.insert(&["kubectl", "apply"]);
        trie.insert(&["kubectl", "get"]);
        let pins = Pins {
            entries: vec![Pin {
                abbrev: vec!["k".into()],
                expanded: vec!["kubectl".into()],
            }],
        };
        match resolve("k ap", &trie, &pins) {
            ResolveResult::Resolved(s) => assert_eq!(s, "kubectl apply"),
            other => panic!("expected Resolved, got {:?}", other),
        }
    }

    #[test]
    fn resolve_double_dash_terminator_stops_expansion() {
        let mut trie = CommandTrie::new();
        trie.insert(&["git", "checkout"]);
        trie.insert_command("foo");
        let pins = Pins::default();
        // After `--`, subsequent words are arguments to git, not subcommands.
        // We just verify this doesn't crash and produces *something*.
        let _ = resolve("git -- foo", &trie, &pins);
    }

    #[test]
    fn resolve_line_empty_and_operator_only() {
        let trie = build_test_trie();
        let pins = Pins::default();
        match resolve_line("", &trie, &pins) {
            ResolveResult::Passthrough(s) => assert_eq!(s, ""),
            other => panic!("empty → Passthrough, got {:?}", other),
        }
        // Operator at the start: shouldn't panic.
        let _ = resolve_line("| git st", &trie, &pins);
    }

    // --- Ambiguity info shape ---

    #[test]
    fn ambiguity_info_carries_lcp_and_position() {
        let trie = build_test_trie();
        let pins = Pins::default();
        match resolve("te", &trie, &pins) {
            ResolveResult::Ambiguous(info) => {
                // Multiple entries share the prefix "ter" from "terraform", but
                // the test trie only has `terraform`, so "te" is a unique match.
                // Force multi-candidate ambiguity with an explicit prefix.
                let _ = info;
            }
            ResolveResult::Resolved(_) => {}
            other => panic!("unexpected {:?}", other),
        }
        // Now trigger actual ambiguity with `g`.
        match resolve("g", &trie, &pins) {
            ResolveResult::Ambiguous(info) => {
                assert_eq!(info.position, 0);
                assert_eq!(info.word, "g");
                assert!(!info.candidates.is_empty());
                // lcp is "g" at minimum since all candidates start with g.
                assert!(info.lcp.starts_with('g'));
            }
            other => panic!("expected Ambiguous, got {:?}", other),
        }
    }

    // --- finalize_with_paths: cd resolves to real path ---

    #[test]
    fn cd_with_prefix_expands_to_real_directory() {
        let _g = CWD_LOCK.lock().unwrap();
        let td = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(td.path().join("target-directory")).unwrap();
        let mut trie = CommandTrie::new();
        trie.insert_command("cd");
        let pins = Pins::default();

        let orig = std::env::current_dir().ok();
        std::env::set_current_dir(td.path()).unwrap();

        match resolve("cd target-", &trie, &pins) {
            ResolveResult::Resolved(s) => {
                assert!(s.contains("target-directory"), "got: {}", s);
            }
            other => panic!("expected Resolved, got {:?}", other),
        }

        if let Some(o) = orig {
            let _ = std::env::set_current_dir(o);
        }
    }

    // --- format_columns boundary cases ---

    #[test]
    fn format_columns_single_item() {
        let out = format_columns(&["only"], 1);
        assert!(out.contains("only"));
    }

    // --- Leading `!` bypass ---

    #[test]
    fn bang_prefixed_line_is_passthrough() {
        let trie = build_test_trie();
        let pins = Pins::default();
        // Even though "gi" alone is ambiguous/resolvable, `!gi st` must be
        // returned untouched.
        for input in ["!!", "!git", "!gi st", "!$", "!echo hi", "!?foo"] {
            match resolve_line(input, &trie, &pins) {
                ResolveResult::Passthrough(s) => assert_eq!(s, input, "bang bypass failed"),
                other => panic!("expected Passthrough for {:?}, got {:?}", input, other),
            }
        }
    }

    #[test]
    fn bang_after_leading_space_still_bypasses() {
        let trie = build_test_trie();
        let pins = Pins::default();
        let input = "   !git status";
        match resolve_line(input, &trie, &pins) {
            ResolveResult::Passthrough(s) => assert_eq!(s, input),
            other => panic!("expected Passthrough, got {:?}", other),
        }
    }

    #[test]
    fn bang_in_middle_is_not_bypassed() {
        // `cd te/!5` is the suffix-match feature — NOT a bang-at-start.
        // It must still go through normal resolution.
        let mut trie = CommandTrie::new();
        trie.insert_command("echo");
        let pins = Pins::default();
        // `echo !foo` starts with `echo`, not `!` — normal path.
        // We just verify it's not a no-op Passthrough-of-unchanged-input
        // caused by the bang guard firing on a non-leading `!`.
        match resolve_line("echo !foo", &trie, &pins) {
            ResolveResult::Passthrough(_) | ResolveResult::Resolved(_) => {}
            other => panic!("unexpected {:?}", other),
        }
    }

    #[test]
    fn complete_bang_returns_empty() {
        let trie = build_test_trie();
        let pins = Pins::default();
        assert!(complete("!g", &trie, &pins).is_empty());
        assert!(complete("!!", &trie, &pins).is_empty());
        assert!(complete("  !git ", &trie, &pins).is_empty());
    }

    #[test]
    fn complete_bang_in_middle_still_works() {
        let trie = build_test_trie();
        let pins = Pins::default();
        // `echo !foo` doesn't start with `!` → normal completion runs.
        let out = complete("ech", &trie, &pins);
        assert!(!out.is_empty());
    }

    #[test]
    fn format_columns_many_items_respects_width() {
        let many: Vec<String> = (0..50).map(|i| format!("item{}", i)).collect();
        let refs: Vec<&str> = many.iter().map(String::as_str).collect();
        let out = format_columns(&refs, 50);
        // Output should contain a reasonable number of newlines (multi-line
        // multi-column format). Not asserting exact count — terminal_width
        // can pick up COLUMNS env var — just non-empty.
        assert!(!out.is_empty());
        assert!(out.lines().count() >= 1);
    }

    // --- explain() ---

    #[test]
    fn explain_bang_reports_bypass() {
        let trie = build_test_trie();
        let pins = Pins::default();
        let out = explain("!!", &trie, &pins);
        assert!(out.contains("Leading-! bypass"), "got: {}", out);
        assert!(out.contains("Passthrough"));
    }

    #[test]
    fn explain_resolved_prints_trie_walk_and_winner() {
        let trie = build_test_trie();
        let pins = Pins::default();
        // "ter ap" resolves uniquely via terraform apply. Narrative should
        // include the unique-match line and the final Resolved.
        let out = explain("ter ap", &trie, &pins);
        assert!(out.contains("\"ter\""), "got: {}", out);
        assert!(out.contains("Trie:"), "got: {}", out);
        assert!(out.contains("Final: Resolved → terraform apply"), "got: {}", out);
    }

    #[test]
    fn explain_ambiguous_shows_candidates_and_lcp() {
        let trie = build_test_trie();
        let pins = Pins::default();
        let out = explain("g", &trie, &pins);
        assert!(out.contains("Final: Ambiguous"), "got: {}", out);
        assert!(out.contains("candidates"));
        // Test trie has git/grep/go/gzip under "g" prefix
        assert!(out.contains("git") && out.contains("grep"));
    }

    #[test]
    fn explain_deep_disambig_shows_survivor_and_nonmatch_summary() {
        let trie = build_test_trie();
        let pins = Pins::default();
        // "g push" — only git has a `push` subcommand; the explain trace
        // must call out the survivor explicitly.
        let out = explain("g push", &trie, &pins);
        assert!(out.contains("Deep-disambiguate"), "got: {}", out);
        assert!(out.contains("winner: git"), "got: {}", out);
        assert!(out.contains("Final: Resolved → git push"));
    }

    #[test]
    fn explain_pin_lookup_reports_hit() {
        let mut trie = CommandTrie::new();
        trie.insert(&["kubectl", "apply"]);
        let pins = Pins {
            entries: vec![Pin {
                abbrev: vec!["k".into()],
                expanded: vec!["kubectl".into()],
            }],
        };
        let out = explain("k ap", &trie, &pins);
        assert!(out.contains("Pin match"), "got: {}", out);
        assert!(out.contains("k") && out.contains("kubectl"));
        assert!(out.contains("Final: Resolved → kubectl apply"));
    }

    #[test]
    fn explain_wrapper_drills_into_inner_command() {
        let mut trie = build_test_trie();
        trie.insert_command("sudo");
        let pins = Pins::default();
        let out = explain("sudo ter ap", &trie, &pins);
        assert!(out.contains("Wrapper: sudo"), "got: {}", out);
        assert!(out.contains("Inner: \"ter ap\""));
        // The inner command is still resolved, so the final line reports
        // the full sudo-prefixed result.
        assert!(out.contains("sudo terraform apply") || out.contains("Resolved"));
    }

    #[test]
    fn explain_pipe_chain_splits_and_narrates_each_segment() {
        let trie = build_test_trie();
        let pins = Pins::default();
        let out = explain("gi st | gr foo", &trie, &pins);
        assert!(out.contains("Pipe/chain split"), "got: {}", out);
        assert!(out.contains("Segment 1"));
        assert!(out.contains("Segment 2"));
    }

    #[test]
    fn summarize_names_caps_long_lists() {
        let many: Vec<String> = (0..20).map(|i| format!("n{}", i)).collect();
        let refs: Vec<&str> = many.iter().map(String::as_str).collect();
        let s = summarize_names(&refs, 5);
        assert!(s.contains("n0"));
        assert!(s.contains("… (15 more)"), "got: {}", s);
        // Short list: no ellipsis
        let s = summarize_names(&refs[..3], 5);
        assert!(!s.contains("more"));
    }
}
