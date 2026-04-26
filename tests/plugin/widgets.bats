#!/usr/bin/env bats
#
# Covers the ZLE widget glue: leading-`!` bypass and the two-stage Tab
# state machine (first Tab = LCP+hint, second Tab on unchanged buffer =
# picker). These are where the user-visible contract lives.

load 'helpers/test_helper'

# ─── Leading `!` bypass ─────────────────────────────────────────────────────
# Rule: any BUFFER starting with `!` must pass through untouched — no binary
# call, no resolution, no completion. History expansion and literal-run are
# zsh's business, not ours.

@test "bang bypass: Enter widget delegates to zle accept-line verbatim" {
    export ZSH_IOS_STUB_LOG="$BATS_TMPDIR/stub_log_bang_enter_$$"
    run zsh_run '
BUFFER="!!"
_zsh_ios_accept_line
print -r -- "BUFFER=$BUFFER"
print -r -- "ZLE=${_zle_calls[*]}"
'
    [[ "$status" -eq 0 ]]
    [[ "$output" == *"BUFFER=!!"* ]]
    [[ "$output" == *"ZLE=accept-line"* ]]
    # No resolve call to the stub — the log file should not exist or be empty.
    [[ ! -s "$ZSH_IOS_STUB_LOG" ]]
    rm -f "$ZSH_IOS_STUB_LOG"
}

@test "bang bypass: Tab widget delegates to zle expand-or-complete" {
    export ZSH_IOS_STUB_LOG="$BATS_TMPDIR/stub_log_bang_tab_$$"
    run zsh_run '
BUFFER="!git status"
_zsh_ios_expand_or_complete
print -r -- "BUFFER=$BUFFER"
print -r -- "ZLE=${_zle_calls[*]}"
'
    [[ "$status" -eq 0 ]]
    [[ "$output" == *"BUFFER=!git status"* ]]
    [[ "$output" == *"ZLE=expand-or-complete"* ]]
    [[ ! -s "$ZSH_IOS_STUB_LOG" ]]
    rm -f "$ZSH_IOS_STUB_LOG"
}

@test "bang bypass: ? widget delegates to zle self-insert" {
    export ZSH_IOS_STUB_LOG="$BATS_TMPDIR/stub_log_bang_help_$$"
    run zsh_run '
BUFFER="!grep"
CURSOR=5
_zsh_ios_help
print -r -- "BUFFER=$BUFFER"
print -r -- "ZLE=${_zle_calls[*]}"
'
    [[ "$status" -eq 0 ]]
    [[ "$output" == *"ZLE=self-insert"* ]]
    [[ ! -s "$ZSH_IOS_STUB_LOG" ]]
    rm -f "$ZSH_IOS_STUB_LOG"
}

# ─── Two-stage Tab state machine ────────────────────────────────────────────
# Contract:
#   • First Tab on an ambiguous buffer: stash buffer, do LCP extend + hint.
#   • Second Tab on the unchanged buffer: clear stash, invoke the picker.
#   • Any non-ambiguous Tab clears the stash.
#   • Any buffer edit naturally clears (state check is equality).

@test "tab stage 1: first press on ambig stashes buffer + shows hint" {
    # Stub `resolve` to return exit 1 with a minimal _zio_* payload.
    export ZSH_IOS_STUB_RESOLVE_EXIT=1
    export ZSH_IOS_STUB_RESOLVE_OUT="_zio_word='tes'
_zio_lcp='tes'
_zio_position=0
_zio_resolved_prefix=''
_zio_remaining=''
_zio_candidates=(test test-yaml)
_zio_deep_items=()
_zio_deep_display=()
_zio_pins_path='/tmp/pins.txt'"

    run zsh_run '
BUFFER="tes"
_zsh_ios_last_tab_buffer=""
_zsh_ios_expand_or_complete
print -r -- "BUFFER=$BUFFER"
print -r -- "STASH=$_zsh_ios_last_tab_buffer"
print -r -- "ZLE=${_zle_calls[*]}"
'
    [[ "$status" -eq 0 ]]
    [[ "$output" == *"BUFFER=tes"* ]]  # LCP == word, no extension
    [[ "$output" == *"STASH=tes"* ]]   # first Tab stashes
    # zle -M should have been called to show the hint.
    [[ "$output" == *"-M"* ]]
}

@test "tab stage 2: second press on unchanged buffer triggers picker" {
    export ZSH_IOS_STUB_RESOLVE_EXIT=1
    export ZSH_IOS_STUB_RESOLVE_OUT="_zio_word='tes'
_zio_lcp='tes'
_zio_position=0
_zio_resolved_prefix=''
_zio_remaining=''
_zio_candidates=(test test-yaml)
_zio_deep_items=()
_zio_deep_display=()
_zio_pins_path='/tmp/pins.txt'"

    keystrokes '1'
    run zsh_run '
BUFFER="tes"
# Simulate state left over from a previous Tab.
_zsh_ios_last_tab_buffer="tes"
exec {fd}<$TMP_INPUT
_ZSH_IOS_TEST_INPUT_FD=$fd
_zsh_ios_expand_or_complete
exec {fd}<&-
print -r -- "BUFFER=$BUFFER"
print -r -- "STASH=$_zsh_ios_last_tab_buffer"
'
    [[ "$status" -eq 0 ]]
    # Picker saw key "1" → option 1 = "test".
    [[ "$output" == *"BUFFER=test"* ]]
    # Stash cleared so a third Tab would start fresh.
    grep -qx 'STASH=' <<<"$output"
}

@test "tab state: buffer edit between Tabs clears the stash implicitly" {
    export ZSH_IOS_STUB_RESOLVE_EXIT=1
    export ZSH_IOS_STUB_RESOLVE_OUT="_zio_word='te'
_zio_lcp='te'
_zio_position=0
_zio_resolved_prefix=''
_zio_remaining=''
_zio_candidates=(test test-yaml terraform)
_zio_deep_items=()
_zio_deep_display=()
_zio_pins_path='/tmp/pins.txt'"

    run zsh_run '
# Stash is from an earlier Tab at "tes"; user then backspaced to "te".
_zsh_ios_last_tab_buffer="tes"
BUFFER="te"
_zsh_ios_expand_or_complete
print -r -- "STASH=$_zsh_ios_last_tab_buffer"
print -r -- "ZLE=${_zle_calls[*]}"
'
    [[ "$status" -eq 0 ]]
    # Buffer "te" != stash "tes", so we take the "first-Tab" branch: stash
    # updates to the new buffer and the hint fires via `zle -M`.
    [[ "$output" == *"STASH=te"* ]]
    [[ "$output" == *"-M"* ]]
}

@test "tab: successful resolve clears the stash" {
    export ZSH_IOS_STUB_RESOLVE_EXIT=0
    export ZSH_IOS_STUB_RESOLVE_OUT='git branch'

    run zsh_run '
BUFFER="gi br"
_zsh_ios_last_tab_buffer="gi br"   # leftover from some earlier state
_zsh_ios_expand_or_complete
print -r -- "BUFFER=$BUFFER"
print -r -- "STASH=$_zsh_ios_last_tab_buffer"
'
    [[ "$status" -eq 0 ]]
    [[ "$output" == *"BUFFER=git branch"* ]]
    # Stash cleared because exit 0 is the "resolved" branch.
    grep -qx 'STASH=' <<<"$output"
}

# ─── Disabled flag ──────────────────────────────────────────────────────────
# When the user runs `zsh-ios toggle` (creating a `disabled` marker file),
# every widget must fall through to its native zsh behavior without calling
# the binary.

@test "disabled: Enter widget falls through when disabled marker exists" {
    # Override the config dir so the disabled marker lives somewhere we own.
    export XDG_CONFIG_HOME="$BATS_TMPDIR/xdg_disabled_$$"
    mkdir -p "$XDG_CONFIG_HOME/zsh-ios"
    touch "$XDG_CONFIG_HOME/zsh-ios/disabled"
    export ZSH_IOS_STUB_LOG="$BATS_TMPDIR/stub_disabled_$$"

    run zsh_run '
# ZSH_IOS_CONFIG_DIR was captured at source time; override it here.
ZSH_IOS_CONFIG_DIR="'"$XDG_CONFIG_HOME"'/zsh-ios"
_zsh_ios_disabled_file="$ZSH_IOS_CONFIG_DIR/disabled"
BUFFER="gi st"
_zsh_ios_accept_line
print -r -- "ZLE=${_zle_calls[*]}"
'
    [[ "$status" -eq 0 ]]
    [[ "$output" == *"ZLE=accept-line"* ]]
    [[ ! -s "$ZSH_IOS_STUB_LOG" ]]
    rm -rf "$XDG_CONFIG_HOME" "$ZSH_IOS_STUB_LOG"
}

# ─── Dynamic toggle: same session ───────────────────────────────────────────
# The disabled marker file is checked on every widget invocation, so toggling
# (creating or removing the file) must take effect immediately — without
# restarting the shell.

@test "toggle: plugin works normally when no disabled marker" {
    export ZSH_IOS_STUB_RESOLVE_EXIT=0
    export ZSH_IOS_STUB_RESOLVE_OUT='git status'

    run zsh_run '
BUFFER="gi st"
_zsh_ios_accept_line
print -r -- "BUFFER=$BUFFER"
print -r -- "ZLE=${_zle_calls[*]}"
'
    [[ "$status" -eq 0 ]]
    # Widget resolved the abbreviation — BUFFER now holds the expanded form.
    [[ "$output" == *"BUFFER=git status"* ]]
    # accept-line was called (indicating the widget ran through to execution).
    [[ "$output" == *"ZLE=accept-line"* ]]
}

@test "toggle: creating disabled marker stops widget mid-session (no restart)" {
    export ZSH_IOS_STUB_RESOLVE_EXIT=0
    export ZSH_IOS_STUB_RESOLVE_OUT='git status'

    # Build a dedicated config dir we own so we can create/remove the marker.
    local cfg_dir="$BATS_TMPDIR/zio_toggle_mid_$$"
    mkdir -p "$cfg_dir"

    run zsh_run '
# Point the plugin at our test config dir (simulating a fresh session env).
ZSH_IOS_CONFIG_DIR="'"$cfg_dir"'"
_zsh_ios_disabled_file="$ZSH_IOS_CONFIG_DIR/disabled"

# ── Step 1: no marker yet → widget should resolve ──
BUFFER="gi st"
_zle_calls=()
_zsh_ios_accept_line
print -r -- "STEP1_BUFFER=$BUFFER"

# ── Step 2: create marker (simulating `zsh-ios toggle`) ──
touch "$ZSH_IOS_CONFIG_DIR/disabled"

BUFFER="gi st"
_zle_calls=()
_zsh_ios_accept_line
print -r -- "STEP2_BUFFER=$BUFFER"
'
    [[ "$status" -eq 0 ]]
    # Step 1: enabled — abbreviation was resolved.
    [[ "$output" == *"STEP1_BUFFER=git status"* ]]
    # Step 2: disabled — BUFFER unchanged (passthrough to native accept-line).
    [[ "$output" == *"STEP2_BUFFER=gi st"* ]]
    rm -rf "$cfg_dir"
}

@test "toggle: removing disabled marker re-enables plugin mid-session (no restart)" {
    export ZSH_IOS_STUB_RESOLVE_EXIT=0
    export ZSH_IOS_STUB_RESOLVE_OUT='git status'

    local cfg_dir="$BATS_TMPDIR/zio_toggle_reenable_$$"
    mkdir -p "$cfg_dir"
    # Start with the marker present (plugin disabled).
    touch "$cfg_dir/disabled"

    run zsh_run '
ZSH_IOS_CONFIG_DIR="'"$cfg_dir"'"
_zsh_ios_disabled_file="$ZSH_IOS_CONFIG_DIR/disabled"

# ── Step 1: marker present → widget should fall through unchanged ──
BUFFER="gi st"
_zle_calls=()
_zsh_ios_accept_line
print -r -- "STEP1_BUFFER=$BUFFER"

# ── Step 2: remove marker (simulating second `zsh-ios toggle`) ──
rm -f "$ZSH_IOS_CONFIG_DIR/disabled"

BUFFER="gi st"
_zle_calls=()
_zsh_ios_accept_line
print -r -- "STEP2_BUFFER=$BUFFER"
'
    [[ "$status" -eq 0 ]]
    # Step 1: disabled — BUFFER unchanged (passthrough to native accept-line).
    [[ "$output" == *"STEP1_BUFFER=gi st"* ]]
    # Step 2: re-enabled — abbreviation resolved.
    [[ "$output" == *"STEP2_BUFFER=git status"* ]]
    rm -rf "$cfg_dir"
}
