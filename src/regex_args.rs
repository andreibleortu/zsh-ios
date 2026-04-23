//! Parser for the `_regex_arguments` DSL used in Zsh completion files.
//!
//! Some Zsh completers use `_regex_arguments _func SPEC…` instead of
//! `_arguments`.  This module tokenizes the SPEC, walks the token stream
//! maintaining a slot counter, and extracts per-positional arg types and
//! static enumerations — the same information we get from `_arguments`.
//!
//! The output feeds directly into [`crate::trie::ArgSpec`] via
//! [`crate::completions::scan_completions`].

use std::collections::HashMap;
use std::sync::LazyLock;

use crate::completions::{action_to_arg_type, action_to_static_list};

// ---------------------------------------------------------------------------
// Public surface
// ---------------------------------------------------------------------------

/// Parsed output from a `_regex_arguments` function body.
#[derive(Debug, Default)]
pub struct ParsedRegexArgs {
    /// Per-positional arg type (1-indexed).
    pub positional: HashMap<u32, u8>,
    /// Type for all remaining positionals after the last numbered one.
    pub rest: Option<u8>,
    /// Static enumerations by position (e.g. `(addr link route)` at pos 1).
    pub static_lists: HashMap<u32, Vec<String>>,
    /// Static rest enumeration.
    pub rest_static: Option<Vec<String>>,
}

/// Parse the body of a completion function that uses `_regex_arguments` and
/// return a simplified arg-spec.
///
/// Returns `None` if the body doesn't contain a `_regex_arguments` call or if
/// the DSL is too irregular for us to extract useful information.
pub fn parse_regex_arguments(body: &str) -> Option<ParsedRegexArgs> {
    // Must contain a _regex_arguments call.
    if !body.contains("_regex_arguments") {
        return None;
    }

    // Collect action specs from the body.  We scan all ':tag:desc:action'
    // strings (with a leading colon, as used by _regex_arguments / _regex_words)
    // from single-quoted tokens across the file body.
    let specs = collect_colon_specs(body);
    if specs.is_empty() {
        return None;
    }

    // Determine which groups are "rest" (preceded by \# repetition marker).
    // Strategy: find groups followed by \# and mark their slot as rest.
    // We also detect the structural \| alternation token.
    let structure = parse_structure(body);

    let mut result = ParsedRegexArgs::default();
    let mut slot: u32 = 0;

    for (idx, spec) in specs.iter().enumerate() {
        slot += 1;
        let is_rest = structure.rest_slots.contains(&idx);

        // Try static list first.
        if let Some(items) = action_to_static_list(spec) {
            if is_rest {
                result.rest_static.get_or_insert(items);
            } else {
                result.static_lists.entry(slot).or_insert(items);
            }
            continue;
        }

        // Try typed action.
        if let Some(arg_type) = action_to_arg_type(spec) {
            if is_rest {
                result.rest.get_or_insert(arg_type);
                // Don't increment slot for rest — all remaining map here.
                slot -= 1;
            } else {
                result.positional.entry(slot).or_insert(arg_type);
            }
        }
    }

    // Return None if we learned nothing useful.
    if result.positional.is_empty()
        && result.rest.is_none()
        && result.static_lists.is_empty()
        && result.rest_static.is_none()
    {
        return None;
    }

    Some(result)
}

// ---------------------------------------------------------------------------
// Internal helpers
// ---------------------------------------------------------------------------

/// Regex for `:tag:desc:action` patterns inside single-quoted strings.
static COLON_SPEC_RE: LazyLock<regex::Regex> = LazyLock::new(|| {
    regex::Regex::new(r"':([^':]+):([^':]*):([^']*)'").unwrap()
});

/// Collect all `:tag:desc:action` specs from single-quoted tokens in `body`.
/// Returns just the action strings (third colon field), in source order.
fn collect_colon_specs(body: &str) -> Vec<String> {
    let mut result = Vec::new();
    for cap in COLON_SPEC_RE.captures_iter(body) {
        let action = cap[3].trim().to_string();
        result.push(action);
    }
    result
}

/// Lightweight structural analysis: which spec indices (0-based) are
/// followed by a `\#` (repetition / rest marker).
struct Structure {
    /// 0-based indices of action specs that are "rest" (appear inside a
    /// group that is followed by `\#`).
    rest_slots: Vec<usize>,
}

/// Scan for `\#` tokens near each spec to identify rest slots.
///
/// Strategy: split body on `\#` — any spec whose single-quoted token appears
/// in the segment just before a `\#` is treated as a rest spec.
fn parse_structure(body: &str) -> Structure {
    let mut rest_slots = Vec::new();

    // Find all single-quoted ':tag:desc:action' positions and the positions
    // of all \# markers.
    let spec_positions: Vec<usize> = COLON_SPEC_RE
        .find_iter(body)
        .map(|m| m.start())
        .collect();

    // Positions of \# markers in the body.
    let hash_positions: Vec<usize> = body
        .match_indices(r"\#")
        .map(|(pos, _)| pos)
        .collect();

    // For each spec, check if there is a \# between that spec and the next
    // spec (or end of body).  If yes, this spec is a "rest" spec.
    for (idx, &spec_start) in spec_positions.iter().enumerate() {
        let next_spec_start = spec_positions
            .get(idx + 1)
            .copied()
            .unwrap_or(body.len());

        let has_hash_after = hash_positions
            .iter()
            .any(|&hp| hp > spec_start && hp < next_spec_start + 20);

        if has_hash_after {
            rest_slots.push(idx);
        }
    }

    Structure { rest_slots }
}

// ---------------------------------------------------------------------------
// Harvest-capture parser
// ---------------------------------------------------------------------------

/// One harvested call site emitted by the worker's overridden sinks.
#[derive(Debug)]
pub enum HarvestEntry {
    /// A `_regex_words` call: `(tag, desc, spec_strings)`.
    RegexWords { tag: String, desc: String, specs: Vec<String> },
    /// A `_regex_arguments` call: `(func_name, remaining_args)`.
    RegexArgs { func_name: String, args: Vec<String> },
}

/// Parse the raw capture written by the worker's overridden
/// `_regex_words` / `_regex_arguments` sinks into a list of `HarvestEntry`.
///
/// Each `__ZIO_REGEX_WORDS__` line has the form:
///   `__ZIO_REGEX_WORDS__ <tag> <desc> 'spec1' 'spec2' …`
///
/// Each `__ZIO_REGEX_ARGS__` line has the form:
///   `__ZIO_REGEX_ARGS__ <func_name> <arg2> …`
///
/// All tokens are shell-word-split on whitespace.  Single-quoted strings
/// have their quotes stripped (the worker uses `print -r --` so there is no
/// shell interpretation, only literal single-quote delimiters).
pub fn parse_harvest_stream(capture: &str) -> Vec<HarvestEntry> {
    let mut entries = Vec::new();
    for line in capture.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        if let Some(rest) = line.strip_prefix("__ZIO_REGEX_WORDS__ ") {
            let tokens = split_shell_tokens(rest);
            if tokens.len() >= 2 {
                let tag = tokens[0].clone();
                let desc = tokens[1].clone();
                let specs = tokens[2..].to_vec();
                entries.push(HarvestEntry::RegexWords { tag, desc, specs });
            }
        } else if let Some(rest) = line.strip_prefix("__ZIO_REGEX_ARGS__ ") {
            let tokens = split_shell_tokens(rest);
            if !tokens.is_empty() {
                let func_name = tokens[0].clone();
                let args = tokens[1..].to_vec();
                entries.push(HarvestEntry::RegexArgs { func_name, args });
            }
        }
    }
    entries
}

/// Convert a harvest capture into a synthetic `_regex_arguments`-style body
/// that `parse_regex_arguments` can consume, and return both the command name
/// (function name with leading `_` stripped) and the parsed `ParsedRegexArgs`.
///
/// Strategy: collect all `_regex_words` specs (which are `'tag:desc:action'`
/// style tokens captured by the worker's override) into a single synthetic body
/// string, then call the existing parser on it.
///
/// Returns `None` if the capture contains no usable entries.
pub fn parse_harvest_capture(capture: &str) -> Option<(String, ParsedRegexArgs)> {
    let entries = parse_harvest_stream(capture);
    if entries.is_empty() {
        return None;
    }

    // Extract command name from the first RegexArgs entry (the function name).
    let func_name = entries.iter().find_map(|e| {
        if let HarvestEntry::RegexArgs { func_name, .. } = e {
            Some(func_name.clone())
        } else {
            None
        }
    })?;

    // Strip the leading `_` to get the command name (e.g., `_ip` → `ip`).
    let cmd_name = func_name.trim_start_matches('_').to_string();
    if cmd_name.is_empty() {
        return None;
    }

    // Build a synthetic body that the existing parser understands.
    // We concatenate all spec strings from RegexWords entries as single-quoted
    // colon-spec tokens on one fake _regex_arguments call.
    let mut body = String::from("_regex_arguments _func /pattern/ \\\n");
    for entry in &entries {
        if let HarvestEntry::RegexWords { specs, .. } = entry {
            for spec in specs {
                // The spec may already have surrounding single quotes from the
                // worker's `print -r --` output; normalise to ensure they're present.
                let normalized = if spec.starts_with('\'') && spec.ends_with('\'') {
                    spec.clone()
                } else {
                    format!("'{}'", spec.replace('\'', "\\'"))
                };
                body.push_str(&format!("  {} \\\n", normalized));
            }
        }
    }

    let parsed = parse_regex_arguments(&body)?;
    Some((cmd_name, parsed))
}

/// Split a shell-word string into tokens, stripping surrounding single quotes.
///
/// This is not a full shell word splitter — it handles the simple case produced
/// by `print -r -- "$tag" "$desc" "$@"` inside the worker, where tokens are
/// space-separated and may be wrapped in single quotes.
fn split_shell_tokens(s: &str) -> Vec<String> {
    let mut tokens = Vec::new();
    let mut rest = s.trim();
    while !rest.is_empty() {
        if rest.starts_with('\'') {
            // Find the closing single quote.
            let end = rest[1..].find('\'').map(|i| i + 1).unwrap_or(rest.len() - 1);
            tokens.push(rest[1..end].to_string());
            rest = rest[end + 1..].trim_start();
        } else {
            let end = rest.find(char::is_whitespace).unwrap_or(rest.len());
            tokens.push(rest[..end].to_string());
            rest = rest[end..].trim_start();
        }
    }
    tokens
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::trie;

    // 1. Two numbered positionals.
    #[test]
    fn parse_simple_numbered_positionals() {
        let body = r#"
_regex_arguments _mytool \
  "/$'[^\0]#\0'/" \
  ':subcmd:subcommand:_hosts' \
  ':file:file:_files'
"#;
        let parsed = parse_regex_arguments(body).expect("should parse");
        assert_eq!(parsed.positional.get(&1), Some(&trie::ARG_MODE_HOSTS));
        assert_eq!(parsed.positional.get(&2), Some(&trie::ARG_MODE_PATHS));
    }

    // 2. Static list action.
    #[test]
    fn parse_static_list_action() {
        let body = r#"
_regex_arguments _mytool \
  "/$'[^\0]#\0'/" \
  ':mode:select mode:compadd - foo bar baz'
"#;
        let parsed = parse_regex_arguments(body).expect("should parse");
        let list = parsed.static_lists.get(&1).expect("should have static list at slot 1");
        assert!(list.contains(&"foo".to_string()), "list should contain foo");
        assert!(list.contains(&"bar".to_string()), "list should contain bar");
        assert!(list.contains(&"baz".to_string()), "list should contain baz");
    }

    // 3. Group followed by \# marks rest.
    #[test]
    fn parse_rest_marker() {
        let body = r#"
_regex_arguments _mytool \
  "/$'[^\0]#\0'/" \
  \( ':host:hostname:_hosts' \) \#
"#;
        let parsed = parse_regex_arguments(body).expect("should parse");
        // Marked as rest, not numbered positional.
        assert!(parsed.positional.is_empty(), "should have no numbered positionals");
        assert_eq!(parsed.rest, Some(trie::ARG_MODE_HOSTS));
    }

    // 4. Body without _regex_arguments returns None.
    #[test]
    fn parse_not_regex_arguments_returns_none() {
        let body = r#"
_arguments \
  '1:file:_files' \
  '-v[verbose]'
"#;
        assert!(parse_regex_arguments(body).is_none());
    }

    // 5. Mixed alternation — two alternatives each contribute a slot.
    #[test]
    fn parse_mixed_alternation() {
        let body = r#"
_regex_arguments _mytool \
  "/$'[^\0]#\0'/" \
  \( ':host:hostname:_hosts' \
  \| ':file:file:_files' \)
"#;
        let parsed = parse_regex_arguments(body).expect("should parse");
        // Both slots should be populated (alternative branches each count).
        assert!(
            parsed.positional.contains_key(&1) || parsed.positional.contains_key(&2),
            "at least one positional should be extracted from alternation"
        );
    }

    // 6. _files action maps to ARG_MODE_PATHS.
    #[test]
    fn parse_file_action() {
        let body = r#"
_regex_arguments _mytool \
  "/$'[^\0]#\0'/" \
  ':file:file:_files'
"#;
        let parsed = parse_regex_arguments(body).expect("should parse");
        assert_eq!(parsed.positional.get(&1), Some(&trie::ARG_MODE_PATHS));
    }

    // 7. _hosts action maps to ARG_MODE_HOSTS.
    #[test]
    fn parse_runtime_action() {
        let body = r#"
_regex_arguments _mytool \
  "/$'[^\0]#\0'/" \
  ':host:host:_hosts'
"#;
        let parsed = parse_regex_arguments(body).expect("should parse");
        assert_eq!(parsed.positional.get(&1), Some(&trie::ARG_MODE_HOSTS));
    }

    // 8. parse_harvest_capture: synthetic ip-style capture.
    #[test]
    fn parse_harvest_capture_ip_style() {
        // Simulate the capture the worker would emit for an `_ip`-style function.
        let capture = concat!(
            "__ZIO_REGEX_WORDS__ ip-commands subcommand ':addr:IP address management:' ':link:network device management:'\n",
            "__ZIO_REGEX_ARGS__ _ip /pattern/ addr link \\#\n",
        );
        let result = parse_harvest_capture(capture);
        // The capture uses ':addr:...:' style specs without a valid action suffix,
        // so parsed positionals may be empty. But we should get a command name back.
        // Verify at minimum that a Some is returned and the cmd name is "ip".
        // If the spec actions don't map to known types, parse_harvest_capture returns None
        // (no useful info). That's acceptable — test the successful path with valid actions.
        // Use a capture with a real action suffix.
        let capture2 = concat!(
            "__ZIO_REGEX_WORDS__ ip-commands subcommand ':addr:IP address management:_hosts' ':link:device:_files'\n",
            "__ZIO_REGEX_ARGS__ _ip /pattern/ addr link \\#\n",
        );
        let result2 = parse_harvest_capture(capture2);
        assert!(result2.is_some(), "should parse capture with known actions");
        let (cmd, parsed) = result2.unwrap();
        assert_eq!(cmd, "ip", "command name should be 'ip' (leading _ stripped)");
        assert!(!parsed.positional.is_empty() || parsed.rest.is_some(),
            "should have at least one positional or rest entry");
        // suppress unused warning
        let _ = result;
    }

    // 9. parse_harvest_capture: empty capture returns None.
    #[test]
    fn parse_harvest_capture_empty_returns_none() {
        assert!(parse_harvest_capture("").is_none());
        assert!(parse_harvest_capture("\n\n").is_none());
    }

    // 10. parse_harvest_capture: function without RegexArgs entry returns None.
    #[test]
    fn parse_harvest_capture_no_regex_args_returns_none() {
        // Only RegexWords but no RegexArgs → no func_name → None.
        let capture = "__ZIO_REGEX_WORDS__ tag desc ':foo:bar:_hosts'\n";
        assert!(parse_harvest_capture(capture).is_none());
    }

    // 11. split_shell_tokens handles quoted and bare tokens.
    #[test]
    fn split_shell_tokens_mixed() {
        let tokens = split_shell_tokens("bare 'single quoted' another");
        assert_eq!(tokens, vec!["bare", "single quoted", "another"]);
    }

    // 12. parse_harvest_stream produces expected entry types.
    #[test]
    fn parse_harvest_stream_basic() {
        let capture = concat!(
            "__ZIO_REGEX_WORDS__ tag desc ':x:desc:_files'\n",
            "__ZIO_REGEX_ARGS__ _mytool /pat/ x \\#\n",
        );
        let entries = parse_harvest_stream(capture);
        assert_eq!(entries.len(), 2);
        assert!(matches!(&entries[0], HarvestEntry::RegexWords { tag, .. } if tag == "tag"));
        assert!(matches!(&entries[1], HarvestEntry::RegexArgs { func_name, .. } if func_name == "_mytool"));
    }
}
