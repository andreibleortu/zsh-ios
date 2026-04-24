//! Fuzz the _regex_arguments DSL parser.
//!
//! `parse_regex_arguments` tokenizes and walks the Zsh `_regex_arguments`
//! DSL — a hand-rolled parser that converts positional regex patterns into
//! ArgSpec types.  Completion files use this for commands like ssh, rsync,
//! and many others.
//!
//! Run with:
//!   cargo +nightly fuzz run fuzz_regex_args -- -max_len=32768
#![no_main]
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let Ok(s) = std::str::from_utf8(data) else { return };

    // Must never panic on arbitrary DSL body content.
    let _ = zsh_ios::regex_args::parse_regex_arguments(s);
    let _ = zsh_ios::regex_args::parse_harvest_stream(s);
});
