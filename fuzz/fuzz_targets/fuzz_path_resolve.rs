//! Fuzz the path-abbreviation resolver with structured inputs.
//!
//! `resolve_path` is called on user-typed strings and must never panic.
//! Using `arbitrary` gives the fuzzer structured (path-like) inputs that
//! exercise the `!`, `*`, `~N`, named-dir, and dirstack code paths far more
//! efficiently than raw bytes.
//!
//! Run with:
//!   cargo fuzz run fuzz_path_resolve -- -max_len=4096
#![no_main]
use arbitrary::Arbitrary;
use libfuzzer_sys::fuzz_target;
use std::collections::HashMap;

/// Structured input so the fuzzer can generate valid-ish path strings.
#[derive(Arbitrary, Debug)]
struct PathFuzzInput {
    /// The abbreviated path to resolve.
    abbreviated: String,
    /// Named-dir entries to populate (key → absolute path).
    named_dirs: Vec<(String, String)>,
    /// A small dir-stack (absolute paths).
    dir_stack: Vec<String>,
}

fuzz_target!(|input: PathFuzzInput| {
    // Cap sizes so the fuzzer doesn't spend all its time on huge inputs.
    if input.abbreviated.len() > 512 { return; }
    if input.named_dirs.len() > 8 { return; }
    if input.dir_stack.len() > 8 { return; }

    let named_dirs: HashMap<String, String> = input
        .named_dirs
        .into_iter()
        .filter(|(k, v)| !k.is_empty() && !v.is_empty())
        .take(8)
        .collect();

    let dir_stack: Vec<String> = input
        .dir_stack
        .into_iter()
        .filter(|p| !p.is_empty())
        .take(8)
        .collect();

    // Must never panic.
    let _ = zsh_ios::path_resolve::resolve_path(&input.abbreviated, &named_dirs, &dir_stack);
    let _ = zsh_ios::path_resolve::resolve_path_dirs_only(&input.abbreviated, &named_dirs, &dir_stack);
});
