#!/usr/bin/env bats
#
# Tests for the _zsh_ios_ghost_preview_widget function.
#
# The widget is hooked into line-pre-redraw but can also be called directly.
# In the test harness, POSTDISPLAY and region_highlight are plain Zsh globals
# (no ZLE magic needed). The stub binary is controlled via ZSH_IOS_STUB_*.

load 'helpers/test_helper'

@test "_zsh_ios_ghost_preview_widget function is defined" {
    run zsh_run '
if (( ${+functions[_zsh_ios_ghost_preview_widget]} )); then
    print "defined"
else
    print "missing"
fi
'
    [[ "$status" -eq 0 ]]
    [[ "$output" == *"defined"* ]]
}

@test "empty BUFFER leaves POSTDISPLAY empty" {
    run zsh_run '
typeset -g POSTDISPLAY=""
typeset -ga region_highlight=()
BUFFER=""
CURSOR=0
_zsh_ios_ghost_preview_widget
print "PD=${POSTDISPLAY}"
'
    [[ "$status" -eq 0 ]]
    [[ "$output" == *"PD="* ]]
    # POSTDISPLAY must be empty (nothing after "PD=")
    [[ "$output" != *"PD= "* ]]
}

@test "BUFFER starting with ! leaves POSTDISPLAY empty" {
    export ZSH_IOS_STUB_RESOLVE_OUT="echo hello"
    export ZSH_IOS_STUB_RESOLVE_EXIT=0
    run zsh_run '
typeset -g POSTDISPLAY=""
typeset -ga region_highlight=()
BUFFER="!echo hi"
CURSOR=${#BUFFER}
_zsh_ios_ghost_preview_widget
print "PD=${POSTDISPLAY}"
'
    [[ "$status" -eq 0 ]]
    [[ "$output" == *"PD="* ]]
    [[ "$output" != *"PD= "* ]]
}

@test "BUFFER equal to resolved leaves POSTDISPLAY empty" {
    # Stub: resolve returns the same text as BUFFER (exit 0 but unchanged)
    export ZSH_IOS_STUB_RESOLVE_OUT="echo hello"
    export ZSH_IOS_STUB_RESOLVE_EXIT=0
    run zsh_run '
typeset -g POSTDISPLAY=""
typeset -ga region_highlight=()
BUFFER="echo hello"
CURSOR=${#BUFFER}
_zsh_ios_ghost_preview_widget
print "PD=${POSTDISPLAY}"
'
    [[ "$status" -eq 0 ]]
    [[ "$output" == *"PD="* ]]
    [[ "$output" != *"PD= "* ]]
}

@test "abbreviated BUFFER sets POSTDISPLAY to prefix plus resolved" {
    # Stub: resolve "gi br" -> "git branch" (exit 0, different from BUFFER)
    export ZSH_IOS_STUB_RESOLVE_OUT="git branch"
    export ZSH_IOS_STUB_RESOLVE_EXIT=0
    run zsh_run '
typeset -g POSTDISPLAY=""
typeset -ga region_highlight=()
BUFFER="gi br"
CURSOR=${#BUFFER}
_zsh_ios_ghost_preview_widget
print "PD=${POSTDISPLAY}"
'
    [[ "$status" -eq 0 ]]
    # POSTDISPLAY should be "  git branch" (two-space prefix + resolved)
    [[ "$output" == *"PD=  git branch"* ]]
}

@test "region_highlight gets P0 entry when ghost is shown" {
    export ZSH_IOS_STUB_RESOLVE_OUT="git branch"
    export ZSH_IOS_STUB_RESOLVE_EXIT=0
    run zsh_run '
typeset -g POSTDISPLAY=""
typeset -ga region_highlight=()
BUFFER="gi br"
CURSOR=${#BUFFER}
_zsh_ios_ghost_preview_widget
print "HL=${region_highlight[*]}"
'
    [[ "$status" -eq 0 ]]
    [[ "$output" == *"HL=P0"* ]]
}

@test "passthrough exit code (2) leaves POSTDISPLAY empty" {
    export ZSH_IOS_STUB_RESOLVE_OUT=""
    export ZSH_IOS_STUB_RESOLVE_EXIT=2
    run zsh_run '
typeset -g POSTDISPLAY=""
typeset -ga region_highlight=()
BUFFER="ls -l"
CURSOR=${#BUFFER}
_zsh_ios_ghost_preview_widget
print "PD=${POSTDISPLAY}"
'
    [[ "$status" -eq 0 ]]
    [[ "$output" == *"PD="* ]]
    [[ "$output" != *"PD= "* ]]
}

@test "ambiguous exit code (1) leaves POSTDISPLAY empty" {
    export ZSH_IOS_STUB_RESOLVE_OUT="_zio_word=g"
    export ZSH_IOS_STUB_RESOLVE_EXIT=1
    run zsh_run '
typeset -g POSTDISPLAY=""
typeset -ga region_highlight=()
BUFFER="g"
CURSOR=1
_zsh_ios_ghost_preview_widget
print "PD=${POSTDISPLAY}"
'
    [[ "$status" -eq 0 ]]
    [[ "$output" == *"PD="* ]]
    [[ "$output" != *"PD= "* ]]
}

@test "repeated call with same BUFFER uses cache and skips binary" {
    export ZSH_IOS_STUB_RESOLVE_OUT="git branch"
    export ZSH_IOS_STUB_RESOLVE_EXIT=0
    export ZSH_IOS_STUB_LOG="$(mktemp)"
    run zsh_run '
typeset -g POSTDISPLAY=""
typeset -ga region_highlight=()
BUFFER="gi br"
CURSOR=${#BUFFER}
# First call: resolves via binary
_zsh_ios_ghost_preview_widget
local first_pd="$POSTDISPLAY"
# Second call: same BUFFER, should hit cache
region_highlight=()
POSTDISPLAY=""
_zsh_ios_ghost_preview_widget
print "first=${first_pd}"
print "second=${POSTDISPLAY}"
'
    [[ "$status" -eq 0 ]]
    [[ "$output" == *"first=  git branch"* ]]
    [[ "$output" == *"second=  git branch"* ]]
    # Count how many times resolve was called in the stub log.
    local resolve_calls
    resolve_calls=$(grep -c '^resolve|' "$ZSH_IOS_STUB_LOG" 2>/dev/null || echo 0)
    # Only one binary call expected (second call hits cache).
    [[ "$resolve_calls" -eq 1 ]]
    rm -f "$ZSH_IOS_STUB_LOG"
    unset ZSH_IOS_STUB_LOG
}

@test "ghost_disabled=1 leaves POSTDISPLAY empty" {
    export ZSH_IOS_STUB_RESOLVE_OUT="git branch"
    export ZSH_IOS_STUB_RESOLVE_EXIT=0
    run zsh_run '
typeset -g POSTDISPLAY=""
typeset -ga region_highlight=()
_zsh_ios_ghost_disabled=1
BUFFER="gi br"
CURSOR=${#BUFFER}
_zsh_ios_ghost_preview_widget
print "PD=${POSTDISPLAY}"
'
    [[ "$status" -eq 0 ]]
    [[ "$output" == *"PD="* ]]
    [[ "$output" != *"PD= "* ]]
}

@test "custom ghost_prefix is used in POSTDISPLAY" {
    export ZSH_IOS_STUB_RESOLVE_OUT="git branch"
    export ZSH_IOS_STUB_RESOLVE_EXIT=0
    run zsh_run '
typeset -g POSTDISPLAY=""
typeset -ga region_highlight=()
typeset -g _zsh_ios_ghost_prefix=" -> "
BUFFER="gi br"
CURSOR=${#BUFFER}
_zsh_ios_ghost_preview_widget
print "PD=${POSTDISPLAY}"
'
    [[ "$status" -eq 0 ]]
    [[ "$output" == *"PD= -> git branch"* ]]
}
