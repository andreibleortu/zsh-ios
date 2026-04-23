use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};

#[derive(Debug)]
pub enum PathResult {
    Resolved(String),
    /// Multiple full resolved paths -- caller should let user pick.
    Ambiguous(Vec<String>),
    Unchanged,
}

/// Expand a `~N` / `~+N` / `~-N` token against the directory stack.
///
/// Zsh convention: `~0` (or `~+0`) is `$PWD`; `~1` is the first pushed
/// directory; `~-1` counts from the end. The plugin stores `$PWD` at
/// `dir_stack[0]` so indices map directly to the slice.
///
/// Returns `None` if `token` doesn't look like a dirstack reference or if
/// the requested index is out of range.
pub fn expand_dir_stack(token: &str, dir_stack: &[String]) -> Option<String> {
    let rest = token.strip_prefix('~')?;
    if rest.is_empty() {
        return None;
    }

    // Collect the numeric (possibly sign-prefixed) part and the tail.
    // Accepted forms: ~N, ~+N, ~-N (where N is one or more decimal digits).
    let sign_and_digits_end = rest
        .char_indices()
        .take_while(|(i, c)| (*i == 0 && (*c == '+' || *c == '-')) || c.is_ascii_digit())
        .last()
        .map(|(i, c)| i + c.len_utf8())
        .unwrap_or(0);

    if sign_and_digits_end == 0 {
        return None;
    }

    let num_str = &rest[..sign_and_digits_end];
    let tail = &rest[sign_and_digits_end..];

    // Must have at least one digit (reject bare `~+` or `~-`).
    if !num_str.chars().any(|c| c.is_ascii_digit()) {
        return None;
    }

    let index: i32 = num_str.trim_start_matches('+').parse().ok()?;
    let len = dir_stack.len() as i32;
    let resolved = if index >= 0 {
        dir_stack.get(index as usize)?
    } else {
        let idx = len + index;
        if idx < 0 {
            return None;
        }
        dir_stack.get(idx as usize)?
    };

    Some(format!("{}{}", resolved, tail))
}

/// Resolve an abbreviated path against the real filesystem.
///
/// For each component separated by `/`, tries:
/// 1. Exact match (case-sensitive) -- wins immediately
/// 2. Unique case-sensitive prefix match
/// 3. Unique case-insensitive prefix match
///
/// When ambiguous, looks ahead at subsequent components to disambiguate.
/// If multiple candidates survive, returns all fully-resolved paths.
///
/// `named_dirs` maps Zsh `hash -d` names to their absolute paths.
/// Tokens of the form `~name`, `name/rest`, or `name:rest` are expanded
/// before filesystem resolution when `name` is a key in `named_dirs`.
///
/// `dir_stack` is the Zsh directory stack (PWD at index 0). Tokens of the
/// form `~N` / `~+N` / `~-N` are expanded against this slice before
/// filesystem resolution.
pub fn resolve_path(
    abbreviated: &str,
    named_dirs: &HashMap<String, String>,
    dir_stack: &[String],
) -> PathResult {
    resolve_path_inner(abbreviated, false, named_dirs, dir_stack)
}

/// Like `resolve_path` but only matches directories (for cd, pushd, etc.).
pub fn resolve_path_dirs_only(
    abbreviated: &str,
    named_dirs: &HashMap<String, String>,
    dir_stack: &[String],
) -> PathResult {
    resolve_path_inner(abbreviated, true, named_dirs, dir_stack)
}

fn resolve_path_inner(
    abbreviated: &str,
    dirs_only: bool,
    named_dirs: &HashMap<String, String>,
    dir_stack: &[String],
) -> PathResult {
    if abbreviated.is_empty() {
        return PathResult::Unchanged;
    }

    // Expand named-dir references first (highest precedence).
    // A reference expands to an absolute path, so we recurse once with an
    // empty named_dirs to avoid a second expansion pass.
    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    if let Some(expanded) = expand_named_dir_with_cwd(abbreviated, named_dirs, &cwd) {
        return resolve_path_inner(&expanded, dirs_only, &HashMap::new(), &[]);
    }

    // Expand dirstack references (~N / ~+N / ~-N).
    if let Some(expanded) = expand_dir_stack(abbreviated, dir_stack) {
        return resolve_path_inner(&expanded, dirs_only, &HashMap::new(), &[]);
    }

    let trailing_slash = abbreviated.ends_with('/');

    let (base_dir, components, prefix_str) = parse_path_parts(abbreviated);
    let is_relative = !abbreviated.starts_with('/') && !abbreviated.starts_with('~');

    let init_parts: Vec<String> = if is_relative {
        vec![]
    } else {
        vec![prefix_str]
    };

    match resolve_components(&base_dir, &components, init_parts, dirs_only) {
        ComponentsResult::Resolved(parts) => {
            let mut result = join_path_parts(&parts);
            if trailing_slash && !result.ends_with('/') {
                result.push('/');
            }
            if result == abbreviated {
                PathResult::Unchanged
            } else {
                PathResult::Resolved(result)
            }
        }
        ComponentsResult::Ambiguous(paths) => {
            let resolved: Vec<String> = paths
                .into_iter()
                .map(|p| {
                    let mut r = join_path_parts(&p);
                    if trailing_slash && !r.ends_with('/') {
                        r.push('/');
                    }
                    r
                })
                .collect();
            if resolved.len() == 1 {
                let r = resolved.into_iter().next().unwrap();
                if r == abbreviated {
                    PathResult::Unchanged
                } else {
                    PathResult::Resolved(r)
                }
            } else {
                PathResult::Ambiguous(resolved)
            }
        }
        ComponentsResult::Unchanged(parts) => {
            let _ = parts;
            PathResult::Unchanged
        }
    }
}

enum ComponentsResult {
    Resolved(Vec<String>),
    /// Multiple possible full resolutions.
    Ambiguous(Vec<Vec<String>>),
    /// Nothing changed.
    Unchanged(Vec<String>),
}

fn resolve_components(
    base_dir: &Path,
    components: &[String],
    prefix_parts: Vec<String>,
    dirs_only: bool,
) -> ComponentsResult {
    let mut current_dir = base_dir.to_path_buf();
    let mut resolved_parts = prefix_parts;

    for (i, component) in components.iter().enumerate() {
        if component.is_empty() {
            continue;
        }

        if *component == ".." || *component == "." {
            current_dir = current_dir.join(component);
            resolved_parts.push(component.to_string());
            continue;
        }

        match resolve_component(&current_dir, component, dirs_only) {
            ComponentMatch::Exact(name) | ComponentMatch::Unique(name) => {
                current_dir = current_dir.join(&name);
                resolved_parts.push(name);
            }
            ComponentMatch::Ambiguous(candidates) => {
                let remaining = &components[i + 1..];

                if remaining.is_empty() {
                    // Surface ambiguity for dirs-only commands and for explicit
                    // pattern matching (* contains, ! suffix) — the user asked
                    // for resolution, so give them a picker rather than giving up.
                    let is_explicit_pattern = component.starts_with('*')
                        || (component.starts_with('!') && !component.starts_with("\\!"));
                    if dirs_only || is_explicit_pattern {
                        let all_paths: Vec<Vec<String>> = candidates
                            .iter()
                            .map(|c| {
                                let mut parts = resolved_parts.clone();
                                parts.push(c.clone());
                                parts
                            })
                            .collect();
                        return ComponentsResult::Ambiguous(all_paths);
                    }
                    // Plain prefix, last component, no look-ahead possible -- give up
                    resolved_parts.push(component.to_string());
                    return ComponentsResult::Unchanged(resolved_parts);
                }

                // Find which candidates have children matching the next component
                let winners = deep_filter(&current_dir, &candidates, remaining, dirs_only);

                if winners.len() == 1 {
                    current_dir = current_dir.join(&winners[0]);
                    resolved_parts.push(winners[0].clone());
                    continue;
                }

                if winners.is_empty() {
                    resolved_parts.push(component.to_string());
                    for r in remaining {
                        resolved_parts.push(r.to_string());
                    }
                    return ComponentsResult::Unchanged(resolved_parts);
                }

                // Multiple candidates survive -- fork resolution for each
                let mut all_paths: Vec<Vec<String>> = Vec::new();
                for winner in &winners {
                    let child_dir = current_dir.join(winner);
                    let mut fork_parts = resolved_parts.clone();
                    fork_parts.push(winner.clone());
                    match resolve_components(&child_dir, remaining, fork_parts, dirs_only) {
                        ComponentsResult::Resolved(parts) | ComponentsResult::Unchanged(parts) => {
                            all_paths.push(parts);
                        }
                        ComponentsResult::Ambiguous(mut nested) => {
                            all_paths.append(&mut nested);
                        }
                    }
                }

                return ComponentsResult::Ambiguous(all_paths);
            }
            ComponentMatch::None => {
                resolved_parts.push(component.to_string());
                for remaining in &components[i + 1..] {
                    resolved_parts.push(remaining.to_string());
                }
                return ComponentsResult::Unchanged(resolved_parts);
            }
        }
    }

    ComponentsResult::Resolved(resolved_parts)
}

/// Returns true when `word` looks like a named-dir reference (`name/rest`,
/// `name:rest`, or `~name`) that would be expanded by `expand_named_dir`.
/// Used in `ArgMode::Normal` to decide whether to call `resolve_path` on
/// words that don't otherwise look like paths.
pub fn looks_like_named_dir_ref(word: &str, named_dirs: &HashMap<String, String>) -> bool {
    if named_dirs.is_empty() {
        return false;
    }
    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    expand_named_dir_with_cwd(word, named_dirs, &cwd).is_some()
}

/// Expand a token whose prefix references a named directory.
///
/// Returns `None` if the token doesn't start with a named-dir reference or
/// if a real directory in `cwd` takes precedence over the named-dir entry.
///
/// Handles three forms:
/// - `~name` / `~name/rest` / `~name:rest`
/// - `name/rest` where `name` is a full key in `named_dirs`
/// - `name:rest` (same, but Zsh colon-separator convention)
///
/// A bare `name` (no `/` or `:`) is NOT expanded — the caller probably
/// means the name literally.
///
/// URL-like tokens (`scheme://...`) and SSH targets (`user@host:...`) are
/// left alone even if they happen to contain a matching name.
pub fn expand_named_dir(
    token: &str,
    named_dirs: &HashMap<String, String>,
) -> Option<String> {
    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    expand_named_dir_with_cwd(token, named_dirs, &cwd)
}

pub(crate) fn expand_named_dir_with_cwd(
    token: &str,
    named_dirs: &HashMap<String, String>,
    cwd: &Path,
) -> Option<String> {
    if named_dirs.is_empty() {
        return None;
    }

    // ~name form: tilde immediately followed by the dir name (no space).
    if let Some(rest) = token.strip_prefix('~') {
        // Split on the first '/' or ':' to isolate the name.
        let (name, sep_and_tail) = match rest.find(['/', ':']) {
            Some(i) => (&rest[..i], &rest[i..]),
            None => (rest, ""),
        };
        if name.is_empty() {
            // Plain `~` or `~/...` — home dir, not a named dir.
            return None;
        }
        if let Some(base) = named_dirs.get(name) {
            // sep_and_tail starts with '/' or ':'; convert ':' to '/'.
            let tail = if let Some(after_colon) = sep_and_tail.strip_prefix(':') {
                format!("/{after_colon}")
            } else {
                sep_and_tail.to_string()
            };
            return Some(format!("{base}{tail}"));
        }
        return None;
    }

    // name/rest or name:rest
    let sep_idx = token.find(['/', ':'])?;
    let name = &token[..sep_idx];
    let sep = &token[sep_idx..sep_idx + 1];

    // Skip URL-like tokens: `scheme://...`
    if sep == ":" {
        if token.get(sep_idx..sep_idx + 3) == Some("://") {
            return None;
        }
        // Skip SSH-style targets: `user@host:/path`
        if token[..sep_idx].contains('@') {
            return None;
        }
    }

    // A real directory in cwd wins over the named dir.
    if cwd.join(name).is_dir() {
        return None;
    }

    // For `name/rest` the separator is already `/`, so tail includes it.
    // For `name:rest` we replace the `:` with `/` so the result is a valid path.
    named_dirs.get(name).map(|base| {
        let after_sep = &token[sep_idx + 1..];
        if sep == ":" && !after_sep.is_empty() {
            format!("{base}/{after_sep}")
        } else {
            // sep is '/' (or ':' with empty remainder — keep base as-is)
            let tail = &token[sep_idx..];
            format!("{base}{tail}")
        }
    })
}

fn parse_path_parts(abbreviated: &str) -> (PathBuf, Vec<String>, String) {
    if abbreviated.starts_with('~') {
        let home = dirs::home_dir().unwrap_or_else(|| PathBuf::from("/"));
        let after_tilde = abbreviated.strip_prefix('~').unwrap_or("");
        let after_tilde = after_tilde.strip_prefix('/').unwrap_or(after_tilde);
        let components: Vec<String> = if after_tilde.is_empty() {
            vec![]
        } else {
            after_tilde.split('/').map(String::from).collect()
        };
        (home, components, "~".to_string())
    } else if let Some(after_slash) = abbreviated.strip_prefix('/') {
        let components: Vec<String> = after_slash.split('/').map(String::from).collect();
        (PathBuf::from("/"), components, String::new())
    } else {
        let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
        let components: Vec<String> = abbreviated.split('/').map(String::from).collect();
        (cwd, components, String::new())
    }
}

#[derive(Debug)]
enum ComponentMatch {
    Exact(String),
    Unique(String),
    Ambiguous(Vec<String>),
    None,
}

fn resolve_component(dir: &Path, pattern: &str, dirs_only: bool) -> ComponentMatch {
    let entries = list_dir(dir, dirs_only);

    // Backslash escapes: \! → literal !, \* → literal *
    if let Some(rest) = pattern.strip_prefix("\\!") {
        let literal = format!("!{rest}");
        return prefix_match(&literal, &entries);
    }
    if let Some(rest) = pattern.strip_prefix("\\*") {
        let literal = format!("*{rest}");
        return prefix_match(&literal, &entries);
    }

    // Double-star passthrough: **rest → *rest (literal shell glob, no resolution)
    if let Some(rest) = pattern.strip_prefix("**") {
        return ComponentMatch::Exact(format!("*{rest}"));
    }

    // Suffix match: !suffix
    if let Some(suffix) = pattern.strip_prefix('!')
        && !suffix.is_empty()
    {
        return match_with(suffix, &entries, |e, s| e.ends_with(s));
    }

    // Contains match: *substring
    if let Some(sub) = pattern.strip_prefix('*')
        && !sub.is_empty()
    {
        return match_with(sub, &entries, |e, s| e.contains(s));
    }

    // Default: prefix match.
    prefix_match(pattern, &entries)
}

fn prefix_match(pattern: &str, entries: &[String]) -> ComponentMatch {
    if entries.iter().any(|e| e == pattern) {
        return ComponentMatch::Exact(pattern.to_string());
    }
    match_with(pattern, entries, |e, s| e.starts_with(s))
}

/// Generic case-sensitive then case-insensitive matching.
fn match_with<F>(needle: &str, entries: &[String], predicate: F) -> ComponentMatch
where
    F: Fn(&str, &str) -> bool,
{
    // Case-sensitive
    let cs: Vec<&String> = entries.iter().filter(|e| predicate(e, needle)).collect();
    match cs.len() {
        1 => return ComponentMatch::Unique(cs[0].clone()),
        2.. => return ComponentMatch::Ambiguous(cs.into_iter().cloned().collect()),
        _ => {}
    }
    // Case-insensitive
    let lower = needle.to_lowercase();
    let ci: Vec<&String> = entries
        .iter()
        .filter(|e| predicate(&e.to_lowercase(), &lower))
        .collect();
    match ci.len() {
        1 => ComponentMatch::Unique(ci[0].clone()),
        2.. => ComponentMatch::Ambiguous(ci.into_iter().cloned().collect()),
        _ => ComponentMatch::None,
    }
}

/// Filter ambiguous candidates by which ones have children matching the next component.
fn deep_filter(
    parent: &Path,
    candidates: &[String],
    remaining: &[String],
    dirs_only: bool,
) -> Vec<String> {
    if remaining.is_empty() {
        return candidates.to_vec();
    }
    let next = &remaining[0];
    if next.is_empty() {
        return candidates.to_vec();
    }

    // Double-star passthrough: can't filter by a shell glob, let all candidates through.
    if next.starts_with("**") {
        return candidates.to_vec();
    }

    // Determine match predicate from the next component's mode.
    let (needle, pred): (&str, fn(&str, &str) -> bool) = if let Some(s) = next.strip_prefix('!') {
        if !s.is_empty() {
            (s, |e, n| e.ends_with(n))
        } else {
            (next.as_str(), |e, n| e.starts_with(n))
        }
    } else if let Some(s) = next.strip_prefix('*') {
        if !s.is_empty() {
            (s, |e, n| e.contains(n))
        } else {
            (next.as_str(), |e, n| e.starts_with(n))
        }
    } else {
        (next.as_str(), |e, n| e.starts_with(n))
    };

    let lower_needle = needle.to_lowercase();

    candidates
        .iter()
        .filter(|cand| {
            let child_dir = parent.join(cand);
            let entries = list_dir(&child_dir, dirs_only);
            entries.iter().any(|e| {
                e == next.as_str() || pred(e, needle) || pred(&e.to_lowercase(), &lower_needle)
            })
        })
        .cloned()
        .collect()
}

fn list_dir(dir: &Path, dirs_only: bool) -> Vec<String> {
    match fs::read_dir(dir) {
        Ok(entries) => entries
            .flatten()
            .filter(|e| !dirs_only || e.path().is_dir())
            .map(|e| e.file_name().to_string_lossy().to_string())
            .collect(),
        Err(_) => vec![],
    }
}

fn join_path_parts(parts: &[String]) -> String {
    if parts.is_empty() {
        return String::new();
    }
    let first = &parts[0];
    let rest = &parts[1..];
    if first.is_empty() && rest.is_empty() {
        return String::new();
    }
    if first == "~" {
        if rest.is_empty() {
            return "~".to_string();
        }
        return format!("~/{}", rest.join("/"));
    }
    if first.is_empty() {
        return format!("/{}", rest.join("/"));
    }
    parts.join("/")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn empty_named_dirs() -> HashMap<String, String> {
        HashMap::new()
    }

    fn empty_dir_stack() -> Vec<String> {
        vec![]
    }

    #[test]
    fn test_resolve_absolute() {
        let result = resolve_path("/usr/lo", &empty_named_dirs(), &empty_dir_stack());
        if Path::new("/usr/local").exists() {
            match result {
                PathResult::Resolved(s) => assert_eq!(s, "/usr/local"),
                _ => panic!("Expected Resolved"),
            }
        }
    }

    #[test]
    fn test_resolve_tilde() {
        let home = dirs::home_dir().unwrap();
        if home.join("Desktop").exists() && !home.join("Desk").exists() {
            match resolve_path("~/Desk", &empty_named_dirs(), &empty_dir_stack()) {
                PathResult::Resolved(s) => assert_eq!(s, "~/Desktop"),
                _ => panic!("Expected Resolved"),
            }
        }
    }

    #[test]
    fn test_exact_match_wins() {
        let _g = crate::test_util::CWD_LOCK.lock().unwrap();
        let dir = std::env::temp_dir().join("zsh-ios-test-path");
        let _ = fs::create_dir_all(dir.join("foo"));
        let _ = fs::create_dir_all(dir.join("foobar"));

        std::env::set_current_dir(&dir).ok();
        let result = resolve_component(&dir, "foo", false);
        match result {
            ComponentMatch::Exact(name) => assert_eq!(name, "foo"),
            _ => panic!("Expected Exact match"),
        }

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_suffix_match() {
        let dir = std::env::temp_dir().join("zsh-ios-test-suffix");
        let _ = fs::remove_dir_all(&dir);
        let _ = fs::create_dir_all(dir.join("test-1"));
        let _ = fs::create_dir_all(dir.join("test-2"));
        let _ = fs::create_dir_all(dir.join("test-3"));

        // !3 matches the entry ending with "3"
        let result = resolve_component(&dir, "!3", false);
        match result {
            ComponentMatch::Unique(name) => assert_eq!(name, "test-3"),
            other => panic!("Expected Unique suffix match, got {:?}", other),
        }

        // !test is ambiguous (all three end with a digit, not "test")
        let result = resolve_component(&dir, "!test", false);
        match result {
            ComponentMatch::None => {}
            other => panic!("Expected None for !test, got {:?}", other),
        }

        // bare ! should not match anything
        let result = resolve_component(&dir, "!", false);
        match result {
            ComponentMatch::None => {}
            other => panic!("Expected None for bare !, got {:?}", other),
        }

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_contains_match() {
        let dir = std::env::temp_dir().join("zsh-ios-test-contains");
        let _ = fs::remove_dir_all(&dir);
        let _ = fs::create_dir_all(dir.join("app-config-prod"));
        let _ = fs::create_dir_all(dir.join("app-config-staging"));
        let _ = fs::create_dir_all(dir.join("unrelated"));

        // *prod matches only app-config-prod
        let result = resolve_component(&dir, "*prod", false);
        match result {
            ComponentMatch::Unique(name) => assert_eq!(name, "app-config-prod"),
            other => panic!("Expected Unique contains match, got {:?}", other),
        }

        // *config matches two entries
        let result = resolve_component(&dir, "*config", false);
        match result {
            ComponentMatch::Ambiguous(names) => assert_eq!(names.len(), 2),
            other => panic!("Expected Ambiguous for *config, got {:?}", other),
        }

        // *zzz matches nothing
        let result = resolve_component(&dir, "*zzz", false);
        match result {
            ComponentMatch::None => {}
            other => panic!("Expected None for *zzz, got {:?}", other),
        }

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_double_star_passthrough() {
        let dir = std::env::temp_dir().join("zsh-ios-test-doublestar");
        let _ = fs::remove_dir_all(&dir);
        let _ = fs::write(dir.join("a.py"), "");
        let _ = fs::write(dir.join("b.py"), "");

        // **.py → *.py (literal glob, not a contains-match attempt)
        let result = resolve_component(&dir, "**.py", false);
        match result {
            ComponentMatch::Exact(name) => assert_eq!(name, "*.py"),
            other => panic!("Expected Exact passthrough for **.py, got {:?}", other),
        }

        // bare ** → *
        let result = resolve_component(&dir, "**", false);
        match result {
            ComponentMatch::Exact(name) => assert_eq!(name, "*"),
            other => panic!("Expected Exact passthrough for **, got {:?}", other),
        }

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_backslash_escape() {
        let dir = std::env::temp_dir().join("zsh-ios-test-escape");
        let _ = fs::remove_dir_all(&dir);
        let _ = fs::create_dir_all(dir.join("!important"));
        let _ = fs::create_dir_all(dir.join("*starred"));

        // \! should match literally as prefix, not as suffix mode
        let result = resolve_component(&dir, "\\!imp", false);
        match result {
            ComponentMatch::Unique(name) => assert_eq!(name, "!important"),
            other => panic!("Expected Unique for escaped !, got {:?}", other),
        }

        // \* should match literally as prefix, not as contains mode
        let result = resolve_component(&dir, "\\*star", false);
        match result {
            ComponentMatch::Unique(name) => assert_eq!(name, "*starred"),
            other => panic!("Expected Unique for escaped *, got {:?}", other),
        }

        let _ = fs::remove_dir_all(&dir);
    }

    // --- Tests for join_path_parts ---

    #[test]
    fn test_join_path_parts_empty() {
        assert_eq!(join_path_parts(&[]), "");
    }

    #[test]
    fn test_join_path_parts_absolute() {
        let parts: Vec<String> = vec!["".into(), "usr".into(), "local".into()];
        assert_eq!(join_path_parts(&parts), "/usr/local");
    }

    #[test]
    fn test_join_path_parts_tilde() {
        let parts: Vec<String> = vec!["~".into(), "Documents".into()];
        assert_eq!(join_path_parts(&parts), "~/Documents");
    }

    #[test]
    fn test_join_path_parts_tilde_alone() {
        let parts: Vec<String> = vec!["~".into()];
        assert_eq!(join_path_parts(&parts), "~");
    }

    #[test]
    fn test_join_path_parts_relative() {
        let parts: Vec<String> = vec!["src".into(), "main.rs".into()];
        assert_eq!(join_path_parts(&parts), "src/main.rs");
    }

    #[test]
    fn test_join_path_parts_root_only() {
        let parts: Vec<String> = vec!["".into()];
        assert_eq!(join_path_parts(&parts), "");
    }

    // --- Tests for resolve_path_dirs_only ---

    #[test]
    fn test_resolve_path_dirs_only() {
        let dir = std::env::temp_dir().join("zsh-ios-test-dirsonly");
        let _ = fs::remove_dir_all(&dir);
        let _ = fs::create_dir_all(dir.join("subdir"));
        // Create a file that shares a prefix
        let _ = fs::write(dir.join("subfile"), "");

        let result = resolve_component(&dir, "sub", true);
        match result {
            ComponentMatch::Unique(name) => assert_eq!(name, "subdir"),
            other => panic!("Expected Unique dir match, got {:?}", other),
        }

        let _ = fs::remove_dir_all(&dir);
    }

    // --- Tests for case-insensitive fallback ---

    #[test]
    fn test_case_insensitive_match() {
        let dir = std::env::temp_dir().join("zsh-ios-test-case");
        let _ = fs::remove_dir_all(&dir);
        let _ = fs::create_dir_all(dir.join("Documents"));

        // Lowercase input should match uppercase entry (case-insensitive fallback)
        let result = resolve_component(&dir, "doc", false);
        match result {
            ComponentMatch::Unique(name) => assert_eq!(name, "Documents"),
            other => panic!("Expected case-insensitive Unique match, got {:?}", other),
        }

        let _ = fs::remove_dir_all(&dir);
    }

    // --- Tests for ambiguous match ---

    #[test]
    fn test_ambiguous_prefix_match() {
        let dir = std::env::temp_dir().join("zsh-ios-test-ambiguous");
        let _ = fs::remove_dir_all(&dir);
        let _ = fs::create_dir_all(dir.join("apple"));
        let _ = fs::create_dir_all(dir.join("application"));

        let result = resolve_component(&dir, "app", false);
        match result {
            ComponentMatch::Ambiguous(names) => {
                assert_eq!(names.len(), 2);
                assert!(names.contains(&"apple".to_string()));
                assert!(names.contains(&"application".to_string()));
            }
            other => panic!("Expected Ambiguous, got {:?}", other),
        }

        let _ = fs::remove_dir_all(&dir);
    }

    // --- Tests for resolve_path end-to-end ---

    #[test]
    fn test_resolve_path_unchanged() {
        // A completely non-matching path should return Unchanged
        match resolve_path("zzzznonexistent", &empty_named_dirs(), &empty_dir_stack()) {
            PathResult::Unchanged => {}
            other => panic!("Expected Unchanged for nonexistent, got {:?}", other),
        }
    }

    #[test]
    fn test_resolve_path_empty() {
        match resolve_path("", &empty_named_dirs(), &empty_dir_stack()) {
            PathResult::Unchanged => {}
            other => panic!("Expected Unchanged for empty, got {:?}", other),
        }
    }

    // --- Tests for list_dir ---

    #[test]
    fn test_list_dir_nonexistent() {
        let entries = list_dir(Path::new("/nonexistent_dir_zshios"), false);
        assert!(entries.is_empty());
    }

    // --- Tests for deep_filter ---

    #[test]
    fn test_deep_filter_empty_remaining() {
        let candidates = vec!["a".to_string(), "b".to_string()];
        let result = deep_filter(Path::new("/tmp"), &candidates, &[], false);
        assert_eq!(result, candidates);
    }

    #[test]
    fn test_deep_filter_empty_next() {
        let candidates = vec!["a".to_string()];
        let remaining = vec!["".to_string()];
        let result = deep_filter(Path::new("/tmp"), &candidates, &remaining, false);
        assert_eq!(result, candidates);
    }

    // --- parse_path_parts ---

    #[test]
    fn parse_path_parts_absolute() {
        let (base, comps, prefix) = parse_path_parts("/usr/local/bin");
        assert_eq!(base, PathBuf::from("/"));
        assert_eq!(comps, vec!["usr", "local", "bin"]);
        assert_eq!(prefix, "");
    }

    #[test]
    fn parse_path_parts_tilde_only() {
        let (_, comps, prefix) = parse_path_parts("~");
        assert!(comps.is_empty());
        assert_eq!(prefix, "~");
    }

    #[test]
    fn parse_path_parts_tilde_with_path() {
        let (_, comps, prefix) = parse_path_parts("~/Documents/foo");
        assert_eq!(comps, vec!["Documents", "foo"]);
        assert_eq!(prefix, "~");
    }

    #[test]
    fn parse_path_parts_relative() {
        let (_, comps, prefix) = parse_path_parts("src/main.rs");
        assert_eq!(comps, vec!["src", "main.rs"]);
        assert_eq!(prefix, "");
    }

    #[test]
    fn parse_path_parts_empty_component_between_slashes() {
        // "a//b" produces an empty middle component; we don't collapse — the
        // downstream resolver sees it.
        let (_, comps, _) = parse_path_parts("a//b");
        assert_eq!(comps, vec!["a", "", "b"]);
    }

    // --- resolve_path end-to-end with tempdirs ---

    #[test]
    fn resolve_path_absolute_prefix_expansion() {
        let td = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(td.path().join("application")).unwrap();
        let abbrev = format!("{}/appl", td.path().display());
        match resolve_path(&abbrev, &empty_named_dirs(), &empty_dir_stack()) {
            PathResult::Resolved(s) => {
                assert!(s.ends_with("/application"), "got: {}", s);
            }
            other => panic!("expected Resolved, got {:?}", other),
        }
    }

    #[test]
    fn resolve_path_deep_disambiguation_picks_branch() {
        // Two top-level dirs share a prefix, but only one has a matching
        // subdirectory further down — deep lookahead should pick it.
        let td = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(td.path().join("Application Support/zsh-ios")).unwrap();
        std::fs::create_dir_all(td.path().join("Application Scripts/other")).unwrap();
        let abbrev = format!("{}/Appli/zsh-", td.path().display());
        match resolve_path(&abbrev, &empty_named_dirs(), &empty_dir_stack()) {
            PathResult::Resolved(s) => {
                assert!(s.contains("Application Support"), "branched wrong: {}", s);
                assert!(s.ends_with("zsh-ios"), "didn't complete leaf: {}", s);
            }
            other => panic!("expected Resolved, got {:?}", other),
        }
    }

    #[test]
    fn resolve_path_dirs_only_surfaces_ambiguous_on_final_component() {
        // For cd/pushd (dirs_only=true), a final-component ambiguous prefix
        // is surfaced as Ambiguous so the Zsh plugin can show a picker.
        // Non-dirs-only commands deliberately return Unchanged and leave
        // the disambiguation to the shell.
        let td = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(td.path().join("apple")).unwrap();
        std::fs::create_dir_all(td.path().join("application")).unwrap();
        let abbrev = format!("{}/app", td.path().display());
        match resolve_path_dirs_only(&abbrev, &empty_named_dirs(), &empty_dir_stack()) {
            PathResult::Ambiguous(paths) => {
                assert_eq!(paths.len(), 2);
                assert!(paths.iter().any(|p| p.ends_with("/apple")));
                assert!(paths.iter().any(|p| p.ends_with("/application")));
            }
            other => panic!("expected Ambiguous, got {:?}", other),
        }
        // Same input in non-dirs-only mode: Unchanged (caller handles it).
        match resolve_path(&abbrev, &empty_named_dirs(), &empty_dir_stack()) {
            PathResult::Unchanged => {}
            other => panic!("expected Unchanged for plain resolve, got {:?}", other),
        }
    }

    #[test]
    fn resolve_path_preserves_trailing_slash() {
        let td = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(td.path().join("targetdir")).unwrap();
        let abbrev = format!("{}/target/", td.path().display());
        match resolve_path(&abbrev, &empty_named_dirs(), &empty_dir_stack()) {
            PathResult::Resolved(s) => assert!(s.ends_with('/'), "lost trailing slash: {}", s),
            other => panic!("expected Resolved, got {:?}", other),
        }
    }

    #[test]
    fn resolve_path_dirs_only_rejects_file_prefix_match() {
        let td = tempfile::tempdir().unwrap();
        std::fs::write(td.path().join("only-a-file"), "").unwrap();
        let abbrev = format!("{}/only", td.path().display());
        match resolve_path_dirs_only(&abbrev, &empty_named_dirs(), &empty_dir_stack()) {
            PathResult::Unchanged => {}
            // Allow Resolved only if the platform for some reason creates
            // a matching dir — in our tempdir we only made a file.
            other => panic!("expected Unchanged, got {:?}", other),
        }
    }

    #[test]
    fn resolve_path_suffix_mode_end_to_end() {
        let td = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(td.path().join("tests")).unwrap();
        std::fs::create_dir_all(td.path().join("tests/test-1")).unwrap();
        std::fs::create_dir_all(td.path().join("tests/test-5")).unwrap();
        let abbrev = format!("{}/tests/!5", td.path().display());
        match resolve_path(&abbrev, &empty_named_dirs(), &empty_dir_stack()) {
            PathResult::Resolved(s) => assert!(s.ends_with("test-5"), "got: {}", s),
            other => panic!("expected Resolved, got {:?}", other),
        }
    }

    #[test]
    fn resolve_path_double_star_turns_into_literal_glob() {
        let td = tempfile::tempdir().unwrap();
        std::fs::write(td.path().join("a.py"), "").unwrap();
        std::fs::write(td.path().join("b.py"), "").unwrap();
        let abbrev = format!("{}/**.py", td.path().display());
        match resolve_path(&abbrev, &empty_named_dirs(), &empty_dir_stack()) {
            PathResult::Resolved(s) => assert!(s.ends_with("/*.py"), "got: {}", s),
            // It is also valid for this to be Unchanged if the path already
            // starts with "*/*.py" after joining — but our tempdir abbrev
            // ensures ** is the second component.
            other => panic!("expected Resolved with literal glob, got {:?}", other),
        }
    }

    // --- deep_filter with a real filesystem ---

    #[test]
    fn deep_filter_narrows_on_next_component() {
        let td = tempfile::tempdir().unwrap();
        let base = td.path();
        std::fs::create_dir_all(base.join("apple/a-specific")).unwrap();
        std::fs::create_dir_all(base.join("application/other-name")).unwrap();

        let candidates = vec!["apple".to_string(), "application".to_string()];
        // Next component "a-" only matches under "apple/"
        let remaining = vec!["a-".to_string()];
        let kept = deep_filter(base, &candidates, &remaining, false);
        assert_eq!(kept, vec!["apple".to_string()]);
    }

    // --- list_dir ---

    #[test]
    fn list_dir_filters_to_dirs_only() {
        let td = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(td.path().join("somedir")).unwrap();
        std::fs::write(td.path().join("somefile"), "").unwrap();

        let all = list_dir(td.path(), false);
        assert!(all.contains(&"somedir".to_string()));
        assert!(all.contains(&"somefile".to_string()));

        let dirs = list_dir(td.path(), true);
        assert!(dirs.contains(&"somedir".to_string()));
        assert!(!dirs.contains(&"somefile".to_string()));
    }

    // --- expand_named_dir tests ---

    fn named_dirs_fixture() -> HashMap<String, String> {
        let mut m = HashMap::new();
        m.insert("proj".to_string(), "/home/me/proj".to_string());
        m
    }

    #[test]
    fn expand_named_dir_slash_form() {
        let dirs = named_dirs_fixture();
        let td = tempfile::tempdir().unwrap();
        let result = expand_named_dir_with_cwd("proj/src/lib.rs", &dirs, td.path());
        assert_eq!(result, Some("/home/me/proj/src/lib.rs".to_string()));
    }

    #[test]
    fn expand_named_dir_colon_form() {
        let dirs = named_dirs_fixture();
        let td = tempfile::tempdir().unwrap();
        let result = expand_named_dir_with_cwd("proj:src/lib.rs", &dirs, td.path());
        assert_eq!(result, Some("/home/me/proj/src/lib.rs".to_string()));
    }

    #[test]
    fn expand_named_dir_tilde_form() {
        let dirs = named_dirs_fixture();
        let td = tempfile::tempdir().unwrap();
        let result = expand_named_dir_with_cwd("~proj/file", &dirs, td.path());
        assert_eq!(result, Some("/home/me/proj/file".to_string()));
    }

    #[test]
    fn expand_named_dir_bare_name_no_expand() {
        let dirs = named_dirs_fixture();
        let td = tempfile::tempdir().unwrap();
        let result = expand_named_dir_with_cwd("proj", &dirs, td.path());
        assert_eq!(result, None);
    }

    #[test]
    fn expand_named_dir_cwd_wins() {
        let _g = crate::test_util::CWD_LOCK.lock().unwrap();
        let dirs = named_dirs_fixture();
        let td = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(td.path().join("proj")).unwrap();
        // cwd has a "proj" directory — named dir should not expand
        let result = expand_named_dir_with_cwd("proj/file", &dirs, td.path());
        assert_eq!(result, None);
    }

    #[test]
    fn expand_named_dir_skips_urls() {
        let mut dirs = HashMap::new();
        dirs.insert("https".to_string(), "/some/path".to_string());
        dirs.insert("ssh".to_string(), "/other/path".to_string());
        let td = tempfile::tempdir().unwrap();
        assert_eq!(
            expand_named_dir_with_cwd("https://example.com", &dirs, td.path()),
            None
        );
        assert_eq!(
            expand_named_dir_with_cwd("ssh://host/path", &dirs, td.path()),
            None
        );
    }

    #[test]
    fn expand_named_dir_skips_ssh_target() {
        let mut dirs = HashMap::new();
        dirs.insert("host".to_string(), "/local/path".to_string());
        let td = tempfile::tempdir().unwrap();
        assert_eq!(
            expand_named_dir_with_cwd("user@host:/remote/path", &dirs, td.path()),
            None
        );
    }

    #[test]
    fn expand_named_dir_miss_returns_none() {
        let dirs = empty_named_dirs();
        let td = tempfile::tempdir().unwrap();
        assert_eq!(
            expand_named_dir_with_cwd("unknown/path", &dirs, td.path()),
            None
        );
    }

    #[test]
    fn expand_named_dir_empty_map_is_fast() {
        let dirs = empty_named_dirs();
        let td = tempfile::tempdir().unwrap();
        // Empty map short-circuits before any filesystem call
        assert_eq!(
            expand_named_dir_with_cwd("proj/src/main.rs", &dirs, td.path()),
            None
        );
    }

    // --- Tests for expand_dir_stack ---

    #[test]
    fn dir_stack_index_zero_returns_pwd() {
        let stack = vec![
            "/home/me".to_string(),
            "/tmp".to_string(),
            "/var".to_string(),
        ];
        // ~0 is PWD (index 0)
        assert_eq!(expand_dir_stack("~0", &stack), Some("/home/me".to_string()));
    }

    #[test]
    fn dir_stack_ancestor_ref() {
        let stack = vec![
            "/home/me".to_string(),
            "/tmp".to_string(),
            "/var".to_string(),
        ];
        assert_eq!(expand_dir_stack("~0", &stack), Some("/home/me".to_string()));
        assert_eq!(expand_dir_stack("~2", &stack), Some("/var".to_string()));
        assert_eq!(expand_dir_stack("~3", &stack), None);
        assert_eq!(expand_dir_stack("~-1", &stack), Some("/var".to_string()));
    }

    #[test]
    fn dir_stack_index_out_of_range() {
        let stack = vec!["/home/me".to_string()];
        assert_eq!(expand_dir_stack("~5", &stack), None);
    }

    #[test]
    fn dir_stack_plus_prefix_same_as_bare() {
        let stack = vec!["/home/me".to_string(), "/tmp".to_string()];
        assert_eq!(expand_dir_stack("~+1", &stack), Some("/tmp".to_string()));
    }

    #[test]
    fn dir_stack_with_tail() {
        let stack = vec!["/home/me".to_string()];
        // ~0/src should expand to /home/me/src
        assert_eq!(
            expand_dir_stack("~0/src", &stack),
            Some("/home/me/src".to_string())
        );
    }

    #[test]
    fn dir_stack_no_digits_returns_none() {
        let stack = vec!["/home/me".to_string()];
        // bare ~ or ~name (no digits) → None
        assert_eq!(expand_dir_stack("~", &stack), None);
        assert_eq!(expand_dir_stack("~name", &stack), None);
    }

    #[test]
    fn dir_stack_negative_out_of_range() {
        let stack = vec!["/home/me".to_string(), "/tmp".to_string()];
        // ~-3 when stack has 2 entries → index -1 → None
        assert_eq!(expand_dir_stack("~-3", &stack), None);
    }

    #[test]
    fn dir_stack_integration_with_resolve_path() {
        // resolve_path should expand ~0 to dir_stack[0] and then resolve from there
        let dir_stack = vec!["/tmp".to_string()];
        // /tmp itself should resolve (it exists), so ~0 → /tmp → Resolved or Unchanged
        let result = resolve_path("~0", &HashMap::new(), &dir_stack);
        // The expansion produces "/tmp" which is a real path: either Unchanged (already
        // fully resolved) or Resolved is acceptable.
        match result {
            PathResult::Unchanged | PathResult::Resolved(_) => {}
            other => panic!("expected Resolved or Unchanged for ~0, got {:?}", other),
        }
    }

    #[test]
    fn path_resolve_uses_named_dir() {
        // End-to-end: set up a tempdir as the mapped path, add a file, call
        // resolve_path with `proj:fil` where `proj → tempdir`, assert it
        // expands and the file-abbrev resolves.
        let td = tempfile::tempdir().unwrap();
        std::fs::write(td.path().join("file.rs"), "").unwrap();

        let mut dirs = HashMap::new();
        dirs.insert("proj".to_string(), td.path().to_str().unwrap().to_string());

        match resolve_path("proj:fil", &dirs, &empty_dir_stack()) {
            PathResult::Resolved(s) => {
                assert!(s.ends_with("file.rs"), "expected file.rs, got: {s}");
            }
            other => panic!("expected Resolved, got {:?}", other),
        }
    }
}
