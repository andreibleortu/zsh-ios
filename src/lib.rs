//! Library surface of the `zsh-ios` crate.
//!
//! This crate is used two ways: the `zsh-ios` binary (in `src/main.rs`)
//! calls these modules to implement each CLI subcommand, and the
//! integration tests in `tests/` can link against them directly for
//! fine-grained checks that don't need to shell out to the built binary.
//!
//! All modules are re-exported as `pub` — there's no concept of "private
//! to the binary" here. Anything that needs to stay hidden should use
//! `pub(crate)` inside its own module.

pub mod bash_completions;
pub mod completions;
pub mod config;
pub mod galiases;
pub mod fish_completions;
pub mod history;
pub mod ingest;
pub mod locks;
pub mod path_resolve;
pub mod pins;
pub mod presets;
pub mod regex_args;
pub mod resolve;
pub mod runtime_cache;
pub mod runtime_complete;
pub mod runtime_config;
pub mod scanner;
pub mod trie;
pub mod type_resolver;
pub mod user_config;

#[cfg(test)]
pub mod test_util {
    //! Cross-module test helpers.
    //!
    //! The unit test suite touches process-global state — current working
    //! directory, `PATH` — that rustc's default parallel runner will race on.
    //! Any test that mutates `cwd` or `$PATH` must take `CWD_LOCK` for the
    //! duration of its work.
    use std::sync::Mutex;

    pub static CWD_LOCK: Mutex<()> = Mutex::new(());
}
