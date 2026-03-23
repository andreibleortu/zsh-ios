mod completions;
mod config;
mod history;
mod path_resolve;
mod pins;
mod resolve;
mod scanner;
mod trie;

use clap::{Parser, Subcommand};
use std::process;

#[derive(Parser)]
#[command(name = "zsh-ios", about = "Cisco IOS-style command abbreviation for Zsh")]
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
        /// The abbreviated command line to resolve
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        line: Vec<String>,
    },
    /// Show completions for a prefix (used by ? key)
    Complete {
        /// The prefix to complete
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        line: Vec<String>,
    },
    /// Learn a single command (add to trie incrementally)
    Learn {
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
}

fn main() {
    let cli = Cli::parse();

    match cli.command {
        Commands::Build { aliases_stdin } => cmd_build(aliases_stdin),
        Commands::Resolve { line } => cmd_resolve(&line.join(" ")),
        Commands::Complete { line } => cmd_complete(&line.join(" ")),
        Commands::Learn { command } => cmd_learn(&command.join(" ")),
        Commands::Pin { abbrev, expanded } => cmd_pin(&abbrev, &expanded),
        Commands::Unpin { abbrev } => cmd_unpin(&abbrev),
        Commands::Pins => cmd_list_pins(),
        Commands::Toggle => cmd_toggle(),
        Commands::Rebuild => cmd_rebuild(),
        Commands::Status => cmd_status(),
    }
}

fn cmd_build(aliases_stdin: bool) {
    config::ensure_config_dir().unwrap_or_else(|e| {
        eprintln!("Error creating config dir: {}", e);
        process::exit(1);
    });

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
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|_| {
            dirs::home_dir()
                .unwrap_or_default()
                .join(".zsh_history")
        });

    match history::parse_history(&hist_path, &mut ct) {
        Ok(count) => eprintln!("Parsed {} commands from history", count),
        Err(e) => eprintln!("Warning: could not parse history: {}", e),
    }

    // 5. Scan Zsh completion files for subcommand definitions
    let comp_count = completions::scan_completions(&mut ct);
    eprintln!("Learned {} subcommands from Zsh completions", comp_count);
    eprintln!("Detected arg modes for {} commands", ct.arg_modes.len());

    // 6. Register our own subcommands so `zsh-ios reb` -> `zsh-ios rebuild` works
    for sub in &[
        "build", "resolve", "complete", "learn", "pin", "unpin",
        "pins", "toggle", "rebuild", "status",
    ] {
        ct.insert(&["zsh-ios", sub]);
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

fn cmd_resolve(line: &str) {
    let trie = load_trie();
    let pin_store = pins::Pins::load(&config::pins_path());

    match resolve::resolve_line(line, &trie, &pin_store) {
        resolve::ResolveResult::Resolved(expanded) => {
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
    println!(
        "_zio_remaining={}",
        shell_quote(&info.remaining.join(" "))
    );

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

fn cmd_complete(line: &str) {
    let trie = load_trie();
    let pin_store = pins::Pins::load(&config::pins_path());
    let output = resolve::complete(line, &trie, &pin_store);
    print!("{}", output);
}

fn cmd_learn(command: &str) {
    if command.trim().is_empty() {
        return;
    }

    let tree_path = config::tree_path();
    let mut trie = match trie::CommandTrie::load(&tree_path) {
        Ok(t) => t,
        Err(_) => return,
    };

    // Resolve the command first so we learn the expanded form,
    // not abbreviated junk (e.g., learn "git checkout" not "gi ch").
    // Only learn when resolution fully succeeds -- ambiguous or passthrough
    // input has nothing valid to teach the trie.
    let pin_store = pins::Pins::load(&config::pins_path());
    let to_learn = match resolve::resolve_line(command, &trie, &pin_store) {
        resolve::ResolveResult::Resolved(r) => r,
        _ => return,
    };

    for segment in history::split_command_segments(&to_learn) {
        let words: Vec<&str> = segment.split_whitespace().collect();
        if !words.is_empty() && !trie.root.is_prefix_of_existing(words[0]) {
            trie.insert(&words);
        }
    }

    let _ = trie.save(&tree_path);
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
    let state_path = config::config_dir().join("disabled");
    if state_path.exists() {
        let _ = std::fs::remove_file(&state_path);
        println!("zsh-ios: enabled");
    } else {
        config::ensure_config_dir().unwrap_or_else(|e| {
            eprintln!("Error: {}", e);
            process::exit(1);
        });
        let _ = std::fs::write(&state_path, "");
        println!("zsh-ios: disabled");
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
    let disabled = config_dir.join("disabled").exists();

    println!("zsh-ios status");
    println!("  Enabled:     {}", if disabled { "no" } else { "yes" });
    println!("  Config dir:  {}", config_dir.display());
    println!("  Tree file:   {}", tree_path.display());
    println!("  Pins file:   {}", pins_path.display());

    if tree_path.exists() {
        if let Ok(meta) = std::fs::metadata(&tree_path) {
            println!("  Tree size:   {} bytes", meta.len());
        }
        if let Ok(trie) = trie::CommandTrie::load(&tree_path) {
            println!("  Commands:    {} top-level", trie.root.len());
            if !trie.arg_modes.is_empty() {
                println!("  Arg modes:   {} commands auto-detected", trie.arg_modes.len());
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
}

fn load_trie() -> trie::CommandTrie {
    let tree_path = config::tree_path();
    trie::CommandTrie::load(&tree_path).unwrap_or_else(|_| {
        eprintln!("Warning: No command tree found. Run `zsh-ios build` first.");
        eprintln!("Tip: source the zsh-ios plugin or run: alias | zsh-ios build --aliases-stdin");
        trie::CommandTrie::new()
    })
}
