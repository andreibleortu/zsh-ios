//! Fuzz the ingest section parser.
//!
//! `split_sections` must never panic on arbitrary input: section headers must
//! only be recognised when they match a known name, and body lines starting
//! with `@` must not corrupt the parse.
//!
//! Run with:
//!   cargo fuzz run fuzz_ingest -- -max_len=65536
#![no_main]
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let Ok(s) = std::str::from_utf8(data) else { return };

    let sections = zsh_ios::ingest::split_sections(s);

    // Verify the invariant: every key returned must be a known section name.
    const KNOWN: &[&str] = &[
        "aliases", "galiases", "saliases", "functions", "nameddirs",
        "history", "dirstack", "jobs", "commands", "parameters",
        "options", "widgets", "modules", "zstyle",
    ];
    for key in sections.keys() {
        assert!(
            KNOWN.contains(key),
            "split_sections returned unknown section name: {key:?}"
        );
    }

    // Exercise apply_ingest on an in-memory trie (no disk I/O).
    let mut trie = zsh_ios::trie::CommandTrie::default();
    zsh_ios::ingest::apply_ingest(&mut trie, s);
});
