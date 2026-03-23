#!/usr/bin/env zsh
# zsh-ios: Cisco IOS-style command abbreviation engine for Zsh

# --- Configuration ---
ZSH_IOS_BIN="${ZSH_IOS_BIN:-zsh-ios}"
ZSH_IOS_CONFIG_DIR="${XDG_CONFIG_HOME:-$HOME/.config}/zsh-ios"

# --- Guard: check if binary exists ---
if ! command -v "$ZSH_IOS_BIN" &>/dev/null; then
    echo "zsh-ios: binary not found in PATH. Run install.sh or cargo install --path ." >&2
    return 1
fi

# --- Background tree build on first load ---
_zsh_ios_build_if_stale() {
    local tree_file
    tree_file=$("$ZSH_IOS_BIN" status 2>/dev/null | grep 'Tree file:' | sed 's/.*Tree file:  *//')
    [[ -z "$tree_file" ]] && return

    local rebuild=0
    if [[ ! -f "$tree_file" ]]; then
        rebuild=1
    else
        local now=$(date +%s)
        local mtime=$(stat -f %m "$tree_file" 2>/dev/null || echo 0)
        if (( now - mtime > 3600 )); then
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

_zsh_ios_precmd() {
    local ec=$?
    if [[ $ec -eq 0 && -n "$_zsh_ios_pending_cmd" ]]; then
        ("$ZSH_IOS_BIN" learn -- "$_zsh_ios_pending_cmd" &>/dev/null &)
    fi
    unset _zsh_ios_pending_cmd
}

autoload -Uz add-zsh-hook
add-zsh-hook preexec _zsh_ios_preexec
add-zsh-hook precmd _zsh_ios_precmd

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

    if [[ "$BUFFER" == \#* ]]; then
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
            _zsh_ios_handle_ambiguity "$output"
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

    local output exit_code
    output=$("$ZSH_IOS_BIN" resolve -- "$BUFFER" 2>/dev/null)
    exit_code=$?

    case $exit_code in
        0)
            if [[ "$output" != "$BUFFER" ]]; then
                BUFFER="$output"
                CURSOR=${#BUFFER}
            else
                zle expand-or-complete
            fi
            ;;
        1)
            # Ambiguous -- parse shell vars from Rust output
            local _zio_word _zio_lcp _zio_position _zio_resolved_prefix _zio_remaining
            local -a _zio_candidates _zio_deep_display _zio_deep_items
            eval "$output"

            # Expand buffer to LCP if longer than what was typed
            if [[ -n "$_zio_lcp" && "$_zio_lcp" != "$_zio_word" ]]; then
                if [[ -n "$_zio_resolved_prefix" ]]; then
                    BUFFER="$_zio_resolved_prefix $_zio_lcp"
                else
                    BUFFER="$_zio_lcp"
                fi
                CURSOR=${#BUFFER}
            fi

            # Show candidates
            if (( ${#_zio_candidates} > 0 )); then
                local msg="% Ambiguous command: \"$_zio_word\""
                local c
                for c in "${_zio_candidates[@]}"; do
                    msg+=$'\n'"  $c"
                done
                zle -M "$msg"
            fi
            ;;
        3)
            _zsh_ios_handle_path_ambiguity "$output" expand
            ;;
        *)
            zle expand-or-complete
            ;;
    esac
}

# --- Path ambiguity handler: single-keypress selection ---
_zsh_ios_handle_path_ambiguity() {
    local -a _zio_path_candidates
    eval "$1"
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
        read -r -k 1 key </dev/tty
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
        read -r choice </dev/tty

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
_zsh_ios_help() {
    if _zsh_ios_is_disabled; then
        zle self-insert
        return
    fi

    if (( CURSOR == 0 )) || [[ "${BUFFER[$CURSOR]}" == " " ]]; then
        local prefix="${BUFFER[1,CURSOR]}"
        prefix="${prefix%% }"
        local output
        output=$("$ZSH_IOS_BIN" complete -- "$prefix" 2>/dev/null)
        if [[ -n "$output" ]]; then
            zle -M "$output"
        else
            zle -M "  No commands found"
        fi
    else
        zle self-insert
    fi
}

# --- Ambiguity handler with interactive clarifier ---
_zsh_ios_handle_ambiguity() {
    local _zio_word _zio_lcp _zio_position _zio_resolved_prefix _zio_remaining _zio_pins_path
    local -a _zio_candidates _zio_deep_display _zio_deep_items
    eval "$1"

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
    local cancel_hint
    (( ${#menu_display} <= 9 )) && cancel_hint="any other key" || cancel_hint="Enter"
    echo "  Pick a number to save as shorthand ($cancel_hint to cancel):"
    local i=1
    for item in "${menu_display[@]}"; do
        echo "    $i) $item"
        (( i++ ))
    done

    local choice
    echo -n "  > "
    if (( ${#menu_display} <= 9 )); then
        read -r -k 1 choice </dev/tty
        echo ""
    else
        read -r choice </dev/tty
    fi

    if [[ -z "$choice" || "$choice" == $'\n' ]]; then
        zle reset-prompt
        return
    fi

    local selected_display selected_expanded
    if [[ "$choice" =~ ^[0-9]+$ ]] && (( choice >= 1 && choice <= ${#menu_display} )); then
        selected_display="${menu_display[$choice]}"
        selected_expanded="${menu_expanded[$choice]}"
    else
        local idx=1
        for item in "${menu_display[@]}"; do
            if [[ "$item" == "$choice"* ]]; then
                selected_display="$item"
                selected_expanded="${menu_expanded[$idx]}"
                break
            fi
            (( idx++ ))
        done
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
    echo -n "  (U to undo) "
    local undo_key
    read -r -k 1 -t 4 undo_key </dev/tty
    echo ""
    if [[ "$undo_key" == "u" || "$undo_key" == "U" ]]; then
        "$ZSH_IOS_BIN" unpin "$abbrev_str" 2>/dev/null
        echo "  Unpinned: \"$abbrev_str\""
        zle reset-prompt
        return
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
    zle accept-line
}

# --- Register widgets ---
zle -N _zsh_ios_accept_line
zle -N _zsh_ios_expand_or_complete
zle -N _zsh_ios_help

# --- Bind keys ---
bindkey '^M' _zsh_ios_accept_line          # Enter
bindkey '^I' _zsh_ios_expand_or_complete   # Tab
bindkey '?' _zsh_ios_help                  # ?
