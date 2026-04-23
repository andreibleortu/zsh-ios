#!/usr/bin/env bats
#
# Tests for the completer-chain fallback functions:
#   _zsh_ios_worker_correct, _zsh_ios_worker_expand_alias,
#   _zsh_ios_worker_history_complete_word
#
# These are tiers 4-6 of the ? key fallback ladder:
#   Rust → ZLE complete-word → approximate → correct → expand_alias → history_complete_word
#
# Because the worker is a live zpty process (not started in the bats
# environment), we test the functions' contract when the worker is absent.

load 'helpers/test_helper'

# ─── _zsh_ios_worker_correct ─────────────────────────────────────────────────

@test "_zsh_ios_worker_correct function is defined" {
    run zsh_run '
if (( ${+functions[_zsh_ios_worker_correct]} )); then
    print "defined"
else
    print "missing"
fi
'
    [[ "$status" -eq 0 ]]
    [[ "$output" == *"defined"* ]]
}

@test "_zsh_ios_worker_correct returns non-zero when worker not ready" {
    run zsh_run '
rm -f "${_ZSH_IOS_WORKER_DIR}/ready" 2>/dev/null
_zsh_ios_worker_correct "git ch" > /dev/null 2>&1
print "rc=$?"
'
    [[ "$status" -eq 0 ]]
    [[ "$output" == *"rc=1"* ]]
}

@test "_zsh_ios_worker_correct produces no output when worker not ready" {
    run zsh_run '
rm -f "${_ZSH_IOS_WORKER_DIR}/ready" 2>/dev/null
local out
out=$(_zsh_ios_worker_correct "git ch" 2>/dev/null)
print "out=${out}"
'
    [[ "$status" -eq 0 ]]
    [[ "$output" == "out=" ]]
}

@test "_zsh_ios_worker_correct does not crash on empty buffer when worker absent" {
    run zsh_run '
rm -f "${_ZSH_IOS_WORKER_DIR}/ready" 2>/dev/null
_zsh_ios_worker_correct "" > /dev/null 2>&1
print "rc=$?"
'
    [[ "$status" -eq 0 ]]
    # Empty buffer should return non-zero (guard in dispatch helper)
    [[ "$output" == *"rc=1"* ]]
}

# ─── _zsh_ios_worker_expand_alias ────────────────────────────────────────────

@test "_zsh_ios_worker_expand_alias function is defined" {
    run zsh_run '
if (( ${+functions[_zsh_ios_worker_expand_alias]} )); then
    print "defined"
else
    print "missing"
fi
'
    [[ "$status" -eq 0 ]]
    [[ "$output" == *"defined"* ]]
}

@test "_zsh_ios_worker_expand_alias returns non-zero when worker not ready" {
    run zsh_run '
rm -f "${_ZSH_IOS_WORKER_DIR}/ready" 2>/dev/null
_zsh_ios_worker_expand_alias "ll" > /dev/null 2>&1
print "rc=$?"
'
    [[ "$status" -eq 0 ]]
    [[ "$output" == *"rc=1"* ]]
}

@test "_zsh_ios_worker_expand_alias produces no output when worker not ready" {
    run zsh_run '
rm -f "${_ZSH_IOS_WORKER_DIR}/ready" 2>/dev/null
local out
out=$(_zsh_ios_worker_expand_alias "ll" 2>/dev/null)
print "out=${out}"
'
    [[ "$status" -eq 0 ]]
    [[ "$output" == "out=" ]]
}

@test "_zsh_ios_worker_expand_alias does not crash on empty buffer when worker absent" {
    run zsh_run '
rm -f "${_ZSH_IOS_WORKER_DIR}/ready" 2>/dev/null
_zsh_ios_worker_expand_alias "" > /dev/null 2>&1
print "rc=$?"
'
    [[ "$status" -eq 0 ]]
    [[ "$output" == *"rc=1"* ]]
}

# ─── _zsh_ios_worker_history_complete_word ───────────────────────────────────

@test "_zsh_ios_worker_history_complete_word function is defined" {
    run zsh_run '
if (( ${+functions[_zsh_ios_worker_history_complete_word]} )); then
    print "defined"
else
    print "missing"
fi
'
    [[ "$status" -eq 0 ]]
    [[ "$output" == *"defined"* ]]
}

@test "_zsh_ios_worker_history_complete_word returns non-zero when worker not ready" {
    run zsh_run '
rm -f "${_ZSH_IOS_WORKER_DIR}/ready" 2>/dev/null
_zsh_ios_worker_history_complete_word "git ch" > /dev/null 2>&1
print "rc=$?"
'
    [[ "$status" -eq 0 ]]
    [[ "$output" == *"rc=1"* ]]
}

@test "_zsh_ios_worker_history_complete_word produces no output when worker not ready" {
    run zsh_run '
rm -f "${_ZSH_IOS_WORKER_DIR}/ready" 2>/dev/null
local out
out=$(_zsh_ios_worker_history_complete_word "git ch" 2>/dev/null)
print "out=${out}"
'
    [[ "$status" -eq 0 ]]
    [[ "$output" == "out=" ]]
}

@test "_zsh_ios_worker_history_complete_word does not crash on empty buffer when worker absent" {
    run zsh_run '
rm -f "${_ZSH_IOS_WORKER_DIR}/ready" 2>/dev/null
_zsh_ios_worker_history_complete_word "" > /dev/null 2>&1
print "rc=$?"
'
    [[ "$status" -eq 0 ]]
    [[ "$output" == *"rc=1"* ]]
}

# ─── Ladder integration: _zsh_ios_help does not crash with all tiers empty ───

@test "_zsh_ios_help falls through all worker tiers gracefully when worker absent" {
    export ZSH_IOS_STUB_COMPLETE_OUT="% <enter argument>"
    run zsh_run '
rm -f "${_ZSH_IOS_WORKER_DIR}/ready" 2>/dev/null
BUFFER="git ch"
CURSOR=6
_zsh_ios_help
print "ZLE=${_zle_calls[*]}"
'
    [[ "$status" -eq 0 ]]
    # Should call zle -M (either "No commands found" or the Rust output)
    [[ "$output" == *"-M"* ]]
}

# ─── Shared dispatch helper ──────────────────────────────────────────────────

@test "_zsh_ios_worker_dispatch_completion function is defined" {
    run zsh_run '
if (( ${+functions[_zsh_ios_worker_dispatch_completion]} )); then
    print "defined"
else
    print "missing"
fi
'
    [[ "$status" -eq 0 ]]
    [[ "$output" == *"defined"* ]]
}

@test "_zsh_ios_worker_dispatch_completion returns non-zero when worker not ready" {
    run zsh_run '
rm -f "${_ZSH_IOS_WORKER_DIR}/ready" 2>/dev/null
_zsh_ios_worker_dispatch_completion correct "git ch" > /dev/null 2>&1
print "rc=$?"
'
    [[ "$status" -eq 0 ]]
    [[ "$output" == *"rc=1"* ]]
}
