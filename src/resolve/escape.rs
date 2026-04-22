//! Shell-escape helpers for filesystem paths returned by path resolution.
//!
//! The engine hands a resolved path to these helpers before splicing it
//! back into the command line; they're the reason `cd Des/Fo` → `cd
//! Desktop/Folder\ With\ Spaces` doesn't break on whitespace.

/// Escape shell metacharacters in resolved paths so they're safe to execute.
pub(super) fn shell_escape_path(path: &str) -> String {
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
pub(super) fn shell_escape_path_glob(path: &str) -> String {
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

pub(super) fn escape_resolved_path(original_word: &str, resolved: &str) -> String {
    if original_word.contains("**") {
        shell_escape_path_glob(resolved)
    } else {
        shell_escape_path(resolved)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
}
