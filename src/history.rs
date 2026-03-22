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
pub fn parse_history(path: &Path, trie: &mut CommandTrie) -> Result<u32, Box<dyn std::error::Error>> {
    let content = fs::read_to_string(path)
        .or_else(|_| {
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
                    // FOO=bar command args -> insert "command args"
                    trie.insert(&words[1..]);
                    count += 1;
                }
                continue;
            }

            trie.insert(&words);
            count += 1;
        }
    }

    Ok(count)
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
                }
                // Single & (background) — end of this command
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
        assert_eq!(strip_history_prefix(": 1234567890:0;git status"), "git status");
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
        let count = parse_history(&path, &mut trie).unwrap();

        assert_eq!(count, 4); // git checkout main, terraform apply, echo hello, grep h
        assert!(trie.root.get_child("git").is_some());
        assert!(trie.root.get_child("terraform").is_some());
        assert!(trie.root.get_child("grep").is_some());

        let git = trie.root.get_child("git").unwrap();
        assert!(git.get_child("checkout").is_some());

        let _ = std::fs::remove_dir_all(&dir);
    }
}
