#!/usr/bin/env bats
#
# Tests for the Esc-to-dismiss ghost text + passthrough mode feature.
#
# When ghost text is visible (POSTDISPLAY non-empty) and the user presses Esc:
#   1. POSTDISPLAY is cleared.
#   2. _zsh_ios_esc_passthrough is set to 1.
#   3. The next Enter runs BUFFER as-is via zle .accept-line (no binary call).
#
# When the buffer is edited after Esc, passthrough mode is cancelled and ghost
# text resumes normally.
#
# When there is no ghost text and Esc is pressed, passthrough mode is NOT set.

load 'helpers/test_helper'

# ─── _zsh_ios_escape_widget ─────────────────────────────────────────────────

@test "Esc with ghost text: clears POSTDISPLAY and sets passthrough flag" {
    export ZSH_IOS_STUB_RESOLVE_OUT="git branch"
    export ZSH_IOS_STUB_RESOLVE_EXIT=0
    run zsh_run '
typeset -g POSTDISPLAY="  git branch"
typeset -ga region_highlight=("5 17 fg=240")
typeset -g _zsh_ios_ghost_last_highlight="5 17 fg=240"
BUFFER="gi br"
CURSOR=${#BUFFER}
_zsh_ios_escape_widget
print "PD=${POSTDISPLAY}"
print "PT=${_zsh_ios_esc_passthrough}"
print "LEB=${_zsh_ios_last_esc_buffer}"
'
    [[ "$status" -eq 0 ]]
    [[ "$output" == *"PD="* ]]
    # POSTDISPLAY must be empty (nothing after "PD=")
    [[ "$output" != *"PD= "* ]]
    [[ "$output" == *"PT=1"* ]]
    [[ "$output" == *"LEB=gi br"* ]]
}

@test "Esc with ghost text: removes ghost region_highlight entry" {
    export ZSH_IOS_STUB_RESOLVE_OUT="git branch"
    export ZSH_IOS_STUB_RESOLVE_EXIT=0
    run zsh_run '
typeset -g POSTDISPLAY="  git branch"
typeset -ga region_highlight=("5 17 fg=240" "0 3 bold")
typeset -g _zsh_ios_ghost_last_highlight="5 17 fg=240"
BUFFER="gi br"
CURSOR=${#BUFFER}
_zsh_ios_escape_widget
print "HL_COUNT=${#region_highlight}"
# The ghost entry should be gone; the bold entry should remain.
print "HL=${region_highlight[*]}"
'
    [[ "$status" -eq 0 ]]
    [[ "$output" == *"HL=0 3 bold"* ]]
    # Ghost entry "5 17 fg=240" should not appear.
    [[ "$output" != *"5 17 fg=240"* ]]
}

@test "Esc without ghost text: passthrough flag is NOT set" {
    run zsh_run '
typeset -g POSTDISPLAY=""
typeset -ga region_highlight=()
BUFFER="ls -l"
CURSOR=${#BUFFER}
_zsh_ios_escape_widget
print "PT=${_zsh_ios_esc_passthrough}"
'
    [[ "$status" -eq 0 ]]
    [[ "$output" == *"PT=0"* ]]
}

@test "Esc without ghost text: is a no-op (no ZLE calls)" {
    run zsh_run '
typeset -g POSTDISPLAY=""
typeset -ga region_highlight=()
typeset -g KEYMAP="main"
BUFFER="ls -l"
CURSOR=${#BUFFER}
_zsh_ios_escape_widget
print "ZLE=${_zle_calls[*]}"
'
    [[ "$status" -eq 0 ]]
    [[ "$output" != *"send-break"* ]]
    [[ "$output" != *"vi-cmd-mode"* ]]
}

# ─── Accept-line passthrough mode ───────────────────────────────────────────

@test "Enter after Esc: calls zle .accept-line and skips binary" {
    export ZSH_IOS_STUB_LOG="$BATS_TMPDIR/stub_log_esc_enter_$$"
    export ZSH_IOS_STUB_RESOLVE_OUT="git branch"
    export ZSH_IOS_STUB_RESOLVE_EXIT=0
    run zsh_run '
typeset -g _zsh_ios_esc_passthrough=1
typeset -g _zsh_ios_last_esc_buffer="gi br"
typeset -g POSTDISPLAY=""
typeset -ga region_highlight=()
BUFFER="gi br"
CURSOR=${#BUFFER}
_zsh_ios_accept_line
print "BUFFER=$BUFFER"
print "ZLE=${_zle_calls[*]}"
print "PT=${_zsh_ios_esc_passthrough}"
'
    [[ "$status" -eq 0 ]]
    # Buffer must not be modified (no resolution).
    [[ "$output" == *"BUFFER=gi br"* ]]
    # Must call the native .accept-line (dot form).
    [[ "$output" == *".accept-line"* ]]
    # Must NOT call the wrapped accept-line (which would try to resolve).
    # Passthrough flag must be cleared after use.
    [[ "$output" == *"PT=0"* ]]
    # Binary should not have been called for resolve.
    [[ ! -s "$ZSH_IOS_STUB_LOG" ]]
    rm -f "$ZSH_IOS_STUB_LOG"
}

@test "Enter after Esc: BUFFER is unchanged (abbreviated form runs as-is)" {
    export ZSH_IOS_STUB_LOG="$BATS_TMPDIR/stub_log_esc_literal_$$"
    run zsh_run '
typeset -g _zsh_ios_esc_passthrough=1
typeset -g _zsh_ios_last_esc_buffer="gi st"
BUFFER="gi st"
CURSOR=${#BUFFER}
_zsh_ios_accept_line
print "BUFFER=$BUFFER"
'
    [[ "$status" -eq 0 ]]
    [[ "$output" == *"BUFFER=gi st"* ]]
    rm -f "$ZSH_IOS_STUB_LOG"
}

# ─── Ghost preview widget passthrough guard ─────────────────────────────────

@test "ghost preview: passthrough mode suppresses new ghost text (same buffer)" {
    export ZSH_IOS_STUB_RESOLVE_OUT="git branch"
    export ZSH_IOS_STUB_RESOLVE_EXIT=0
    export ZSH_IOS_STUB_LOG="$(mktemp)"
    run zsh_run '
typeset -g POSTDISPLAY=""
typeset -ga region_highlight=()
typeset -g _zsh_ios_esc_passthrough=1
typeset -g _zsh_ios_last_esc_buffer="gi br"
BUFFER="gi br"
CURSOR=${#BUFFER}
_zsh_ios_ghost_preview_widget
print "PD=${POSTDISPLAY}"
print "PT=${_zsh_ios_esc_passthrough}"
'
    [[ "$status" -eq 0 ]]
    [[ "$output" == *"PD="* ]]
    [[ "$output" != *"PD= "* ]]
    # Passthrough flag remains set (buffer unchanged).
    [[ "$output" == *"PT=1"* ]]
    # Binary should NOT have been called for resolve.
    local resolve_calls=0
    if [[ -s "$ZSH_IOS_STUB_LOG" ]]; then
        resolve_calls=$(grep -c '^resolve|' "$ZSH_IOS_STUB_LOG" 2>/dev/null; true)
    fi
    [[ "$resolve_calls" -eq 0 ]]
    rm -f "$ZSH_IOS_STUB_LOG"
    unset ZSH_IOS_STUB_LOG
}

@test "ghost preview: buffer edit after Esc cancels passthrough and resumes ghost" {
    export ZSH_IOS_STUB_RESOLVE_OUT="git branch --all"
    export ZSH_IOS_STUB_RESOLVE_EXIT=0
    run zsh_run '
typeset -g POSTDISPLAY=""
typeset -ga region_highlight=()
typeset -g _zsh_ios_esc_passthrough=1
typeset -g _zsh_ios_last_esc_buffer="gi br"
# User typed more — buffer differs from when Esc was pressed.
BUFFER="gi br --"
CURSOR=${#BUFFER}
_zsh_ios_ghost_preview_widget
print "PD=${POSTDISPLAY}"
print "PT=${_zsh_ios_esc_passthrough}"
'
    [[ "$status" -eq 0 ]]
    # Passthrough must be cancelled.
    [[ "$output" == *"PT=0"* ]]
    # Ghost should have resumed: POSTDISPLAY set to the new resolved form.
    [[ "$output" == *"PD=  git branch --all"* ]]
}

# ─── line-init clears passthrough state ─────────────────────────────────────

@test "line-init: resets passthrough flag and last-esc-buffer for new line" {
    run zsh_run '
typeset -g _zsh_ios_esc_passthrough=1
typeset -g _zsh_ios_last_esc_buffer="some old buffer"
_zsh_ios_ghost_line_init
print "PT=${_zsh_ios_esc_passthrough}"
print "LEB=${_zsh_ios_last_esc_buffer}"
'
    [[ "$status" -eq 0 ]]
    [[ "$output" == *"PT=0"* ]]
    # LEB should be empty after init.
    grep -qx 'LEB=' <<<"$output"
}
