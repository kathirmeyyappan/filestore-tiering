#!/usr/bin/env bash
# bench_lru2q_sweep.sh — Sweep LRU-2Q's a1in_fraction across presets → CSV.
#
# Usage:
#   ./scripts/bench_lru2q_sweep.sh                    # output: lru2q_sweep.csv
#   ./scripts/bench_lru2q_sweep.sh my_sweep.csv       # custom output file
#   RUNS=3 ./scripts/bench_lru2q_sweep.sh             # 3 traces per (preset, a1in_fraction)
#   VERBOSE=1 ./scripts/bench_lru2q_sweep.sh          # show stderr from each run
#
# Output CSV columns:
#   preset, run, a1in_fraction, <standard tiering_bench columns>
#
# Only the lru_2q policy is run; we vary a1in_fraction in [0.05, 0.50].

set -euo pipefail
cd "$(dirname "$0")/.."

CSV_OUT="${1:-lru2q_sweep.csv}"
RUNS="${RUNS:-1}"
VERBOSE="${VERBOSE:-}"
BIN="./target/release/tiering_bench"

# Values to sweep for a1in_fraction (probationary A1in byte fraction).
# 0.05 → very small A1in; 0.50 → half of hot reserved for probationary queue.
A1IN_VALUES=(
  0.05
  0.10
  0.15
  0.20
  0.25
  0.30
  0.35
  0.40
  0.45
  0.50
)

PRESETS=(steady_state frequency_favored recency_favored high_churn hot_set)

N_PRESETS=${#PRESETS[@]}
N_A1IN=${#A1IN_VALUES[@]}
N_TOTAL=$(( N_PRESETS * N_A1IN * RUNS ))

echo "Building tiering_bench (release)..."
cargo build --release --bin tiering_bench 2>&1 \
  | grep -E "^(error|warning\[|Compiling|Finished)" || true
echo ""

# ── CSV header ─────────────────────────────────────────────────────────────────
# Prepend "preset,run,a1in_fraction" to the standard tiering_bench header.
{ printf "preset,run,a1in_fraction,"; "$BIN" --header 2>/dev/null; } > "$CSV_OUT"

echo "Output  → $CSV_OUT"
echo "Configs : ${N_PRESETS} presets × ${N_A1IN} a1in_fraction values × ${RUNS} run(s) = ${N_TOTAL} rows"
echo ""

run_case() {
  local preset="$1"
  local warmup_ops="$2"
  local measure_ops="$3"
  local poll_interval_ops="$4"
  local depth="$5"
  local hot_capacity="$6"
  local min_file_size="$7"
  local max_file_size="$8"
  local create_pct="$9"
  local delete_pct="${10}"
  local edit_pct="${11}"
  local skew="${12}"

  echo "=== ${preset} (lru_2q sweep over a1in_fraction) ==="
  local t0; t0=$(date +%s)

  for run in $(seq 1 "$RUNS"); do
    for a1 in "${A1IN_VALUES[@]}"; do
      printf "  [run %d] a1in_fraction=%-4s ... " "$run" "$a1"
      local p0; p0=$(date +%s)

      local stderr_dest="/dev/null"
      [ -n "$VERBOSE" ] && stderr_dest="/dev/stderr"

      local row
      if row=$("$BIN" \
          --csv \
          --policy lru_2q \
          --policy-param "a1in_fraction=${a1}" \
          --warmup-ops "$warmup_ops" \
          --measure-ops "$measure_ops" \
          --poll-interval-ops "$poll_interval_ops" \
          --depth "$depth" \
          --hot-capacity "$hot_capacity" \
          --min-file-size "$min_file_size" \
          --max-file-size "$max_file_size" \
          --create-pct "$create_pct" \
          --delete-pct "$delete_pct" \
          --edit-pct "$edit_pct" \
          --skew "$skew" \
          2>"$stderr_dest"); then
        printf "%s,%d,%s,%s\n" "$preset" "$run" "$a1" "$row" >> "$CSV_OUT"
        printf "%ds\n" "$(( $(date +%s) - p0 ))"
      else
        printf "FAILED\n"
      fi
    done
  done

  printf "  preset total: %ds\n\n" "$(( $(date +%s) - t0 ))"
}

OVERALL=$(date +%s)

# steady_state
run_case steady_state \
  1000 5000 50 \
  3 20000 \
  256 2048 \
  10 5 85 1.0

# frequency_favored
run_case frequency_favored \
  2000 10000 50 \
  3 4000 \
  256 768 \
  3 0 97 5.0

# recency_favored (new sharp recency skew)
run_case recency_favored \
  2000 10000 50 \
  3 4000 \
  128 512 \
  8 0 92 0.05

# high_churn (mild recency skew)
run_case high_churn \
  1000 5000 50 \
  3 20000 \
  256 2048 \
  30 20 50 0.35

# hot_set
run_case hot_set \
  1000 5000 50 \
  3 20000 \
  256 2048 \
  10 5 85 3.0

TOTAL_ROWS=$(( $(wc -l < "$CSV_OUT") - 1 ))
echo "────────────────────────────────────────────────────────"
printf "Done.  %d / %d rows written to %s\n" "$TOTAL_ROWS" "$N_TOTAL" "$CSV_OUT"
printf "Total elapsed: %ds\n" "$(( $(date +%s) - OVERALL ))"

