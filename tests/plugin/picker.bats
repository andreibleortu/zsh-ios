#!/usr/bin/env bats
#
# Covers the keystroke-driven picker in `_zsh_ios_handle_ambiguity` —
# digit auto-accept, multi-digit wait-or-commit, Tab cycle, Backspace,
# Enter commit-or-cancel, typo-drop.
#
# Each test pre-populates the picker's input-fd with a raw byte sequence
# via `keystrokes`, then invokes `_zsh_ios_handle_ambiguity <payload> expand`
# and asserts on the resulting BUFFER + recorded zle calls.

load 'helpers/test_helper'

# Shared snippet prologue: 3-option ambiguity, BUFFER="c", picker wired to
# the test input fd. We glue the per-test body after this.
three_opts_prologue='
payload="_zio_word='\''c'\''
_zio_lcp='\''c'\''
_zio_position=0
_zio_resolved_prefix='\'''\''
_zio_remaining='\'''\''
_zio_candidates=(cat cd cargo)
_zio_deep_items=()
_zio_deep_display=()
_zio_pins_path='\''/tmp/pins.txt'\''"
BUFFER="c"
exec {fd}<$TMP_INPUT
_ZSH_IOS_TEST_INPUT_FD=$fd
_zsh_ios_handle_ambiguity "$payload" expand
exec {fd}<&-
print -r -- "BUFFER=$BUFFER"
print -r -- "CURSOR=$CURSOR"
print -r -- "ZLE=${_zle_calls[*]}"
'

# 20-option payload for multi-digit tests.
twenty_opts_prologue='
cands=()
for i in {1..20}; do cands+=("cmd$i"); done
payload="_zio_word='\''c'\''
_zio_lcp='\''c'\''
_zio_position=0
_zio_resolved_prefix='\'''\''
_zio_remaining='\'''\''
_zio_candidates=(${cands[*]})
_zio_deep_items=()
_zio_deep_display=()
_zio_pins_path='\''/tmp/pins.txt'\''"
BUFFER="c"
exec {fd}<$TMP_INPUT
_ZSH_IOS_TEST_INPUT_FD=$fd
_zsh_ios_handle_ambiguity "$payload" expand
exec {fd}<&-
print -r -- "BUFFER=$BUFFER"
print -r -- "ZLE=${_zle_calls[*]}"
'

# ─── Digit auto-accept ──────────────────────────────────────────────────────

@test "digit: single keystroke commits instantly in a 3-option menu" {
    keystrokes '3'
    run zsh_run "$three_opts_prologue"
    [[ "$status" -eq 0 ]]
    [[ "$output" == *"BUFFER=cargo"* ]]
    # expand mode → reset-prompt, not accept-line
    [[ "$output" == *"ZLE="*"reset-prompt"* ]]
    [[ "$output" != *"accept-line"* ]]
}

@test "digit: 1 in 3-option menu auto-accepts (no longer number extends 1)" {
    keystrokes '1'
    run zsh_run "$three_opts_prologue"
    [[ "$status" -eq 0 ]]
    [[ "$output" == *"BUFFER=cat"* ]]
}

@test "digit: out-of-range digit is silently dropped, then Enter cancels" {
    # `7` isn't a valid option, should be dropped. Enter on empty = cancel.
    keystrokes "7${KEY_ENTER}"
    run zsh_run "$three_opts_prologue"
    [[ "$status" -eq 0 ]]
    # BUFFER unchanged (still "c") — cancel path.
    [[ "$output" == *"BUFFER=c"$'\n'* ]]
    [[ "$output" == *"reset-prompt"* ]]
}

@test "digit: 0 at start is silently dropped" {
    # 0 is never a valid option. Drop it, then `2` accepts option 2.
    keystrokes '02'
    run zsh_run "$three_opts_prologue"
    [[ "$status" -eq 0 ]]
    [[ "$output" == *"BUFFER=cd"* ]]
}

# ─── Multi-digit (waits for extendable prefix) ──────────────────────────────

@test "multi-digit: 1 in a 20-option menu waits (10-19 still reachable)" {
    # Type 1, then Enter to cancel (the pending digit isn't a commit).
    keystrokes "1${KEY_ENTER}"
    run zsh_run "$twenty_opts_prologue"
    [[ "$status" -eq 0 ]]
    # Enter with digits buffered → commit. So this commits choice=1.
    [[ "$output" == *"BUFFER=cmd1"* ]]
}

@test "multi-digit: 13 in a 20-option menu commits instantly (30+ unreachable)" {
    keystrokes '13'
    run zsh_run "$twenty_opts_prologue"
    [[ "$status" -eq 0 ]]
    [[ "$output" == *"BUFFER=cmd13"* ]]
}

@test "multi-digit: backspace erases a buffered digit" {
    # Type 1 (waits), backspace (erases), 3 (auto-commits — no 3X in 20).
    keystrokes "1${KEY_BACKSPACE}3"
    run zsh_run "$twenty_opts_prologue"
    [[ "$status" -eq 0 ]]
    [[ "$output" == *"BUFFER=cmd3"* ]]
}

# ─── Tab cycle ──────────────────────────────────────────────────────────────

@test "tab: single Tab + Enter picks option 1" {
    keystrokes "${KEY_TAB}${KEY_ENTER}"
    run zsh_run "$three_opts_prologue"
    [[ "$status" -eq 0 ]]
    [[ "$output" == *"BUFFER=cat"* ]]
}

@test "tab: two Tabs + Enter picks option 2" {
    keystrokes "${KEY_TAB}${KEY_TAB}${KEY_ENTER}"
    run zsh_run "$three_opts_prologue"
    [[ "$status" -eq 0 ]]
    [[ "$output" == *"BUFFER=cd"* ]]
}

@test "tab: wraps past the last option (4 Tabs in 3-menu = option 1)" {
    keystrokes "${KEY_TAB}${KEY_TAB}${KEY_TAB}${KEY_TAB}${KEY_ENTER}"
    run zsh_run "$three_opts_prologue"
    [[ "$status" -eq 0 ]]
    [[ "$output" == *"BUFFER=cat"* ]]
}

@test "tab: backspace steps the cycle back one position" {
    # Tab Tab Tab → idx=3. Backspace → idx=2. Enter commits option 2.
    keystrokes "${KEY_TAB}${KEY_TAB}${KEY_TAB}${KEY_BACKSPACE}${KEY_ENTER}"
    run zsh_run "$three_opts_prologue"
    [[ "$status" -eq 0 ]]
    [[ "$output" == *"BUFFER=cd"* ]]
}

@test "tab: digit after Tab jumps directly to that number" {
    # Tab (idx=1), then digit 3 → should auto-commit option 3 (clears cycle).
    keystrokes "${KEY_TAB}3"
    run zsh_run "$three_opts_prologue"
    [[ "$status" -eq 0 ]]
    [[ "$output" == *"BUFFER=cargo"* ]]
}

# ─── Enter semantics ────────────────────────────────────────────────────────

@test "enter: empty buffer cancels, BUFFER unchanged" {
    keystrokes "${KEY_ENTER}"
    run zsh_run "$three_opts_prologue"
    [[ "$status" -eq 0 ]]
    [[ "$output" == *"BUFFER=c"$'\n'* ]]
    [[ "$output" == *"reset-prompt"* ]]
}

# ─── Typo cancels ───────────────────────────────────────────────────────────

@test "typo: arbitrary key cancels the picker" {
    keystrokes 'x'
    run zsh_run "$three_opts_prologue"
    [[ "$status" -eq 0 ]]
    [[ "$output" == *"BUFFER=c"$'\n'* ]]
    [[ "$output" == *"reset-prompt"* ]]
}

# ─── expand mode: no accept-line ────────────────────────────────────────────

@test "expand mode: never calls accept-line even on commit" {
    # Explicit safety check: the Tab-path MUST NOT run the command.
    keystrokes '2'
    run zsh_run "$three_opts_prologue"
    [[ "$status" -eq 0 ]]
    [[ "$output" == *"BUFFER=cd"* ]]
    [[ "$output" != *"accept-line"* ]]
    [[ "$output" == *"reset-prompt"* ]]
}

@test "save-mode: plain digit pick does NOT write a pin" {
    export ZSH_IOS_STUB_LOG="$(mktemp)"
    keystrokes '2'
    run zsh_run "$three_opts_prologue"
    [[ "$status" -eq 0 ]]
    [[ "$output" == *"BUFFER=cd"* ]]
    # No pin subcommand should have been invoked.
    run grep -c '^pin|' "$ZSH_IOS_STUB_LOG"
    [[ "$output" == "0" ]]
    rm -f "$ZSH_IOS_STUB_LOG"
}

@test "save-mode: !<digit> pick DOES write a pin" {
    export ZSH_IOS_STUB_LOG="$(mktemp)"
    keystrokes '!2'
    run zsh_run "$three_opts_prologue"
    [[ "$status" -eq 0 ]]
    [[ "$output" == *"BUFFER=cd"* ]]
    [[ "$output" == *"Saved pin:"* ]]
    run grep -c '^pin|' "$ZSH_IOS_STUB_LOG"
    [[ "$output" == "1" ]]
    rm -f "$ZSH_IOS_STUB_LOG"
}

@test "save-mode: toggle on then off with ! does not save" {
    export ZSH_IOS_STUB_LOG="$(mktemp)"
    # First `!` enters save mode, second `!` leaves it.
    keystrokes '!!2'
    run zsh_run "$three_opts_prologue"
    [[ "$status" -eq 0 ]]
    [[ "$output" == *"BUFFER=cd"* ]]
    [[ "$output" != *"Saved pin:"* ]]
    run grep -c '^pin|' "$ZSH_IOS_STUB_LOG"
    [[ "$output" == "0" ]]
    rm -f "$ZSH_IOS_STUB_LOG"
}

@test "accept mode: commits AND calls accept-line" {
    # Same payload but accept mode — should call accept-line.
    local snippet='
payload="_zio_word='\''c'\''
_zio_lcp='\''c'\''
_zio_position=0
_zio_resolved_prefix='\'''\''
_zio_remaining='\'''\''
_zio_candidates=(cat cd cargo)
_zio_deep_items=()
_zio_deep_display=()
_zio_pins_path='\''/tmp/pins.txt'\''"
BUFFER="c"
exec {fd}<$TMP_INPUT
_ZSH_IOS_TEST_INPUT_FD=$fd
_zsh_ios_handle_ambiguity "$payload" accept
exec {fd}<&-
print -r -- "BUFFER=$BUFFER"
print -r -- "ZLE=${_zle_calls[*]}"
'
    keystrokes '2'
    run zsh_run "$snippet"
    [[ "$status" -eq 0 ]]
    [[ "$output" == *"BUFFER=cd"* ]]
    [[ "$output" == *"accept-line"* ]]
}
