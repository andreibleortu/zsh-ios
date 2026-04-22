# Common bats helper for plugin tests.
#
# Each test:
#   1. Calls keystrokes '<raw chars>' to seed the picker input (optional).
#   2. Calls zsh_run '<zsh snippet>' to run code against the sourced plugin.
#
# Special keys:
#     $KEY_ENTER $KEY_TAB $KEY_BACKSPACE
#
# Plugin globals you can read/write from the snippet: BUFFER, CURSOR,
# _zle_calls (array recording every `zle <args>` call).
#
# Fake-binary knobs (export before zsh_run):
#     ZSH_IOS_STUB_RESOLVE_OUT, ZSH_IOS_STUB_RESOLVE_EXIT
#     ZSH_IOS_STUB_COMPLETE_OUT, ZSH_IOS_STUB_STATUS_OUT
#     ZSH_IOS_STUB_LOG (filepath — each call appends "<sub>|<args>")

RUN_IN_ZSH="${BATS_TEST_DIRNAME}/helpers/run-in-zsh"

# Literal control chars for building keystroke sequences.
KEY_ENTER=$'\n'
KEY_TAB=$'\t'
KEY_BACKSPACE=$'\x7f'

setup() {
    TMP_SNIPPET=$(mktemp --suffix=.zsh)
    TMP_INPUT=$(mktemp)
    export TMP_INPUT
}

teardown() {
    [[ -n "$TMP_SNIPPET" ]] && rm -f "$TMP_SNIPPET"
    [[ -n "$TMP_INPUT" ]] && rm -f "$TMP_INPUT"
    unset ZSH_IOS_STUB_RESOLVE_OUT ZSH_IOS_STUB_RESOLVE_EXIT
    unset ZSH_IOS_STUB_COMPLETE_OUT ZSH_IOS_STUB_STATUS_OUT ZSH_IOS_STUB_LOG
}

# Seed the picker's input fd with the given raw bytes.
keystrokes() {
    printf '%s' "$1" > "$TMP_INPUT"
}

# Run a zsh snippet with the plugin sourced.
zsh_run() {
    printf '%s' "$1" > "$TMP_SNIPPET"
    "$RUN_IN_ZSH" "$TMP_SNIPPET"
}
