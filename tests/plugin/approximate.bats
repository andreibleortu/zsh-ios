#!/usr/bin/env bats
#
# Tests for the _approximate worker fallback path.
#
# _zsh_ios_worker_approximate is the third tier of the ? key: it runs
# _approximate inside the ZLE worker when both the Rust binary and the
# complete-word worker path return empty.  Because the worker is a live
# zpty process (not started in the bats environment), we test the
# function's contract when the worker is not ready.

load 'helpers/test_helper'

@test "_zsh_ios_worker_approximate function is defined" {
    run zsh_run '
if (( ${+functions[_zsh_ios_worker_approximate]} )); then
    print "defined"
else
    print "missing"
fi
'
    [[ "$status" -eq 0 ]]
    [[ "$output" == *"defined"* ]]
}

@test "_zsh_ios_worker_approximate returns non-zero when worker not ready" {
    run zsh_run '
# Ensure the worker is not ready by removing the ready file
rm -f "${_ZSH_IOS_WORKER_DIR}/ready" 2>/dev/null
_zsh_ios_worker_approximate "git ch" > /dev/null 2>&1
print "rc=$?"
'
    [[ "$status" -eq 0 ]]
    [[ "$output" == *"rc=1"* ]]
}

@test "_zsh_ios_worker_approximate produces no output when worker not ready" {
    run zsh_run '
rm -f "${_ZSH_IOS_WORKER_DIR}/ready" 2>/dev/null
local out
out=$(_zsh_ios_worker_approximate "git ch" 2>/dev/null)
print "out=${out}"
'
    [[ "$status" -eq 0 ]]
    [[ "$output" == *"out="* ]]
    # output line should have nothing after "out="
    [[ "$output" != *"out=?"* ]]
}

@test "_zsh_ios_help does not crash when approximate fallback finds nothing" {
    # Stub the Rust binary to signal "generic output" via exit code 4
    # (triggers the ZLE worker fallback path).
    export ZSH_IOS_STUB_COMPLETE_OUT="% Expects: <argument>"
    export ZSH_IOS_STUB_COMPLETE_EXIT=4
    run zsh_run '
# Worker not ready — both worker paths return empty. Help should not crash.
rm -f "${_ZSH_IOS_WORKER_DIR}/ready" 2>/dev/null
BUFFER="git ch"
CURSOR=6
_zsh_ios_help
print "ZLE=${_zle_calls[*]}"
'
    [[ "$status" -eq 0 ]]
    # Should have called zle -M with "No commands found" (neither worker ready)
    [[ "$output" == *"-M"* ]]
}
