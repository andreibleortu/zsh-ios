use std::fs;
use std::path::{Path, PathBuf};

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

    match resolve_components(&base_dir, &components, init_parts) {
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

        match resolve_component(&current_dir, component) {
            ComponentMatch::Exact(name) | ComponentMatch::Unique(name) => {
                current_dir = current_dir.join(&name);
                resolved_parts.push(name);
            }
            ComponentMatch::Ambiguous(candidates) => {
                let remaining = &components[i + 1..];

                if remaining.is_empty() {
                    // Last component, no look-ahead possible -- give up
                    resolved_parts.push(component.to_string());
                    return ComponentsResult::Unchanged(resolved_parts);
                }

                // Find which candidates have children matching the next component
                let winners = deep_filter(&current_dir, &candidates, remaining);

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
                    match resolve_components(&child_dir, remaining, fork_parts) {
                        ComponentsResult::Resolved(parts)
                        | ComponentsResult::Unchanged(parts) => {
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

enum ComponentMatch {
    Exact(String),
    Unique(String),
    Ambiguous(Vec<String>),
    None,
}

fn resolve_component(dir: &Path, prefix: &str) -> ComponentMatch {
    let entries = list_dir(dir);

    if entries.iter().any(|e| e == prefix) {
        return ComponentMatch::Exact(prefix.to_string());
    }

    let cs_matches: Vec<&String> = entries.iter().filter(|e| e.starts_with(prefix)).collect();
    match cs_matches.len() {
        1 => return ComponentMatch::Unique(cs_matches[0].clone()),
        0 => {}
        _ => return ComponentMatch::Ambiguous(cs_matches.into_iter().cloned().collect()),
    }

    let lower_prefix = prefix.to_lowercase();
    let ci_matches: Vec<&String> = entries
        .iter()
        .filter(|e| e.to_lowercase().starts_with(&lower_prefix))
        .collect();
    match ci_matches.len() {
        1 => ComponentMatch::Unique(ci_matches[0].clone()),
        0 => ComponentMatch::None,
        _ => ComponentMatch::Ambiguous(ci_matches.into_iter().cloned().collect()),
    }
}

/// Filter ambiguous candidates by which ones have children matching the next component.
fn deep_filter(parent: &Path, candidates: &[String], remaining: &[String]) -> Vec<String> {
    if remaining.is_empty() {
        return candidates.to_vec();
    }
    let next = &remaining[0];
    if next.is_empty() {
        return candidates.to_vec();
    }
    let lower_next = next.to_lowercase();

    candidates
        .iter()
        .filter(|cand| {
            let child_dir = parent.join(cand);
            let entries = list_dir(&child_dir);
            entries.iter().any(|e| {
                e == next
                    || e.starts_with(next.as_str())
                    || e.to_lowercase().starts_with(&lower_next)
            })
        })
        .cloned()
        .collect()
}

fn list_dir(dir: &Path) -> Vec<String> {
    match fs::read_dir(dir) {
        Ok(entries) => entries
            .flatten()
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
        let result = resolve_component(&dir, "foo");
        match result {
            ComponentMatch::Exact(name) => assert_eq!(name, "foo"),
            _ => panic!("Expected Exact match"),
        }

        let _ = fs::remove_dir_all(&dir);
    }
}
