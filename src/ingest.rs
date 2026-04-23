use crate::trie::MatcherRule;
use std::collections::HashMap;
use std::io::Read;

/// Entry point for `zsh-ios ingest`.
///
/// Reads sectioned shell-state data from stdin and applies it to the stored
/// trie under an exclusive advisory lock.  The input format is:
///
///   @aliases\n
///   name='value'\n
///   ...
///   @functions\n
///   fn_name\n
///   ...
///   @nameddirs\n
///   name=/abs/path\n
///   ...
///
/// Unknown sections are silently skipped.  Missing config dir or a missing
/// trie file are silent no-ops (the trie will be built eventually by
/// `zsh-ios build`; ingesting before that makes no progress).
pub fn cmd_ingest() {
    let mut input = String::new();
    if std::io::stdin().read_to_string(&mut input).is_err() {
        return;
    }
    if input.trim().is_empty() {
        return;
    }
    if crate::config::ensure_config_dir().is_err() {
        return;
    }

    let tree_path = crate::config::tree_path();
    let _lock = crate::locks::lock_for(&tree_path);
    let mut trie = match crate::trie::CommandTrie::load(&tree_path) {
        Ok(t) => t,
        Err(_) => return,
    };

    apply_ingest(&mut trie, &input);
    let _ = trie.save(&tree_path);
}

/// Apply a sectioned ingest payload to an in-memory trie.
///
/// Exported so unit tests can exercise it without touching the filesystem.
pub fn apply_ingest(trie: &mut crate::trie::CommandTrie, input: &str) {
    let sections = split_sections(input);
    if let Some(body) = sections.get("aliases") {
        apply_aliases(trie, body);
    }
    if let Some(body) = sections.get("galiases") {
        apply_galiases(trie, body);
    }
    if let Some(body) = sections.get("saliases") {
        apply_aliases(trie, body);
    }
    if let Some(body) = sections.get("functions") {
        apply_functions(trie, body);
    }
    if let Some(body) = sections.get("nameddirs") {
        apply_nameddirs(trie, body);
    }
    if let Some(body) = sections.get("history") {
        apply_history(trie, body);
    }
    if let Some(body) = sections.get("dirstack") {
        apply_dirstack(trie, body);
    }
    for meta_kind in &["jobs", "commands", "parameters", "options", "widgets", "modules"] {
        if let Some(body) = sections.get(*meta_kind) {
            trie.live_state.insert((*meta_kind).to_string(), body.to_string());
        }
    }
    if let Some(body) = sections.get("zstyle") {
        let rules = parse_zstyle_matchers(body);
        if !rules.is_empty() {
            trie.matcher_rules = rules;
        }
    }
}

/// Split an ingest payload into named sections.
///
/// Lines that begin with `@` at column 0 are section headers.  The body of
/// each section is the text between that header and the next `@`-line (or
/// end-of-input).  Returns a map of section name -> body (including trailing
/// newline, if any).
pub fn split_sections(input: &str) -> HashMap<&str, &str> {
    let mut map = HashMap::new();
    let mut current_name: Option<&str> = None;
    let mut current_start: usize = 0;

    let mut pos = 0usize;
    for line in input.split_inclusive('\n') {
        let line_start = pos;
        pos += line.len();
        let trimmed = line.trim_end_matches('\n').trim_end_matches('\r');
        if let Some(name) = trimmed.strip_prefix('@') {
            // Close the previous section.
            if let Some(prev_name) = current_name {
                map.insert(prev_name, &input[current_start..line_start]);
            }
            current_name = Some(name);
            current_start = pos;
        }
    }
    // Close the final section.
    if let Some(name) = current_name {
        map.insert(name, &input[current_start..]);
    }
    map
}

/// Apply alias lines (output of `alias` or `alias -g` / `alias -s`) to the
/// trie.  Each line has the form `name='value'` or `name=value`.
///
/// Mirrors `scanner::parse_aliases` but operates on an `&str` body directly
/// rather than an `io::BufRead`, avoiding the need to factor a shared helper
/// across two call sites with different ownership patterns.
pub fn apply_aliases(trie: &mut crate::trie::CommandTrie, body: &str) {
    use crate::history::split_command_segments;

    for line in body.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        if let Some((name, value)) = line.split_once('=') {
            let name = name.trim();
            if !name.is_empty() && !name.contains(' ') {
                trie.insert_command(name);
                let value = value.trim().trim_matches('\'').trim_matches('"');
                for segment in split_command_segments(value) {
                    let words: Vec<&str> = segment.split_whitespace().collect();
                    if words.len() >= 2 {
                        trie.insert(&words);
                    }
                }
            }
        }
    }
}

/// Apply global-alias lines (`alias -g` output) to `trie.galiases`.
///
/// Each line has the form `name='value'` or `name=value`. The alias name is
/// stored as-is; the value has surrounding single or double quotes stripped.
/// Entries whose name is empty or contains whitespace are skipped.
pub fn apply_galiases(trie: &mut crate::trie::CommandTrie, body: &str) {
    for line in body.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        if let Some((name, value)) = line.split_once('=') {
            let name = name.trim();
            if name.is_empty() || name.contains(char::is_whitespace) {
                continue;
            }
            let value = value.trim().trim_matches('\'').trim_matches('"');
            trie.galiases.insert(name.to_string(), value.to_string());
        }
    }
}

/// Apply a list of shell function names (one per line) to the trie.
///
/// Underscore-prefixed names (internal helpers), whitespace-containing names
/// (invalid identifiers), and names the trie already knows are all skipped.
pub fn apply_functions(trie: &mut crate::trie::CommandTrie, body: &str) {
    for line in body.lines() {
        let name = line.trim();
        if name.is_empty() || name.starts_with('_') || name.contains(char::is_whitespace) {
            continue;
        }
        if trie.root.get_child(name).is_some() {
            continue;
        }
        trie.insert_command(name);
    }
}

/// Apply live history lines (one command per line) to the trie.
///
/// Each non-empty line is treated as a historical command. Lines are validated
/// with the same rules as `history::parse_history`: skip control-flow keywords,
/// subshell artifacts, env-var prefixes, and commands not already known to the
/// trie. Uses `insert_with_time(…, 0)` — live `$history` doesn't expose timestamps.
pub fn apply_history(trie: &mut crate::trie::CommandTrie, body: &str) {
    use crate::history::split_command_segments;

    for line in body.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        for segment in split_command_segments(line) {
            let words: Vec<&str> = segment.split_whitespace().collect();
            if words.is_empty() {
                continue;
            }
            // Skip env-var prefixed invocations (FOO=bar cmd …)
            let (words, start) = if words[0].contains('=') && !words[0].starts_with('-') {
                if words.len() > 1 { (&words[1..], 1) } else { continue; }
            } else {
                (&words[..], 0)
            };
            let _ = start;
            if words.is_empty() {
                continue;
            }
            let cmd = words[0];
            // Skip subshell artifacts
            if cmd.starts_with("$(") || cmd.starts_with('`') {
                continue;
            }
            // Skip shell control-flow keywords
            if matches!(
                cmd,
                "if" | "then" | "else" | "elif" | "fi" | "while" | "do" | "done" | "for" | "in"
                    | "case" | "esac" | "{" | "}" | "[[" | "((" | "function"
            ) {
                continue;
            }
            // Only insert commands the trie already knows
            if trie.root.get_child(cmd).is_none() {
                continue;
            }
            // Don't insert if this word is a strict prefix of an existing entry
            if trie.root.is_prefix_of_existing(cmd) {
                continue;
            }
            trie.root.insert_with_time(words, 0);
        }
    }
}

/// Apply directory-stack lines (one absolute path per line) to `trie.dir_stack`.
///
/// The first line is PWD (index 0); subsequent lines are pushed directories.
/// Trailing slashes are stripped. Consecutive duplicate entries are de-duplicated.
pub fn apply_dirstack(trie: &mut crate::trie::CommandTrie, body: &str) {
    let mut stack: Vec<String> = Vec::new();
    for line in body.lines() {
        let path = line.trim().trim_end_matches('/');
        if path.is_empty() {
            continue;
        }
        // De-duplicate consecutive identical entries
        if stack.last().is_some_and(|last| last == path) {
            continue;
        }
        stack.push(path.to_string());
    }
    trie.dir_stack = stack;
}

/// Parse `zstyle -L` output and extract matcher-list rules.
///
/// Only lines of the form `zstyle '<context>' matcher-list <args>` are
/// examined; all other `zstyle` lines are ignored.  Each quoted argument
/// after `matcher-list` is a single matcher string; within it, individual
/// specs are whitespace-separated.
pub fn parse_zstyle_matchers(body: &str) -> Vec<MatcherRule> {
    let mut rules = Vec::new();
    for line in body.lines() {
        let t = line.trim();
        let Some(rest) = t.strip_prefix("zstyle ") else {
            continue;
        };
        // Skip the context argument (first quoted token).
        let Some(after_context) = skip_quoted(rest) else {
            continue;
        };
        let after_context = after_context.trim_start();
        let Some(after_key) = after_context.strip_prefix("matcher-list") else {
            continue;
        };
        // Each remaining quoted token is one matcher string.
        for matcher in split_quoted_args(after_key) {
            for spec in matcher.split_whitespace() {
                rules.push(classify_matcher_spec(spec));
            }
        }
    }
    rules
}

/// Classify one zstyle match spec into a `MatcherRule`.
fn classify_matcher_spec(spec: &str) -> MatcherRule {
    // m:{...}={...} forms that reference a-z or A-Z → case-insensitive.
    if spec.starts_with("m:{")
        && (spec.contains("a-z") || spec.contains("A-Z"))
        && spec.contains('=')
    {
        return MatcherRule::CaseInsensitive;
    }
    // r:|[CHARSET]=* or r:|=* — partial / any-position matching.
    if spec.starts_with("r:|") {
        let charset = extract_charset(spec);
        return MatcherRule::PartialOn(charset.unwrap_or_default());
    }
    MatcherRule::Unknown(spec.to_string())
}

/// Advance past the first shell-quoted token in `s` and return the remainder.
///
/// Handles single-quoted (`'...'`) and double-quoted (`"..."`) tokens as well
/// as bare (unquoted) tokens terminated by whitespace.  Returns `None` when
/// `s` is empty after trimming leading whitespace.
fn skip_quoted(s: &str) -> Option<&str> {
    let s = s.trim_start();
    if s.is_empty() {
        return None;
    }
    let bytes = s.as_bytes();
    let quote = bytes[0];
    if quote == b'\'' || quote == b'"' {
        // Find the matching closing quote.
        let mut i = 1;
        while i < bytes.len() && bytes[i] != quote {
            i += 1;
        }
        // i now points at the closing quote (or end-of-string).
        Some(&s[i.saturating_add(1)..])
    } else {
        // Bare token — advance until whitespace.
        let end = s.find(char::is_whitespace).unwrap_or(s.len());
        Some(&s[end..])
    }
}

/// Split the remainder of a `zstyle ... matcher-list` line into individual
/// quoted argument strings, stripping the outer quotes.
///
/// Handles `'...'` and `"..."` quoting only; unquoted tokens are returned
/// verbatim.
fn split_quoted_args(s: &str) -> Vec<&str> {
    let mut args = Vec::new();
    let mut rest = s.trim_start();
    while !rest.is_empty() {
        let bytes = rest.as_bytes();
        let quote = bytes[0];
        if quote == b'\'' || quote == b'"' {
            // Find closing quote.
            let mut i = 1;
            while i < bytes.len() && bytes[i] != quote {
                i += 1;
            }
            // Content between quotes.
            args.push(&rest[1..i]);
            rest = rest[i.saturating_add(1)..].trim_start();
        } else {
            // Bare token.
            let end = rest.find(char::is_whitespace).unwrap_or(rest.len());
            args.push(&rest[..end]);
            rest = rest[end..].trim_start();
        }
    }
    args
}

/// Extract the character-set string from `r:|[CHARSET]=*`.
///
/// Returns `Some("._-")` for `r:|[._-]=*`, `None` for bare `r:|=*`.
fn extract_charset(spec: &str) -> Option<String> {
    // Expect the form r:|[...]=*
    let inner = spec.strip_prefix("r:|[")?;
    let end = inner.find(']')?;
    Some(inner[..end].to_string())
}

/// Apply named-directory lines (`name=/abs/path`) to `trie.named_dirs`.
///
/// Lines that contain no `=` or whose left-hand side is empty are skipped.
pub fn apply_nameddirs(trie: &mut crate::trie::CommandTrie, body: &str) {
    for line in body.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        if let Some((name, path)) = line.split_once('=') {
            let name = name.trim();
            let path = path.trim();
            if !name.is_empty() && !path.is_empty() {
                trie.named_dirs.insert(name.to_string(), path.to_string());
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::trie::CommandTrie;

    // ── split_sections ────────────────────────────────────────────────────────

    #[test]
    fn split_sections_single_section() {
        let input = "@aliases\nll='ls -la'\n";
        let sections = split_sections(input);
        assert_eq!(sections.get("aliases"), Some(&"ll='ls -la'\n"));
    }

    #[test]
    fn split_sections_multiple() {
        let input = "@aliases\nll='ls -la'\n@functions\nmyfn\n@nameddirs\nproj=/home/me/proj\n";
        let sections = split_sections(input);
        assert_eq!(sections.get("aliases"), Some(&"ll='ls -la'\n"));
        assert_eq!(sections.get("functions"), Some(&"myfn\n"));
        assert_eq!(sections.get("nameddirs"), Some(&"proj=/home/me/proj\n"));
    }

    #[test]
    fn split_sections_ignores_unknown() {
        let input = "@zstyle\nfoo\n";
        let sections = split_sections(input);
        assert_eq!(sections.get("zstyle"), Some(&"foo\n"));
        // apply_ingest should not crash on unknown sections.
        let mut trie = CommandTrie::new();
        apply_ingest(&mut trie, input);
        // "foo" is not a valid named dir or function (no = sign for nameddirs),
        // but the unknown "zstyle" section is silently skipped.
        assert!(trie.named_dirs.is_empty());
    }

    // ── apply_aliases ─────────────────────────────────────────────────────────

    #[test]
    fn apply_aliases_inserts_names() {
        let mut trie = CommandTrie::new();
        apply_aliases(&mut trie, "ll='ls -la'\ngs='git status'\n");
        assert!(trie.root.get_child("ll").is_some(), "alias name 'll' not inserted");
        assert!(trie.root.get_child("gs").is_some(), "alias name 'gs' not inserted");
        // Alias expansion should also teach the underlying command sequence.
        let ls = trie.root.get_child("ls").expect("'ls' from ll alias expansion");
        assert!(ls.get_child("-la").is_some(), "'ls -la' path missing");
        let git = trie.root.get_child("git").expect("'git' from gs alias expansion");
        assert!(git.get_child("status").is_some(), "'git status' path missing");
    }

    // ── apply_functions ───────────────────────────────────────────────────────

    #[test]
    fn apply_functions_inserts_non_underscore_names() {
        let mut trie = CommandTrie::new();
        apply_functions(&mut trie, "myfn\n_internal\n");
        assert!(trie.root.get_child("myfn").is_some(), "'myfn' should be inserted");
        assert!(trie.root.get_child("_internal").is_none(), "'_internal' should be skipped");
    }

    #[test]
    fn apply_functions_preserves_existing() {
        let mut trie = CommandTrie::new();
        trie.insert(&["git", "status"]);
        // Give 'git' a nonzero count so we can verify it isn't regressed.
        let count_before = trie.root.get_child("git").unwrap().count;
        apply_functions(&mut trie, "git\n");
        let count_after = trie.root.get_child("git").unwrap().count;
        // The existing entry should be left alone (skip path).
        assert_eq!(count_before, count_after, "existing entry count regressed");
    }

    // ── apply_nameddirs ───────────────────────────────────────────────────────

    #[test]
    fn apply_nameddirs_populates_field() {
        let mut trie = CommandTrie::new();
        apply_nameddirs(&mut trie, "proj=/home/me/proj\npkgs=/usr/local\n");
        assert_eq!(
            trie.named_dirs.get("proj"),
            Some(&"/home/me/proj".to_string())
        );
        assert_eq!(
            trie.named_dirs.get("pkgs"),
            Some(&"/usr/local".to_string())
        );
    }

    #[test]
    fn apply_nameddirs_ignores_malformed() {
        let mut trie = CommandTrie::new();
        apply_nameddirs(&mut trie, "bad-line-no-equals\nok=/tmp\n");
        assert!(!trie.named_dirs.contains_key("bad-line-no-equals"));
        assert_eq!(trie.named_dirs.get("ok"), Some(&"/tmp".to_string()));
    }

    // ── apply_history ─────────────────────────────────────────────────────────

    #[test]
    fn apply_history_inserts_known_commands() {
        let mut trie = CommandTrie::new();
        // Pre-insert known commands so history validation passes
        trie.insert_command("git");
        trie.insert_command("ls");
        // Store counts before so we can verify they don't regress
        let git_count_before = trie.root.get_child("git").unwrap().count;
        let ls_count_before = trie.root.get_child("ls").unwrap().count;

        apply_history(&mut trie, "git status\nls -la\nunknowncmd foo\n");

        // 'git' and 'ls' must still be present (counts may go up via sub-path inserts)
        assert!(trie.root.get_child("git").is_some());
        assert!(trie.root.get_child("ls").is_some());
        // 'unknowncmd' must NOT be inserted
        assert!(trie.root.get_child("unknowncmd").is_none());
        // sub-paths should have been inserted for the known commands
        let git_count_after = trie.root.get_child("git").unwrap().count;
        let ls_count_after = trie.root.get_child("ls").unwrap().count;
        // counts should not regress
        assert!(git_count_after >= git_count_before);
        assert!(ls_count_after >= ls_count_before);
    }

    // ── apply_dirstack ────────────────────────────────────────────────────────

    #[test]
    fn apply_dirstack_populates_field() {
        let mut trie = CommandTrie::new();
        apply_dirstack(&mut trie, "/home/me\n/tmp\n");
        assert_eq!(trie.dir_stack, vec!["/home/me", "/tmp"]);
    }

    #[test]
    fn apply_dirstack_dedupes_consecutive() {
        let mut trie = CommandTrie::new();
        apply_dirstack(&mut trie, "/tmp\n/tmp\n/var\n");
        assert_eq!(trie.dir_stack, vec!["/tmp", "/var"]);
    }

    #[test]
    fn apply_dirstack_strips_trailing_slash() {
        let mut trie = CommandTrie::new();
        apply_dirstack(&mut trie, "/home/me/\n/tmp/\n");
        assert_eq!(trie.dir_stack, vec!["/home/me", "/tmp"]);
    }

    // ── apply_live_state ──────────────────────────────────────────────────────

    #[test]
    fn apply_live_state_stores_metadata() {
        let mut trie = CommandTrie::new();
        let input = "@jobs\n[1]+ Running some_cmd\n@options\nauto_cd\n";
        apply_ingest(&mut trie, input);
        assert!(
            trie.live_state.contains_key("jobs"),
            "live_state missing 'jobs'"
        );
        assert!(
            trie.live_state.contains_key("options"),
            "live_state missing 'options'"
        );
        assert!(trie.live_state["jobs"].contains("Running"));
        assert!(trie.live_state["options"].contains("auto_cd"));
    }

    // ── end_to_end_apply_ingest ───────────────────────────────────────────────

    #[test]
    fn end_to_end_apply_ingest() {
        let mut trie = CommandTrie::new();
        let input = concat!(
            "@aliases\n",
            "ll='ls -la'\n",
            "gs='git status'\n",
            "@functions\n",
            "myfn\n",
            "_internal\n",
            "@nameddirs\n",
            "proj=/home/me/proj\n",
        );
        apply_ingest(&mut trie, input);
        // aliases section
        assert!(trie.root.get_child("ll").is_some());
        assert!(trie.root.get_child("gs").is_some());
        // function section
        assert!(trie.root.get_child("myfn").is_some());
        assert!(trie.root.get_child("_internal").is_none());
        // nameddirs section
        assert_eq!(
            trie.named_dirs.get("proj"),
            Some(&"/home/me/proj".to_string())
        );
    }

    // ── apply_galiases ────────────────────────────────────────────────────────

    #[test]
    fn apply_galiases_populates_trie_field() {
        let mut trie = CommandTrie::new();
        apply_galiases(&mut trie, "G='| grep'\nL='| less'\n");
        assert_eq!(trie.galiases.get("G"), Some(&"| grep".to_string()));
        assert_eq!(trie.galiases.get("L"), Some(&"| less".to_string()));
    }

    #[test]
    fn apply_galiases_strips_double_quotes() {
        let mut trie = CommandTrie::new();
        apply_galiases(&mut trie, "NE=\"2>/dev/null\"\n");
        assert_eq!(trie.galiases.get("NE"), Some(&"2>/dev/null".to_string()));
    }

    #[test]
    fn apply_galiases_ignores_malformed() {
        let mut trie = CommandTrie::new();
        // Line with no `=` sign.
        apply_galiases(&mut trie, "name without equals\nG='| grep'\n");
        assert!(!trie.galiases.contains_key("name without equals"));
        assert_eq!(trie.galiases.get("G"), Some(&"| grep".to_string()));
    }

    #[test]
    fn apply_galiases_skips_names_with_whitespace() {
        let mut trie = CommandTrie::new();
        apply_galiases(&mut trie, "bad name='value'\n");
        assert!(trie.galiases.is_empty());
    }

    #[test]
    fn apply_galiases_does_not_insert_into_trie_root() {
        // Unlike apply_aliases, apply_galiases must NOT touch trie.root.
        let mut trie = CommandTrie::new();
        apply_galiases(&mut trie, "G='| grep'\n");
        assert!(trie.root.get_child("G").is_none());
    }

    #[test]
    fn apply_ingest_routes_galiases_section() {
        let mut trie = CommandTrie::new();
        let input = "@galiases\nG='| grep'\n";
        apply_ingest(&mut trie, input);
        assert_eq!(trie.galiases.get("G"), Some(&"| grep".to_string()));
        // Must NOT have been inserted into the trie root as a command.
        assert!(trie.root.get_child("G").is_none());
    }

    // ── parse_zstyle_matchers ─────────────────────────────────────────────────

    #[test]
    fn parse_zstyle_matchers_case_insensitive() {
        use crate::trie::MatcherRule;
        let body = "zstyle ':completion:*' matcher-list 'm:{a-z}={A-Z}'\n";
        let rules = parse_zstyle_matchers(body);
        assert_eq!(rules.len(), 1);
        assert_eq!(rules[0], MatcherRule::CaseInsensitive);
    }

    #[test]
    fn parse_zstyle_matchers_partial() {
        use crate::trie::MatcherRule;
        let body = "zstyle ':completion:*' matcher-list 'r:|[._-]=* r:|=*'\n";
        let rules = parse_zstyle_matchers(body);
        assert_eq!(rules.len(), 2, "expected 2 rules, got {:?}", rules);
        assert_eq!(rules[0], MatcherRule::PartialOn("._-".to_string()));
        assert_eq!(rules[1], MatcherRule::PartialOn(String::new()));
    }

    #[test]
    fn parse_zstyle_matchers_ignores_unrelated_zstyle() {
        let body = "zstyle ':completion:*' completer _complete _approximate _ignored\n";
        let rules = parse_zstyle_matchers(body);
        assert!(rules.is_empty(), "expected no rules, got {:?}", rules);
    }

    #[test]
    fn parse_zstyle_matchers_unknown_recorded() {
        use crate::trie::MatcherRule;
        let body = "zstyle ':completion:*' matcher-list 'b:=*'\n";
        let rules = parse_zstyle_matchers(body);
        assert_eq!(rules.len(), 1);
        assert_eq!(rules[0], MatcherRule::Unknown("b:=*".to_string()));
    }

    #[test]
    fn apply_ingest_populates_matcher_rules() {
        use crate::trie::MatcherRule;
        let mut trie = CommandTrie::new();
        let input = concat!(
            "@zstyle\n",
            "zstyle ':completion:*' matcher-list 'm:{a-z}={A-Z}' 'r:|[._-]=* r:|=*'\n",
        );
        apply_ingest(&mut trie, input);
        // Three rules: CaseInsensitive, PartialOn("._-"), PartialOn("").
        assert_eq!(
            trie.matcher_rules.len(),
            3,
            "expected 3 matcher rules, got {:?}",
            trie.matcher_rules
        );
        assert_eq!(trie.matcher_rules[0], MatcherRule::CaseInsensitive);
        assert_eq!(trie.matcher_rules[1], MatcherRule::PartialOn("._-".to_string()));
        assert_eq!(trie.matcher_rules[2], MatcherRule::PartialOn(String::new()));
    }
}
