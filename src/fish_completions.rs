use crate::trie::{CommandTrie, ARG_MODE_PATHS};
use std::collections::HashMap;
use std::fs;
use std::path::Path;

/// Scan standard Fish completion locations and supplement trie.arg_specs
/// and trie.descriptions. Existing entries are preserved — Zsh's richer
/// per-command data wins when both sources cover the same command.
///
/// Returns (commands_enriched, subcommands_added, flags_added).
pub fn scan_fish_completions(trie: &mut CommandTrie) -> (u32, u32, u32) {
    let dirs = fish_dirs();
    scan_fish_dirs(trie, &dirs.iter().map(String::as_str).collect::<Vec<_>>())
}

fn fish_dirs() -> Vec<String> {
    let mut out = Vec::new();
    for p in &[
        "/usr/share/fish/completions",
        "/usr/share/fish/vendor_completions.d",
        "/usr/local/share/fish/completions",
        "/opt/homebrew/share/fish/completions",
    ] {
        if Path::new(p).is_dir() {
            out.push((*p).into());
        }
    }
    if let Some(h) = dirs::home_dir() {
        let user = h.join(".config/fish/completions");
        if user.is_dir() {
            out.push(user.to_string_lossy().into());
        }
        let user_vendor = h.join(".local/share/fish/vendor_completions.d");
        if user_vendor.is_dir() {
            out.push(user_vendor.to_string_lossy().into());
        }
    }
    out
}

fn scan_fish_dirs(trie: &mut CommandTrie, dirs: &[&str]) -> (u32, u32, u32) {
    // Collect all entries per command before merging so we can count accurately.
    // Key: cmd  Value: (args_to_insert, condition_subs -> flags, descriptions)
    let mut per_cmd: HashMap<String, ParsedCommand> = HashMap::new();

    for dir in dirs {
        let Ok(read_dir) = fs::read_dir(dir) else {
            continue;
        };
        for entry in read_dir.flatten() {
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) != Some("fish") {
                continue;
            }
            let Ok(content) = fs::read_to_string(&path) else {
                continue;
            };
            parse_fish_file(&content, &mut per_cmd);
        }
    }

    merge_into_trie(trie, per_cmd)
}

/// Intermediate representation for all data extracted from a single command's
/// Fish completion files before it is merged into the trie.
#[derive(Default)]
struct ParsedCommand {
    /// Top-level subcommands (from lines without a meaningful condition).
    top_args: Vec<String>,
    /// Descriptions for top-level subcommands/flags.
    top_descriptions: HashMap<String, String>,
    /// Sub-scoped data when `__fish_seen_subcommand_from X Y` is used.
    /// Key = parent subcommand name (e.g. "push"), Value = args under it.
    sub_args: HashMap<String, Vec<String>>,
    /// Descriptions that apply under a specific subcommand scope.
    sub_descriptions: HashMap<String, HashMap<String, String>>,
    /// Long/short flags at top level that take a value.
    top_flags: Vec<String>,
    /// Flags scoped under a subcommand.
    sub_flags: HashMap<String, Vec<String>>,
}

fn parse_fish_file(content: &str, per_cmd: &mut HashMap<String, ParsedCommand>) {
    // Join continuation lines (trailing backslash).
    let mut lines: Vec<String> = Vec::new();
    let mut pending = String::new();
    for raw in content.lines() {
        if let Some(stripped) = raw.strip_suffix('\\') {
            pending.push_str(stripped);
            pending.push(' ');
        } else {
            pending.push_str(raw);
            lines.push(std::mem::take(&mut pending));
        }
    }
    if !pending.is_empty() {
        lines.push(pending);
    }

    for line in &lines {
        let trimmed = line.trim_start();
        if trimmed.starts_with('#') || trimmed.is_empty() {
            continue;
        }
        if !trimmed.starts_with("complete") {
            continue;
        }
        // Require "complete " or "complete\t" — avoid false matches on words
        // starting with "complete" but not being the complete builtin.
        let after = trimmed.strip_prefix("complete").unwrap_or("");
        if !after.starts_with(|c: char| c.is_whitespace()) {
            continue;
        }
        if let Some(entry) = parse_complete_entry(trimmed) {
            let rec = per_cmd.entry(entry.cmd.clone()).or_default();
            if entry.condition_subs.is_empty() {
                // Top-level completion line.
                for arg in &entry.args {
                    if !rec.top_args.contains(arg) {
                        rec.top_args.push(arg.clone());
                    }
                    if let Some(ref desc) = entry.description {
                        rec.top_descriptions
                            .entry(arg.clone())
                            .or_insert_with(|| desc.clone());
                    }
                }
                if entry.takes_value {
                    if let Some(ref flag) = entry.long_flag
                        && !rec.top_flags.contains(flag)
                    {
                        rec.top_flags.push(flag.clone());
                    }
                    if let Some(ref flag) = entry.short_flag
                        && !rec.top_flags.contains(flag)
                    {
                        rec.top_flags.push(flag.clone());
                    }
                }
            } else {
                // Scoped under one or more parent subcommands.
                for parent_sub in &entry.condition_subs {
                    let sub_args = rec.sub_args.entry(parent_sub.clone()).or_default();
                    for arg in &entry.args {
                        if !sub_args.contains(arg) {
                            sub_args.push(arg.clone());
                        }
                    }
                    if let Some(ref desc) = entry.description {
                        let sub_descs = rec
                            .sub_descriptions
                            .entry(parent_sub.clone())
                            .or_default();
                        for arg in &entry.args {
                            sub_descs.entry(arg.clone()).or_insert_with(|| desc.clone());
                        }
                    }
                    if entry.takes_value {
                        let sub_flags = rec.sub_flags.entry(parent_sub.clone()).or_default();
                        if let Some(ref flag) = entry.long_flag
                            && !sub_flags.contains(flag)
                        {
                            sub_flags.push(flag.clone());
                        }
                        if let Some(ref flag) = entry.short_flag
                            && !sub_flags.contains(flag)
                        {
                            sub_flags.push(flag.clone());
                        }
                    }
                }
            }
        }
    }
}

fn merge_into_trie(
    trie: &mut CommandTrie,
    per_cmd: HashMap<String, ParsedCommand>,
) -> (u32, u32, u32) {
    let mut cmds_enriched: u32 = 0;
    let mut subs_added: u32 = 0;
    let mut flags_added: u32 = 0;

    for (cmd, parsed) in per_cmd {
        let mut did_something = false;

        // Insert top-level subcommands.
        for arg in &parsed.top_args {
            trie.insert(&[cmd.as_str(), arg.as_str()]);
            subs_added += 1;
            did_something = true;
        }

        // Descriptions for top-level subcommands (richest wins).
        for (sub, desc) in &parsed.top_descriptions {
            crate::trie::merge_description(
                &mut trie.descriptions,
                cmd.clone(),
                sub.clone(),
                desc.clone(),
            );
        }

        // Top-level value-taking flags.
        if !parsed.top_flags.is_empty() {
            let spec = trie
                .arg_specs
                .entry(cmd.clone())
                .or_default();
            for flag in &parsed.top_flags {
                let inserted = spec
                    .flag_args
                    .entry(flag.clone())
                    .or_insert(ARG_MODE_PATHS);
                let _ = inserted;
                flags_added += 1;
                did_something = true;
            }
        }

        // Sub-scoped args: insert as sub-subcommands of cmd -> parent_sub -> arg.
        for (parent_sub, sub_args) in &parsed.sub_args {
            for arg in sub_args {
                trie.insert(&[cmd.as_str(), parent_sub.as_str(), arg.as_str()]);
                subs_added += 1;
                did_something = true;
            }
        }

        // Descriptions for sub-scoped completions (richest wins).
        for (parent_sub, descs) in &parsed.sub_descriptions {
            let key = format!("{} {}", cmd, parent_sub);
            for (sub, desc) in descs {
                crate::trie::merge_description(
                    &mut trie.descriptions,
                    key.clone(),
                    sub.clone(),
                    desc.clone(),
                );
            }
        }

        // Flags scoped under a subcommand.
        for (parent_sub, flags) in &parsed.sub_flags {
            let key = format!("{} {}", cmd, parent_sub);
            let spec = trie
                .arg_specs
                .entry(key)
                .or_default();
            for flag in flags {
                spec.flag_args.entry(flag.clone()).or_insert(ARG_MODE_PATHS);
                flags_added += 1;
                did_something = true;
            }
        }

        if did_something {
            cmds_enriched += 1;
        }
    }

    (cmds_enriched, subs_added, flags_added)
}

/// Parse a single `complete ...` invocation line into a `FishEntry`.
/// Returns `None` if the line doesn't produce anything actionable.
fn parse_complete_entry(line: &str) -> Option<FishEntry> {
    let tokens = tokenize_complete_line(line);
    if tokens.is_empty() {
        return None;
    }

    let mut cmd: Option<String> = None;
    let mut args: Vec<String> = Vec::new();
    let mut long_flag: Option<String> = None;
    let mut short_flag: Option<String> = None;
    let mut description: Option<String> = None;
    let mut takes_value = false;
    let mut condition_subs: Vec<String> = Vec::new();

    let mut i = 0;
    while i < tokens.len() {
        let tok = &tokens[i];
        match tok.as_str() {
            "-c" | "--command" => {
                i += 1;
                if i < tokens.len() {
                    cmd = Some(tokens[i].clone());
                }
            }
            "-a" | "--arguments" => {
                i += 1;
                if i < tokens.len() {
                    // The value may be a space-separated list of completion words.
                    for word in tokens[i].split_whitespace() {
                        let w = word.trim();
                        if is_plausible_fish_arg(w) {
                            args.push(w.to_string());
                        }
                    }
                }
            }
            "-l" | "--long-option" => {
                i += 1;
                if i < tokens.len() {
                    long_flag = Some(tokens[i].clone());
                }
            }
            "-s" | "--short-option" => {
                i += 1;
                if i < tokens.len() {
                    short_flag = Some(tokens[i].clone());
                }
            }
            "-o" | "--old-option" => {
                // Treat like a long flag but rarely used; parse the value.
                i += 1;
                if i < tokens.len() && long_flag.is_none() {
                    long_flag = Some(tokens[i].clone());
                }
            }
            "-d" | "--description" => {
                i += 1;
                if i < tokens.len() {
                    description = Some(tokens[i].clone());
                }
            }
            "-n" | "--condition" => {
                i += 1;
                if i < tokens.len() {
                    condition_subs = extract_seen_subcommands(&tokens[i]);
                }
            }
            "-r" | "--require-parameter" | "-F" | "--force-files" => {
                takes_value = true;
            }
            // Flags that carry no value for us: -f, -k, -e, etc.
            _ => {}
        }
        i += 1;
    }

    let cmd = cmd?;
    // Skip lines that have neither subcommand args nor a named flag.
    if args.is_empty() && long_flag.is_none() && short_flag.is_none() {
        return None;
    }
    // Only record a flag entry when it takes a value.
    let effective_long = if takes_value { long_flag } else { None };
    let effective_short = if takes_value { short_flag } else { None };

    Some(FishEntry {
        cmd,
        args,
        long_flag: effective_long,
        short_flag: effective_short,
        description,
        takes_value,
        condition_subs,
    })
}

/// Extract the list of subcommand names from a `__fish_seen_subcommand_from X Y Z`
/// condition expression. Returns an empty Vec if the pattern is not matched.
fn extract_seen_subcommands(condition: &str) -> Vec<String> {
    let marker = "__fish_seen_subcommand_from";
    let Some(pos) = condition.find(marker) else {
        return Vec::new();
    };
    let after = condition[pos + marker.len()..].trim();
    // Collect whitespace-separated tokens until we hit a shell metacharacter or end.
    after
        .split_whitespace()
        .take_while(|t| !t.starts_with([';', '|', '&', ')']))
        .map(|t| t.to_string())
        .collect()
}

struct FishEntry {
    cmd: String,
    args: Vec<String>,
    long_flag: Option<String>,
    short_flag: Option<String>,
    description: Option<String>,
    takes_value: bool,
    condition_subs: Vec<String>,
}

/// Reject tokens that aren't real subcommand names. Fish completion scripts
/// frequently pass shell variable references (`$__podman_comp_results`),
/// subshell invocations, or brace expansions to `complete -a`; those are
/// resolved at runtime, and the literal token is useless as a static hint.
fn is_plausible_fish_arg(s: &str) -> bool {
    if s.is_empty() || s.len() >= 64 {
        return false;
    }
    let first = s.as_bytes()[0];
    if matches!(first, b'$' | b'(' | b'{' | b'"' | b'\'' | b'`' | b'*' | b'?') {
        return false;
    }
    if s.contains('$') || s.contains('(') || s.contains('`') {
        return false;
    }
    // Real subcommand/arg names are alphanumeric + a small set of punctuation.
    s.chars().all(|c| {
        c.is_ascii_alphanumeric()
            || matches!(c, '_' | '-' | '.' | '/' | ':' | '@' | '+' | '=' | ',')
    })
}

/// Tokenize a Fish `complete` invocation line, respecting single- and
/// double-quoted strings and backslash escapes.
fn tokenize_complete_line(line: &str) -> Vec<String> {
    let rest = line.trim_start();
    let rest = rest.strip_prefix("complete").unwrap_or(rest).trim_start();

    let mut out = Vec::new();
    let mut cur = String::new();
    let mut chars = rest.chars().peekable();
    let mut quote: Option<char> = None;

    while let Some(c) = chars.next() {
        match (c, quote) {
            ('\\', _) => {
                if let Some(&next) = chars.peek() {
                    cur.push(next);
                    chars.next();
                }
            }
            ('\'' | '"', None) => quote = Some(c),
            (c2, Some(q)) if c2 == q => quote = None,
            (c2, Some(_)) => cur.push(c2),
            (c2, None) if c2.is_whitespace() => {
                if !cur.is_empty() {
                    out.push(std::mem::take(&mut cur));
                }
            }
            (c2, None) => cur.push(c2),
        }
    }
    if !cur.is_empty() {
        out.push(cur);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::trie::CommandTrie;

    // --- tokenizer tests ---

    #[test]
    fn tokenize_simple() {
        let tokens = tokenize_complete_line("complete -c git -a 'push pull'");
        assert_eq!(tokens, vec!["-c", "git", "-a", "push pull"]);
    }

    #[test]
    fn tokenize_escaped_quote() {
        let tokens = tokenize_complete_line(r#"complete -c git -d "say \"hi\"""#);
        // The outer double-quote wraps `say "hi"` after escape processing.
        assert!(tokens.iter().any(|t| t.contains("hi")), "{:?}", tokens);
        let desc = tokens.iter().find(|t| t.contains("say")).unwrap();
        assert!(desc.contains("hi"), "expected inner quote preserved: {:?}", desc);
    }

    #[test]
    fn tokenize_double_quoted() {
        let tokens = tokenize_complete_line(r#"complete -c foo -d "hello world""#);
        assert_eq!(tokens, vec!["-c", "foo", "-d", "hello world"]);
    }

    // --- parse_complete_entry tests ---

    #[test]
    fn parse_complete_with_args_and_desc() {
        let entry = parse_complete_entry("complete -c git -a 'push pull' -d 'Git cmd'").unwrap();
        assert_eq!(entry.cmd, "git");
        assert_eq!(entry.args, vec!["push", "pull"]);
        assert_eq!(entry.description.as_deref(), Some("Git cmd"));
        assert!(entry.condition_subs.is_empty());
        assert!(!entry.takes_value);
    }

    #[test]
    fn parse_complete_long_flag_with_value() {
        let entry =
            parse_complete_entry("complete -c x -l color -r -d 'Output color'").unwrap();
        assert_eq!(entry.long_flag.as_deref(), Some("color"));
        assert!(entry.takes_value);
        assert_eq!(entry.description.as_deref(), Some("Output color"));
    }

    #[test]
    fn parse_complete_seen_subcommand_condition() {
        let entry = parse_complete_entry(
            "complete -c git -n '__fish_seen_subcommand_from push pull' -l force",
        )
        .unwrap();
        assert_eq!(entry.condition_subs, vec!["push", "pull"]);
        // long_flag present but takes_value is false => effective_long is None in entry,
        // but we still detect the condition correctly.
        assert_eq!(entry.cmd, "git");
    }

    // --- trie merge tests ---

    #[test]
    fn scan_fish_merges_into_trie() {
        let dir = tempfile::tempdir().unwrap();
        let fish_file = dir.path().join("foo.fish");
        std::fs::write(
            &fish_file,
            "complete -c foo -a 'bar baz' -d 'Foo subcommands'\n\
             complete -c foo -a 'qux'\n",
        )
        .unwrap();

        let mut trie = CommandTrie::new();
        let (cmds, subs, _flags) =
            scan_fish_dirs(&mut trie, &[dir.path().to_str().unwrap()]);

        assert!(cmds >= 1, "expected at least 1 command enriched");
        assert!(subs >= 3, "expected bar, baz, qux added; got {}", subs);

        let foo_node = trie.root.get_child("foo").expect("foo not in trie");
        assert!(foo_node.get_child("bar").is_some(), "bar missing");
        assert!(foo_node.get_child("baz").is_some(), "baz missing");
        assert!(foo_node.get_child("qux").is_some(), "qux missing");
    }

    #[test]
    fn scan_fish_description_longest_wins() {
        let dir = tempfile::tempdir().unwrap();
        let fish_file = dir.path().join("foo.fish");
        std::fs::write(
            &fish_file,
            "complete -c foo -a bar -d 'from fish longer description'\ncomplete -c foo -a baz -d 'short'\n",
        )
        .unwrap();

        let mut trie = CommandTrie::new();
        // "bar": zsh has short desc, fish has longer → fish wins.
        trie.descriptions
            .entry("foo".into())
            .or_default()
            .insert("bar".into(), "short".into());
        // "baz": zsh has longer desc, fish has shorter → zsh wins.
        trie.descriptions
            .entry("foo".into())
            .or_default()
            .insert("baz".into(), "a longer description from zsh".into());

        scan_fish_dirs(&mut trie, &[dir.path().to_str().unwrap()]);

        let desc_bar = trie.descriptions.get("foo").and_then(|m| m.get("bar")).map(String::as_str);
        assert_eq!(desc_bar, Some("from fish longer description"), "fish longer desc should win for bar");

        let desc_baz = trie.descriptions.get("foo").and_then(|m| m.get("baz")).map(String::as_str);
        assert_eq!(desc_baz, Some("a longer description from zsh"), "zsh longer desc should be kept for baz");
    }

    #[test]
    fn skip_shell_var_reference_arg() {
        // podman.fish does: complete -c podman -a '$__podman_comp_results'
        // The literal string isn't a real subcommand; filter it out.
        let mut per = HashMap::new();
        parse_fish_file(
            "complete -c podman -a '$__podman_comp_results'\ncomplete -c podman -a build\n",
            &mut per,
        );
        let rec = per.get("podman").expect("podman record");
        assert_eq!(rec.top_args, vec!["build"]);
    }

    #[test]
    fn skip_command_substitution_arg() {
        let mut per = HashMap::new();
        parse_fish_file(
            "complete -c foo -a '(bar baz)'\ncomplete -c foo -a 'real'\n",
            &mut per,
        );
        let rec = per.get("foo").expect("foo record");
        assert_eq!(rec.top_args, vec!["real"]);
    }
}
