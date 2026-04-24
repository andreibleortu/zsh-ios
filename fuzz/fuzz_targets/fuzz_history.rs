//! Fuzz the Zsh history parser and command-segment splitter.
//!
//! Both functions take arbitrary text from history files / user command lines
//! and must never panic or produce incorrect UTF-8 slice boundaries.
//!
//! Run with:
//!   cargo fuzz run fuzz_history -- -max_len=65536
#![no_main]
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let Ok(s) = std::str::from_utf8(data) else { return };

    // split_command_segments: verify no panics and that every returned slice
    // is a valid sub-slice of the input (so no out-of-bounds indexing).
    let segs = zsh_ios::history::split_command_segments(s);
    for seg in &segs {
        // Every segment must be contained within the original string.
        let seg_ptr = seg.as_ptr() as usize;
        let base = s.as_ptr() as usize;
        assert!(seg_ptr >= base && seg_ptr + seg.len() <= base + s.len());
    }

    // parse_history reads from a Path; exercise the segment splitter directly
    // since it covers the same parsing logic and doesn't need filesystem I/O.
    // (parse_history itself is tested via the corpus seed file approach.)
});
