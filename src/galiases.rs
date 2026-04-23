//! Global-alias expansion.
//!
//! Zsh global aliases (`alias -g NAME=VALUE`) expand anywhere on the command
//! line, not just at command position. This module provides `expand_galiases`
//! which performs that substitution token-by-token before the trie walk so
//! that the engine sees the expanded form.
//!
//! Expansion is intentionally NOT recursive: if `G='| grep'` and `grep` is
//! itself a galias key, a single pass through the input expands `G` to
//! `| grep` and stops there.

use std::collections::HashMap;

/// Expand global aliases in `input`, substituting whole-word tokens whose text
/// exactly matches a key in `galiases`.
///
/// Tokens inside single-quoted strings, double-quoted strings, `$(...)`,
/// `` `...` ``, or `${...}` are never expanded.  Operator characters that
/// appear in an expansion value (`|`, `&&`, `;`, redirects) pass through
/// verbatim so the engine's split-on-operators step sees them downstream.
pub fn expand_galiases(input: &str, galiases: &HashMap<String, String>) -> String {
    if galiases.is_empty() {
        return input.to_string();
    }

    let mut out = String::with_capacity(input.len() + 32);
    let mut token = String::new();
    let mut chars = input.chars().peekable();
    let mut quote: Option<char> = None; // Some('\'') or Some('"')
    let mut paren_depth: u32 = 0; // inside $( ... )
    let mut brace_depth: u32 = 0; // inside ${ ... }
    let mut backtick = false;
    let mut escape = false;

    while let Some(c) = chars.next() {
        if escape {
            token.push(c);
            escape = false;
            continue;
        }

        match c {
            '\\' => {
                token.push(c);
                escape = true;
            }
            '\'' if quote.is_none() && paren_depth == 0 && brace_depth == 0 && !backtick => {
                quote = Some('\'');
                token.push(c);
            }
            '"' if quote.is_none() && paren_depth == 0 && brace_depth == 0 && !backtick => {
                quote = Some('"');
                token.push(c);
            }
            '\'' if quote == Some('\'') => {
                quote = None;
                token.push(c);
            }
            '"' if quote == Some('"') => {
                quote = None;
                token.push(c);
            }
            '`' if quote.is_none() && paren_depth == 0 && brace_depth == 0 => {
                backtick = !backtick;
                token.push(c);
            }
            '$' if quote.is_none() && !backtick => {
                if chars.peek() == Some(&'(') {
                    chars.next();
                    paren_depth += 1;
                    token.push('$');
                    token.push('(');
                } else if chars.peek() == Some(&'{') {
                    chars.next();
                    brace_depth += 1;
                    token.push('$');
                    token.push('{');
                } else {
                    token.push(c);
                }
            }
            ')' if paren_depth > 0 => {
                paren_depth -= 1;
                token.push(c);
            }
            '}' if brace_depth > 0 => {
                brace_depth -= 1;
                token.push(c);
            }
            c2 if c2.is_whitespace()
                && quote.is_none()
                && paren_depth == 0
                && brace_depth == 0
                && !backtick =>
            {
                // Token boundary: look up and maybe expand.
                if let Some(expansion) = galiases.get(&token) {
                    out.push_str(expansion);
                } else {
                    out.push_str(&token);
                }
                token.clear();
                out.push(c2);
            }
            other => token.push(other),
        }
    }

    // Flush trailing token.
    if !token.is_empty() {
        if let Some(expansion) = galiases.get(&token) {
            out.push_str(expansion);
        } else {
            out.push_str(&token);
        }
    }

    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn gmap(pairs: &[(&str, &str)]) -> HashMap<String, String> {
        pairs.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect()
    }

    #[test]
    fn expand_simple() {
        let ga = gmap(&[("G", "| grep")]);
        assert_eq!(
            expand_galiases("find . -type f G pat", &ga),
            "find . -type f | grep pat"
        );
    }

    #[test]
    fn no_expansion_inside_single_quotes() {
        let ga = gmap(&[("G", "| grep")]);
        assert_eq!(
            expand_galiases("echo 'G foo'", &ga),
            "echo 'G foo'"
        );
    }

    #[test]
    fn no_expansion_inside_double_quotes() {
        let ga = gmap(&[("G", "| grep")]);
        assert_eq!(
            expand_galiases("echo \"G foo\"", &ga),
            "echo \"G foo\""
        );
    }

    #[test]
    fn no_expansion_inside_command_subst() {
        let ga = gmap(&[("G", "| grep")]);
        assert_eq!(
            expand_galiases("echo $(cmd G arg)", &ga),
            "echo $(cmd G arg)"
        );
    }

    #[test]
    fn no_expansion_inside_backticks() {
        let ga = gmap(&[("G", "| grep")]);
        assert_eq!(
            expand_galiases("echo `cmd G arg`", &ga),
            "echo `cmd G arg`"
        );
    }

    #[test]
    fn no_expansion_inside_braces() {
        let ga = gmap(&[("VAR", "replaced")]);
        assert_eq!(
            expand_galiases("echo ${VAR}", &ga),
            "echo ${VAR}"
        );
    }

    #[test]
    fn no_partial_match() {
        let ga = gmap(&[("G", "| grep")]);
        // "Ghost" is not "G" — no expansion.
        assert_eq!(
            expand_galiases("echo Ghost", &ga),
            "echo Ghost"
        );
    }

    #[test]
    fn expand_at_start() {
        let ga = gmap(&[("G", "| grep")]);
        assert_eq!(expand_galiases("G", &ga), "| grep");
    }

    #[test]
    fn expand_multiple() {
        let ga = gmap(&[("G", "| grep"), ("L", "| less")]);
        assert_eq!(
            expand_galiases("find . G foo L", &ga),
            "find . | grep foo | less"
        );
    }

    #[test]
    fn no_recursive_expansion() {
        // G expands to "H"; H is also a galias, but we do only one pass.
        let ga = gmap(&[("G", "H"), ("H", "X")]);
        assert_eq!(expand_galiases("G", &ga), "H");
    }

    #[test]
    fn empty_galiases_returns_input() {
        let ga = HashMap::new();
        let input = "find . -type f G pat";
        let result = expand_galiases(input, &ga);
        // Must be equal AND must be a fresh allocation (ptr may differ — just check equality)
        assert_eq!(result, input);
    }

    #[test]
    fn escaped_token_not_expanded() {
        // A backslash before a character keeps it in the token verbatim,
        // so "\G" does not match key "G".
        let ga = gmap(&[("G", "| grep")]);
        // \G → the token is \G (backslash then G), not "G".
        assert_eq!(expand_galiases("echo \\G foo", &ga), "echo \\G foo");
    }

    #[test]
    fn double_quoted_section_roundtrips() {
        // Content inside "..." must come through unchanged.
        let ga = gmap(&[("hello", "world")]);
        assert_eq!(
            expand_galiases("echo \"hello world\"", &ga),
            "echo \"hello world\""
        );
    }

    #[test]
    fn expand_trailing_whitespace_preserved() {
        // Trailing spaces should survive the token-flush path.
        let ga = gmap(&[("G", "| grep")]);
        assert_eq!(expand_galiases("ls G ", &ga), "ls | grep ");
    }
}
