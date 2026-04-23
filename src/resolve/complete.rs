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

/// Return the configured picker-header prefix (default `%`).
#[inline]
fn hdr() -> String {
    crate::runtime_config::get().picker_header_prefix
}

/// Heuristic filter for tag-group item noise. The `_description` /
/// `_wanted` extractor sometimes grabs raw zsh code fragments —
/// matcher-list patterns (`r:|/=*`), parameter expansions (`${…}`),
/// internal cache-var names (`_git_refs_cache`), lone backslashes, etc.
/// Displaying those as completion items is confusing; drop anything
/// that doesn't look like a plausible argument value.
///
/// Conservative — prefers keeping over dropping. Only rejects strings
/// that clearly originate from zsh source rather than from a compadd
/// argument list. Legit items like `%1` (job spec), `--flag`, `v2.0`
/// must survive.
fn is_plausible_item(s: &str) -> bool {
    let t = s.trim();
    if t.is_empty() { return false; }

    // Character whitelist: alphanumerics plus punctuation that can
    // legitimately appear in command arguments (paths, job specs, flags,
    // URLs, email addresses, etc.). Anything with `{`, `}`, `#`, `*`, `\`,
    // `$`, `(`, `)`, `[`, `]`, `&`, `|`, `;`, `<`, `>`, backtick, quote
    // characters, whitespace — we treat as zsh syntax leakage rather than
    // a real value.
    let allowed = |c: char| {
        c.is_ascii_alphanumeric()
            || matches!(c, '_' | '-' | '.' | '/' | ':' | '@' | '%' | '+' | ',' | '=' | '~')
    };
    if !t.chars().all(allowed) { return false; }

    // Lone punctuation.
    if t.len() == 1 && !t.chars().next().unwrap().is_ascii_alphanumeric() {
        return false;
    }

    // Zsh matcher-list fragments: `r:|…=*`, `l:|…=*`, `b:=*`, `e:=*`.
    if (t.starts_with("r:") || t.starts_with("l:") || t.starts_with("b:") || t.starts_with("e:"))
        && (t.contains('*') || t.contains('='))
    {
        return false;
    }

    // Leading-underscore identifiers (zsh convention for private helpers
    // and cache vars: `_git_refs_cache`, `__git_describe`, …). Regular
    // flags starting with `-` are fine.
    if t.starts_with('_') {
        return false;
    }

    // Convention suffixes that reveal a zsh *variable name* has leaked
    // into the item list — the parser grabbed the identifier that names
    // a list rather than the list's contents. `git_present_options`,
    // `valid_hosts_cache`, `known_commands`, etc.
    const INTERNAL_SUFFIXES: &[&str] = &[
        "_options", "_cache", "_names", "_commands", "_refs",
        "_args", "_flags", "_files", "_vars", "_tags", "_aliases",
        "_completions", "_subcommands",
    ];
    if INTERNAL_SUFFIXES.iter().any(|suf| t.ends_with(suf)) {
        return false;
    }

    // URL scheme fragments (`ftp://`, `http://`, `https://`, etc.) are never
    // valid top-level completions — they only make sense after a specific flag
    // like `--url=`.  Their `://` sigil distinguishes them unambiguously.
    if t.contains("://") {
        return false;
    }

    // Single uppercase letters like `I` and `M` (sed s-command modifiers)
    // are not useful as top-level completions.  Single lowercase alphanumerics
    // and single digits are already accepted — we only block lone uppercase.
    if t.len() == 1 && t.chars().next().unwrap().is_ascii_uppercase() {
        return false;
    }

    true
}

/// Tags that carry context-specific completions only meaningful after a flag
/// (e.g. HTTP headers after `--header=`, URL schemes after `--url=`, ssh
/// subsystem names inside `sftp://` completion).  Displaying these groups at
/// the command root produces noise and confuses users.
fn is_noisy_tag(tag: &str) -> bool {
    // Normalise to lowercase for comparison.
    let t = tag.to_lowercase();
    matches!(
        t.as_str(),
        "headers"     // HTTP / mail headers
        | "urls"      // URL scheme prefixes (ftp://, http://)
        | "subsystems"// ssh subsystem names (sftp)
        | "mods"      // sed s-command modifiers (I, M)
        | "address-forms" // sed address forms
        | "steps"     // sed step values
    )
}

/// When the parser grabbed a group's own tag or label as if it were an item,
/// the "item" text ends up matching the group's tag / label (case-insensitive,
/// possibly plural vs singular). Filter these defensively at display time.
fn matches_group_meta(item: &str, tag: &str, label: &str) -> bool {
    let item_l = item.to_lowercase();
    let tag_l = tag.to_lowercase();
    let label_l = label.to_lowercase();
    if item_l == tag_l || item_l == label_l {
        return true;
    }
    // Plural/singular slack: "stashes" vs tag "stash", "branches" vs "branch".
    let stem = |s: &str| {
        if s.ends_with("ies") && s.len() > 3 {
            format!("{}y", &s[..s.len() - 3])
        } else if s.ends_with('s') && s.len() > 1 {
            s[..s.len() - 1].to_string()
        } else {
            s.to_string()
        }
    };
    stem(&item_l) == tag_l || stem(&item_l) == label_l
        || item_l == stem(&tag_l) || item_l == stem(&label_l)
}

/// Uppercase the first character of a string, leave the rest unchanged.
fn titlecase(s: &str) -> String {
    let mut chars = s.chars();
    match chars.next() {
        None => String::new(),
        Some(first) => first.to_uppercase().to_string() + chars.as_str(),
    }
}

/// Classify a `complete()` output string as "generic" — meaning the static
/// analysis couldn't produce useful suggestions and the caller should fall
/// back to the ZLE worker's tiered completion. Kept close to `complete()`
/// so the two stay in sync; previously the plugin grepped for a bare
/// `<enter argument>` placeholder, which was fragile.
pub fn is_generic_output(output: &str) -> bool {
    output.contains("Expects: <argument>") || output.contains("No commands matching")
}

pub fn complete(input: &str, trie: &CommandTrie, pins: &Pins, context_hint: super::engine::ContextHint) -> String {
    use super::engine::ContextHint;

    // Leading `!` is a hands-off marker (see `starts_with_bang`). Produce no
    // completions so the shell's native completion (or history expansion)
    // gets a clean look.
    if starts_with_bang(input) {
        return String::new();
    }

    // Single-quoted: no completions are possible inside single quotes.
    if context_hint == ContextHint::SingleQuoted {
        return String::new();
    }

    // Backticked: treat conservatively — no completions (inner shell context).
    if context_hint == ContextHint::Backticked {
        return String::new();
    }

    // ParameterName: the cursor is inside `${PARAM…}`. Find the partial
    // parameter name after the last `${` and offer matching parameter names.
    if context_hint == ContextHint::ParameterName {
        return complete_parameter_name(input, trie);
    }

    // Redirection context: complete the last word as a filesystem path.
    if context_hint == ContextHint::Redirection {
        let words: Vec<&str> = input.split_whitespace().collect();
        let prefix = if input.ends_with(' ') || input.ends_with('\t') {
            ""
        } else {
            words.last().copied().unwrap_or("")
        };
        return complete_filesystem(prefix, false);
    }

    // math / condition: no completions.
    if matches!(context_hint, ContextHint::Math | ContextHint::Condition) {
        return String::new();
    }

    // DoubleQuoted: only allow parameter ($VAR) completion; suppress
    // command/subcommand/flag output. We run the normal path below but it
    // will naturally offer parameter completion when the prefix starts with
    // `$`. For any other prefix we return empty — the quoted string is data.
    let double_quoted = context_hint == ContextHint::DoubleQuoted;
    if double_quoted {
        // Only offer completions if the cursor is on a `$`-prefixed word.
        let words: Vec<&str> = input.split_whitespace().collect();
        let prefix = if input.ends_with(' ') || input.ends_with('\t') {
            ""
        } else {
            words.last().copied().unwrap_or("")
        };
        if !prefix.starts_with('$') {
            return String::new();
        }
        // Fall through: the $VAR branch in complete_segment handles this.
    }

    // Expand global aliases so the `?` key sees the same expanded buffer
    // that resolve_line would use.
    let input_string;
    let input: &str = if !trie.galiases.is_empty() {
        input_string = crate::galiases::expand_galiases(input, &trie.galiases);
        &input_string
    } else {
        input
    };

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

/// Complete a parameter name when the cursor is inside `${PARAM…}`.
///
/// Scans `input` backwards from the end to find the last `${` that has no
/// matching `}`, extracts the partial name between `${` and end-of-input,
/// and returns matching parameter names from the live-state cache.
fn complete_parameter_name(input: &str, trie: &CommandTrie) -> String {
    // Find the last `${` not closed by a `}` before end of input.
    let partial = {
        let bytes = input.as_bytes();
        let mut depth: i32 = 0;
        let mut dollar_brace_pos: Option<usize> = None;
        let mut i = 0;
        while i < bytes.len() {
            if i + 1 < bytes.len() && bytes[i] == b'$' && bytes[i + 1] == b'{' {
                depth += 1;
                dollar_brace_pos = Some(i + 2); // position after `${`
                i += 2;
                continue;
            }
            if bytes[i] == b'}' && depth > 0 {
                depth -= 1;
                if depth == 0 {
                    dollar_brace_pos = None;
                }
            }
            i += 1;
        }
        // If there is an unclosed `${`, the partial name starts at dollar_brace_pos.
        if depth > 0 {
            dollar_brace_pos.map(|pos| &input[pos..])
        } else {
            None
        }
    };

    let param_prefix = partial.unwrap_or("").trim_start();
    let params = runtime_complete::live_state_for("parameters");

    // Also include parameters from the trie's live_state snapshot (populated
    // at build time from the ingest worker).
    let trie_params: Vec<String> = trie
        .live_state
        .get("parameters")
        .map(|s| runtime_complete::parse_parameters_output(s))
        .unwrap_or_default();

    let mut all_params: Vec<String> = params.clone();
    for p in trie_params {
        if !all_params.contains(&p) {
            all_params.push(p);
        }
    }

    let hits: Vec<String> = all_params
        .iter()
        .filter(|p| p.starts_with(param_prefix))
        .cloned()
        .collect();

    if hits.is_empty() {
        return String::new();
    }

    let mut output = format!("{} Expects: <$parameter>\n", hdr());
    let refs: Vec<&str> = hits.iter().map(String::as_str).collect();
    output.push_str(&format_columns(&refs, 80));
    output
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
        output.push_str(&format!("{} Possible commands:\n", hdr()));
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
        let mut matches = trie.root.matcher_aware_search(prefix, &trie.matcher_rules);
        if matches.is_empty() {
            output.push_str(&format!("{} No commands matching \"{}\"\n", hdr(), prefix));
        } else {
            matches.sort_by(|a, b| b.1.count.cmp(&a.1.count).then(a.0.cmp(b.0)));
            let names: Vec<&str> = matches.iter().map(|(n, _)| *n).collect();
            output.push_str(&format!("{} Possible commands:\n", hdr()));
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
    // Track whether we actually walked INTO the trie for the parent command.
    // If the first word isn't in the trie, `node` would remain at `trie.root` and
    // we'd spuriously return every top-level command as a "subcommand of <unknown>".
    let mut cmd_in_trie = false;

    if pin_consumed > 0 {
        resolved_words = expanded_prefix.clone();
        for w in &expanded_prefix {
            match node.get_child(w) {
                Some(child) => {
                    node = child;
                    cmd_in_trie = true;
                }
                None => break,
            }
        }
        resolve_start = pin_consumed;
    } else {
        if let Some(child) = node.get_child(&resolved_cmd) {
            node = child;
            cmd_in_trie = true;
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
        let matches = node.matcher_aware_search(word, &trie.matcher_rules);
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
                output.push_str(&format!("{} Possible completions:\n", hdr()));
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

    // --- Parameter reference completion ---
    // When the prefix looks like `$NAME` (not `$(...)` or `${NAME}`),
    // offer shell parameter names from the worker's live-state dump.
    if let Some(param_prefix) = prefix.strip_prefix('$')
        && !param_prefix.starts_with('(')
        && !param_prefix.starts_with('{')
        && param_prefix.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
    {
        let params = runtime_complete::live_state_for("parameters");
        let hits: Vec<String> = params
            .iter()
            .filter(|p| p.starts_with(param_prefix))
            .map(|p| format!("${p}"))
            .collect();
        if !hits.is_empty() {
            output.push_str(&format!("{} Expects: <$parameter>\n", hdr()));
            let refs: Vec<&str> = hits.iter().map(String::as_str).collect();
            output.push_str(&format_columns(&refs, 80));
            return output;
        }
    }

    // --- Flag completion mode ---
    // When typing a flag prefix (starts with '-'), show known flags + their expected arg types.
    if prefix.starts_with('-') {
        return complete_flags(prefix, spec, node, &completed_words, output);
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
        output.push_str(&format!("{} Expects: <value>\n", hdr()));
        let mut seen = std::collections::HashSet::new();
        let filtered: Vec<&str> = items
            .iter()
            .filter(|i| prefix.is_empty() || i.starts_with(prefix))
            .filter(|i| is_plausible_item(i))
            .filter(|i| seen.insert(i.as_str()))
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

    // Only offer rest_* completions when we've exhausted the subcommand tree —
    // otherwise a parent command like `git` with a leaked rest_static_list from
    // some nested `_values` block would hide its 100+ real subcommands behind
    // a random enumeration. Subcommands always win while children exist.
    let node_has_subcommands = node
        .children
        .keys()
        .any(|k| !k.starts_with('-'));

    if !prefix.starts_with('-')
        && !prev_is_flag_consuming
        && !node_has_subcommands
        && let Some((tag, argv)) = spec.and_then(|s| s.rest_call_program.as_ref())
    {
        let results = runtime_complete::call_program_cached(argv, prefix);
        let mut seen = std::collections::HashSet::new();
        let names: Vec<&str> = results
            .iter()
            .filter(|i| is_plausible_item(i))
            .filter(|i| seen.insert(i.as_str()))
            .map(String::as_str)
            .collect();
        if !names.is_empty() {
            output.push_str(&format!("% Expects: <{}>\n", tag));
            output.push_str(&format_columns(&names, 80));
            return output;
        }
    }

    // Rest position with static list
    if !prefix.starts_with('-')
        && !prev_is_flag_consuming
        && !node_has_subcommands
        && let Some(items) = spec.and_then(|s| s.rest_static_list.as_ref())
    {
        let mut seen = std::collections::HashSet::new();
        let filtered: Vec<&str> = items
            .iter()
            .filter(|i| prefix.is_empty() || i.starts_with(prefix))
            .filter(|i| is_plausible_item(i))
            .filter(|i| seen.insert(i.as_str()))
            .map(String::as_str)
            .collect();
        if !filtered.is_empty() {
            output.push_str(&format!("{} Expects: <value>\n", hdr()));
            output.push_str(&format_columns(&filtered, 80));
            return output;
        }
    }

    // Skip trie when we're completing the value of a flag that takes a typed
    // argument (e.g. `sudo -u <user>`).  The trie children here are learned
    // prior invocations of the command, not values for this flag.
    let in_flag_value_context = prev_word
        .is_some_and(|p| p.starts_with('-') && spec.is_some_and(|s| s.flag_takes_value(p)));

    let trie_matches = if in_flag_value_context || !cmd_in_trie {
        vec![]
    } else {
        node.matcher_aware_search(prefix, &trie.matcher_rules)
    };

    // --- Tag-group display ---
    // When the trie has tag_groups for the resolved command path, prefer
    // a grouped display over the flat subcommand list.  Fall through to the
    // flat path if every group filtered to zero items, when tag_grouping
    // is disabled in the runtime config, or when the current node has a
    // lot of real subcommand children (flat list wins — a parent command
    // like `git` with 100+ subcommands gets useless noise from tag groups
    // scraped out of state-body fragments).
    let cmd_key = resolved_words.join(" ");
    let subcommand_child_count = node
        .children
        .keys()
        .filter(|k| !k.starts_with('-'))
        .count();
    let has_many_subcommands = subcommand_child_count >= 10;
    if !has_many_subcommands
        && crate::runtime_config::get().tag_grouping
        && let Some(groups) = trie.tag_groups.get(&cmd_key)
        && !groups.is_empty()
        && !prefix.starts_with('-')
    {
        // Resolve the format template for tag-group headers (from zstyle format).
        let format_template: Option<&str> = trie
            .completion_styles
            .formats
            .iter()
            .find(|(k, _)| k.contains(":descriptions"))
            .map(|(_, v)| v.as_str());

        // Resolve the group-name setting (empty string = per-tag headers, which is default).
        let group_name: Option<&str> = trie
            .completion_styles
            .group_names
            .iter()
            .find(|(k, _)| k.as_str() == ":completion:*" || k.starts_with(":completion:*:"))
            .map(|(_, v)| v.as_str())
            .filter(|v| !v.is_empty());

        // Determine whether stdout is a TTY (for list-colors).
        let use_colors = !trie.completion_styles.list_colors.is_empty() && is_stdout_tty();

        let mut group_output = String::new();

        if let Some(gname) = group_name {
            // Flatten all groups under one combined header.
            let header_label = match format_template {
                Some(tpl) => apply_format(tpl, gname),
                None => gname.to_string(),
            };
            let mut all_items: Vec<String> = Vec::new();
            for group in groups {
                // Skip entire tag groups known to carry context-specific items
                // that are only meaningful after a specific flag, not at root.
                if is_noisy_tag(&group.tag) {
                    continue;
                }
                let filtered: Vec<String> = group
                    .items
                    .iter()
                    .filter(|i| prefix.is_empty() || i.starts_with(prefix))
                    .filter(|i| is_plausible_item(i))
                    .filter(|i| !matches_group_meta(i, &group.tag, &group.label))
                    .cloned()
                    .collect();
                all_items.extend(filtered);
            }
            if !all_items.is_empty() {
                group_output.push_str(&format!("% {}:\n", header_label));
                let display: Vec<String> = if use_colors {
                    all_items.iter().map(|i| colorize_item(i)).collect()
                } else {
                    all_items.clone()
                };
                let refs: Vec<&str> = display.iter().map(String::as_str).collect();
                group_output.push_str(&format_columns(&refs, 80));
            }
        } else {
            // Per-tag headers (default behavior). Skip empty groups entirely
            // — the old "empty-group stub" path printed a header with no
            // items below it, which is noise when the parser attached a
            // label but no list (happens a lot with `_description TAG expl`
            // calls whose compadd runs on a separate line we don't thread).
            for group in groups {
                // Skip known noisy tags that are context-specific (e.g. HTTP
                // headers, URL schemes, ssh subsystems, sed modifiers).
                if is_noisy_tag(&group.tag) {
                    continue;
                }
                let filtered: Vec<&str> = group
                    .items
                    .iter()
                    .filter(|i| prefix.is_empty() || i.starts_with(prefix))
                    .filter(|i| is_plausible_item(i))
                    .filter(|i| !matches_group_meta(i, &group.tag, &group.label))
                    .map(String::as_str)
                    .collect();
                if filtered.is_empty() {
                    continue;
                }
                let header_label = {
                    let raw = titlecase(&group.label);
                    match format_template {
                        Some(tpl) => apply_format(tpl, &raw),
                        None => raw,
                    }
                };
                group_output.push_str(&format!("% {}:\n", header_label));
                if use_colors {
                    let display: Vec<String> =
                        filtered.iter().map(|i| colorize_item(i)).collect();
                    let refs: Vec<&str> = display.iter().map(String::as_str).collect();
                    group_output.push_str(&format_columns(&refs, 80));
                } else {
                    group_output.push_str(&format_columns(&filtered, 80));
                }
            }
        }

        if !group_output.is_empty() {
            output.push_str(&group_output);
            return output;
        }
    }

    if trie_matches.is_empty() {
        // No trie matches — show type-aware completions based on arg spec
        show_type_completions(&mut output, current_mode, prefix, spec, arg_position);
    } else {
        // Separate subcommands from flags (flags from history are trie children too).
        // Subcommands sourced from history can include shell-syntax junk
        // (`'export`, `$TOKEN`, backticks, `#`-prefixed fragments) — apply the
        // same plausibility filter we use elsewhere. Flags are kept as-is
        // since they're structurally constrained (must start with `-`) and
        // formatted separately via format_flags_from_trie.
        let subcmds: Vec<(&str, &TrieNode)> = trie_matches
            .iter()
            .filter(|(n, _)| !n.starts_with('-'))
            .filter(|(n, _)| is_plausible_item(n))
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
            let descs = trie.descriptions.get(&cmd_key);

            output.push_str(&format!("{} Possible subcommands:\n", hdr()));
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
                output.push_str(&format!("{} Possible flags:\n", hdr()));
            } else {
                output.push_str(&format!("{} Flags:\n", hdr()));
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

/// Substitute `%d` tokens in a zstyle format string with the given description.
fn apply_format(raw_format: &str, description: &str) -> String {
    raw_format.replace("%d", description)
}

/// Apply ANSI cyan color to items that look like directories (end with `/`).
/// Used when `list-colors` is set and stdout is a TTY. Suppressed when
/// `disable_list_colors` is true in the runtime config.
fn colorize_item(item: &str) -> String {
    if crate::runtime_config::get().disable_list_colors {
        return item.to_string();
    }
    if item.ends_with('/') {
        format!("\x1b[36m{item}\x1b[0m")
    } else {
        item.to_string()
    }
}

/// Return true when stdout is connected to a terminal.
fn is_stdout_tty() -> bool {
    #[cfg(unix)]
    {
        use std::os::unix::io::RawFd;
        unsafe extern "C" {
            fn isatty(fd: RawFd) -> i32;
        }
        unsafe { isatty(1) != 0 }
    }
    #[cfg(not(unix))]
    {
        false
    }
}

/// Complete a flag prefix: show matching flags from spec and trie.
/// If the prefix exactly matches a single flag that takes an argument, show what it expects.
/// `prior_words` is the list of already-completed words on the command line; flags in exclusion
/// groups where a sibling is already present are filtered out.
pub(super) fn complete_flags(
    prefix: &str,
    spec: Option<&trie::ArgSpec>,
    node: &TrieNode,
    prior_words: &[&str],
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

        // Filter out flags that are mutually exclusive with a flag already on the line.
        if !spec.flag_exclusions.is_empty() {
            known_flags.retain(|(flag, _)| {
                for group in &spec.flag_exclusions {
                    if group.contains(flag) {
                        // If any OTHER flag in this exclusion group is already present, drop this one.
                        let sibling_present = group
                            .iter()
                            .filter(|g| *g != flag)
                            .any(|g| prior_words.contains(&g.as_str()));
                        if sibling_present {
                            return false;
                        }
                    }
                }
                true
            });
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
    output.push_str(&format!("{} Possible flags:\n", hdr()));

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

/// Format a list of names into columns, capped at `max_items` (or the
/// configured `max_completions_shown` when `max_items` is 200 — the old
/// hardcoded value — so call sites that pass 200 automatically pick up the
/// config knob without a signature change).
pub(super) fn format_columns(names: &[&str], max_items: usize) -> String {
    if names.is_empty() {
        return String::new();
    }

    let term_width = terminal_width();

    // Use the runtime-configured cap when the caller passed the legacy
    // sentinel value of 200.
    let effective_max = if max_items == 200 {
        crate::runtime_config::get().max_completions_shown as usize
    } else {
        max_items
    };

    let total = names.len();
    let visible_count = total.min(effective_max);
    let shown = &names[..visible_count];

    // Single-column for small lists (≤ 12 items)
    if shown.len() <= 12 {
        let mut out = String::new();
        for name in shown {
            out.push_str(&format!("  {}\n", name));
        }
        if total > effective_max {
            out.push_str(&format!("  ... and {} more\n", total - effective_max));
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

    if total > effective_max {
        out.push_str(&format!("  ... and {} more\n", total - effective_max));
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
                output.push_str(&format!("{} Expects: <user@host>\n", hdr()));
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
            // Show alternatives from the spec if present.
            let alt_suffix = if let Some(spec) = spec {
                let types = spec.types_at(arg_position);
                let alts: Vec<&str> = types
                    .iter()
                    .skip(1) // skip primary; we already show it
                    .filter(|&&t| t != type_id)
                    .map(|&t| runtime_complete::type_hint(t))
                    .collect();
                if alts.is_empty() {
                    String::new()
                } else {
                    format!(" (also accepts: {})", alts.join(", "))
                }
            } else {
                String::new()
            };
            output.push_str(&format!("% Expects: {}{}\n", hint, alt_suffix));
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
            if let Some(spec) = spec {
                let types = spec.types_at(arg_position);
                if let Some(&pos_type) = types.first()
                    && pos_type != 0
                {
                    let hint = runtime_complete::type_hint(pos_type);
                    let alts: Vec<&str> = types
                        .iter()
                        .skip(1)
                        .map(|&t| runtime_complete::type_hint(t))
                        .collect();
                    let alt_suffix = if alts.is_empty() {
                        String::new()
                    } else {
                        format!(" (also accepts: {})", alts.join(", "))
                    };
                    output.push_str(&format!("% Expects: {}{}\n", hint, alt_suffix));
                    let rt = runtime_complete::list_matches(pos_type, prefix);
                    let names: Vec<&str> = rt.iter().map(String::as_str).collect();
                    if !names.is_empty() {
                        output.push_str(&format_columns(&names, 80));
                        return;
                    }
                }
            }
            if prefix.is_empty() {
                // Generic-output sentinel: the caller (CLI / plugin) uses
                // `is_generic_output` to detect this case and trigger the
                // worker fallback. The user-visible wording is consistent
                // with the other `Expects: <X>` sibling lines.
                output.push_str(&format!("{} Expects: <argument>\n", hdr()));
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
    let matches = trie.root.matcher_aware_search(word, &trie.matcher_rules);
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
        output.push_str(&format!("{} Possible completions:\n", hdr()));
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

    /// Verify the Rust binary never emits tier-tag labels that are exclusive to
    /// the plugin's worker fallback chain.  Each tag ([approximate], [correct],
    /// [expand], [history]) is inserted only by the plugin function that wraps
    /// the corresponding worker request type; seeing one in Rust output would
    /// indicate a misplaced label that confuses the UI.
    #[test]
    fn complete_never_emits_worker_tier_labels() {
        let trie = build_test_trie();
        let pins = crate::pins::Pins::default();
        use super::super::engine::ContextHint;

        // Inputs that exercise different output paths
        let inputs = [
            ("",              ContextHint::Unknown),   // top-level command list
            ("gi",            ContextHint::Unknown),   // ambiguous prefix
            ("zzznomatch",    ContextHint::Unknown),   // no match
            ("git ",          ContextHint::Argument),  // subcommand list
            ("git ch",        ContextHint::Argument),  // subcommand prefix
            ("git checkout ", ContextHint::Argument),  // positional arg (unknown type)
        ];
        // Plugin-exclusive tier tags — Rust complete() must never emit these.
        let forbidden = ["[approximate]", "[correct]", "[expand]", "[history]"];
        for (input, hint) in inputs {
            let out = super::complete(input, &trie, &pins, hint);
            for tag in forbidden {
                assert!(
                    !out.contains(tag),
                    "complete({:?}) must not emit {}, got: {:?}",
                    input,
                    tag,
                    out
                );
            }
        }
    }

    #[test]
    fn complete_displays_tag_groups() {
        use crate::trie::TagGroup;
        let mut trie = CommandTrie::new();
        trie.insert_command("kill");
        trie.tag_groups.insert(
            "kill".to_string(),
            vec![
                TagGroup {
                    tag: "processes".to_string(),
                    label: "process".to_string(),
                    items: vec!["123".to_string(), "456".to_string()],
                },
                TagGroup {
                    tag: "jobs".to_string(),
                    label: "job".to_string(),
                    items: vec!["%1".to_string()],
                },
            ],
        );
        let pins = crate::pins::Pins::default();
        use super::super::engine::ContextHint;
        let out = super::complete("kill ", &trie, &pins, ContextHint::Argument);
        assert!(out.contains("% Process:"), "expected '% Process:' header, got: {:?}", out);
        assert!(out.contains("% Job:"), "expected '% Job:' header, got: {:?}", out);
        assert!(out.contains("123"), "expected item '123'");
        assert!(out.contains("%1"), "expected item '%1'");
    }

    #[test]
    fn complete_without_tag_groups_keeps_flat_output() {
        let trie = build_test_trie();
        let pins = crate::pins::Pins::default();
        use super::super::engine::ContextHint;
        // git has no tag_groups → should use the flat subcommand path
        let out = super::complete("git ", &trie, &pins, ContextHint::Argument);
        assert!(out.contains("% Possible subcommands:"), "expected flat subcommand header, got: {:?}", out);
        // Must not have tag-style headers
        assert!(!out.contains("% Process:"), "should not have tag-group header");
    }

    #[test]
    fn complete_applies_format_to_tag_header() {
        use crate::trie::{CompletionStyles, TagGroup};
        let mut trie = CommandTrie::new();
        trie.insert_command("kill");
        trie.tag_groups.insert(
            "kill".to_string(),
            vec![TagGroup {
                tag: "processes".to_string(),
                label: "process".to_string(),
                items: vec!["123".to_string()],
            }],
        );
        // Set format template: "[%d]"
        trie.completion_styles = CompletionStyles {
            formats: {
                let mut m = std::collections::HashMap::new();
                m.insert(":completion:*:descriptions".to_string(), "[%d]".to_string());
                m
            },
            ..Default::default()
        };
        let pins = crate::pins::Pins::default();
        use super::super::engine::ContextHint;
        let out = super::complete("kill ", &trie, &pins, ContextHint::Argument);
        assert!(
            out.contains("% [Process]:"),
            "expected '% [Process]:' with format applied, got: {:?}",
            out
        );
    }

    #[test]
    fn complete_merges_when_group_name_set() {
        use crate::trie::{CompletionStyles, TagGroup};
        let mut trie = CommandTrie::new();
        trie.insert_command("kill");
        trie.tag_groups.insert(
            "kill".to_string(),
            vec![
                TagGroup {
                    tag: "processes".to_string(),
                    label: "process".to_string(),
                    items: vec!["123".to_string()],
                },
                TagGroup {
                    tag: "jobs".to_string(),
                    label: "job".to_string(),
                    items: vec!["%1".to_string()],
                },
            ],
        );
        // Set non-empty group-name: all items under one header.
        trie.completion_styles = CompletionStyles {
            group_names: {
                let mut m = std::collections::HashMap::new();
                m.insert(":completion:*".to_string(), "all".to_string());
                m
            },
            ..Default::default()
        };
        let pins = crate::pins::Pins::default();
        use super::super::engine::ContextHint;
        let out = super::complete("kill ", &trie, &pins, ContextHint::Argument);
        // Should have a single combined header "% all:" and both items.
        assert!(out.contains("% all:"), "expected '% all:' merged header, got: {:?}", out);
        assert!(out.contains("123"), "expected item '123' in merged output");
        assert!(out.contains("%1"), "expected item '%1' in merged output");
        // Must NOT have per-tag headers.
        assert!(!out.contains("% Process:"), "should not have separate Process header");
        assert!(!out.contains("% Job:"), "should not have separate Job header");
    }

    // ── quote / param-context completion behaviour ───────────────────────────

    #[test]
    fn complete_single_quoted_returns_empty() {
        let trie = build_test_trie();
        let pins = crate::pins::Pins::default();
        use super::super::engine::ContextHint;
        // Inside a single-quoted string, no completions should be offered.
        let out = super::complete("echo 'git ", &trie, &pins, ContextHint::SingleQuoted);
        assert!(out.is_empty(), "expected empty output for SingleQuoted, got: {:?}", out);
    }

    #[test]
    fn complete_param_context_offers_parameters() {
        use super::super::engine::ContextHint;
        use crate::runtime_complete;
        // Seed the live state with some parameter names.
        let mut state = std::collections::HashMap::new();
        state.insert("parameters".to_string(), vec!["HOME".to_string(), "PATH".to_string(), "HISTFILE".to_string()]);
        runtime_complete::set_live_state(state);

        let trie = build_test_trie();
        let pins = crate::pins::Pins::default();
        let out = super::complete("echo ${HO", &trie, &pins, ContextHint::ParameterName);
        assert!(out.contains("HOME"), "expected HOME in parameter completions, got: {:?}", out);
        assert!(!out.contains("PATH"), "PATH should not match prefix HO, got: {:?}", out);
    }

    #[test]
    fn complete_double_quoted_suppresses_subcommands() {
        let trie = build_test_trie();
        let pins = crate::pins::Pins::default();
        use super::super::engine::ContextHint;
        // Inside a double-quoted string with no `$`-prefix, no completions.
        let out = super::complete("echo \"git ch", &trie, &pins, ContextHint::DoubleQuoted);
        assert!(!out.contains("checkout"), "subcommand should be suppressed in DoubleQuoted, got: {:?}", out);
        assert!(!out.contains("% Possible"), "no completion headers expected, got: {:?}", out);
    }

    #[test]
    fn complete_omits_excluded_flags() {
        use crate::trie::ArgSpec;
        // ArgSpec: flag_exclusions [["-a", "-v"]], flag_args for both.
        let spec = ArgSpec {
            flag_args: [
                ("-a".into(), crate::trie::ARG_MODE_PATHS),
                ("-v".into(), crate::trie::ARG_MODE_PATHS),
            ]
            .into_iter()
            .collect(),
            flag_exclusions: vec![vec!["-a".to_string(), "-v".to_string()]],
            ..Default::default()
        };

        let trie = build_test_trie();
        let node = trie.root.get_child("git").unwrap();

        // prior_words contains "-a" → "-v" should be omitted
        let prior: Vec<&str> = vec!["-a"];
        let out = complete_flags("-", Some(&spec), node, &prior, String::new());
        assert!(out.contains("-a"), "should still list -a");
        assert!(!out.contains("-v"), "should omit -v because -a is in prior_words");

        // prior_words empty → both should appear
        let out2 = complete_flags("-", Some(&spec), node, &[], String::new());
        assert!(out2.contains("-a"), "should list -a when no prior");
        assert!(out2.contains("-v"), "should list -v when no prior");
    }

    // --- is_plausible_item display-time filter tests ---

    #[test]
    fn is_plausible_item_rejects_url_schemes() {
        // URL scheme fragments like `ftp://` and `http://` must be filtered
        // — they are only valid after a specific flag, not at the root.
        assert!(!is_plausible_item("ftp://"), "ftp:// should be rejected");
        assert!(!is_plausible_item("http://"), "http:// should be rejected");
        assert!(!is_plausible_item("https://"), "https:// should be rejected");
        assert!(!is_plausible_item("sftp://"), "sftp:// should be rejected");
    }

    #[test]
    fn is_plausible_item_rejects_lone_uppercase() {
        // Single uppercase letters (sed s-command modifiers I, M) must be filtered.
        assert!(!is_plausible_item("I"), "lone uppercase I should be rejected");
        assert!(!is_plausible_item("M"), "lone uppercase M should be rejected");
        // Single lowercase and digits are fine (e.g. `v`, `1`).
        assert!(is_plausible_item("v"), "single lowercase should be accepted");
        assert!(is_plausible_item("1"), "single digit should be accepted");
        // Multi-character uppercase tokens are fine (subcommands).
        assert!(is_plausible_item("GET"), "multi-char uppercase should be accepted");
    }

    // --- is_noisy_tag filter tests ---

    #[test]
    fn is_noisy_tag_rejects_known_noisy_tags() {
        assert!(is_noisy_tag("headers"),      "headers tag should be noisy");
        assert!(is_noisy_tag("urls"),         "urls tag should be noisy");
        assert!(is_noisy_tag("subsystems"),   "subsystems tag should be noisy");
        assert!(is_noisy_tag("mods"),         "mods tag should be noisy");
        assert!(is_noisy_tag("address-forms"),"address-forms tag should be noisy");
        // Case-insensitive
        assert!(is_noisy_tag("Headers"),      "Headers (mixed case) should be noisy");
    }

    #[test]
    fn is_noisy_tag_accepts_useful_tags() {
        assert!(!is_noisy_tag("commands"),  "commands tag should not be noisy");
        assert!(!is_noisy_tag("files"),     "files tag should not be noisy");
        assert!(!is_noisy_tag("branches"),  "branches tag should not be noisy");
    }

    // --- noisy tag group display suppression test ---

    #[test]
    fn complete_suppresses_noisy_tag_groups_at_root() {
        use crate::trie::{CommandTrie, TagGroup};
        let mut trie = CommandTrie::default();
        trie.insert(&["wget"]);
        // Inject noisy tag group (HTTP headers, like wget has in _wget)
        trie.tag_groups.insert(
            "wget".to_string(),
            vec![
                TagGroup {
                    tag: "headers".to_string(),
                    label: "HTTP header".to_string(),
                    items: vec!["Accept".to_string(), "Content-Type".to_string()],
                },
            ],
        );
        let pins = crate::pins::Pins::default();
        // Even though the tag group has items, they should be suppressed.
        let out = super::complete("wget ", &trie, &pins, super::super::engine::ContextHint::Unknown);
        assert!(!out.contains("Accept"), "HTTP headers should not appear in wget root completions");
        assert!(!out.contains("Content-Type"), "HTTP headers should not appear in wget root completions");
    }
}
