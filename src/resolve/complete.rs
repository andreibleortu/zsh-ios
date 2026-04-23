//! `?` key completion — the secondary widget path.
//!
//! Invoked by the Zsh plugin's help widget. This builds either the per-
//! command subcommand list (`git ?`), the per-flag argument description
//! (`git checkout -?`), or a runtime argument list (branches, hosts, etc.)
//! via `runtime_complete`. Shares the engine's arg-spec lookup + context
//! rules so what `?` shows matches what resolution would accept.

use crate::pins::Pins;
use crate::runtime_complete;
use crate::trie::{self, CommandTrie, TrieNode};

use super::engine::*;

pub fn complete(input: &str, trie: &CommandTrie, pins: &Pins) -> String {
    // Leading `!` is a hands-off marker (see `starts_with_bang`). Produce no
    // completions so the shell's native completion (or history expansion)
    // gets a clean look.
    if starts_with_bang(input) {
        return String::new();
    }

    // Use only the last segment after any pipe/chain operator.
    // Preserve trailing whitespace — it tells complete_segment whether the user
    // has finished the current word (trailing space) or is still typing it.
    let parts = split_on_operators(input);
    let last_cmd = parts
        .iter()
        .rev()
        .find_map(|p| match p {
            LinePart::Command(c) => Some(c.trim_start()),
            _ => None,
        })
        .unwrap_or(input.trim_start());

    complete_segment(last_cmd, trie, pins)
}

pub(super) fn complete_segment(input: &str, trie: &CommandTrie, pins: &Pins) -> String {
    let words: Vec<&str> = input.split_whitespace().collect();
    // Trailing whitespace means the user has finished the current word and is
    // starting a new one — the completion prefix for the next word is empty.
    let completing_next = input.ends_with(' ') || input.ends_with('\t');
    let mut output = String::new();

    if words.is_empty() {
        // Show top-level commands sorted by usage count
        let mut entries: Vec<(&str, &TrieNode)> = trie.root.prefix_search("");
        entries.sort_by(|a, b| b.1.count.cmp(&a.1.count).then(a.0.cmp(b.0)));
        let names: Vec<&str> = entries.iter().map(|(n, _)| *n).collect();
        output.push_str("% Possible commands:\n");
        output.push_str(&format_columns(&names, 80));
        return output;
    }

    // Determine which words are "completed" (fully typed) vs the prefix being completed.
    // completed_words: words that are done (user has moved past them)
    // prefix: the partial word being completed (empty if user is starting a fresh word)
    let (completed_words, prefix): (Vec<&str>, &str) = if completing_next {
        (words.clone(), "")
    } else if words.len() == 1 {
        // Single word, no trailing space → completing at root level
        (vec![], words[0])
    } else {
        (words[..words.len() - 1].to_vec(), words[words.len() - 1])
    };

    // If no completed words, just search the root
    if completed_words.is_empty() {
        let mut matches = trie.root.prefix_search(prefix);
        if matches.is_empty() {
            output.push_str(&format!("% No commands matching \"{}\"\n", prefix));
        } else {
            matches.sort_by(|a, b| b.1.count.cmp(&a.1.count).then(a.0.cmp(b.0)));
            let names: Vec<&str> = matches.iter().map(|(n, _)| *n).collect();
            output.push_str("% Possible commands:\n");
            output.push_str(&format_columns(&names, 80));
        }
        return output;
    }

    // Resolve completed words to build the resolved command prefix and walk trie
    let mut resolved_words: Vec<String> = Vec::new();
    let resolved_cmd = resolve_first_word(completed_words[0], trie);
    resolved_words.push(resolved_cmd.clone());

    // Check pins first
    let pin_result = pins.longest_match(&completed_words);
    let (pin_consumed, expanded_prefix) = match pin_result {
        Some((consumed, expanded)) => (consumed, expanded),
        None => (0, vec![]),
    };

    // Walk the trie through completed words
    let mut node = &trie.root;
    let resolve_start;

    if pin_consumed > 0 {
        resolved_words = expanded_prefix.clone();
        for w in &expanded_prefix {
            match node.get_child(w) {
                Some(child) => node = child,
                None => break,
            }
        }
        resolve_start = pin_consumed;
    } else {
        if let Some(child) = node.get_child(&resolved_cmd) {
            node = child;
        }
        resolve_start = 1;
    }

    // Walk remaining completed words
    for word in &completed_words[resolve_start..] {
        if let Some(child) = node.get_child(word) {
            resolved_words.push(word.to_string());
            node = child;
            continue;
        }
        let matches = node.prefix_search(word);
        match matches.len() {
            1 => {
                resolved_words.push(matches[0].0.to_string());
                node = matches[0].1;
            }
            0 => {
                resolved_words.push(word.to_string());
                break;
            }
            _ => {
                // Intermediate word is ambiguous — show its completions
                let names: Vec<&str> = matches.iter().map(|(n, _)| *n).collect();
                output.push_str("% Possible completions:\n");
                output.push_str(&format_columns(&names, 80));
                return output;
            }
        }
    }

    // Determine the arg type at the current position
    let (spec, cmd_words) = lookup_arg_spec(
        &resolved_words.iter().map(String::from).collect::<Vec<_>>(),
        &trie.arg_specs,
    );
    let fallback_mode = arg_mode(&resolved_cmd, &trie.arg_modes);
    // Position of the word being completed (1-indexed)
    let total_words = completed_words.len() + 1; // completed + the word being typed
    let arg_position = total_words.saturating_sub(cmd_words).max(1) as u32;
    let prev_word = completed_words.last().copied();
    let current_mode = {
        let base = arg_type_for_word(arg_position, prev_word, spec, fallback_mode);
        apply_context_rules(spec, &resolved_words, base)
    };

    // --- Flag completion mode ---
    // When typing a flag prefix (starts with '-'), show known flags + their expected arg types.
    if prefix.starts_with('-') {
        return complete_flags(prefix, spec, node, output);
    }

    // --- Trie-based completion (subcommands) ---
    // _call_program: flag value or rest is produced by running an external command.
    // Check this before the trie so we show live dynamic values (e.g. ssh -Q cipher).
    if let Some(prev) = prev_word
        && prev.starts_with('-')
        && let Some((tag, argv)) = spec.and_then(|s| s.flag_call_programs.get(prev))
    {
        output.push_str(&format!("% Expects: <{}>\n", tag));
        let results = runtime_complete::call_program_cached(argv, prefix);
        if !results.is_empty() {
            let names: Vec<&str> = results.iter().map(String::as_str).collect();
            output.push_str(&format_columns(&names, 80));
        } else if !prefix.is_empty() {
            output.push_str(&format!("% No matches for \"{}\"\n", prefix));
        }
        return output;
    }

    // Static list: flag value is a literal enumeration (compadd - yes no, _values, etc.)
    if let Some(prev) = prev_word
        && prev.starts_with('-')
        && let Some(items) = spec.and_then(|s| s.flag_static_lists.get(prev))
    {
        output.push_str("% Expects: <value>\n");
        let filtered: Vec<&str> = items
            .iter()
            .filter(|i| prefix.is_empty() || i.starts_with(prefix))
            .map(String::as_str)
            .collect();
        if !filtered.is_empty() {
            output.push_str(&format_columns(&filtered, 80));
        } else if !prefix.is_empty() {
            output.push_str(&format!("% No matches for \"{}\"\n", prefix));
        }
        return output;
    }

    // Rest position with call_program (and not completing a subcommand / flag)
    let prev_is_flag_consuming =
        prev_word.is_some_and(|p| p.starts_with('-') && spec.is_some_and(|s| s.flag_takes_value(p)));
    if !prefix.starts_with('-')
        && !prev_is_flag_consuming
        && let Some((tag, argv)) = spec.and_then(|s| s.rest_call_program.as_ref())
    {
        let results = runtime_complete::call_program_cached(argv, prefix);
        if !results.is_empty() {
            output.push_str(&format!("% Expects: <{}>\n", tag));
            let names: Vec<&str> = results.iter().map(String::as_str).collect();
            output.push_str(&format_columns(&names, 80));
            return output;
        }
    }

    // Rest position with static list
    if !prefix.starts_with('-')
        && !prev_is_flag_consuming
        && let Some(items) = spec.and_then(|s| s.rest_static_list.as_ref())
    {
        let filtered: Vec<&str> = items
            .iter()
            .filter(|i| prefix.is_empty() || i.starts_with(prefix))
            .map(String::as_str)
            .collect();
        if !filtered.is_empty() {
            output.push_str("% Expects: <value>\n");
            output.push_str(&format_columns(&filtered, 80));
            return output;
        }
    }

    // Skip trie when we're completing the value of a flag that takes a typed
    // argument (e.g. `sudo -u <user>`).  The trie children here are learned
    // prior invocations of the command, not values for this flag.
    let in_flag_value_context = prev_word
        .is_some_and(|p| p.starts_with('-') && spec.is_some_and(|s| s.flag_takes_value(p)));

    let trie_matches = if in_flag_value_context {
        vec![]
    } else {
        node.prefix_search(prefix)
    };

    if trie_matches.is_empty() {
        // No trie matches — show type-aware completions based on arg spec
        show_type_completions(&mut output, current_mode, prefix, spec, arg_position);
    } else {
        // Separate subcommands from flags (flags from history are trie children too)
        let subcmds: Vec<(&str, &TrieNode)> = trie_matches
            .iter()
            .filter(|(n, _)| !n.starts_with('-'))
            .copied()
            .collect();
        let flag_matches: Vec<(&str, &TrieNode)> = trie_matches
            .iter()
            .filter(|(n, _)| n.starts_with('-'))
            .copied()
            .collect();

        if !subcmds.is_empty() {
            let mut sorted = subcmds.clone();
            sorted.sort_by(|a, b| b.1.count.cmp(&a.1.count).then(a.0.cmp(b.0)));

            // Try to show descriptions for subcommands (Cisco IOS style)
            let cmd_key = resolved_words.join(" ");
            let descs = trie.descriptions.get(&cmd_key);

            output.push_str("% Possible subcommands:\n");
            if descs.is_some_and(|d| !d.is_empty()) && sorted.len() <= 40 {
                let descs = descs.unwrap();
                let col_width = sorted.iter().map(|(n, _)| n.len()).max().unwrap_or(0) + 2;
                for (name, _) in &sorted {
                    if let Some(desc) = descs.get(*name) {
                        output.push_str(&format!("  {:<width$}{}\n", name, desc, width = col_width));
                    } else {
                        output.push_str(&format!("  {}\n", name));
                    }
                }
            } else {
                let names: Vec<&str> = sorted.iter().map(|(n, _)| *n).collect();
                output.push_str(&format_columns(&names, 80));
            }
        }

        if !flag_matches.is_empty() {
            if subcmds.is_empty() {
                output.push_str("% Possible flags:\n");
            } else {
                output.push_str("% Flags:\n");
            }
            output.push_str(&format_flags_from_trie(&flag_matches, spec));
        }

        // Type hint when completing next (empty prefix) and type is known
        if prefix.is_empty() && !matches!(current_mode, ArgMode::Normal | ArgMode::ExecsOnly) {
            let type_hint = match current_mode {
                ArgMode::DirsOnly => Some("<directory>"),
                ArgMode::Paths => Some("<file>"),
                ArgMode::Runtime(type_id) => Some(runtime_complete::type_hint(type_id)),
                _ => None,
            };
            if let Some(hint) = type_hint {
                output.push_str(&format!("  (also accepts: {})\n", hint));
            }
        }
    }

    output
}

/// Complete a flag prefix: show matching flags from spec and trie.
/// If the prefix exactly matches a single flag that takes an argument, show what it expects.
pub(super) fn complete_flags(
    prefix: &str,
    spec: Option<&trie::ArgSpec>,
    node: &TrieNode,
    mut output: String,
) -> String {
    // Collect flags from ArgSpec (flags that take typed arguments)
    let mut known_flags: Vec<(String, Option<u8>)> = Vec::new();
    if let Some(spec) = spec {
        for (flag, &arg_type) in &spec.flag_args {
            if flag.starts_with(prefix) {
                known_flags.push((flag.clone(), Some(arg_type)));
            }
        }
        // Also include _call_program flags (they take a value but the type is dynamic)
        for flag in spec.flag_call_programs.keys() {
            if flag.starts_with(prefix) && !known_flags.iter().any(|(f, _)| f == flag) {
                known_flags.push((flag.clone(), None));
            }
        }
        // Also include static list flags
        for flag in spec.flag_static_lists.keys() {
            if flag.starts_with(prefix) && !known_flags.iter().any(|(f, _)| f == flag) {
                known_flags.push((flag.clone(), None));
            }
        }
    }

    // Collect flags from trie children (flags learned from history — may be boolean)
    let trie_flags: Vec<&str> = node
        .prefix_search(prefix)
        .into_iter()
        .filter(|(n, _)| n.starts_with('-'))
        .map(|(n, _)| n)
        .collect();
    for flag in &trie_flags {
        if !known_flags.iter().any(|(f, _)| f == flag) {
            known_flags.push((flag.to_string(), None));
        }
    }

    known_flags.sort_by(|a, b| a.0.cmp(&b.0));

    if known_flags.is_empty() {
        output.push_str(&format!("% No flags matching \"{}\"\n", prefix));
        return output;
    }

    // If exactly one match and it IS the prefix: flag is complete — show what it expects
    if known_flags.len() == 1 && known_flags[0].0 == prefix {
        if let Some(arg_type) = known_flags[0].1 {
            let hint = runtime_complete::type_hint(arg_type);
            output.push_str(&format!("% {} expects: {}\n", prefix, hint));
            let rt = runtime_complete::list_matches(arg_type, "");
            let names: Vec<&str> = rt.iter().map(String::as_str).collect();
            if !names.is_empty() {
                output.push_str(&format_columns(&names, 80));
            }
        } else if let Some((tag, argv)) =
            spec.and_then(|s| s.flag_call_programs.get(prefix))
        {
            // _call_program flag: run it now to show valid values
            output.push_str(&format!("% {} expects: <{}>\n", prefix, tag));
            let results = runtime_complete::call_program_cached(argv, "");
            if !results.is_empty() {
                let names: Vec<&str> = results.iter().map(String::as_str).collect();
                output.push_str(&format_columns(&names, 80));
            }
        } else if let Some(items) = spec.and_then(|s| s.flag_static_lists.get(prefix)) {
            // Static list flag: show the known items
            output.push_str(&format!("% {} expects: <value>\n", prefix));
            let names: Vec<&str> = items.iter().map(String::as_str).collect();
            output.push_str(&format_columns(&names, 80));
        } else {
            // Boolean flag, no argument
            output.push_str(&format!("% {} (no argument)\n", prefix));
        }
        return output;
    }

    // Multiple flag matches
    output.push_str("% Possible flags:\n");

    // Multiple matches or partial: show flag names with their expected arg type
    let col_width = known_flags.iter().map(|(f, _)| f.len()).max().unwrap_or(0) + 2;
    for (flag, arg_type) in &known_flags {
        if let Some(at) = arg_type {
            let hint = runtime_complete::type_hint(*at);
            output.push_str(&format!("  {:<width$}{}\n", flag, hint, width = col_width));
        } else {
            output.push_str(&format!("  {}\n", flag));
        }
    }
    output
}

/// Format flags from trie (history-learned) with their spec-derived arg type hints.
pub(super) fn format_flags_from_trie(flags: &[(&str, &TrieNode)], spec: Option<&trie::ArgSpec>) -> String {
    let col_width = flags.iter().map(|(n, _)| n.len()).max().unwrap_or(0) + 2;
    let mut out = String::new();
    for (name, _) in flags {
        let typed_hint = spec
            .and_then(|s| s.type_after_flag(name))
            .map(runtime_complete::type_hint);
        let call_program_hint = spec
            .and_then(|s| s.flag_call_programs.get(*name))
            .map(|(tag, _)| tag.as_str());
        let static_hint: Option<String> = spec
            .and_then(|s| s.flag_static_lists.get(*name))
            .map(|items| items.iter().take(4).cloned().collect::<Vec<_>>().join("|"));
        if let Some(hint) = typed_hint {
            out.push_str(&format!("  {:<width$}<{}>\n", name, hint, width = col_width));
        } else if let Some(hint) = call_program_hint {
            out.push_str(&format!("  {:<width$}<{}>\n", name, hint, width = col_width));
        } else if let Some(hint) = static_hint {
            out.push_str(&format!("  {:<width$}{}\n", name, hint, width = col_width));
        } else {
            out.push_str(&format!("  {}\n", name));
        }
    }
    out
}

/// Detect the current terminal width via ioctl(TIOCGWINSZ).
/// Falls back to $COLUMNS, then 80.
pub(super) fn terminal_width() -> usize {
    // Try ioctl on stderr (fd 2) — most likely to be a real tty even when
    // stdout/stdin are redirected (e.g. in a pipeline).
    #[cfg(unix)]
    {
        use std::os::unix::io::RawFd;

        #[repr(C)]
        struct Winsize {
            ws_row: u16,
            ws_col: u16,
            _ws_xpixel: u16,
            _ws_ypixel: u16,
        }

        // TIOCGWINSZ varies by platform
        #[cfg(target_os = "macos")]
        const TIOCGWINSZ: u64 = 0x4008_7468;
        #[cfg(not(target_os = "macos"))]
        const TIOCGWINSZ: u64 = 0x5413;

        // Try stderr (2), then stdout (1), then stdin (0)
        for fd in [2i32, 1, 0] as [RawFd; 3] {
            let mut ws = Winsize {
                ws_row: 0,
                ws_col: 0,
                _ws_xpixel: 0,
                _ws_ypixel: 0,
            };
            let ret = unsafe { libc_ioctl(fd, TIOCGWINSZ, &mut ws as *mut Winsize as *mut u8) };
            if ret == 0 && ws.ws_col > 0 {
                return ws.ws_col as usize;
            }
        }

        // ioctl failed (not a tty) — try $COLUMNS
        if let Some(w) = std::env::var("COLUMNS")
            .ok()
            .and_then(|c| c.parse::<usize>().ok())
        {
            return w.clamp(40, 500);
        }
    }
    #[cfg(not(unix))]
    {
        if let Some(w) = std::env::var("COLUMNS")
            .ok()
            .and_then(|c| c.parse::<usize>().ok())
        {
            return w.clamp(40, 500);
        }
    }
    80
}

#[cfg(unix)]
unsafe fn libc_ioctl(fd: i32, request: u64, arg: *mut u8) -> i32 {
    unsafe extern "C" {
        fn ioctl(fd: i32, request: u64, ...) -> i32;
    }
    unsafe { ioctl(fd, request, arg) }
}

/// Format a list of names into columns, capped at `max_items`.
/// Uses terminal width from COLUMNS env (default 80). Short lists use a single column.
pub(super) fn format_columns(names: &[&str], max_items: usize) -> String {
    if names.is_empty() {
        return String::new();
    }

    let term_width = terminal_width();

    let total = names.len();
    let visible_count = total.min(max_items);
    let shown = &names[..visible_count];

    // Single-column for small lists (≤ 12 items)
    if shown.len() <= 12 {
        let mut out = String::new();
        for name in shown {
            out.push_str(&format!("  {}\n", name));
        }
        if total > max_items {
            out.push_str(&format!("  ... and {} more\n", total - max_items));
        }
        return out;
    }

    // Multi-column for larger lists
    let max_name_len = shown.iter().map(|s| s.len()).max().unwrap_or(0);
    let col_width = max_name_len + 2; // 2-space gap between columns
    // Account for the 2-space indent
    let usable_width = term_width.saturating_sub(2);
    let num_cols = (usable_width / col_width).clamp(1, 6);

    let rows = shown.len().div_ceil(num_cols);
    let mut out = String::new();

    for row in 0..rows {
        out.push_str("  ");
        for col in 0..num_cols {
            let idx = col * rows + row; // column-major (like `ls`)
            if idx >= shown.len() {
                break;
            }
            let is_last_in_row = col == num_cols - 1 || (col + 1) * rows + row >= shown.len();
            if is_last_in_row {
                out.push_str(shown[idx]);
            } else {
                out.push_str(&format!("{:<width$}", shown[idx], width = col_width));
            }
        }
        out.push('\n');
    }

    if total > max_items {
        out.push_str(&format!("  ... and {} more\n", total - max_items));
    }

    out
}

/// Show type-aware completions for a given arg mode and prefix.
pub(super) fn show_type_completions(
    output: &mut String,
    mode: ArgMode,
    prefix: &str,
    spec: Option<&trie::ArgSpec>,
    arg_position: u32,
) {
    match mode {
        ArgMode::DirsOnly => {
            output.push_str("  Expects: <directory>\n");
            output.push_str(&complete_filesystem(prefix, true));
        }
        ArgMode::Paths => {
            output.push_str("  Expects: <file>\n");
            output.push_str(&complete_filesystem(prefix, false));
        }
        ArgMode::Runtime(type_id) => {
            // Handle user@host prefix splitting: typing `alice@gi` means
            // we should complete host names that start with "gi", then
            // prepend "alice@" to each result.  Mirrors `compset -P '*@'`
            // in Zsh completion functions like _ssh.
            if type_id == trie::ARG_MODE_HOSTS
                && let Some(at_pos) = prefix.find('@')
            {
                let user_prefix = &prefix[..=at_pos]; // e.g. "alice@"
                let host_prefix = &prefix[at_pos + 1..]; // e.g. "gi"
                let hosts = runtime_complete::list_matches(trie::ARG_MODE_HOSTS, host_prefix);
                let with_user: Vec<String> =
                    hosts.iter().map(|h| format!("{user_prefix}{h}")).collect();
                output.push_str("% Expects: <user@host>\n");
                if with_user.is_empty() {
                    if !host_prefix.is_empty() {
                        output.push_str(&format!("% No matches for \"{host_prefix}\"\n"));
                    }
                } else {
                    let names: Vec<&str> = with_user.iter().map(String::as_str).collect();
                    output.push_str(&format_columns(&names, 80));
                }
                return;
            }
            let hint = runtime_complete::type_hint(type_id);
            output.push_str(&format!("% Expects: {}\n", hint));
            let rt = runtime_complete::list_matches(type_id, prefix);
            let names: Vec<&str> = rt.iter().map(String::as_str).collect();
            if names.is_empty() {
                if !prefix.is_empty() {
                    output.push_str(&format!("% No matches for \"{}\"\n", prefix));
                }
            } else {
                output.push_str(&format_columns(&names, 80));
            }
        }
        _ => {
            // Check spec for type hint even in Normal/ExecsOnly mode
            if let Some(spec) = spec
                && let Some(pos_type) = spec.type_at(arg_position)
                && pos_type != 0
            {
                let hint = runtime_complete::type_hint(pos_type);
                output.push_str(&format!("% Expects: {}\n", hint));
                let rt = runtime_complete::list_matches(pos_type, prefix);
                let names: Vec<&str> = rt.iter().map(String::as_str).collect();
                if !names.is_empty() {
                    output.push_str(&format_columns(&names, 80));
                    return;
                }
            }
            if prefix.is_empty() {
                output.push_str("% <enter argument>\n");
            } else {
                output.push_str(&format!("% No commands matching \"{}\"\n", prefix));
            }
        }
    }
}

/// Resolve just the first word of a command against the trie root.
pub(super) fn resolve_first_word(word: &str, trie: &CommandTrie) -> String {
    if trie.root.get_child(word).is_some() {
        return word.to_string();
    }
    let matches = trie.root.prefix_search(word);
    if matches.len() == 1 {
        return matches[0].0.to_string();
    }
    word.to_string()
}

/// List filesystem entries for `?` completion in dir/path commands.
pub(super) fn complete_filesystem(word: &str, dirs_only: bool) -> String {
    let cwd = std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from("."));

    let (search_dir, pattern) = if let Some((dir_part, comp)) = word.rsplit_once('/') {
        let dir = if let Some(rest) = dir_part.strip_prefix('~') {
            let home = dirs::home_dir().unwrap_or_default();
            let rest = rest.strip_prefix('/').unwrap_or(rest);
            if rest.is_empty() {
                home
            } else {
                home.join(rest)
            }
        } else if dir_part.is_empty() {
            std::path::PathBuf::from("/")
        } else {
            cwd.join(dir_part)
        };
        (dir, comp)
    } else {
        (cwd, word)
    };

    let mut entries: Vec<String> = match std::fs::read_dir(&search_dir) {
        Ok(rd) => rd
            .flatten()
            .filter(|e| !dirs_only || e.path().is_dir())
            .map(|e| e.file_name().to_string_lossy().to_string())
            .collect(),
        Err(_) => return "  (cannot read directory)\n".to_string(),
    };
    entries.sort();

    let filtered: Vec<&String> = if pattern.is_empty() {
        entries.iter().collect()
    } else if let Some(suffix) = pattern.strip_prefix('!') {
        if suffix.is_empty() {
            entries.iter().collect()
        } else {
            let lower = suffix.to_lowercase();
            entries
                .iter()
                .filter(|e| e.ends_with(suffix) || e.to_lowercase().ends_with(&lower))
                .collect()
        }
    } else if let Some(sub) = pattern.strip_prefix('*') {
        if sub.is_empty() {
            entries.iter().collect()
        } else {
            let lower = sub.to_lowercase();
            entries
                .iter()
                .filter(|e| e.contains(sub) || e.to_lowercase().contains(&lower))
                .collect()
        }
    } else {
        let lower = pattern.to_lowercase();
        entries
            .iter()
            .filter(|e| e.starts_with(pattern) || e.to_lowercase().starts_with(&lower))
            .collect()
    };

    let mut output = String::new();
    if filtered.is_empty() {
        output.push_str(&format!("% No matches for \"{}\"\n", word));
    } else {
        output.push_str("% Possible completions:\n");
        // Append directory marker and use multi-column display
        let display_names: Vec<String> = filtered
            .iter()
            .map(|name| {
                if search_dir.join(name.as_str()).is_dir() {
                    format!("{}/", name)
                } else {
                    name.to_string()
                }
            })
            .collect();
        let refs: Vec<&str> = display_names.iter().map(String::as_str).collect();
        output.push_str(&format_columns(&refs, 80));
    }
    output
}



#[cfg(test)]
mod tests {
    use super::*;
    use crate::trie::CommandTrie;

    fn build_test_trie() -> CommandTrie {
        let mut trie = CommandTrie::new();
        trie.insert(&["git", "checkout", "main"]);
        trie.insert(&["git", "checkout", "develop"]);
        trie.insert(&["git", "commit", "-m"]);
        trie.insert(&["git", "push"]);
        trie.insert(&["grep", "-r", "pattern"]);
        trie.insert(&["go", "build"]);
        trie.insert(&["terraform", "apply"]);
        trie.insert(&["terraform", "destroy"]);
        trie.insert(&["terraform", "init"]);
        trie.insert(&["terraform", "plan"]);
        trie.insert_command("gzip");
        trie
    }

    #[test]
    fn test_format_columns_empty() {
        assert_eq!(format_columns(&[], 100), "");
    }
    #[test]
    fn test_format_columns_single_column() {
        let names = vec!["add", "commit", "push"];
        let result = format_columns(&names, 100);
        assert!(result.contains("  add\n"));
        assert!(result.contains("  commit\n"));
        assert!(result.contains("  push\n"));
    }
    #[test]
    fn test_format_columns_overflow_message() {
        let names: Vec<&str> = (0..5).map(|i| match i {
            0 => "a", 1 => "b", 2 => "c", 3 => "d", _ => "e",
        }).collect();
        let result = format_columns(&names, 3);
        assert!(result.contains("... and 2 more"));
    }
    #[test]
    fn test_format_columns_multi_column() {
        // >12 items should use multi-column layout
        let names: Vec<&str> = vec![
            "aa", "bb", "cc", "dd", "ee", "ff", "gg", "hh", "ii", "jj", "kk", "ll", "mm",
        ];
        let result = format_columns(&names, 100);
        // Should have fewer lines than items (multi-column)
        let lines: Vec<&str> = result.lines().collect();
        assert!(lines.len() < names.len());
    }
    #[test]
    fn test_resolve_first_word_exact() {
        let trie = build_test_trie();
        assert_eq!(resolve_first_word("git", &trie), "git");
    }
    #[test]
    fn test_resolve_first_word_prefix() {
        let trie = build_test_trie();
        assert_eq!(resolve_first_word("ter", &trie), "terraform");
    }
    #[test]
    fn test_resolve_first_word_ambiguous() {
        let trie = build_test_trie();
        // "g" matches git, grep, go, gzip — returns unchanged
        assert_eq!(resolve_first_word("g", &trie), "g");
    }
    #[test]
    fn test_resolve_first_word_no_match() {
        let trie = build_test_trie();
        assert_eq!(resolve_first_word("zzz", &trie), "zzz");
    }
}
