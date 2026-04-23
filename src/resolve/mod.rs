//! Abbreviation-resolution subsystem.
//!
//! The old `resolve.rs` had grown past 3000 lines housing three distinct
//! responsibilities that happen to share helpers. Splitting keeps each
//! file under a thousand lines and makes the division explicit:
//!
//!   • `engine`   — trie walk, pin lookup, deep disambiguation, path
//!                  resolution, arg-spec application, the `explain`
//!                  narrator.
//!   • `complete` — the `?` key completion path.
//!   • `escape`   — shell-quoting helpers for paths spliced back into a
//!                  buffer.
//!
//! External callers should keep using `crate::resolve::<Item>` — the
//! public API is re-exported here.

mod complete;
mod engine;
mod escape;

pub use complete::complete;
pub use engine::{
    explain, resolve_line, set_statistics_disabled, AmbiguityInfo, ContextHint, DeepCandidate,
    ResolveResult,
};
