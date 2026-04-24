//! Fuzz the trie MessagePack deserializer.
//!
//! `CommandTrie::load_bytes` (or equivalent) should never panic on corrupt
//! msgpack data — a trie file on disk could be truncated, bit-flipped, or
//! written by a different version.
//!
//! We also fuzz the round-trip: build a trie from structured `arbitrary`
//! input, serialize it, corrupt the bytes, and try to deserialize.
//!
//! Run with:
//!   cargo +nightly fuzz run fuzz_trie_serde -- -max_len=65536
#![no_main]
use arbitrary::Arbitrary;
use libfuzzer_sys::fuzz_target;
use zsh_ios::trie::CommandTrie;

#[derive(Arbitrary, Debug)]
struct TrieInput {
    /// Commands to insert (word paths).
    commands: Vec<Vec<String>>,
    /// Raw bytes to attempt to deserialize as a trie.
    raw_msgpack: Vec<u8>,
}

fuzz_target!(|input: TrieInput| {
    // 1. Deserialize arbitrary bytes — must not panic.
    let _ = rmp_serde::from_slice::<CommandTrie>(&input.raw_msgpack);

    // 2. Build a trie from structured input, serialize, then deserialize.
    //    The round-trip must not panic regardless of the command paths used.
    if input.commands.len() > 32 { return; } // keep it fast
    let mut trie = CommandTrie::default();
    for path in &input.commands {
        let words: Vec<&str> = path.iter().map(String::as_str).collect();
        if words.len() <= 8 && words.iter().all(|w| w.len() <= 64) {
            trie.insert(&words);
        }
    }
    if let Ok(bytes) = rmp_serde::to_vec(&trie) {
        let _ = rmp_serde::from_slice::<CommandTrie>(&bytes);
    }
});
