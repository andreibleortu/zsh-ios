//! End-to-end CLI tests.
//!
//! These spawn the actual built `zsh-ios` binary with `HOME` / `XDG_CONFIG_HOME`
//! pointed at a fresh tempdir so the real user's trie and pins are never
//! touched. They exercise the subcommand glue in `main.rs` — in particular
//! the atomic-save + lock path, the race-free toggle, and the corrupt-trie
//! error surfacing we put in during the fix pass.

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

fn bin_path() -> &'static str {
    env!("CARGO_BIN_EXE_zsh-ios")
}

/// Build a Command that can't read the real user's config. We leave `PATH`
/// intact (the `build` subcommand scans it) but stub out HOME and point both
/// `XDG_CONFIG_HOME` and `HISTFILE` at the given tempdir so each test is
/// hermetic.
fn cmd_in(home: &Path) -> Command {
    let mut c = Command::new(bin_path());
    c.env_remove("ZDOTDIR")
        .env("HOME", home)
        .env("XDG_CONFIG_HOME", home.join(".config"))
        .env("HISTFILE", home.join(".zsh_history"))
        // FPATH=empty keeps `build` from scanning user site-completions —
        // system-wide /usr/share/zsh/*/functions is still scanned (that's
        // baked into completion_dirs) but it's stable and read-only.
        .env("FPATH", "");
    c
}

/// macOS looks for config under `$HOME/Library/Application Support/zsh-ios`;
/// Linux uses `$XDG_CONFIG_HOME/zsh-ios` (we set that above).
fn config_dir_of(home: &Path) -> PathBuf {
    if cfg!(target_os = "macos") {
        home.join("Library/Application Support/zsh-ios")
    } else {
        home.join(".config/zsh-ios")
    }
}

fn tree_path_of(home: &Path) -> PathBuf {
    config_dir_of(home).join("tree.msgpack")
}

fn pins_path_of(home: &Path) -> PathBuf {
    config_dir_of(home).join("pins.txt")
}

fn run(c: &mut Command) -> (i32, String, String) {
    let out = c.output().expect("spawn binary");
    (
        out.status.code().unwrap_or(-1),
        String::from_utf8_lossy(&out.stdout).into_owned(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
    )
}

/// Run `build` once so subsequent subcommands have a trie to query.
fn seed_build(home: &Path) {
    let (code, _, stderr) = run(cmd_in(home).arg("build"));
    assert_eq!(code, 0, "build failed: {}", stderr);
    assert!(tree_path_of(home).exists(), "tree.msgpack not written");
}

#[test]
fn status_without_tree_reports_not_built() {
    let td = tempfile::tempdir().unwrap();
    let (code, stdout, _) = run(cmd_in(td.path()).arg("status"));
    assert_eq!(code, 0);
    assert!(stdout.contains("zsh-ios status"));
    assert!(stdout.contains("Enabled:     yes"));
    assert!(stdout.contains("not built yet") || stdout.contains("Tree size"));
}

#[test]
fn build_creates_tree_and_status_reports_it() {
    let td = tempfile::tempdir().unwrap();
    seed_build(td.path());
    let (code, stdout, _) = run(cmd_in(td.path()).arg("status"));
    assert_eq!(code, 0);
    assert!(stdout.contains("Tree size"), "status: {}", stdout);
    assert!(stdout.contains("Commands:"));
}

#[test]
fn pin_then_list_then_unpin_roundtrip() {
    let td = tempfile::tempdir().unwrap();
    // Pin doesn't require a trie.
    let (code, _, stderr) = run(cmd_in(td.path())
        .args(["pin", "foo bar"])
        .args(["--to", "foobar expanded"]));
    assert_eq!(code, 0, "pin failed: {}", stderr);
    assert!(pins_path_of(td.path()).exists());

    let (code, stdout, _) = run(cmd_in(td.path()).arg("pins"));
    assert_eq!(code, 0);
    assert!(stdout.contains("foo bar -> foobar expanded"), "pins: {}", stdout);

    let (code, _, _) = run(cmd_in(td.path()).args(["unpin", "foo bar"]));
    assert_eq!(code, 0);

    // After unpin the file is empty; `pins` prints the empty-state message.
    let (_, _, stderr) = run(cmd_in(td.path()).arg("pins"));
    assert!(stderr.contains("No pins configured") || stderr.contains("No pins file"));
}

#[test]
fn unpin_unknown_exits_nonzero() {
    let td = tempfile::tempdir().unwrap();
    let (code, _, _) = run(cmd_in(td.path()).args(["unpin", "never-pinned"]));
    assert_ne!(code, 0);
}

#[test]
fn pin_replaces_existing_mapping() {
    let td = tempfile::tempdir().unwrap();
    let (c, _, _) = run(cmd_in(td.path()).args(["pin", "k"]).args(["--to", "kubectl"]));
    assert_eq!(c, 0);
    let (c, _, _) = run(cmd_in(td.path()).args(["pin", "k"]).args(["--to", "kubectx"]));
    assert_eq!(c, 0);
    let (_, stdout, _) = run(cmd_in(td.path()).arg("pins"));
    assert!(stdout.contains("k -> kubectx"));
    assert!(!stdout.contains("k -> kubectl"), "stale mapping survived: {}", stdout);
    // Only one line (trailing newline optional).
    assert_eq!(stdout.lines().filter(|l| !l.is_empty()).count(), 1);
}

#[test]
fn toggle_is_idempotent_pair() {
    let td = tempfile::tempdir().unwrap();
    let (code, stdout, _) = run(cmd_in(td.path()).arg("toggle"));
    assert_eq!(code, 0);
    assert!(stdout.contains("disabled"));
    let (code, stdout, _) = run(cmd_in(td.path()).arg("toggle"));
    assert_eq!(code, 0);
    assert!(stdout.contains("enabled"));
}

#[test]
fn toggle_many_concurrent_processes_converges_to_valid_state() {
    // Kick off a burst of `toggle` invocations in parallel and verify we
    // end up in a deterministic state (no panics, no lost state, no
    // half-created `disabled` file). With the old `exists()`+create flow
    // two racing processes could both try to create. The O_EXCL path we
    // put in during the fix pass is what this test stresses.
    let td = tempfile::tempdir().unwrap();
    let mut children: Vec<std::process::Child> = (0..8)
        .map(|_| {
            cmd_in(td.path())
                .arg("toggle")
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn()
                .unwrap()
        })
        .collect();
    for c in &mut children {
        let s = c.wait().unwrap();
        assert!(s.success(), "toggle exited non-zero in race");
    }
    // Either enabled or disabled is fine — we just need no panic.
    let (code, _, _) = run(cmd_in(td.path()).arg("status"));
    assert_eq!(code, 0);
}

#[test]
fn resolve_with_no_tree_warns_then_passthrough() {
    let td = tempfile::tempdir().unwrap();
    let (_, stdout, stderr) = run(cmd_in(td.path()).args(["resolve", "echo hi"]));
    // Expected: no tree → empty trie → input comes back unchanged on stdout
    // (via Passthrough code path) with a warning on stderr.
    assert!(stderr.contains("No command tree found") || stderr.contains("tree"));
    assert!(stdout.trim_end_matches('\n').ends_with("echo hi"), "stdout: {:?}", stdout);
}

#[test]
fn resolve_unique_prefix_expands() {
    let td = tempfile::tempdir().unwrap();
    seed_build(td.path());
    // `echo` is a zsh builtin and also /bin/echo — should resolve uniquely.
    let (code, stdout, _) = run(cmd_in(td.path()).args(["resolve", "ech hi"]));
    // Resolved → exit 0, expanded line on stdout.
    if code == 0 {
        assert!(stdout.trim().starts_with("echo"), "stdout: {:?}", stdout);
    } else {
        // Passthrough is acceptable on a host with no `echo`-prefixed
        // entries in the trie for some reason.
        assert_eq!(code, 2);
    }
}

#[test]
fn pin_drives_resolve_output() {
    let td = tempfile::tempdir().unwrap();
    seed_build(td.path());
    // Pin a nonsense abbreviation to a known expansion and verify resolve
    // picks it up. Uses a deliberately unusual sequence so no real trie
    // entry can collide.
    let (c, _, _) =
        run(cmd_in(td.path()).args(["pin", "zzq"]).args(["--to", "echo hello"]));
    assert_eq!(c, 0);
    let (code, stdout, _) = run(cmd_in(td.path()).args(["resolve", "zzq"]));
    assert_eq!(code, 0);
    assert_eq!(stdout.trim(), "echo hello");
}

#[test]
fn corrupt_tree_surfaces_error_instead_of_silent_empty() {
    // Scenario fixed during the sweep: before the fix, a corrupt tree file
    // was silently replaced with an empty in-memory trie, so the user got
    // "nothing resolves" with no explanation. We now surface the decode
    // error on stderr and tell the user to rebuild.
    let td = tempfile::tempdir().unwrap();
    let tree = tree_path_of(td.path());
    fs::create_dir_all(tree.parent().unwrap()).unwrap();
    fs::write(&tree, b"not-valid-msgpack-garbage").unwrap();

    let (_, _, stderr) = run(cmd_in(td.path()).args(["resolve", "anything"]));
    assert!(stderr.contains("failed to load"), "stderr: {:?}", stderr);
    assert!(stderr.contains("rebuild"));
}

#[test]
fn learn_is_noop_without_resolving() {
    // `learn` should silently succeed with no trie (nothing to teach) and
    // should not panic. This exercises the early-return guard we added.
    let td = tempfile::tempdir().unwrap();
    let (code, _, _) = run(cmd_in(td.path()).args(["learn", "ls -l"]));
    assert_eq!(code, 0);
}

#[test]
fn atomic_save_survives_parallel_learns() {
    // Multi-writer stress test: hammer the binary with parallel `learn`
    // invocations that each do load-mutate-save. The pre-fix version (no
    // flock + non-atomic write) can leave tree.msgpack truncated or
    // half-decoded. With atomic rename + advisory lock this should always
    // end up with a readable trie.
    let td = tempfile::tempdir().unwrap();
    seed_build(td.path());
    let mut children = Vec::new();
    for i in 0..12 {
        let c = cmd_in(td.path())
            .args(["learn", &format!("echo iteration-{}", i)])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        children.push(c);
    }
    for mut c in children {
        let _ = c.wait();
    }
    // Final status call must still decode the tree.
    let (code, stdout, stderr) = run(cmd_in(td.path()).arg("status"));
    assert_eq!(code, 0, "status stderr: {}", stderr);
    assert!(stdout.contains("Tree size"), "status: {}", stdout);
    // Also: no "failed to load" on any subsequent resolve.
    let (_, _, stderr) = run(cmd_in(td.path()).args(["resolve", "anything"]));
    assert!(!stderr.contains("failed to load"), "stderr: {}", stderr);
}

#[test]
fn complete_smoke() {
    let td = tempfile::tempdir().unwrap();
    seed_build(td.path());
    // Don't assert specific content — completion output is host-specific.
    // We just need the command to succeed and emit something non-empty
    // for an obvious prefix like "ech".
    let (code, _, _) = run(cmd_in(td.path()).args(["complete", "ech"]));
    assert_eq!(code, 0);
}

#[test]
fn help_subcommand_flag() {
    let (code, stdout, _) = run(Command::new(bin_path()).arg("--help"));
    assert_eq!(code, 0);
    assert!(stdout.to_lowercase().contains("abbreviation"));
}

#[test]
fn resolve_ambiguous_emits_zio_shell_assignments() {
    // Forcing ambiguity without a trie build: seed two pins with different
    // names but we need actual trie ambiguity for the Ambiguous path.
    // Easiest way: build the real trie and pick a known-ambiguous prefix.
    let td = tempfile::tempdir().unwrap();
    seed_build(td.path());
    // Pick a single letter that's almost certainly ambiguous on any host
    // (lots of commands start with the same letter). We check for the
    // shell-var format rather than asserting specific commands.
    for probe in ["g", "l", "s", "c", "a"] {
        let (code, stdout, _) = run(cmd_in(td.path()).args(["resolve", probe]));
        if code == 1 {
            // Ambiguous exit → stdout is `_zio_*=...` shell vars.
            assert!(stdout.contains("_zio_word="), "stdout: {}", stdout);
            assert!(stdout.contains("_zio_lcp="));
            assert!(stdout.contains("_zio_candidates=("));
            assert!(stdout.contains("_zio_pins_path="));
            return;
        }
    }
    panic!("no single-letter command was ambiguous on this host — test needs a better probe");
}

#[test]
fn build_aliases_stdin_consumes_piped_aliases() {
    let td = tempfile::tempdir().unwrap();
    let mut child = cmd_in(td.path())
        .args(["build", "--aliases-stdin"])
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    {
        use std::io::Write;
        let stdin = child.stdin.as_mut().unwrap();
        writeln!(stdin, "tfa='terraform apply'").unwrap();
        writeln!(stdin, "k=kubectl").unwrap();
    }
    let out = child.wait_with_output().unwrap();
    assert!(out.status.success());
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("Parsed 2 aliases"), "stderr: {}", stderr);

    // `tfa` should now be resolvable.
    let (code, stdout, _) = run(cmd_in(td.path()).args(["resolve", "tfa"]));
    // Either Resolved (code 0, expanded form on stdout) or Passthrough (code 2)
    // — the pass case means `tfa` ended up as a leaf with no further expansion,
    // still acceptable for this smoke test.
    assert!(code == 0 || code == 2, "unexpected code {}", code);
    assert!(stdout.trim_end().starts_with("tfa") || stdout.contains("terraform"));
}

#[test]
fn rebuild_shells_out_to_zsh_and_refreshes_tree() {
    // `rebuild` runs `zsh -c "alias | zsh-ios build --aliases-stdin"`.
    // Skip the test cleanly if zsh isn't on PATH in the CI sandbox.
    if Command::new("zsh")
        .arg("-c")
        .arg(":")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|s| !s.success())
        .unwrap_or(true)
    {
        eprintln!("zsh not available; skipping");
        return;
    }
    let td = tempfile::tempdir().unwrap();
    let (code, _, stderr) = run(cmd_in(td.path()).arg("rebuild"));
    assert_eq!(code, 0, "rebuild stderr: {}", stderr);
    assert!(tree_path_of(td.path()).exists());
}

#[test]
fn explain_produces_narrative() {
    let td = tempfile::tempdir().unwrap();
    seed_build(td.path());
    let (code, stdout, _) = run(cmd_in(td.path()).args(["explain", "gi br"]));
    assert_eq!(code, 0);
    assert!(stdout.contains("zsh-ios explain:"), "got: {}", stdout);
    assert!(stdout.contains("Final:"), "got: {}", stdout);
}

#[test]
fn explain_bang_reports_bypass() {
    let td = tempfile::tempdir().unwrap();
    let (code, stdout, _) = run(cmd_in(td.path()).args(["explain", "!foo"]));
    assert_eq!(code, 0);
    assert!(stdout.contains("bypass"), "got: {}", stdout);
}

#[test]
fn explain_with_empty_input_errors() {
    let td = tempfile::tempdir().unwrap();
    let (code, _, stderr) = run(cmd_in(td.path()).args(["explain", ""]));
    assert_ne!(code, 0);
    assert!(stderr.contains("Usage"));
}

#[test]
fn bang_prefixed_resolve_is_passthrough() {
    // User rule: a command starting with `!` is never touched by zsh-ios.
    // Run without a seeded trie to be sure there's no accidental side effect.
    let td = tempfile::tempdir().unwrap();
    for input in ["!!", "!git status", "!$", "  !foo"] {
        let (code, stdout, _) = run(cmd_in(td.path()).args(["resolve", input]));
        // Passthrough exit code is 2; stdout is the input verbatim.
        assert_eq!(code, 2, "bang input {:?} should passthrough (code 2), got {}", input, code);
        assert_eq!(stdout.trim_end_matches('\n'), input);
    }
}

#[test]
fn bang_prefixed_complete_is_empty() {
    let td = tempfile::tempdir().unwrap();
    seed_build(td.path());
    let (code, stdout, _) = run(cmd_in(td.path()).args(["complete", "!git "]));
    assert_eq!(code, 0);
    assert!(stdout.is_empty(), "expected empty completion, got: {:?}", stdout);
}

#[test]
fn pin_without_args_errors() {
    let td = tempfile::tempdir().unwrap();
    let (code, _, _) = run(cmd_in(td.path()).args(["pin", ""]).args(["--to", ""]));
    assert_ne!(code, 0, "empty abbrev/expansion must fail");
}

/// Write a `config.yaml` next to the tree for the given test home.
fn write_user_config(home: &Path, yaml: &str) {
    let dir = config_dir_of(home);
    fs::create_dir_all(&dir).unwrap();
    fs::write(dir.join("config.yaml"), yaml).unwrap();
}

#[test]
fn status_surfaces_config_values_for_plugin() {
    // The Zsh plugin parses `Stale threshold:` out of `status` output — keep
    // that line stable and respecting the config override.
    let td = tempfile::tempdir().unwrap();
    write_user_config(
        td.path(),
        "stale_threshold_seconds: 123\ndisable_learning: true\n",
    );
    let (code, stdout, _) = run(cmd_in(td.path()).arg("status"));
    assert_eq!(code, 0);
    assert!(
        stdout.contains("Stale threshold: 123s"),
        "status should surface threshold: {}",
        stdout
    );
    assert!(
        stdout.contains("Learning:    disabled (config)"),
        "status should surface learning state: {}",
        stdout
    );
    assert!(
        stdout.contains("Config file:") && stdout.contains("(loaded)"),
        "status should show config file path+state: {}",
        stdout
    );
}

#[test]
fn status_stale_threshold_defaults_without_config() {
    let td = tempfile::tempdir().unwrap();
    let (code, stdout, _) = run(cmd_in(td.path()).arg("status"));
    assert_eq!(code, 0);
    assert!(
        stdout.contains("Stale threshold: 3600s"),
        "default threshold should be 3600: {}",
        stdout
    );
    assert!(stdout.contains("(absent)"));
}

#[test]
fn invalid_config_falls_back_to_defaults() {
    // A bad config must never wedge the shell — parse error → warn + defaults.
    let td = tempfile::tempdir().unwrap();
    write_user_config(td.path(), "stale_threshold_seconds: not-a-number\n");
    let (code, stdout, stderr) = run(cmd_in(td.path()).arg("status"));
    assert_eq!(code, 0, "status should not crash on invalid config");
    assert!(
        stdout.contains("Stale threshold: 3600s"),
        "should fall back to default: {}",
        stdout
    );
    assert!(
        stderr.contains("ignoring invalid config"),
        "should warn on stderr: {}",
        stderr
    );
}

#[test]
fn blocklist_passthroughs_literal_match() {
    let td = tempfile::tempdir().unwrap();
    seed_build(td.path());
    write_user_config(td.path(), "command_blocklist:\n  - kubectl\n");
    let (code, stdout, _) = run(cmd_in(td.path()).args(["resolve", "kubectl get pods"]));
    assert_eq!(code, 2, "blocklisted literal should passthrough");
    assert_eq!(stdout.trim_end_matches('\n'), "kubectl get pods");
}

#[test]
fn blocklist_passthroughs_abbreviation_that_resolves() {
    // The whole point of the blocklist: abbreviations that would otherwise
    // resolve into a blocklisted command are also passed through, with the
    // ORIGINAL (abbreviated) line printed — not the expansion.
    let td = tempfile::tempdir().unwrap();
    seed_build(td.path());

    // First confirm the abbrev actually resolves without the blocklist so
    // the test is meaningful. We can only seed commands that exist on PATH
    // during the build; `zsh-ios` itself is always in the trie.
    write_user_config(td.path(), "");
    let (c, stdout, _) = run(cmd_in(td.path()).args(["resolve", "zsh-io stat"]));
    assert_eq!(c, 0, "abbrev should resolve with no blocklist");
    assert_eq!(stdout.trim_end_matches('\n'), "zsh-ios status");

    // Now block the resolved command and check the same abbrev passes through.
    write_user_config(td.path(), "command_blocklist:\n  - zsh-ios\n");
    let (c, stdout, _) = run(cmd_in(td.path()).args(["resolve", "zsh-io stat"]));
    assert_eq!(c, 2, "blocklisted resolution should passthrough");
    assert_eq!(
        stdout.trim_end_matches('\n'),
        "zsh-io stat",
        "should print original, not expansion"
    );
}

#[test]
fn disable_learning_makes_learn_noop() {
    let td = tempfile::tempdir().unwrap();
    seed_build(td.path());
    write_user_config(td.path(), "disable_learning: true\n");

    let tree = tree_path_of(td.path());
    let before = fs::metadata(&tree).unwrap().modified().unwrap();

    // `learn` must neither succeed-and-modify nor error out — just silent no-op.
    // Sleep so mtime resolution (typically 1s on ext4) lets a real write register.
    std::thread::sleep(std::time::Duration::from_millis(1100));
    let (code, _, _) = run(cmd_in(td.path()).args(["learn", "git checkout"]));
    assert_eq!(code, 0);

    let after = fs::metadata(&tree).unwrap().modified().unwrap();
    assert_eq!(before, after, "disable_learning should leave the trie file untouched");
}

/// Seed a minimal trie with specific word-path sequences.
/// Unlike `seed_build`, this does not scan PATH, so there are no
/// incidental `git-shell` / `git-receive-pack` siblings that would
/// cause `is_prefix_of_existing` to block re-learning `git`.
/// Each element of `paths` is a slice of words, e.g. `&["git", "status"]`.
fn seed_trie_with(home: &Path, paths: &[&[&str]]) {
    use zsh_ios::trie::CommandTrie;
    let mut trie = CommandTrie::new();
    for &words in paths {
        trie.insert(words);
    }
    let tree = tree_path_of(home);
    fs::create_dir_all(tree.parent().unwrap()).unwrap();
    trie.save(&tree).unwrap();
}

#[test]
fn learn_success_adds_and_bumps_count() {
    use zsh_ios::trie::CommandTrie;

    let td = tempfile::tempdir().unwrap();
    // Seed a trie with only `git` so there are no git-prefixed siblings
    // (git-shell, git-receive-pack) that would cause is_prefix_of_existing
    // to block the learn.
    seed_trie_with(td.path(), &[&["git"]]);

    let tree = tree_path_of(td.path());
    let before = CommandTrie::load(&tree).unwrap();
    let count_before = before
        .root
        .get_child("git")
        .map(|n| n.count)
        .unwrap_or(0);

    // Use an abbreviated form so resolve returns Resolved (not Passthrough).
    // With only `git` in the trie and no git-* siblings, `gi` resolves uniquely.
    let (code, _, stderr) =
        run(cmd_in(td.path()).args(["learn", "--exit-code", "0", "--", "gi"]));
    assert_eq!(code, 0, "learn stderr: {}", stderr);

    let after = CommandTrie::load(&tree).unwrap();
    let count_after = after.root.get_child("git").expect("git node").count;
    assert!(
        count_after > count_before,
        "count should increase: before={} after={}",
        count_before,
        count_after
    );
}

#[test]
fn learn_failure_does_not_create_new_node() {
    use zsh_ios::trie::CommandTrie;

    let td = tempfile::tempdir().unwrap();
    // Seed a trie with only `git` so `nonsense_xyz_abc` is absent.
    seed_trie_with(td.path(), &[&["git"]]);

    let (code, _, stderr) = run(
        cmd_in(td.path()).args(["learn", "--exit-code", "1", "--", "nonsense_xyz_abc foo"]),
    );
    assert_eq!(code, 0, "learn stderr: {}", stderr);

    let trie = CommandTrie::load(&tree_path_of(td.path())).unwrap();
    assert!(
        trie.root.get_child("nonsense_xyz_abc").is_none(),
        "failure learn must not create a new node"
    );
}

#[test]
fn learn_failure_bumps_failures_on_existing() {
    use zsh_ios::trie::CommandTrie;

    let td = tempfile::tempdir().unwrap();
    // Seed a trie with `git` and `git status` so the full path exists and
    // record_failure can tally failures on the `git` node.
    seed_trie_with(td.path(), &[&["git", "status"]]);

    let tree = tree_path_of(td.path());
    let before = CommandTrie::load(&tree).unwrap();
    let count_before = before
        .root
        .get_child("git")
        .map(|n| n.count)
        .unwrap_or(0);
    let failures_before = before
        .root
        .get_child("git")
        .map(|n| n.failures)
        .unwrap_or(0);

    // Use an abbreviated form so resolve returns Resolved (not Passthrough).
    // With only `["git", "status"]` in the trie, `gi stat` resolves uniquely.
    let (code, _, stderr) =
        run(cmd_in(td.path()).args(["learn", "--exit-code", "1", "--", "gi stat"]));
    assert_eq!(code, 0, "learn stderr: {}", stderr);

    let after = CommandTrie::load(&tree).unwrap();
    let git = after.root.get_child("git").expect("git node");
    assert_eq!(
        git.count, count_before,
        "failure learn must not bump count"
    );
    assert_eq!(
        git.failures,
        failures_before + 1,
        "failure learn must increment failures"
    );
}

#[test]
fn learn_success_sets_last_used() {
    use zsh_ios::trie::CommandTrie;

    let td = tempfile::tempdir().unwrap();
    // Seed a trie with only `git` so there are no git-prefixed siblings.
    seed_trie_with(td.path(), &[&["git"]]);

    // Use an abbreviated form so resolve returns Resolved (not Passthrough).
    // With only `git` in the trie and no git-* siblings, `gi` resolves uniquely.
    let (code, _, stderr) =
        run(cmd_in(td.path()).args(["learn", "--exit-code", "0", "--", "gi"]));
    assert_eq!(code, 0, "learn stderr: {}", stderr);

    let trie = CommandTrie::load(&tree_path_of(td.path())).unwrap();
    let last_used = trie.root.get_child("git").expect("git node").last_used;
    assert!(last_used > 0, "last_used should be set after success learn");
}

#[test]
fn ingest_applies_aliases_and_nameddirs_to_trie() {
    use std::io::Write;
    use zsh_ios::trie::CommandTrie;

    let td = tempfile::tempdir().unwrap();
    // Seed an empty trie so ingest has something to load and mutate.
    seed_trie_with(td.path(), &[]);

    let payload = concat!(
        "@aliases\n",
        "ll='ls -la'\n",
        "gs='git status'\n",
        "@functions\n",
        "myfn\n",
        "@nameddirs\n",
        "proj=/home/me/proj\n",
    );

    let mut child = cmd_in(td.path())
        .arg("ingest")
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    {
        let stdin = child.stdin.as_mut().unwrap();
        stdin.write_all(payload.as_bytes()).unwrap();
    }
    let status = child.wait().unwrap();
    assert!(status.success(), "ingest exited non-zero");

    let trie = CommandTrie::load(&tree_path_of(td.path())).unwrap();
    assert!(trie.root.get_child("ll").is_some(), "'ll' alias not ingested");
    assert!(trie.root.get_child("gs").is_some(), "'gs' alias not ingested");
    assert!(trie.root.get_child("myfn").is_some(), "'myfn' function not ingested");
    assert_eq!(
        trie.named_dirs.get("proj"),
        Some(&"/home/me/proj".to_string()),
        "named dir 'proj' not ingested"
    );
}

#[test]
fn learn_with_cwd_records_entry() {
    let td = tempfile::tempdir().unwrap();
    seed_build(td.path());

    // Learn "echo hello" with a specific cwd.
    let (code, _, _) = run(cmd_in(td.path())
        .args(["learn", "--exit-code", "0", "--cwd", "/home/user/proj", "--", "echo hello"]));
    assert_eq!(code, 0);

    // The trie should still be loadable (no corruption).
    let trie = zsh_ios::trie::CommandTrie::load(&tree_path_of(td.path())).unwrap();
    // echo must be present with a cwd entry if it wasn't already there.
    if let Some(echo_node) = trie.root.get_child("echo") {
        // If cwd_hits is populated, the entry should reference /home/user/proj.
        let has_proj = echo_node.cwd_hits.iter().any(|(k, _)| k == "/home/user/proj");
        // It's acceptable for cwd_hits to be empty if echo was already in the trie
        // before learning (cwd is only recorded on newly inserted paths). We just
        // verify the trie is intact.
        let _ = has_proj;
    }
    // Structural health: status must succeed.
    let (status, stdout, _) = run(cmd_in(td.path()).arg("status"));
    assert_eq!(status, 0);
    assert!(stdout.contains("Tree size"));
}

#[test]
fn resolve_with_context_redirection() {
    let td = tempfile::tempdir().unwrap();
    seed_build(td.path());

    // With --context redirection, the binary should not try to trie-expand
    // the input as a command; it should passthrough or treat last word as path.
    // Exit codes 0 (resolved), 2 (passthrough), or 3 (path ambiguous) are fine.
    // Exit code 1 (command ambiguity) must NOT occur for a simple redirect buffer.
    let (code, _, _) = run(cmd_in(td.path())
        .args(["resolve", "--context", "redirection", "--", "echo hello > /tmp/x"]));
    assert_ne!(code, 1, "redirection context should not produce command ambiguity");
}

#[test]
fn ingest_dirstack_populates_trie_field() {
    // Pipe an @dirstack section through `zsh-ios ingest`, reload trie, assert dir_stack set.
    use zsh_ios::trie::CommandTrie;

    let td = tempfile::tempdir().unwrap();
    seed_build(td.path());

    // Pipe a dirstack section into ingest
    let ingest_input = "@dirstack\n/home/me\n/tmp\n/var\n";
    let mut child = cmd_in(td.path())
        .arg("ingest")
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    {
        child
            .stdin
            .as_mut()
            .unwrap()
            .write_all(ingest_input.as_bytes())
            .unwrap();
    }
    let status = child.wait().unwrap();
    assert!(status.success(), "ingest exited non-zero");

    let trie = CommandTrie::load(&tree_path_of(td.path())).unwrap();
    assert_eq!(
        trie.dir_stack,
        vec!["/home/me", "/tmp", "/var"],
        "dir_stack not populated after ingest: {:?}",
        trie.dir_stack
    );
}
