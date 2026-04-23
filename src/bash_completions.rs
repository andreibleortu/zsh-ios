use crate::trie::{CommandTrie, ARG_MODE_PATHS};
use std::collections::HashMap;
use std::fs;
use std::path::Path;

/// Scan standard Bash completion locations and supplement the trie with
/// subcommand and flag data extracted by static text analysis only.
/// Existing entries are preserved — Zsh and Fish data wins on conflicts.
///
/// Returns (commands_enriched, subcommands_added, flags_added).
pub fn scan_bash_completions(trie: &mut CommandTrie) -> (u32, u32, u32) {
    let dirs = bash_dirs();
    scan_bash_dirs(trie, &dirs.iter().map(String::as_str).collect::<Vec<_>>())
}

fn bash_dirs() -> Vec<String> {
    let mut out = Vec::new();
    for p in &[
        "/etc/bash_completion.d",
        "/usr/share/bash-completion/completions",
        "/usr/local/share/bash-completion/completions",
        "/opt/homebrew/share/bash-completion/completions",
        "/opt/homebrew/etc/bash_completion.d",
    ] {
        if Path::new(p).is_dir() {
            out.push((*p).into());
        }
    }
    if let Some(h) = dirs::home_dir() {
        for rel in &[
            ".local/share/bash-completion/completions",
            ".bash_completion.d",
        ] {
            let p = h.join(rel);
            if p.is_dir() {
                out.push(p.to_string_lossy().into());
            }
        }
    }
    out
}

fn scan_bash_dirs(trie: &mut CommandTrie, dirs: &[&str]) -> (u32, u32, u32) {
    let mut per_cmd: HashMap<String, ParsedCommand> = HashMap::new();

    for dir in dirs {
        let Ok(read_dir) = fs::read_dir(dir) else {
            continue;
        };
        for entry in read_dir.flatten() {
            let path = entry.path();
            let ext = path.extension().and_then(|e| e.to_str());
            // Accept .bash files and extension-less files (common in completion dirs).
            let ok = ext == Some("bash") || ext.is_none();
            if !ok {
                continue;
            }
            // Skip directories.
            if path.is_dir() {
                continue;
            }
            let Ok(content) = fs::read_to_string(&path) else {
                continue;
            };
            parse_bash_file(&content, &mut per_cmd);
        }
    }

    merge_into_trie(trie, per_cmd)
}

/// Intermediate representation for all data extracted from a bash completion
/// file before merging into the trie.
#[derive(Default)]
struct ParsedCommand {
    /// Top-level subcommands / words (including flag-style words like `--foo`).
    top_subs: Vec<String>,
    /// Per-subcommand nested args from `case "$prev" in sub) compgen -W "..." ;;` blocks.
    case_subs: HashMap<String, Vec<String>>,
}

/// Strip inline comments from a single bash line.
/// Only strip `#` that is outside quotes and not part of a word.
fn strip_comment(line: &str) -> &str {
    let mut in_single = false;
    let mut in_double = false;
    let mut prev_backslash = false;
    let bytes = line.as_bytes();
    for (i, &b) in bytes.iter().enumerate() {
        if prev_backslash {
            prev_backslash = false;
            continue;
        }
        if b == b'\\' && !in_single {
            prev_backslash = true;
            continue;
        }
        if b == b'\'' && !in_double {
            in_single = !in_single;
            continue;
        }
        if b == b'"' && !in_single {
            in_double = !in_double;
            continue;
        }
        if b == b'#' && !in_single && !in_double {
            return &line[..i];
        }
    }
    line
}

/// Tokenize a bash line into a list of tokens, respecting single- and
/// double-quoted strings and backslash escapes. Stops at `;`, `|`, `&&`,
/// `||` shell metacharacters encountered outside quotes.
pub fn tokenize_bash_line(line: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut chars = line.chars().peekable();
    let mut quote: Option<char> = None;

    while let Some(c) = chars.next() {
        match (c, quote) {
            ('\\', None) | ('\\', Some('"')) => {
                // Backslash escape: consume the next character literally.
                if let Some(&next) = chars.peek() {
                    cur.push(next);
                    chars.next();
                }
            }
            ('\'', None) => quote = Some('\''),
            ('\'', Some('\'')) => quote = None,
            ('"', None) => quote = Some('"'),
            ('"', Some('"')) => quote = None,
            (c2, Some(_)) => cur.push(c2),
            // Stop at shell control characters outside quotes.
            (';' | '|' | '&', None) => break,
            (c2, None) if c2.is_whitespace() => {
                if !cur.is_empty() {
                    out.push(std::mem::take(&mut cur));
                }
            }
            (c2, None) => cur.push(c2),
        }
    }
    if !cur.is_empty() {
        out.push(cur);
    }
    out
}

/// Parse a full bash completion file and collect entries into `per_cmd`.
fn parse_bash_file(content: &str, per_cmd: &mut HashMap<String, ParsedCommand>) {
    // Strip comments, join continuation lines.
    let mut lines: Vec<String> = Vec::new();
    let mut pending = String::new();
    for raw in content.lines() {
        let stripped = strip_comment(raw);
        if let Some(cont) = stripped.trim_end().strip_suffix('\\') {
            pending.push_str(cont);
            pending.push(' ');
        } else {
            pending.push_str(stripped);
            lines.push(std::mem::take(&mut pending));
        }
    }
    if !pending.is_empty() {
        lines.push(pending);
    }

    // Collect the function bodies: name -> body_text.
    let func_bodies = extract_function_bodies(&lines);

    // Process each `complete` line.
    for line in &lines {
        let trimmed = line.trim_start();
        if !trimmed.starts_with("complete") {
            continue;
        }
        let after = &trimmed["complete".len()..];
        if !after.starts_with(|c: char| c.is_whitespace()) {
            continue;
        }

        let tokens = tokenize_bash_line(trimmed);
        if tokens.is_empty() || tokens[0] != "complete" {
            continue;
        }

        parse_complete_invocation(&tokens, &func_bodies, per_cmd);
    }
}

/// Given a tokenized `complete ...` invocation, extract subcommands/flags
/// and add them to the per-cmd map.
fn parse_complete_invocation(
    tokens: &[String],
    func_bodies: &HashMap<String, String>,
    per_cmd: &mut HashMap<String, ParsedCommand>,
) {
    let mut w_words: Vec<String> = Vec::new();
    let mut func_name: Option<String> = None;
    // Trailing non-flag tokens are the command names.
    let mut cmd_names: Vec<String> = Vec::new();

    let mut i = 1; // skip "complete"
    while i < tokens.len() {
        let tok = &tokens[i];
        match tok.as_str() {
            "-W" => {
                i += 1;
                if i < tokens.len() {
                    for w in tokens[i].split_whitespace() {
                        let w = w.trim();
                        if !w.is_empty() && looks_like_completion_word(w) {
                            w_words.push(w.to_string());
                        }
                    }
                }
            }
            "-F" => {
                i += 1;
                if i < tokens.len() {
                    func_name = Some(tokens[i].clone());
                }
            }
            // Flags that consume a value but we don't use: -A, -G, -P, -S, -X, -u, -g, etc.
            "-A" | "-G" | "-P" | "-S" | "-X" | "-u" | "-g" | "-j" | "-v" | "-e" => {
                i += 1; // skip the following value
            }
            // Boolean flags: -o, -a, -b, -c, -d, -f, -k, -s, -t, -p, -r, -C, -D, -E, -I, -T
            "-o" => {
                i += 1; // -o takes a value like "default", skip it
            }
            tok if tok.starts_with('-') => {
                // Unknown flag; if it could take a value we risk misidentifying
                // the next token. We conservatively skip only single-char flags.
                if tok.len() == 2
                    && let Some(next) = tokens.get(i + 1)
                    && !next.starts_with('-')
                    && !is_likely_cmd_name(next)
                {
                    i += 1; // skip the value
                }
            }
            _ => {
                // Non-flag token: it's a command name that `complete` applies to.
                cmd_names.push(tok.clone());
            }
        }
        i += 1;
    }

    if cmd_names.is_empty() {
        return;
    }

    for cmd in &cmd_names {
        // Sanitize: command names shouldn't contain slashes or spaces.
        if cmd.contains('/') || cmd.contains(' ') {
            continue;
        }
        let rec = per_cmd.entry(cmd.clone()).or_default();

        // Pattern 1: -W "word list"
        for w in &w_words {
            if !rec.top_subs.contains(w) {
                rec.top_subs.push(w.clone());
            }
        }

        // Pattern 2/3: -F _function
        if let Some(ref fname) = func_name
            && let Some(body) = func_bodies.get(fname.as_str())
        {
            parse_function_body(body, rec);
        }
    }
}

/// Returns true if a token looks like a plain command name (used to avoid
/// misidentifying command args as flag values).
fn is_likely_cmd_name(tok: &str) -> bool {
    !tok.is_empty()
        && !tok.starts_with('-')
        && tok.chars().all(|c| c.is_alphanumeric() || c == '_' || c == '-' || c == '.')
}

/// Extract all bash function bodies from the already-stripped line list.
/// Supports both `_func() {` and `function _func {` forms.
/// Returns a map of function_name -> body_text.
fn extract_function_bodies(lines: &[String]) -> HashMap<String, String> {
    let mut bodies: HashMap<String, String> = HashMap::new();
    let mut i = 0;

    while i < lines.len() {
        let trimmed = lines[i].trim();

        // Detect function start: `fname()` or `fname () {` or `function fname {`.
        if let Some(name) = try_parse_func_line(trimmed) {
            // Collect body until matching `}` at the outermost level.
            let body = collect_function_body(lines, &mut i);
            bodies.insert(name, body);
        }

        i += 1;
    }

    bodies
}

/// Try to parse a function definition line and return the function name.
/// Handles:
///   `_func() {`        `_func()  {`
///   `_func ()  {`      (space before parens)
///   `function _func {` `function _func() {`
fn try_parse_func_line(line: &str) -> Option<String> {
    let trimmed = line.trim();

    // `function NAME` form.
    if let Some(rest) = trimmed.strip_prefix("function")
        && rest.starts_with(|c: char| c.is_whitespace())
    {
        let name_part = rest.trim_start();
        // Name ends at whitespace, `(`, or `{`.
        let name: String = name_part
            .chars()
            .take_while(|&c| c != '(' && c != '{' && !c.is_whitespace())
            .collect();
        if name.is_empty() {
            return None;
        }
        return Some(name);
    }

    // `NAME()` form.
    // The name comes before `(`.
    if let Some(paren_pos) = trimmed.find('(') {
        let name_part = trimmed[..paren_pos].trim();
        // Function names must consist of identifier-safe characters only.
        // This guards against matching case arm patterns like `*)`  or `foo|bar)`.
        let is_valid_name = !name_part.is_empty()
            && !name_part.starts_with('#')
            && name_part
                .chars()
                .all(|c| c.is_alphanumeric() || c == '_' || c == '-' || c == '.');
        if is_valid_name {
            // After the closing `)` there should be optional whitespace and `{`.
            let after_paren = trimmed[paren_pos..].trim_start_matches('(');
            let after_close = after_paren.trim_start_matches(')').trim();
            if after_close.starts_with('{') || after_close.is_empty() {
                return Some(name_part.to_string());
            }
        }
    }

    None
}

/// Starting from line `i` (the function definition line), scan forward and
/// collect everything until the matching closing `}`. Updates `i` to point at
/// the closing brace line. Returns the body as a single string.
fn collect_function_body(lines: &[String], i: &mut usize) -> String {
    let mut depth: i32 = 0;
    let mut body = String::new();
    let start = *i;

    for (j, line) in lines.iter().enumerate().skip(start) {
        // Count `{` and `}` outside strings to track nesting.
        depth += count_braces(line);
        if j > start {
            body.push_str(line);
            body.push('\n');
        } else {
            // First line: only add the part after the opening `{`.
            if let Some(brace_pos) = line.find('{') {
                body.push_str(&line[brace_pos + 1..]);
                body.push('\n');
            }
        }
        if depth <= 0 && j > start {
            *i = j;
            return body;
        }
    }
    *i = lines.len().saturating_sub(1);
    body
}

/// Count net brace depth change on a line (naively, outside strings).
fn count_braces(line: &str) -> i32 {
    let mut depth: i32 = 0;
    let mut in_single = false;
    let mut in_double = false;
    let mut prev_bs = false;
    for b in line.bytes() {
        if prev_bs {
            prev_bs = false;
            continue;
        }
        if b == b'\\' && !in_single {
            prev_bs = true;
            continue;
        }
        if b == b'\'' && !in_double {
            in_single = !in_single;
            continue;
        }
        if b == b'"' && !in_single {
            in_double = !in_double;
            continue;
        }
        if !in_single && !in_double {
            if b == b'{' {
                depth += 1;
            } else if b == b'}' {
                depth -= 1;
            }
        }
    }
    depth
}

/// Heuristics applied to a function body to extract subcommand words.
fn parse_function_body(body: &str, rec: &mut ParsedCommand) {
    // Build a small local variable map first: `local VAR="words"` or `VAR="words"`.
    let var_map = collect_local_vars(body);

    // Skip functions that are clearly too dynamic.
    if is_too_dynamic(body) {
        return;
    }

    // Look for `case "$prev" in` or `case $prev in` blocks.
    let case_subs = extract_case_prev_subs(body, &var_map);
    for (sub, words) in case_subs {
        let entry = rec.case_subs.entry(sub).or_default();
        for w in words {
            if !entry.contains(&w) {
                entry.push(w);
            }
        }
    }

    // Look for direct `COMPREPLY=( $(compgen -W "..." ...) )` or
    // `COMPREPLY=( $(compgen -W "$VAR" ...) )` lines outside any case block
    // (treat the whole function body for top-level patterns).
    for w in extract_compgen_words(body, &var_map) {
        if !rec.top_subs.contains(&w) {
            rec.top_subs.push(w);
        }
    }

    // Also extract `opts="--foo --bar"` style static option lists.
    for w in extract_opts_vars(body, &var_map) {
        if !rec.top_subs.contains(&w) {
            rec.top_subs.push(w);
        }
    }
}

/// Returns true if the body uses constructs we can't safely analyze statically.
fn is_too_dynamic(body: &str) -> bool {
    // eval is always a red flag.
    if body.contains("eval ") || body.contains("eval\t") {
        return true;
    }
    false
}

/// Collect `local VAR="value"` and `VAR="value"` assignments into a map.
/// Only handles simple quoted string values on a single line.
fn collect_local_vars(body: &str) -> HashMap<String, String> {
    let mut map: HashMap<String, String> = HashMap::new();
    for line in body.lines() {
        let t = line.trim();
        // Strip optional `local ` prefix.
        let t = t.strip_prefix("local ").unwrap_or(t).trim_start();
        // Match `NAME="value"` or `NAME='value'`.
        if let Some((name, rest)) = t.split_once('=') {
            let name = name.trim();
            if name.contains(|c: char| c.is_whitespace() || c == '$') {
                continue;
            }
            let rest = rest.trim();
            let value = if rest.len() >= 2
                && ((rest.starts_with('"') && rest.ends_with('"'))
                    || (rest.starts_with('\'') && rest.ends_with('\'')))
            {
                &rest[1..rest.len() - 1]
            } else {
                rest
            };
            map.insert(name.to_string(), value.to_string());
        }
    }
    map
}

/// Extract words from `compgen -W "..."` or `compgen -W "$VAR"` patterns.
///
/// Skips compgen invocations inside `case ... esac` blocks — those are
/// flag-value enumerations (handled separately by extract_case_prev_subs),
/// not top-level subcommands. Without this guard, `fdisk`'s
/// `case $prev in '-b') compgen -W "512 1024 2048 4096" ;; esac`
/// leaks "512" etc. as apparent top-level subcommands.
fn extract_compgen_words(body: &str, var_map: &HashMap<String, String>) -> Vec<String> {
    let mut out = Vec::new();
    let mut case_depth: u32 = 0;
    for line in body.lines() {
        let t = line.trim();

        // Track case..esac nesting so we don't capture per-flag enum values.
        if t.starts_with("case ") && t.ends_with(" in") {
            case_depth += 1;
            continue;
        }
        if (t == "esac" || t.starts_with("esac ") || t.starts_with("esac;"))
            && case_depth > 0
        {
            case_depth -= 1;
            continue;
        }
        if case_depth > 0 {
            continue;
        }

        // Require `compgen` as a standalone command (not `_comp_compgen` or
        // other wrapper functions where the word list is contextual rather than
        // global). `compgen` must be preceded by `$(`, `(`, or whitespace/line
        // start, not by an underscore or alphanumeric character.
        if !contains_standalone_compgen(t) {
            continue;
        }
        // Find `-W` in the line.
        if let Some(w_pos) = find_flag_w(t) {
            let after = t[w_pos..].trim_start();
            // after starts with the value (possibly quoted).
            let raw = extract_quoted_or_var(after, var_map);
            // If the extracted value is itself a variable reference (e.g. the
            // string literal was "$opts"), resolve it through the var_map.
            let value: &str = if let Some(stripped) = raw.strip_prefix('$') {
                let var = stripped.trim_matches(|c| c == '{' || c == '}');
                var_map.get(var).map(String::as_str).unwrap_or(raw)
            } else {
                raw
            };
            // If the resolved value is itself a subshell / shell expression
            // (e.g. `comps=$( compgen -A file ... )`), don't split it — the
            // tokens inside are shell syntax, not completion words.
            if value.contains("$(") || value.contains('`') || value.trim_start().starts_with('$') {
                continue;
            }
            for word in value.split_whitespace() {
                let word = word.trim();
                if !word.is_empty() && looks_like_completion_word(word) {
                    out.push(word.to_string());
                }
            }
        }
    }
    out
}

/// Return true if `line` contains `compgen` as a standalone command token
/// (not as part of `_comp_compgen` or other wrapper function names).
///
/// `compgen` must be immediately preceded by `$(`, `(`, whitespace, or the
/// start of the string — not by an alphanumeric character or underscore.
fn contains_standalone_compgen(line: &str) -> bool {
    let needle = b"compgen";
    let bytes = line.as_bytes();
    let nlen = needle.len();
    let mut i = 0;
    while i + nlen <= bytes.len() {
        if &bytes[i..i + nlen] == needle {
            // Check character before `compgen` (if any).
            let before_ok = i == 0
                || matches!(bytes[i - 1], b' ' | b'\t' | b'(' | b'`');
            // Check character after `compgen` (if any).
            let after_ok = i + nlen >= bytes.len()
                || !bytes[i + nlen].is_ascii_alphanumeric() && bytes[i + nlen] != b'_';
            if before_ok && after_ok {
                return true;
            }
        }
        i += 1;
    }
    false
}

/// Find the position of the word following `-W` in a compgen line.
/// Returns the byte offset of the first character of the value token.
fn find_flag_w(line: &str) -> Option<usize> {
    // We need `-W` followed by whitespace.
    let bytes = line.as_bytes();
    let mut i = 0;
    while i + 1 < bytes.len() {
        if bytes[i] == b'-' && bytes[i + 1] == b'W' {
            let after = i + 2;
            // Must be followed by whitespace.
            if after < bytes.len() && (bytes[after] == b' ' || bytes[after] == b'\t') {
                // Skip whitespace.
                let val_start = line[after..].find(|c: char| !c.is_whitespace())? + after;
                return Some(val_start);
            }
        }
        i += 1;
    }
    None
}

/// Given a string starting at a quoted value or a `$VAR` reference, return
/// the underlying string value. Strips surrounding quotes.
fn extract_quoted_or_var<'a>(s: &'a str, var_map: &'a HashMap<String, String>) -> &'a str {
    let s = s.trim();
    if s.starts_with('"') || s.starts_with('\'') {
        // Find the matching end quote, return the inner content.
        let q = s.chars().next().unwrap();
        if let Some(end) = s[1..].find(q) {
            return &s[1..end + 1];
        }
        return s;
    }
    if let Some(var) = s.strip_prefix('$') {
        // Strip braces if present.
        let var = var.trim_matches(|c| c == '{' || c == '}');
        if let Some(val) = var_map.get(var) {
            return val.as_str();
        }
    }
    s
}

/// Extract words from `opts="--foo --bar"` or `OPTIONS="..."` style variables
/// that are later referenced via `compgen -W "$opts"`.
fn extract_opts_vars(body: &str, var_map: &HashMap<String, String>) -> Vec<String> {
    let mut out = Vec::new();
    let mut case_depth: u32 = 0;
    // Find all variable names referenced in compgen -W "$VAR" patterns.
    for line in body.lines() {
        let t = line.trim();

        if t.starts_with("case ") && t.ends_with(" in") {
            case_depth += 1;
            continue;
        }
        if (t == "esac" || t.starts_with("esac ") || t.starts_with("esac;"))
            && case_depth > 0
        {
            case_depth -= 1;
            continue;
        }
        if case_depth > 0 {
            continue;
        }

        if !t.contains("compgen") {
            continue;
        }
        if let Some(w_pos) = find_flag_w(t) {
            let after = t[w_pos..].trim_start();
            if let Some(after_dollar) = after.strip_prefix('$') {
                let var = after_dollar.trim_matches(|c| c == '{' || c == '}');
                let var: String = var
                    .chars()
                    .take_while(|c| c.is_alphanumeric() || *c == '_')
                    .collect();
                if let Some(val) = var_map.get(&var) {
                    // Skip variable values that are themselves subshell
                    // expressions — splitting them yields shell syntax, not
                    // completion words.
                    if val.contains("$(") || val.contains('`') || val.trim_start().starts_with('$')
                    {
                        continue;
                    }
                    for word in val.split_whitespace() {
                        let word = word.trim();
                        if !word.is_empty() && looks_like_completion_word(word) {
                            out.push(word.to_string());
                        }
                    }
                }
            }
        }
    }
    out
}

/// Returns true if a word looks like a valid completion word (subcommand or flag).
/// Rejects shell variable expressions, subshell syntax, redirections, and
/// other shell-operator tokens that leak in when a compgen -W value captured
/// raw shell source.
fn looks_like_completion_word(w: &str) -> bool {
    if w.contains('$') || w.contains('(') || w.contains(')') || w.contains('`') {
        return false;
    }
    if w.len() > 64 {
        return false;
    }
    // Redirections: `2>&1`, `>file`, `<file`, `>>log`, `2>`, `&>`, etc.
    if w.contains('>') || w.contains('<') {
        return false;
    }
    // Pipes / control operators embedded in a word.
    if w.contains('|') || w.contains(';') || w.contains('&') {
        return false;
    }
    // Brace expansion / parameter syntax.
    if w.contains('{') || w.contains('}') {
        return false;
    }
    // Must start with alphanumeric, `-`, or `_`.
    w.starts_with(|c: char| c.is_alphanumeric() || c == '-' || c == '_')
}

/// Extract per-subcommand words from `case "$prev" in ... esac` blocks.
/// Returns a map of subcommand_label -> list_of_words.
fn extract_case_prev_subs(
    body: &str,
    var_map: &HashMap<String, String>,
) -> HashMap<String, Vec<String>> {
    let mut result: HashMap<String, Vec<String>> = HashMap::new();

    // Guard: if the function ALSO dispatches on a subcommand word (e.g.
    // `case $words[2]`, `case "$1"`, `case $object`, `case $command`) then the
    // `case $prev in` block is almost certainly handling *flag-value*
    // completions (e.g. `wep-key-type)` → `compgen -W "key phrase"`), not
    // subcommand dispatch.  Treating flag-value arm labels as subcommands
    // pollutes the trie with junk like `nmcli → wep-key-type`.
    let has_other_case_dispatch = body.lines().any(|l| {
        let t = l.trim();
        if !t.starts_with("case ") || !t.ends_with(" in") {
            return false;
        }
        // Words-based dispatch: `case $words[N]`, `case ${words[N]}`, `case $1`, `case "$1"`
        let is_words = t.contains("$words") || t.contains("${words") || t.contains("\"$words");
        let is_positional = t.contains("\"$1\"") || t.contains("$1 ") || t.ends_with("$1 in")
            || t.contains("\"$2\"") || t.contains("$2 ") || t.ends_with("$2 in");
        // Named variable dispatch that is NOT $prev/$cur/$COMP_WORDS:
        // e.g. `case $object in`, `case $command in`, `case $subcommand in`
        let is_named_var = (t.contains("$object") || t.contains("$command") || t.contains("$subcommand")
            || t.contains("$cmd") || t.contains("$subcmd") || t.contains("$action"))
            && !t.contains("$prev") && !t.contains("$cur");
        is_words || is_positional || is_named_var
    });
    if has_other_case_dispatch {
        return result;
    }

    // Two-pass approach:
    // Pass 1: collect ALL arm label→word mappings, including flag arms (labels
    //   starting with `-`).  Flag arms are needed to identify which non-flag
    //   labels are actually flag VALUES rather than subcommands.
    // Pass 2: drop non-flag arm labels that appear as completion words for any
    //   flag arm (they are flag values, not subcommands).

    // --- Pass 1 ---
    // Full result including flag arms.
    let mut full: HashMap<String, Vec<String>> = HashMap::new();

    // Find `case "$prev" in` or `case $prev in` (also `$1`, `$cur`, etc. — we skip those).
    let mut in_case = false;
    let mut current_labels: Vec<String> = Vec::new();

    for line in body.lines() {
        let t = line.trim();

        if !in_case {
            // Look for `case "$prev" in` or `case $prev in`.
            if is_case_prev_line(t) {
                in_case = true;
                current_labels.clear();
            }
            continue;
        }

        // Inside a case block.
        if t == "esac" || t.starts_with("esac ") || t.starts_with("esac;") {
            in_case = false;
            current_labels.clear();
            continue;
        }

        // Case arm pattern: `word|word2)` or `"word")` or `*)`.
        // Guard: must end with `)` AND the text before `)` must not contain
        // nested parens (which would indicate a command substitution or
        // function call, not a pattern arm).
        if t.ends_with(')') && !t.starts_with('#') && !t.contains("$(") && !t.contains("=(") {
            let pattern = t.trim_end_matches(')');
            // A case arm pattern should not contain spaces (outside quotes) in
            // the simple forms we care about; complex patterns are skipped.
            let has_unquoted_space = {
                let mut in_q = false;
                let mut found = false;
                for ch in pattern.chars() {
                    if ch == '\'' || ch == '"' {
                        in_q = !in_q;
                    } else if ch == ' ' && !in_q {
                        found = true;
                        break;
                    }
                }
                found
            };
            if !has_unquoted_space {
                // Collect all labels, including flags (starting with `-`).
                // `*` and `$` are still excluded; `is_likely_cmd_name` is NOT applied here
                // so that flag labels (like `--passthrough`) are also captured for pass 2.
                let labels: Vec<String> = pattern
                    .split('|')
                    .map(|p| p.trim().trim_matches(|c| c == '"' || c == '\'').to_string())
                    .filter(|p| {
                        !p.is_empty()
                            && !p.contains('*')
                            && !p.contains('$')
                            && p != "--"
                    })
                    .collect();
                current_labels = labels;
                continue;
            }
        }

        // Inside an arm: look for compgen -W.
        if !current_labels.is_empty()
            && t.contains("compgen")
            && let Some(w_pos) = find_flag_w(t)
        {
            let after = t[w_pos..].trim_start();
            let value = extract_quoted_or_var(after, var_map);
            let words: Vec<String> = value
                .split_whitespace()
                .filter(|w| looks_like_completion_word(w))
                .map(|w| w.to_string())
                .collect();
            if !words.is_empty() {
                for label in &current_labels {
                    full.entry(label.clone()).or_default().extend(words.clone());
                }
            }
        }

        // `;;` ends the arm.
        if t == ";;" || t.ends_with(";;") {
            current_labels.clear();
        }
    }

    // --- Pass 2 ---
    // Build the set of words produced by any FLAG arm (label starts with `-`).
    // These words are flag values, not subcommands — any non-flag arm label
    // that also appears in this set is a flag value and must not become a
    // trie child.
    //
    // Example (firewall-cmd):
    //   --passthrough|--*-chain|...)   → ipv4 ipv6 eb   ← flag arm, words are flag values
    //   ipv4|ipv6|eb)                  → nat filter mangle
    // Without this filter, `ipv4`, `ipv6`, and `eb` would be inserted as
    // `firewall-cmd → ipv4`, polluting the completion list.
    let flag_words: std::collections::HashSet<String> = full
        .iter()
        .filter(|(label, _)| label.starts_with('-'))
        .flat_map(|(_, words)| words.clone())
        .collect();

    for (label, words) in full {
        // Keep only non-flag labels that:
        // (a) look like plausible command/subcommand names, AND
        // (b) do NOT appear as completion words for flag arms.
        if !label.starts_with('-') && is_likely_cmd_name(&label) && !flag_words.contains(&label) {
            result.entry(label).or_default().extend(words);
        }
    }

    result
}

/// Returns true if a line looks like `case "$prev" in` or `case $prev in`.
fn is_case_prev_line(line: &str) -> bool {
    let t = line.trim();
    if !t.starts_with("case ") {
        return false;
    }
    // Must end with `in` or `in;` or have `in` before `;`.
    let has_prev = t.contains("$prev") || t.contains("\"$prev\"") || t.contains("'$prev'");
    let has_in = {
        let words: Vec<&str> = t.split_whitespace().collect();
        words.last().copied() == Some("in") || words.contains(&"in")
    };
    has_prev && has_in
}

/// Merge the collected per-command data into the trie. Additive only —
/// existing entries are not overwritten.
fn merge_into_trie(
    trie: &mut CommandTrie,
    per_cmd: HashMap<String, ParsedCommand>,
) -> (u32, u32, u32) {
    let mut cmds_enriched: u32 = 0;
    let mut subs_added: u32 = 0;
    let mut flags_added: u32 = 0;

    for (cmd, parsed) in per_cmd {
        let mut did_something = false;

        // Top-level subcommands / words.
        for sub in &parsed.top_subs {
            trie.insert(&[cmd.as_str(), sub.as_str()]);
            subs_added += 1;
            did_something = true;
        }

        // Case-arm per-subcommand words: insert as cmd -> sub -> word.
        for (sub, words) in &parsed.case_subs {
            for w in words {
                trie.insert(&[cmd.as_str(), sub.as_str(), w.as_str()]);
                subs_added += 1;
                did_something = true;
            }
        }

        // Flags that start with `-`: record in arg_specs.
        for sub in &parsed.top_subs {
            if sub.starts_with('-') {
                let spec = trie.arg_specs.entry(cmd.clone()).or_default();
                spec.flag_args.entry(sub.clone()).or_insert(ARG_MODE_PATHS);
                flags_added += 1;
            }
        }

        if did_something {
            cmds_enriched += 1;
        }
    }

    (cmds_enriched, subs_added, flags_added)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::trie::CommandTrie;
    use std::fs;

    // --- tokenizer tests ---

    #[test]
    fn tokenize_bash_simple() {
        let tokens = tokenize_bash_line("complete -W \"a b\" cmd");
        assert_eq!(tokens, vec!["complete", "-W", "a b", "cmd"]);
    }

    #[test]
    fn tokenize_bash_escaped_quote() {
        let tokens = tokenize_bash_line(r#"complete -W "foo \"bar\" baz" cmd"#);
        // Inner escaped quotes should be included in the token value.
        assert!(tokens.iter().any(|t| t.contains("bar")), "{:?}", tokens);
        let w_val = tokens.iter().find(|t| t.contains("foo")).unwrap();
        assert!(w_val.contains("bar"), "expected inner quote preserved: {:?}", w_val);
    }

    #[test]
    fn tokenize_bash_single_quoted() {
        let tokens = tokenize_bash_line("complete -W 'foo bar' cmd");
        assert_eq!(tokens, vec!["complete", "-W", "foo bar", "cmd"]);
    }

    // --- end-to-end scan tests ---

    #[test]
    fn parse_complete_w_form() {
        let dir = tempfile::tempdir().unwrap();
        let f = dir.path().join("mycmd.bash");
        fs::write(&f, "complete -W \"foo bar baz\" mycmd\n").unwrap();

        let mut trie = CommandTrie::new();
        let (cmds, subs, _flags) =
            scan_bash_dirs(&mut trie, &[dir.path().to_str().unwrap()]);

        assert!(cmds >= 1, "expected at least 1 command enriched");
        assert!(subs >= 3, "expected foo, bar, baz; got {}", subs);

        let node = trie.root.get_child("mycmd").expect("mycmd not in trie");
        assert!(node.get_child("foo").is_some(), "foo missing");
        assert!(node.get_child("bar").is_some(), "bar missing");
        assert!(node.get_child("baz").is_some(), "baz missing");
    }

    #[test]
    fn parse_complete_f_static_compgen() {
        let dir = tempfile::tempdir().unwrap();
        let f = dir.path().join("mycmd.bash");
        fs::write(
            &f,
            r#"_mycmd() {
    COMPREPLY=( $(compgen -W "start stop restart" -- "$cur") )
}
complete -F _mycmd mycmd
"#,
        )
        .unwrap();

        let mut trie = CommandTrie::new();
        let (_cmds, subs, _flags) =
            scan_bash_dirs(&mut trie, &[dir.path().to_str().unwrap()]);

        assert!(subs >= 3, "expected start, stop, restart; got {}", subs);
        let node = trie.root.get_child("mycmd").expect("mycmd not in trie");
        assert!(node.get_child("start").is_some(), "start missing");
        assert!(node.get_child("stop").is_some(), "stop missing");
        assert!(node.get_child("restart").is_some(), "restart missing");
    }

    #[test]
    fn parse_complete_f_local_var() {
        let dir = tempfile::tempdir().unwrap();
        let f = dir.path().join("mycmd.bash");
        fs::write(
            &f,
            r#"_mycmd() {
    local opts="a b c"
    COMPREPLY=( $(compgen -W "$opts" -- "$cur") )
}
complete -F _mycmd mycmd
"#,
        )
        .unwrap();

        let mut trie = CommandTrie::new();
        let (_cmds, subs, _) =
            scan_bash_dirs(&mut trie, &[dir.path().to_str().unwrap()]);

        assert!(subs >= 3, "expected a, b, c; got {}", subs);
        let node = trie.root.get_child("mycmd").expect("mycmd not in trie");
        assert!(node.get_child("a").is_some(), "a missing");
        assert!(node.get_child("b").is_some(), "b missing");
        assert!(node.get_child("c").is_some(), "c missing");
    }

    #[test]
    fn parse_complete_f_case_prev() {
        let dir = tempfile::tempdir().unwrap();
        let f = dir.path().join("mycmd.bash");
        fs::write(
            &f,
            r#"_mycmd() {
    case "$prev" in
        remote)
            COMPREPLY=( $(compgen -W "add remove show" -- "$cur") )
            ;;
        branch)
            COMPREPLY=( $(compgen -W "list delete rename" -- "$cur") )
            ;;
        *)
            ;;
    esac
}
complete -F _mycmd mycmd
"#,
        )
        .unwrap();

        let mut trie = CommandTrie::new();
        let (_cmds, subs, _) =
            scan_bash_dirs(&mut trie, &[dir.path().to_str().unwrap()]);

        assert!(subs >= 6, "expected 6 nested subs; got {}", subs);
        let node = trie.root.get_child("mycmd").expect("mycmd not in trie");
        let remote = node.get_child("remote").expect("remote missing");
        assert!(remote.get_child("add").is_some(), "remote->add missing");
        assert!(remote.get_child("remove").is_some(), "remote->remove missing");
        let branch = node.get_child("branch").expect("branch missing");
        assert!(branch.get_child("list").is_some(), "branch->list missing");
    }

    #[test]
    fn case_prev_with_word_dispatch_drops_flag_value_labels() {
        // When a completion function ALSO has a `case $words[N]` (real subcommand
        // dispatch), the `case $prev in` block handles flag-value completions.
        // Its arm labels (like `wep-key-type`) must NOT be treated as subcommands.
        let dir = tempfile::tempdir().unwrap();
        let f = dir.path().join("nmcli.bash");
        fs::write(
            &f,
            r#"_nmcli() {
    local cur prev words cword
    _init_completion || return

    case $prev in
        wep-key-type)
            COMPREPLY=( $(compgen -W "key phrase" -- "$cur") )
            return
            ;;
        id)
            return
            ;;
    esac

    case ${words[2]} in
        nm|con|dev)
            COMPREPLY=( $(compgen -W "status" -- "$cur") )
            ;;
    esac
}
complete -F _nmcli nmcli
"#,
        )
        .unwrap();

        let mut trie = CommandTrie::new();
        scan_bash_dirs(&mut trie, &[dir.path().to_str().unwrap()]);

        // When the `case $prev in` block is skipped (because there's a
        // `case $words[N]` dispatch), nmcli may or may not be in the trie.
        // What matters is that `wep-key-type` is NOT a child of nmcli.
        if let Some(node) = trie.root.get_child("nmcli") {
            assert!(
                node.get_child("wep-key-type").is_none(),
                "wep-key-type should not be a subcommand"
            );
            assert!(node.get_child("id").is_none(), "id should not be a subcommand");
        }
        // If nmcli is not in the trie at all, wep-key-type definitely isn't either.
    }

    #[test]
    fn case_prev_flag_value_labels_filtered() {
        // When a flag arm (e.g. `--passthrough`) produces words like `ipv4 ipv6 eb`,
        // those words must NOT be treated as subcommands even if they also appear
        // as arm labels in the same `case $prev in` block.
        let dir = tempfile::tempdir().unwrap();
        let f = dir.path().join("firewall.bash");
        fs::write(
            &f,
            r#"_firewall() {
    local cur prev
    _init_completion || return

    case $prev in
        --passthrough|--get-chains|--get-rules)
            COMPREPLY=( $(compgen -W "ipv4 ipv6 eb" -- "$cur") )
            ;;
        ipv4|ipv6|eb)
            COMPREPLY=( $(compgen -W "nat filter mangle" -- "$cur") )
            ;;
    esac
}
complete -F _firewall firewall
"#,
        )
        .unwrap();

        let mut trie = CommandTrie::new();
        scan_bash_dirs(&mut trie, &[dir.path().to_str().unwrap()]);

        // ipv4, ipv6, eb are flag values → must NOT be subcommands.
        // If firewall is not in the trie at all, none of them are either.
        if let Some(node) = trie.root.get_child("firewall") {
            assert!(
                node.get_child("ipv4").is_none(),
                "ipv4 should not be a subcommand"
            );
            assert!(
                node.get_child("ipv6").is_none(),
                "ipv6 should not be a subcommand"
            );
            assert!(node.get_child("eb").is_none(), "eb should not be a subcommand");
        }
    }

    #[test]
    fn bash_scan_does_not_overwrite() {
        let dir = tempfile::tempdir().unwrap();
        let f = dir.path().join("git.bash");
        fs::write(&f, "complete -W \"bash-sub\" git\n").unwrap();

        let mut trie = CommandTrie::new();
        // Pre-populate git with a subcommand so it gets a node with count 1.
        trie.insert(&["git", "commit"]);
        let before_count = trie
            .root
            .get_child("git")
            .expect("git missing before scan")
            .children
            .len();

        scan_bash_dirs(&mut trie, &[dir.path().to_str().unwrap()]);

        let after_count = trie
            .root
            .get_child("git")
            .expect("git missing after scan")
            .children
            .len();

        // The scan adds bash-sub, so count grows. But `commit` must still be there.
        assert!(after_count >= before_count, "existing entries must be preserved");
        assert!(
            trie.root
                .get_child("git")
                .unwrap()
                .get_child("commit")
                .is_some(),
            "commit was overwritten"
        );
    }

    // --- unit tests for internal helpers ---

    #[test]
    fn strip_comment_basic() {
        assert_eq!(strip_comment("complete -W \"a b\" cmd # this is a comment"), "complete -W \"a b\" cmd ");
        assert_eq!(strip_comment("# full line comment"), "");
        assert_eq!(strip_comment("no comment here"), "no comment here");
    }

    #[test]
    fn strip_comment_hash_in_string() {
        // Hash inside a double-quoted string should NOT be stripped.
        assert_eq!(
            strip_comment(r#"echo "foo#bar""#),
            r#"echo "foo#bar""#
        );
    }

    #[test]
    fn extract_compgen_words_basic() {
        let var_map = HashMap::new();
        let body = r#"COMPREPLY=( $(compgen -W "alpha beta gamma" -- "$cur") )"#;
        let words = extract_compgen_words(body, &var_map);
        assert!(words.contains(&"alpha".to_string()), "{:?}", words);
        assert!(words.contains(&"beta".to_string()), "{:?}", words);
        assert!(words.contains(&"gamma".to_string()), "{:?}", words);
    }

    #[test]
    fn extract_compgen_words_var_ref() {
        let mut var_map = HashMap::new();
        var_map.insert("opts".to_string(), "x y z".to_string());
        let body = r#"COMPREPLY=( $(compgen -W "$opts" -- "$cur") )"#;
        let words = extract_compgen_words(body, &var_map);
        assert!(words.contains(&"x".to_string()), "{:?}", words);
        assert!(words.contains(&"z".to_string()), "{:?}", words);
    }

    #[test]
    fn looks_like_completion_word_rejects_shell_operators() {
        assert!(!looks_like_completion_word("2>&1"));
        assert!(!looks_like_completion_word(">file"));
        assert!(!looks_like_completion_word("foo|bar"));
        assert!(!looks_like_completion_word("foo;bar"));
        assert!(!looks_like_completion_word("${parts"));
        assert!(!looks_like_completion_word("parts}"));
        assert!(looks_like_completion_word("install"));
        assert!(looks_like_completion_word("--help"));
        assert!(looks_like_completion_word("git-lfs"));
    }

    #[test]
    fn extract_compgen_skips_subshell_value() {
        // systemd-nspawn assigns `comps=$(compgen -A file ...)` then calls
        // `compgen -W "$comps"`. The resolved value is shell source, not
        // completion words; it must not be split as words.
        let mut var_map = HashMap::new();
        var_map.insert(
            "comps".to_string(),
            "$( compgen -A file -- \"$cur\" )".to_string(),
        );
        let body = r#"COMPREPLY=( $(compgen -W "$comps" -- "$cur") )"#;
        let words = extract_compgen_words(body, &var_map);
        assert!(words.is_empty(), "{:?}", words);
    }

    #[test]
    fn extract_compgen_skips_inside_case() {
        // The fdisk bug: `case $prev in '-b') COMPREPLY=( $(compgen -W "512 1024 ...") ) ;; esac`
        // leaks "512" etc. as top-level subcommands. Those are per-flag values
        // and must NOT appear in the top-level compgen harvest.
        let var_map = HashMap::new();
        let body = r#"
_fdisk_module() {
    case $prev in
        '-b'|'--sector-size')
            COMPREPLY=( $(compgen -W "512 1024 2048 4096" -- $cur) )
            return 0
            ;;
        '-L'|'--color')
            COMPREPLY=( $(compgen -W "auto never always" -- $cur) )
            return 0
            ;;
    esac
    COMPREPLY=( $(compgen -W "alpha beta" -- $cur) )
}
"#;
        let words = extract_compgen_words(body, &var_map);
        // Only the post-case compgen should contribute.
        assert!(words.contains(&"alpha".to_string()), "{:?}", words);
        assert!(words.contains(&"beta".to_string()), "{:?}", words);
        // Per-flag enum values must not leak.
        assert!(!words.contains(&"512".to_string()), "{:?}", words);
        assert!(!words.contains(&"1024".to_string()), "{:?}", words);
        assert!(!words.contains(&"always".to_string()), "{:?}", words);
    }

    #[test]
    fn collect_local_vars_basic() {
        let body = r#"
    local opts="--foo --bar --baz"
    local commands='start stop'
"#;
        let vars = collect_local_vars(body);
        assert_eq!(vars.get("opts").map(String::as_str), Some("--foo --bar --baz"));
        assert_eq!(vars.get("commands").map(String::as_str), Some("start stop"));
    }

    #[test]
    fn is_case_prev_line_basic() {
        assert!(is_case_prev_line(r#"case "$prev" in"#));
        assert!(is_case_prev_line("case $prev in"));
        assert!(!is_case_prev_line("case $cur in"));
        assert!(!is_case_prev_line("echo hello"));
    }

    #[test]
    fn no_ext_file_is_scanned() {
        // Files with no extension (common in /usr/share/bash-completion/completions)
        // should also be processed.
        let dir = tempfile::tempdir().unwrap();
        // No extension.
        let f = dir.path().join("mynoext");
        fs::write(&f, "complete -W \"alpha beta\" mynoext\n").unwrap();

        let mut trie = CommandTrie::new();
        let (cmds, subs, _) = scan_bash_dirs(&mut trie, &[dir.path().to_str().unwrap()]);
        assert!(cmds >= 1, "no-ext file not scanned");
        assert!(subs >= 2, "words from no-ext file not inserted");
        assert!(
            trie.root.get_child("mynoext").unwrap().get_child("alpha").is_some(),
            "alpha missing"
        );
    }
}
