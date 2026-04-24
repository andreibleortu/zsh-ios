#!/usr/bin/env bash
# Build all fuzz targets, start them, and watch for crashes/timeouts/leaks.
# Usage: ./fuzz/watch-crashes.sh
#
# Ctrl-C stops all fuzzers and exits cleanly.
# Prints an alert with reproduce + minimize commands when a new artifact
# appears.  Also prints a live exec/s summary from each fuzzer every minute.

set -euo pipefail

FUZZ_DIR="$(cd "$(dirname "$0")" && pwd)"
ARTIFACTS_DIR="$FUZZ_DIR/artifacts"
CORPUS_DIR="$FUZZ_DIR/corpus"

# Ordered list of (target, log) pairs — kept as parallel arrays so the status
# table always prints in a consistent order.
TARGETS=(
  fuzz_ingest
  fuzz_history
  fuzz_completions_parser
  fuzz_path_resolve
  fuzz_resolve
  fuzz_bash_completions
  fuzz_regex_args
  fuzz_trie_serde
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

FUZZ_PIDS=()

cleanup() {
    echo
    echo "Stopping fuzzers..."
    for pid in "${FUZZ_PIDS[@]}"; do
        kill "$pid" 2>/dev/null || true
    done
    wait 2>/dev/null || true
    echo "Done."
    exit 0
}
trap cleanup INT TERM

# ── Build ─────────────────────────────────────────────────────────────────────
echo "Building fuzz targets..."
(cd "$FUZZ_DIR" && cargo +nightly fuzz build 2>&1) || {
    echo "Build failed — aborting." >&2
    exit 1
}

# Locate the binary directory produced by cargo fuzz build.
BIN_DIR=$(dirname "$(find "$FUZZ_DIR/target" -name 'fuzz_ingest' -type f 2>/dev/null | head -1)")
if [[ -z "$BIN_DIR" || "$BIN_DIR" == "." ]]; then
    echo "Cannot find fuzz binaries under $FUZZ_DIR/target" >&2
    exit 1
fi

# ── Launch fuzzers directly (no cargo lock contention) ────────────────────────
# Match cargo fuzz run defaults: disable leak-sanitizer so only real crashes
# produce artifacts (leaks can be investigated separately).
export LSAN_OPTIONS="${LSAN_OPTIONS:-detect_leaks=0}"
export RUST_BACKTRACE=1

echo "Starting fuzzers from $BIN_DIR ..."
for i in "${!TARGETS[@]}"; do
    target="${TARGETS[$i]}"
    log="${LOGS[$i]}"
    mkdir -p "$ARTIFACTS_DIR/$target" "$CORPUS_DIR/$target"
    : > "$log"
    "$BIN_DIR/$target" \
        -artifact_prefix="$ARTIFACTS_DIR/$target/" \
        -max_len=4096 -timeout=5 \
        "$CORPUS_DIR/$target" \
        > "$log" 2>&1 &
    FUZZ_PIDS+=($!)
    echo "  started $target (pid $!, log $log)"
done
echo

# ── Monitor loop ─────────────────────────────────────────────────────────────
declare -A seen

print_status() {
    echo "──── $(date '+%H:%M:%S') ────────────────────────────────────────────"
    for i in "${!TARGETS[@]}"; do
        target="${TARGETS[$i]}"
        log="${LOGS[$i]}"
        [[ -f "$log" ]] || continue
        execs=$(grep -oP '#\K[0-9]+' "$log" 2>/dev/null | tail -1 || true)
        speed=$(grep -oP 'exec/s: \K[0-9]+' "$log" 2>/dev/null | tail -1 || true)
        cov=$(grep -oP 'cov: \K[0-9]+' "$log" 2>/dev/null | tail -1 || true)
        printf "  %-28s execs=%-12s exec/s=%-8s cov=%s\n" \
            "$target" "${execs:--}" "${speed:--}" "${cov:--}"
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
