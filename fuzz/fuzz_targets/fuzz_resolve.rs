//! Fuzz the resolution engine: resolve_line and complete.
//!
//! These are the hot paths called on every Enter / Tab keypress. The trie
//! is pre-populated with a fixed set of realistic commands so the fuzzer
//! can exercise the full trie-walk, scoring, disambiguation, and path-
//! resolution code paths rather than always hitting the empty-trie fast exit.
//!
//! Run with:
//!   cargo +nightly fuzz run fuzz_resolve -- -max_len=256 -timeout=5
#![no_main]
use arbitrary::Arbitrary;
use libfuzzer_sys::fuzz_target;
use std::sync::OnceLock;
use zsh_ios::pins::Pins;
use zsh_ios::resolve::ContextHint;
use zsh_ios::trie::CommandTrie;

/// A shared trie pre-populated with realistic commands, built once.
fn shared_trie() -> &'static CommandTrie {
    static TRIE: OnceLock<CommandTrie> = OnceLock::new();
    TRIE.get_or_init(|| {
        let mut t = CommandTrie::default();
        // Git subcommands — exercises multi-level trie walk + disambiguation
        for sub in &["branch", "checkout", "commit", "clone", "diff", "fetch",
                     "log", "merge", "pull", "push", "rebase", "reset",
                     "stash", "status", "tag"] {
            t.insert(&["git", sub]);
        }
        // cargo
        for sub in &["build", "check", "clean", "clippy", "doc", "fmt",
                     "install", "publish", "run", "test", "update"] {
            t.insert(&["cargo", sub]);
        }
        // docker
        for sub in &["build", "exec", "images", "inspect", "kill", "logs",
                     "ps", "pull", "push", "rm", "run", "start", "stop"] {
            t.insert(&["docker", sub]);
        }
        // plain commands
        for cmd in &["grep", "sed", "awk", "find", "ls", "cat", "echo",
                     "ssh", "scp", "curl", "wget", "tar", "zip", "unzip"] {
            t.insert(&[cmd]);
        }
        t
    })
}

#[derive(Arbitrary, Debug)]
struct ResolveInput {
    /// The abbreviated command line the user typed.
    line: String,
    /// Simulated current working directory (affects cwd scoring).
    cwd: String,
    /// Which context the cursor is in.
    context: u8,
}

fuzz_target!(|input: ResolveInput| {
    // Cap sizes to keep the fuzzer fast.
    if input.line.len() > 200 { return; }

    let trie = shared_trie();
    let pins = Pins::default();

    let ctx = match input.context % 4 {
        0 => ContextHint::Unknown,
        1 => ContextHint::Command,
        2 => ContextHint::Argument,
        _ => ContextHint::Redirection,
    };
    let cwd = if input.cwd.is_empty() { None } else { Some(input.cwd.as_str()) };

    // resolve_line must never panic.
    let _ = zsh_ios::resolve::resolve_line(&input.line, trie, &pins, cwd, ctx.clone());

    // complete must never panic.
    let _ = zsh_ios::resolve::complete(&input.line, trie, &pins, ctx);
});
