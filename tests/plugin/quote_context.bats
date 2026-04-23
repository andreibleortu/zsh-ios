#!/usr/bin/env bats
#
# Tests for _zsh_ios_infer_quote and _zsh_ios_infer_param_context.
# These helpers drive the --quote / --param-context flags passed to the binary.

load 'helpers/test_helper'

# ─── _zsh_ios_infer_quote ────────────────────────────────────────────────────

@test "infer_quote: returns 'single' when buffer ends inside a single-quoted string" {
    run zsh_run '
result=$(_zsh_ios_infer_quote "echo '\''g")
print -r -- "$result"
'
    [[ "$status" -eq 0 ]]
    [[ "$output" == "single" ]]
}

@test "infer_quote: returns 'double' when buffer ends inside a double-quoted string" {
    run zsh_run '
result=$(_zsh_ios_infer_quote '\''echo "g'\'')
print -r -- "$result"
'
    [[ "$status" -eq 0 ]]
    [[ "$output" == "double" ]]
}

@test "infer_quote: returns 'none' when all quotes are balanced" {
    run zsh_run '
result=$(_zsh_ios_infer_quote "echo '\''hello'\'' world")
print -r -- "$result"
'
    [[ "$status" -eq 0 ]]
    [[ "$output" == "none" ]]
}

@test "infer_quote: returns 'none' for plain unquoted buffer" {
    run zsh_run '
result=$(_zsh_ios_infer_quote "git checkout main")
print -r -- "$result"
'
    [[ "$status" -eq 0 ]]
    [[ "$output" == "none" ]]
}

@test "infer_quote: returns 'backtick' when buffer ends inside backtick substitution" {
    run zsh_run 'result=$(_zsh_ios_infer_quote "echo \`ls"); print -r -- "$result"'
    [[ "$status" -eq 0 ]]
    [[ "$output" == "backtick" ]]
}

# ─── _zsh_ios_infer_param_context ────────────────────────────────────────────

@test "infer_param_context: returns 1 when buffer ends inside unclosed \${" {
    run zsh_run '
result=$(_zsh_ios_infer_param_context "echo \${HOM")
print -r -- "$result"
'
    [[ "$status" -eq 0 ]]
    [[ "$output" == "1" ]]
}

@test "infer_param_context: returns 0 when \${ is properly closed" {
    run zsh_run '
result=$(_zsh_ios_infer_param_context "echo ${HOME} world")
print -r -- "$result"
'
    [[ "$status" -eq 0 ]]
    [[ "$output" == "0" ]]
}

@test "infer_param_context: returns 0 for plain buffer" {
    run zsh_run '
result=$(_zsh_ios_infer_param_context "git commit -m msg")
print -r -- "$result"
'
    [[ "$status" -eq 0 ]]
    [[ "$output" == "0" ]]
}
