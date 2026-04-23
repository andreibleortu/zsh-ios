use crate::trie::{CommandTrie, ARG_MODE_DIRS_ONLY, ARG_MODE_PATHS};
use serde::Deserialize;
use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

// ── serde model ──────────────────────────────────────────────────────────────

#[derive(Debug, Deserialize, Default)]
struct CarapaceSpec {
    #[serde(default)]
    name: String,
    #[serde(default)]
    description: String,
    #[serde(default)]
    flags: HashMap<String, String>,
    #[serde(default)]
    persistentflags: HashMap<String, String>,
    #[serde(default)]
    completion: CarapaceCompletion,
    #[serde(default)]
    commands: Vec<CarapaceSpec>,
}

#[derive(Debug, Deserialize, Default)]
struct CarapaceCompletion {
    #[serde(default)]
    positional: Vec<Vec<String>>,
    #[serde(default)]
    positionalany: Vec<String>,
    #[serde(default)]
    flag: HashMap<String, Vec<String>>,
}

// ── action classifier ────────────────────────────────────────────────────────

/// The three-way return type of `classify_action`.
///
/// At most one field is `Some`:
/// - `arg_mode`: an `ARG_MODE_*` constant for well-known macros.
/// - `call_program`: `(tag, argv)` for `$(cmd ...)` dynamic generators.
/// - `static_list`: literal completion items (tab-separated descriptions stripped).
type ClassifyResult = (Option<u8>, Option<(String, Vec<String>)>, Option<Vec<String>>);

/// Classify a carapace action list into one of three output shapes.
fn classify_action(actions: &[String]) -> ClassifyResult {
    if actions.is_empty() {
        return (None, None, None);
    }

    // Fast-path: check the first element for well-known macros.
    // Collect all elements; if all are macros of the same kind, use that kind.
    let mut mode: Option<u8> = None;
    let mut call: Option<(String, Vec<String>)> = None;
    let mut statics: Vec<String> = Vec::new();

    for action in actions {
        let s = action.trim();

        // $files or $files(pattern) → PATHS
        if s == "$files" || (s.starts_with("$files(") && s.ends_with(')')) {
            mode = Some(ARG_MODE_PATHS);
            continue;
        }

        // $directories → DIRS_ONLY
        if s == "$directories" {
            mode = Some(ARG_MODE_DIRS_ONLY);
            continue;
        }

        // $list(sep,items) — we extract the items
        if let Some(inner) = s.strip_prefix("$list(").and_then(|x| x.strip_suffix(')')) {
            // First character is the separator.
            let mut chars = inner.chars();
            let sep = chars.next().unwrap_or(',');
            let rest = chars.as_str();
            // Skip the separator char if it's immediately followed by a comma.
            let items_part = rest.strip_prefix(sep).unwrap_or(rest);
            for item in items_part.split(sep) {
                let item = strip_tab_description(item.trim());
                if !item.is_empty() {
                    statics.push(item.to_string());
                }
            }
            continue;
        }

        // $(cmd args…) → call_program
        if s.starts_with("$(") && s.ends_with(')') {
            let inner = &s[2..s.len() - 1];
            if let Some(argv) = shlex::split(inner)
                && !argv.is_empty()
            {
                let tag = argv[0].clone();
                call = Some((tag, argv));
            }
            continue;
        }

        // Anything else is a literal item (possibly "key\tdescription").
        let item = strip_tab_description(s);
        if !item.is_empty() {
            statics.push(item.to_string());
        }
    }

    // Priority: if we found a well-known mode, return that.  If we found a
    // call_program, return that.  Otherwise return static list if non-empty.
    if mode.is_some() {
        return (mode, None, None);
    }
    if let Some(cp) = call {
        return (None, Some(cp), None);
    }
    if !statics.is_empty() {
        return (None, None, Some(statics));
    }
    (None, None, None)
}

/// Strip everything from the first tab character onward (carapace uses
/// `"key\tdescription"` in static lists).
fn strip_tab_description(s: &str) -> &str {
    match s.find('\t') {
        Some(pos) => &s[..pos],
        None => s,
    }
}

// ── flag name helpers ─────────────────────────────────────────────────────────

/// Carapace flag keys can look like:
///   `-v`            single-character flag
///   `--verbose`     long flag
///   `-v, --verbose` comma-separated aliases
///   `--output=`     `=` suffix means "takes a value"
///   `--config?`     `?` suffix means "optional value" (treat as takes-value)
///   `--tags*`       `*` suffix means "repeatable"  (strip)
///
/// Returns `(names, takes_value)` where `names` are the canonical flag strings
/// (with hyphens, without suffix modifiers).
fn parse_flag_key(raw: &str) -> (Vec<String>, bool) {
    let mut takes_value = false;
    let mut names: Vec<String> = Vec::new();

    for part in raw.split(',') {
        let part = part.trim();
        // Strip suffix modifiers.
        let (part, tv) = strip_flag_suffix(part);
        if tv {
            takes_value = true;
        }
        if !part.is_empty() {
            names.push(part.to_string());
        }
    }

    (names, takes_value)
}

/// Strip the modifier suffix from a single flag name.
/// Returns `(stripped_name, takes_value)`.
fn strip_flag_suffix(flag: &str) -> (&str, bool) {
    if let Some(s) = flag.strip_suffix('=') {
        return (s, true);
    }
    if let Some(s) = flag.strip_suffix('?') {
        return (s, true);
    }
    if let Some(s) = flag.strip_suffix('*') {
        return (s, false);
    }
    (flag, false)
}

// ── recursive spec application ────────────────────────────────────────────────

/// Apply one spec (and all its sub-specs) to the trie.
///
/// `parent` is the path of ancestor command names above this spec.
/// `persistent_inherited` is the accumulated set of persistent flags from
/// ancestor specs; this spec's own `persistentflags` are merged in and passed
/// down to children.
///
/// Returns `(commands_enriched, subs_added, flags_added)`.
fn apply_spec_to_trie(
    spec: &CarapaceSpec,
    parent: &[&str],
    trie: &mut CommandTrie,
    persistent_inherited: &HashMap<String, String>,
) -> (u32, u32, u32) {
    let mut cmds_enriched: u32 = 0;
    let mut subs_added: u32 = 0;
    let mut flags_added: u32 = 0;

    // Build the full path for this node.
    let mut key_words: Vec<&str> = parent.to_vec();
    if !spec.name.is_empty() {
        key_words.push(&spec.name);
    }

    // Nothing to do if there is no name at all (malformed top-level spec with
    // an empty name and no parent — skip silently).
    if key_words.is_empty() {
        return (0, 0, 0);
    }

    let cmd_key = key_words.join(" ");

    // Insert this node into the trie so it's discoverable.
    if !spec.name.is_empty() {
        trie.insert(&key_words);
        subs_added += 1;
    }

    // Description: store under parent → child so the `?` key can surface it (richest wins).
    if !spec.description.is_empty() && key_words.len() >= 2 {
        let (child_name, rest) = key_words.split_last().unwrap();
        let parent_key = rest.join(" ");
        crate::trie::merge_description(
            &mut trie.descriptions,
            parent_key,
            (*child_name).to_string(),
            spec.description.clone(),
        );
    }

    // Merge persistent flags: inherited ∪ this spec's own.
    let mut merged_persistent = persistent_inherited.clone();
    for (k, v) in &spec.persistentflags {
        merged_persistent.insert(k.clone(), v.clone());
    }

    // Collect items that need trie.insert() calls (positional static lists that
    // become subcommands). We must not hold a borrow on trie.arg_specs while
    // calling trie.insert(), which mutates trie.root.
    let mut extra_trie_inserts: Vec<Vec<String>> = Vec::new();

    // Scope the mutable borrow of trie.arg_specs so it ends before insert calls.
    {
        let entry = trie.arg_specs.entry(cmd_key.clone()).or_default();

        // Flags: persistentflags (inherited+own) and spec.flags, cross-referenced
        // with spec.completion.flag for completion types.
        for flag_map in [&merged_persistent, &spec.flags] {
            for raw_key in flag_map.keys() {
                let (names, takes_value) = parse_flag_key(raw_key);
                if names.is_empty() {
                    continue;
                }

                // Determine completion type from completion.flag if available.
                let completion_actions: Option<&Vec<String>> = names
                    .iter()
                    .find_map(|n| spec.completion.flag.get(n.as_str()));

                if takes_value {
                    if let Some(actions) = completion_actions {
                        let (mode, call, statics) = classify_action(actions);
                        for name in &names {
                            if let Some(m) = mode {
                                entry.flag_args.entry(name.clone()).or_insert(m);
                                flags_added += 1;
                            } else if let Some(ref cp) = call {
                                entry
                                    .flag_call_programs
                                    .entry(name.clone())
                                    .or_insert_with(|| cp.clone());
                                flags_added += 1;
                            } else if let Some(ref sl) = statics {
                                entry
                                    .flag_static_lists
                                    .entry(name.clone())
                                    .or_insert_with(|| sl.clone());
                                flags_added += 1;
                            } else {
                                entry.flag_args.entry(name.clone()).or_insert(ARG_MODE_PATHS);
                                flags_added += 1;
                            }
                        }
                    } else {
                        for name in &names {
                            entry.flag_args.entry(name.clone()).or_insert(ARG_MODE_PATHS);
                            flags_added += 1;
                        }
                    }
                }

                // Register alias group when there are multiple names.
                if names.len() >= 2 {
                    let already_present = entry
                        .flag_aliases
                        .iter()
                        .any(|g| names.iter().any(|n| g.contains(n)));
                    if !already_present {
                        entry.flag_aliases.push(names.clone());
                    }
                }
            }
        }

        // Positional completions (0-indexed in carapace → 1-indexed in our ArgSpec).
        for (idx, actions) in spec.completion.positional.iter().enumerate() {
            let pos = (idx + 1) as u32;
            let (mode, call, statics) = classify_action(actions);
            if let Some(m) = mode {
                entry.positional.entry(pos).or_insert(m);
            } else if let Some(cp) = call {
                // Positional call programs: store in rest_call_program for pos 0.
                if entry.rest_call_program.is_none() && idx == 0 {
                    entry.rest_call_program = Some(cp);
                }
            } else if let Some(sl) = statics {
                if entry.rest_static_list.is_none() && idx == 0 {
                    entry.rest_static_list = Some(sl);
                } else {
                    // Subsequent static lists become trie subcommands — collect
                    // for insertion after this borrow scope ends.
                    for item in &sl {
                        let mut path: Vec<String> =
                            key_words.iter().map(|s| s.to_string()).collect();
                        path.push(item.clone());
                        extra_trie_inserts.push(path);
                    }
                }
            }
        }

        // positionalany → rest slot.
        if !spec.completion.positionalany.is_empty() {
            let (mode, call, statics) = classify_action(&spec.completion.positionalany);
            if let Some(m) = mode {
                entry.rest.get_or_insert(m);
            } else if let Some(cp) = call {
                if entry.rest_call_program.is_none() {
                    entry.rest_call_program = Some(cp);
                }
            } else if let Some(sl) = statics
                && entry.rest_static_list.is_none()
            {
                entry.rest_static_list = Some(sl);
            }
        }
    } // end borrow of trie.arg_specs

    // Insert extra subcommands (from positional static lists beyond the first).
    for path_owned in extra_trie_inserts {
        let path_refs: Vec<&str> = path_owned.iter().map(String::as_str).collect();
        trie.insert(&path_refs);
        subs_added += 1;
    }

    if !spec.description.is_empty()
        || !spec.flags.is_empty()
        || !spec.persistentflags.is_empty()
        || !spec.completion.positional.is_empty()
        || !spec.completion.positionalany.is_empty()
        || !spec.commands.is_empty()
    {
        cmds_enriched += 1;
    }

    // Recurse for subcommands, passing the merged persistent flags downward.
    for sub in &spec.commands {
        let (sc, ss, sf) = apply_spec_to_trie(sub, &key_words, trie, &merged_persistent);
        cmds_enriched += sc;
        subs_added += ss;
        flags_added += sf;
    }

    (cmds_enriched, subs_added, flags_added)
}

// ── directory discovery ───────────────────────────────────────────────────────

fn static_spec_dirs() -> Vec<PathBuf> {
    let mut out = Vec::new();

    // User specs.
    if let Some(cfg) = dirs::config_dir() {
        let p = cfg.join("carapace/specs");
        if p.is_dir() {
            out.push(p);
        }
    }

    // System specs.
    for dir in &[
        "/usr/share/carapace/specs",
        "/usr/local/share/carapace/specs",
    ] {
        let p = Path::new(dir);
        if p.is_dir() {
            out.push(p.to_path_buf());
        }
    }

    out
}

/// Locate the carapace binary, if any.
fn find_carapace_bin() -> Option<PathBuf> {
    which_carapace()
}

#[cfg(not(test))]
fn which_carapace() -> Option<PathBuf> {
    let output = Command::new("sh")
        .args(["-c", "command -v carapace 2>/dev/null"])
        .output()
        .ok()?;
    if output.status.success() {
        let path = String::from_utf8_lossy(&output.stdout).trim().to_string();
        if !path.is_empty() {
            return Some(PathBuf::from(path));
        }
    }
    None
}

#[cfg(test)]
fn which_carapace() -> Option<PathBuf> {
    // Tests must not depend on a system-installed carapace.
    None
}

/// Ask `carapace --version` and return the trimmed output string.
fn carapace_version(bin: &Path) -> Option<String> {
    let output = Command::new(bin).arg("--version").output().ok()?;
    Some(String::from_utf8_lossy(&output.stdout).trim().to_string())
}

/// Cache directory for dumped specs: `$XDG_CACHE_HOME/zsh-ios/carapace-specs/`.
fn carapace_cache_dir() -> PathBuf {
    let base = dirs::cache_dir().unwrap_or_else(|| PathBuf::from("/tmp"));
    base.join("zsh-ios/carapace-specs")
}

/// Return the list of commands exposed by `carapace _list`.
fn carapace_list(bin: &Path) -> Vec<String> {
    let output = match Command::new(bin).arg("_list").output() {
        Ok(o) => o,
        Err(_) => return Vec::new(),
    };
    if !output.status.success() {
        return Vec::new();
    }
    String::from_utf8_lossy(&output.stdout)
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .map(|l| l.to_string())
        .collect()
}

/// Dump the YAML spec for `cmd` from carapace.  Uses cache keyed by `version`.
fn carapace_spec_yaml(bin: &Path, cmd: &str, version: &str, cache_dir: &Path) -> Option<String> {
    // Sanitize cmd so it can't escape the cache path.
    if cmd.contains('/') || cmd.contains("..") {
        return None;
    }
    let cache_file = cache_dir.join(format!("{}.yaml", cmd));

    // Check cache: read the file and verify version tag in the first line.
    if let Ok(cached) = fs::read_to_string(&cache_file) {
        let first = cached.lines().next().unwrap_or("");
        let version_tag = format!("# carapace-version: {}", version);
        if first == version_tag {
            // Cache hit — return the content without the version header line.
            let body: String = cached.lines().skip(1).collect::<Vec<_>>().join("\n");
            return Some(body);
        }
    }

    // Cache miss — call `carapace <cmd> _spec`.
    let output = Command::new(bin)
        .args([cmd, "_spec"])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let yaml = String::from_utf8_lossy(&output.stdout).to_string();
    if yaml.trim().is_empty() {
        return None;
    }

    // Write to cache.
    if fs::create_dir_all(cache_dir).is_ok() {
        let version_tag = format!("# carapace-version: {}\n", version);
        let _ = fs::write(&cache_file, format!("{}{}", version_tag, yaml));
    }

    Some(yaml)
}

// ── public entry points ───────────────────────────────────────────────────────

/// Scan all carapace spec sources and merge into `trie`.
///
/// Sources (in priority order, lowest wins — Zsh data is applied last so it
/// is never overwritten here):
///   1. Static YAML files in known directories.
///   2. If `carapace` binary is available and
///      `disable_build_time_shell_exec` is false, enumerate `carapace _list`
///      and dump a spec per command, caching by carapace version.
///
/// Returns `(commands_enriched, subs_added, flags_added)`.
pub fn scan_carapace_completions(trie: &mut CommandTrie) -> (u32, u32, u32) {
    let dirs = static_spec_dirs();
    let (mut cmds, mut subs, mut flags) =
        scan_carapace_dirs(trie, &dirs.iter().map(PathBuf::as_path).collect::<Vec<_>>());

    let rcfg = crate::runtime_config::get();
    if !rcfg.disable_build_time_shell_exec
        && let Some(bin) = find_carapace_bin()
    {
        let (bc, bs, bf) = scan_carapace_binary(trie, &bin);
        cmds += bc;
        subs += bs;
        flags += bf;
    }

    (cmds, subs, flags)
}

/// Scan a list of directories for `*.yaml` carapace spec files.
/// Exposed separately so tests can point at a tempdir.
pub fn scan_carapace_dirs(trie: &mut CommandTrie, dirs: &[&Path]) -> (u32, u32, u32) {
    let mut cmds: u32 = 0;
    let mut subs: u32 = 0;
    let mut flags: u32 = 0;

    for dir in dirs {
        let Ok(entries) = fs::read_dir(dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) != Some("yaml") {
                continue;
            }
            let Ok(content) = fs::read_to_string(&path) else {
                continue;
            };
            let (c, s, f) = apply_yaml_to_trie(&content, trie);
            cmds += c;
            subs += s;
            flags += f;
        }
    }

    (cmds, subs, flags)
}

/// Enumerate `carapace _list`, dump a YAML spec per command, and merge.
fn scan_carapace_binary(trie: &mut CommandTrie, bin: &Path) -> (u32, u32, u32) {
    let version = match carapace_version(bin) {
        Some(v) => v,
        None => return (0, 0, 0),
    };
    let cache_dir = carapace_cache_dir();
    let commands = carapace_list(bin);

    let mut cmds: u32 = 0;
    let mut subs: u32 = 0;
    let mut flags: u32 = 0;

    for cmd in &commands {
        if let Some(yaml) = carapace_spec_yaml(bin, cmd, &version, &cache_dir) {
            let (c, s, f) = apply_yaml_to_trie(&yaml, trie);
            cmds += c;
            subs += s;
            flags += f;
        }
    }

    (cmds, subs, flags)
}

/// Deserialize a YAML string as a `CarapaceSpec` and apply it to the trie.
fn apply_yaml_to_trie(yaml: &str, trie: &mut CommandTrie) -> (u32, u32, u32) {
    if yaml.trim().is_empty() {
        return (0, 0, 0);
    }
    let spec: CarapaceSpec = match serde_yaml_ng::from_str(yaml) {
        Ok(s) => s,
        Err(_) => return (0, 0, 0),
    };
    apply_spec_to_trie(&spec, &[], trie, &HashMap::new())
}

// ── tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::trie::CommandTrie;

    // --- classify_action unit tests ---

    #[test]
    fn classify_files_macro() {
        let (mode, call, statics) = classify_action(&["$files".to_string()]);
        assert_eq!(mode, Some(ARG_MODE_PATHS));
        assert!(call.is_none());
        assert!(statics.is_none());
    }

    #[test]
    fn classify_files_with_pattern() {
        let (mode, call, statics) = classify_action(&["$files(*.go)".to_string()]);
        assert_eq!(mode, Some(ARG_MODE_PATHS));
        assert!(call.is_none());
        assert!(statics.is_none());
    }

    #[test]
    fn classify_directories_macro() {
        let (mode, call, statics) = classify_action(&["$directories".to_string()]);
        assert_eq!(mode, Some(ARG_MODE_DIRS_ONLY));
        assert!(call.is_none());
        assert!(statics.is_none());
    }

    #[test]
    fn classify_call_program() {
        let action = "$(docker ps --format {{.Names}})".to_string();
        let (mode, call, statics) = classify_action(&[action]);
        assert!(mode.is_none());
        assert!(statics.is_none());
        let (tag, argv) = call.expect("expected call_program");
        assert_eq!(tag, "docker");
        assert_eq!(argv, vec!["docker", "ps", "--format", "{{.Names}}"]);
    }

    #[test]
    fn classify_static_list() {
        let actions: Vec<String> = vec!["start".into(), "stop".into()];
        let (mode, call, statics) = classify_action(&actions);
        assert!(mode.is_none());
        assert!(call.is_none());
        let list = statics.expect("expected static list");
        assert_eq!(list, vec!["start", "stop"]);
    }

    #[test]
    fn classify_static_list_with_tab_descriptions() {
        let actions: Vec<String> = vec![
            "foo\tdescription of foo".into(),
            "bar".into(),
        ];
        let (mode, call, statics) = classify_action(&actions);
        assert!(mode.is_none());
        assert!(call.is_none());
        let list = statics.expect("expected static list");
        assert_eq!(list, vec!["foo", "bar"]);
    }

    // --- parse_flag_key unit tests ---

    #[test]
    fn flag_key_single_no_value() {
        let (names, tv) = parse_flag_key("--verbose");
        assert_eq!(names, vec!["--verbose"]);
        assert!(!tv);
    }

    #[test]
    fn flag_key_equals_suffix_takes_value() {
        let (names, tv) = parse_flag_key("--output=");
        assert_eq!(names, vec!["--output"]);
        assert!(tv);
    }

    #[test]
    fn flag_key_question_suffix_takes_value() {
        let (names, tv) = parse_flag_key("--config?");
        assert_eq!(names, vec!["--config"]);
        assert!(tv);
    }

    #[test]
    fn flag_key_star_suffix_no_value() {
        let (names, tv) = parse_flag_key("--tags*");
        assert_eq!(names, vec!["--tags"]);
        assert!(!tv);
    }

    #[test]
    fn flag_key_aliases() {
        let (names, tv) = parse_flag_key("-v, --verbose");
        assert_eq!(names, vec!["-v", "--verbose"]);
        assert!(!tv);
    }

    // --- integration test: YAML → trie ---

    const SAMPLE_YAML: &str = r#"
name: mycmd
description: My test command
flags:
  -v, --verbose: Enable verbose output
  --output=: Output file
completion:
  flag:
    --output: ["$files"]
  positional:
    - ["start", "stop", "restart"]
commands:
  - name: start
    description: Start the service
    completion:
      positional:
        - ["$directories"]
"#;

    #[test]
    fn scan_yaml_file_merges_into_trie() {
        let dir = tempfile::tempdir().unwrap();
        let yaml_path = dir.path().join("mycmd.yaml");
        std::fs::write(&yaml_path, SAMPLE_YAML).unwrap();

        let mut trie = CommandTrie::new();
        let (cmds, subs, flags) =
            scan_carapace_dirs(&mut trie, &[dir.path()]);

        assert!(cmds >= 1, "expected at least 1 command enriched; got {}", cmds);
        assert!(subs >= 1, "expected at least 1 sub inserted; got {}", subs);
        assert!(flags >= 1, "expected at least 1 flag registered; got {}", flags);

        // The top-level command should have a `start` child.
        let mycmd_node = trie.root.get_child("mycmd").expect("mycmd not in trie");
        assert!(mycmd_node.get_child("start").is_some(), "start missing");
    }

    #[test]
    fn start_subcommand_has_dirs_only_positional() {
        let dir = tempfile::tempdir().unwrap();
        let yaml_path = dir.path().join("mycmd.yaml");
        std::fs::write(&yaml_path, SAMPLE_YAML).unwrap();

        let mut trie = CommandTrie::new();
        scan_carapace_dirs(&mut trie, &[dir.path()]);

        let spec = trie.arg_specs.get("mycmd start").expect("arg_spec for mycmd start missing");
        // positional[1] should be DIRS_ONLY (from "$directories").
        let pos1 = spec.positional.get(&1).copied();
        // rest may also hold it depending on path taken.
        let rest = spec.rest;
        let has_dirs = pos1 == Some(ARG_MODE_DIRS_ONLY) || rest == Some(ARG_MODE_DIRS_ONLY)
            || spec.rest_call_program.is_none();
        // Weak assertion: we at least stored something meaningful for start.
        assert!(
            pos1 == Some(ARG_MODE_DIRS_ONLY) || rest == Some(ARG_MODE_DIRS_ONLY),
            "expected DIRS_ONLY for mycmd start positional; pos1={:?} rest={:?} has_dirs={}",
            pos1, rest, has_dirs
        );
    }

    #[test]
    fn output_flag_is_registered_with_paths_type() {
        let dir = tempfile::tempdir().unwrap();
        let yaml_path = dir.path().join("mycmd.yaml");
        std::fs::write(&yaml_path, SAMPLE_YAML).unwrap();

        let mut trie = CommandTrie::new();
        scan_carapace_dirs(&mut trie, &[dir.path()]);

        let spec = trie.arg_specs.get("mycmd").expect("arg_spec for mycmd missing");
        let output_type = spec.flag_args.get("--output").copied();
        assert_eq!(output_type, Some(ARG_MODE_PATHS), "expected --output to have PATHS type");
    }

    #[test]
    fn description_merge_keeps_longer() {
        let dir = tempfile::tempdir().unwrap();
        let yaml_path = dir.path().join("mycmd.yaml");
        std::fs::write(&yaml_path, SAMPLE_YAML).unwrap();

        let mut trie = CommandTrie::new();
        // "Start the service" (carapace) is longer than "short" (zsh) → carapace wins.
        trie.descriptions
            .entry("mycmd".into())
            .or_default()
            .insert("start".into(), "short".into());

        scan_carapace_dirs(&mut trie, &[dir.path()]);

        let desc = trie
            .descriptions
            .get("mycmd")
            .and_then(|m| m.get("start"))
            .map(String::as_str);
        assert_eq!(desc, Some("Start the service"), "longer carapace description should win");

        // Also verify: if zsh had a longer description it should be preserved.
        let dir2 = tempfile::tempdir().unwrap();
        let yaml_path2 = dir2.path().join("mycmd.yaml");
        std::fs::write(&yaml_path2, SAMPLE_YAML).unwrap();

        let mut trie2 = CommandTrie::new();
        trie2.descriptions
            .entry("mycmd".into())
            .or_default()
            .insert("start".into(), "A very detailed description from zsh that is longer".into());

        scan_carapace_dirs(&mut trie2, &[dir2.path()]);

        let desc2 = trie2
            .descriptions
            .get("mycmd")
            .and_then(|m| m.get("start"))
            .map(String::as_str);
        assert_eq!(desc2, Some("A very detailed description from zsh that is longer"),
            "longer zsh description should be preserved over shorter carapace one");
    }

    #[test]
    fn persistent_flags_inherited_by_subcommand() {
        let yaml = r#"
name: tool
persistentflags:
  --config=: Config file
completion:
  flag:
    --config: ["$files"]
commands:
  - name: run
    description: Run something
"#;
        let dir = tempfile::tempdir().unwrap();
        let yaml_path = dir.path().join("tool.yaml");
        std::fs::write(&yaml_path, yaml).unwrap();

        let mut trie = CommandTrie::new();
        scan_carapace_dirs(&mut trie, &[dir.path()]);

        // The subcommand `tool run` should have --config in its flag_args.
        let spec = trie.arg_specs.get("tool run").expect("arg_spec for tool run missing");
        assert!(
            spec.flag_args.contains_key("--config")
                || spec.flag_call_programs.contains_key("--config")
                || spec.flag_static_lists.contains_key("--config"),
            "expected --config to be inherited by tool run"
        );
    }

    #[test]
    fn static_list_at_positional_zero_becomes_rest_static_list() {
        let yaml = r#"
name: svc
completion:
  positional:
    - ["start", "stop"]
"#;
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("svc.yaml"), yaml).unwrap();

        let mut trie = CommandTrie::new();
        scan_carapace_dirs(&mut trie, &[dir.path()]);

        let spec = trie.arg_specs.get("svc").expect("arg_spec for svc missing");
        // The two-item static list at position 0 should end up as rest_static_list.
        let has_list = spec.rest_static_list.is_some();
        assert!(has_list, "expected rest_static_list for svc; spec={:?}", spec);
        let list = spec.rest_static_list.as_ref().unwrap();
        assert!(list.contains(&"start".to_string()), "start missing from rest_static_list");
        assert!(list.contains(&"stop".to_string()), "stop missing from rest_static_list");
    }

    #[test]
    fn call_program_action_stored_correctly() {
        let yaml = r#"
name: myapp
completion:
  positional:
    - ["$(myapp list)"]
"#;
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("myapp.yaml"), yaml).unwrap();

        let mut trie = CommandTrie::new();
        scan_carapace_dirs(&mut trie, &[dir.path()]);

        let spec = trie.arg_specs.get("myapp").expect("arg_spec for myapp missing");
        let cp = spec.rest_call_program.as_ref().expect("expected rest_call_program");
        assert_eq!(cp.0, "myapp", "tag should be 'myapp'");
        assert_eq!(cp.1, vec!["myapp", "list"]);
    }

    #[test]
    fn invalid_yaml_is_skipped_silently() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("bad.yaml"), "{ this is: [not valid yaml").unwrap();
        let mut trie = CommandTrie::new();
        // Should not panic.
        let (c, s, f) = scan_carapace_dirs(&mut trie, &[dir.path()]);
        assert_eq!((c, s, f), (0, 0, 0));
    }

    #[test]
    fn empty_spec_name_top_level_is_skipped() {
        // A spec with no name and no parent path produces nothing.
        let yaml = r#"
description: Nameless spec
flags:
  --foo: some flag
"#;
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("noname.yaml"), yaml).unwrap();
        let mut trie = CommandTrie::new();
        let (c, s, _f) = scan_carapace_dirs(&mut trie, &[dir.path()]);
        // We inserted 0 nodes with an empty-name spec at root level.
        assert_eq!(s, 0, "expected 0 subs from empty-name spec; got {}", s);
        let _ = c;
    }
}
