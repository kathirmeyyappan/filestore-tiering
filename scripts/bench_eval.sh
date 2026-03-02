#!/usr/bin/env bash
# Run benchmark across policies and parameter sets; print results as a table.
# Usage:
#   ./scripts/bench_eval.sh
#   POLICIES="basic_lru dummy" MEASURE_SEC=20 ./scripts/bench_eval.sh
#   HOT_CAP=5K MEASURE_SEC=30 ./scripts/bench_eval.sh   # smaller hot cap → more evictions

set -e
cd "$(dirname "$0")/.."

# Defaults (override with env)
# Hardcode known policies; override with POLICIES env if needed.
POLICIES="${POLICIES:-basic_lru arc}"
WARMUP_SEC="${WARMUP_SEC:-5}"
MEASURE_SEC="${MEASURE_SEC:-30}"
POLL_INTERVAL_SEC="${POLL_INTERVAL_SEC:-0.2}"
HOT_CAP="${HOT_CAP:-20K}"
DEPTH="${DEPTH:-3}"
CREATE_PCT="${CREATE_PCT:-50}"
DELETE_PCT="${DELETE_PCT:-0}"
EDIT_PCT="${EDIT_PCT:-50}"
BATCH_SIZE="${BATCH_SIZE:-1}"
# Output CSV file (header + one row per policy)
CSV_OUT="${CSV_OUT:-bench_results.csv}"

# Build once
cargo build --release --bin tiering_bench 2>&1 | tail -1

# Run header + all policies, capture raw CSV, pretty-print as table.
# By default we silence bench stderr; set VERBOSE=1 to see full logs.
if [ -n "${VERBOSE:-}" ]; then
  (
    cargo run --release --bin tiering_bench -- --header
    for policy in $POLICIES; do
      cargo run --release --bin tiering_bench -- --csv \
        --policy "$policy" \
        --warmup-sec "$WARMUP_SEC" \
        --measure-sec "$MEASURE_SEC" \
        --poll-interval-sec "$POLL_INTERVAL_SEC" \
        --depth "$DEPTH" \
        --hot-capacity "$HOT_CAP" \
        --create-pct "$CREATE_PCT" \
        --delete-pct "$DELETE_PCT" \
        --edit-pct "$EDIT_PCT" \
        --batch-size "$BATCH_SIZE"
    done
  ) | tee "$CSV_OUT" | column -t -s,
else
  (
    cargo run --release --bin tiering_bench -- --header 2>/dev/null
    for policy in $POLICIES; do
      cargo run --release --bin tiering_bench -- --csv \
        --policy "$policy" \
        --warmup-sec "$WARMUP_SEC" \
        --measure-sec "$MEASURE_SEC" \
        --poll-interval-sec "$POLL_INTERVAL_SEC" \
        --depth "$DEPTH" \
        --hot-capacity "$HOT_CAP" \
        --create-pct "$CREATE_PCT" \
        --delete-pct "$DELETE_PCT" \
        --edit-pct "$EDIT_PCT" \
        --batch-size "$BATCH_SIZE" \
        2>/dev/null
    done
  ) | tee "$CSV_OUT" | column -t -s,
fi

echo ""
echo "Params: warmup=${WARMUP_SEC}s measure=${MEASURE_SEC}s poll_interval=${POLL_INTERVAL_SEC}s depth=$DEPTH hot_cap=$HOT_CAP create=$CREATE_PCT% delete=$DELETE_PCT% edit=$EDIT_PCT%"
