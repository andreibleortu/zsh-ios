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
