#!/usr/bin/env bats
#
# Tests for arrow-key navigation in `_zsh_ios_handle_ambiguity`.
#
# Arrow keys send ANSI escape sequences: ESC [ A (Up), ESC [ B (Down),
# ESC [ C (Right), ESC [ D (Left).  The picker reads ESC, then peeks ahead
# with a short timeout to distinguish a full escape sequence from a lone Esc.
#
# Down / Right advance the cycle highlight (same as Tab).
# Up / Left retreat the cycle highlight (wraps from 1 to max).
# A lone Esc (two consecutive Escs with nothing between) cancels.

load 'helpers/test_helper'

# ANSI escape sequences as shell literals.
KEY_UP=$'\e[A'
KEY_DOWN=$'\e[B'
KEY_RIGHT=$'\e[C'
KEY_LEFT=$'\e[D'

# Shared 3-option ambiguity payload (cat, cd, cargo).
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

# ─── Down arrow ─────────────────────────────────────────────────────────────

@test "arrow: Down + Enter commits option 1 (cycle_idx=1)" {
    # Down moves from 0 → 1.  Enter commits the highlight.
    keystrokes "${KEY_DOWN}${KEY_ENTER}"
    run zsh_run "$three_opts_prologue"
    [[ "$status" -eq 0 ]]
    [[ "$output" == *"BUFFER=cat"* ]]
    [[ "$output" == *"reset-prompt"* ]]
    [[ "$output" != *"accept-line"* ]]
}

@test "arrow: Down Down + Enter commits option 2" {
    keystrokes "${KEY_DOWN}${KEY_DOWN}${KEY_ENTER}"
    run zsh_run "$three_opts_prologue"
    [[ "$status" -eq 0 ]]
    [[ "$output" == *"BUFFER=cd"* ]]
}

@test "arrow: Down then Up reaches last option" {
    # Down: idx=0 -> 1 (cat).  Up: idx=1 -> (1+3-2)%3+1 = 3 (cargo).
    keystrokes "${KEY_DOWN}${KEY_UP}${KEY_ENTER}"
    run zsh_run "$three_opts_prologue"
    [[ "$status" -eq 0 ]]
    [[ "$output" == *"BUFFER=cargo"* ]]
}

@test "arrow: Up from idx=0 moves to option 2 (wrap formula)" {
    # From 0, Up: (0 + 3 - 2) % 3 + 1 = 1 % 3 + 1 = 2 → cd
    keystrokes "${KEY_UP}${KEY_ENTER}"
    run zsh_run "$three_opts_prologue"
    [[ "$status" -eq 0 ]]
    [[ "$output" == *"BUFFER=cd"* ]]
}

# ─── Right arrow (same as Down) ──────────────────────────────────────────────

@test "arrow: Right + Enter commits option 1 (same as Down)" {
    keystrokes "${KEY_RIGHT}${KEY_ENTER}"
    run zsh_run "$three_opts_prologue"
    [[ "$status" -eq 0 ]]
    [[ "$output" == *"BUFFER=cat"* ]]
}

# ─── Left arrow (same as Up) ─────────────────────────────────────────────────

@test "arrow: Left from idx=0 moves to option 2 (same as Up)" {
    keystrokes "${KEY_LEFT}${KEY_ENTER}"
    run zsh_run "$three_opts_prologue"
    [[ "$status" -eq 0 ]]
    [[ "$output" == *"BUFFER=cd"* ]]
}

# ─── Lone Esc cancels ────────────────────────────────────────────────────────

@test "arrow: lone Esc (no CSI) cancels the picker" {
    # Send two Esc bytes with nothing between them.  The first Esc triggers the
    # timeout peek; the second arrives as _zio_esc_next but is not '[', so the
    # handler treats the first as a lone Esc and cancels.
    keystrokes $'\e\e'
    run zsh_run "$three_opts_prologue"
    [[ "$status" -eq 0 ]]
    [[ "$output" == *"BUFFER=c"$'\n'* ]]
    [[ "$output" == *"reset-prompt"* ]]
}

# ─── Mixing arrows with digits ───────────────────────────────────────────────

@test "arrow: digit after arrow key jumps to that number (clears cycle)" {
    # Down → cycle_idx=1 (cat), then digit 3 → auto-commits option 3 (cargo).
    keystrokes "${KEY_DOWN}3"
    run zsh_run "$three_opts_prologue"
    [[ "$status" -eq 0 ]]
    [[ "$output" == *"BUFFER=cargo"* ]]
}

# ─── Mixing arrows with Tab ───────────────────────────────────────────────────

@test "arrow: Tab after Down advances one more step" {
    # Down → idx=1, Tab → idx=2 (cd), Enter commits.
    keystrokes "${KEY_DOWN}${KEY_TAB}${KEY_ENTER}"
    run zsh_run "$three_opts_prologue"
    [[ "$status" -eq 0 ]]
    [[ "$output" == *"BUFFER=cd"* ]]
}
