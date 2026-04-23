use crate::trie::{ArgSpec, CommandTrie, ARG_MODE_DIRS_ONLY, ARG_MODE_HOSTS, ARG_MODE_PATHS};
use serde::Deserialize;
use serde_json::Value;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

// ---------------------------------------------------------------------------
// Serde data model
// ---------------------------------------------------------------------------

#[derive(Deserialize, Default)]
struct FigSpec {
    #[serde(default)]
    name: Value,
    #[serde(default)]
    description: String,
    #[serde(default)]
    subcommands: Vec<FigSpec>,
    #[serde(default)]
    options: Vec<FigOption>,
    #[serde(default)]
    args: Value,
}

#[derive(Deserialize, Default)]
struct FigOption {
    #[serde(default)]
    name: Value,
    #[serde(default)]
    #[allow(dead_code)]
    description: String,
    #[serde(default)]
    args: Value,
}

#[derive(Deserialize, Default)]
struct FigArg {
    #[serde(default)]
    name: String,
    #[serde(default)]
    template: Value,
    #[serde(default)]
    suggestions: Vec<Value>,
    #[serde(default)]
    generators: Value,
    #[serde(default, rename = "isOptional")]
    #[allow(dead_code)]
    is_optional: bool,
    #[serde(default, rename = "isVariadic")]
    #[allow(dead_code)]
    is_variadic: bool,
}

#[derive(Deserialize, Default)]
struct FigGenerator {
    #[serde(default)]
    script: Value,
}

// ---------------------------------------------------------------------------
// Public API
// ---------------------------------------------------------------------------

/// Walk `$XDG_CACHE_HOME/zsh-ios/fig-json/*.json` and fold each spec into the
/// trie. Existing entries win (Zsh/Fish/Bash data is richer; Fig is additive).
///
/// Returns (commands_enriched, subcommands_added, flags_added).
pub fn scan_fig_completions(trie: &mut CommandTrie) -> (u32, u32, u32) {
    let json_dir = fig_json_dir();
    scan_fig_dirs(trie, &json_dir)
}

/// Clone / update the withfig/autocomplete repo, build it, and dump every
/// compiled spec to JSON under `$XDG_CACHE_HOME/zsh-ios/fig-json/`.
/// Requires Node.js and pnpm (or npm) on PATH. Run once per upstream update.
pub fn cmd_fig_fetch() {
    let cache = fig_repo_dir();
    let json_out = fig_json_dir();

    // 1. Clone or pull the upstream repo.
    if !cache.join(".git").is_dir() {
        eprintln!("Cloning withfig/autocomplete into {}", cache.display());
        if let Some(parent) = cache.parent()
            && let Err(e) = fs::create_dir_all(parent)
        {
            eprintln!("Error creating cache parent: {}", e);
            std::process::exit(1);
        }
        run(&[
            "git",
            "clone",
            "--depth=1",
            "https://github.com/withfig/autocomplete.git",
            &cache.to_string_lossy(),
        ]);
    } else {
        eprintln!("Pulling latest withfig/autocomplete");
        run_in(&cache, &["git", "pull", "--ff-only"]);
    }

    // 2. Install dependencies (pnpm preferred, npm ci fallback).
    let pm = which_package_manager();
    eprintln!("Installing dependencies with {}", pm);
    if pm == "pnpm" {
        run_in(&cache, &["pnpm", "install", "--frozen-lockfile"]);
    } else {
        run_in(&cache, &["npm", "ci"]);
    }

    // 3. Build the TypeScript sources.
    eprintln!("Building TypeScript sources");
    if pm == "pnpm" {
        run_in(&cache, &["pnpm", "run", "build"]);
    } else {
        run_in(&cache, &["npm", "run", "build"]);
    }

    // 4. Dump each spec to JSON via the bundled Node scriptlet.
    if let Err(e) = fs::create_dir_all(&json_out) {
        eprintln!("Error creating JSON output dir: {}", e);
        std::process::exit(1);
    }
    let node_script = include_str!("../data/fig_dump.js");
    let script_path = cache.join(".zsh-ios-dump.js");
    if let Err(e) = fs::write(&script_path, node_script) {
        eprintln!("Error writing dump script: {}", e);
        std::process::exit(1);
    }
    run_in(
        &cache,
        &[
            "node",
            ".zsh-ios-dump.js",
            "build",
            &json_out.to_string_lossy(),
        ],
    );

    eprintln!("Dumped fig specs to {}", json_out.display());
}

// ---------------------------------------------------------------------------
// Internal helpers
// ---------------------------------------------------------------------------

fn fig_cache_base() -> PathBuf {
    dirs::cache_dir()
        .unwrap_or_else(|| PathBuf::from(".cache"))
        .join("zsh-ios")
}

fn fig_repo_dir() -> PathBuf {
    fig_cache_base().join("fig-autocomplete")
}

fn fig_json_dir() -> PathBuf {
    fig_cache_base().join("fig-json")
}

/// Check PATH for `pnpm`; fall back to `npm`. Exits if neither is found.
fn which_package_manager() -> &'static str {
    if command_exists("pnpm") {
        return "pnpm";
    }
    if command_exists("npm") {
        return "npm";
    }
    eprintln!("Error: neither pnpm nor npm found on PATH. Install Node.js and pnpm/npm first.");
    std::process::exit(1);
}

fn command_exists(name: &str) -> bool {
    Command::new("which")
        .arg(name)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

/// Run a command, printing argv before execution. Exits if the command fails.
fn run(argv: &[&str]) {
    let (prog, args) = argv.split_first().expect("empty argv");
    let status = Command::new(prog)
        .args(args)
        .status()
        .unwrap_or_else(|e| {
            eprintln!("Error running {:?}: {}", prog, e);
            std::process::exit(1);
        });
    if !status.success() {
        eprintln!("Command {:?} failed with {:?}", argv, status.code());
        std::process::exit(1);
    }
}

/// Run a command in a specific working directory. Exits if the command fails.
fn run_in(dir: &Path, argv: &[&str]) {
    let (prog, args) = argv.split_first().expect("empty argv");
    let status = Command::new(prog)
        .args(args)
        .current_dir(dir)
        .status()
        .unwrap_or_else(|e| {
            eprintln!("Error running {:?} in {}: {}", prog, dir.display(), e);
            std::process::exit(1);
        });
    if !status.success() {
        eprintln!(
            "Command {:?} in {} failed with {:?}",
            argv,
            dir.display(),
            status.code()
        );
        std::process::exit(1);
    }
}

// ---------------------------------------------------------------------------
// Scanning
// ---------------------------------------------------------------------------

/// Walk a directory of `.json` fig spec files and fold them into the trie.
pub(crate) fn scan_fig_dirs(trie: &mut CommandTrie, json_dir: &Path) -> (u32, u32, u32) {
    let Ok(read_dir) = fs::read_dir(json_dir) else {
        return (0, 0, 0);
    };

    let mut cmds_enriched: u32 = 0;
    let mut subs_added: u32 = 0;
    let mut flags_added: u32 = 0;

    for entry in read_dir.flatten() {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("json") {
            continue;
        }
        let Ok(content) = fs::read_to_string(&path) else {
            continue;
        };
        let spec: FigSpec = match serde_json::from_str(&content) {
            Ok(s) => s,
            Err(_) => continue,
        };

        let (c, s, f) = apply_fig_spec(&spec, &[], trie);
        cmds_enriched += c;
        subs_added += s;
        flags_added += f;
    }

    (cmds_enriched, subs_added, flags_added)
}

// ---------------------------------------------------------------------------
// Spec application
// ---------------------------------------------------------------------------

/// Recursively apply a `FigSpec` at `parent_path` depth.
///
/// - At depth 0 the spec *is* the top-level command (its names become the
///   command names in the trie).
/// - At depth ≥1 the spec describes a subcommand; `parent_path` holds the
///   words above it.
///
/// Returns (commands_enriched, subcommands_added, flags_added) contributed by
/// this call and all recursive descendants.
fn apply_fig_spec(spec: &FigSpec, parent_path: &[&str], trie: &mut CommandTrie) -> (u32, u32, u32) {
    let names = fig_names(&spec.name);
    if names.is_empty() {
        return (0, 0, 0);
    }

    let mut cmds: u32 = 0;
    let mut subs: u32 = 0;
    let mut flags: u32 = 0;

    for name in &names {
        // Build the full path for this node.
        let mut full: Vec<&str> = parent_path.to_vec();
        full.push(name);

        // Insert into trie.
        if full.len() == 1 {
            // Top-level command — just ensure the node exists.
            trie.insert(&full);
        } else {
            trie.insert(&full);
            subs += 1;
        }

        // Description (richest wins).
        if !spec.description.is_empty() && full.len() >= 2 {
            let parent_key = full[..full.len() - 1].join(" ");
            let child_name = full[full.len() - 1];
            crate::trie::merge_description(
                &mut trie.descriptions,
                parent_key,
                child_name.to_string(),
                spec.description.clone(),
            );
        }

        // The trie key used for arg_specs and descriptions at this level.
        let spec_key = full.join(" ");

        // Options / flags.
        let f = apply_options(&spec.options, &spec_key, trie);
        flags += f;

        // Top-level args for this command.
        apply_args_value(&spec.args, &spec_key, trie);

        // Recurse into subcommands.
        for sub in &spec.subcommands {
            let full_refs: Vec<&str> = full.to_vec();
            let (c2, s2, f2) = apply_fig_spec(sub, &full_refs, trie);
            let _ = c2;
            subs += s2;
            flags += f2;
        }

        cmds += 1;
    }

    (cmds, subs, flags)
}

/// Apply the `options` array of a FigSpec at the given `spec_key` level.
/// Returns the number of flag entries added.
fn apply_options(options: &[FigOption], spec_key: &str, trie: &mut CommandTrie) -> u32 {
    let mut flags_added: u32 = 0;

    for opt in options {
        let flag_names = fig_names(&opt.name);
        if flag_names.is_empty() {
            continue;
        }

        // Determine what the option's argument looks like (if any).
        let arg = single_fig_arg(&opt.args);

        let mode = arg.as_ref().and_then(classify_arg);
        let call_prog = arg.as_ref().and_then(extract_generator);
        let static_list = arg.as_ref().and_then(extract_suggestions);

        let has_value = mode.is_some() || call_prog.is_some() || static_list.is_some();

        // Build alias group if there are multiple names.
        let alias_group: Vec<String> = if flag_names.len() > 1 {
            flag_names.iter().map(|s| s.to_string()).collect()
        } else {
            Vec::new()
        };

        for flag in &flag_names {
            let normalized = normalize_flag(flag);
            if normalized.is_empty() {
                continue;
            }

            if has_value {
                let entry = trie.arg_specs.entry(spec_key.to_string()).or_default();

                if let Some(m) = mode {
                    entry.flag_args.entry(normalized.clone()).or_insert(m);
                }
                if let Some((ref tag, ref argv)) = call_prog {
                    entry
                        .flag_call_programs
                        .entry(normalized.clone())
                        .or_insert_with(|| (tag.clone(), argv.clone()));
                }
                if let Some(ref list) = static_list {
                    entry
                        .flag_static_lists
                        .entry(normalized.clone())
                        .or_insert_with(|| list.clone());
                }
                flags_added += 1;
            }
        }

        // Register alias group (gap-fill only).
        if alias_group.len() > 1 {
            let entry = trie.arg_specs.entry(spec_key.to_string()).or_default();
            let already = entry.flag_aliases.iter().any(|g| {
                g.iter().any(|f| alias_group.contains(f))
            });
            if !already {
                entry.flag_aliases.push(alias_group);
            }
        }
    }

    flags_added
}

/// Apply a top-level `args` value to the rest/call_program/static_list of the
/// spec at `spec_key`. Handles both a single arg object and an array.
fn apply_args_value(args_val: &Value, spec_key: &str, trie: &mut CommandTrie) {
    let arg = single_fig_arg(args_val);
    let Some(arg) = arg else { return };

    let mode = classify_arg(&arg);
    let call_prog = extract_generator(&arg);
    let static_list = extract_suggestions(&arg);

    if mode.is_none() && call_prog.is_none() && static_list.is_none() {
        return;
    }

    let entry: &mut ArgSpec = trie.arg_specs.entry(spec_key.to_string()).or_default();

    if let Some(m) = mode
        && entry.rest.is_none()
    {
        entry.rest = Some(m);
    }
    if let Some((tag, argv)) = call_prog
        && entry.rest_call_program.is_none()
    {
        entry.rest_call_program = Some((tag, argv));
    }
    if let Some(list) = static_list
        && entry.rest_static_list.is_none()
    {
        entry.rest_static_list = Some(list);
    }
}

// ---------------------------------------------------------------------------
// Value helpers
// ---------------------------------------------------------------------------

/// Normalize a raw `name` Value (string or array) into a list of `&str`
/// references into the JSON value.
pub(crate) fn fig_names(v: &Value) -> Vec<&str> {
    match v {
        Value::String(s) => {
            let s = s.trim();
            if s.is_empty() { vec![] } else { vec![s] }
        }
        Value::Array(arr) => arr
            .iter()
            .filter_map(|item| item.as_str())
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .collect(),
        _ => vec![],
    }
}

/// Map a fig template string to an ARG_MODE constant.
/// Returns `None` for templates we don't have a resolver for.
pub(crate) fn classify_template(tmpl: &str) -> Option<u8> {
    match tmpl {
        "filepaths" => Some(ARG_MODE_PATHS),
        "folders" => Some(ARG_MODE_DIRS_ONLY),
        "hosts" => Some(ARG_MODE_HOSTS),
        // "history" and "help" have no resolver — skip.
        _ => None,
    }
}

/// Given a FigArg, return the ARG_MODE from its `template` field, if any.
fn classify_arg(arg: &FigArg) -> Option<u8> {
    match &arg.template {
        Value::String(s) => classify_template(s.trim()),
        Value::Array(arr) => {
            // First template that maps to a known mode wins.
            arr.iter()
                .filter_map(|v| v.as_str())
                .find_map(|s| classify_template(s.trim()))
        }
        _ => None,
    }
}

/// Parse a `generators` value into `(tag, argv)` for `call_program` storage.
/// Only handles generators with a `script` field; ignores function generators.
fn extract_generator(arg: &FigArg) -> Option<(String, Vec<String>)> {
    let gen_val = &arg.generators;
    let r#gen = parse_generator(gen_val)?;
    let argv = generator_script_argv(&r#gen.script)?;
    let tag = if arg.name.is_empty() {
        argv.first().cloned().unwrap_or_default()
    } else {
        arg.name.trim_matches(|c| c == '<' || c == '>').to_string()
    };
    Some((tag, argv))
}

/// Parse a generator value (object or first element of an array of objects).
fn parse_generator(val: &Value) -> Option<FigGenerator> {
    match val {
        Value::Object(_) => serde_json::from_value(val.clone()).ok(),
        Value::Array(arr) => arr.first().and_then(|v| serde_json::from_value(v.clone()).ok()),
        _ => None,
    }
}

/// Parse a generator `script` value into an argv vector.
/// `script` can be a string (a single shell command) or an array of strings.
/// Function sentinels (`"__FN__"`) are ignored.
fn generator_script_argv(script: &Value) -> Option<Vec<String>> {
    match script {
        Value::String(s) if s == "__FN__" => None,
        Value::String(s) => {
            let s = s.trim();
            if s.is_empty() {
                None
            } else {
                Some(vec![s.to_string()])
            }
        }
        Value::Array(arr) => {
            let words: Vec<String> = arr
                .iter()
                .filter_map(|v| v.as_str())
                .filter(|s| *s != "__FN__")
                .map(|s| s.to_string())
                .collect();
            if words.is_empty() { None } else { Some(words) }
        }
        _ => None,
    }
}

/// Parse static suggestions from a FigArg's `suggestions` field.
/// Each suggestion can be a string or an object with a `name` field.
fn extract_suggestions(arg: &FigArg) -> Option<Vec<String>> {
    if arg.suggestions.is_empty() {
        return None;
    }
    let mut list: Vec<String> = arg
        .suggestions
        .iter()
        .filter_map(|v| match v {
            Value::String(s) => Some(s.clone()),
            Value::Object(m) => m.get("name").and_then(|n| n.as_str()).map(|s| s.to_string()),
            _ => None,
        })
        .collect();
    if list.is_empty() {
        return None;
    }
    list.sort_unstable();
    list.dedup();
    Some(list)
}

/// Extract a single `FigArg` from an `args` Value (object or first element of
/// an array).  Returns `None` when the value is null/missing/non-object.
fn single_fig_arg(val: &Value) -> Option<FigArg> {
    match val {
        Value::Object(_) => serde_json::from_value(val.clone()).ok(),
        Value::Array(arr) => arr.first().and_then(|v| serde_json::from_value(v.clone()).ok()),
        _ => None,
    }
}

/// Normalize a flag name: add `--` prefix for long flags that have none,
/// or `-` for single-char flags.  Leaves already-prefixed flags alone.
/// Returns empty string if the name is clearly not a flag (no leading `-`
/// and more than one character — those are positional words, not flags).
fn normalize_flag(name: &str) -> String {
    let trimmed = name.trim();
    if trimmed.is_empty() {
        return String::new();
    }
    if trimmed.starts_with('-') {
        return trimmed.to_string();
    }
    // Fig sometimes stores flag names without the leading dashes.
    if trimmed.len() == 1 {
        format!("-{}", trimmed)
    } else {
        format!("--{}", trimmed)
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::trie::CommandTrie;

    // ---- fig_names ----

    #[test]
    fn fig_names_single_string() {
        let v = Value::String("foo".into());
        assert_eq!(fig_names(&v), vec!["foo"]);
    }

    #[test]
    fn fig_names_array() {
        let v = serde_json::json!(["-v", "--verbose"]);
        assert_eq!(fig_names(&v), vec!["-v", "--verbose"]);
    }

    #[test]
    fn fig_names_empty_string_is_empty() {
        let v = Value::String(String::new());
        assert!(fig_names(&v).is_empty());
    }

    #[test]
    fn fig_names_null_is_empty() {
        assert!(fig_names(&Value::Null).is_empty());
    }

    // ---- classify_template ----

    #[test]
    fn classify_template_filepaths() {
        assert_eq!(classify_template("filepaths"), Some(ARG_MODE_PATHS));
    }

    #[test]
    fn classify_template_folders() {
        assert_eq!(classify_template("folders"), Some(ARG_MODE_DIRS_ONLY));
    }

    #[test]
    fn classify_template_hosts() {
        assert_eq!(classify_template("hosts"), Some(ARG_MODE_HOSTS));
    }

    #[test]
    fn classify_template_history_is_none() {
        assert_eq!(classify_template("history"), None);
    }

    #[test]
    fn classify_template_help_is_none() {
        assert_eq!(classify_template("help"), None);
    }

    #[test]
    fn classify_template_unknown_is_none() {
        assert_eq!(classify_template("weird"), None);
    }

    // ---- apply_fig_spec minimal ----

    #[test]
    fn apply_fig_spec_minimal() {
        let json = serde_json::json!({
            "name": "foo",
            "subcommands": [
                { "name": "bar" }
            ]
        });
        let spec: FigSpec = serde_json::from_value(json).unwrap();
        let mut trie = CommandTrie::new();
        apply_fig_spec(&spec, &[], &mut trie);

        let foo = trie.root.get_child("foo").expect("foo missing");
        assert!(foo.get_child("bar").is_some(), "bar missing under foo");
    }

    #[test]
    fn apply_fig_spec_array_name() {
        let json = serde_json::json!({
            "name": ["mycmd", "mc"],
            "subcommands": [{ "name": "sub" }]
        });
        let spec: FigSpec = serde_json::from_value(json).unwrap();
        let mut trie = CommandTrie::new();
        apply_fig_spec(&spec, &[], &mut trie);

        assert!(trie.root.get_child("mycmd").is_some());
        assert!(trie.root.get_child("mc").is_some());
    }

    // ---- apply_fig_spec with template ----

    #[test]
    fn apply_fig_spec_with_template() {
        let json = serde_json::json!({
            "name": "foo",
            "args": { "name": "file", "template": "filepaths" }
        });
        let spec: FigSpec = serde_json::from_value(json).unwrap();
        let mut trie = CommandTrie::new();
        apply_fig_spec(&spec, &[], &mut trie);

        let rest = trie
            .arg_specs
            .get("foo")
            .and_then(|s| s.rest);
        assert_eq!(rest, Some(ARG_MODE_PATHS));
    }

    #[test]
    fn apply_fig_spec_folders_template() {
        let json = serde_json::json!({
            "name": "foo",
            "args": { "template": "folders" }
        });
        let spec: FigSpec = serde_json::from_value(json).unwrap();
        let mut trie = CommandTrie::new();
        apply_fig_spec(&spec, &[], &mut trie);

        assert_eq!(
            trie.arg_specs.get("foo").and_then(|s| s.rest),
            Some(ARG_MODE_DIRS_ONLY)
        );
    }

    // ---- apply_fig_spec with generator script ----

    #[test]
    fn apply_fig_spec_with_generator_script() {
        let json = serde_json::json!({
            "name": "foo",
            "args": {
                "name": "branch",
                "generators": {
                    "script": ["git", "for-each-ref", "refs/heads", "--format=%(refname:short)"]
                }
            }
        });
        let spec: FigSpec = serde_json::from_value(json).unwrap();
        let mut trie = CommandTrie::new();
        apply_fig_spec(&spec, &[], &mut trie);

        let call_prog = trie
            .arg_specs
            .get("foo")
            .and_then(|s| s.rest_call_program.as_ref());
        assert!(call_prog.is_some(), "rest_call_program should be set");
        let (tag, argv) = call_prog.unwrap();
        assert_eq!(tag, "branch");
        assert_eq!(argv[0], "git");
    }

    #[test]
    fn apply_fig_spec_generator_fn_sentinel_ignored() {
        let json = serde_json::json!({
            "name": "foo",
            "args": {
                "name": "thing",
                "generators": { "script": "__FN__", "postProcess": "__FN__" }
            }
        });
        let spec: FigSpec = serde_json::from_value(json).unwrap();
        let mut trie = CommandTrie::new();
        apply_fig_spec(&spec, &[], &mut trie);

        // "__FN__" script should not produce a call_program entry.
        let rest_cp = trie.arg_specs.get("foo").and_then(|s| s.rest_call_program.as_ref());
        assert!(rest_cp.is_none(), "FN sentinel should not create call_program");
    }

    // ---- static suggestions ----

    #[test]
    fn apply_fig_spec_suggestions() {
        let json = serde_json::json!({
            "name": "foo",
            "args": { "suggestions": ["alpha", "beta", "gamma"] }
        });
        let spec: FigSpec = serde_json::from_value(json).unwrap();
        let mut trie = CommandTrie::new();
        apply_fig_spec(&spec, &[], &mut trie);

        let list = trie
            .arg_specs
            .get("foo")
            .and_then(|s| s.rest_static_list.as_ref());
        assert!(list.is_some(), "rest_static_list should be set");
        let list = list.unwrap();
        assert!(list.contains(&"alpha".to_string()));
        assert!(list.contains(&"beta".to_string()));
        assert!(list.contains(&"gamma".to_string()));
    }

    // ---- flag options ----

    #[test]
    fn apply_fig_spec_flag_with_template() {
        let json = serde_json::json!({
            "name": "foo",
            "options": [{
                "name": ["--config", "-c"],
                "args": { "template": "filepaths" }
            }]
        });
        let spec: FigSpec = serde_json::from_value(json).unwrap();
        let mut trie = CommandTrie::new();
        apply_fig_spec(&spec, &[], &mut trie);

        let spec_entry = trie.arg_specs.get("foo").expect("foo spec missing");
        assert_eq!(spec_entry.flag_args.get("--config"), Some(&ARG_MODE_PATHS));
        assert_eq!(spec_entry.flag_args.get("-c"), Some(&ARG_MODE_PATHS));
        // Alias group recorded.
        assert!(!spec_entry.flag_aliases.is_empty());
    }

    // ---- scan_fig_dirs integration test ----

    #[test]
    fn scan_fig_dirs_integration() {
        let dir = tempfile::tempdir().unwrap();
        let spec_json = serde_json::json!({
            "name": "testcmd",
            "description": "A test command",
            "subcommands": [
                { "name": "run", "description": "Run it" },
                { "name": "stop", "description": "Stop it" }
            ],
            "options": [
                { "name": "--verbose", "description": "Verbose" },
                {
                    "name": ["--output", "-o"],
                    "args": { "template": "filepaths" }
                }
            ],
            "args": { "template": "folders" }
        });
        std::fs::write(
            dir.path().join("testcmd.json"),
            serde_json::to_string(&spec_json).unwrap(),
        )
        .unwrap();

        let mut trie = CommandTrie::new();
        let (cmds, subs, flags) = scan_fig_dirs(&mut trie, dir.path());

        assert!(cmds >= 1, "expected at least 1 command enriched");
        assert!(subs >= 2, "expected run+stop; got {}", subs);
        assert!(flags >= 1, "expected --output flag; got {}", flags);

        let testcmd = trie.root.get_child("testcmd").expect("testcmd missing");
        assert!(testcmd.get_child("run").is_some(), "run missing");
        assert!(testcmd.get_child("stop").is_some(), "stop missing");

        let spec_entry = trie.arg_specs.get("testcmd").expect("testcmd arg_spec missing");
        assert_eq!(spec_entry.rest, Some(ARG_MODE_DIRS_ONLY));
        assert_eq!(spec_entry.flag_args.get("--output"), Some(&ARG_MODE_PATHS));
    }

    #[test]
    fn scan_fig_dirs_empty_dir_returns_zero() {
        let dir = tempfile::tempdir().unwrap();
        let mut trie = CommandTrie::new();
        let (c, s, f) = scan_fig_dirs(&mut trie, dir.path());
        assert_eq!((c, s, f), (0, 0, 0));
    }

    #[test]
    fn scan_fig_dirs_nonexistent_dir_returns_zero() {
        let mut trie = CommandTrie::new();
        let (c, s, f) = scan_fig_dirs(&mut trie, Path::new("/nonexistent/path/zz"));
        assert_eq!((c, s, f), (0, 0, 0));
    }

    #[test]
    fn scan_fig_dirs_does_not_overwrite_existing_spec() {
        let dir = tempfile::tempdir().unwrap();
        let spec_json = serde_json::json!({
            "name": "git",
            "args": { "template": "folders" }
        });
        std::fs::write(
            dir.path().join("git.json"),
            serde_json::to_string(&spec_json).unwrap(),
        )
        .unwrap();

        let mut trie = CommandTrie::new();
        // Pre-seed with a "better" rest type.
        trie.arg_specs
            .entry("git".into())
            .or_default()
            .rest = Some(ARG_MODE_PATHS);

        scan_fig_dirs(&mut trie, dir.path());

        // Should still be ARG_MODE_PATHS (existing wins).
        assert_eq!(
            trie.arg_specs.get("git").and_then(|s| s.rest),
            Some(ARG_MODE_PATHS)
        );
    }
}
