#!/usr/bin/env bash
# Watch all fuzz artifact directories for new crash files and print an alert.
# Run this in a terminal while fuzzing: ./fuzz/watch-crashes.sh
#
# It also prints a live exec/s summary from each fuzzer log every minute.

ARTIFACTS_DIR="$(cd "$(dirname "$0")" && pwd)/artifacts"
LOGS=(/tmp/fuzz-ingest.log /tmp/fuzz-history.log /tmp/fuzz-completions.log /tmp/fuzz-path.log)

echo "Watching $ARTIFACTS_DIR for crashes..."
echo "Fuzzer logs: ${LOGS[*]}"
echo

declare -A seen

print_status() {
    echo "──── $(date '+%H:%M:%S') ────────────────────────────────"
    for log in "${LOGS[@]}"; do
        name=$(basename "$log" .log)
        if [[ -f "$log" ]]; then
            last=$(grep -oP 'exec/s: \K[0-9]+' "$log" 2>/dev/null | tail -1)
            execs=$(grep -oP '#\K[0-9]+' "$log" 2>/dev/null | tail -1)
            cov=$(grep -oP 'cov: \K[0-9]+' "$log" 2>/dev/null | tail -1)
            echo "  $name: ${execs:-?} execs, cov=${cov:-?}, exec/s=${last:-?}"
        fi
    done
}

print_status
tick=0
while true; do
    sleep 5
    tick=$((tick + 1))

    # Check for new crashes
    for f in "$ARTIFACTS_DIR"/*/crash-* "$ARTIFACTS_DIR"/*/timeout-* "$ARTIFACTS_DIR"/*/leak-* 2>/dev/null; do
        [[ -f "$f" ]] || continue
        key=$(basename "$f")
        if [[ -z "${seen[$key]}" ]]; then
            seen[$key]=1
            target=$(basename "$(dirname "$f")")
            echo
            echo "!!! CRASH FOUND in $target: $f"
            echo "    Reproduce: cargo +nightly fuzz run $target $f"
            echo "    Minimize:  cargo +nightly fuzz tmin $target $f"
        fi
    done

    # Print exec/s summary once per minute (every 12 × 5s ticks)
    if (( tick % 12 == 0 )); then
        echo
        print_status
    fi
done
