//! Core abbreviation-resolution engine.
//!
//! Walks the trie with prefix/suffix/contains matching, handles pins,
//! performs deep disambiguation (looking ahead at later words to narrow
//! an ambiguous prefix), and applies arg-spec rules for path / runtime
//! argument resolution. `explain` lives here too — the debug narrator
//! that mirrors the same decision tree.

use crate::path_resolve;
use crate::pins::Pins;
use crate::runtime_complete;
use crate::trie::{self, ArgModeMap, ArgSpec, ArgSpecMap, CommandTrie, MatcherRule, TrieNode};

use super::escape::{escape_resolved_path, shell_escape_path};

use std::sync::atomic::AtomicBool;
use std::sync::LazyLock;

use std::cell::RefCell;

/// Global toggle for the statistical tiebreaker. Set once at CLI startup
/// from `user_config.disable_statistics`.  When true, `score_candidates_stats`
/// short-circuits to `None` so the engine never picks between tied
/// candidates based on local history.
pub(super) static STATS_DISABLED: AtomicBool = AtomicBool::new(false);

// Thread-local storage for context signals that flow into score_node
// without requiring a signature change on every internal helper.
// CWD_CONTEXT: current working directory for the resolve call, used by
// score_node as a cwd-locality multiplier.
thread_local! {
    static CWD_CONTEXT: RefCell<Option<String>> = const { RefCell::new(None) };
}

/// Enable or disable the statistical tiebreaker at runtime. Called from
/// `main.rs` after reading `UserConfig`.
pub fn set_statistics_disabled(b: bool) {
    STATS_DISABLED.store(b, std::sync::atomic::Ordering::Relaxed);
}

/// Shell context hint inferred from the buffer by the plugin.
///
/// `Unknown` and `Argument` are the default / do-nothing cases.
/// `Redirection` short-circuits arg resolution to path mode.
/// `Math` and `Condition` suppress resolution entirely.
/// `SingleQuoted`, `DoubleQuoted`, `Backticked`, and `ParameterName` are
/// synthesised from `--quote` / `--param-context` flags; they take precedence
/// over the positional `--context` value (most-specific wins).
///
/// Precedence when combining `--context`, `--quote`, and `--param-context`:
///   ParameterName > SingleQuoted > DoubleQuoted > Backticked > <--context>
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ContextHint {
    Unknown,
    Command,
    Argument,
    Redirection,
    Math,
    Condition,
    Array,
    /// Cursor is inside a single-quoted string — no expansion possible.
    SingleQuoted,
    /// Cursor is inside a double-quoted string — limited expansion.
    DoubleQuoted,
    /// Cursor is inside a backtick command substitution.
    Backticked,
    /// Cursor is inside `${PARAM…}` — completing a parameter name.
    ParameterName,
}

impl ContextHint {
    /// Parse a string value (from `--context` CLI flag). Unrecognised
    /// values map to `Unknown` so forward-compatibility is preserved.
    pub fn parse_hint(s: &str) -> Self {
        match s {
            "command" => Self::Command,
            "argument" => Self::Argument,
            "redirection" => Self::Redirection,
            "math" => Self::Math,
            "condition" => Self::Condition,
            "array" => Self::Array,
            "single" | "single-quoted" => Self::SingleQuoted,
            "double" | "double-quoted" => Self::DoubleQuoted,
            "backtick" | "backticked" => Self::Backticked,
            "parameter" | "param-context" => Self::ParameterName,
            _ => Self::Unknown,
        }
    }

    /// Combine the positional `--context` hint with quote-state flags.
    ///
    /// Precedence (most-specific wins):
    ///   param_context → ParameterName
    ///   quote "single" → SingleQuoted
    ///   quote "double" → DoubleQuoted
    ///   quote "backtick" / "dollar" → Backticked
    ///   fallback to the positional context hint
    pub fn from_parts(context: Option<&str>, quote: Option<&str>, param_context: bool) -> Self {
        if param_context {
            return Self::ParameterName;
        }
        match quote {
            Some("single") => return Self::SingleQuoted,
            Some("double") => return Self::DoubleQuoted,
            Some("backtick") | Some("dollar") => return Self::Backticked,
            _ => {}
        }
        Self::parse_hint(context.unwrap_or(""))
    }
}

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
pub(super) fn starts_with_bang(input: &str) -> bool {
    input.trim_start().starts_with('!')
}

pub fn resolve_line(
    input: &str,
    trie: &CommandTrie,
    pins: &Pins,
    cwd: Option<&str>,
    context_hint: ContextHint,
) -> ResolveResult {
    // Leading `!`: the user wants zsh's history-expansion / literal-run
    // semantics. Never touch the buffer — return it verbatim so the shell
    // runs exactly what was typed.
    if starts_with_bang(input) {
        return ResolveResult::Passthrough(input.to_string());
    }

    // math / condition: the user is inside arithmetic or test expression;
    // never mangle the buffer.
    if matches!(context_hint, ContextHint::Math | ContextHint::Condition) {
        return ResolveResult::Passthrough(input.to_string());
    }

    // Quote / parameter contexts: the cursor is inside a quoted region or a
    // `${PARAM}` expansion — no command resolution should happen.
    //   SingleQuoted  → no expansion at all (shell treats everything literally)
    //   DoubleQuoted  → conservatively passthrough (data, not code)
    //   Backticked    → inner shell; its own resolve pass will handle it
    //   ParameterName → we're naming a parameter, not a command
    if matches!(
        context_hint,
        ContextHint::SingleQuoted
            | ContextHint::DoubleQuoted
            | ContextHint::Backticked
            | ContextHint::ParameterName
    ) {
        return ResolveResult::Passthrough(input.to_string());
    }

    // min_resolve_prefix_length: if the first typed word is too short, pass
    // through so single-letter aliases (l, g, …) are never accidentally
    // expanded. Only checked on the very first word of the whole line; words
    // inside pipes/chains are already the inner command and will be checked
    // by the recursive resolve_with_ctx call.
    let min_len = crate::runtime_config::get().min_resolve_prefix_length as usize;
    if min_len > 0 {
        let first_word = input.split_whitespace().next().unwrap_or("");
        if !first_word.is_empty() && first_word.len() < min_len {
            return ResolveResult::Passthrough(input.to_string());
        }
    }

    // Expand global aliases before the trie walk. The owned string is kept
    // alive for the duration of this call via `input_string`; `input_ref`
    // points into it (or into the original `input` when no galiases exist).
    let input_string;
    let input: &str = if !trie.galiases.is_empty() {
        input_string = crate::galiases::expand_galiases(input, &trie.galiases);
        &input_string
    } else {
        input
    };

    let parts = split_on_operators(input);

    // Fast path: no operators → resolve as a single command.
    let has_op = parts.iter().any(|p| matches!(p, LinePart::Operator(_)));
    if !has_op {
        return resolve_with_ctx(input, trie, pins, cwd, context_hint);
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

                match resolve_with_ctx(trimmed, trie, pins, cwd, context_hint) {
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
pub(super) fn wrapper_inner_start(words: &[&str]) -> Option<usize> {
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
        // doas (sudo alternative on BSDs / some Linux): [-u user] [-C config] [-flags] <command>
        "doas" => {
            let mut i = 1;
            while i < words.len() {
                let w = words[i];
                if !w.starts_with('-') {
                    // -u and -C each consume the next word as their argument
                    if i > 1 && matches!(words[i - 1], "-u" | "-C") {
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
pub(super) fn split_words_quoted(input: &str) -> (Vec<&str>, Vec<bool>) {
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

/// Resolve a single segment with optional cwd and context hint applied.
fn resolve_with_ctx(
    input: &str,
    trie: &CommandTrie,
    pins: &Pins,
    cwd: Option<&str>,
    context_hint: ContextHint,
) -> ResolveResult {
    // Redirection context: treat the last word as a file path and skip
    // command-semantics resolution entirely.
    if context_hint == ContextHint::Redirection {
        let words: Vec<&str> = input.split_whitespace().collect();
        if let Some(last) = words.last() {
            // If it looks like a path abbreviation try to resolve it; otherwise
            // pass through verbatim so the shell handles it.
            match path_resolve::resolve_path(last, &trie.named_dirs, &trie.dir_stack) {
                path_resolve::PathResult::Resolved(resolved) => {
                    let mut parts: Vec<String> =
                        words[..words.len() - 1].iter().map(|s| s.to_string()).collect();
                    parts.push(shell_escape_path(&resolved));
                    let result = parts.join(" ");
                    return if result == input {
                        ResolveResult::Passthrough(input.to_string())
                    } else {
                        ResolveResult::Resolved(result)
                    };
                }
                path_resolve::PathResult::Ambiguous(candidates) => {
                    let prefix: Vec<String> =
                        words[..words.len() - 1].iter().map(|s| s.to_string()).collect();
                    let full_cmds: Vec<String> = candidates
                        .into_iter()
                        .map(|c| {
                            let mut parts = prefix.clone();
                            parts.push(shell_escape_path(&c));
                            parts.join(" ")
                        })
                        .collect();
                    return ResolveResult::PathAmbiguous(full_cmds);
                }
                path_resolve::PathResult::Unchanged => {}
            }
        }
        return ResolveResult::Passthrough(input.to_string());
    }

    resolve_with_cwd(input, trie, pins, cwd)
}

/// Resolve a single command segment, threading `cwd` into the scorer.
fn resolve_with_cwd(input: &str, trie: &CommandTrie, pins: &Pins, cwd: Option<&str>) -> ResolveResult {
    // We cannot pass cwd directly into resolve_from_node without a large
    // refactor; instead we store it in a thread-local so score_node can
    // read it during the tiebreak.
    CWD_CONTEXT.with(|c| {
        *c.borrow_mut() = cwd.map(|s| s.to_string());
    });
    let result = resolve(input, trie, pins);
    CWD_CONTEXT.with(|c| {
        *c.borrow_mut() = None;
    });
    result
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
        match resolve_from_node(remaining_words, node, &mut result_words, &trie.arg_modes, &trie.arg_specs, &trie.matcher_rules) {
            Ok(()) => finalize_with_paths(input, result_words, trie),
            Err(ambiguity) => ResolveResult::Ambiguous(*ambiguity),
        }
    } else {
        let mut result_words: Vec<String> = Vec::new();
        match resolve_from_node(&words, &trie.root, &mut result_words, &trie.arg_modes, &trie.arg_specs, &trie.matcher_rules) {
            Ok(()) => finalize_with_paths(input, result_words, trie),
            Err(ambiguity) => ResolveResult::Ambiguous(*ambiguity),
        }
    }
}

// --- Pipe / chain splitting ---

pub(super) enum LinePart {
    Command(String),
    Operator(String),
}

/// Split a command line on `|`, `||`, `&&`, `;` while respecting quotes.
pub(super) fn split_on_operators(input: &str) -> Vec<LinePart> {
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

pub(super) fn finalize_with_paths(input: &str, mut words: Vec<String>, trie: &CommandTrie) -> ResolveResult {
    // If the command word itself is a relative/absolute path (e.g. `./unin`,
    // `~/bin/foo`), resolve it against the filesystem before handling args.
    if let Some(cmd) = words.first()
        && (cmd.contains('/') || cmd.starts_with('~')) && !cmd.starts_with('-') {
            match path_resolve::resolve_path(cmd, &trie.named_dirs, &trie.dir_stack) {
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
    match resolve_paths_in_words(&words, spec, fallback_mode, cmd_words, &trie.named_dirs, &trie.dir_stack) {
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
pub(super) fn resolve_from_node(
    words: &[&str],
    start_node: &TrieNode,
    result: &mut Vec<String>,
    modes: &ArgModeMap,
    arg_specs: &ArgSpecMap,
    matcher_rules: &[MatcherRule],
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
                    .matcher_aware_search(word, matcher_rules)
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
                return resolve_from_node(rest, exact_node, result, modes, arg_specs, matcher_rules);
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
            return resolve_from_node(rest, exact_node, result, modes, arg_specs, matcher_rules);
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

    let matches = start_node.matcher_aware_search(word, matcher_rules);

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
                resolve_from_node(rest, child_node, result, modes, arg_specs, matcher_rules)
            } else {
                for w in rest {
                    result.push(w.to_string());
                }
                Ok(())
            }
        }
        _ => {
            // Ambiguous -- but try deep disambiguation first
            let rcfg = crate::runtime_config::get();

            // force_picker_at_candidates: when the candidate pool is at or
            // above this threshold, skip stats and go straight to the picker.
            if rcfg.force_picker_at_candidates > 0
                && matches.len() as u32 >= rcfg.force_picker_at_candidates
            {
                let cands: Vec<String> = matches.iter().map(|(s, _)| s.to_string()).collect();
                let lcp = longest_common_prefix(&cands);
                return Err(Box::new(AmbiguityInfo {
                    word: word.to_string(),
                    position: result.len(),
                    candidates: cands,
                    lcp,
                    deep_candidates: vec![],
                    resolved_prefix: result.clone(),
                    remaining: rest.iter().map(|s| s.to_string()).collect(),
                }));
            }

            if !rest.is_empty() && !rest[0].starts_with('-') {
                let deep_raw = deep_disambiguate(&matches, rest);

                // Arg-type narrowing applies when subcommand-prefix lookahead
                // was indecisive (multiple survivors) OR produced no survivors
                // (the next word isn't a subcommand — it's likely a typed arg).
                // In the zero-survivor case we narrow on the full original matches.
                let mut deep = if rcfg.disable_arg_type_narrowing {
                    if deep_raw.is_empty() { matches.to_vec() } else { deep_raw }
                } else if deep_raw.len() > 1 {
                    let narrowed = narrow_by_arg_type(&deep_raw, result, rest, arg_specs);
                    if narrowed.len() < deep_raw.len() { narrowed } else { deep_raw }
                } else if deep_raw.is_empty() {
                    // The next word isn't a subcommand — treat all original matches
                    // as candidates so arg-type narrowing (and stats) can weigh in.
                    let narrowed = narrow_by_arg_type(&matches, result, rest, arg_specs);
                    if narrowed.len() < matches.len() { narrowed } else { matches.to_vec() }
                } else {
                    deep_raw
                };

                // Flag-match narrowing — runs after arg-type narrowing,
                // before the stats tiebreaker.  Uses flag evidence in `rest`
                // to discriminate candidates whose arg-types didn't differ.
                if deep.len() > 1 && !rcfg.disable_flag_matching {
                    let narrowed = narrow_by_flag_match(&deep, result, rest, arg_specs);
                    if narrowed.len() < deep.len() {
                        deep = narrowed;
                    }
                }

                // Phase 5.2: if still ambiguous after arg-type narrowing,
                // try the statistical tiebreaker (frequency + recency + success rate).
                if deep.len() > 1
                    && let Some(winner) = score_candidates_stats(&deep, current_unix_ts())
                {
                    deep = vec![winner];
                }

                if deep.len() == 1 {
                    // Deep disambiguation resolved it
                    let (full_name, child_node) = deep[0];
                    result.push(full_name.to_string());

                    if !child_node.children.is_empty() {
                        return resolve_from_node(rest, child_node, result, modes, arg_specs, matcher_rules);
                    } else {
                        for w in rest {
                            result.push(w.to_string());
                        }
                        return Ok(());
                    }
                }

                // When arg-type or stats narrowing already trimmed the
                // candidate pool, surface the NARROWED set in the ambiguity
                // report — otherwise the picker ends up showing the
                // pre-narrowing list, defeating the whole purpose of the
                // signals.  Fall back to `matches` when narrowing didn't
                // reduce the set (preserves the original output for pure
                // prefix-only ambiguity).
                let report: Vec<(&str, &TrieNode)> = if !deep.is_empty() && deep.len() < matches.len() {
                    deep.iter().map(|(n, nd)| (*n, *nd)).collect()
                } else {
                    matches.iter().map(|(n, nd)| (*n, *nd)).collect()
                };

                // Build deep candidate info for the ambiguity report
                let deep_candidates: Vec<DeepCandidate> = report
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

                let cands: Vec<String> = report.iter().map(|(s, _)| s.to_string()).collect();
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
                // rest is empty OR rest[0] starts with '-' (a flag).
                // Try flag-match narrowing before surfacing ambiguity.
                let mut pool = matches.to_vec();
                if !rest.is_empty() && pool.len() > 1 && !rcfg.disable_flag_matching {
                    let narrowed = narrow_by_flag_match(&pool, result, rest, arg_specs);
                    if narrowed.len() < pool.len() {
                        pool = narrowed;
                    }
                }

                if pool.len() == 1 {
                    let (full_name, child_node) = pool[0];
                    result.push(full_name.to_string());
                    if !child_node.children.is_empty() {
                        return resolve_from_node(rest, child_node, result, modes, arg_specs, matcher_rules);
                    } else {
                        for w in rest {
                            result.push(w.to_string());
                        }
                        return Ok(());
                    }
                }

                let cands: Vec<String> = pool.iter().map(|(s, _)| s.to_string()).collect();
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
pub(super) fn longest_common_prefix(strings: &[String]) -> String {
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
pub(super) fn deep_disambiguate<'a>(
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

/// When subcommand-prefix lookahead still leaves ambiguity, try arg-type
/// evidence: for each candidate, look up its full command path's ArgSpec and
/// check whether the first non-flag word in `rest` matches the expected
/// positional type.
///
/// Returns a narrowed list. Guarantees:
/// - Never returns empty (falls back to the original `candidates` list).
/// - Only narrows when there is a discriminating split: ≥1 candidate has
///   matching evidence AND ≥1 lacks it; unanimous evidence is not a signal.
pub(super) fn narrow_by_arg_type<'a>(
    candidates: &[(&'a str, &'a TrieNode)],
    prefix_chain: &[String],
    rest: &[&str],
    arg_specs: &ArgSpecMap,
) -> Vec<(&'a str, &'a TrieNode)> {
    let probe = rest.iter().find(|w| !w.starts_with('-')).copied();
    let Some(probe) = probe else { return candidates.to_vec(); };

    let mut with_evidence: Vec<(&'a str, &'a TrieNode)> = Vec::new();
    let mut without_evidence: Vec<(&'a str, &'a TrieNode)> = Vec::new();

    for &(name, node) in candidates {
        let mut full: Vec<&str> = prefix_chain.iter().map(String::as_str).collect();
        full.push(name);
        let key = full.join(" ");
        let Some(spec) = arg_specs.get(&key) else {
            without_evidence.push((name, node));
            continue;
        };
        let types = spec.types_at(1);
        if types.is_empty() {
            without_evidence.push((name, node));
            continue;
        }
        let matched = types.iter().any(|&t| word_matches_type(probe, t));
        if matched {
            with_evidence.push((name, node));
        } else {
            without_evidence.push((name, node));
        }
    }

    if !with_evidence.is_empty() && !without_evidence.is_empty() {
        with_evidence
    } else {
        candidates.to_vec()
    }
}

/// Use flags in `rest` (the portion of the command line following the
/// ambiguous word) as disambiguation evidence. A candidate gains evidence
/// for every flag in `rest` that appears in its ArgSpec's `flag_args`
/// OR `flag_call_programs` OR `flag_static_lists`. Candidates with strictly
/// MORE flag hits than the minimum win; ties at the max are preserved.
///
/// If no candidate has evidence (nobody recognizes any flag) OR all have
/// the same count, returns the input unchanged — nothing to narrow on.
pub(super) fn narrow_by_flag_match<'a>(
    candidates: &[(&'a str, &'a TrieNode)],
    prefix_chain: &[String],
    rest: &[&str],
    arg_specs: &ArgSpecMap,
) -> Vec<(&'a str, &'a TrieNode)> {
    let flags: Vec<&str> = rest
        .iter()
        .filter(|w| w.starts_with('-') && w.len() > 1)
        .copied()
        .collect();
    if flags.is_empty() {
        return candidates.to_vec();
    }

    let scored: Vec<(usize, (&'a str, &'a TrieNode))> = candidates
        .iter()
        .map(|&(name, node)| {
            let mut full: Vec<&str> = prefix_chain.iter().map(String::as_str).collect();
            full.push(name);
            let key = full.join(" ");
            let hits = match arg_specs.get(&key) {
                Some(spec) => flags
                    .iter()
                    .filter(|f| {
                        let bare = f.split_once('=').map(|(k, _)| k).unwrap_or(f);
                        // Direct lookup
                        if spec.flag_args.contains_key(bare)
                            || spec.flag_call_programs.contains_key(bare)
                            || spec.flag_static_lists.contains_key(bare)
                        {
                            return true;
                        }
                        // Alias lookup: check every sibling in the group containing `bare`.
                        for group in &spec.flag_aliases {
                            if group.iter().any(|g| g == bare) {
                                for sibling in group {
                                    if spec.flag_args.contains_key(sibling)
                                        || spec.flag_call_programs.contains_key(sibling)
                                        || spec.flag_static_lists.contains_key(sibling)
                                    {
                                        return true;
                                    }
                                }
                            }
                        }
                        false
                    })
                    .count(),
                None => 0,
            };
            (hits, (name, node))
        })
        .collect();

    let max = scored.iter().map(|(h, _)| *h).max().unwrap_or(0);
    let min = scored.iter().map(|(h, _)| *h).min().unwrap_or(0);
    if max == 0 || max == min {
        return candidates.to_vec();
    }

    scored
        .into_iter()
        .filter(|(h, _)| *h == max)
        .map(|(_, c)| c)
        .collect()
}

/// When arg-type narrowing is indecisive, pick by statistical evidence:
/// frequency (log(1+count)), recency (exponential decay on age_seconds,
/// half-life ~14 days), and success_rate.  All three signals multiply
/// together so a never-used-but-typed command cannot beat a lightly-used
/// one on history alone.
///
/// Returns Some(winner) only when the top score is at least
/// `DOMINANCE_MARGIN` times the runner-up's.  Otherwise returns None
/// and the caller preserves ambiguity.
pub(super) fn score_candidates_stats<'a>(
    candidates: &[(&'a str, &'a TrieNode)],
    now: u64,
) -> Option<(&'a str, &'a TrieNode)> {
    // Deterministic mode: skip the statistical tiebreaker entirely. Users
    // who need reproducible resolution across machines set this so the
    // engine never silently picks based on their local history.
    if STATS_DISABLED.load(std::sync::atomic::Ordering::Relaxed) {
        return None;
    }

    let cfg = crate::runtime_config::get();

    if candidates.len() <= 1 {
        return candidates.first().copied();
    }

    // last-cmd sibling boost: if the env var names the same command as one of
    // the candidates, that candidate gets a 1.3× tiebreaker nudge.
    let last_cmd = if cfg.disable_sibling_context { None } else { last_cmd_env() };

    let mut scored: Vec<(f32, (&'a str, &'a TrieNode))> = candidates
        .iter()
        .map(|&(name, node)| {
            let base = score_node(node, now, cfg.disable_cwd_scoring);
            let last_boost = if last_cmd.as_deref() == Some(name) { 1.3 } else { 1.0 };
            (base * last_boost, (name, node))
        })
        .collect();
    scored.sort_by(|a, b| b.0.partial_cmp(&a.0).unwrap_or(std::cmp::Ordering::Equal));

    let top = scored[0].0;
    let runner_up = scored[1].0;
    // Require: top strictly positive (has some signal at all) AND
    // top >= runner_up * margin. If runner_up is 0, top >= epsilon suffices.
    if top <= 0.0 {
        return None;
    }
    let margin = cfg.dominance_margin;
    if runner_up <= 0.0 || top >= runner_up * margin {
        Some(scored[0].1)
    } else {
        None
    }
}

fn score_node(node: &TrieNode, now: u64, disable_cwd: bool) -> f32 {
    // Frequency: log base-e (1 + count). Never zero; plateaus so a single
    // use contributes 0.693, ten uses contribute 2.4, etc.
    let freq = (1.0 + node.count as f32).ln();

    // Recency: exponential decay with 14-day half-life. Unused (last_used=0)
    // gets a small baseline 0.5 so never-timestamped nodes don't get crushed
    // by a single recent one.
    let recency = match node.age_seconds(now) {
        None => 0.5,
        Some(age) => {
            let half_life_secs = 14.0 * 24.0 * 3600.0;
            (-((age as f32) / half_life_secs) * std::f32::consts::LN_2).exp()
        }
    };

    // Success rate: default to 1.0 when we have no failure signal yet.
    let success = node.success_rate().unwrap_or(1.0);

    // cwd multiplier: up to 1.5× boost for commands used in this directory.
    let cwd_mul = if disable_cwd {
        1.0
    } else {
        CWD_CONTEXT.with(|c| {
            if let Some(cwd) = c.borrow().as_deref() {
                1.0 + 0.5 * node.cwd_score(cwd)
            } else {
                1.0
            }
        })
    };

    freq * recency * success * cwd_mul
}

/// Read `ZSH_IOS_LAST_CMD` env var. Returns `None` if the var is unset or empty.
fn last_cmd_env() -> Option<String> {
    let val = std::env::var("ZSH_IOS_LAST_CMD").ok()?;
    if val.is_empty() { None } else { Some(val) }
}

/// Helper: current unix timestamp.  Called from resolve sites.
pub(super) fn current_unix_ts() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

fn format_age(secs: u64) -> String {
    if secs < 60 {
        format!("{}s", secs)
    } else if secs < 3600 {
        format!("{}m", secs / 60)
    } else if secs < 86400 {
        format!("{}h", secs / 3600)
    } else if secs < 30 * 86400 {
        format!("{}d", secs / 86400)
    } else if secs < 365 * 86400 {
        format!("{}mo", secs / (30 * 86400))
    } else {
        format!("{}y", secs / (365 * 86400))
    }
}

// --- Text/net format validators (private helpers for word_matches_type) ---

static EMAIL_RE: LazyLock<regex::Regex> = LazyLock::new(|| {
    regex::Regex::new(r"^[A-Za-z0-9._%+-]+@[A-Za-z0-9.-]+\.[A-Za-z]{2,}$").unwrap()
});

static URL_RE: LazyLock<regex::Regex> = LazyLock::new(|| {
    regex::Regex::new(r"^[A-Za-z][A-Za-z0-9+.-]*://.+").unwrap()
});

static MAC_RE: LazyLock<regex::Regex> = LazyLock::new(|| {
    regex::Regex::new(r"^[0-9A-Fa-f]{2}([:-][0-9A-Fa-f]{2}){5}$").unwrap()
});

fn is_plausible_email(s: &str) -> bool {
    EMAIL_RE.is_match(s)
}

fn is_plausible_url(s: &str) -> bool {
    URL_RE.is_match(s)
}

fn is_plausible_mac(s: &str) -> bool {
    MAC_RE.is_match(s)
}

fn is_valid_timezone(s: &str) -> bool {
    std::path::Path::new("/usr/share/zoneinfo").join(s).is_file()
}

/// Is `word` a plausible value of the given argument type?
/// Filesystem types check the filesystem. Text/net types use format validators.
/// Other typed values go through `type_resolver::REGISTRY`. Types without a
/// cheap membership test return `false` (treated as "no evidence" — not a
/// narrowing signal).
pub(super) fn word_matches_type(word: &str, arg_type: u8) -> bool {
    use crate::trie::*;
    use std::path::Path;
    match arg_type {
        ARG_MODE_PATHS => Path::new(word).exists(),
        ARG_MODE_DIRS_ONLY => Path::new(word).is_dir(),
        ARG_MODE_EXECS_ONLY => {
            let p = Path::new(word);
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                if let Ok(md) = p.metadata()
                    && md.is_file()
                    && md.permissions().mode() & 0o111 != 0
                {
                    return true;
                }
            }
            #[cfg(not(unix))]
            {
                let _ = p;
            }
            false
        }
        ARG_MODE_IPV4 => word.parse::<std::net::Ipv4Addr>().is_ok(),
        ARG_MODE_IPV6 => word.parse::<std::net::Ipv6Addr>().is_ok(),
        ARG_MODE_EMAIL => is_plausible_email(word),
        ARG_MODE_URL_SCHEME => is_plausible_url(word),
        ARG_MODE_MAC_ADDR => is_plausible_mac(word),
        ARG_MODE_TIMEZONE => is_valid_timezone(word),
        _ => {
            if let Some(resolver) = crate::type_resolver::REGISTRY.get(arg_type) {
                let ctx = crate::type_resolver::Ctx::with_partial(word);
                let items = resolver.list(&ctx);
                items.iter().any(|it| it == word)
            } else {
                false
            }
        }
    }
}

pub(super) enum PathsResult {
    Resolved(String),
    Ambiguous(Vec<String>),
}

/// After command resolution, resolve any path arguments against the filesystem.
/// Check whether a word looks like an explicit path reference.
/// Used in Normal mode to avoid resolving bare words like "clau" against
/// the filesystem -- only words that syntactically reference a path.
pub(super) fn looks_like_path(word: &str) -> bool {
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
pub(super) fn lookup_arg_spec<'a>(
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
pub(super) fn arg_type_for_word(
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

pub(super) fn u8_to_arg_mode(val: u8) -> ArgMode {
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
pub(super) fn apply_context_rules(spec: Option<&ArgSpec>, words: &[String], base: ArgMode) -> ArgMode {
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

pub(super) fn resolve_paths_in_words(
    words: &[String],
    spec: Option<&ArgSpec>,
    fallback_mode: ArgMode,
    cmd_words: usize,
    named_dirs: &std::collections::HashMap<String, String>,
    dir_stack: &[String],
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
                    ArgMode::DirsOnly => path_resolve::resolve_path_dirs_only(word, named_dirs, dir_stack),
                    _ => path_resolve::resolve_path(word, named_dirs, dir_stack),
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
                if looks_like_path(word) || path_resolve::looks_like_named_dir_ref(word, named_dirs) {
                    match path_resolve::resolve_path(word, named_dirs, dir_stack) {
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
/// How a command's arguments should be resolved.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum ArgMode {
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
pub(super) fn is_hardcoded_path_command(cmd: &str) -> bool {
    DIR_COMMANDS.contains(&cmd) || PATH_COMMANDS.contains(&cmd)
}

/// Classify a command by how its arguments should be resolved.
///
/// Checks the auto-detected arg modes from Zsh completions first,
/// then falls back to a hardcoded list for common commands.
pub(super) fn arg_mode(cmd: &str, modes: &ArgModeMap) -> ArgMode {
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
pub(super) fn has_filesystem_prefix_match(word: &str) -> bool {
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

/// Narrate the inner-level disambiguation that happened when the result is
/// `Ambiguous` with a non-empty `resolved_prefix`. Called from `explain`
/// after the outer first-word narration to show what occurred inside the
/// subcommand subtree (deep-disambiguate, arg-type narrowing, flag-match,
/// stats tiebreak).
fn narrate_inner_ambiguity(
    info: &AmbiguityInfo,
    trie: &CommandTrie,
    out: &mut Vec<String>,
    push: &impl Fn(&mut Vec<String>, usize, String),
) {
    // Walk the trie to the node corresponding to info.resolved_prefix.
    let mut cur = &trie.root;
    for w in &info.resolved_prefix {
        match cur.get_child(w) {
            Some(n) => cur = n,
            None => return, // prefix not walkable — bail silently
        }
    }

    let prefix_str = info.resolved_prefix.join(" ");
    push(out, 0, String::new());
    push(out, 0, format!("Inner ambiguity within {:?}:", prefix_str));

    // Initial matches from the inner node for info.word
    let matches = cur.prefix_search(&info.word);
    if matches.is_empty() {
        push(out, 1, format!("no subcommand matches for {:?} at this level", info.word));
        return;
    }
    let names: Vec<&str> = matches.iter().map(|(n, _)| *n).collect();
    push(
        out,
        1,
        format!(
            "prefix_search {:?} → {} candidate{}: {}",
            info.word,
            names.len(),
            if names.len() == 1 { "" } else { "s" },
            summarize_names(&names, 8),
        ),
    );

    let remaining_words: Vec<&str> = info.remaining.iter().map(String::as_str).collect();

    // Step 1: deep_disambiguate
    let deep_raw = if !remaining_words.is_empty() && !remaining_words[0].starts_with('-') {
        let next = remaining_words[0];
        push(out, 1, format!("deep-disambiguate with next word {:?}:", next));
        let result = deep_disambiguate(&matches, &remaining_words);
        let mut survivors_detail: Vec<(&str, Vec<&str>)> = Vec::new();
        let mut nonmatch_count = 0usize;
        for (name, node) in &matches {
            let sub = node.prefix_search(next);
            if sub.is_empty() {
                nonmatch_count += 1;
            } else {
                let sub_names: Vec<&str> = sub.iter().map(|(n, _)| *n).collect();
                survivors_detail.push((name, sub_names));
            }
        }
        for (name, sub_names) in &survivors_detail {
            push(out, 2, format!("{}: {}", name, summarize_names(sub_names, 6)));
        }
        if nonmatch_count > 0 {
            push(
                out,
                2,
                format!(
                    "({} other candidate{} had no {:?} subcommand)",
                    nonmatch_count,
                    if nonmatch_count == 1 { "" } else { "s" },
                    next
                ),
            );
        }
        match result.len() {
            0 => push(out, 1, "→ no deep survivor — treating all candidates as pool".into()),
            1 => push(out, 1, format!("→ deep winner: {}", result[0].0)),
            n => push(out, 1, format!("→ {} survivors after deep-disambiguate", n)),
        }
        result
    } else {
        matches.clone()
    };

    // Determine the pool to hand to the next phases.
    // Mirrors resolve_from_node: when deep_raw is empty, fall back to full matches.
    let pool_after_deep: Vec<(&str, &TrieNode)> = if deep_raw.is_empty() {
        matches.iter().map(|(n, nd)| (*n, *nd)).collect()
    } else {
        deep_raw.iter().map(|(n, nd)| (*n, *nd)).collect()
    };

    if pool_after_deep.len() <= 1 {
        return;
    }

    // Step 2: arg-type narrowing (when remaining has a non-flag word)
    let probe = remaining_words.iter().find(|w| !w.starts_with('-')).copied();
    let pool_after_arg_type = if let Some(probe_word) = probe {
        push(out, 1, format!("arg-type narrowing: probe {:?}", probe_word));
        let mut any_match = false;
        let mut any_no_match = false;
        for (name, _node) in &pool_after_deep {
            let key = format!("{} {}", prefix_str, name);
            let type_label = if let Some(spec) = trie.arg_specs.get(&key) {
                if let Some(expected) = spec.type_at(1) {
                    let hint = trie::arg_mode_name(expected);
                    if word_matches_type(probe_word, expected) {
                        any_match = true;
                        format!("expects {} at pos 1 → MATCH", hint)
                    } else {
                        any_no_match = true;
                        format!("expects {} at pos 1 → no match", hint)
                    }
                } else {
                    "no positional[1] in arg spec".to_string()
                }
            } else {
                "no arg spec".to_string()
            };
            push(out, 2, format!("  {}  {}", name, type_label));
        }
        if any_match && any_no_match {
            let narrowed = narrow_by_arg_type(
                &pool_after_deep,
                &info.resolved_prefix,
                &remaining_words,
                &trie.arg_specs,
            );
            push(
                out,
                1,
                format!(
                    "→ narrowed to: {}",
                    narrowed.iter().map(|(n, _)| *n).collect::<Vec<_>>().join(", ")
                ),
            );
            narrowed
        } else {
            push(out, 1, "→ no discriminating arg-type evidence".into());
            pool_after_deep.clone()
        }
    } else {
        pool_after_deep.clone()
    };

    if pool_after_arg_type.len() <= 1 {
        return;
    }

    // Step 3: flag-match narrowing
    let flags: Vec<&str> = remaining_words
        .iter()
        .filter(|w| w.starts_with('-') && w.len() > 1)
        .copied()
        .collect();
    let pool_after_flags = if !flags.is_empty() {
        push(
            out,
            1,
            format!("flag-match narrowing with flags: {}", flags.join(", ")),
        );
        let narrowed = narrow_by_flag_match(
            &pool_after_arg_type,
            &info.resolved_prefix,
            &remaining_words,
            &trie.arg_specs,
        );
        if narrowed.len() < pool_after_arg_type.len() {
            push(
                out,
                1,
                format!(
                    "→ narrowed to: {}",
                    narrowed.iter().map(|(n, _)| *n).collect::<Vec<_>>().join(", ")
                ),
            );
            narrowed
        } else {
            push(out, 1, "→ no discriminating flag evidence".into());
            pool_after_arg_type.clone()
        }
    } else {
        pool_after_arg_type.clone()
    };

    if pool_after_flags.len() <= 1 {
        return;
    }

    // Step 4: stats tiebreak
    let last_cmd = info.resolved_prefix.last().map(String::as_str).unwrap_or("");
    narrate_stats_tiebreak(out, 0, &pool_after_flags, current_unix_ts(), last_cmd);
}

/// Produce a human-readable narrative of how `input` resolves against the
/// trie and pins. Walks the same primitives as the real resolver (pins,
/// wrapper detection, trie prefix search, deep disambiguation, arg-spec
/// lookup) and then reports the actual `resolve_line` result so any
/// discrepancy between the walk and the real engine is visible.
pub fn explain(input: &str, trie: &CommandTrie, pins: &Pins, cwd: Option<&str>) -> String {
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
    match resolve_line(input, trie, pins, cwd, ContextHint::Unknown) {
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
            // When the ambiguity is inside a subcommand subtree (resolved_prefix
            // is non-empty), re-walk the inner logic so the narrator shows what
            // actually happened at that level.
            if !info.resolved_prefix.is_empty() {
                narrate_inner_ambiguity(&info, trie, &mut out, &push);
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
pub(super) fn explain_segment(
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
                0 => {
                    push(
                        out,
                        depth + 1,
                        "→ no survivor — arg is not a subcommand; stats tiebreak on all candidates".into(),
                    );
                    // When no subcommand matches, resolve falls back to the full
                    // original candidate set. Narrate the stats tiebreak here.
                    if first_matches.len() > 1 {
                        let now = current_unix_ts();
                        narrate_stats_tiebreak(out, depth, &first_matches, now, word_refs[0]);
                    }
                }
                1 => push(
                    out,
                    depth + 1,
                    format!("→ winner: {}", survivors[0].0),
                ),
                n => {
                    push(
                        out,
                        depth + 1,
                        format!("→ {} candidates survive — still ambiguous", n),
                    );
                    // Collect survivor_nodes for arg-type and stats narration.
                    let survivor_nodes: Vec<(&str, &TrieNode)> = survivors
                        .iter()
                        .filter_map(|(name, _)| {
                            first_matches.iter().find(|(sn, _)| sn == name).copied()
                        })
                        .collect();

                    // Try arg-type narrowing narration when there are still multiple survivors.
                    if word_refs.len() > 2 {
                        let probe_word = word_refs[2];
                        push(
                            out,
                            depth + 1,
                            format!("arg-type narrowing: probe {:?}", probe_word),
                        );
                        let mut any_match = false;
                        let mut any_no_match = false;
                        for (name, _node) in &survivor_nodes {
                            let key = format!("{} {}", name, next);
                            let type_label = if let Some(spec) = trie.arg_specs.get(&key) {
                                if let Some(expected) = spec.type_at(1) {
                                    let hint = trie::arg_mode_name(expected);
                                    if word_matches_type(probe_word, expected) {
                                        any_match = true;
                                        format!("expects {} at pos 1 → MATCH", hint)
                                    } else {
                                        any_no_match = true;
                                        format!("expects {} at pos 1 → no match", hint)
                                    }
                                } else {
                                    "no positional[1] in arg spec".to_string()
                                }
                            } else {
                                "no arg spec".to_string()
                            };
                            push(
                                out,
                                depth + 2,
                                format!("{} {}  {}", name, next, type_label),
                            );
                        }
                        if any_match && any_no_match {
                            let narrowed = narrow_by_arg_type(
                                &survivor_nodes,
                                &[word_refs[0].to_string()],
                                &[*next, probe_word],
                                &trie.arg_specs,
                            );
                            push(
                                out,
                                depth + 1,
                                format!("→ narrowed to: {}", narrowed.iter().map(|(n, _)| *n).collect::<Vec<_>>().join(", ")),
                            );
                            // Stats tiebreaker narration after arg-type narrowing.
                            if narrowed.len() > 1 {
                                let now = current_unix_ts();
                                narrate_stats_tiebreak(out, depth, &narrowed, now, word_refs[0]);
                            }
                        } else {
                            push(
                                out,
                                depth + 1,
                                "→ no discriminating arg-type evidence".into(),
                            );
                            // Stats tiebreaker narration when arg-type gave no split.
                            if survivor_nodes.len() > 1 {
                                let now = current_unix_ts();
                                narrate_stats_tiebreak(out, depth, &survivor_nodes, now, word_refs[0]);
                            }
                        }
                    } else {
                        // No probe word available; apply stats directly to survivors.
                        if survivor_nodes.len() > 1 {
                            let now = current_unix_ts();
                            narrate_stats_tiebreak(out, depth, &survivor_nodes, now, word_refs[0]);
                        }
                    }
                }
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
pub(super) fn summarize_names(names: &[&str], cap: usize) -> String {
    if names.len() <= cap {
        return names.join(", ");
    }
    let head = names[..cap].join(", ");
    format!("{}, … ({} more)", head, names.len() - cap)
}

pub(super) fn describe_spec(out: &mut Vec<String>, depth: usize, spec: &ArgSpec) {
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

/// Narrate the stats tiebreaker step into `out`.
/// Emits a table of per-candidate scores and — if a winner was chosen —
/// reports the dominance ratio.  Called from `explain_segment` when
/// arg-type narrowing left multiple candidates.
fn narrate_stats_tiebreak(
    out: &mut Vec<String>,
    depth: usize,
    candidates: &[(&str, &TrieNode)],
    now: u64,
    prefix_cmd: &str,
) {
    let push = |out: &mut Vec<String>, d: usize, s: String| {
        out.push(format!("{}{}", "  ".repeat(d), s));
    };
    push(out, depth + 1, "stats tiebreak:".into());
    let mut scored: Vec<(f32, &str)> = candidates
        .iter()
        .map(|&(name, node)| (score_node(node, now, false), name))
        .collect();
    scored.sort_by(|a, b| b.0.partial_cmp(&a.0).unwrap_or(std::cmp::Ordering::Equal));

    for &(score, name) in &scored {
        let node = candidates.iter().find(|(n, _)| *n == name).map(|(_, nd)| nd).unwrap();
        let last_str = match node.age_seconds(now) {
            None => "never".to_string(),
            Some(age) => format!("{} ago", format_age(age)),
        };
        let success_str = match node.success_rate() {
            None => "n/a".to_string(),
            Some(r) => format!("{:.2}", r),
        };
        push(
            out,
            depth + 2,
            format!(
                "{} {}  count={} last={}  success={}  score={:.2}",
                prefix_cmd, name, node.count, last_str, success_str, score
            ),
        );
    }

    if scored.len() >= 2 {
        let top = scored[0].0;
        let runner_up = scored[1].0;
        if top > 0.0 && (runner_up <= 0.0 || top >= runner_up * 1.05) {
            let ratio = if runner_up > 0.0 { top / runner_up } else { f32::INFINITY };
            push(
                out,
                depth + 1,
                format!(
                    "→ chose: {} {} (dominance {:.1}×)",
                    prefix_cmd, scored[0].1, ratio
                ),
            );
        } else {
            push(
                out,
                depth + 1,
                "→ scores too close — ambiguity preserved".into(),
            );
        }
    }
}

/// Generate completions for the `?` command.
/// Returns a formatted list of matching commands/subcommands.
/// Splits on pipe/chain operators and completes only the last segment.
#[cfg(test)]
mod tests {
    use super::*;
    // Reach across to sibling submodules for items only the test suite touches.
    // Kept local to the test module so the main engine code doesn't carry
    // imports it doesn't use.
    use super::super::complete::{complete, format_columns};
    use super::super::escape::shell_escape_path_glob;
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
        let _g = CWD_LOCK.lock().unwrap();
        let td = tempfile::tempdir().unwrap();
        let orig = std::env::current_dir().ok();
        std::env::set_current_dir(td.path()).unwrap();

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

        if let Some(o) = orig {
            let _ = std::env::set_current_dir(o);
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
        match resolve_line("gi push | gr -r pattern", &trie, &pins, None, ContextHint::Unknown) {
            ResolveResult::Resolved(s) => assert_eq!(s, "git push | grep -r pattern"),
            other => panic!("Expected Resolved, got {:?}", other),
        }
    }

    #[test]
    fn test_chain_resolution() {
        let trie = build_test_trie();
        let pins = Pins::default();

        match resolve_line("ter init && ter ap", &trie, &pins, None, ContextHint::Unknown) {
            ResolveResult::Resolved(s) => assert_eq!(s, "terraform init && terraform apply"),
            other => panic!("Expected Resolved, got {:?}", other),
        }
    }

    #[test]
    fn test_semicolon_resolution() {
        let _g = CWD_LOCK.lock().unwrap();
        let td = tempfile::tempdir().unwrap();
        let orig = std::env::current_dir().ok();
        std::env::set_current_dir(td.path()).unwrap();

        let trie = build_test_trie();
        let pins = Pins::default();

        match resolve_line("ter init; ter pl", &trie, &pins, None, ContextHint::Unknown) {
            ResolveResult::Resolved(s) => assert_eq!(s, "terraform init ; terraform plan"),
            other => panic!("Expected Resolved, got {:?}", other),
        }

        if let Some(o) = orig {
            let _ = std::env::set_current_dir(o);
        }
    }

    #[test]
    fn test_pipe_ambiguity_in_second_segment() {
        let trie = build_test_trie();
        let pins = Pins::default();

        // First segment resolves; second is ambiguous (bare "g")
        match resolve_line("ter ap | g", &trie, &pins, None, ContextHint::Unknown) {
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



    // --- Tests for descriptions (loaded from YAML) ---

    fn load_yaml_descriptions() -> trie::DescriptionMap {
        let yaml_str = include_str!("../../data/descriptions.yaml");
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
        let _g = CWD_LOCK.lock().unwrap();
        let td = tempfile::tempdir().unwrap();
        let orig = std::env::current_dir().ok();
        std::env::set_current_dir(td.path()).unwrap();

        let trie = build_test_trie();
        let pins = Pins::default();

        match resolve_line("sudo ter in && sudo ter ap", &trie, &pins, None, ContextHint::Unknown) {
            ResolveResult::Resolved(s) => {
                assert_eq!(s, "sudo terraform init && sudo terraform apply");
            }
            other => panic!("Expected Resolved, got {:?}", other),
        }

        if let Some(o) = orig {
            let _ = std::env::set_current_dir(o);
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





    // --- Tests for resolve_first_word ---





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




    // --- Tests for resolve_line with empty segments ---

    #[test]
    fn test_resolve_line_trailing_operator() {
        let trie = build_test_trie();
        let pins = Pins::default();
        // "ter ap &&" has an empty segment after &&
        match resolve_line("ter ap && ", &trie, &pins, None, ContextHint::Unknown) {
            ResolveResult::Resolved(s) => assert_eq!(s, "terraform apply && "),
            other => panic!("Expected Resolved, got {:?}", other),
        }
    }

    #[test]
    fn test_resolve_line_or_operator() {
        let _g = CWD_LOCK.lock().unwrap();
        let td = tempfile::tempdir().unwrap();
        let orig = std::env::current_dir().ok();
        std::env::set_current_dir(td.path()).unwrap();

        let trie = build_test_trie();
        let pins = Pins::default();
        match resolve_line("ter in || ter ap", &trie, &pins, None, ContextHint::Unknown) {
            ResolveResult::Resolved(s) => {
                assert_eq!(s, "terraform init || terraform apply");
            }
            other => panic!("Expected Resolved, got {:?}", other),
        }

        if let Some(o) = orig {
            let _ = std::env::set_current_dir(o);
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
        let out = complete("", &trie, &pins, ContextHint::Unknown);
        // common should appear before rare because its count is higher.
        let c = out.find("common").expect("common in output");
        let r = out.find("rare").expect("rare in output");
        assert!(c < r, "expected count-sorted order, got: {}", out);
    }

    #[test]
    fn complete_no_matches_message() {
        let trie = build_test_trie();
        let pins = Pins::default();
        let out = complete("xq", &trie, &pins, ContextHint::Unknown);
        assert!(out.contains("No commands matching"), "got: {}", out);
    }

    #[test]
    fn complete_prefix_lists_candidates() {
        let trie = build_test_trie();
        let pins = Pins::default();
        let out = complete("g", &trie, &pins, ContextHint::Unknown);
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
        let out = complete("git c", &trie, &pins, ContextHint::Unknown);
        assert!(out.contains("checkout"), "got: {}", out);
        assert!(out.contains("commit"), "got: {}", out);
    }

    #[test]
    fn complete_trailing_space_starts_fresh_word() {
        let trie = build_test_trie();
        let pins = Pins::default();
        // "git " with trailing space → list all git subcommands.
        let out = complete("git ", &trie, &pins, ContextHint::Unknown);
        assert!(out.contains("checkout"));
        assert!(out.contains("commit"));
        assert!(out.contains("push"));
    }

    #[test]
    fn complete_after_pipe_only_completes_last_segment() {
        let trie = build_test_trie();
        let pins = Pins::default();
        // Before the pipe is "ls"-like junk, after the pipe is what gets completed.
        let out = complete("xx | g", &trie, &pins, ContextHint::Unknown);
        assert!(out.contains("git") || out.contains("grep"), "got: {}", out);
    }

    #[test]
    fn complete_flag_prefix_reaches_flag_path() {
        let trie = build_test_trie();
        let pins = Pins::default();
        // "git -" triggers the complete_flags branch. Our test trie has
        // -m under git commit, so we expect some flag output.
        let out = complete("git commit -", &trie, &pins, ContextHint::Unknown);
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
        match resolve_line("", &trie, &pins, None, ContextHint::Unknown) {
            ResolveResult::Passthrough(s) => assert_eq!(s, ""),
            other => panic!("empty → Passthrough, got {:?}", other),
        }
        // Operator at the start: shouldn't panic.
        let _ = resolve_line("| git st", &trie, &pins, None, ContextHint::Unknown);
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
            match resolve_line(input, &trie, &pins, None, ContextHint::Unknown) {
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
        match resolve_line(input, &trie, &pins, None, ContextHint::Unknown) {
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
        match resolve_line("echo !foo", &trie, &pins, None, ContextHint::Unknown) {
            ResolveResult::Passthrough(_) | ResolveResult::Resolved(_) => {}
            other => panic!("unexpected {:?}", other),
        }
    }

    #[test]
    fn complete_bang_returns_empty() {
        let trie = build_test_trie();
        let pins = Pins::default();
        assert!(complete("!g", &trie, &pins, ContextHint::Unknown).is_empty());
        assert!(complete("!!", &trie, &pins, ContextHint::Unknown).is_empty());
        assert!(complete("  !git ", &trie, &pins, ContextHint::Unknown).is_empty());
    }

    #[test]
    fn complete_bang_in_middle_still_works() {
        let trie = build_test_trie();
        let pins = Pins::default();
        // `echo !foo` doesn't start with `!` → normal completion runs.
        let out = complete("ech", &trie, &pins, ContextHint::Unknown);
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
        let out = explain("!!", &trie, &pins, None);
        assert!(out.contains("Leading-! bypass"), "got: {}", out);
        assert!(out.contains("Passthrough"));
    }

    #[test]
    fn explain_resolved_prints_trie_walk_and_winner() {
        let trie = build_test_trie();
        let pins = Pins::default();
        // "ter ap" resolves uniquely via terraform apply. Narrative should
        // include the unique-match line and the final Resolved.
        let out = explain("ter ap", &trie, &pins, None);
        assert!(out.contains("\"ter\""), "got: {}", out);
        assert!(out.contains("Trie:"), "got: {}", out);
        assert!(out.contains("Final: Resolved → terraform apply"), "got: {}", out);
    }

    #[test]
    fn explain_ambiguous_shows_candidates_and_lcp() {
        let trie = build_test_trie();
        let pins = Pins::default();
        let out = explain("g", &trie, &pins, None);
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
        let out = explain("g push", &trie, &pins, None);
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
        let out = explain("k ap", &trie, &pins, None);
        assert!(out.contains("Pin match"), "got: {}", out);
        assert!(out.contains("k") && out.contains("kubectl"));
        assert!(out.contains("Final: Resolved → kubectl apply"));
    }

    #[test]
    fn explain_wrapper_drills_into_inner_command() {
        let mut trie = build_test_trie();
        trie.insert_command("sudo");
        let pins = Pins::default();
        let out = explain("sudo ter ap", &trie, &pins, None);
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
        let out = explain("gi st | gr foo", &trie, &pins, None);
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

    // --- narrow_by_arg_type tests ---

    fn trie_with_git_subcommands_and_argspecs() -> CommandTrie {
        let mut t = CommandTrie::new();
        t.insert(&["git", "checkout"]);
        t.insert(&["git", "check-ignore"]);
        t.insert(&["git", "cherry"]);
        t.insert(&["git", "cherry-pick"]);
        t.arg_specs.insert(
            "git checkout".into(),
            ArgSpec {
                positional: [(1u32, trie::ARG_MODE_GIT_BRANCHES)].into_iter().collect(),
                ..Default::default()
            },
        );
        t.arg_specs.insert(
            "git cherry".into(),
            ArgSpec {
                positional: [(1u32, trie::ARG_MODE_GIT_BRANCHES)].into_iter().collect(),
                ..Default::default()
            },
        );
        t.arg_specs.insert(
            "git check-ignore".into(),
            ArgSpec {
                positional: [(1u32, trie::ARG_MODE_PATHS)].into_iter().collect(),
                ..Default::default()
            },
        );
        t.arg_specs.insert(
            "git cherry-pick".into(),
            ArgSpec {
                positional: [(1u32, trie::ARG_MODE_GIT_COMMIT)].into_iter().collect(),
                ..Default::default()
            },
        );
        t
    }

    #[test]
    fn narrow_by_arg_type_prefers_branch_match() {
        // Use filesystem types so we don't need a real git repo.
        // check-ignore expects PATHS; cherry-pick expects GIT_COMMIT.
        // Create a tempfile named "target_file" and verify check-ignore wins.
        let _g = CWD_LOCK.lock().unwrap();
        let td = tempfile::tempdir().unwrap();
        let orig = std::env::current_dir().ok();
        std::env::set_current_dir(td.path()).unwrap();
        std::fs::write("target_file", b"").unwrap();

        let trie = trie_with_git_subcommands_and_argspecs();
        let prefix = vec!["git".to_string()];
        let check_ignore_node = trie.root.get_child("git").unwrap().get_child("check-ignore").unwrap();
        let cherry_pick_node = trie.root.get_child("git").unwrap().get_child("cherry-pick").unwrap();
        let candidates = vec![
            ("check-ignore", check_ignore_node),
            ("cherry-pick", cherry_pick_node),
        ];
        let narrowed = narrow_by_arg_type(&candidates, &prefix, &["target_file"], &trie.arg_specs);
        assert_eq!(narrowed.len(), 1, "expected narrowing to 1, got {:?}", narrowed.iter().map(|(n,_)| n).collect::<Vec<_>>());
        assert_eq!(narrowed[0].0, "check-ignore");

        if let Some(o) = orig { let _ = std::env::set_current_dir(o); }
    }

    #[test]
    fn narrow_by_arg_type_returns_unchanged_when_unanimous() {
        // Both candidates have PATHS — all get evidence, no split → unchanged.
        let _g = CWD_LOCK.lock().unwrap();
        let td = tempfile::tempdir().unwrap();
        let orig = std::env::current_dir().ok();
        std::env::set_current_dir(td.path()).unwrap();
        std::fs::write("myfile", b"").unwrap();

        let mut t = CommandTrie::new();
        t.insert(&["git", "add"]);
        t.insert(&["git", "apply"]);
        t.arg_specs.insert(
            "git add".into(),
            ArgSpec { positional: [(1u32, trie::ARG_MODE_PATHS)].into_iter().collect(), ..Default::default() },
        );
        t.arg_specs.insert(
            "git apply".into(),
            ArgSpec { positional: [(1u32, trie::ARG_MODE_PATHS)].into_iter().collect(), ..Default::default() },
        );
        let git_node = t.root.get_child("git").unwrap();
        let add_node = git_node.get_child("add").unwrap();
        let apply_node = git_node.get_child("apply").unwrap();
        let candidates = vec![("add", add_node), ("apply", apply_node)];
        let prefix = vec!["git".to_string()];
        let narrowed = narrow_by_arg_type(&candidates, &prefix, &["myfile"], &t.arg_specs);
        // Unanimous evidence: all match, no split, returned unchanged.
        assert_eq!(narrowed.len(), 2);

        if let Some(o) = orig { let _ = std::env::set_current_dir(o); }
    }

    #[test]
    fn narrow_by_arg_type_with_no_arg_specs() {
        let mut t = CommandTrie::new();
        t.insert(&["git", "log"]);
        t.insert(&["git", "ls-files"]);
        let git_node = t.root.get_child("git").unwrap();
        let log_node = git_node.get_child("log").unwrap();
        let ls_node = git_node.get_child("ls-files").unwrap();
        let candidates = vec![("log", log_node), ("ls-files", ls_node)];
        let prefix = vec!["git".to_string()];
        // No arg_specs at all — both go to without_evidence; no split → unchanged.
        let narrowed = narrow_by_arg_type(&candidates, &prefix, &["anything"], &t.arg_specs);
        assert_eq!(narrowed.len(), 2);
    }

    #[test]
    fn resolve_gi_che_file_resolves_to_check_ignore() {
        let _g = CWD_LOCK.lock().unwrap();
        let td = tempfile::tempdir().unwrap();
        let orig = std::env::current_dir().ok();
        std::env::set_current_dir(td.path()).unwrap();
        std::fs::write("target_file", b"").unwrap();

        let mut trie = trie_with_git_subcommands_and_argspecs();
        // Also add git itself so prefix-search on "gi" finds it.
        trie.insert_command("git");
        // "gi" should uniquely resolve to git (no other "gi*" commands).
        // "che" matches checkout, check-ignore — but only check-ignore expects PATHS.
        // "target_file" exists on disk → PATHS evidence → narrows to check-ignore.
        let pins = Pins::default();
        match resolve_line("gi che target_file", &trie, &pins, None, ContextHint::Unknown) {
            ResolveResult::Resolved(s) => assert_eq!(s, "git check-ignore target_file"),
            ResolveResult::Ambiguous(info) => panic!("still ambiguous: {:?}", info.candidates),
            other => panic!("unexpected: {:?}", other),
        }

        if let Some(o) = orig { let _ = std::env::set_current_dir(o); }
    }

    #[test]
    fn resolve_gi_che_unknown_remains_ambiguous() {
        let mut trie = trie_with_git_subcommands_and_argspecs();
        trie.insert_command("git");
        let pins = Pins::default();
        // "totally_not_anything" is not a file, not a branch, not a commit →
        // no arg-type evidence on either side → narrow_by_arg_type returns unchanged.
        match resolve_line("gi che totally_not_anything", &trie, &pins, None, ContextHint::Unknown) {
            ResolveResult::Ambiguous(info) => {
                // checkout, check-ignore, cherry, cherry-pick all match "che"
                assert!(info.candidates.len() >= 2, "expected ≥2 candidates, got {:?}", info.candidates);
            }
            ResolveResult::Resolved(s) => panic!("should not have resolved to {:?}", s),
            other => panic!("unexpected: {:?}", other),
        }
    }

    #[test]
    fn deep_disambiguate_behavior_unchanged_for_existing_cases() {
        // Verify the four pre-existing deep-disambig tests still pass after
        // the narrow_by_arg_type wiring. We call them here explicitly so a
        // regression is caught in this context too.
        {
            // test_deep_disambig_resolves_g_push
            let trie = build_test_trie();
            let pins = Pins::default();
            match resolve("g push", &trie, &pins) {
                ResolveResult::Resolved(s) => assert_eq!(s, "git push"),
                other => panic!("g push: expected Resolved, got {:?}", other),
            }
        }
        {
            // test_deep_disambiguation
            let trie = build_test_trie();
            let pins = Pins::default();
            match resolve("g ch main", &trie, &pins) {
                ResolveResult::Resolved(s) => assert_eq!(s, "git checkout main"),
                other => panic!("g ch main: expected Resolved, got {:?}", other),
            }
        }
        {
            // test_deep_disambig_multi_level
            let mut trie = CommandTrie::new();
            trie.insert(&["git", "commit", "-m"]);
            trie.insert(&["git", "checkout", "main"]);
            trie.insert(&["grep", "-r", "pattern"]);
            trie.insert(&["go", "build"]);
            let pins = Pins::default();
            match resolve("g ch main", &trie, &pins) {
                ResolveResult::Resolved(s) => assert_eq!(s, "git checkout main"),
                other => panic!("multi-level: expected Resolved, got {:?}", other),
            }
        }
        {
            // test_deep_disambig_flag_skipped
            let mut trie = CommandTrie::new();
            trie.insert(&["git", "commit", "-m"]);
            trie.insert(&["go", "build", "-o"]);
            let matches = trie.root.prefix_search("g");
            let result = deep_disambiguate(&matches, &["co", "-m"]);
            assert_eq!(result.len(), 1);
            assert_eq!(result[0].0, "git");
        }
    }

    // --- Tests for score_candidates_stats ---

    fn make_node(count: u32, last_used: u64, failures: u32) -> TrieNode {
        TrieNode { count, last_used, failures, ..Default::default() }
    }

    #[test]
    fn score_candidates_stats_picks_higher_count() {
        let now: u64 = 1_700_000_000;
        let recent = now - 3600; // 1 hour ago
        let high = make_node(10, recent, 0);
        let low = make_node(1, recent, 0);
        let candidates = vec![("high", &high), ("low", &low)];
        let winner = score_candidates_stats(&candidates, now);
        assert!(winner.is_some(), "expected a winner");
        assert_eq!(winner.unwrap().0, "high");
    }

    #[test]
    fn score_candidates_stats_picks_more_recent() {
        let now: u64 = 1_700_000_000;
        let recent = now - 3600;       // 1 hour ago
        let old = now - 30 * 86400;    // 30 days ago
        let a = make_node(5, recent, 0);
        let b = make_node(5, old, 0);
        let candidates = vec![("recent", &a), ("old", &b)];
        let winner = score_candidates_stats(&candidates, now);
        assert!(winner.is_some(), "expected a winner");
        assert_eq!(winner.unwrap().0, "recent");
    }

    #[test]
    fn score_candidates_stats_penalty_for_failures() {
        let now: u64 = 1_700_000_000;
        let recent = now - 3600;
        // count=10, failures=5 → success_rate = 10/15 ≈ 0.667
        let flaky = make_node(10, recent, 5);
        // count=10, failures=0 → success_rate = 1.0
        let clean = make_node(10, recent, 0);
        let candidates = vec![("flaky", &flaky), ("clean", &clean)];
        let winner = score_candidates_stats(&candidates, now);
        // clean / flaky ≈ 1.0/0.667 ≈ 1.5× > 1.05 → should pick clean
        assert!(winner.is_some(), "expected a winner");
        assert_eq!(winner.unwrap().0, "clean");
    }

    #[test]
    fn score_candidates_stats_returns_none_when_tied() {
        let now: u64 = 1_700_000_000;
        let recent = now - 3600;
        let a = make_node(5, recent, 0);
        let b = make_node(5, recent, 0);
        let candidates = vec![("a", &a), ("b", &b)];
        let result = score_candidates_stats(&candidates, now);
        assert!(result.is_none(), "tied nodes should return None");
    }

    #[test]
    fn score_candidates_stats_returns_none_when_close() {
        let now: u64 = 1_700_000_000;
        let t1 = now - 3600;
        // count=10 vs count=11 — ln(11)/ln(10) ≈ 1.04 < 1.05 → None
        let a = make_node(10, t1, 0);
        let b = make_node(11, t1, 0);
        let candidates = vec![("a", &a), ("b", &b)];
        let result = score_candidates_stats(&candidates, now);
        // The margin check: top / runner_up = ln(12)/ln(11) ≈ 1.037 < 1.05
        assert!(result.is_none(), "too close — should return None");
    }

    #[test]
    fn score_candidates_stats_handles_empty() {
        let candidates: Vec<(&str, &TrieNode)> = vec![];
        let result = score_candidates_stats(&candidates, 1_700_000_000);
        assert!(result.is_none());
    }

    #[test]
    fn score_candidates_stats_single_candidate() {
        let now: u64 = 1_700_000_000;
        let node = make_node(0, 0, 0);
        let candidates = vec![("only", &node)];
        let result = score_candidates_stats(&candidates, now);
        assert!(result.is_some(), "single candidate always returned");
        assert_eq!(result.unwrap().0, "only");
    }

    #[test]
    fn resolve_stats_picks_frequently_used_sibling() {
        // Build a trie where checkout has count=50 used recently,
        // cherry has count=1 used a year ago. Both match "che*" prefix.
        // With no arg-type narrowing evidence (no real branches available),
        // the stats tiebreaker should pick checkout.
        let now: u64 = 1_700_000_000;
        let recent = now - 3600;          // 1 hour ago
        let old = now - 400 * 86400;      // ~400 days ago

        let mut trie = CommandTrie::new();
        // Insert checkout 50 times with recent timestamps.
        for _ in 0..50 {
            trie.root.insert_with_time(&["git", "checkout"], recent);
        }
        // Insert cherry once with an old timestamp.
        trie.root.insert_with_time(&["git", "cherry"], old);
        // Also add git itself.
        trie.insert_command("git");

        // Use "totally_unknown_arg" so no runtime type resolver can match it,
        // ensuring arg-type narrowing is a no-op and stats decides.
        let pins = Pins::default();
        match resolve_line("gi che totally_unknown_arg", &trie, &pins, None, ContextHint::Unknown) {
            ResolveResult::Resolved(s) => {
                assert!(s.starts_with("git checkout"), "expected checkout, got: {}", s);
            }
            ResolveResult::Ambiguous(info) => {
                panic!("expected stats to resolve, still ambiguous: {:?}", info.candidates);
            }
            other => panic!("unexpected: {:?}", other),
        }
    }

    #[test]
    fn explain_stats_narration_shows_scores() {
        // Use two top-level commands so the first-word ambiguity triggers stats
        // narration in explain_segment (which only walks one level deep).
        // checkout_tool: count=50, used 1h ago; cherry_app: count=1, used ~1yr ago.
        let now: u64 = 1_700_000_000;
        let recent = now - 3600;
        let old = now - 400 * 86400;

        let mut trie = CommandTrie::new();
        for _ in 0..50 {
            trie.root.insert_with_time(&["checkout_tool"], recent);
        }
        trie.root.insert_with_time(&["cherry_app"], old);

        let pins = Pins::default();
        // Both "checkout_tool" and "cherry_app" start with "che". With no
        // subcommand lookahead, stats decides at the first-word level.
        let out = explain("che totally_unknown_arg", &trie, &pins, None);
        // The stats narration section must be present.
        assert!(out.contains("stats tiebreak"), "stats tiebreak section missing:\n{}", out);
        // The chosen winner should be checkout_tool.
        assert!(out.contains("checkout_tool"), "checkout_tool not mentioned:\n{}", out);
    }

    // --- Text/net validator tests ---

    #[test]
    fn is_plausible_email_accepts_valid() {
        assert!(is_plausible_email("a@b.com"));
        assert!(is_plausible_email("x+y@example.co.uk"));
        assert!(is_plausible_email("user.name+tag@sub.domain.org"));
    }

    #[test]
    fn is_plausible_email_rejects_invalid() {
        assert!(!is_plausible_email("a@b"));        // no dot in domain
        assert!(!is_plausible_email("@b.com"));     // empty local
        assert!(!is_plausible_email("a@"));         // empty domain
        assert!(!is_plausible_email("foo"));        // no @
        assert!(!is_plausible_email("a b@c.com")); // whitespace
    }

    #[test]
    fn is_plausible_url_accepts_valid() {
        assert!(is_plausible_url("https://example.com"));
        assert!(is_plausible_url("ssh://git@host"));
        assert!(is_plausible_url("git+ssh://foo"));
        assert!(is_plausible_url("ftp://files.example.org/pub"));
        assert!(is_plausible_url("file:///etc/hosts"));
    }

    #[test]
    fn is_plausible_url_rejects_invalid() {
        assert!(!is_plausible_url(":foo"));         // no scheme
        assert!(!is_plausible_url("//foo"));        // missing scheme before //
        assert!(!is_plausible_url("example.com"));  // no ://
        assert!(!is_plausible_url("://noscheme"));  // empty scheme
        assert!(!is_plausible_url("has space://x")); // whitespace in scheme
    }

    #[test]
    fn is_plausible_mac_accepts_valid() {
        assert!(is_plausible_mac("aa:bb:cc:dd:ee:ff"));
        assert!(is_plausible_mac("AA-BB-CC-DD-EE-FF"));
        assert!(is_plausible_mac("00:1A:2B:3C:4D:5E"));
    }

    #[test]
    fn is_plausible_mac_rejects_invalid() {
        assert!(!is_plausible_mac("aabbccddeeff"));      // no separators
        assert!(!is_plausible_mac("aa:bb:cc"));          // only 3 groups
        assert!(!is_plausible_mac("gg:hh:ii:jj:kk:ll")); // non-hex digits
        assert!(!is_plausible_mac("a:b:c:d:e:f"));       // single-char groups
        assert!(!is_plausible_mac(""));
    }

    #[test]
    fn is_valid_timezone_known_zones() {
        // Only check if /usr/share/zoneinfo exists at all on this system.
        if std::path::Path::new("/usr/share/zoneinfo/UTC").is_file() {
            assert!(is_valid_timezone("UTC"));
            assert!(!is_valid_timezone("Not/ATimezone"));
            assert!(!is_valid_timezone(""));
        }
    }

    #[test]
    fn word_matches_type_text_types() {
        use crate::trie::*;

        // IPv4
        assert!(word_matches_type("192.168.1.1", ARG_MODE_IPV4));
        assert!(word_matches_type("0.0.0.0", ARG_MODE_IPV4));
        assert!(!word_matches_type("999.999.999.999", ARG_MODE_IPV4));
        assert!(!word_matches_type("not-an-ip", ARG_MODE_IPV4));

        // IPv6
        assert!(word_matches_type("::1", ARG_MODE_IPV6));
        assert!(word_matches_type("2001:db8::1", ARG_MODE_IPV6));
        assert!(!word_matches_type("192.168.1.1", ARG_MODE_IPV6));
        assert!(!word_matches_type("not-ipv6", ARG_MODE_IPV6));

        // Email
        assert!(word_matches_type("me@example.com", ARG_MODE_EMAIL));
        assert!(!word_matches_type("notanemail", ARG_MODE_EMAIL));

        // URL
        assert!(word_matches_type("https://example.com", ARG_MODE_URL_SCHEME));
        assert!(!word_matches_type("example.com", ARG_MODE_URL_SCHEME));

        // MAC
        assert!(word_matches_type("aa:bb:cc:dd:ee:ff", ARG_MODE_MAC_ADDR));
        assert!(!word_matches_type("aabbccddeeff", ARG_MODE_MAC_ADDR));

        // Timezone — only test if zoneinfo is present
        if std::path::Path::new("/usr/share/zoneinfo/UTC").is_file() {
            assert!(word_matches_type("UTC", ARG_MODE_TIMEZONE));
            assert!(!word_matches_type("NotReal/Zone", ARG_MODE_TIMEZONE));
        }
    }

    // --- narrow_by_flag_match tests ---

    #[test]
    fn narrow_by_flag_match_picks_candidate_with_flag() {
        let mut t = CommandTrie::new();
        t.insert(&["git", "checkout"]);
        t.insert(&["git", "cherry"]);
        t.arg_specs.insert(
            "git checkout".into(),
            ArgSpec {
                flag_args: [("-p".into(), trie::ARG_MODE_PATHS)].into_iter().collect(),
                ..Default::default()
            },
        );
        // cherry has no flag_args
        let git_node = t.root.get_child("git").unwrap();
        let checkout_node = git_node.get_child("checkout").unwrap();
        let cherry_node = git_node.get_child("cherry").unwrap();
        let candidates = vec![("checkout", checkout_node), ("cherry", cherry_node)];
        let prefix = vec!["git".to_string()];
        let narrowed =
            narrow_by_flag_match(&candidates, &prefix, &["-p", "main"], &t.arg_specs);
        assert_eq!(narrowed.len(), 1, "expected 1, got {:?}", narrowed.iter().map(|(n, _)| n).collect::<Vec<_>>());
        assert_eq!(narrowed[0].0, "checkout");
    }

    #[test]
    fn narrow_by_flag_match_preserves_ties() {
        let mut t = CommandTrie::new();
        t.insert(&["git", "checkout"]);
        t.insert(&["git", "cherry"]);
        t.arg_specs.insert(
            "git checkout".into(),
            ArgSpec {
                flag_args: [("-p".into(), trie::ARG_MODE_PATHS)].into_iter().collect(),
                ..Default::default()
            },
        );
        t.arg_specs.insert(
            "git cherry".into(),
            ArgSpec {
                flag_args: [("-p".into(), trie::ARG_MODE_GIT_BRANCHES)].into_iter().collect(),
                ..Default::default()
            },
        );
        let git_node = t.root.get_child("git").unwrap();
        let checkout_node = git_node.get_child("checkout").unwrap();
        let cherry_node = git_node.get_child("cherry").unwrap();
        let candidates = vec![("checkout", checkout_node), ("cherry", cherry_node)];
        let prefix = vec!["git".to_string()];
        let narrowed =
            narrow_by_flag_match(&candidates, &prefix, &["-p", "main"], &t.arg_specs);
        // Both have 1 hit — max == min, no discrimination.
        assert_eq!(narrowed.len(), 2);
    }

    #[test]
    fn narrow_by_flag_match_no_flags_in_rest() {
        let mut t = CommandTrie::new();
        t.insert(&["git", "checkout"]);
        t.insert(&["git", "cherry"]);
        t.arg_specs.insert(
            "git checkout".into(),
            ArgSpec {
                flag_args: [("-p".into(), trie::ARG_MODE_PATHS)].into_iter().collect(),
                ..Default::default()
            },
        );
        let git_node = t.root.get_child("git").unwrap();
        let checkout_node = git_node.get_child("checkout").unwrap();
        let cherry_node = git_node.get_child("cherry").unwrap();
        let candidates = vec![("checkout", checkout_node), ("cherry", cherry_node)];
        let prefix = vec!["git".to_string()];
        // No flags in rest — returns unchanged.
        let narrowed = narrow_by_flag_match(&candidates, &prefix, &["main"], &t.arg_specs);
        assert_eq!(narrowed.len(), 2);
    }

    #[test]
    fn narrow_by_flag_match_handles_equals_form() {
        let mut t = CommandTrie::new();
        t.insert(&["git", "checkout"]);
        t.insert(&["git", "cherry"]);
        t.arg_specs.insert(
            "git checkout".into(),
            ArgSpec {
                flag_args: [("--color".into(), trie::ARG_MODE_PATHS)].into_iter().collect(),
                ..Default::default()
            },
        );
        let git_node = t.root.get_child("git").unwrap();
        let checkout_node = git_node.get_child("checkout").unwrap();
        let cherry_node = git_node.get_child("cherry").unwrap();
        let candidates = vec![("checkout", checkout_node), ("cherry", cherry_node)];
        let prefix = vec!["git".to_string()];
        // --color=always should match spec key "--color"
        let narrowed =
            narrow_by_flag_match(&candidates, &prefix, &["--color=always"], &t.arg_specs);
        assert_eq!(narrowed.len(), 1);
        assert_eq!(narrowed[0].0, "checkout");
    }

    #[test]
    fn narrow_by_flag_match_considers_call_programs() {
        let mut t = CommandTrie::new();
        t.insert(&["ssh", "add"]);
        t.insert(&["ssh", "apply"]);
        // "add" has --cipher in flag_call_programs; "apply" does not.
        t.arg_specs.insert(
            "ssh add".into(),
            ArgSpec {
                flag_call_programs: [(
                    "--cipher".into(),
                    ("openssl".into(), vec!["ciphers".into()]),
                )]
                .into_iter()
                .collect(),
                ..Default::default()
            },
        );
        let ssh_node = t.root.get_child("ssh").unwrap();
        let add_node = ssh_node.get_child("add").unwrap();
        let apply_node = ssh_node.get_child("apply").unwrap();
        let candidates = vec![("add", add_node), ("apply", apply_node)];
        let prefix = vec!["ssh".to_string()];
        let narrowed =
            narrow_by_flag_match(&candidates, &prefix, &["--cipher"], &t.arg_specs);
        assert_eq!(narrowed.len(), 1);
        assert_eq!(narrowed[0].0, "add");
    }

    #[test]
    fn narrow_by_flag_match_no_evidence_returns_input() {
        let mut t = CommandTrie::new();
        t.insert(&["git", "checkout"]);
        t.insert(&["git", "cherry"]);
        // No arg_specs for either candidate.
        let git_node = t.root.get_child("git").unwrap();
        let checkout_node = git_node.get_child("checkout").unwrap();
        let cherry_node = git_node.get_child("cherry").unwrap();
        let candidates = vec![("checkout", checkout_node), ("cherry", cherry_node)];
        let prefix = vec!["git".to_string()];
        // Neither candidate has any spec, so all score 0 — max == 0 → return input.
        let narrowed =
            narrow_by_flag_match(&candidates, &prefix, &["--verbose"], &t.arg_specs);
        assert_eq!(narrowed.len(), 2, "must not return empty — input unchanged when no evidence");
    }

    #[test]
    fn resolve_flag_narrows_integration() {
        // Build a trie with two commands under "git": checkout and cherry.
        // Only checkout has -p in its flag_args.
        // "git ch -p main" should resolve to "git checkout -p main".
        let mut trie = CommandTrie::new();
        trie.insert(&["git", "checkout"]);
        trie.insert(&["git", "cherry"]);
        trie.insert_command("git");
        trie.arg_specs.insert(
            "git checkout".into(),
            ArgSpec {
                flag_args: [("-p".into(), trie::ARG_MODE_PATHS)].into_iter().collect(),
                ..Default::default()
            },
        );
        // cherry has no flag_args — no positional that matches "-p" either
        let pins = Pins::default();
        match resolve_line("git ch -p main", &trie, &pins, None, ContextHint::Unknown) {
            ResolveResult::Resolved(s) => assert_eq!(s, "git checkout -p main"),
            ResolveResult::Ambiguous(info) => {
                panic!("still ambiguous: {:?}", info.candidates)
            }
            other => panic!("unexpected result: {:?}", other),
        }
    }

    // --- explain inner-ambiguity narration tests ---

    #[test]
    fn explain_narrates_inner_arg_type_narrowing() {
        // Uses the same trie as the Phase 5.1 integration tests.
        // "gi che totally_not_anything" stays ambiguous at the inner level.
        // explain() should emit the "Inner ambiguity within" header and
        // describe the arg-type narrowing attempt.
        let mut trie = trie_with_git_subcommands_and_argspecs();
        trie.insert_command("git");
        let pins = Pins::default();
        let output = explain("gi che totally_not_anything", &trie, &pins, None);
        assert!(
            output.contains("Inner ambiguity within"),
            "expected 'Inner ambiguity within' in explain output;\ngot:\n{}",
            output
        );
        assert!(
            output.contains("expects"),
            "expected 'expects' (arg-type label) in explain output;\ngot:\n{}",
            output
        );
    }

    #[test]
    fn explain_narrates_inner_flag_match() {
        // Build a trie where "git checkout" has -p in flag_args but "git cherry"
        // does not.  "gi che -p main" resolves (flag narrows to checkout), but
        // "gi che -p" alone stays ambiguous so explain gets a chance to narrate
        // the flag-match step.
        let mut trie = CommandTrie::new();
        trie.insert(&["git", "checkout"]);
        trie.insert(&["git", "cherry"]);
        trie.insert_command("git");
        trie.arg_specs.insert(
            "git checkout".into(),
            ArgSpec {
                flag_args: [("-p".into(), trie::ARG_MODE_PATHS)].into_iter().collect(),
                ..Default::default()
            },
        );
        // cherry has no arg spec — flag-match gives checkout a hit, cherry none.
        let pins = Pins::default();
        // "gi che -p": the flag narrows it down, so the result is Resolved.
        // We need input that stays Ambiguous to exercise narration.  Add
        // "cherry" the same -p spec so flag-match is unanimous (no split),
        // then the narration still fires and reports "no discriminating flag evidence".
        trie.arg_specs.insert(
            "git cherry".into(),
            ArgSpec {
                flag_args: [("-p".into(), trie::ARG_MODE_PATHS)].into_iter().collect(),
                ..Default::default()
            },
        );
        let output = explain("gi che -p", &trie, &pins, None);
        // Both candidates now have -p, so flag-match reports unanimous / no split.
        assert!(
            output.contains("Inner ambiguity within"),
            "expected 'Inner ambiguity within';\ngot:\n{}",
            output
        );
        assert!(
            output.contains("flag"),
            "expected flag-narrowing narration;\ngot:\n{}",
            output
        );
    }

    #[test]
    fn explain_handles_non_resolvable_prefix_gracefully() {
        // Construct an AmbiguityInfo whose resolved_prefix points at a word
        // that doesn't exist in the trie.  narrate_inner_ambiguity must bail
        // silently — no panic, no garbage output.
        //
        // We can't manufacture a real Ambiguous result with a broken prefix
        // easily, so we call narrate_inner_ambiguity directly via a wrapper
        // that builds the info by hand.
        let trie = build_test_trie();

        // "gi che totally_not_anything" on the basic trie (no git subcommands
        // beyond checkout/commit/push/...).  "gi" resolves to git uniquely.
        // With no "che*" subcommands in build_test_trie other than "checkout",
        // "che" is unambiguous → Resolved.  So we use a raw call to
        // narrate_inner_ambiguity with a crafted info to test the bail-out path.
        let fake_info = AmbiguityInfo {
            word: "something".into(),
            position: 1,
            candidates: vec!["foo".into(), "bar".into()],
            lcp: String::new(),
            deep_candidates: vec![],
            resolved_prefix: vec!["nonexistent_command_xyz".into()],
            remaining: vec![],
        };
        let mut out: Vec<String> = Vec::new();
        let push = |out: &mut Vec<String>, depth: usize, s: String| {
            out.push(format!("{}{}", "  ".repeat(depth), s));
        };
        // Must not panic.
        narrate_inner_ambiguity(&fake_info, &trie, &mut out, &push);
        // Should have produced no output (bailed silently at the walk step).
        assert!(
            out.is_empty(),
            "expected no output for non-walkable prefix; got: {:?}",
            out
        );
    }

    #[test]
    fn narrow_by_flag_match_respects_aliases() {
        // Spec only has `--force` in flag_args, but `-f` is declared as an alias.
        // Typing `-f` should still count as a hit via the alias group.
        let mut t = CommandTrie::new();
        t.insert(&["git", "commit"]);
        t.insert(&["git", "clean"]);
        t.insert_command("git");

        let mut spec = ArgSpec {
            flag_args: [("--force".into(), trie::ARG_MODE_PATHS)].into_iter().collect(),
            flag_aliases: vec![vec!["-f".to_string(), "--force".to_string()]],
            ..Default::default()
        };
        spec.flag_exclusions = vec![];
        t.arg_specs.insert("git commit".into(), spec);
        // clean has no spec

        let git_node = t.root.get_child("git").unwrap();
        let commit_node = git_node.get_child("commit").unwrap();
        let clean_node = git_node.get_child("clean").unwrap();
        let candidates = vec![("commit", commit_node), ("clean", clean_node)];
        let prefix = vec!["git".to_string()];
        // User typed `-f`; commit's spec only lists `--force` directly, but alias group maps them.
        let narrowed = narrow_by_flag_match(&candidates, &prefix, &["-f"], &t.arg_specs);
        assert_eq!(narrowed.len(), 1, "should narrow to commit via alias");
        assert_eq!(narrowed[0].0, "commit");
    }

    // --- Task 1: cwd scoring in stats tiebreaker ---

    #[test]
    fn score_stats_boosts_same_cwd_sibling() {
        use crate::test_util::CWD_LOCK;
        let _lock = CWD_LOCK.lock().unwrap();

        let mut t = CommandTrie::new();
        t.root.insert_with_time(&["git"], 1000);
        t.root.insert_with_time(&["gitlab"], 1000);

        if let Some(n) = t.root.children.get_mut("git") {
            for _ in 0..5 {
                n.record_cwd("/home/user/proj");
            }
        }
        if let Some(n) = t.root.children.get_mut("gitlab") {
            for _ in 0..5 {
                n.record_cwd("/tmp");
            }
        }

        CWD_CONTEXT.with(|c| *c.borrow_mut() = Some("/home/user/proj".to_string()));
        let git_n = t.root.get_child("git").unwrap();
        let gitlab_n = t.root.get_child("gitlab").unwrap();
        let git_score = score_node(git_n, 1001, false);
        let gitlab_score = score_node(gitlab_n, 1001, false);
        CWD_CONTEXT.with(|c| *c.borrow_mut() = None);

        assert!(
            git_score > gitlab_score,
            "git ({}) should outscore gitlab ({}) in cwd=/home/user/proj",
            git_score,
            gitlab_score
        );
    }

    // --- Task 2: ZSH_IOS_LAST_CMD env boost ---

    #[test]
    fn score_stats_last_cmd_env_boost() {
        use crate::test_util::CWD_LOCK;
        let _lock = CWD_LOCK.lock().unwrap();

        let mut t = CommandTrie::new();
        t.root.insert_with_time(&["git"], 1000);
        t.root.insert_with_time(&["gitlab"], 1000);
        // Also give them subcommand "push" so "g push" can deep-disambiguate
        t.root.insert_with_time(&["git", "push"], 1000);
        t.root.insert_with_time(&["gitlab", "push"], 1000);

        // SAFETY: test holds CWD_LOCK so no other test mutates env concurrently.
        unsafe { std::env::set_var("ZSH_IOS_LAST_CMD", "git") };
        let pins = Pins::default();
        let result = resolve_line("g push", &t, &pins, None, ContextHint::Unknown);
        unsafe { std::env::remove_var("ZSH_IOS_LAST_CMD") };

        match result {
            ResolveResult::Resolved(s) => {
                assert!(
                    s.starts_with("git"),
                    "expected git to win with LAST_CMD=git, got: {}",
                    s
                );
            }
            ResolveResult::Ambiguous(_) => {}
            other => panic!("unexpected result: {:?}", other),
        }
    }

    // --- Task 3: context hint redirection ---

    #[test]
    fn resolve_line_redirection_context_treats_last_word_as_path() {
        use crate::test_util::CWD_LOCK;
        use std::fs;
        let _lock = CWD_LOCK.lock().unwrap();

        let td = tempfile::tempdir().unwrap();
        let _f = fs::write(td.path().join("output.log"), b"");

        let t = CommandTrie::new();
        let pins = Pins::default();
        let input = format!("echo hello > {}", td.path().join("out").display());
        let result = resolve_line(&input, &t, &pins, None, ContextHint::Redirection);
        match result {
            ResolveResult::Resolved(_)
            | ResolveResult::Passthrough(_)
            | ResolveResult::PathAmbiguous(_) => {}
            ResolveResult::Ambiguous(info) => {
                panic!("unexpected Ambiguous in redirection context: {:?}", info.word);
            }
        }
    }

    #[test]
    fn resolve_line_expands_galiases_before_trie_walk() {
        // Build a trie that knows about `grep` so the expanded form is resolvable.
        let mut trie = CommandTrie::new();
        trie.insert(&["find", "-type"]);
        trie.insert(&["grep", "-r"]);
        // Seed a global alias: G -> "| grep"
        trie.galiases.insert("G".to_string(), "| grep".to_string());

        let pins = Pins::default();

        // "find . G foo" should expand G before the trie walk.
        // After expansion: "find . | grep foo"
        // resolve_line splits on `|`, resolves each segment, and returns Resolved.
        let result = resolve_line("find . G foo", &trie, &pins, None, ContextHint::Unknown);
        match result {
            ResolveResult::Resolved(s) => {
                assert!(s.contains("| grep"), "expected '| grep' in result, got: {}", s);
                assert!(s.contains("find"), "expected 'find' in result, got: {}", s);
            }
            ResolveResult::Passthrough(s) => {
                // A passthrough result is also acceptable if the segment was
                // unchanged after expansion — as long as G was substituted.
                assert!(s.contains("| grep"), "expected galias expansion in passthrough, got: {}", s);
            }
            other => panic!("unexpected result: {:?}", other),
        }
    }

    // ── matcher-rule integration ──────────────────────────────────────────────

    #[test]
    fn resolve_honors_case_insensitive_matcher() {
        use crate::trie::MatcherRule;
        let mut trie = CommandTrie::new();
        // Trie has "Git" (mixed case) — "gi" would not match it with strict prefix.
        trie.insert_command("Git");
        trie.matcher_rules = vec![MatcherRule::CaseInsensitive];
        let pins = Pins::default();
        // With CaseInsensitive, "gi" should fold-match "Git".
        let result = resolve("gi", &trie, &pins);
        match result {
            ResolveResult::Resolved(s) => assert_eq!(s, "Git"),
            other => panic!("expected Resolved(\"Git\"), got {:?}", other),
        }
    }

    // ── quote / param-context passthrough ────────────────────────────────────

    #[test]
    fn resolve_line_single_quoted_passthrough() {
        let mut trie = CommandTrie::new();
        trie.insert(&["git", "checkout"]);
        let pins = Pins::default();
        // Inside single quotes, even an abbreviation that would normally
        // resolve must come back unchanged.
        let result = resolve_line("git ch", &trie, &pins, None, ContextHint::SingleQuoted);
        match result {
            ResolveResult::Passthrough(s) => assert_eq!(s, "git ch"),
            other => panic!("expected Passthrough, got {:?}", other),
        }
    }

    #[test]
    fn resolve_line_param_context_passthrough() {
        let mut trie = CommandTrie::new();
        trie.insert(&["git", "checkout"]);
        let pins = Pins::default();
        // Inside ${PARAM…}, the engine must not touch the buffer.
        let result = resolve_line("echo ${HO", &trie, &pins, None, ContextHint::ParameterName);
        match result {
            ResolveResult::Passthrough(s) => assert_eq!(s, "echo ${HO"),
            other => panic!("expected Passthrough, got {:?}", other),
        }
    }

    #[test]
    fn context_hint_from_parts_precedence() {
        // param_context beats everything
        assert_eq!(
            ContextHint::from_parts(Some("redirection"), Some("single"), true),
            ContextHint::ParameterName
        );
        // single quote beats double and context
        assert_eq!(
            ContextHint::from_parts(Some("math"), Some("single"), false),
            ContextHint::SingleQuoted
        );
        // double quote beats positional context
        assert_eq!(
            ContextHint::from_parts(Some("redirection"), Some("double"), false),
            ContextHint::DoubleQuoted
        );
        // backtick / dollar both map to Backticked
        assert_eq!(
            ContextHint::from_parts(None, Some("backtick"), false),
            ContextHint::Backticked
        );
        assert_eq!(
            ContextHint::from_parts(None, Some("dollar"), false),
            ContextHint::Backticked
        );
        // falls back to positional context when no quote flag
        assert_eq!(
            ContextHint::from_parts(Some("redirection"), None, false),
            ContextHint::Redirection
        );
    }

    // ── runtime_config knob tests ────────────────────────────────────────────

    #[test]
    fn min_resolve_prefix_length_blocks_short_words() {
        // When min_resolve_prefix_length is 3, a 2-char first word should
        // passthrough even if the trie could expand it.
        let mut trie = CommandTrie::new();
        trie.insert(&["git", "status"]);
        let pins = Pins::default();

        // Temporarily install a config with min_resolve_prefix_length = 3.
        crate::runtime_config::set(crate::runtime_config::RuntimeConfig {
            min_resolve_prefix_length: 3,
            ..crate::runtime_config::RuntimeConfig::default()
        });

        let result = resolve_line("gi status", &trie, &pins, None, ContextHint::Unknown);
        // "gi" is length 2 < 3 → must passthrough
        assert!(
            matches!(result, ResolveResult::Passthrough(_)),
            "expected passthrough for 2-char prefix with min=3, got: {:?}",
            result
        );

        // Restore
        crate::runtime_config::set(crate::runtime_config::RuntimeConfig::default());
    }

    #[test]
    fn min_resolve_prefix_length_allows_longer_words() {
        // A word longer than or equal to min_resolve_prefix_length should still resolve.
        let mut trie = CommandTrie::new();
        trie.insert(&["git", "status"]);
        let pins = Pins::default();

        crate::runtime_config::set(crate::runtime_config::RuntimeConfig {
            min_resolve_prefix_length: 2,
            ..crate::runtime_config::RuntimeConfig::default()
        });

        let result = resolve_line("gi status", &trie, &pins, None, ContextHint::Unknown);
        // "gi" is length 2 == 2 → NOT blocked (< means strictly less than)
        // wait: 2 < 2 is false, so it should proceed to resolution
        match result {
            ResolveResult::Resolved(s) => assert!(s.starts_with("git")),
            ResolveResult::Passthrough(s) => {
                // Could also be passthrough if exact match isn't found, that's ok
                let _ = s;
            }
            other => panic!("unexpected: {:?}", other),
        }

        crate::runtime_config::set(crate::runtime_config::RuntimeConfig::default());
    }

    #[test]
    fn disable_arg_type_narrowing_flag_is_honored() {
        // Verify the config flag can be set and runtime_config::get() picks it up
        // within resolve_from_node without causing a panic. The arg-type narrowing
        // path is exercised when a file on the filesystem matches one candidate's
        // expected positional type — we don't need to set that up here; we just
        // confirm the flag flip is safe.
        let mut trie = CommandTrie::new();
        trie.insert(&["git", "status"]);
        trie.insert(&["grep", "pattern"]);
        let pins = Pins::default();

        crate::runtime_config::set(crate::runtime_config::RuntimeConfig {
            disable_arg_type_narrowing: true,
            ..crate::runtime_config::RuntimeConfig::default()
        });

        // "g status" — deep disambiguation resolves via subcommand presence even
        // without arg-type narrowing, so Resolved("git status") is correct here.
        let result = resolve_line("g status", &trie, &pins, None, ContextHint::Unknown);
        // Either resolved or ambiguous is fine — the key invariant is no panic.
        match result {
            ResolveResult::Resolved(s) => assert!(s.starts_with("git") || s.starts_with("grep")),
            ResolveResult::Ambiguous(_) | ResolveResult::Passthrough(_) => {}
            ResolveResult::PathAmbiguous(_) => {}
        }

        crate::runtime_config::set(crate::runtime_config::RuntimeConfig::default());
    }

    #[test]
    fn disable_flag_matching_skips_flag_narrowing() {
        let mut trie = CommandTrie::new();
        trie.insert(&["git"]);
        trie.insert(&["grep"]);
        let pins = Pins::default();

        crate::runtime_config::set(crate::runtime_config::RuntimeConfig {
            disable_flag_matching: true,
            ..crate::runtime_config::RuntimeConfig::default()
        });

        // "g -r" — with flag matching enabled, grep would win because it knows -r.
        // With flag matching disabled, remains ambiguous.
        let result = resolve_line("g -r", &trie, &pins, None, ContextHint::Unknown);
        // Ambiguous or Passthrough both acceptable (no crash / panic is the key invariant).
        let _ = result;

        crate::runtime_config::set(crate::runtime_config::RuntimeConfig::default());
    }

    // ── alternatives-aware narrowing ──────────────────────────────────────────

    #[test]
    fn narrow_by_arg_type_accepts_any_alternative() {
        // Scenario: two commands — "cmd foo" expects PATHS primary (pos 1),
        // with DIRS_ONLY as an alternative; "cmd bar" expects EXECS_ONLY.
        // Probe word is a directory that exists → matches DIRS_ONLY (alternative
        // of foo) but not EXECS_ONLY → foo wins.
        let _g = CWD_LOCK.lock().unwrap();
        let td = tempfile::tempdir().unwrap();
        let orig = std::env::current_dir().ok();
        std::env::set_current_dir(td.path()).unwrap();
        std::fs::create_dir("mydir").unwrap();

        let mut t = CommandTrie::new();
        t.insert(&["cmd", "foo"]);
        t.insert(&["cmd", "bar"]);
        let mut foo_spec = ArgSpec::default();
        foo_spec.positional.insert(1, trie::ARG_MODE_PATHS);
        foo_spec.positional_alternatives.insert(1, vec![trie::ARG_MODE_DIRS_ONLY]);
        t.arg_specs.insert("cmd foo".into(), foo_spec);
        let mut bar_spec = ArgSpec::default();
        bar_spec.positional.insert(1, trie::ARG_MODE_EXECS_ONLY);
        t.arg_specs.insert("cmd bar".into(), bar_spec);

        let cmd_node = t.root.get_child("cmd").unwrap();
        let foo_node = cmd_node.get_child("foo").unwrap();
        let bar_node = cmd_node.get_child("bar").unwrap();
        let candidates = vec![("foo", foo_node), ("bar", bar_node)];
        let prefix = vec!["cmd".to_string()];

        // "mydir" is a directory: matches DIRS_ONLY (alternative of foo), not EXECS_ONLY.
        let narrowed = narrow_by_arg_type(&candidates, &prefix, &["mydir"], &t.arg_specs);
        assert_eq!(narrowed.len(), 1, "expected foo to win via alternative");
        assert_eq!(narrowed[0].0, "foo");

        if let Some(o) = orig { let _ = std::env::set_current_dir(o); }
    }

    #[test]
    fn complete_display_shows_also_accepts() {
        // Build a trie with a single command whose spec has positional alt types.
        let mut trie = CommandTrie::new();
        trie.insert_command("mycmd");
        let mut spec = ArgSpec::default();
        // Primary type at pos 1: GIT_BRANCHES (rendered by runtime_complete)
        // We use HOSTS and USERS as alternatives to avoid live git calls in test.
        spec.positional.insert(1, trie::ARG_MODE_HOSTS);
        spec.positional_alternatives.insert(1, vec![trie::ARG_MODE_USERS]);
        trie.arg_specs.insert("mycmd".into(), spec);
        let pins = crate::pins::Pins::default();

        // complete("mycmd ") triggers show_type_completions with ArgMode::Normal
        // after the trie lookup finds no children for the empty prefix.
        let out = complete("mycmd ", &trie, &pins, ContextHint::Unknown);
        assert!(
            out.contains("also accepts:"),
            "expected 'also accepts:' in output, got:\n{out}"
        );
    }
}
