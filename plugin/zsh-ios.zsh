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
        ("$ZSH_IOS_BIN" learn --exit-code "$ec" -- "$_zsh_ios_pending_cmd" &>/dev/null &)
    fi
    unset _zsh_ios_pending_cmd
    unset _zsh_ios_last_pin
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

# --- ZLE Widget: Enter key (resolve + execute) ---
_zsh_ios_accept_line() {
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

    local output exit_code
    output=$("$ZSH_IOS_BIN" resolve -- "$BUFFER" 2>/dev/null)
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

    local output exit_code
    output=$("$ZSH_IOS_BIN" resolve -- "$BUFFER" 2>/dev/null)
    exit_code=$?

    case $exit_code in
        0)
            _zsh_ios_last_tab_buffer=""
            if [[ "$output" != "$BUFFER" ]]; then
                BUFFER="$output"
                CURSOR=${#BUFFER}
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
        local msg="% Ambiguous command: \"$_zio_word\""
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
    echo "% Ambiguous path:"
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

# --- ZLE Widget: ? key (show completions) ---
# Cisco IOS behavior:
#   "show ?" (space before ?) = what arguments/subcommands come after "show"
#   "sh?"    (no space)       = what commands match the "sh" prefix
#
# When the Rust binary cannot provide completions (returns a generic
# "no completions" signal), we fall back to asking the ZLE worker —
# a background Zsh process with the full completion system loaded.
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
    local output
    output=$("$ZSH_IOS_BIN" complete -- "$prefix" 2>/dev/null)

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
            # Format items into columns (simple, matches the Rust binary style)
            local -a items=("${(@f)worker_items}")
            # Deduplicate and sort
            items=("${(@u)items}")
            local col_output
            # Build a simple two-column layout at 80 chars
            local max_w=0 item
            for item in "${items[@]}"; do
                (( ${#item} > max_w )) && max_w=${#item}
            done
            local col_w=$(( max_w + 2 ))
            local cols=$(( 80 / col_w ))
            (( cols < 1 )) && cols=1
            local line="" col_n=0
            col_output=""
            for item in "${items[@]}"; do
                (( col_n == cols )) && { col_output+="${line}"$'\n'; line=""; col_n=0 }
                line+="$(printf "  %-${max_w}s" "$item")"
                (( col_n++ ))
            done
            [[ -n "$line" ]] && col_output+="${line}"$'\n'
            output="  Expects: <argument> [ZLE]\n${col_output}"
        fi
    fi

    if [[ -n "$output" ]]; then
        zle -M "$output"
    else
        zle -M "  No commands found"
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

    echo ""
    echo "% Ambiguous command: \"$abbrev_str\""
    echo "  Pick a number to save as shorthand (Enter to cancel):"
    local i=1
    for item in "${menu_display[@]}"; do
        echo "    $i) $item"
        (( i++ ))
    done

    echo -n "  > "

    # Keystroke-by-keystroke picker. Three input modes, freely intermixed:
    #   • Digits: accept as soon as the buffered digits uniquely identify an
    #     option (no Enter needed). `5` fires instantly in a 5-option menu;
    #     `1` in a 20-option menu waits for a second digit because 10-19 are
    #     still reachable; `13` fires instantly.
    #   • Tab: advance a cycle highlight through the options (wraps). The
    #     prompt line redraws to `> [N] <choice>`. Enter or another Tab-pick
    #     commits. Useful when the user wants to eyeball options rather than
    #     map digits to positions.
    #   • Enter on empty: cancel. Enter while cycling: commit the highlight.
    #     Any other key cancels.
    local choice=""
    local cycle_idx=0
    local max=${#menu_display}
    local key trial extendable k sk
    # Redraw the `> ` prompt line showing the current cycle highlight (or
    # empty if cycle_idx==0). \r returns to column 0; \e[K clears to EOL.
    _zsh_ios_pick_redraw_cycle() {
        if (( cycle_idx == 0 )); then
            printf '\r  > \e[K'
        else
            printf '\r  > [%d] %s\e[K' "$cycle_idx" "${menu_display[$cycle_idx]}"
        fi
    }
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
            $'\x7f'|$'\b')
                # Backspace: erase one digit, or step back one cycle position.
                if [[ -n "$choice" ]]; then
                    choice="${choice%?}"
                    echo -n $'\b \b'
                elif (( cycle_idx > 0 )); then
                    (( cycle_idx-- ))
                    _zsh_ios_pick_redraw_cycle
                fi
                ;;
            [0-9])
                # Switching from cycle → digits: clear the highlight display.
                if (( cycle_idx > 0 )); then
                    cycle_idx=0
                    _zsh_ios_pick_redraw_cycle
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
                    echo -n "$key"
                    if (( !extendable )); then
                        echo ""
                        break
                    fi
                fi
                ;;
            *)
                # Any non-digit, non-enter, non-backspace, non-tab cancels.
                echo ""
                choice=""
                cycle_idx=0
                break
                ;;
        esac
    done
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

    # Pin the full abbreviated sequence -> full expansion
    # abbrev = all typed words up to and including the ambiguous word
    # expanded = the full resolved command (resolved_prefix + selected)
    local pin_expanded="$selected_display"

    "$ZSH_IOS_BIN" pin "$abbrev_str" --to "$pin_expanded" 2>/dev/null
    local display_path="${_zio_pins_path/#$HOME/~}"
    echo "  Saved: \"$abbrev_str\" → \"$pin_expanded\""
    echo "  In $display_path"
    _zsh_ios_last_pin="$abbrev_str"

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

# Override accept-line: when a request file exists, run completion instead of
# accepting input.  The parent triggers this by sending a newline via zpty -w.
_zio_accept_line() {
    local _req="${_ZSH_IOS_WORKER_DIR}/request"
    if [[ -f "\$_req" ]]; then
        source "\$_req"
        BUFFER="\$_ZIO_BUFFER"
        CURSOR="\${_ZIO_CURSOR:-\${#BUFFER}}"
        : > "\$_ZIO_RF"
        zle complete-word 2>/dev/null
        BUFFER=""
        CURSOR=0
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

# --- Register widgets ---
zle -N _zsh_ios_accept_line
zle -N _zsh_ios_expand_or_complete
zle -N _zsh_ios_help

# --- Bind keys ---
bindkey '^M' _zsh_ios_accept_line          # Enter
bindkey '^I' _zsh_ios_expand_or_complete   # Tab
bindkey '?' _zsh_ios_help                  # ?
