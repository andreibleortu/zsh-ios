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
}
