use zsh_ios::*;

use clap::{Parser, Subcommand};
use std::fs::OpenOptions;
use std::process;

#[derive(Parser)]
#[command(
    name = "zsh-ios",
    about = "Cisco IOS-style command abbreviation for Zsh"
)]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    /// Build the command trie from PATH, history, and aliases
    Build {
        /// Read aliases from stdin (pipe `alias` output)
        #[arg(long)]
        aliases_stdin: bool,
    },
    /// Resolve an abbreviated command line
    Resolve {
        /// Shell context hint inferred from the buffer (redirection, math, condition, …)
        #[arg(long = "context")]
        context: Option<String>,
        /// Quote state: none / single / double / backtick / dollar
        /// (inferred from the buffer by the plugin).
        #[arg(long = "quote")]
        quote: Option<String>,
        /// When present, the cursor is inside a ${PARAM} expansion and
        /// completion should suggest parameter names.
        #[arg(long = "param-context")]
        param_context: bool,
        /// The abbreviated command line to resolve
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        line: Vec<String>,
    },
    /// Show completions for a prefix (used by ? key)
    Complete {
        /// Shell context hint inferred from the buffer (redirection, math, condition, …)
        #[arg(long = "context")]
        context: Option<String>,
        /// Quote state: none / single / double / backtick / dollar
        /// (inferred from the buffer by the plugin).
        #[arg(long = "quote")]
        quote: Option<String>,
        /// When present, the cursor is inside a ${PARAM} expansion and
        /// completion should suggest parameter names.
        #[arg(long = "param-context")]
        param_context: bool,
        /// The prefix to complete
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        line: Vec<String>,
    },
    /// Learn a single command (add to trie incrementally)
    Learn {
        /// Exit code of the command (0 = success, non-zero = failure)
        #[arg(long = "exit-code", default_value_t = 0)]
        exit_code: i32,
        /// Working directory where the command ran
        #[arg(long = "cwd")]
        cwd: Option<String>,
        /// The full command that was executed
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        command: Vec<String>,
    },
    /// Pin an abbreviation sequence to an expansion
    Pin {
        /// The abbreviated sequence (e.g., "g ch")
        abbrev: String,
        #[arg(long = "to")]
        /// The expanded sequence (e.g., "git checkout")
        expanded: String,
    },
    /// Remove a pin
    Unpin {
        /// The abbreviated sequence to unpin
        abbrev: String,
    },
    /// List all current pins
    Pins,
    /// Enable or disable zsh-ios (toggles state file)
    Toggle,
    /// Rebuild the command tree (run from shell so aliases are captured)
    Rebuild,
    /// Show status: enabled/disabled, tree stats, config paths
    Status,
    /// Explain step-by-step how an input would resolve (for debugging)
    Explain {
        /// The abbreviated command line to trace
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        line: Vec<String>,
    },
    /// Ingest structured shell state from stdin (aliases, functions, named dirs)
    Ingest,
    /// Read a harvest capture from stdin and fold _regex_arguments specs into the trie
    #[command(name = "regex-args-ingest")]
    RegexArgsIngest,
    /// Show or apply a config preset to $XDG_CONFIG_HOME/zsh-ios/config.yaml.
    /// With no name: list available presets.
    /// With a name: apply (backs up existing config.yaml first).
    Preset {
        /// Preset name: deterministic | privacy | power
        name: Option<String>,
        /// Print the preset YAML to stdout without writing to config.yaml.
        #[arg(long)]
        show: bool,
        /// Skip backup + existing-file prompt. Still writes to the real path.
        #[arg(long)]
        force: bool,
    },
    /// Clone / update the withfig/autocomplete repo, build it, and dump every
    /// compiled spec to JSON under $XDG_CACHE_HOME/zsh-ios/fig-json/.
    /// Requires Node.js and pnpm (or npm) on PATH. Run once per upstream update.
    #[command(name = "fig-fetch")]
    FigFetch,
    /// Download the latest carapace-bin release into the cache dir and dump
    /// every builtin completer's YAML spec there. Subsequent `zsh-ios rebuild`
    /// reads those YAMLs and folds them into the trie.
    #[command(name = "carapace-fetch")]
    CarapaceFetch,
}

fn main() {
    let cli = Cli::parse();

    match cli.command {
        Commands::Build { aliases_stdin } => cmd_build(aliases_stdin),
        Commands::Resolve { context, quote, param_context, line } => {
            cmd_resolve(&line.join(" "), context.as_deref(), quote.as_deref(), param_context)
        }
        Commands::Complete { context, quote, param_context, line } => {
            cmd_complete(&line.join(" "), context.as_deref(), quote.as_deref(), param_context)
        }
        Commands::Learn { exit_code, cwd, command } => cmd_learn(&command.join(" "), exit_code, cwd.as_deref()),
        Commands::Pin { abbrev, expanded } => cmd_pin(&abbrev, &expanded),
        Commands::Unpin { abbrev } => cmd_unpin(&abbrev),
        Commands::Pins => cmd_list_pins(),
        Commands::Toggle => cmd_toggle(),
        Commands::Rebuild => cmd_rebuild(),
        Commands::Status => cmd_status(),
        Commands::Explain { line } => cmd_explain(&line.join(" ")),
        Commands::Ingest => ingest::cmd_ingest(),
        Commands::RegexArgsIngest => cmd_regex_args_ingest(),
        Commands::Preset { name, show, force } => presets::cmd_preset(name.as_deref(), show, force),
        Commands::FigFetch => fig_completions::cmd_fig_fetch(),
        Commands::CarapaceFetch => {
            if let Err(e) = carapace_completions::cmd_carapace_fetch() {
                eprintln!("carapace-fetch: {}", e);
                process::exit(1);
            }
        }
    }
}

fn cmd_build(aliases_stdin: bool) {
    config::ensure_config_dir().unwrap_or_else(|e| {
        eprintln!("Error creating config dir: {}", e);
        process::exit(1);
    });

    let user_cfg = user_config::UserConfig::load(&config::user_config_path());
    runtime_config::set(user_cfg.to_runtime_config());

    // Serialize against concurrent `learn` writers.
    let _lock = locks::lock_for(&config::tree_path());

    let mut ct = trie::CommandTrie::new();

    // 1. Scan PATH for executables
    let path_count = scanner::scan_path(&mut ct);
    eprintln!("Scanned {} executables from PATH", path_count);

    // 2. Add builtins
    let builtin_count = scanner::add_builtins(&mut ct);
    eprintln!("Added {} Zsh builtins", builtin_count);

    // 3. Parse aliases from stdin if requested
    if aliases_stdin {
        let alias_count = scanner::parse_aliases_from_stdin(&mut ct);
        eprintln!("Parsed {} aliases from stdin", alias_count);
    }

    // 4. Parse history
    let hist_path = std::env::var("HISTFILE")
        .ok()
        .map(std::path::PathBuf::from)
        .filter(|p| p.exists())
        .unwrap_or_else(|| {
            let home = dirs::home_dir().unwrap_or_default();
            // Try common history file locations
            let candidates = [".zsh_history", ".histfile"];
            candidates
                .iter()
                .map(|name| home.join(name))
                .find(|p| p.exists())
                .unwrap_or_else(|| home.join(".zsh_history"))
        });

    match history::parse_history(&hist_path, &mut ct) {
        Ok(count) => eprintln!("Parsed {} commands from history", count),
        Err(e) => eprintln!("Warning: could not parse history: {}", e),
    }

    // 5. Scan Zsh completion files for subcommand definitions
    let comp_count = completions::scan_completions(&mut ct);
    eprintln!("Learned {} subcommands from Zsh completions", comp_count);
    eprintln!(
        "Detected arg specs for {} commands ({} with flag-level detail)",
        ct.arg_specs.len(),
        ct.arg_specs
            .values()
            .filter(|s| !s.flag_args.is_empty() || !s.positional.is_empty())
            .count()
    );

    // 5b. Supplement with Fish completion data (additive — Zsh wins on conflicts)
    let (fish_cmds, fish_subs, fish_flags) =
        fish_completions::scan_fish_completions(&mut ct);
    if fish_cmds > 0 {
        eprintln!(
            "Enriched {} commands with Fish completion data ({} subs, {} flags)",
            fish_cmds, fish_subs, fish_flags,
        );
    }

    // 5d. Supplement with Bash completion data (additive — Zsh and Fish win on conflicts)
    let (bash_cmds, bash_subs, bash_flags) =
        bash_completions::scan_bash_completions(&mut ct);
    if bash_cmds > 0 {
        eprintln!(
            "Enriched {} commands with Bash completion data ({} subs, {} flags)",
            bash_cmds, bash_subs, bash_flags,
        );
    }

    // 5e. Supplement with carapace spec data (static YAMLs + optional binary dump).
    let (cara_cmds, cara_subs, cara_flags) =
        carapace_completions::scan_carapace_completions(&mut ct);
    if cara_cmds > 0 {
        eprintln!(
            "Enriched {} commands with carapace spec data ({} subs, {} flags)",
            cara_cmds, cara_subs, cara_flags,
        );
    }

    // 5f. Supplement with Fig spec data from the cached JSON dump (additive).
    // If the user has never run `zsh-ios fig-fetch` the cache dir is absent
    // and scan_fig_completions returns (0, 0, 0) silently.
    let (fig_cmds, fig_subs, fig_flags) =
        fig_completions::scan_fig_completions(&mut ct);
    if fig_cmds > 0 {
        eprintln!(
            "Enriched {} commands with Fig spec data ({} subs, {} flags)",
            fig_cmds, fig_subs, fig_flags,
        );
    }

    // 5c. Import user-defined shell functions so they're resolvable as commands.
    // We run `zsh -ic` (interactive) so .zshrc runs and user's functions are
    // visible. Cheap because the result only needs to be fetched at build time.
    let fn_count = import_shell_functions(&mut ct);
    if fn_count > 0 {
        eprintln!("Imported {} shell functions", fn_count);
    }

    // 6. Register our own subcommands so `zsh-ios reb` -> `zsh-ios rebuild` works
    for sub in &[
        "build", "resolve", "complete", "learn", "pin", "unpin", "pins", "toggle", "rebuild",
        "status", "explain", "ingest", "regex-args-ingest", "preset", "fig-fetch",
        "carapace-fetch",
    ] {
        ct.insert(&["zsh-ios", sub]);
    }

    // 7a. Retention: prune stale nodes (forget_unused_after_days).
    let rcfg = runtime_config::get();
    if rcfg.forget_unused_after_days > 0 {
        let now = current_unix_ts();
        let cutoff_secs = rcfg.forget_unused_after_days as u64 * 86400;
        let pruned = prune_stale_nodes(&mut ct.root, now, cutoff_secs);
        if pruned > 0 {
            eprintln!("Pruned {} stale trie nodes (unused >{} days, count<3)", pruned, rcfg.forget_unused_after_days);
        }
    }

    // 7b. Retention: cap trie size (max_trie_size).
    if rcfg.max_trie_size > 0 {
        let total = count_all_nodes(&ct.root);
        if total > rcfg.max_trie_size as usize {
            let drop_count = total - rcfg.max_trie_size as usize;
            let dropped = drop_lowest_nodes(&mut ct.root, drop_count);
            eprintln!("Capped trie: dropped {} nodes to stay within max_trie_size={}", dropped, rcfg.max_trie_size);
        }
    }

    // 7. Save trie
    let tree_path = config::tree_path();
    ct.save(&tree_path).unwrap_or_else(|e| {
        eprintln!("Error saving trie: {}", e);
        process::exit(1);
    });

    eprintln!(
        "Tree saved to {} ({} top-level commands)",
        tree_path.display(),
        ct.root.len()
    );
}

/// Recursively prune nodes that haven't been used in `cutoff_secs` and have
/// a use count < 3. Returns the number of nodes removed.
fn prune_stale_nodes(node: &mut trie::TrieNode, now: u64, cutoff_secs: u64) -> usize {
    let mut removed = 0usize;
    node.children.retain(|_, child| {
        let age = if child.last_used == 0 {
            u64::MAX // never used → always old
        } else {
            now.saturating_sub(child.last_used)
        };
        let stale = age > cutoff_secs && child.count < 3;
        if stale {
            removed += 1 + count_all_nodes(child);
            false
        } else {
            removed += prune_stale_nodes(child, now, cutoff_secs);
            true
        }
    });
    removed
}

/// Count total nodes (including `node` itself).
fn count_all_nodes(node: &trie::TrieNode) -> usize {
    1 + node.children.values().map(count_all_nodes).sum::<usize>()
}

/// Drop `target` nodes from the trie by removing the least-used / oldest
/// leaf-level subtrees. Returns the actual number removed.
fn drop_lowest_nodes(root: &mut trie::TrieNode, target: usize) -> usize {
    // Collect all (count, last_used, path) for leaves and score them.
    // We drop the subtree at the lowest-scoring top-level child first.
    // Simple strategy: sort top-level children by (count asc, last_used asc)
    // and drop them until we've freed enough nodes.
    let mut dropped = 0usize;
    loop {
        if dropped >= target {
            break;
        }
        // Find the child with the lowest (count, last_used).
        let worst_key: Option<String> = root
            .children
            .iter()
            .min_by_key(|(_, n)| (n.count, n.last_used))
            .map(|(k, _)| k.clone());
        match worst_key {
            None => break,
            Some(k) => {
                let subtree_size = count_all_nodes(root.children.get(&k).unwrap());
                root.children.remove(&k);
                dropped += subtree_size;
            }
        }
    }
    dropped
}

/// Ask an interactive Zsh to print its function names, one per line, and
/// insert non-underscore-prefixed ones into the trie as leaf commands.
///
/// Returns the number of functions actually inserted.  Missing zsh,
/// .zshrc errors, or an empty result all quietly yield 0.
fn import_shell_functions(trie: &mut trie::CommandTrie) -> u32 {
    if runtime_config::get().disable_build_time_shell_exec {
        return 0;
    }
    let mut cmd = std::process::Command::new("zsh");
    cmd.args(["-ic", "print -l ${(k)functions}"]);
    cmd.stderr(std::process::Stdio::null());
    let output = match cmd.output() {
        Ok(o) if o.status.success() => o,
        _ => return 0,
    };
    let mut n = 0u32;
    for line in String::from_utf8_lossy(&output.stdout).lines() {
        let name = line.trim();
        if name.is_empty() || name.starts_with('_') || name.contains(char::is_whitespace) {
            continue;
        }
        // Skip anything the trie already knows — we don't want to displace
        // a real executable with a same-named function entry (both work;
        // first one wins for descriptions).
        if trie.root.get_child(name).is_some() {
            continue;
        }
        trie.insert_command(name);
        n += 1;
    }
    n
}

fn cmd_resolve(line: &str, context: Option<&str>, quote: Option<&str>, param_context: bool) {
    let mut trie = load_trie();
    let pin_store = pins::Pins::load(&config::pins_path());
    let user_cfg = user_config::UserConfig::load(&config::user_config_path());
    runtime_config::set(user_cfg.to_runtime_config());
    resolve::set_statistics_disabled(user_cfg.disable_statistics);
    runtime_complete::reset_runtime_call_counter();
    if user_cfg.disable_galiases {
        trie.galiases.clear();
    }
    hydrate_live_state(&trie);

    // Blocklist pre-check: if the user typed the blocklisted name literally,
    // passthrough immediately so the engine does zero work.
    let typed_first = first_word(line);
    if user_cfg.is_blocklisted(typed_first) {
        println!("{}", line);
        process::exit(2);
    }

    let cwd = std::env::current_dir()
        .ok()
        .and_then(|p| p.into_os_string().into_string().ok());
    let context_hint = resolve::ContextHint::from_parts(context, quote, param_context);
    match resolve::resolve_line(line, &trie, &pin_store, cwd.as_deref(), context_hint) {
        resolve::ResolveResult::Resolved(expanded) => {
            // Blocklist post-check: if the resolved command is blocklisted,
            // print the ORIGINAL input (not the expansion) and passthrough.
            // This catches abbreviations — `kub ...` that resolves to
            // `kubectl ...` is blocked by `command_blocklist: [kubectl]`.
            if user_cfg.is_blocklisted(first_word(&expanded)) {
                println!("{}", line);
                process::exit(2);
            }
            println!("{}", expanded);
            process::exit(0);
        }
        resolve::ResolveResult::Ambiguous(info) => {
            print_ambiguity_shell(&info);
            process::exit(1);
        }
        resolve::ResolveResult::PathAmbiguous(candidates) => {
            print_path_ambiguity_shell(&candidates);
            process::exit(3);
        }
        resolve::ResolveResult::Passthrough(original) => {
            println!("{}", original);
            process::exit(2);
        }
    }
}

fn first_word(s: &str) -> &str {
    s.split_whitespace().next().unwrap_or("")
}

fn print_ambiguity_shell(info: &resolve::AmbiguityInfo) {
    fn shell_quote(s: &str) -> String {
        format!("'{}'", s.replace('\'', "'\\''"))
    }

    println!("_zio_word={}", shell_quote(&info.word));
    println!("_zio_lcp={}", shell_quote(&info.lcp));
    println!("_zio_position={}", info.position);
    println!(
        "_zio_resolved_prefix={}",
        shell_quote(&info.resolved_prefix.join(" "))
    );
    println!("_zio_remaining={}", shell_quote(&info.remaining.join(" ")));

    // Candidates as a shell array
    let cands: Vec<String> = info.candidates.iter().map(|c| shell_quote(c)).collect();
    println!("_zio_candidates=({})", cands.join(" "));

    // Deep candidates: display lines and selectable items
    let mut deep_display: Vec<String> = Vec::new();
    let mut deep_items: Vec<String> = Vec::new();
    for dc in &info.deep_candidates {
        let subs = dc.subcommand_matches.join(", ");
        deep_display.push(format!("{} ({})", dc.command, subs));
        for sub in &dc.subcommand_matches {
            deep_items.push(format!("{} {}", dc.command, sub));
        }
    }
    let dd: Vec<String> = deep_display.iter().map(|s| shell_quote(s)).collect();
    println!("_zio_deep_display=({})", dd.join(" "));
    let di: Vec<String> = deep_items.iter().map(|s| shell_quote(s)).collect();
    println!("_zio_deep_items=({})", di.join(" "));

    println!(
        "_zio_pins_path={}",
        shell_quote(&config::pins_path().to_string_lossy())
    );
}

fn print_path_ambiguity_shell(candidates: &[String]) {
    fn shell_quote(s: &str) -> String {
        format!("'{}'", s.replace('\'', "'\\''"))
    }
    let items: Vec<String> = candidates.iter().map(|c| shell_quote(c)).collect();
    println!("_zio_path_candidates=({})", items.join(" "));
}

fn cmd_complete(line: &str, context: Option<&str>, quote: Option<&str>, param_context: bool) {
    let mut trie = load_trie();
    let pin_store = pins::Pins::load(&config::pins_path());
    let user_cfg = user_config::UserConfig::load(&config::user_config_path());
    runtime_config::set(user_cfg.to_runtime_config());
    resolve::set_statistics_disabled(user_cfg.disable_statistics);
    runtime_complete::reset_runtime_call_counter();
    if user_cfg.disable_galiases {
        trie.galiases.clear();
    }
    hydrate_live_state(&trie);
    let context_hint = resolve::ContextHint::from_parts(context, quote, param_context);
    let output = resolve::complete(line, &trie, &pin_store, context_hint);
    print!("{}", output);
    // Exit code 4 signals "generic output" — the plugin uses this to trigger
    // the ZLE worker fallback chain instead of grepping stdout for a marker
    // string. Keep the classifier and the constant next to `complete()`.
    if resolve::is_generic_output(&output) {
        std::process::exit(4);
    }
}

/// Populate the runtime_complete live-state cache from the trie's stored dumps.
/// Called at CLI entry so the JobSpec / ShellParameter / Widget / Module /
/// HashedCommand resolvers see the shell's current state.
fn hydrate_live_state(trie: &trie::CommandTrie) {
    use std::collections::HashMap;
    let mut parsed: HashMap<String, Vec<String>> = HashMap::new();
    if let Some(s) = trie.live_state.get("jobs") {
        parsed.insert("jobs".into(), runtime_complete::parse_jobs_output(s));
    }
    if let Some(s) = trie.live_state.get("parameters") {
        parsed.insert("parameters".into(), runtime_complete::parse_parameters_output(s));
    }
    if let Some(s) = trie.live_state.get("widgets") {
        parsed.insert("widgets".into(), runtime_complete::parse_widgets_output(s));
    }
    if let Some(s) = trie.live_state.get("modules") {
        parsed.insert("modules".into(), runtime_complete::parse_modules_output(s));
    }
    if let Some(s) = trie.live_state.get("commands") {
        parsed.insert("hashed".into(), runtime_complete::parse_hashed_commands_output(s));
    }
    runtime_complete::set_live_state(parsed);
}

fn cmd_learn(command: &str, exit_code: i32, cwd: Option<&str>) {
    if command.trim().is_empty() {
        return;
    }

    let user_cfg = user_config::UserConfig::load(&config::user_config_path());
    if user_cfg.disable_learning {
        return;
    }
    runtime_config::set(user_cfg.to_runtime_config());
    resolve::set_statistics_disabled(user_cfg.disable_statistics);

    if config::ensure_config_dir().is_err() {
        return;
    }

    let tree_path = config::tree_path();
    // Hold a lock across load-mutate-save so background `learn` processes
    // spawned in rapid succession don't race and truncate the trie file.
    let _lock = locks::lock_for(&tree_path);
    let mut trie = match trie::CommandTrie::load(&tree_path) {
        Ok(t) => t,
        Err(_) => return,
    };

    // Resolve the command first so we learn the expanded form,
    // not abbreviated junk (e.g., learn "git checkout" not "gi ch").
    // Only learn when resolution fully succeeds -- ambiguous or passthrough
    // input has nothing valid to teach the trie.
    let pin_store = pins::Pins::load(&config::pins_path());
    let to_learn = match resolve::resolve_line(command, &trie, &pin_store, None, resolve::ContextHint::Unknown) {
        resolve::ResolveResult::Resolved(r) => r,
        _ => return,
    };

    let now = current_unix_ts();
    let mut dirty = false;
    for segment in history::split_command_segments(&to_learn) {
        let words: Vec<&str> = segment.split_whitespace().collect();
        if words.is_empty() {
            continue;
        }
        if exit_code == 0 {
            if !trie.root.is_prefix_of_existing(words[0]) {
                trie.root.insert_with_time(&words, now);
                // Record cwd for each node along the inserted path when cwd is Some.
                if let Some(cwd_str) = cwd {
                    let mut node = &mut trie.root;
                    for word in &words {
                        if let Some(child) = node.children.get_mut(*word) {
                            child.record_cwd(cwd_str);
                            node = child;
                        } else {
                            break;
                        }
                    }
                }
                dirty = true;
            }
        } else {
            if trie.root.record_failure(&words, now) {
                dirty = true;
            }
        }
    }

    if dirty {
        let _ = trie.save(&tree_path);
    }
}

fn current_unix_ts() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

fn cmd_pin(abbrev: &str, expanded: &str) {
    let abbrev_words: Vec<&str> = abbrev.split_whitespace().collect();
    let expanded_words: Vec<&str> = expanded.split_whitespace().collect();

    if abbrev_words.is_empty() || expanded_words.is_empty() {
        eprintln!("Usage: zsh-ios pin \"g ch\" --to \"git checkout\"");
        process::exit(1);
    }

    let pins_path = config::pins_path();
    config::ensure_config_dir().unwrap_or_else(|e| {
        eprintln!("Error: {}", e);
        process::exit(1);
    });

    let _lock = locks::lock_for(&pins_path);

    // Remove existing pin for this abbreviation first
    let _ = pins::Pins::remove(&pins_path, &abbrev_words);

    pins::Pins::append(&pins_path, &abbrev_words, &expanded_words).unwrap_or_else(|e| {
        eprintln!("Error writing pin: {}", e);
        process::exit(1);
    });

    eprintln!("{} -> {}", abbrev, expanded);
}

fn cmd_unpin(abbrev: &str) {
    let abbrev_words: Vec<&str> = abbrev.split_whitespace().collect();
    let pins_path = config::pins_path();
    let _lock = locks::lock_for(&pins_path);

    match pins::Pins::remove(&pins_path, &abbrev_words) {
        Ok(true) => eprintln!("Removed pin: {}", abbrev),
        Ok(false) => {
            eprintln!("No pin found for: {}", abbrev);
            process::exit(1);
        }
        Err(e) => {
            eprintln!("Error: {}", e);
            process::exit(1);
        }
    }
}

fn cmd_list_pins() {
    let pins_path = config::pins_path();
    match std::fs::read_to_string(&pins_path) {
        Ok(content) => {
            if content.trim().is_empty() {
                eprintln!("No pins configured.");
            } else {
                print!("{}", content);
            }
        }
        Err(_) => {
            eprintln!("No pins file found. Use `zsh-ios pin` to create one.");
        }
    }
}

fn cmd_toggle() {
    config::ensure_config_dir().unwrap_or_else(|e| {
        eprintln!("Error: {}", e);
        process::exit(1);
    });
    let state_path = config::config_dir().join("disabled");
    // Race-free toggle: try exclusive-create; if it already exists, remove it.
    match OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&state_path)
    {
        Ok(_) => println!("zsh-ios: disabled"),
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
            match std::fs::remove_file(&state_path) {
                Ok(_) => println!("zsh-ios: enabled"),
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                    // Lost the race to another toggle; treat as enabled.
                    println!("zsh-ios: enabled");
                }
                Err(e) => {
                    eprintln!("Error: {}", e);
                    process::exit(1);
                }
            }
        }
        Err(e) => {
            eprintln!("Error: {}", e);
            process::exit(1);
        }
    }
}

fn cmd_rebuild() {
    // Invoke ourselves with build, capturing aliases from the current shell
    let exe = std::env::current_exe().unwrap_or_else(|_| "zsh-ios".into());
    let status = std::process::Command::new("zsh")
        .arg("-c")
        .arg(format!(
            "alias | \"{}\" build --aliases-stdin",
            exe.display()
        ))
        .status();

    match status {
        Ok(s) if s.success() => {}
        Ok(s) => process::exit(s.code().unwrap_or(1)),
        Err(e) => {
            eprintln!("Error running rebuild: {}", e);
            process::exit(1);
        }
    }
}

fn cmd_status() {
    let config_dir = config::config_dir();
    let tree_path = config::tree_path();
    let pins_path = config::pins_path();
    let user_config_path = config::user_config_path();
    let user_cfg = user_config::UserConfig::load(&user_config_path);
    let disabled = config_dir.join("disabled").exists();

    println!("zsh-ios status");
    println!("  Enabled:     {}", if disabled { "no" } else { "yes" });
    println!("  Config dir:  {}", config_dir.display());
    println!("  Tree file:   {}", tree_path.display());
    println!("  Pins file:   {}", pins_path.display());
    // Stable key-value lines; the Zsh plugin parses "Stale threshold:" to
    // know how long tree.msgpack may be before it auto-rebuilds.
    println!(
        "  Config file: {} ({})",
        user_config_path.display(),
        if user_config_path.exists() {
            "loaded"
        } else {
            "absent"
        }
    );
    println!("  Stale threshold: {}s", user_cfg.stale_threshold());
    println!(
        "  Learning:    {}",
        if user_cfg.disable_learning {
            "disabled (config)"
        } else {
            "enabled"
        }
    );
    println!(
        "  Statistics:  {}",
        if user_cfg.disable_statistics {
            "disabled (deterministic)"
        } else {
            "enabled"
        }
    );
    println!("  Blocklist:   {}", user_cfg.command_blocklist.len());
    println!(
        "  Dyn harvest: {}",
        if user_cfg.disable_dynamic_harvest {
            "disabled (config)"
        } else {
            "enabled"
        }
    );
    // Plugin-parseable lines: prefix must be stable so grep in the plugin works.
    println!(
        "  Worker:      {}",
        if user_cfg.disable_worker {
            "disabled (config)"
        } else {
            "enabled"
        }
    );
    println!("  Worker timeout: {}ms", user_cfg.worker_timeout_ms);
    println!("  Picker prefix: {}", user_cfg.picker_header_prefix);
    println!(
        "  Ghost preview: {}",
        if user_cfg.disable_ghost_preview {
            "disabled (config)"
        } else {
            "enabled"
        }
    );
    let ghost_style = user_cfg
        .ghost_preview_style
        .as_deref()
        .unwrap_or("fg=240");
    println!("  Ghost style: {}", ghost_style);
    let ghost_prefix = user_cfg
        .ghost_preview_prefix
        .as_deref()
        .unwrap_or("  ");
    println!("  Ghost prefix: \"{}\"", ghost_prefix);

    if tree_path.exists() {
        if let Ok(meta) = std::fs::metadata(&tree_path) {
            println!("  Tree size:   {} bytes", meta.len());
        }
        if let Ok(trie) = trie::CommandTrie::load(&tree_path) {
            println!("  Commands:    {} top-level", trie.root.len());
            if !trie.arg_specs.is_empty() {
                let detailed = trie
                    .arg_specs
                    .values()
                    .filter(|s| !s.flag_args.is_empty() || !s.positional.is_empty())
                    .count();
                println!(
                    "  Arg specs:   {} commands ({} with per-position/flag detail)",
                    trie.arg_specs.len(),
                    detailed
                );
            }
            if !trie.dir_stack.is_empty() {
                println!("  Dir stack:   {} entries", trie.dir_stack.len());
            }
            if !trie.live_state.is_empty() {
                let mut keys: Vec<&str> = trie.live_state.keys().map(String::as_str).collect();
                keys.sort_unstable();
                println!("  Live state:  {} ({})", keys.len(), keys.join(", "));
            }
            if !trie.matcher_rules.is_empty() {
                let supported = trie
                    .matcher_rules
                    .iter()
                    .filter(|r| !matches!(r, trie::MatcherRule::Unknown(_)))
                    .count();
                println!(
                    "  Matcher rules: {} ({} honored)",
                    trie.matcher_rules.len(),
                    supported
                );
            }
            println!(
                "  Global aliases: {} ({})",
                trie.galiases.len(),
                if user_cfg.disable_galiases { "disabled (config)" } else { "expanded" }
            );
            if !trie.tag_groups.is_empty() {
                let total_groups: usize = trie.tag_groups.values().map(Vec::len).sum();
                println!(
                    "  Tag groups:  {} commands, {} total groups",
                    trie.tag_groups.len(),
                    total_groups
                );
            }
            if !trie.completion_styles.is_empty() {
                let s = &trie.completion_styles;
                println!(
                    "  Styles:      formats={} group-names={} list-colors={} menu={} completer-chain={}",
                    s.formats.len(),
                    s.group_names.len(),
                    s.list_colors.len(),
                    s.menu_threshold.map(|n| format!("select={n}")).unwrap_or_else(|| "off".to_string()),
                    s.completer_chain.len(),
                );
            }
        }
    } else {
        println!("  Tree:        not built yet (run `zsh-ios rebuild`)");
    }

    if pins_path.exists() {
        let pins = pins::Pins::load(&pins_path);
        println!("  Pins:        {}", pins.entries.len());
    } else {
        println!("  Pins:        none");
    }

    // Runtime cache stats — one line with entry count and total bytes.
    if let Some(cache) = runtime_cache::RuntimeCache::default_location() {
        let (n, bytes) = cache.stats();
        println!("  Cache:       {} entries, {} bytes", n, bytes);
    }

    // Registered resolvers by count per ARG_MODE — gives a pulse on which
    // data sources the build has hooked up.
    let registered: Vec<u8> = (1u8..=72u8)
        .filter(|m| type_resolver::REGISTRY.contains(*m))
        .collect();
    println!(
        "  Resolvers:   {} registered",
        registered.len(),
    );
}

fn cmd_explain(line: &str) {
    if line.trim().is_empty() {
        eprintln!("Usage: zsh-ios explain <command line>");
        process::exit(1);
    }
    let mut trie = load_trie();
    let pin_store = pins::Pins::load(&config::pins_path());
    let user_cfg = user_config::UserConfig::load(&config::user_config_path());
    runtime_config::set(user_cfg.to_runtime_config());
    resolve::set_statistics_disabled(user_cfg.disable_statistics);
    if user_cfg.disable_galiases {
        trie.galiases.clear();
    }
    hydrate_live_state(&trie);
    let cwd = std::env::current_dir()
        .ok()
        .and_then(|p| p.into_os_string().into_string().ok());
    println!("{}", resolve::explain(line, &trie, &pin_store, cwd.as_deref()));
}

fn cmd_regex_args_ingest() {
    use std::io::Read;

    let user_cfg = user_config::UserConfig::load(&config::user_config_path());
    if user_cfg.disable_dynamic_harvest {
        return;
    }

    let mut input = String::new();
    if std::io::stdin().read_to_string(&mut input).is_err() {
        return;
    }
    if input.trim().is_empty() {
        return;
    }
    if config::ensure_config_dir().is_err() {
        return;
    }

    let tree_path = config::tree_path();
    let _lock = locks::lock_for(&tree_path);
    let mut trie = match trie::CommandTrie::load(&tree_path) {
        Ok(t) => t,
        Err(_) => return,
    };

    // The input may contain multiple back-to-back captures, one per function.
    // Each capture starts with __ZIO_REGEX_WORDS__ / __ZIO_REGEX_ARGS__ lines.
    // We split them by __ZIO_REGEX_ARGS__ boundaries so each function is parsed
    // independently, then fold all results into the trie.
    let mut ingested = 0usize;
    for capture_block in split_harvest_blocks(&input) {
        if let Some((cmd_name, parsed)) = regex_args::parse_harvest_capture(capture_block) {
            if cmd_name.is_empty() {
                continue;
            }
            let spec = trie.arg_specs.entry(cmd_name).or_default();
            // Fold positionals (don't overwrite existing entries from the static parser).
            for (pos, arg_type) in parsed.positional {
                spec.positional.entry(pos).or_insert(arg_type);
            }
            if spec.rest.is_none() {
                spec.rest = parsed.rest;
            }
            for (pos, list) in parsed.static_lists {
                spec.flag_static_lists.entry(format!("__pos_{}", pos)).or_insert(list);
            }
            if spec.rest_static_list.is_none() {
                spec.rest_static_list = parsed.rest_static;
            }
            ingested += 1;
        }
    }

    if ingested > 0 {
        let _ = trie.save(&tree_path);
    }
}

/// Split a raw harvest stream into per-function blocks.
///
/// Each block starts with one or more `__ZIO_REGEX_WORDS__` lines followed by
/// exactly one `__ZIO_REGEX_ARGS__` line.  We accumulate lines until we see a
/// `__ZIO_REGEX_ARGS__` line, then yield the accumulated block (including that
/// terminating line).  Leftover lines after the last `__ZIO_REGEX_ARGS__` are
/// discarded (they're malformed input).
fn split_harvest_blocks(input: &str) -> Vec<&str> {
    let mut blocks = Vec::new();
    let mut block_start = 0usize;
    let mut pos = 0usize;

    for line in input.split_inclusive('\n') {
        let line_start = pos;
        pos += line.len();
        if line.trim_start().starts_with("__ZIO_REGEX_ARGS__") {
            // This line is the last line of the current block.
            blocks.push(&input[block_start..pos]);
            block_start = pos;
        }
        let _ = line_start;
    }

    blocks
}

fn load_trie() -> trie::CommandTrie {
    let tree_path = config::tree_path();
    match trie::CommandTrie::load(&tree_path) {
        Ok(t) => t,
        Err(e) => {
            if tree_path.exists() {
                // File is present but won't decode: surface the actual cause
                // (likely corruption from a pre-atomic-save crash, or a stale
                // format). Silently falling back to an empty trie hid this.
                eprintln!(
                    "zsh-ios: failed to load command tree at {}: {}",
                    tree_path.display(),
                    e
                );
                eprintln!("zsh-ios: run `zsh-ios rebuild` to regenerate it.");
            } else {
                eprintln!("Warning: No command tree found. Run `zsh-ios build` first.");
                eprintln!(
                    "Tip: source the zsh-ios plugin or run: alias | zsh-ios build --aliases-stdin"
                );
            }
            trie::CommandTrie::new()
        }
    }
}
