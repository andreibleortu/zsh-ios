use std::collections::HashMap;
use std::fs;
use std::path::Path;

use crate::trie::CommandTrie;

/// Scan Zsh completion files for subcommand definitions and add them to the trie.
///
/// Parses `_cmd-subcmd` function patterns from completion files (e.g.,
/// `_git-checkout` in `_git` means `checkout` is a subcommand of `git`).
pub fn scan_completions(trie: &mut CommandTrie) -> u32 {
    let fpath_dirs = completion_dirs();
    let mut total = 0u32;

    let subcmds = extract_subcommands_from_dirs(&fpath_dirs);
    for (cmd, subs) in &subcmds {
        for sub in subs {
            trie.insert(&[cmd.as_str(), sub.as_str()]);
            total += 1;
        }
    }

    total
}

fn completion_dirs() -> Vec<String> {
    let mut dirs = Vec::new();

    // Standard Zsh completion directories
    for pattern in &[
        "/usr/share/zsh/*/functions",
        "/usr/local/share/zsh/site-functions",
        "/opt/homebrew/share/zsh/site-functions",
    ] {
        if let Ok(entries) = glob_simple(pattern) {
            dirs.extend(entries);
        }
    }

    // Also check $fpath from environment if available
    if let Ok(fpath) = std::env::var("FPATH") {
        for dir in fpath.split(':') {
            if !dir.is_empty() && !dirs.contains(&dir.to_string()) {
                dirs.push(dir.to_string());
            }
        }
    }

    dirs
}

/// Extract subcommands from completion files in the given directories.
/// Returns a map of command -> list of subcommands.
fn extract_subcommands_from_dirs(dirs: &[String]) -> HashMap<String, Vec<String>> {
    let mut result: HashMap<String, Vec<String>> = HashMap::new();

    for dir in dirs {
        let dir_path = Path::new(dir);
        let entries = match fs::read_dir(dir_path) {
            Ok(e) => e,
            Err(_) => continue,
        };

        for entry in entries.flatten() {
            let name = entry.file_name();
            let name = name.to_string_lossy();

            // Completion files start with _
            if !name.starts_with('_') || name.starts_with("__") {
                continue;
            }

            let cmd = &name[1..]; // strip leading _
            if cmd.is_empty() || cmd.contains('.') {
                continue;
            }

            // Skip internal Zsh completion helpers
            if is_internal_completion(cmd) {
                continue;
            }

            let file_path = entry.path();

            // Follow symlinks to get the real file
            let real_path = match fs::canonicalize(&file_path) {
                Ok(p) => p,
                Err(_) => file_path,
            };

            if let Ok(content) = fs::read_to_string(&real_path) {
                let subs = extract_subcommands_from_content(cmd, &content);
                if !subs.is_empty() {
                    result.entry(cmd.to_string()).or_default().extend(subs);
                }
            }
        }
    }

    // Deduplicate
    for subs in result.values_mut() {
        subs.sort();
        subs.dedup();
    }

    result
}

/// Extract subcommands from a completion file's content.
/// Looks for patterns like `_git-checkout`, `_docker-build`, etc.
fn extract_subcommands_from_content(cmd: &str, content: &str) -> Vec<String> {
    let mut subs = Vec::new();
    let prefix = format!("_{}-", cmd);

    for line in content.lines() {
        // Pattern: (( $+functions[_cmd-subcmd] ))
        if let Some(start) = line.find(&prefix) {
            let after = &line[start + prefix.len()..];
            // Extract the subcmd name (alphanumeric, hyphens, underscores)
            let subcmd: String = after
                .chars()
                .take_while(|c| c.is_alphanumeric() || *c == '-' || *c == '_')
                .collect();
            if !subcmd.is_empty() && subcmd.len() < 40 {
                subs.push(subcmd);
            }
        }
    }

    subs.sort();
    subs.dedup();
    subs
}

fn is_internal_completion(name: &str) -> bool {
    matches!(
        name,
        "arguments"
            | "values"
            | "alternative"
            | "describe"
            | "all_labels"
            | "all_matches"
            | "approximate"
            | "cache_invalid"
            | "call_function"
            | "combination"
            | "command_names"
            | "complete"
            | "completion"
            | "configure"
            | "default"
            | "dispatch"
            | "equal"
            | "expand"
            | "extensions"
            | "file_descriptors"
            | "files"
            | "guard"
            | "have_glob_qual"
            | "history"
            | "ignored"
            | "list"
            | "main_complete"
            | "message"
            | "multi_parts"
            | "next_label"
            | "normal"
            | "oldlist"
            | "parameters"
            | "path_files"
            | "pick_variant"
            | "prefix"
            | "regex_arguments"
            | "regex_words"
            | "requested"
            | "retrieve_cache"
            | "sep_parts"
            | "sequence"
            | "set_command"
            | "setup"
            | "store_cache"
            | "style"
            | "sub_command"
            | "suffix"
            | "tags"
            | "user_expand"
            | "wanted"
    )
}

/// Simple glob that expands `*` in a single path component.
fn glob_simple(pattern: &str) -> Result<Vec<String>, std::io::Error> {
    let mut results = Vec::new();

    if let Some(star_pos) = pattern.find('*') {
        let parent = &pattern[..pattern[..star_pos].rfind('/').unwrap_or(0)];
        let suffix = &pattern[pattern[star_pos..].find('/').map(|p| star_pos + p).unwrap_or(pattern.len())..];

        if let Ok(entries) = fs::read_dir(parent) {
            for entry in entries.flatten() {
                let path = entry.path();
                if path.is_dir() {
                    let candidate = format!("{}{}", path.display(), suffix);
                    if Path::new(&candidate).exists() {
                        results.push(candidate);
                    }
                }
            }
        }
    } else if Path::new(pattern).exists() {
        results.push(pattern.to_string());
    }

    Ok(results)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_extract_subcommands() {
        let content = r#"
(( $+functions[_git-add] )) ||
_git-add () {
  local curcontext=$curcontext state line ret=1
}

(( $+functions[_git-checkout] )) ||
_git-checkout () {
}

(( $+functions[_git-commit] )) ||
_git-commit () {
}
"#;
        let subs = extract_subcommands_from_content("git", content);
        assert_eq!(subs, vec!["add", "checkout", "commit"]);
    }

    #[test]
    fn test_extract_no_match() {
        let content = "some random content\n_arguments -S\n";
        let subs = extract_subcommands_from_content("foo", content);
        assert!(subs.is_empty());
    }
}
