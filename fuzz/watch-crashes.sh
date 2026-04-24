#!/usr/bin/env bash
# Watch all fuzz artifact directories for new crash/timeout/leak files.
# Run this in a terminal while fuzzing: ./fuzz/watch-crashes.sh
#
# Prints an alert with reproduce + minimize commands when a new artifact
# appears.  Also prints a live exec/s summary from each fuzzer every minute.

set -euo pipefail

ARTIFACTS_DIR="$(cd "$(dirname "$0")" && pwd)/artifacts"

# All 8 fuzzer logs — hyphens for the original four, underscores for the new four.
LOGS=(
  /tmp/fuzz-ingest.log
  /tmp/fuzz-history.log
  /tmp/fuzz-completions.log
  /tmp/fuzz-path.log
  /tmp/fuzz_resolve.log
  /tmp/fuzz_bash_completions.log
  /tmp/fuzz_regex_args.log
  /tmp/fuzz_trie_serde.log
)

echo "Watching $ARTIFACTS_DIR for crashes..."
echo

declare -A seen

print_status() {
    echo "──── $(date '+%H:%M:%S') ────────────────────────────────────────────"
    for log in "${LOGS[@]}"; do
        [[ -f "$log" ]] || continue
        name=$(basename "$log" .log | sed 's/^fuzz[-_]//')
        execs=$(grep -oP '#\K[0-9]+' "$log" 2>/dev/null | tail -1)
        speed=$(grep -oP 'exec/s: \K[0-9]+' "$log" 2>/dev/null | tail -1)
        cov=$(grep -oP 'cov: \K[0-9]+' "$log" 2>/dev/null | tail -1)
        printf "  %-28s execs=%-12s exec/s=%-8s cov=%s\n" \
            "$name" "${execs:--}" "${speed:--}" "${cov:--}"
    done
}

print_status
tick=0

while true; do
    sleep 5
    tick=$((tick + 1))

    # Check for new crash/timeout/leak artifacts.
    # Use find to avoid glob-expansion errors when directories don't exist yet.
    while IFS= read -r f; do
        [[ -f "$f" ]] || continue
        key=$(basename "$f")
        if [[ -z "${seen[$key]+x}" ]]; then
            seen[$key]=1
            target=$(basename "$(dirname "$f")")
            kind=$(echo "$key" | grep -oP '^(crash|timeout|leak)' || echo "artifact")
            echo
            echo "!!! $kind in $target"
            echo "    file:      $f"
            echo "    reproduce: cargo +nightly fuzz run $target $f"
            echo "    minimize:  cargo +nightly fuzz tmin $target $f"
        fi
    done < <(find "$ARTIFACTS_DIR" \
        \( -name 'crash-*' -o -name 'timeout-*' -o -name 'leak-*' \) \
        2>/dev/null)

    # Print exec/s summary once per minute (every 12 × 5s ticks).
    if (( tick % 12 == 0 )); then
        echo
        print_status
    fi
done
