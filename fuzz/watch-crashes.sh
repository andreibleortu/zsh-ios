#!/usr/bin/env bash
# Start all fuzz targets and watch for crashes/timeouts/leaks.
# Usage: ./fuzz/watch-crashes.sh
#
# Ctrl-C stops all fuzzers and exits cleanly.
# Prints an alert with reproduce + minimize commands when a new artifact
# appears.  Also prints a live exec/s summary from each fuzzer every minute.

set -euo pipefail

FUZZ_DIR="$(cd "$(dirname "$0")" && pwd)"
ARTIFACTS_DIR="$FUZZ_DIR/artifacts"

# target-name → log-file (hyphens for original four, underscores for new four)
declare -A TARGETS=(
  [fuzz_ingest]=/tmp/fuzz-ingest.log
  [fuzz_history]=/tmp/fuzz-history.log
  [fuzz_completions_parser]=/tmp/fuzz-completions.log
  [fuzz_path_resolve]=/tmp/fuzz-path.log
  [fuzz_resolve]=/tmp/fuzz_resolve.log
  [fuzz_bash_completions]=/tmp/fuzz_bash_completions.log
  [fuzz_regex_args]=/tmp/fuzz_regex_args.log
  [fuzz_trie_serde]=/tmp/fuzz_trie_serde.log
)

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

# PIDs of background fuzz processes, collected during startup.
FUZZ_PIDS=()

cleanup() {
    echo
    echo "Stopping fuzzers..."
    for pid in "${FUZZ_PIDS[@]}"; do
        kill "$pid" 2>/dev/null || true
    done
    # Wait briefly so nohup children also get the signal.
    wait 2>/dev/null || true
    echo "Done."
    exit 0
}
trap cleanup INT TERM

# ── Build first ──────────────────────────────────────────────────────────────
echo "Building fuzz targets..."
(cd "$FUZZ_DIR" && cargo +nightly fuzz build 2>&1) || {
    echo "Build failed — aborting." >&2
    exit 1
}

# ── Launch fuzzers ────────────────────────────────────────────────────────────
echo "Starting fuzzers..."
for target in "${!TARGETS[@]}"; do
    log="${TARGETS[$target]}"
    # Truncate old log so print_status doesn't show stale numbers.
    : > "$log"
    cargo +nightly fuzz run "$target" -- -max_len=4096 -timeout=5 \
        > "$log" 2>&1 &
    FUZZ_PIDS+=($!)
    echo "  started $target (pid $!, log $log)"
done
echo

# ── Monitor loop ─────────────────────────────────────────────────────────────
declare -A seen

print_status() {
    echo "──── $(date '+%H:%M:%S') ────────────────────────────────────────────"
    for log in "${LOGS[@]}"; do
        [[ -f "$log" ]] || continue
        name=$(basename "$log" .log | sed 's/^fuzz[-_]//')
        execs=$(grep -oP '#\K[0-9]+' "$log" 2>/dev/null | tail -1 || true)
        speed=$(grep -oP 'exec/s: \K[0-9]+' "$log" 2>/dev/null | tail -1 || true)
        cov=$(grep -oP 'cov: \K[0-9]+' "$log" 2>/dev/null | tail -1 || true)
        printf "  %-28s execs=%-12s exec/s=%-8s cov=%s\n" \
            "$name" "${execs:--}" "${speed:--}" "${cov:--}"
    done
}

echo "Watching $ARTIFACTS_DIR for crashes... (Ctrl-C to stop)"
print_status
tick=0

while true; do
    sleep 5
    tick=$((tick + 1))

    # Check for new crash/timeout/leak artifacts.
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
