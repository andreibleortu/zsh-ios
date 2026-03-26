use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashMap};
use std::fs;
use std::path::Path;

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct TrieNode {
    pub children: BTreeMap<String, TrieNode>,
    pub count: u32,
    /// Whether this node represents a real command/subcommand (not just an intermediate)
    pub is_leaf: bool,
}

impl TrieNode {
    /// Insert a command sequence (e.g., ["git", "checkout", "main"]) into the trie.
    /// Each word becomes a level in the trie.
    pub fn insert(&mut self, words: &[&str]) {
        if words.is_empty() {
            return;
        }
        let child = self.children.entry(words[0].to_string()).or_default();
        child.count += 1;
        // Only mark as leaf if this is the terminal word in the sequence.
        // Intermediate nodes get is_leaf from insert_command() or from being
        // the terminal word in a different insertion.
        if words.len() == 1 {
            child.is_leaf = true;
        }
        if words.len() > 1 {
            child.insert(&words[1..]);
        }
    }

    /// Insert a single first-level command (executable name) without incrementing count
    /// from history. Marks it as a leaf so it's discoverable.
    pub fn insert_command(&mut self, name: &str) {
        let child = self.children.entry(name.to_string()).or_default();
        child.is_leaf = true;
    }

    /// Find all children whose names start with the given prefix.
    /// Returns (full_name, &child_node) pairs.
    pub fn prefix_search(&self, prefix: &str) -> Vec<(&str, &TrieNode)> {
        if prefix.is_empty() {
            return self.children.iter().map(|(k, v)| (k.as_str(), v)).collect();
        }
        // Use BTreeMap range for O(log n + m) lookup instead of O(n) full scan
        let start = prefix.to_string();
        self.children
            .range(start..)
            .take_while(|(k, _)| k.starts_with(prefix))
            .map(|(k, v)| (k.as_str(), v))
            .collect()
    }

    /// Exact lookup for a child by name.
    pub fn get_child(&self, name: &str) -> Option<&TrieNode> {
        self.children.get(name)
    }

    /// Total number of distinct first-level entries.
    pub fn len(&self) -> usize {
        self.children.len()
    }

    #[allow(dead_code)]
    pub fn is_empty(&self) -> bool {
        self.children.is_empty()
    }

    /// Check whether `name` is a strict prefix of any existing child.
    /// Used to prevent learning abbreviated junk like "terr" when "terraform" exists.
    /// Uses BTreeMap range for O(log n) instead of O(n) full scan.
    pub fn is_prefix_of_existing(&self, name: &str) -> bool {
        // Range from `name` onward; the first entry >= name is either `name` itself
        // or something that starts with `name` (a longer command).
        self.children
            .range(name.to_string()..)
            .take_while(|(k, _)| k.starts_with(name))
            .any(|(k, _)| k.as_str() != name)
    }
}

/// Argument type constants for positions and flags.
/// Values 1-3 are the original modes; 4+ are extended types from Zsh completions.
pub const ARG_MODE_PATHS: u8 = 1;
pub const ARG_MODE_DIRS_ONLY: u8 = 2;
pub const ARG_MODE_EXECS_ONLY: u8 = 3;
pub const ARG_MODE_USERS: u8 = 4;
pub const ARG_MODE_HOSTS: u8 = 5;
pub const ARG_MODE_PIDS: u8 = 6;
pub const ARG_MODE_SIGNALS: u8 = 7;
pub const ARG_MODE_PORTS: u8 = 8;
pub const ARG_MODE_NET_IFACES: u8 = 9;
pub const ARG_MODE_GIT_BRANCHES: u8 = 10;
pub const ARG_MODE_GIT_TAGS: u8 = 11;
pub const ARG_MODE_GIT_REMOTES: u8 = 12;
pub const ARG_MODE_GIT_FILES: u8 = 13;
pub const ARG_MODE_URLS: u8 = 14;
pub const ARG_MODE_GROUPS: u8 = 15;
pub const ARG_MODE_LOCALES: u8 = 16;
/// Accepts either a user name or a group name (e.g. `chown owner:group`).
pub const ARG_MODE_USERS_GROUPS: u8 = 17;

/// A context-sensitive completion rule evaluated at query time.
///
/// When any flag in `trigger_flags` is already present on the current command
/// line, the completion type for the next positional argument is overridden
/// with `override_type` instead of the default.
///
/// Parsed from Zsh `if [[ -n ${opt_args[(I)-b|-B|...]} ]]; then ACTION`
/// patterns inside `case $state in` arm bodies.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ContextRule {
    /// Any of these flags being present in the current words triggers the rule.
    pub trigger_flags: Vec<String>,
    /// The completion type (ARG_MODE_* constant) to use when triggered.
    pub override_type: u8,
}

/// Per-command argument specification, parsed from Zsh completion files.
/// Knows what type of argument each position and flag expects.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct ArgSpec {
    /// Argument type for specific positions (1-indexed: 1 = first arg after command).
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    pub positional: HashMap<u32, u8>,
    /// Argument type for all remaining/unspecified positions (from `*:...:_files`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rest: Option<u8>,
    /// Flags that consume the next word as a typed argument.
    /// e.g., "-o" → Paths means the word after -o is a file path.
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    pub flag_args: HashMap<String, u8>,
    /// Flags whose value is produced by running an external command.
    /// From Zsh `_call_program` specs: `'-c+:cipher:_call_program ciphers ssh -Q cipher'`.
    /// Maps flag → (tag, argv).  `tag` is the human label; `argv` is run to get completions.
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    pub flag_call_programs: HashMap<String, (String, Vec<String>)>,
    /// Same as `flag_call_programs` but for rest/positional arguments.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rest_call_program: Option<(String, Vec<String>)>,
    /// Flags whose value is a static enumeration of literal strings.
    /// From `compadd - yes no`, `_values 'mode' fast slow`, etc.
    /// Maps flag → sorted deduplicated completion items.
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    pub flag_static_lists: HashMap<String, Vec<String>>,
    /// Same as `flag_static_lists` but for rest/positional arguments.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rest_static_list: Option<Vec<String>>,
    /// Context-sensitive rules: when certain flags are present in the current
    /// command line, override what we complete for the next positional argument.
    /// Evaluated at completion time by checking the typed words.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub context_rules: Vec<ContextRule>,
}

impl ArgSpec {
    /// Get the argument type for a given position (1-indexed).
    pub fn type_at(&self, position: u32) -> Option<u8> {
        self.positional.get(&position).copied().or(self.rest)
    }

    /// Get the argument type expected after a flag.
    /// Also checks the flag with trailing `=` stripped (e.g., `--output=` → `--output`).
    pub fn type_after_flag(&self, flag: &str) -> Option<u8> {
        if let Some(&t) = self.flag_args.get(flag) {
            return Some(t);
        }
        let stripped = flag.trim_end_matches('=');
        if stripped != flag {
            return self.flag_args.get(stripped).copied();
        }
        None
    }

    /// Convenience: is this spec non-empty?
    pub fn is_empty(&self) -> bool {
        self.positional.is_empty()
            && self.rest.is_none()
            && self.flag_args.is_empty()
            && self.flag_call_programs.is_empty()
            && self.rest_call_program.is_none()
            && self.flag_static_lists.is_empty()
            && self.rest_static_list.is_none()
            && self.context_rules.is_empty()
    }

    /// Whether a flag consumes the next word (either via typed arg, call_program, or static list).
    pub fn flag_takes_value(&self, flag: &str) -> bool {
        self.type_after_flag(flag).is_some()
            || self.flag_call_programs.contains_key(flag)
            || self.flag_static_lists.contains_key(flag)
    }

    /// Merge another `ArgSpec` into this one (pure gap-fill).
    /// Only slots that are completely absent in `self` are filled from `other`;
    /// any existing value — even a generic one — is preserved.  This ensures
    /// that the primary function's explicit specs always take precedence over
    /// what a helper function infers.
    pub fn merge(&mut self, other: &ArgSpec) {
        for (&pos, &arg_type) in &other.positional {
            self.positional.entry(pos).or_insert(arg_type);
        }
        if self.rest.is_none() {
            self.rest = other.rest;
        }
        for (flag, arg_type) in &other.flag_args {
            self.flag_args.entry(flag.clone()).or_insert(*arg_type);
        }
        for (flag, entry) in &other.flag_call_programs {
            self.flag_call_programs
                .entry(flag.clone())
                .or_insert_with(|| entry.clone());
        }
        if self.rest_call_program.is_none() {
            self.rest_call_program = other.rest_call_program.clone();
        }
        for (flag, list) in &other.flag_static_lists {
            self.flag_static_lists
                .entry(flag.clone())
                .or_insert_with(|| list.clone());
        }
        if self.rest_static_list.is_none() {
            self.rest_static_list = other.rest_static_list.clone();
        }
        // Gap-fill context rules: add any rules from other whose trigger_flags
        // are not already covered by an existing rule in self.
        for other_rule in &other.context_rules {
            let already_covered = self.context_rules.iter().any(|r| {
                r.trigger_flags
                    .iter()
                    .any(|f| other_rule.trigger_flags.contains(f))
            });
            if !already_covered {
                self.context_rules.push(other_rule.clone());
            }
        }
    }
}

/// Maps command path (e.g., "git add", "cp") to its argument spec.
pub type ArgSpecMap = HashMap<String, ArgSpec>;

/// Legacy flat map kept for backward compat during deserialization.
pub type ArgModeMap = HashMap<String, u8>;

/// Maps parent command (e.g., "git", "docker compose") to
/// subcommand -> description pairs for IOS-style `?` help.
pub type DescriptionMap = HashMap<String, HashMap<String, String>>;

/// The full command trie with serialization.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct CommandTrie {
    pub root: TrieNode,
    /// Per-position argument specs from Zsh completion files.
    #[serde(default)]
    pub arg_specs: ArgSpecMap,
    /// Legacy flat arg modes (kept for backward compat with old tree files).
    #[serde(default)]
    pub arg_modes: ArgModeMap,
    /// Subcommand descriptions for IOS-style `?` help.
    /// Key = parent command (e.g. "git"), value = subcommand -> description.
    #[serde(default)]
    pub descriptions: DescriptionMap,
}

impl CommandTrie {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn insert(&mut self, words: &[&str]) {
        self.root.insert(words);
    }

    pub fn insert_command(&mut self, name: &str) {
        self.root.insert_command(name);
    }

    /// Serialize to MessagePack and write to file.
    /// Uses named (map) encoding so the format survives field additions.
    pub fn save(&self, path: &Path) -> Result<(), Box<dyn std::error::Error>> {
        let data = rmp_serde::to_vec_named(self)?;
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::write(path, data)?;
        Ok(())
    }

    /// Load from MessagePack file.
    pub fn load(path: &Path) -> Result<Self, Box<dyn std::error::Error>> {
        let data = fs::read(path)?;
        let trie: Self = rmp_serde::from_slice(&data)?;
        Ok(trie)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_insert_and_search() {
        let mut trie = CommandTrie::new();
        trie.insert(&["git", "checkout", "main"]);
        trie.insert(&["git", "commit", "-m"]);
        trie.insert(&["grep", "-r", "pattern"]);

        let matches = trie.root.prefix_search("gi");
        assert_eq!(matches.len(), 1);
        assert_eq!(matches[0].0, "git");

        let matches = trie.root.prefix_search("g");
        assert_eq!(matches.len(), 2); // git, grep

        let git_node = trie.root.get_child("git").unwrap();
        let matches = git_node.prefix_search("ch");
        assert_eq!(matches.len(), 1);
        assert_eq!(matches[0].0, "checkout");

        let matches = git_node.prefix_search("c");
        assert_eq!(matches.len(), 2); // checkout, commit
    }

    #[test]
    fn test_insert_command() {
        let mut trie = CommandTrie::new();
        trie.insert_command("terraform");
        trie.insert_command("telnet");

        let matches = trie.root.prefix_search("ter");
        assert_eq!(matches.len(), 1);
        assert_eq!(matches[0].0, "terraform");

        let matches = trie.root.prefix_search("te");
        assert_eq!(matches.len(), 2);
    }

    #[test]
    fn test_serialize_roundtrip() {
        let mut trie = CommandTrie::new();
        trie.insert(&["git", "checkout"]);
        trie.insert(&["terraform", "apply"]);

        let data = rmp_serde::to_vec(&trie).unwrap();
        let loaded: CommandTrie = rmp_serde::from_slice(&data).unwrap();

        assert_eq!(loaded.root.children.len(), 2);
        assert!(loaded.root.get_child("git").is_some());
        assert!(loaded.root.get_child("terraform").is_some());
    }

    #[test]
    fn test_is_prefix_of_existing() {
        let mut trie = CommandTrie::new();
        trie.insert_command("terraform");
        trie.insert_command("telnet");

        assert!(trie.root.is_prefix_of_existing("terr")); // prefix of "terraform"
        assert!(trie.root.is_prefix_of_existing("te")); // prefix of both
        assert!(!trie.root.is_prefix_of_existing("terraform")); // exact, not strict prefix
        assert!(!trie.root.is_prefix_of_existing("xyz")); // prefix of nothing
    }
}
