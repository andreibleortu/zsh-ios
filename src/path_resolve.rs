use std::fs;
use std::path::{Path, PathBuf};

#[derive(Debug)]
pub enum PathResult {
    Resolved(String),
    /// Multiple full resolved paths -- caller should let user pick.
    Ambiguous(Vec<String>),
    Unchanged,
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
pub fn resolve_path(abbreviated: &str) -> PathResult {
    resolve_path_inner(abbreviated, false)
}

/// Like `resolve_path` but only matches directories (for cd, pushd, etc.).
pub fn resolve_path_dirs_only(abbreviated: &str) -> PathResult {
    resolve_path_inner(abbreviated, true)
}

fn resolve_path_inner(abbreviated: &str, dirs_only: bool) -> PathResult {
    if abbreviated.is_empty() {
        return PathResult::Unchanged;
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

    #[test]
    fn test_resolve_absolute() {
        let result = resolve_path("/usr/lo");
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
            match resolve_path("~/Desk") {
                PathResult::Resolved(s) => assert_eq!(s, "~/Desktop"),
                _ => panic!("Expected Resolved"),
            }
        }
    }

    #[test]
    fn test_exact_match_wins() {
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
        match resolve_path("zzzznonexistent") {
            PathResult::Unchanged => {}
            other => panic!("Expected Unchanged for nonexistent, got {:?}", other),
        }
    }

    #[test]
    fn test_resolve_path_empty() {
        match resolve_path("") {
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
}
