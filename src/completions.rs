use std::collections::HashMap;
use std::fs;
use std::path::Path;

use crate::trie::{self, CommandTrie};

/// Scan Zsh completion files for subcommand definitions and argument modes,
/// adding them to the trie.
///
/// Parses `_cmd-subcmd` function patterns from completion files (e.g.,
/// `_git-checkout` in `_git` means `checkout` is a subcommand of `git`).
///
/// Also extracts argument type actions (`_files`, `_directories`,
/// `_command_names`, etc.) to determine what kind of arguments each
/// command expects (paths, directories only, executables, etc.).
pub fn scan_completions(trie: &mut CommandTrie) -> u32 {
    let fpath_dirs = completion_dirs();
    let mut total = 0u32;

    let (subcmds, arg_modes) = extract_from_dirs(&fpath_dirs);
    for (cmd, subs) in &subcmds {
        for sub in subs {
            trie.insert(&[cmd.as_str(), sub.as_str()]);
            total += 1;
        }
    }

    trie.arg_modes.extend(arg_modes);
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

/// Extract subcommands and argument modes from completion files.
/// Returns (command -> subcommands, command -> arg_mode).
fn extract_from_dirs(dirs: &[String]) -> (HashMap<String, Vec<String>>, HashMap<String, u8>) {
    let mut subcmds: HashMap<String, Vec<String>> = HashMap::new();
    let mut arg_modes: HashMap<String, u8> = HashMap::new();

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
                    subcmds.entry(cmd.to_string()).or_default().extend(subs);
                }

                // Extract which commands this file covers and their arg mode
                let commands = parse_compdef_commands(&content);
                if let Some(mode) = detect_arg_mode(&content) {
                    for c in &commands {
                        arg_modes.insert(c.clone(), mode);
                    }
                    // If no #compdef, use the filename-derived command
                    if commands.is_empty() {
                        arg_modes.insert(cmd.to_string(), mode);
                    }
                }
            }
        }
    }

    // Deduplicate subcommands
    for subs in subcmds.values_mut() {
        subs.sort();
        subs.dedup();
    }

    (subcmds, arg_modes)
}

/// Parse the `#compdef` header to get the list of commands this file covers.
/// e.g., `#compdef cd chdir pushd` → ["cd", "chdir", "pushd"]
fn parse_compdef_commands(content: &str) -> Vec<String> {
    for line in content.lines() {
        let trimmed = line.trim();
        if let Some(rest) = trimmed.strip_prefix("#compdef ") {
            return rest
                .split_whitespace()
                .filter(|w| !w.starts_with('-'))
                .map(String::from)
                .collect();
        }
        // #compdef must be in the first few lines
        if !trimmed.is_empty() && !trimmed.starts_with('#') {
            break;
        }
    }
    vec![]
}

/// Detect the dominant argument mode from a completion file's content.
///
/// Scans for Zsh completion actions:
/// - `_directories`, `_path_files -/`, `_files -/` → DirsOnly
/// - `_files` (without -/) → Paths
/// - `_command_names`, `_path_commands`, `:_commands` → ExecsOnly
///
/// Returns the most specific mode found. DirsOnly and ExecsOnly are
/// more specific than Paths; Paths is more specific than Normal.
fn detect_arg_mode(content: &str) -> Option<u8> {
    let mut has_files = false;
    let mut has_dirs = false;
    let mut has_execs = false;

    for line in content.lines() {
        let trimmed = line.trim();

        // Skip comments
        if trimmed.starts_with('#') {
            continue;
        }

        // Directories only
        if trimmed.contains("_directories")
            || trimmed.contains("_path_files -/")
            || trimmed.contains("_path_files -W") && trimmed.contains("-/")
            || trimmed.contains("_files -/")
        {
            has_dirs = true;
        }

        // General files (only count patterns that are clearly argument specs,
        // not internal helper calls)
        if trimmed.contains(":_files")
            || trimmed.contains(": :_files")
            || trimmed.contains("_path_files") && !trimmed.contains("-/")
        {
            has_files = true;
        }

        // Executable/command arguments
        if trimmed.contains("_command_names")
            || trimmed.contains("_path_commands")
            || trimmed.contains(":_commands")
        {
            has_execs = true;
        }
    }

    // DirsOnly and ExecsOnly are exclusive — if a file has _directories but
    // no _files, it's DirsOnly. If it has _command_names, it's ExecsOnly.
    // If it has both _files and _directories, call it Paths (more general).
    if has_execs && !has_files && !has_dirs {
        Some(trie::ARG_MODE_EXECS_ONLY)
    } else if has_dirs && !has_files {
        Some(trie::ARG_MODE_DIRS_ONLY)
    } else if has_files || has_dirs {
        Some(trie::ARG_MODE_PATHS)
    } else {
        None
    }
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

    #[test]
    fn test_parse_compdef_commands() {
        assert_eq!(
            parse_compdef_commands("#compdef cd chdir pushd\n"),
            vec!["cd", "chdir", "pushd"]
        );
        assert_eq!(
            parse_compdef_commands("#compdef rm grm zf_rm\n"),
            vec!["rm", "grm", "zf_rm"]
        );
        assert_eq!(
            parse_compdef_commands("# just a comment\nsome code\n"),
            Vec::<String>::new()
        );
    }

    #[test]
    fn test_detect_arg_mode_files() {
        let content = "#compdef cat\n_arguments '*: :_files'\n";
        assert_eq!(detect_arg_mode(content), Some(trie::ARG_MODE_PATHS));
    }

    #[test]
    fn test_detect_arg_mode_dirs() {
        let content = "#compdef rmdir\n_arguments '*: :_directories'\n";
        assert_eq!(detect_arg_mode(content), Some(trie::ARG_MODE_DIRS_ONLY));
    }

    #[test]
    fn test_detect_arg_mode_execs() {
        let content = "#compdef which\n_arguments '*:command:_command_names'\n";
        assert_eq!(detect_arg_mode(content), Some(trie::ARG_MODE_EXECS_ONLY));
    }

    #[test]
    fn test_detect_arg_mode_dirs_with_path_files() {
        // _path_files -/ means directories only
        let content = "#compdef cd\n_path_files -W tmpcdpath -/\n_path_files -/\n";
        assert_eq!(detect_arg_mode(content), Some(trie::ARG_MODE_DIRS_ONLY));
    }

    #[test]
    fn test_detect_arg_mode_mixed_files_and_dirs() {
        // If both _files and _directories appear, Paths wins (more general)
        let content = "#compdef cp\n'*:file or directory:_files'\n'-t:target directory:_directories'\n";
        assert_eq!(detect_arg_mode(content), Some(trie::ARG_MODE_PATHS));
    }

    #[test]
    fn test_detect_arg_mode_none() {
        let content = "#compdef git\n_arguments -S\n";
        assert_eq!(detect_arg_mode(content), None);
    }
}
