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

        let command = strip_history_prefix(&full_line);
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
                        trie.insert(&words[1..]);
                        count += 1;
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

            trie.insert(&words);
            count += 1;
        }
    }

    Ok(count)
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

/// Strip the Zsh extended history prefix (`: timestamp:duration;`) if present.
fn strip_history_prefix(line: &str) -> &str {
    if line.starts_with(": ") {
        // Extended format: `: 1234567890:0;actual command`
        if let Some(pos) = line.find(';') {
            return line[pos + 1..].trim();
        }
    }
    line.trim()
}

/// Split a command line on unquoted pipes and semicolons to extract
/// individual command segments.
pub fn split_command_segments(line: &str) -> Vec<&str> {
    let mut segments = Vec::new();
    let mut start = 0;
    let mut in_single_quote = false;
    let mut in_double_quote = false;
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
            b'|' | b';' if !in_single_quote && !in_double_quote => {
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
            b'&' if !in_single_quote && !in_double_quote => {
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
    fn test_strip_history_prefix() {
        assert_eq!(
            strip_history_prefix(": 1234567890:0;git status"),
            "git status"
        );
        assert_eq!(strip_history_prefix("git status"), "git status");
        assert_eq!(strip_history_prefix("  ls -la  "), "ls -la");
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
}
