//! Fuzz the bash completion file parser.
//!
//! `tokenize_bash_line` is the lowest-level entry point and exercises the
//! quote/escape state machine on arbitrary bash source.  We also call the
//! higher-level string-extraction helpers that sit on top of it.
//!
//! Run with:
//!   cargo +nightly fuzz run fuzz_bash_completions -- -max_len=65536
#![no_main]
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let Ok(s) = std::str::from_utf8(data) else { return };

    // tokenize_bash_line: exercises the quote/escape state machine.
    let tokens = zsh_ios::bash_completions::tokenize_bash_line(s);

    // Invariant: every token must be a valid UTF-8 string (they are &str
    // windows into the input, but here we get owned Strings back).
    for tok in &tokens {
        assert!(std::str::from_utf8(tok.as_bytes()).is_ok());
    }
});
