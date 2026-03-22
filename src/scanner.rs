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
            if !meta.is_file() && !meta.file_type().is_symlink() {
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
    "alias", "autoload", "bg", "bindkey", "break", "builtin", "bye", "cd",
    "chdir", "command", "compctl", "compadd", "compdef", "continue", "declare",
    "dirs", "disable", "disown", "echo", "emulate", "enable", "eval", "exec",
    "exit", "export", "false", "fc", "fg", "float", "functions", "getln",
    "getopts", "hash", "history", "integer", "jobs", "kill", "let", "limit",
    "local", "log", "logout", "noglob", "popd", "print", "printf", "pushd",
    "pushln", "pwd", "read", "readonly", "rehash", "return", "sched", "set",
    "setopt", "shift", "source", "suspend", "test", "times", "trap", "true",
    "ttyctl", "type", "typeset", "ulimit", "umask", "unalias", "unfunction",
    "unhash", "unlimit", "unset", "unsetopt", "vared", "wait", "whence",
    "where", "which", "zcompile", "zformat", "zle", "zmodload", "zparseopts",
    "zregexparse", "zstyle",
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
        let terraform = trie.root.get_child("terraform").expect("terraform should be learned from tfa alias");
        assert!(terraform.get_child("apply").is_some());

        // Semicolon-separated commands in alias values are split properly
        let sudo = trie.root.get_child("sudo").expect("sudo should be learned from dns alias");
        assert!(sudo.get_child("dscacheutil").is_some(), "first segment: sudo dscacheutil");
        assert!(sudo.get_child("killall").is_some(), "second segment: sudo killall");

        // dscacheutil's subcommand should NOT have a trailing semicolon
        let dscacheutil = sudo.get_child("dscacheutil").unwrap();
        assert!(dscacheutil.get_child("-flushcache").is_some());
        assert!(dscacheutil.get_child("-flushcache;").is_none(), "semicolon should not be part of the flag");
    }

    #[test]
    fn test_builtins() {
        let mut trie = CommandTrie::new();
        let count = add_builtins(&mut trie);
        assert!(count > 50);
        assert!(trie.root.get_child("cd").is_some());
        assert!(trie.root.get_child("echo").is_some());
    }
}
