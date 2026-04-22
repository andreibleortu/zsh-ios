use std::collections::HashSet;
use std::fs;
use std::io::{self, BufRead};
use std::os::unix::fs::PermissionsExt;

use crate::history::split_command_segments;
use crate::trie::CommandTrie;

/// Scan all directories in PATH for executable files, adding them as
/// first-level commands in the trie.
pub fn scan_path(trie: &mut CommandTrie) -> u32 {
    let path_var = std::env::var("PATH").unwrap_or_default();
    let mut seen = HashSet::new();
    let mut count = 0u32;

    for dir in path_var.split(':') {
        if dir.is_empty() {
            continue;
        }
        let entries = match fs::read_dir(dir) {
            Ok(e) => e,
            Err(_) => continue,
        };
        for entry in entries.flatten() {
            let Ok(meta) = entry.metadata() else {
                continue;
            };
            // entry.metadata() follows symlinks — symlinks to executable files
            // are caught by is_file(); broken symlinks fail at metadata() above.
            if !meta.is_file() {
                continue;
            }
            if meta.permissions().mode() & 0o111 == 0 {
                continue;
            }
            let name = entry.file_name();
            let name = name.to_string_lossy();
            if name.starts_with('.') {
                continue;
            }
            if seen.insert(name.to_string()) {
                trie.insert_command(&name);
                count += 1;
            }
        }
    }

    count
}

/// Known Zsh builtins to add to the trie.
const ZSH_BUILTINS: &[&str] = &[
    "alias",
    "autoload",
    "bg",
    "bindkey",
    "break",
    "builtin",
    "bye",
    "cd",
    "chdir",
    "command",
    "compctl",
    "compadd",
    "compdef",
    "continue",
    "declare",
    "dirs",
    "disable",
    "disown",
    "echo",
    "emulate",
    "enable",
    "eval",
    "exec",
    "exit",
    "export",
    "false",
    "fc",
    "fg",
    "float",
    "functions",
    "getln",
    "getopts",
    "hash",
    "history",
    "integer",
    "jobs",
    "kill",
    "let",
    "limit",
    "local",
    "log",
    "logout",
    "noglob",
    "popd",
    "print",
    "printf",
    "pushd",
    "pushln",
    "pwd",
    "read",
    "readonly",
    "rehash",
    "return",
    "sched",
    "set",
    "setopt",
    "shift",
    "source",
    "suspend",
    "test",
    "times",
    "trap",
    "true",
    "ttyctl",
    "type",
    "typeset",
    "ulimit",
    "umask",
    "unalias",
    "unfunction",
    "unhash",
    "unlimit",
    "unset",
    "unsetopt",
    "vared",
    "wait",
    "whence",
    "where",
    "which",
    "zcompile",
    "zformat",
    "zle",
    "zmodload",
    "zparseopts",
    "zregexparse",
    "zstyle",
    // Additional builtins
    "coproc",
    "repeat",
    "select",
    "nocorrect",
    "zpty",
    "zstat",
    "scalar",
    "array",
    "assoc",
];

/// Add Zsh builtins to the trie.
pub fn add_builtins(trie: &mut CommandTrie) -> u32 {
    for builtin in ZSH_BUILTINS {
        trie.insert_command(builtin);
    }
    ZSH_BUILTINS.len() as u32
}

/// Parse alias definitions from stdin (output of `alias` command).
/// Format: `name=value` or `name='value'`.
///
/// Adds both the alias name as a first-level command AND parses the alias
/// value as a command sequence so the underlying commands are learnable
/// (e.g., `tfa='terraform apply -auto-approve'` teaches the trie about
/// `terraform -> apply`).
pub fn parse_aliases(reader: impl BufRead, trie: &mut CommandTrie) -> u32 {
    let mut count = 0u32;
    for line in reader.lines() {
        let Ok(line) = line else { continue };
        let line = line.trim().to_string();
        if line.is_empty() {
            continue;
        }
        if let Some((name, value)) = line.split_once('=') {
            let name = name.trim();
            if !name.is_empty() && !name.contains(' ') {
                trie.insert_command(name);
                count += 1;

                // Also learn the alias value as command sequences,
                // splitting on ; | && just like the history parser does.
                let value = value.trim().trim_matches('\'').trim_matches('"');
                for segment in split_command_segments(value) {
                    let words: Vec<&str> = segment.split_whitespace().collect();
                    if words.len() >= 2 {
                        trie.insert(&words);
                    }
                }
            }
        }
    }
    count
}

/// Parse aliases from a file or stdin, depending on the argument.
pub fn parse_aliases_from_stdin(trie: &mut CommandTrie) -> u32 {
    let stdin = io::stdin();
    parse_aliases(stdin.lock(), trie)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_aliases() {
        let input = b"dns='sudo dscacheutil -flushcache; sudo killall -HUP mDNSResponder'\nll='ls -la'\ntfa='terraform apply'\n";
        let cursor = io::Cursor::new(input);
        let mut trie = CommandTrie::new();
        let count = parse_aliases(io::BufReader::new(cursor), &mut trie);

        assert_eq!(count, 3);
        // Alias names are added as commands
        assert!(trie.root.get_child("dns").is_some());
        assert!(trie.root.get_child("ll").is_some());
        assert!(trie.root.get_child("tfa").is_some());

        // Alias values are learned as command sequences
        let terraform = trie
            .root
            .get_child("terraform")
            .expect("terraform should be learned from tfa alias");
        assert!(terraform.get_child("apply").is_some());

        // Semicolon-separated commands in alias values are split properly
        let sudo = trie
            .root
            .get_child("sudo")
            .expect("sudo should be learned from dns alias");
        assert!(
            sudo.get_child("dscacheutil").is_some(),
            "first segment: sudo dscacheutil"
        );
        assert!(
            sudo.get_child("killall").is_some(),
            "second segment: sudo killall"
        );

        // dscacheutil's subcommand should NOT have a trailing semicolon
        let dscacheutil = sudo.get_child("dscacheutil").unwrap();
        assert!(dscacheutil.get_child("-flushcache").is_some());
        assert!(
            dscacheutil.get_child("-flushcache;").is_none(),
            "semicolon should not be part of the flag"
        );
    }

    #[test]
    fn test_builtins() {
        let mut trie = CommandTrie::new();
        let count = add_builtins(&mut trie);
        assert!(count > 50);
        assert!(trie.root.get_child("cd").is_some());
        assert!(trie.root.get_child("echo").is_some());
    }

    #[test]
    fn test_parse_aliases_skips_invalid_lines() {
        let input = b"\
# a comment line
just-a-word
=leading-equals
name with space=value
ok='value'
";
        let cur = io::Cursor::new(input);
        let mut trie = CommandTrie::new();
        let count = parse_aliases(io::BufReader::new(cur), &mut trie);
        assert_eq!(count, 1, "only `ok` is a valid alias line");
        assert!(trie.root.get_child("ok").is_some());
    }

    #[test]
    fn test_parse_aliases_strips_surrounding_quotes() {
        let input = b"a='git status'\nb=\"git log\"\nc=git\\ status\n";
        let cur = io::Cursor::new(input);
        let mut trie = CommandTrie::new();
        parse_aliases(io::BufReader::new(cur), &mut trie);
        // Both quote styles should have taught `git status` and `git log`.
        let git = trie.root.get_child("git").unwrap();
        assert!(git.get_child("status").is_some());
        assert!(git.get_child("log").is_some());
    }

    #[test]
    fn test_parse_aliases_single_word_value_not_learned_as_sequence() {
        // Single-word values shouldn't insert a sequence into the trie — only
        // the alias name itself becomes a command.
        let input = b"k=kubectl\n";
        let cur = io::Cursor::new(input);
        let mut trie = CommandTrie::new();
        parse_aliases(io::BufReader::new(cur), &mut trie);
        assert!(trie.root.get_child("k").is_some());
        // kubectl is NOT added because it's the only word; the alias-value
        // split requires 2+ words to become a trie sequence.
        assert!(trie.root.get_child("kubectl").is_none());
    }

    #[test]
    fn test_parse_aliases_with_pipe_chain() {
        let input = b"x='ls -la | grep foo'\n";
        let cur = io::Cursor::new(input);
        let mut trie = CommandTrie::new();
        parse_aliases(io::BufReader::new(cur), &mut trie);
        // Each pipeline segment becomes its own sequence.
        let ls = trie.root.get_child("ls").expect("ls learned");
        assert!(ls.get_child("-la").is_some());
        let grep = trie.root.get_child("grep").expect("grep learned");
        assert!(grep.get_child("foo").is_some());
    }

    #[test]
    fn test_scan_path_finds_executables_in_tmpdir() {
        let _g = crate::test_util::CWD_LOCK.lock().unwrap();
        // Build a fake PATH pointing at a tempdir containing one executable.
        let td = tempfile::tempdir().unwrap();
        let exe = td.path().join("my-fake-cmd");
        std::fs::write(&exe, "#!/bin/sh\n").unwrap();
        let mut perms = std::fs::metadata(&exe).unwrap().permissions();
        perms.set_mode(0o755);
        std::fs::set_permissions(&exe, perms).unwrap();

        // Drop a hidden file and a non-executable file to confirm they're filtered.
        std::fs::write(td.path().join(".hidden"), "#!/bin/sh\n").unwrap();
        let ro = td.path().join("not-executable");
        std::fs::write(&ro, "x").unwrap();
        let mut p = std::fs::metadata(&ro).unwrap().permissions();
        p.set_mode(0o644);
        std::fs::set_permissions(&ro, p).unwrap();

        // Take the lock on PATH; tests can run in parallel but we restore
        // the original PATH before scan returns so interleaving is fine —
        // we just need our dir visible for this process during scan_path.
        //
        // SAFETY: set_var is `unsafe` in recent Rust editions because other
        // threads may read PATH concurrently. The test binary is a
        // controlled single-threaded call site here.
        let orig = std::env::var("PATH").unwrap_or_default();
        // SAFETY: see above.
        unsafe {
            std::env::set_var("PATH", td.path());
        }

        let mut trie = CommandTrie::new();
        let count = scan_path(&mut trie);

        // SAFETY: restore for the rest of the process.
        unsafe {
            std::env::set_var("PATH", &orig);
        }

        assert_eq!(count, 1, "only the one executable file should be counted");
        assert!(trie.root.get_child("my-fake-cmd").is_some());
        assert!(trie.root.get_child(".hidden").is_none());
        assert!(trie.root.get_child("not-executable").is_none());
    }
}
