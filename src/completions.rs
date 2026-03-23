use std::collections::HashMap;
use std::fs;
use std::path::Path;

use crate::trie::{self, ArgSpec, CommandTrie};

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

    let (subcmds, arg_specs) = extract_from_dirs(&fpath_dirs);
    for (cmd, subs) in &subcmds {
        for sub in subs {
            trie.insert(&[cmd.as_str(), sub.as_str()]);
            total += 1;
        }
    }

    // Populate both the new arg_specs and the legacy arg_modes (for compat)
    for (cmd, spec) in &arg_specs {
        if let Some(mode) = spec.rest {
            trie.arg_modes.insert(cmd.clone(), mode);
        }
    }
    trie.arg_specs.extend(arg_specs);
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

/// Extract subcommands and per-position argument specs from completion files.
/// Returns (command -> subcommands, command -> ArgSpec).
fn extract_from_dirs(dirs: &[String]) -> (HashMap<String, Vec<String>>, HashMap<String, ArgSpec>) {
    let mut subcmds: HashMap<String, Vec<String>> = HashMap::new();
    let mut arg_specs: HashMap<String, ArgSpec> = HashMap::new();

    for dir in dirs {
        let dir_path = Path::new(dir);
        let entries = match fs::read_dir(dir_path) {
            Ok(e) => e,
            Err(_) => continue,
        };

        for entry in entries.flatten() {
            let name = entry.file_name();
            let name = name.to_string_lossy();

            if !name.starts_with('_') || name.starts_with("__") {
                continue;
            }

            let cmd = &name[1..];
            if cmd.is_empty() || cmd.contains('.') {
                continue;
            }

            if is_internal_completion(cmd) {
                continue;
            }

            let file_path = entry.path();
            let real_path = match fs::canonicalize(&file_path) {
                Ok(p) => p,
                Err(_) => file_path,
            };

            if let Ok(content) = fs::read_to_string(&real_path) {
                let subs = extract_subcommands_from_content(cmd, &content);
                if !subs.is_empty() {
                    subcmds.entry(cmd.to_string()).or_default().extend(subs);
                }

                let commands = parse_compdef_commands(&content);
                let spec = parse_arg_spec(&content);
                if !spec.is_empty() {
                    for c in &commands {
                        arg_specs.insert(c.clone(), spec.clone());
                    }
                    if commands.is_empty() {
                        arg_specs.insert(cmd.to_string(), spec);
                    }
                }
            }
        }
    }

    for subs in subcmds.values_mut() {
        subs.sort();
        subs.dedup();
    }

    (subcmds, arg_specs)
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

/// Detect the argument type from a Zsh completion action string.
fn action_to_arg_type(action: &str) -> Option<u8> {
    let action = action.trim().trim_matches('\'').trim_matches('"');
    if action.contains("_command_names") || action.contains("_path_commands") {
        Some(trie::ARG_MODE_EXECS_ONLY)
    } else if action.contains("_directories")
        || action.contains("_files -/")
        || action.contains("_path_files -/")
        || (action.contains("_path_files") && action.contains("-/"))
    {
        Some(trie::ARG_MODE_DIRS_ONLY)
    } else if action.contains("_files") || action.contains("_path_files") {
        Some(trie::ARG_MODE_PATHS)
    } else if action.contains(":_commands") {
        Some(trie::ARG_MODE_EXECS_ONLY)
    } else {
        None
    }
}

/// Parse per-position and per-flag argument specs from a completion file.
///
/// Extracts from `_arguments` specs:
/// - `'N:desc:_files'` → position N expects files
/// - `'*:desc:_files'` → all remaining args expect files
/// - `'-f+:desc:_files'` → flag -f takes a file argument
/// - `'--flag=:desc:_files'` → flag --flag takes a file argument
fn parse_arg_spec(content: &str) -> ArgSpec {
    let mut spec = ArgSpec::default();

    for line in content.lines() {
        let trimmed = line.trim().trim_end_matches('\\').trim();
        if trimmed.starts_with('#') {
            continue;
        }

        // We're looking for _arguments spec strings.
        // These are single-quoted or double-quoted strings with colons separating
        // the spec parts: 'specifier:description:action'
        // The action (after the last colon) tells us what type of completion.

        // Extract quoted argument specs from the line
        for spec_str in extract_argument_specs(trimmed) {
            process_spec_string(&spec_str, &mut spec);
        }

        // Also catch bare _files/_directories calls used as direct actions
        // in non-_arguments style completions (e.g., `_diff_options ... ':file:_files'`)
        if !trimmed.contains("_arguments")
            && (trimmed.contains(":_files") || trimmed.contains(":_directories") || trimmed.contains(":_command"))
        {
            // Try to parse colon-separated specs in the line
            for part in trimmed.split_whitespace() {
                let part = part.trim_matches('\'').trim_matches('"');
                if part.contains(':') {
                    process_spec_string(part, &mut spec);
                }
            }
        }
    }

    spec
}

/// Extract argument spec strings from a line.
/// Looks for single-quoted strings that contain colons (argument specs).
fn extract_argument_specs(line: &str) -> Vec<String> {
    let mut specs = Vec::new();
    let mut chars = line.chars().peekable();

    while let Some(&ch) = chars.peek() {
        if ch == '\'' {
            chars.next(); // consume opening quote
            let mut s = String::new();
            while let Some(&c) = chars.peek() {
                if c == '\'' {
                    chars.next();
                    break;
                }
                s.push(c);
                chars.next();
            }
            // Only include strings that look like argument specs (contain colons
            // and an action we care about)
            if s.contains(':')
                && (s.contains("_files")
                    || s.contains("_directories")
                    || s.contains("_command")
                    || s.contains("_path_files")
                    || s.contains("_path_commands"))
            {
                specs.push(s);
            }
        } else {
            chars.next();
        }
    }

    specs
}

/// Process a single _arguments spec string and add to the ArgSpec.
fn process_spec_string(spec_str: &str, spec: &mut ArgSpec) {
    // Find the action: it's after the last colon that isn't inside brackets
    let action = match find_action_in_spec(spec_str) {
        Some(a) => a,
        None => return,
    };

    let arg_type = match action_to_arg_type(&action) {
        Some(t) => t,
        None => return,
    };

    let s = spec_str.trim();

    // Positional: starts with a digit or *
    if s.starts_with('*') {
        spec.rest = Some(arg_type);
        return;
    }

    if let Some(first_char) = s.chars().next()
        && first_char.is_ascii_digit()
            && let Ok(pos) = s.chars().take_while(|c| c.is_ascii_digit()).collect::<String>().parse::<u32>() {
                spec.positional.insert(pos, arg_type);
                return;
            }

    // Flag spec: starts with - or ( (exclusion group)
    // Extract flag names from patterns like:
    //   '-o+:desc:_files'  → flag "-o"
    //   '--output=:desc:_files'  → flag "--output"
    //   '(-f --flag)'{-f,--flag}':desc:_files' → flags "-f", "--flag"
    //   But we see the inner part after brace expansion, so we get:
    //   '-f:desc:_files' and '--flag:desc:_files'
    if s.starts_with('-') || s.starts_with('(') {
        let flags = extract_flags_from_spec(s);
        for flag in flags {
            spec.flag_args.insert(flag, arg_type);
        }
    }
}

/// Find the action (completion function) in a spec string.
/// The action is after the last `:` that's part of the argument description,
/// not inside brackets `[...]`.
fn find_action_in_spec(spec: &str) -> Option<String> {
    // Strategy: split on colons, but skip content inside []
    let mut parts: Vec<String> = Vec::new();
    let mut current = String::new();
    let mut bracket_depth: u32 = 0;

    for ch in spec.chars() {
        match ch {
            '[' => {
                bracket_depth += 1;
                current.push(ch);
            }
            ']' => {
                bracket_depth = bracket_depth.saturating_sub(1);
                current.push(ch);
            }
            ':' if bracket_depth == 0 => {
                parts.push(current.clone());
                current.clear();
            }
            _ => current.push(ch),
        }
    }
    parts.push(current);

    // The action is the last part
    let last = parts.last()?;
    let last = last.trim();
    if last.contains("_files")
        || last.contains("_directories")
        || last.contains("_command")
        || last.contains("_path_files")
        || last.contains("_path_commands")
    {
        Some(last.to_string())
    } else {
        None
    }
}

/// Extract flag names from a spec string.
fn extract_flags_from_spec(spec: &str) -> Vec<String> {
    let mut flags = Vec::new();
    let s = spec.trim();

    // Strip leading exclusion group: (...)
    let s = if s.starts_with('(') {
        if let Some(end) = s.find(')') {
            s[end + 1..].trim()
        } else {
            s
        }
    } else {
        s
    };

    // The flag is at the start, up to the first [ or :
    let flag_part: String = s
        .chars()
        .take_while(|c| *c != '[' && *c != ':')
        .collect();
    let flag_part = flag_part.trim();

    // Handle comma-separated alternatives inside braces: {-f,--flag}
    if flag_part.contains('{') && flag_part.contains('}')
        && let Some(start) = flag_part.find('{')
            && let Some(end) = flag_part.find('}') {
                let inner = &flag_part[start + 1..end];
                for part in inner.split(',') {
                    let f = part.trim().trim_end_matches('+').trim_end_matches('=');
                    if f.starts_with('-') {
                        flags.push(f.to_string());
                    }
                }
                return flags;
            }

    // Single flag: strip trailing + or =
    let flag = flag_part.trim_end_matches('+').trim_end_matches('=');
    if flag.starts_with('-') && !flag.is_empty() {
        flags.push(flag.to_string());
    }

    flags
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
    fn test_parse_arg_spec_rest_files() {
        let content = "#compdef cat\n_arguments '*: :_files'\n";
        let spec = parse_arg_spec(content);
        assert_eq!(spec.rest, Some(trie::ARG_MODE_PATHS));
    }

    #[test]
    fn test_parse_arg_spec_rest_dirs() {
        let content = "#compdef rmdir\n_arguments '*: :_directories'\n";
        let spec = parse_arg_spec(content);
        assert_eq!(spec.rest, Some(trie::ARG_MODE_DIRS_ONLY));
    }

    #[test]
    fn test_parse_arg_spec_rest_execs() {
        let content = "#compdef which\n_arguments '*:command:_command_names'\n";
        let spec = parse_arg_spec(content);
        assert_eq!(spec.rest, Some(trie::ARG_MODE_EXECS_ONLY));
    }

    #[test]
    fn test_parse_arg_spec_flag_with_file() {
        // -t takes a directory argument, * takes files
        let content = r#"#compdef cp
_arguments \
  '-t+[target directory]:target directory:_files -/' \
  '*:file or directory:_files'
"#;
        let spec = parse_arg_spec(content);
        assert_eq!(spec.rest, Some(trie::ARG_MODE_PATHS));
        assert_eq!(
            spec.flag_args.get("-t"),
            Some(&trie::ARG_MODE_DIRS_ONLY),
            "flag -t should expect directories"
        );
    }

    #[test]
    fn test_parse_arg_spec_positional() {
        let content = r#"#compdef diff
_arguments '1:original file:_files' '2:new file:_files'
"#;
        let spec = parse_arg_spec(content);
        assert_eq!(spec.positional.get(&1), Some(&trie::ARG_MODE_PATHS));
        assert_eq!(spec.positional.get(&2), Some(&trie::ARG_MODE_PATHS));
    }

    #[test]
    fn test_parse_arg_spec_empty() {
        let content = "#compdef git\n_arguments -S\n";
        let spec = parse_arg_spec(content);
        assert!(spec.is_empty());
    }

    #[test]
    fn test_parse_arg_spec_gcc_output_flag() {
        let content = r#"#compdef gcc
_arguments \
  '-o+:output file:_files' \
  '*:input file:_files'
"#;
        let spec = parse_arg_spec(content);
        assert_eq!(spec.flag_args.get("-o"), Some(&trie::ARG_MODE_PATHS));
        assert_eq!(spec.rest, Some(trie::ARG_MODE_PATHS));
    }
}
