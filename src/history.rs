use std::fs;
use std::path::Path;

use crate::trie::CommandTrie;

/// Parse a Zsh history file and insert command sequences into the trie.
///
/// Handles two formats:
/// - Plain text: one command per line
/// - Extended history: `: timestamp:duration;command`
///
/// Multi-line commands (continuation with `\`) are joined.
pub fn parse_history(
    path: &Path,
    trie: &mut CommandTrie,
) -> Result<u32, Box<dyn std::error::Error>> {
    let content = fs::read_to_string(path).or_else(|_| {
        // Try reading as bytes and lossy-converting (history can contain non-UTF8)
        let bytes = fs::read(path)?;
        Ok::<String, std::io::Error>(String::from_utf8_lossy(&bytes).into_owned())
    })?;

    let mut count = 0u32;
    let mut continuation = String::new();

    for line in content.lines() {
        // Handle line continuations (trailing backslash)
        if let Some(stripped) = line.strip_suffix('\\') {
            continuation.push_str(stripped);
            continuation.push(' ');
            continue;
        }

        let full_line = if continuation.is_empty() {
            line.to_string()
        } else {
            continuation.push_str(line);
            let result = continuation.clone();
            continuation.clear();
            result
        };

        let (command, ts) = parse_history_line(&full_line);
        if command.is_empty() {
            continue;
        }

        // Split on pipes and semicolons to get individual commands
        for segment in split_command_segments(command) {
            let words: Vec<&str> = segment.split_whitespace().collect();
            if words.is_empty() {
                continue;
            }

            // Skip if the first word looks like an env var assignment (FOO=bar)
            if words[0].contains('=') && !words[0].starts_with('-') {
                if words.len() > 1 {
                    let cmd = words[1];
                    if !should_skip_command(cmd, trie) {
                        let clean = truncate_at_implausible(&words[1..]);
                        if !clean.is_empty() {
                            trie.root.insert_with_time(&clean, ts);
                            count += 1;
                        }
                    }
                }
                continue;
            }

            // Skip subshell / command-substitution artifacts like $(...) or `...`
            if words[0].starts_with("$(") || words[0].starts_with('`') {
                continue;
            }

            // Skip lines starting with shell control flow keywords
            if matches!(
                words[0],
                "if" | "then" | "else" | "elif" | "fi" | "while" | "do" | "done" | "for" | "in"
                    | "case" | "esac" | "{" | "}" | "[[" | "((" | "function"
            ) {
                continue;
            }

            if should_skip_command(words[0], trie) {
                continue;
            }

            let clean = truncate_at_implausible(&words);
            if clean.is_empty() {
                continue;
            }
            trie.root.insert_with_time(&clean, ts);
            count += 1;
        }
    }

    Ok(count)
}

/// Truncate a word list at the first word that looks like shell-syntax junk
/// rather than a real subcommand/arg. History-sourced words are inserted as
/// trie children verbatim, so things like `'export`, `$TOKEN`, backticks, and
/// `#`-prefixed fragments would otherwise surface as "subcommands" later.
///
/// We truncate (rather than skip-and-continue) because anything after an
/// implausible word is syntactically disconnected from the command root —
/// learning `git 'export foo` as `git → 'export → foo` produces two pieces of
/// junk, not one.
fn truncate_at_implausible<'a>(words: &[&'a str]) -> Vec<&'a str> {
    words
        .iter()
        .take_while(|w| is_plausible_word(w))
        .copied()
        .collect()
}

/// Reject words that carry shell-syntax characters (quotes, `$`, backticks,
/// `#`, `*`, redirections, braces, parentheses). These never appear in
/// legitimate subcommand / arg names and always mean the history segment is
/// shell source rather than a simple command invocation.
fn is_plausible_word(w: &str) -> bool {
    if w.is_empty() {
        return false;
    }
    if w.chars().any(|c| {
        matches!(
            c,
            '"' | '\''
                | '`'
                | '$'
                | '#'
                | '*'
                | '<'
                | '>'
                | '|'
                | ';'
                | '&'
                | '{'
                | '}'
                | '('
                | ')'
                | '\\'
        )
    }) {
        return false;
    }
    true
}

/// Check whether a command word from history should be skipped.
///
/// Returns true if:
/// - It's a strict prefix of an existing trie entry (abbreviated junk like "terr")
/// - It doesn't exist as a known command in the trie (not on PATH, not a builtin,
///   not an alias). This prevents learning garbage like typos or one-off scripts
///   that no longer exist.
fn should_skip_command(cmd: &str, trie: &CommandTrie) -> bool {
    if trie.root.is_prefix_of_existing(cmd) {
        return true;
    }
    // If the trie already has this command (from PATH scan, builtins, or aliases),
    // it's real. If not, it's probably a typo or gone executable — skip it.
    trie.root.get_child(cmd).is_none()
}

/// Strip the Zsh extended history prefix and return `(command, timestamp)`.
/// For plain lines without a prefix, returns `(line.trim(), 0)`.
/// Malformed extended entries fall back to the whole line + 0.
fn parse_history_line(line: &str) -> (&str, u64) {
    if line.starts_with(": ")
        && let Some(semi) = line.find(';')
    {
        let cmd = line[semi + 1..].trim();
        // Between `: ` and `;`, layout is `ts:duration`.
        let meta = &line[2..semi];
        let ts = meta
            .split(':')
            .next()
            .and_then(|s| s.trim().parse::<u64>().ok())
            .unwrap_or(0);
        return (cmd, ts);
    }
    (line.trim(), 0)
}

/// Split a command line on unquoted pipes and semicolons to extract
/// individual command segments.
pub fn split_command_segments(line: &str) -> Vec<&str> {
    let mut segments = Vec::new();
    let mut start = 0;
    let mut in_single_quote = false;
    let mut in_double_quote = false;
    // Track [[ ... ]] depth so that `||` / `&&` inside Zsh conditionals
    // (e.g. `[[ $x == a || $x == b ]]`) are not treated as segment separators.
    let mut bracket_depth: u32 = 0;
    let bytes = line.as_bytes();
    let mut i = 0;

    while i < bytes.len() {
        let b = bytes[i];
        match b {
            b'\'' if !in_double_quote => in_single_quote = !in_single_quote,
            b'"' if !in_single_quote => in_double_quote = !in_double_quote,
            b'\\' if !in_single_quote => {
                i += 1; // skip escaped char
            }
            b'[' if !in_single_quote && !in_double_quote => {
                if i + 1 < bytes.len() && bytes[i + 1] == b'[' {
                    bracket_depth += 1;
                    i += 1; // consume second [
                }
            }
            b']' if !in_single_quote && !in_double_quote && bracket_depth > 0 => {
                if i + 1 < bytes.len() && bytes[i + 1] == b']' {
                    bracket_depth = bracket_depth.saturating_sub(1);
                    i += 1; // consume second ]
                }
            }
            b'|' | b';' if !in_single_quote && !in_double_quote && bracket_depth == 0 => {
                // Also handle || and && as segment separators
                let seg = line[start..i].trim();
                if !seg.is_empty() {
                    segments.push(seg);
                }
                // Skip over || or &&
                if i + 1 < bytes.len() && (bytes[i + 1] == b'|' || bytes[i + 1] == b'&') {
                    i += 1;
                }
                start = i + 1;
            }
            b'&' if !in_single_quote && !in_double_quote && bracket_depth == 0 => {
                if i + 1 < bytes.len() && bytes[i + 1] == b'&' {
                    let seg = line[start..i].trim();
                    if !seg.is_empty() {
                        segments.push(seg);
                    }
                    i += 1;
                    start = i + 1;
                } else {
                    // Single & (background) — split here too
                    let seg = line[start..i].trim();
                    if !seg.is_empty() {
                        segments.push(seg);
                    }
                    start = i + 1;
                }
            }
            _ => {}
        }
        i += 1;
    }

    let seg = line[start..].trim();
    if !seg.is_empty() {
        segments.push(seg);
    }

    segments
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_history_line_extended_format() {
        assert_eq!(
            parse_history_line(": 1700000000:5;git status"),
            ("git status", 1700000000)
        );
    }

    #[test]
    fn parse_history_line_plain() {
        assert_eq!(parse_history_line("git status"), ("git status", 0));
    }

    #[test]
    fn parse_history_line_malformed_extended() {
        assert_eq!(parse_history_line(": not-a-number:x;foo"), ("foo", 0));
    }

    #[test]
    fn parse_history_line_missing_semicolon() {
        // No `;` found — falls through to the plain branch, which returns line.trim().
        // The `: ` prefix is preserved because we fall through without stripping it.
        assert_eq!(
            parse_history_line(": 123 no-semi"),
            (": 123 no-semi", 0)
        );
    }

    #[test]
    fn test_split_segments() {
        let segs = split_command_segments("echo hello | grep h");
        assert_eq!(segs, vec!["echo hello", "grep h"]);

        let segs = split_command_segments("cd foo && ls -la");
        assert_eq!(segs, vec!["cd foo", "ls -la"]);

        let segs = split_command_segments("echo 'hello | world'");
        assert_eq!(segs, vec!["echo 'hello | world'"]);

        let segs = split_command_segments("git status; git diff");
        assert_eq!(segs, vec!["git status", "git diff"]);

        let segs = split_command_segments("sleep 5 & echo done");
        assert_eq!(segs, vec!["sleep 5", "echo done"]);
    }

    #[test]
    fn test_parse_history_plain() {
        let dir = std::env::temp_dir().join("zsh-ios-test-history");
        let _ = std::fs::create_dir_all(&dir);
        let path = dir.join("history");
        std::fs::write(
            &path,
            "git checkout main\nterraform apply\necho hello | grep h\n",
        )
        .unwrap();

        let mut trie = CommandTrie::new();
        // Pre-populate with known commands (mirrors real build where PATH is scanned first)
        trie.insert_command("git");
        trie.insert_command("terraform");
        trie.insert_command("echo");
        trie.insert_command("grep");

        let count = parse_history(&path, &mut trie).unwrap();

        assert_eq!(count, 4); // git checkout main, terraform apply, echo hello, grep h
        assert!(trie.root.get_child("git").is_some());
        assert!(trie.root.get_child("terraform").is_some());
        assert!(trie.root.get_child("grep").is_some());

        let git = trie.root.get_child("git").unwrap();
        assert!(git.get_child("checkout").is_some());

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_unknown_command_not_learned() {
        let dir = std::env::temp_dir().join("zsh-ios-test-unknown-skip");
        let _ = std::fs::create_dir_all(&dir);
        let path = dir.join("history");
        std::fs::write(&path, "xyzzy foo bar\ngit status\n").unwrap();

        let mut trie = CommandTrie::new();
        trie.insert_command("git");

        let count = parse_history(&path, &mut trie).unwrap();
        assert_eq!(count, 1); // only "git status", not "xyzzy foo bar"
        assert!(
            trie.root.get_child("xyzzy").is_none(),
            "unknown command should not be learned from history"
        );
        assert!(
            trie.root
                .get_child("git")
                .unwrap()
                .get_child("status")
                .is_some()
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_subshell_not_learned() {
        let dir = std::env::temp_dir().join("zsh-ios-test-subshell");
        let _ = std::fs::create_dir_all(&dir);
        let path = dir.join("history");
        std::fs::write(&path, "$(git rev-parse HEAD)\ngit status\n").unwrap();

        let mut trie = CommandTrie::new();
        trie.insert_command("git");

        let count = parse_history(&path, &mut trie).unwrap();
        assert_eq!(count, 1); // only "git status"
        assert!(
            trie.root.get_child("$(git").is_none(),
            "subshell artifact should not be learned"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_control_flow_not_learned() {
        let dir = std::env::temp_dir().join("zsh-ios-test-control");
        let _ = std::fs::create_dir_all(&dir);
        let path = dir.join("history");
        std::fs::write(&path, "if true; then echo hello; fi\n").unwrap();

        let mut trie = CommandTrie::new();
        trie.insert_command("echo");

        let _count = parse_history(&path, &mut trie).unwrap();
        // All segments start with control flow keywords (if, then, fi),
        // so none are learned as commands.
        assert!(
            trie.root.get_child("if").is_none(),
            "control flow keyword 'if' should not be learned"
        );
        assert!(
            trie.root.get_child("then").is_none(),
            "control flow keyword 'then' should not be learned"
        );
        assert!(
            trie.root.get_child("fi").is_none(),
            "control flow keyword 'fi' should not be learned"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_prefix_not_learned() {
        // If "terraform" is already in the trie, "terr apply" should NOT
        // insert "terr" as a separate top-level command.
        let dir = std::env::temp_dir().join("zsh-ios-test-prefix-skip");
        let _ = std::fs::create_dir_all(&dir);
        let path = dir.join("history");
        std::fs::write(&path, "terr apply\n").unwrap();

        let mut trie = CommandTrie::new();
        // Pre-populate with the real command
        trie.insert(&["terraform", "apply"]);

        let count = parse_history(&path, &mut trie).unwrap();
        assert_eq!(count, 0); // "terr" is a prefix of "terraform", so skipped
        assert!(
            trie.root.get_child("terr").is_none(),
            "abbreviated prefix 'terr' should not be learned"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn parse_history_writes_last_used() {
        let dir = std::env::temp_dir().join("zsh-ios-test-last-used");
        let _ = std::fs::create_dir_all(&dir);
        let path = dir.join("history");
        std::fs::write(
            &path,
            ": 1700000000:0;git status\n: 1700000100:0;ls -la\n",
        )
        .unwrap();

        let mut trie = CommandTrie::new();
        trie.insert_command("git");
        trie.insert_command("ls");

        parse_history(&path, &mut trie).unwrap();

        let git = trie.root.get_child("git").unwrap();
        assert_eq!(git.last_used, 1700000000);

        let ls = trie.root.get_child("ls").unwrap();
        assert_eq!(ls.last_used, 1700000100);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn parse_history_plain_leaves_last_used_zero() {
        let dir = std::env::temp_dir().join("zsh-ios-test-plain-ts-zero");
        let _ = std::fs::create_dir_all(&dir);
        let path = dir.join("history");
        std::fs::write(&path, "foo bar\n").unwrap();

        let mut trie = CommandTrie::new();
        trie.insert_command("foo");

        parse_history(&path, &mut trie).unwrap();

        let foo = trie.root.get_child("foo").unwrap();
        assert_eq!(foo.last_used, 0);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn parse_history_extended_keeps_max_ts() {
        let dir = std::env::temp_dir().join("zsh-ios-test-max-ts");
        let _ = std::fs::create_dir_all(&dir);
        let path = dir.join("history");
        // Newer timestamp first, older second — max must win.
        std::fs::write(
            &path,
            ": 1700000200:0;git status\n: 1700000100:0;git status\n",
        )
        .unwrap();

        let mut trie = CommandTrie::new();
        trie.insert_command("git");

        parse_history(&path, &mut trie).unwrap();

        let git = trie.root.get_child("git").unwrap();
        assert_eq!(git.last_used, 1700000200);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn truncate_at_implausible_drops_shell_junk() {
        // `echo 'export FOO=bar'` splits into `echo`, `'export`, `FOO=bar`.
        // We should keep `echo` and stop — `'export` is shell-quoted junk and
        // inserting `echo -> 'export -> FOO=bar` would surface both as
        // apparent subcommands. Quotes, `$`, backticks, `#`, `*`, and shell
        // operators all truncate.
        assert_eq!(
            truncate_at_implausible(&["echo", "'export", "FOO=bar"]),
            vec!["echo"]
        );
        assert_eq!(
            truncate_at_implausible(&["echo", "$TOKEN"]),
            vec!["echo"]
        );
        assert_eq!(
            truncate_at_implausible(&["sort", "`foo`"]),
            vec!["sort"]
        );
        // URLs pass is_plausible_word (no shell-syntax chars) — they're
        // filtered at display time by is_plausible_item's `://` check
        // instead. Belt and suspenders.
    }

    #[test]
    fn truncate_at_implausible_keeps_clean_invocation() {
        assert_eq!(
            truncate_at_implausible(&["git", "checkout", "main"]),
            vec!["git", "checkout", "main"]
        );
        assert_eq!(
            truncate_at_implausible(&["docker", "run", "-it", "alpine"]),
            vec!["docker", "run", "-it", "alpine"]
        );
    }

    #[test]
    fn parse_history_filters_quoted_args() {
        let dir = std::env::temp_dir().join(format!("zsh-ios-hist-junk-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let path = dir.join("history");
        std::fs::write(&path, "echo 'export FOO=bar'\n").unwrap();

        let mut trie = CommandTrie::new();
        trie.insert_command("echo");

        parse_history(&path, &mut trie).unwrap();

        // echo learned, but 'export must NOT be a subcommand.
        let echo = trie.root.get_child("echo").unwrap();
        assert!(echo.get_child("'export").is_none(), "quoted junk leaked");
        assert!(echo.get_child("FOO=bar").is_none(), "assignment leaked");

        let _ = std::fs::remove_dir_all(&dir);
    }
}
