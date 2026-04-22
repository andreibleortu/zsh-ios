#!/usr/bin/env bats
#
# Covers `_zsh_ios_safe_eval` — the plugin's security boundary. The Rust
# binary's output is `eval`'d by the plugin, so any regression in this
# validator is a real exploit vector.

load 'helpers/test_helper'

@test "safe_eval: empty input succeeds" {
    run zsh_run '_zsh_ios_safe_eval ""; print -r -- "rc=$?"'
    [[ "$status" -eq 0 ]]
    [[ "$output" == *"rc=0"* ]]
}

@test "safe_eval: single _zio_ assignment is accepted and evaluated" {
    run zsh_run '_zsh_ios_safe_eval "_zio_word='\''git'\''"; print -r -- "rc=$? word=$_zio_word"'
    [[ "$status" -eq 0 ]]
    [[ "$output" == *"rc=0 word=git"* ]]
}

@test "safe_eval: multiple _zio_ lines all accepted" {
    local payload='_zio_word='\''git'\''
_zio_position=0
_zio_candidates=(a b c)'
    run zsh_run "_zsh_ios_safe_eval \"$payload\"
print -r -- \"rc=\$? word=\$_zio_word pos=\$_zio_position n=\${#_zio_candidates}\""
    [[ "$status" -eq 0 ]]
    [[ "$output" == *"rc=0 word=git pos=0 n=3"* ]]
}

@test "safe_eval: non-_zio line is rejected without eval" {
    # A rogue command must NOT execute. Use a side-effect (file touch) that
    # would be visible if eval ran.
    local canary="$BATS_TMPDIR/safe_eval_canary_$$"
    rm -f "$canary"
    local payload="_zio_word='git'
touch $canary"
    run zsh_run "_zsh_ios_safe_eval \"$payload\"
print -r -- \"rc=\$?\""
    [[ "$status" -eq 0 ]]
    [[ "$output" == *"rc=1"* ]]
    [[ ! -f "$canary" ]]
    rm -f "$canary"
}

@test "safe_eval: mixed valid + invalid rejects the whole payload" {
    # One bad line is enough to reject — no partial eval.
    local canary="$BATS_TMPDIR/safe_eval_canary_mixed_$$"
    rm -f "$canary"
    local payload="_zio_word='ok'
rm -rf $canary-should-never-run
_zio_position=0"
    run zsh_run "_zsh_ios_safe_eval \"$payload\"
print -r -- \"rc=\$?\"
print -r -- \"word=\${_zio_word:-UNSET}\""
    [[ "$status" -eq 0 ]]
    [[ "$output" == *"rc=1"* ]]
    # The _zio_ lines must NOT have been evaluated either (all-or-nothing).
    [[ "$output" == *"word=UNSET"* ]]
}

@test "safe_eval: function definition rejected" {
    local payload="_zio_word='ok'
_zsh_ios_pwn() { echo PWNED }"
    run zsh_run "_zsh_ios_safe_eval \"$payload\"
print -r -- \"rc=\$?\"
print -r -- \"fn=\${+functions[_zsh_ios_pwn]}\""
    [[ "$status" -eq 0 ]]
    [[ "$output" == *"rc=1"* ]]
    [[ "$output" == *"fn=0"* ]]
}

@test "safe_eval: empty lines within a payload don't trip validation" {
    local payload='_zio_word='\''git'\''

_zio_position=0'
    run zsh_run "_zsh_ios_safe_eval \"$payload\"
print -r -- \"rc=\$? word=\$_zio_word pos=\$_zio_position\""
    [[ "$status" -eq 0 ]]
    [[ "$output" == *"rc=0 word=git pos=0"* ]]
}
