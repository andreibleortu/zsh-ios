#!/usr/bin/env zsh
# zsh-ios: Cisco IOS-style command abbreviation engine for Zsh
# vim: set ft=zsh:

# When running inside the ZLE completion worker, skip the entire plugin.
# The worker only needs the base completion system (compinit from .zshrc).
[[ -n "$_ZSH_IOS_IS_WORKER" ]] && return 0

# --- Configuration ---
ZSH_IOS_BIN="${ZSH_IOS_BIN:-zsh-ios}"
ZSH_IOS_CONFIG_DIR="${XDG_CONFIG_HOME:-$HOME/.config}/zsh-ios"

# Tab-preview state. First Tab on an ambiguous buffer does LCP-extend +
# multi-column hint and stores the post-LCP buffer here. A second Tab on the
# unchanged buffer sees the match and enters the picker instead.
typeset -g _zsh_ios_last_tab_buffer=""

# One-shot ingest guard: set to 1 after the first worker-state ingest so
# subsequent precmd invocations don't re-run it.
typeset -g _zsh_ios_ingested=0

# Ghost preview state.
typeset -g _zsh_ios_ghost_disabled=0
typeset -g _zsh_ios_ghost_style="fg=240"
typeset -g _zsh_ios_ghost_prefix="  "
typeset -g _zsh_ios_ghost_last_buffer=""
typeset -g _zsh_ios_ghost_last_postdisplay=""
typeset -g _zsh_ios_ghost_last_highlight=""
# Set non-empty while a widget is mutating BUFFER (e.g. accept-line
# substituting the resolved form). Any redraw that fires during that
# window must NOT add a ghost — otherwise an entry set for the
# pre-mutation BUFFER length ends up styling part of the expanded
# BUFFER in the scrollback after the command runs.
typeset -g _zsh_ios_ghost_suspended=0

# Picker keystroke source. Defaults to /dev/tty because ZLE widgets don't
# have stdin attached to the terminal. Tests set $_ZSH_IOS_TEST_INPUT_FD to
# an already-open file descriptor containing the simulated keystrokes.
_zsh_ios_read_picker_key() {
    if [[ -n "$_ZSH_IOS_TEST_INPUT_FD" ]]; then
        # -u must come BEFORE the variable name; otherwise zsh consumes the
        # var name as the first positional and loses the -u fd binding.
        read -r -k 1 -u "$_ZSH_IOS_TEST_INPUT_FD" "$1"
    else
        read -r -k 1 "$1" </dev/tty
    fi
}

# Picker keystroke source with a timeout (for multi-byte escape sequences).
# _ms is the timeout in milliseconds; non-zero return = timed out / no char.
_zsh_ios_read_picker_key_timeout() {
    local _var="$1" _ms="$2"
    local _secs=$(( _ms / 1000.0 ))
    if [[ -n "$_ZSH_IOS_TEST_INPUT_FD" ]]; then
        read -r -k 1 -t "$_secs" -u "$_ZSH_IOS_TEST_INPUT_FD" "$_var"
    else
        read -r -k 1 -t "$_secs" "$_var" </dev/tty
    fi
}

# --- Guard: check if binary exists ---
if ! command -v "$ZSH_IOS_BIN" &>/dev/null; then
    echo "zsh-ios: binary not found in PATH. Run install.sh or cargo install --path ." >&2
    return 1
fi

# --- Background tree build on first load ---
_zsh_ios_build_if_stale() {
    # One status call; parse both the tree path and the stale threshold from
    # it so the binary stays the single source of truth (user can override via
    # $config_dir/config.yaml → stale_threshold_seconds).
    local status_out
    status_out=$("$ZSH_IOS_BIN" status 2>/dev/null)
    [[ -z "$status_out" ]] && return

    local tree_file threshold
    tree_file=$(print -r -- "$status_out" | grep 'Tree file:' | sed 's/.*Tree file:  *//')
    threshold=$(print -r -- "$status_out" | grep 'Stale threshold:' | sed -E 's/.*Stale threshold:  *([0-9]+).*/\1/')
    [[ -z "$tree_file" ]] && return
    [[ "$threshold" =~ '^[0-9]+$' ]] || threshold=3600

    # Parse new config-driven status lines.
    local worker_disabled picker_prefix worker_timeout
    worker_disabled=$(print -r -- "$status_out" | grep 'Worker:' | grep -q 'disabled' && echo 1 || echo 0)
    picker_prefix=$(print -r -- "$status_out" | grep 'Picker prefix:' | sed -E 's/.*Picker prefix:[[:space:]]+(.*)/\1/')
    worker_timeout=$(print -r -- "$status_out" | grep 'Worker timeout:' | sed -E 's/.*Worker timeout:[[:space:]]+([0-9]+)ms.*/\1/')
    export _zsh_ios_worker_disabled="$worker_disabled"
    [[ -n "$picker_prefix" ]] && typeset -g _zsh_ios_picker_prefix="$picker_prefix"
    [[ "$worker_timeout" =~ '^[0-9]+$' ]] && ZSH_IOS_WORKER_TIMEOUT_MS="$worker_timeout"

    # Ghost preview config.
    local ghost_preview ghost_style ghost_prefix_quoted
    ghost_preview=$(print -r -- "$status_out" | grep 'Ghost preview:')
    if [[ "$ghost_preview" == *"disabled"* ]]; then
        _zsh_ios_ghost_disabled=1
    else
        _zsh_ios_ghost_disabled=0
    fi
    ghost_style=$(print -r -- "$status_out" | grep 'Ghost style:' | sed -E 's/.*Ghost style:[[:space:]]+(.*)/\1/')
    [[ -n "$ghost_style" ]] && typeset -g _zsh_ios_ghost_style="$ghost_style"
    if print -r -- "$status_out" | grep -q 'Ghost prefix:'; then
        ghost_prefix_quoted=$(print -r -- "$status_out" | grep 'Ghost prefix:' | sed -E 's/.*Ghost prefix:[[:space:]]+"(.*)"/\1/')
        typeset -g _zsh_ios_ghost_prefix="$ghost_prefix_quoted"
    fi

    local rebuild=0
    if [[ ! -f "$tree_file" ]]; then
        rebuild=1
    else
        local now=$(date +%s)
        # Cross-platform mtime: macOS uses stat -f, Linux uses stat -c
        local mtime
        if [[ "$(uname -s)" == "Darwin" ]]; then
            mtime=$(stat -f %m "$tree_file" 2>/dev/null || echo 0)
        else
            mtime=$(stat -c %Y "$tree_file" 2>/dev/null || echo 0)
        fi
        if (( now - mtime > threshold )); then
            rebuild=1
        fi
    fi

    if (( rebuild )); then
        (alias | "$ZSH_IOS_BIN" build --aliases-stdin &>/dev/null &)
    fi
}

_zsh_ios_build_if_stale

# --- Learn commands only after successful execution ---
_zsh_ios_preexec() {
    _zsh_ios_pending_cmd="$1"
    _zsh_ios_pending_cwd="$PWD"
}

# Capture exit code before any other precmd hook can modify it.
#
# zsh invokes the `precmd` magic function BEFORE the precmd_functions array,
# so wrapping `precmd` catches $? even if third-party plugins prepend their
# own entry to precmd_functions after us. We preserve any user-defined
# `precmd` by chaining through a renamed copy.
if (( ! ${+functions[_zsh_ios_orig_precmd]} )); then
    if (( ${+functions[precmd]} )); then
        functions[_zsh_ios_orig_precmd]="${functions[precmd]}"
    else
        _zsh_ios_orig_precmd() { :; }
    fi
fi

precmd() {
    _zsh_ios_retval=$?
    _zsh_ios_orig_precmd "$@"
}

_zsh_ios_precmd() {
    local ec=${_zsh_ios_retval:-0}
    if [[ -n "$_zsh_ios_pending_cmd" ]]; then
        ("$ZSH_IOS_BIN" learn --exit-code "$ec" --cwd "$_zsh_ios_pending_cwd" -- "$_zsh_ios_pending_cmd" &>/dev/null &)
        if [[ $ec -eq 0 ]]; then
            # Stash the first word of the command as the sibling-context hint
            # for the next resolution. The engine reads ZSH_IOS_LAST_CMD.
            export _ZSH_IOS_LAST_CMD="${_zsh_ios_pending_cmd%% *}"
        fi
    fi
    unset _zsh_ios_pending_cmd
    unset _zsh_ios_pending_cwd
    unset _zsh_ios_last_pin
    if (( ! _zsh_ios_ingested )) && _zsh_ios_worker_is_ready; then
        _zsh_ios_ingested=1
        # `&|` = start in background AND disown, so zsh doesn't print
        # the `[N] pid` and `[N] + exit …` job-control notifications
        # that a plain `&` would emit on every new shell.
        ( _zsh_ios_ingest_worker_state; _zsh_ios_harvest_regex_args ) &>/dev/null &|
    fi
}

autoload -Uz add-zsh-hook
add-zsh-hook preexec _zsh_ios_preexec
add-zsh-hook precmd _zsh_ios_precmd

# --- Safe eval: validates output contains only expected _zio_ assignments ---
_zsh_ios_safe_eval() {
    local line
    while IFS= read -r line; do
        # Allow empty lines and lines starting with expected variable names
        [[ -z "$line" || "$line" == _zio_* ]] || return 1
    done <<< "$1"
    eval "$1"
}

# --- Check if disabled (file-based toggle via `zsh-ios toggle`) ---
_zsh_ios_is_disabled() {
    [[ -f "$ZSH_IOS_CONFIG_DIR/disabled" ]]
}

# --- Infer shell context from the buffer ---
# Outputs one of: math, condition, redirection, argument
_zsh_ios_infer_context() {
    local buf="$1"
    if [[ "$buf" == *'(('* && "$buf" != *'))'* ]]; then
        echo math
    elif [[ "$buf" == *'[['* && "$buf" != *']]'* ]]; then
        echo condition
    elif [[ "${buf##*[[:space:]]}" =~ '^[12&]?[>]{1,2}' ]]; then
        echo redirection
    else
        echo argument
    fi
}

# --- Infer quote state from the buffer ---
# Walks forward tracking unclosed quotes; emits the deepest unclosed quote
# type at end of buffer, or "none" when all quotes are balanced.
# Outputs one of: none, single, double, backtick
_zsh_ios_infer_quote() {
    local buf="$1"
    local single=0 double=0 backtick=0 escaped=0
    local i=0 c
    while (( i < ${#buf} )); do
        c="${buf:$i:1}"
        if (( escaped )); then
            escaped=0
            (( i++ ))
            continue
        fi
        case "$c" in
            '\\') escaped=1 ;;
            "'")
                if (( single == 0 && double == 0 && backtick == 0 )); then
                    single=1
                elif (( single == 1 )); then
                    single=0
                fi
                ;;
            '"')
                if (( single == 0 )); then
                    (( double ^= 1 ))
                fi
                ;;
            '`')
                if (( single == 0 && double == 0 )); then
                    (( backtick ^= 1 ))
                fi
                ;;
        esac
        (( i++ ))
    done
    if (( single )); then
        echo single
    elif (( double )); then
        echo double
    elif (( backtick )); then
        echo backtick
    else
        echo none
    fi
}

# --- Infer whether the cursor is inside an unclosed ${ expansion ---
# Returns 1 (true) when the buffer ends inside `${…` without a closing `}`.
# Returns 0 otherwise.
_zsh_ios_infer_param_context() {
    local buf="$1"
    local i=0 depth=0
    while (( i < ${#buf} - 1 )); do
        if [[ "${buf:$i:2}" == '${' ]]; then
            (( depth++ ))
            (( i += 2 ))
            continue
        fi
        if [[ "${buf:$i:1}" == '}' ]] && (( depth > 0 )); then
            (( depth-- ))
        fi
        (( i++ ))
    done
    (( depth > 0 )) && echo 1 || echo 0
}

# --- Build the extra quote/param-context args for the binary ---
# Sets _zsh_ios_extra_args (array) based on the buffer prefix.
# Usage: _zsh_ios_quote_args "$prefix"; then use "${_zsh_ios_extra_args[@]}"
_zsh_ios_quote_args() {
    local _prefix="$1"
    _zsh_ios_extra_args=()
    local _quote
    _quote=$(_zsh_ios_infer_quote "$_prefix")
    [[ "$_quote" != "none" ]] && _zsh_ios_extra_args+=(--quote "$_quote")
    local _param_ctx
    _param_ctx=$(_zsh_ios_infer_param_context "$_prefix")
    [[ "$_param_ctx" == "1" ]] && _zsh_ios_extra_args+=(--param-context)
}

# --- ZLE Widget: Enter key (resolve + execute) ---
_zsh_ios_accept_line() {
    # Suspend the ghost widget for the duration of this accept-line;
    # any line-pre-redraw hook that fires during BUFFER mutation /
    # finalization must leave POSTDISPLAY + region_highlight alone.
    _zsh_ios_ghost_suspended=1
    POSTDISPLAY=""
    # Drop both the remembered entry AND any stray highlight that
    # carries our style (covers the case where a mid-widget redraw
    # added a new entry before we got here).
    region_highlight=("${(@)region_highlight:#* * $_zsh_ios_ghost_style}")
    _zsh_ios_ghost_last_highlight=""
    _zsh_ios_ghost_last_buffer=""

    if _zsh_ios_is_disabled || [[ -z "${BUFFER// /}" ]]; then
        zle accept-line
        return
    fi

    if [[ "${BUFFER// /}" == "unpin" && -n "$_zsh_ios_last_pin" ]]; then
        zle -I
        "$ZSH_IOS_BIN" unpin "$_zsh_ios_last_pin" 2>/dev/null
        echo "  Unpinned: \"$_zsh_ios_last_pin\""
        unset _zsh_ios_last_pin
        BUFFER=""
        zle reset-prompt
        return
    fi

    if [[ "$BUFFER" == \#* ]]; then
        zle accept-line
        return
    fi

    # Leading `!` bypass: anything starting with ! is run as-is. Lets zsh's
    # history expansion (!!, !$, !string) and explicit "run the literal
    # command" usage pass through without zsh-ios touching the buffer.
    if [[ "$BUFFER" == \!* ]]; then
        zle accept-line
        return
    fi

    # Multi-line paste: resolve each line individually
    if [[ "$BUFFER" == *$'\n'* ]]; then
        local -a lines result
        lines=("${(@f)BUFFER}")
        for line in "${lines[@]}"; do
            if [[ -n "${line// /}" ]]; then
                local out
                out=$("$ZSH_IOS_BIN" resolve -- "$line" 2>/dev/null)
                if (( $? == 0 )); then
                    result+=("$out")
                else
                    result+=("$line")
                fi
            else
                result+=("$line")
            fi
        done
        BUFFER="${(pj:\n:)result}"
        zle accept-line
        return
    fi

    local output exit_code context
    context=$(_zsh_ios_infer_context "$BUFFER")
    local -a _zsh_ios_extra_args
    _zsh_ios_quote_args "$BUFFER"
    output=$("$ZSH_IOS_BIN" resolve --context "$context" "${_zsh_ios_extra_args[@]}" -- "$BUFFER" 2>/dev/null)
    exit_code=$?

    case $exit_code in
        0)
            BUFFER="$output"
            zle accept-line
            ;;
        1)
            _zsh_ios_handle_ambiguity "$output" accept
            ;;
        3)
            _zsh_ios_handle_path_ambiguity "$output" accept
            ;;
        *)
            zle accept-line
            ;;
    esac
}

# --- ZLE Widget: Tab key (resolve + expand, no execute) ---
_zsh_ios_expand_or_complete() {
    if _zsh_ios_is_disabled || [[ -z "${BUFFER// /}" ]]; then
        zle expand-or-complete
        return
    fi

    # Leading `!` bypass: fall through to native Zsh completion, untouched.
    if [[ "$BUFFER" == \!* ]]; then
        zle expand-or-complete
        return
    fi

    local output exit_code context
    context=$(_zsh_ios_infer_context "$BUFFER")
    local -a _zsh_ios_extra_args
    _zsh_ios_quote_args "$BUFFER"
    output=$("$ZSH_IOS_BIN" resolve --context "$context" "${_zsh_ios_extra_args[@]}" -- "$BUFFER" 2>/dev/null)
    exit_code=$?

    case $exit_code in
        0)
            _zsh_ios_last_tab_buffer=""
            if [[ "$output" != "$BUFFER" ]]; then
                _zsh_ios_ghost_suspended=1
                BUFFER="$output"
                CURSOR=${#BUFFER}
                POSTDISPLAY=""
                region_highlight=("${(@)region_highlight:#* * $_zsh_ios_ghost_style}")
                _zsh_ios_ghost_last_highlight=""
                _zsh_ios_ghost_last_buffer=""
                _zsh_ios_ghost_suspended=0
            else
                zle expand-or-complete
            fi
            ;;
        1)
            # Two-stage Tab: first Tab = LCP extend + one-per-line hint (old
            # behavior); second Tab on the same buffer = picker (with Tab-cycle
            # and number-jump). Any edit to the buffer between Tabs resets.
            if [[ -n "$_zsh_ios_last_tab_buffer" && "$BUFFER" == "$_zsh_ios_last_tab_buffer" ]]; then
                _zsh_ios_last_tab_buffer=""
                _zsh_ios_handle_ambiguity "$output" expand
            else
                _zsh_ios_tab_preview "$output"
                _zsh_ios_last_tab_buffer="$BUFFER"
            fi
            ;;
        3)
            _zsh_ios_last_tab_buffer=""
            _zsh_ios_handle_path_ambiguity "$output" expand
            ;;
        *)
            _zsh_ios_last_tab_buffer=""
            zle expand-or-complete
            ;;
    esac
}

# First-Tab behavior on ambiguity: LCP-extend the buffer (same as the old
# expand-or-complete path) and show the candidate list one-per-line via
# `zle -M`. The hint clears on the next keystroke — so a subsequent Tab
# (caught by the state check above) escalates to the picker.
_zsh_ios_tab_preview() {
    local _zio_word _zio_lcp _zio_position _zio_resolved_prefix _zio_remaining
    local -a _zio_candidates _zio_deep_display _zio_deep_items
    _zsh_ios_safe_eval "$1"

    if [[ -n "$_zio_lcp" && "$_zio_lcp" != "$_zio_word" ]]; then
        if [[ -n "$_zio_resolved_prefix" ]]; then
            BUFFER="$_zio_resolved_prefix $_zio_lcp"
        else
            BUFFER="$_zio_lcp"
        fi
        CURSOR=${#BUFFER}
    fi

    if (( ${#_zio_candidates} > 0 )); then
        local msg="${_zsh_ios_picker_prefix:-%} Ambiguous command: \"$_zio_word\""
        local c
        for c in "${_zio_candidates[@]}"; do
            msg+=$'\n'"  $c"
        done
        msg+=$'\n'"  (Tab again to pick)"
        zle -M "$msg"
    fi
}

# --- Path ambiguity handler: single-keypress selection ---
_zsh_ios_handle_path_ambiguity() {
    local -a _zio_path_candidates
    _zsh_ios_safe_eval "$1"
    local mode="$2"  # "accept" or "expand"

    local count=${#_zio_path_candidates}
    if (( count == 0 )); then
        [[ "$mode" == "accept" ]] && zle accept-line || zle expand-or-complete
        return
    fi
    if (( count == 1 )); then
        BUFFER="${_zio_path_candidates[1]}"
        CURSOR=${#BUFFER}
        [[ "$mode" == "accept" ]] && zle accept-line
        return
    fi

    zle -I

    echo ""
    echo "${_zsh_ios_picker_prefix:-%} Ambiguous path:"
    local i=1
    for item in "${_zio_path_candidates[@]}"; do
        echo "  $i) $item"
        (( i++ ))
    done

    if (( count <= 9 )); then
        echo -n "  > "
        local key
        _zsh_ios_read_picker_key key
        echo ""

        if [[ "$key" =~ ^[1-9]$ ]] && (( key >= 1 && key <= count )); then
            BUFFER="${_zio_path_candidates[$key]}"
            CURSOR=${#BUFFER}
            [[ "$mode" == "accept" ]] && zle accept-line
        else
            zle reset-prompt
        fi
    else
        echo -n "  > "
        local choice
        if [[ -n "$_ZSH_IOS_TEST_INPUT_FD" ]]; then
            read -r -u "$_ZSH_IOS_TEST_INPUT_FD" choice
        else
            read -r choice </dev/tty
        fi

        if [[ "$choice" =~ ^[0-9]+$ ]] && (( choice >= 1 && choice <= count )); then
            BUFFER="${_zio_path_candidates[$choice]}"
            CURSOR=${#BUFFER}
            [[ "$mode" == "accept" ]] && zle accept-line
        else
            zle reset-prompt
        fi
    fi
}

# --- Column-layout helper used by _zsh_ios_help ---
# Usage: _zsh_ios_format_items <label> <newline-separated items string>
# Prints:  "% Expects: <argument> [<label>]\n<two-column layout>"
# Returns the formatted string in $_zio_format_result (avoids subshell fork).
_zsh_ios_format_items() {
    local _label="$1" _raw="$2"
    local -a _items=("${(@f)_raw}")
    _items=("${(@u)_items}")
    local _col_output _max_w=0 _item
    for _item in "${_items[@]}"; do
        (( ${#_item} > _max_w )) && _max_w=${#_item}
    done
    local _col_w=$(( _max_w + 2 ))
    local _cols=$(( 80 / _col_w ))
    (( _cols < 1 )) && _cols=1
    local _line="" _col_n=0
    _col_output=""
    for _item in "${_items[@]}"; do
        (( _col_n == _cols )) && { _col_output+="${_line}"$'\n'; _line=""; _col_n=0 }
        _line+="$(printf "  %-${_max_w}s" "$_item")"
        (( _col_n++ ))
    done
    [[ -n "$_line" ]] && _col_output+="${_line}"$'\n'
    _zio_format_result="${_zsh_ios_picker_prefix:-%} Expects: <argument> [${_label}]\n${_col_output}"
}

# --- ZLE Widget: ? key (show completions) ---
# Cisco IOS behavior:
#   "show ?" (space before ?) = what arguments/subcommands come after "show"
#   "sh?"    (no space)       = what commands match the "sh" prefix
#
# Three-tier fallback when the Rust binary returns a generic "no completions":
#   1. Rust binary — fast, typed completions (branches, hosts, etc.)
#   2. ZLE worker complete-word — full Zsh completion system via compadd intercept
#   3. ZLE worker _approximate — fuzzy/typo-tolerant last resort
# Two-tier worker fallback: complete-word first, _approximate as last resort.
_zsh_ios_help() {
    if _zsh_ios_is_disabled; then
        zle self-insert
        return
    fi

    # Leading `!` bypass: `?` should be a literal self-insert so the user can
    # edit their `!`-prefixed command without zsh-ios popping the help menu.
    if [[ "$BUFFER" == \!* ]]; then
        zle self-insert
        return
    fi

    # If cursor is inside a quoted string, insert a literal '?'
    local prefix="${BUFFER[1,CURSOR]}"
    local sq_count=${#prefix//[^\']/}
    local dq_count=${#prefix//[^\"]/}
    if (( sq_count % 2 != 0 || dq_count % 2 != 0 )); then
        zle self-insert
        return
    fi
    # Also pass through if the previous char is a backslash (literal \?)
    if (( CURSOR > 0 )) && [[ "${BUFFER[CURSOR,CURSOR]}" == "\\" ]]; then
        zle self-insert
        return
    fi

    # Fast path: Rust binary handles typed completions (branches, hosts, etc.)
    local output context
    context=$(_zsh_ios_infer_context "$prefix")
    local -a _zsh_ios_extra_args
    _zsh_ios_quote_args "$prefix"
    output=$("$ZSH_IOS_BIN" complete --context "$context" "${_zsh_ios_extra_args[@]}" -- "$prefix" 2>/dev/null)

    # Detect "generic" output — the Rust binary signaling it has nothing useful.
    # In these cases the ZLE worker may have better results.
    # Also treat a static list of ≤2 items as potentially incomplete: the static
    # parser may have captured only Zsh syntax tokens or a prefix-mode pair (+/-)
    # when the real completions come from a dynamic dispatch (e.g. ssh -o 'Ciphers=').
    local _zio_generic=0
    if [[ "$output" == *'<enter argument>'* || "$output" == *'No commands matching'* ]]; then
        _zio_generic=1
    elif [[ "$output" == *'Expects: <value>'* ]]; then
        # Count non-empty, non-header lines to detect thin static lists
        local _item_count
        _item_count=$(printf '%s' "$output" | grep -c '^  [^E]')
        (( _item_count <= 2 )) && _zio_generic=1
    fi

    if (( _zio_generic )) && _zsh_ios_worker_is_ready; then
        # If the prefix ends with a closing single-quote, strip it before
        # sending to the worker so the cursor is inside the open-quoted context.
        local _worker_prefix="$prefix"
        # Strip trailing whitespace then check for closing quote.
        local _trimmed="${_worker_prefix%%[[:space:]]}"
        if [[ "$_trimmed" == *\' ]]; then
            local _stripped="${_trimmed%\'}"
            local _sq_n="${#${_stripped//[^\']/}}"
            (( _sq_n % 2 != 0 )) && _worker_prefix="$_stripped"
        fi

        # IMPORTANT: call directly in the main shell process, NOT in a $(...)
        # subshell.  zpty handles don't survive fork — the subshell silently
        # fails to write to the worker.  Output goes to a temp file instead.
        local _wc_out="${TMPDIR:-/tmp}/zio-wc-out.$$"
        _zsh_ios_worker_complete "$_worker_prefix" > "$_wc_out" 2>/dev/null
        local worker_items=""
        [[ -s "$_wc_out" ]] && worker_items=$(<"$_wc_out")
        rm -f "$_wc_out"

        if [[ -n "$worker_items" ]]; then
            local _zio_format_result
            _zsh_ios_format_items "ZLE" "$worker_items"
            output="$_zio_format_result"
        else
            # Worker complete returned nothing — try the fallback ladder:
            # approximate → correct → expand_alias → history_complete_word
            local _zio_format_result

            local _wa_out="${TMPDIR:-/tmp}/zio-wa-out.$$"
            _zsh_ios_worker_approximate "$_worker_prefix" > "$_wa_out" 2>/dev/null
            local _approx_out=""
            [[ -s "$_wa_out" ]] && _approx_out=$(<"$_wa_out")
            rm -f "$_wa_out"
            if [[ -n "$_approx_out" ]]; then
                _zsh_ios_format_items "approximate" "$_approx_out"
                output="$_zio_format_result"
            fi

            if [[ -z "$output" ]]; then
                local _corr_out="${TMPDIR:-/tmp}/zio-corr-out.$$"
                _zsh_ios_worker_correct "$_worker_prefix" > "$_corr_out" 2>/dev/null
                local _correct_out=""
                [[ -s "$_corr_out" ]] && _correct_out=$(<"$_corr_out")
                rm -f "$_corr_out"
                if [[ -n "$_correct_out" ]]; then
                    _zsh_ios_format_items "correct" "$_correct_out"
                    output="$_zio_format_result"
                fi
            fi

            if [[ -z "$output" ]]; then
                local _exp_out="${TMPDIR:-/tmp}/zio-exp-out.$$"
                _zsh_ios_worker_expand_alias "$_worker_prefix" > "$_exp_out" 2>/dev/null
                local _expand_out=""
                [[ -s "$_exp_out" ]] && _expand_out=$(<"$_exp_out")
                rm -f "$_exp_out"
                if [[ -n "$_expand_out" ]]; then
                    _zsh_ios_format_items "expand" "$_expand_out"
                    output="$_zio_format_result"
                fi
            fi

            if [[ -z "$output" ]]; then
                local _hist_out="${TMPDIR:-/tmp}/zio-hist-out.$$"
                _zsh_ios_worker_history_complete_word "$_worker_prefix" > "$_hist_out" 2>/dev/null
                local _history_out=""
                [[ -s "$_hist_out" ]] && _history_out=$(<"$_hist_out")
                rm -f "$_hist_out"
                if [[ -n "$_history_out" ]]; then
                    _zsh_ios_format_items "history" "$_history_out"
                    output="$_zio_format_result"
                fi
            fi
        fi
    fi

    if [[ -n "$output" ]]; then
        zle -M "$output"
    else
        zle -M "${_zsh_ios_picker_prefix:-%} No commands found"
    fi
}

# --- Ambiguity handler with interactive clarifier ---
# Modes:
#   accept — pick and run (Enter path)
#   expand — pick, populate BUFFER, return to prompt so the user can edit or Enter (Tab path)
_zsh_ios_handle_ambiguity() {
    local mode="${2:-accept}"
    local _zio_word _zio_lcp _zio_position _zio_resolved_prefix _zio_remaining _zio_pins_path
    local -a _zio_candidates _zio_deep_display _zio_deep_items
    _zsh_ios_safe_eval "$1"

    zle -I

    # Build display items (full command paths) and selection items
    local -a menu_display menu_expanded
    if (( ${#_zio_deep_items} > 0 )); then
        # Multiple first-word matches with subcommand context
        for item in "${_zio_deep_items[@]}"; do
            if [[ -n "$_zio_resolved_prefix" ]]; then
                menu_display+=("$_zio_resolved_prefix $item")
            else
                menu_display+=("$item")
            fi
            menu_expanded+=("$item")
        done
        # Add plain candidates that aren't covered by deep items
        local c dc_cmd
        for c in "${_zio_candidates[@]}"; do
            local found=0
            for dc_cmd in "${_zio_deep_items[@]}"; do
                [[ "$dc_cmd" == "$c "* ]] && { found=1; break; }
            done
            if (( !found )); then
                if [[ -n "$_zio_resolved_prefix" ]]; then
                    menu_display+=("$_zio_resolved_prefix $c")
                else
                    menu_display+=("$c")
                fi
                menu_expanded+=("$c")
            fi
        done
    else
        # Single-level ambiguity -- prepend resolved prefix for display
        for c in "${_zio_candidates[@]}"; do
            if [[ -n "$_zio_resolved_prefix" ]]; then
                menu_display+=("$_zio_resolved_prefix $c")
            else
                menu_display+=("$c")
            fi
            menu_expanded+=("$c")
        done
    fi

    if (( ${#menu_display} == 0 )); then
        echo "  No candidates available."
        zle reset-prompt
        return
    fi

    # Build the abbreviation string from the original typed words
    local -a abbrev_words=( ${(z)BUFFER} )
    local abbrev_str="${(j: :)abbrev_words[1,$((_zio_position+1))]}"

    echo "${_zsh_ios_picker_prefix:-%} Ambiguous command: \"$abbrev_str\""
    echo "  Pick one (prefix ! to also save as pin; Esc/Enter to cancel):"
    local i=1
    for item in "${menu_display[@]}"; do
        echo "    $i) $item"
        (( i++ ))
    done

    echo -n "  > "

    # Keystroke-by-keystroke picker. Input modes, freely intermixed:
    #   • Digits: accept as soon as the buffered digits uniquely identify an
    #     option (no Enter needed). `5` fires instantly in a 5-option menu;
    #     `1` in a 20-option menu waits for a second digit because 10-19 are
    #     still reachable; `13` fires instantly.
    #   • Tab / arrows: advance (or reverse) a cycle highlight through the
    #     options (wraps). The prompt redraws to `> [N] <choice>`. Enter or
    #     another Tab commits.
    #   • `!` (as the first char): toggles "save as pin" mode — the current
    #     pick will also be written to the pins file. Prompt redraws with a
    #     leading `!` so the mode is visible.
    #   • Enter on empty: cancel. Enter while cycling: commit the highlight.
    #     Esc or any other unhandled key cancels.
    local choice=""
    local cycle_idx=0
    local save_mode=0
    local max=${#menu_display}
    local key trial extendable k sk
    # Redraw the `> ` prompt line showing the current cycle highlight (or
    # buffered digits) and the `!` save-mode indicator. \r returns to
    # column 0; \e[K clears to EOL.
    _zsh_ios_pick_redraw_cycle() {
        local _prefix="  > "
        (( save_mode )) && _prefix="  > !"
        if (( cycle_idx == 0 )); then
            if [[ -n "$choice" ]]; then
                printf '\r%s%s\e[K' "$_prefix" "$choice"
            else
                printf '\r%s\e[K' "$_prefix"
            fi
        else
            printf '\r%s[%d] %s\e[K' "$_prefix" "$cycle_idx" "${menu_display[$cycle_idx]}"
        fi
    }

    # Put the terminal into cbreak / no-echo for the duration of the picker
    # loop so single-byte reads deliver arrow-key escape sequences correctly
    # instead of being line-buffered and echoed as literal ^[[A bytes. Only
    # meaningful on the real /dev/tty path — tests feed keystrokes through
    # $_ZSH_IOS_TEST_INPUT_FD, which doesn't go through the tty.
    local _saved_stty=""
    if [[ -z "$_ZSH_IOS_TEST_INPUT_FD" ]]; then
        _saved_stty=$(stty -g </dev/tty 2>/dev/null)
        [[ -n "$_saved_stty" ]] && stty -icanon -echo min 1 time 0 </dev/tty 2>/dev/null
    fi
    {
    while true; do
        _zsh_ios_read_picker_key key
        case "$key" in
            $'\n'|$'\r')
                # Enter: commit digits if present, else cycle highlight, else cancel.
                echo ""
                if [[ -z "$choice" && $cycle_idx -gt 0 ]]; then
                    choice=$cycle_idx
                fi
                break
                ;;
            $'\t')
                # Tab cycles the highlight. If digits were being typed, wipe
                # them first — mixing half-typed numbers with a cycle position
                # would be confusing.
                choice=""
                (( cycle_idx = cycle_idx % max + 1 ))
                _zsh_ios_pick_redraw_cycle
                ;;
            '!')
                # `!` toggles save-as-pin mode. The next commit will also
                # write a pin. Only meaningful before any digit has been
                # entered; pressing `!` mid-digit-entry also toggles.
                save_mode=$(( 1 - save_mode ))
                _zsh_ios_pick_redraw_cycle
                ;;
            $'\x7f'|$'\b')
                # Backspace: erase one digit, step back one cycle position,
                # or turn off save-mode if that's the only active state.
                if [[ -n "$choice" ]]; then
                    choice="${choice%?}"
                    _zsh_ios_pick_redraw_cycle
                elif (( cycle_idx > 0 )); then
                    (( cycle_idx-- ))
                    _zsh_ios_pick_redraw_cycle
                elif (( save_mode )); then
                    save_mode=0
                    _zsh_ios_pick_redraw_cycle
                fi
                ;;
            [0-9])
                # Switching from cycle → digits: clear the highlight display.
                if (( cycle_idx > 0 )); then
                    cycle_idx=0
                fi
                trial="$choice$key"
                # Is any option number strictly longer than `trial` that
                # starts with `trial`? If yes, we must wait for more input.
                extendable=0
                for (( k = 1; k <= max; k++ )); do
                    sk="$k"
                    if (( ${#sk} > ${#trial} )) && [[ "$sk" == "$trial"* ]]; then
                        extendable=1
                        break
                    fi
                done
                # `trial` itself must either equal a valid option or be a
                # prefix of one. Anything else (e.g. `6` with only 5 options,
                # `0` at any time) is a typo and gets silently dropped.
                if (( trial >= 1 && trial <= max )) || (( extendable )); then
                    choice="$trial"
                    _zsh_ios_pick_redraw_cycle
                    if (( !extendable )); then
                        echo ""
                        break
                    fi
                fi
                ;;
            $'\x1b')
                # Possibly an ANSI escape sequence (arrow keys send ESC [ A/B/C/D).
                # Peek at the next two bytes with a short timeout so a lone Esc
                # (which has nothing following it) is still treated as cancel.
                local _zio_esc_next _zio_esc_code
                if _zsh_ios_read_picker_key_timeout _zio_esc_next 50 && [[ "$_zio_esc_next" == '[' ]]; then
                    if _zsh_ios_read_picker_key_timeout _zio_esc_code 50; then
                        case "$_zio_esc_code" in
                            A|D)
                                # Up / Left — move to previous option (wraps).
                                choice=""
                                (( cycle_idx = (cycle_idx + max - 2) % max + 1 ))
                                _zsh_ios_pick_redraw_cycle
                                ;;
                            B|C)
                                # Down / Right — move to next option (same as Tab).
                                choice=""
                                (( cycle_idx = cycle_idx % max + 1 ))
                                _zsh_ios_pick_redraw_cycle
                                ;;
                            *)
                                # Unrecognised escape sequence — cancel.
                                echo ""
                                choice=""
                                cycle_idx=0
                                break
                                ;;
                        esac
                    else
                        # ESC [ but no third byte — cancel.
                        echo ""
                        choice=""
                        cycle_idx=0
                        break
                    fi
                else
                    # Lone ESC (nothing followed within timeout) — cancel.
                    echo ""
                    choice=""
                    cycle_idx=0
                    break
                fi
                ;;
            *)
                # Any non-digit, non-enter, non-backspace, non-tab, non-esc cancels.
                echo ""
                choice=""
                cycle_idx=0
                break
                ;;
        esac
    done
    } always {
        # Restore the terminal to its pre-picker mode on every exit path
        # (normal commit, cancel, typo, Esc, error). If we leave the tty in
        # cbreak/no-echo, the shell prompt after a cancel is unusable.
        [[ -n "$_saved_stty" ]] && stty "$_saved_stty" </dev/tty 2>/dev/null
    }
    unfunction _zsh_ios_pick_redraw_cycle 2>/dev/null

    # Cycle commit: if Enter/Tab-commit-digit path didn't populate `choice`
    # but a cycle highlight was active, use the highlighted option.
    if [[ -z "$choice" && $cycle_idx -gt 0 ]]; then
        choice=$cycle_idx
    fi

    if [[ -z "$choice" ]]; then
        zle reset-prompt
        return
    fi

    local selected_display selected_expanded
    if (( choice >= 1 && choice <= ${#menu_display} )); then
        selected_display="${menu_display[$choice]}"
        selected_expanded="${menu_expanded[$choice]}"
    fi

    if [[ -z "$selected_display" ]]; then
        echo "  Invalid selection."
        zle reset-prompt
        return
    fi

    # Pin the full abbreviated sequence -> full expansion, but only when the
    # user explicitly opted in via the `!` prefix during selection. A plain
    # digit / Tab / arrow pick is one-shot: useful for the current command
    # line without committing the abbreviation to the pins file forever.
    local pin_expanded="$selected_display"
    if (( save_mode )); then
        "$ZSH_IOS_BIN" pin "$abbrev_str" --to "$pin_expanded" 2>/dev/null
        local display_path="${_zio_pins_path/#$HOME/~}"
        echo "  Saved pin: \"$abbrev_str\" → \"$pin_expanded\""
        echo "  In $display_path"
        _zsh_ios_last_pin="$abbrev_str"
    fi

    # Build the full command to execute
    local full_cmd="$selected_display"

    if [[ -n "$_zio_remaining" ]]; then
        local -a sel_words=( ${(z)selected_expanded} )
        local -a remaining_words=( ${(z)_zio_remaining} )
        if (( ${#sel_words} > 1 )); then
            # Deep candidate: first remaining word was used for disambiguation, skip it
            full_cmd="$full_cmd ${remaining_words[*]:1}"
        else
            full_cmd="$full_cmd $_zio_remaining"
        fi
    fi

    # Append any words after the ambiguous position that weren't in remaining
    local -a all_words=( ${(z)BUFFER} )
    local after_pos=$(( _zio_position + 2 ))
    if (( after_pos <= ${#all_words} )) && [[ -z "$_zio_remaining" ]]; then
        full_cmd="$full_cmd ${(j: :)all_words[$after_pos,-1]}"
    fi

    BUFFER="${full_cmd%% }"
    CURSOR=${#BUFFER}
    if [[ "$mode" == "expand" ]]; then
        # Redraw so the user can inspect / edit before pressing Enter.
        zle reset-prompt
    else
        zle accept-line
    fi
}

# ─────────────────────────────────────────────────────────────────────────────
# ZLE COMPLETION WORKER
#
# A persistent background Zsh process (started via `zpty`) that has the full
# completion system loaded and ZLE active.  When the static analysis in the
# Rust binary cannot provide completions (e.g. `ssh -o 'Ciphers='`, `ip link
# add type`), we ask the worker to run `zle complete-word` with the current
# buffer and capture what `compadd` would normally add.
#
# Architecture
# ────────────
#  • The worker starts lazily on the first ZLE line-init event (so startup cost
#    is hidden while the user reads the previous output / thinks).
#  • The worker's Zsh loads via a custom ZDOTDIR that sources the user's real
#    .zshrc (plugin returns early due to _ZSH_IOS_IS_WORKER), then a setup
#    script that overrides `compadd` and `accept-line`.
#  • The parent triggers completion by writing a request file then sending a
#    newline via `zpty -w`, which triggers the worker's accept-line override.
#  • Before each request, the parent drains accumulated pty output to prevent
#    the worker's ZLE from blocking on a full output buffer.
#  • Requests and results are exchanged via temp files to avoid the noise that
#    pty I/O carries (ANSI escape sequences, echoed input, etc.).
#  • A "done file" (result_file.done) acts as a zero-cost semaphore: the worker
#    touches it when the completion widget finishes.  We poll in 10 ms slices
#    up to ZSH_IOS_WORKER_TIMEOUT_MS (default 500 ms).
#  • We only invoke the worker when the Rust binary returns a generic "no
#    completions" signal — fast / typed completions (branches, hosts, etc.) are
#    still served by the Rust binary with zero IPC overhead.
# ─────────────────────────────────────────────────────────────────────────────

# How long (ms) to wait for the worker before giving up on a single request.
: ${ZSH_IOS_WORKER_TIMEOUT_MS:=500}

# Well-known directory for this shell's worker (deterministic path = no leaks).
typeset -g _ZSH_IOS_WORKER_DIR="${TMPDIR:-/tmp}/zsh-ios-worker-$$"
typeset -g _ZSH_IOS_WORKER_STARTING=0

# On (re-)source: always tear down any previous worker from this PID.
_zsh_ios_worker_teardown() {
    zmodload zsh/zpty 2>/dev/null
    zpty -d _zsh_ios_worker 2>/dev/null
    if [[ -f "${_ZSH_IOS_WORKER_DIR}/pid" ]]; then
        local _p; _p=$(<"${_ZSH_IOS_WORKER_DIR}/pid")
        [[ -n "$_p" ]] && kill "$_p" 2>/dev/null && sleep 0.1 && kill -9 "$_p" 2>/dev/null
    fi
    [[ -d "$_ZSH_IOS_WORKER_DIR" ]] && rm -rf "$_ZSH_IOS_WORKER_DIR"
    _ZSH_IOS_WORKER_STARTING=0
}
_zsh_ios_worker_teardown

_zsh_ios_worker_is_ready() {
    [[ -f "${_ZSH_IOS_WORKER_DIR}/ready" ]]
}

_zsh_ios_worker_start() {
    # Respect config-driven disable_worker knob (parsed from `zsh-ios status`).
    [[ "${_zsh_ios_worker_disabled:-0}" == "1" ]] && return 0
    _zsh_ios_worker_is_ready && return 0
    (( _ZSH_IOS_WORKER_STARTING )) && return 0
    _ZSH_IOS_WORKER_STARTING=1

    zmodload zsh/zpty 2>/dev/null || { _ZSH_IOS_WORKER_STARTING=0; return 1; }
    mkdir -p "$_ZSH_IOS_WORKER_DIR" || { _ZSH_IOS_WORKER_STARTING=0; return 1; }

    local _sf="${_ZSH_IOS_WORKER_DIR}/setup.zsh"
    # Heredoc delimiter is unquoted: parent-side $vars expand at write time,
    # worker-side vars use \$ to defer evaluation to source time.
    cat > "$_sf" <<WORKER_SETUP
# Ensure emacs keybindings so regular chars → self-insert (predictable).
bindkey -e
# No history pollution from the worker.
HISTSIZE=0; SAVEHIST=0
# Prevent interactive menu/listing behavior during programmatic completion.
# NO_AUTO_MENU: don't enter menu selection on ambiguous completions.
# NO_AUTO_LIST: don't display the match list (would swallow the next keypress).
# NO_LIST_BEEP: don't beep on ambiguous completions.
setopt NO_AUTO_MENU NO_AUTO_LIST NO_LIST_BEEP
# Ensure the completion system is initialized (the user's .zshrc may skip
# compinit under ZDOTDIR or conditional checks).  Use the real home zcompdump.
autoload -Uz compinit && compinit -d "$HOME/.zcompdump" 2>/dev/null

compadd() {
    builtin compadd "\$@"
    [[ -z "\$_ZIO_RF" ]] && return
    local -a _zio_items=()
    local _zio_sep=0 _zio_skip=0 _zio_arr_mode=0 _zio_arr_name=""
    local _zio_a
    for _zio_a in "\$@"; do
        if (( _zio_skip )); then _zio_skip=0; continue; fi
        if (( _zio_sep )); then _zio_items+=("\$_zio_a"); continue; fi
        case "\$_zio_a" in
            --|-)  _zio_sep=1 ;;
            -a)    _zio_arr_mode=1 ;;
            -k)    _zio_arr_mode=2 ;;
            -[JVXxMPSpsIWFrRDOAEd]) _zio_skip=1 ;;
            -[JVXxMPSpsIWFrRDOAEd]*) ;;
            -*)    ;;
            *)  if (( _zio_arr_mode )); then _zio_arr_name="\$_zio_a"
                else _zio_items+=("\$_zio_a"); fi ;;
        esac
    done
    if [[ -n "\$_zio_arr_name" ]] && (( _zio_arr_mode )); then
        eval 'printf "%s\n" "\${(P@)_zio_arr_name}"' >> "\$_ZIO_RF" 2>/dev/null
    elif (( \$#_zio_items )); then
        printf '%s\n' "\${_zio_items[@]}" >> "\$_ZIO_RF"
    fi
}

# Configure the approximate completer for the 'approximate' request type.
# max-errors 2 numeric — tolerate up to 2 typos (insertion/deletion/swap),
# shown as numeric substitutions in the prompt.  Applies only when we opt
# into it via compstate[completer].
zstyle ':completion:zsh-ios:approximate:*' max-errors 2 numeric
zstyle ':completion:zsh-ios:approximate:*' completer _approximate

# Override accept-line: when a request file exists, handle the request instead
# of accepting input.  The parent triggers this by sending a newline via zpty -w.
_zio_accept_line() {
    local _req="${_ZSH_IOS_WORKER_DIR}/request"
    if [[ -f "\$_req" ]]; then
        source "\$_req"
        : > "\$_ZIO_RF"
        case "\${_ZIO_REQUEST:-complete-word}" in
            complete-word)
                BUFFER="\$_ZIO_BUFFER"
                CURSOR="\${_ZIO_CURSOR:-\${#BUFFER}}"
                zle complete-word 2>/dev/null
                BUFFER=""
                CURSOR=0
                ;;
            approximate)
                BUFFER="\$_ZIO_BUFFER"
                CURSOR="\${_ZIO_CURSOR:-\${#BUFFER}}"
                # Force the approximate completer by temporarily overriding the
                # global completer list.  zle _approximate dispatches the widget
                # directly so we don't depend on compstate manipulation.
                local _zio_prev_completer
                zstyle -s ':completion:*' completer _zio_prev_completer 2>/dev/null
                zstyle ':completion:*' completer _approximate
                zle _approximate 2>/dev/null || zle complete-word 2>/dev/null
                if [[ -n "\$_zio_prev_completer" ]]; then
                    zstyle ':completion:*' completer "\$_zio_prev_completer"
                else
                    zstyle -d ':completion:*' completer 2>/dev/null
                fi
                BUFFER=""
                CURSOR=0
                ;;
            correct)
                BUFFER="\$_ZIO_BUFFER"
                CURSOR="\${_ZIO_CURSOR:-\${#BUFFER}}"
                local _zio_prev_completer
                if ! zstyle -s ':completion:*' completer _zio_prev_completer 2>/dev/null; then
                    _zio_prev_completer=""
                fi
                zstyle ':completion:*' completer _correct
                zle _correct 2>/dev/null || zle complete-word 2>/dev/null
                if [[ -n "\$_zio_prev_completer" ]]; then
                    zstyle ':completion:*' completer "\$_zio_prev_completer"
                else
                    zstyle -d ':completion:*' completer
                fi
                BUFFER=""
                CURSOR=0
                ;;
            expand_alias)
                BUFFER="\$_ZIO_BUFFER"
                CURSOR="\${_ZIO_CURSOR:-\${#BUFFER}}"
                # _expand_alias mutates BUFFER in place rather than emitting
                # compadd completions.  Capture the before/after delta and emit it.
                local _zio_before="\$BUFFER"
                zle _expand_alias 2>/dev/null || true
                if [[ "\$BUFFER" != "\$_zio_before" ]]; then
                    print -r -- "\$BUFFER" >> "\$_ZIO_RF" 2>/dev/null
                fi
                BUFFER=""
                CURSOR=0
                ;;
            history_complete_word)
                BUFFER="\$_ZIO_BUFFER"
                CURSOR="\${_ZIO_CURSOR:-\${#BUFFER}}"
                local _zio_prev_completer
                if ! zstyle -s ':completion:*' completer _zio_prev_completer 2>/dev/null; then
                    _zio_prev_completer=""
                fi
                zstyle ':completion:*' completer _history_complete_word
                zle _history-complete-word 2>/dev/null || zle complete-word 2>/dev/null
                if [[ -n "\$_zio_prev_completer" ]]; then
                    zstyle ':completion:*' completer "\$_zio_prev_completer"
                else
                    zstyle -d ':completion:*' completer
                fi
                BUFFER=""
                CURSOR=0
                ;;
            dump-aliases)
                alias >> "\$_ZIO_RF" 2>/dev/null
                ;;
            dump-galiases)
                alias -g >> "\$_ZIO_RF" 2>/dev/null
                ;;
            dump-saliases)
                alias -s >> "\$_ZIO_RF" 2>/dev/null
                ;;
            dump-functions)
                print -l "\${(k)functions}" >> "\$_ZIO_RF" 2>/dev/null
                ;;
            dump-nameddirs)
                hash -d >> "\$_ZIO_RF" 2>/dev/null
                ;;
            dump-zstyle)
                zstyle -L >> "\$_ZIO_RF" 2>/dev/null
                ;;
            dump-history)
                print -l "\${history[@]}" >> "\$_ZIO_RF" 2>/dev/null
                ;;
            dump-dirstack)
                # Combines \$dirstack (array) with \$PWD at index 0.  Zsh's
                # dirstack array doesn't include the current dir — users expect
                # ~0 to map to PWD and ~1 to the first pushed entry.
                print -r -- "\$PWD" >> "\$_ZIO_RF" 2>/dev/null
                print -l "\${dirstack[@]}" >> "\$_ZIO_RF" 2>/dev/null
                ;;
            dump-jobs)
                jobs >> "\$_ZIO_RF" 2>/dev/null
                ;;
            dump-commands)
                hash -L >> "\$_ZIO_RF" 2>/dev/null
                ;;
            dump-parameters)
                typeset +m '*' >> "\$_ZIO_RF" 2>/dev/null
                ;;
            dump-options)
                setopt >> "\$_ZIO_RF" 2>/dev/null
                ;;
            dump-widgets)
                zle -l >> "\$_ZIO_RF" 2>/dev/null
                ;;
            dump-modules)
                zmodload >> "\$_ZIO_RF" 2>/dev/null
                ;;
            dump-regex-args)
                # Override the two _regex_arguments sinks so the completion
                # function captures its spec strings instead of dispatching.
                # Restored via unfunction right after the call.
                function _regex_words() {
                    local _zrw_tag="\$1"
                    local _zrw_desc="\$2"
                    shift 2
                    reply=("\$@")
                    print -r -- "__ZIO_REGEX_WORDS__" "\$_zrw_tag" "\$_zrw_desc" "\$@" >> "\$_ZIO_RF" 2>/dev/null
                    return 0
                }
                function _regex_arguments() {
                    print -r -- "__ZIO_REGEX_ARGS__" "\$@" >> "\$_ZIO_RF" 2>/dev/null
                    return 0
                }
                if [[ -n "\$_ZIO_REGEX_FUNC" ]]; then
                    autoload -Uz "\$_ZIO_REGEX_FUNC" 2>/dev/null
                    "\$_ZIO_REGEX_FUNC" 2>/dev/null || true
                fi
                unfunction _regex_words _regex_arguments 2>/dev/null
                ;;
        esac
        [[ -n "\$_ZIO_DF" ]] && touch "\$_ZIO_DF"
        rm -f "\$_req"
        return
    fi
    zle .accept-line
}
zle -N accept-line _zio_accept_line
WORKER_SETUP

    # Create a ZDOTDIR so the worker's zsh -i auto-sources setup — no zpty -w needed.
    local _real_zdotdir="${ZDOTDIR:-$HOME}"
    local _zdot="${_ZSH_IOS_WORKER_DIR}/zdot"
    mkdir -p "$_zdot"
    cat > "${_zdot}/.zshenv" <<EOF
[[ -f "${_real_zdotdir}/.zshenv" ]] && source "${_real_zdotdir}/.zshenv"
EOF
    cat > "${_zdot}/.zshrc" <<EOF
source "${_real_zdotdir}/.zshrc"
source "${_sf}"
touch "${_ZSH_IOS_WORKER_DIR}/ready"
EOF

    zpty -b _zsh_ios_worker "exec env _ZSH_IOS_IS_WORKER=1 ZDOTDIR='${_zdot}' TERM=${TERM:-xterm-256color} ${SHELL:-zsh} -i 2>/dev/null" 2>/dev/null || {
        _ZSH_IOS_WORKER_STARTING=0; return 1
    }

    # Record the worker PID for kill-based cleanup.
    local _wpid
    _wpid=$(pgrep -n -u "$UID" -f 'zsh -i' 2>/dev/null)
    [[ -n "$_wpid" ]] && printf '%s' "$_wpid" > "${_ZSH_IOS_WORKER_DIR}/pid"
}

_zsh_ios_worker_complete() {
    _zsh_ios_worker_is_ready || return 1
    local _buf="$1" _rf _df

    # Drain accumulated pty output so the worker's ZLE can start/continue.
    # Without this, ZLE blocks writing the prompt to a full pty output buffer.
    local _zio_drain_buf _zio_drain_n=0
    while zpty -t _zsh_ios_worker 2>/dev/null; do
        zpty -r _zsh_ios_worker _zio_drain_buf 2>/dev/null || break
        (( ++_zio_drain_n > 200 )) && break
    done

    _rf=$(mktemp "${TMPDIR:-/tmp}/zio-result.XXXXXX") || return 1
    _df="${_rf}.done"

    # Write request file (sourced by the worker — no quoting issues).
    local _req="${_ZSH_IOS_WORKER_DIR}/request"
    local _bf="${_rf}.buf"
    printf '%s' "$_buf" > "$_bf"
    cat > "$_req" <<EOF
_ZIO_RF='$_rf'
_ZIO_DF='$_df'
_ZIO_BUFFER="\$(<'$_bf')"
_ZIO_CURSOR=${#_buf}
EOF

    # Send a newline to the worker's ZLE — triggers our accept-line override.
    # Use -n + explicit \n (zpty -w rejects empty strings but accepts no args).
    zpty -w -n _zsh_ios_worker $'\n' 2>/dev/null || {
        rm -f "$_rf" "$_df" "$_bf" "$_req"; return 1
    }

    # Poll for the done-file, draining pty output each cycle so the worker's
    # ZLE can re-enter after precmd / prompt display without blocking.
    local _slices=$(( ZSH_IOS_WORKER_TIMEOUT_MS / 10 )) _i
    for _i in $(seq 1 $_slices); do
        [[ -f "$_df" ]] && break
        sleep 0.01
        while zpty -t _zsh_ios_worker 2>/dev/null; do
            zpty -r _zsh_ios_worker _zio_drain_buf 2>/dev/null || break
        done
    done
    local _rc=1
    if [[ -f "$_rf" && -s "$_rf" ]]; then cat "$_rf"; _rc=0; fi
    rm -f "$_rf" "$_df" "$_bf" "$_req"
    return $_rc
}

_zsh_ios_worker_approximate() {
    _zsh_ios_worker_is_ready || return 1
    local _buf="$1" _rf _df

    # Drain accumulated pty output
    local _zio_drain_buf _zio_drain_n=0
    while zpty -t _zsh_ios_worker 2>/dev/null; do
        zpty -r _zsh_ios_worker _zio_drain_buf 2>/dev/null || break
        (( ++_zio_drain_n > 200 )) && break
    done

    _rf=$(mktemp "${TMPDIR:-/tmp}/zio-approx.XXXXXX") || return 1
    _df="${_rf}.done"
    local _req="${_ZSH_IOS_WORKER_DIR}/request"
    local _bf="${_rf}.buf"
    printf '%s' "$_buf" > "$_bf"
    cat > "$_req" <<EOF
_ZIO_REQUEST='approximate'
_ZIO_RF='$_rf'
_ZIO_DF='$_df'
_ZIO_BUFFER="\$(<'$_bf')"
_ZIO_CURSOR=${#_buf}
EOF

    zpty -w -n _zsh_ios_worker $'\n' 2>/dev/null || {
        rm -f "$_rf" "$_df" "$_bf" "$_req"; return 1
    }

    local _slices=$(( ZSH_IOS_WORKER_TIMEOUT_MS / 10 )) _i
    for _i in $(seq 1 $_slices); do
        [[ -f "$_df" ]] && break
        sleep 0.01
        while zpty -t _zsh_ios_worker 2>/dev/null; do
            zpty -r _zsh_ios_worker _zio_drain_buf 2>/dev/null || break
        done
    done
    local _rc=1
    if [[ -f "$_rf" && -s "$_rf" ]]; then cat "$_rf"; _rc=0; fi
    rm -f "$_rf" "$_df" "$_bf" "$_req"
    return $_rc
}

# Shared implementation for the completer-chain fallback functions.
# Usage: _zsh_ios_worker_dispatch_completion <kind> <buffer>
# Sends a request of the given type to the worker and prints any results.
_zsh_ios_worker_dispatch_completion() {
    local _kind="$1" _buf="$2"
    _zsh_ios_worker_is_ready || return 1
    [[ -n "$_kind" && -n "$_buf" ]] || return 1

    # Drain accumulated pty output
    local _zio_drain_buf _zio_drain_n=0
    while zpty -t _zsh_ios_worker 2>/dev/null; do
        zpty -r _zsh_ios_worker _zio_drain_buf 2>/dev/null || break
        (( ++_zio_drain_n > 200 )) && break
    done

    local _rf
    _rf=$(mktemp "${TMPDIR:-/tmp}/zio-${_kind}.XXXXXX") || return 1
    local _df="${_rf}.done"
    local _req="${_ZSH_IOS_WORKER_DIR}/request"
    local _bf="${_rf}.buf"
    printf '%s' "$_buf" > "$_bf"
    cat > "$_req" <<EOF
_ZIO_REQUEST='${_kind}'
_ZIO_RF='$_rf'
_ZIO_DF='$_df'
_ZIO_BUFFER="\$(<'$_bf')"
_ZIO_CURSOR=${#_buf}
EOF

    zpty -w -n _zsh_ios_worker $'\n' 2>/dev/null || {
        rm -f "$_rf" "$_df" "$_bf" "$_req"; return 1
    }

    local _slices=$(( ZSH_IOS_WORKER_TIMEOUT_MS / 10 )) _i
    for _i in $(seq 1 $_slices); do
        [[ -f "$_df" ]] && break
        sleep 0.01
        while zpty -t _zsh_ios_worker 2>/dev/null; do
            zpty -r _zsh_ios_worker _zio_drain_buf 2>/dev/null || break
        done
    done
    local _rc=1
    if [[ -f "$_rf" && -s "$_rf" ]]; then cat "$_rf"; _rc=0; fi
    rm -f "$_rf" "$_df" "$_bf" "$_req"
    return $_rc
}

_zsh_ios_worker_correct()               { _zsh_ios_worker_dispatch_completion correct "$1"; }
_zsh_ios_worker_expand_alias()          { _zsh_ios_worker_dispatch_completion expand_alias "$1"; }
_zsh_ios_worker_history_complete_word() { _zsh_ios_worker_dispatch_completion history_complete_word "$1"; }

_zsh_ios_worker_dump() {
    _zsh_ios_worker_is_ready || return 1
    local kind="$1"
    [[ -n "$kind" ]] || return 1

    # Drain accumulated pty output (same pattern as _zsh_ios_worker_complete).
    local _zio_drain_buf _zio_drain_n=0
    while zpty -t _zsh_ios_worker 2>/dev/null; do
        zpty -r _zsh_ios_worker _zio_drain_buf 2>/dev/null || break
        (( ++_zio_drain_n > 200 )) && break
    done

    local _rf; _rf=$(mktemp "${TMPDIR:-/tmp}/zio-dump.XXXXXX") || return 1
    local _df="${_rf}.done"
    local _req="${_ZSH_IOS_WORKER_DIR}/request"

    cat > "$_req" <<EOF
_ZIO_REQUEST='dump-${kind}'
_ZIO_RF='$_rf'
_ZIO_DF='$_df'
EOF

    zpty -w -n _zsh_ios_worker $'\n' 2>/dev/null || {
        rm -f "$_rf" "$_df" "$_req"; return 1
    }

    local _slices=$(( ZSH_IOS_WORKER_TIMEOUT_MS / 10 )) _i
    for _i in $(seq 1 $_slices); do
        [[ -f "$_df" ]] && break
        sleep 0.01
        while zpty -t _zsh_ios_worker 2>/dev/null; do
            zpty -r _zsh_ios_worker _zio_drain_buf 2>/dev/null || break
        done
    done
    local _rc=1
    if [[ -f "$_rf" && -s "$_rf" ]]; then cat "$_rf"; _rc=0; fi
    rm -f "$_rf" "$_df" "$_req"
    return $_rc
}

_zsh_ios_worker_dump_regex_args() {
    _zsh_ios_worker_is_ready || return 1
    local _func="$1"
    [[ -n "$_func" ]] || return 1

    # Drain accumulated pty output (same pattern as _zsh_ios_worker_dump).
    local _zio_drain_buf _zio_drain_n=0
    while zpty -t _zsh_ios_worker 2>/dev/null; do
        zpty -r _zsh_ios_worker _zio_drain_buf 2>/dev/null || break
        (( ++_zio_drain_n > 200 )) && break
    done

    local _rf; _rf=$(mktemp "${TMPDIR:-/tmp}/zio-rxa.XXXXXX") || return 1
    local _df="${_rf}.done"
    local _req="${_ZSH_IOS_WORKER_DIR}/request"

    # Write request file. _ZIO_REGEX_FUNC is a worker-side variable so we use
    # a literal assignment — no parent-side expansion wanted.
    cat > "$_req" <<EOF
_ZIO_REQUEST='dump-regex-args'
_ZIO_RF='$_rf'
_ZIO_DF='$_df'
_ZIO_REGEX_FUNC='$_func'
EOF

    zpty -w -n _zsh_ios_worker $'\n' 2>/dev/null || {
        rm -f "$_rf" "$_df" "$_req"; return 1
    }

    local _slices=$(( ZSH_IOS_WORKER_TIMEOUT_MS / 10 )) _i
    for _i in $(seq 1 $_slices); do
        [[ -f "$_df" ]] && break
        sleep 0.01
        while zpty -t _zsh_ios_worker 2>/dev/null; do
            zpty -r _zsh_ios_worker _zio_drain_buf 2>/dev/null || break
        done
    done
    local _rc=1
    if [[ -f "$_rf" && -s "$_rf" ]]; then cat "$_rf"; _rc=0; fi
    rm -f "$_rf" "$_df" "$_req"
    return $_rc
}

_zsh_ios_ingest_worker_state() {
    # One-shot; runs in background.  Concatenates worker dumps with @<kind>
    # section markers and pipes into `zsh-ios ingest` for trie integration.
    {
        print "@aliases"
        _zsh_ios_worker_dump aliases
        print "@galiases"
        _zsh_ios_worker_dump galiases
        print "@saliases"
        _zsh_ios_worker_dump saliases
        print "@functions"
        _zsh_ios_worker_dump functions
        print "@nameddirs"
        _zsh_ios_worker_dump nameddirs
        print "@history"
        _zsh_ios_worker_dump history
        print "@dirstack"
        _zsh_ios_worker_dump dirstack
        print "@jobs"
        _zsh_ios_worker_dump jobs
        print "@commands"
        _zsh_ios_worker_dump commands
        print "@parameters"
        _zsh_ios_worker_dump parameters
        print "@options"
        _zsh_ios_worker_dump options
        print "@widgets"
        _zsh_ios_worker_dump widgets
        print "@modules"
        _zsh_ios_worker_dump modules
    } | "$ZSH_IOS_BIN" ingest 2>/dev/null
}

_zsh_ios_harvest_regex_args() {
    _zsh_ios_worker_is_ready || return 1

    # Cache file: one line per already-processed file: "<abs-path>|<mtime-epoch>".
    local _cache_dir="${XDG_CACHE_HOME:-$HOME/.cache}/zsh-ios"
    local _cache_file="$_cache_dir/regex-harvest.cache"
    mkdir -p "$_cache_dir" 2>/dev/null

    # Search fpath directories that may contain _regex_arguments completions.
    local -a _search_paths
    _search_paths=(
        /usr/share/zsh/*/functions/_*
        /usr/local/share/zsh/site-functions/_*
        /opt/homebrew/share/zsh/site-functions/_*
    )

    local _tmp="${TMPDIR:-/tmp}/zio-regex-harvest.$$"
    : > "$_tmp"

    local _file _fn _mtime _cache_hit _new_cache
    _new_cache=""

    for _file in "${_search_paths[@]}"; do
        [[ -f "$_file" ]] || continue
        grep -q '_regex_arguments' "$_file" 2>/dev/null || continue

        # Get mtime as epoch seconds (cross-platform).
        if [[ "$(uname -s)" == "Darwin" ]]; then
            _mtime=$(stat -f %m "$_file" 2>/dev/null || echo 0)
        else
            _mtime=$(stat -c %Y "$_file" 2>/dev/null || echo 0)
        fi

        # Cache check: skip if we've already processed this file at this mtime.
        _cache_hit=0
        if [[ -f "$_cache_file" ]]; then
            if grep -qF "${_file}|${_mtime}" "$_cache_file" 2>/dev/null; then
                _cache_hit=1
            fi
        fi

        if (( _cache_hit )); then
            # Preserve this entry in the new cache.
            _new_cache+="${_file}|${_mtime}"$'\n'
            continue
        fi

        # Function name matches the filename (basename).
        _fn="${_file:t}"
        local _rxa_out="${TMPDIR:-/tmp}/zio-rxa-out.$$"
        _zsh_ios_worker_dump_regex_args "$_fn" > "$_rxa_out" 2>/dev/null
        if [[ -s "$_rxa_out" ]]; then
            cat "$_rxa_out" >> "$_tmp"
        fi
        rm -f "$_rxa_out"

        # Record this file+mtime as processed.
        _new_cache+="${_file}|${_mtime}"$'\n'
    done

    # Write updated cache (replaces old contents).
    if [[ -n "$_new_cache" ]]; then
        printf '%s' "$_new_cache" > "$_cache_file" 2>/dev/null
    fi

    # Feed combined capture to the binary for trie folding.
    if [[ -s "$_tmp" ]]; then
        "$ZSH_IOS_BIN" regex-args-ingest < "$_tmp" 2>/dev/null
    fi
    rm -f "$_tmp"
}

_zsh_ios_worker_cleanup() {
    _zsh_ios_worker_teardown
}
zshexit_functions+=(_zsh_ios_worker_cleanup)

_zsh_ios_worker_ping() {
    print "=== Worker Diagnostics ==="
    print "worker_dir: $_ZSH_IOS_WORKER_DIR"
    print "ready file: $([[ -f "${_ZSH_IOS_WORKER_DIR}/ready" ]] && echo YES || echo NO)"
    print "zpty       : $(zpty -L 2>/dev/null | grep _zsh_ios_worker || echo 'none')"
    local _wpid; _wpid=$(cat "${_ZSH_IOS_WORKER_DIR}/pid" 2>/dev/null)
    print "pid        : ${_wpid:-none}"
    [[ -n "$_wpid" ]] && print "pid alive  : $(kill -0 "$_wpid" 2>/dev/null && echo YES || echo NO)"
    print "=== Completion Test ==="
    # Drain pty first
    local _d; for _d in {1..200}; do zpty -t _zsh_ios_worker 2>/dev/null || break; zpty -r _zsh_ios_worker _d 2>/dev/null; done
    local _req="${_ZSH_IOS_WORKER_DIR}/request"
    local _rf="${_ZSH_IOS_WORKER_DIR}/ping-result"
    local _df="${_rf}.done"
    rm -f "$_rf" "$_df" "$_req"
    cat > "$_req" <<EOF
_ZIO_RF='$_rf'
_ZIO_DF='$_df'
_ZIO_BUFFER='echo hello'
_ZIO_CURSOR=10
EOF
    zpty -w -n _zsh_ios_worker $'\n' 2>/dev/null
    print "zpty -w rc=$?"
    sleep 1.0
    print "Done file: $([[ -f "$_df" ]] && echo YES || echo NO)"
    print "Result: $(cat "$_rf" 2>/dev/null || echo 'no result')"
    rm -f "$_rf" "$_df" "$_req"
}

_zsh_ios_worker_status() {
    print "  worker_dir : $_ZSH_IOS_WORKER_DIR"
    print "  ready      : $(_zsh_ios_worker_is_ready && echo yes || echo no)"
    print "  pid        : $(cat "${_ZSH_IOS_WORKER_DIR}/pid" 2>/dev/null || echo none)"
    print "  starting   : $_ZSH_IOS_WORKER_STARTING"
    print "  zpty       : $(zpty -L 2>/dev/null | grep _zsh_ios_worker || echo none)"
}
alias zsh-ios-worker-status='_zsh_ios_worker_status'

_zsh_ios_worker_lazy_start() {
    _zsh_ios_worker_start
    add-zle-hook-widget -d line-init _zsh_ios_worker_lazy_start 2>/dev/null
}
autoload -Uz add-zle-hook-widget 2>/dev/null
add-zle-hook-widget line-init _zsh_ios_worker_lazy_start 2>/dev/null

# ─────────────────────────────────────────────────────────────────────────────
# END WORKER
# ─────────────────────────────────────────────────────────────────────────────

# --- ZLE Widget: ghost-text preview (hooked into line-pre-redraw) ---
# Shows the fully-resolved command as faint text after the cursor via
# POSTDISPLAY so it never touches BUFFER or CURSOR.
#
# region_highlight ranges are measured in *display columns* starting at
# column 0 of BUFFER.  To paint POSTDISPLAY we use the range
# [#BUFFER, #BUFFER + #POSTDISPLAY] — positions that are literally past
# the end of BUFFER, which zsh then resolves into POSTDISPLAY at render
# time.  (The `P0` prefix form is for predisplay, a different beast; using
# it here made the range mistakenly cover BUFFER + the start of POSTDISPLAY
# while the tail of POSTDISPLAY stayed unstyled.)
_zsh_ios_ghost_preview_widget() {
    # While another widget is in the middle of mutating BUFFER (e.g.
    # accept-line substituting the resolved form), we must not touch
    # POSTDISPLAY or region_highlight — the widget will finalize them.
    (( _zsh_ios_ghost_suspended )) && return 0

    # Skip during input bursts — paste, auto-type, or very fast typing.
    # $PENDING is zsh's count of bytes already read from the terminal but
    # not yet processed by ZLE. When it's nonzero we're behind on input,
    # so computing a ghost now would burn a subprocess per buffered key
    # for no visible gain (the next redraw will immediately replace it).
    # Leave any prior ghost visible so there's no flicker during paste.
    (( ${PENDING:-0} > 0 )) && return 0

    # Drop any previous ghost entry before adding a fresh one.
    if [[ -n "$_zsh_ios_ghost_last_highlight" ]]; then
        region_highlight=("${(@)region_highlight:#$_zsh_ios_ghost_last_highlight}")
        _zsh_ios_ghost_last_highlight=""
    fi
    POSTDISPLAY=""

    (( _zsh_ios_ghost_disabled )) && return 0
    _zsh_ios_is_disabled && return 0
    [[ -z "${BUFFER// /}" ]] && { _zsh_ios_ghost_last_buffer=""; return 0; }
    [[ "$BUFFER" == \!* ]] && return 0
    [[ "$BUFFER" == *$'\n'* ]] && return 0

    if [[ "$BUFFER" == "$_zsh_ios_ghost_last_buffer" ]]; then
        POSTDISPLAY="$_zsh_ios_ghost_last_postdisplay"
        if [[ -n "$POSTDISPLAY" ]]; then
            local _start=${#BUFFER}
            local _end=$(( _start + ${#POSTDISPLAY} ))
            _zsh_ios_ghost_last_highlight="$_start $_end $_zsh_ios_ghost_style"
            region_highlight+=("$_zsh_ios_ghost_last_highlight")
        fi
        return 0
    fi

    local resolved
    resolved=$("$ZSH_IOS_BIN" resolve -- "$BUFFER" 2>/dev/null)
    local rc=$?

    # Strip trailing whitespace from both sides before deciding whether the
    # resolved form is meaningfully different. Without this, typing "git
    # commit " (trailing space) produces resolved="git commit" which is
    # byte-different from BUFFER and spuriously ghosts the user's own input.
    local _ghost_b="$BUFFER"
    local _ghost_r="$resolved"
    while [[ "$_ghost_b" == *[[:space:]] ]]; do _ghost_b="${_ghost_b% }"; _ghost_b="${_ghost_b%$'\t'}"; done
    while [[ "$_ghost_r" == *[[:space:]] ]]; do _ghost_r="${_ghost_r% }"; _ghost_r="${_ghost_r%$'\t'}"; done

    if (( rc == 0 )) && [[ -n "$_ghost_r" && "$_ghost_r" != "$_ghost_b" ]]; then
        POSTDISPLAY="${_zsh_ios_ghost_prefix}${resolved}"
        local _start=${#BUFFER}
        local _end=$(( _start + ${#POSTDISPLAY} ))
        _zsh_ios_ghost_last_highlight="$_start $_end $_zsh_ios_ghost_style"
        region_highlight+=("$_zsh_ios_ghost_last_highlight")
    fi

    _zsh_ios_ghost_last_buffer="$BUFFER"
    _zsh_ios_ghost_last_postdisplay="$POSTDISPLAY"
}

add-zle-hook-widget line-pre-redraw _zsh_ios_ghost_preview_widget 2>/dev/null

# Reset the suspend flag at the start of every new line. The flag was
# set by accept-line / expand-or-complete to keep the ghost widget out
# of the way while they mutated BUFFER; once the new prompt is live,
# the flag must drop so the next line's ghost works normally.
_zsh_ios_ghost_line_init() {
    _zsh_ios_ghost_suspended=0
    _zsh_ios_ghost_last_buffer=""
    _zsh_ios_ghost_last_postdisplay=""
    _zsh_ios_ghost_last_highlight=""
}
add-zle-hook-widget line-init _zsh_ios_ghost_line_init 2>/dev/null

# --- Register widgets ---
zle -N _zsh_ios_accept_line
zle -N _zsh_ios_expand_or_complete
zle -N _zsh_ios_help

# --- Bind keys ---
bindkey '^M' _zsh_ios_accept_line          # Enter
bindkey '^I' _zsh_ios_expand_or_complete   # Tab
bindkey '?' _zsh_ios_help                  # ?
