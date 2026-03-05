#!/usr/bin/env bash
# bench_big_daddy.sh — Long‑running “big daddy” benchmark (single mixed workload).
#
# Goal:
#   Heavier, single **mixed** workload run over all policy variants, writing to
#   its own CSV so it does not interfere with the regular benchmark paths.
#
# Usage:
#   ./scripts/bench_big_daddy.sh                         # output: big_daddy_results.csv
#   ./scripts/bench_big_daddy.sh my_big_results.csv      # custom output file
#   RUNS=1 ./scripts/bench_big_daddy.sh                  # 1 run by default (single trace per policy)
#   RUNS=3 ./scripts/bench_big_daddy.sh                  # 3 independent traces per policy
#   VERBOSE=1 ./scripts/bench_big_daddy.sh               # show stderr from each run
#
# Output CSV columns:
#   run, <standard tiering_bench columns>
#
# Compared to bench_full.sh:
#   - Uses a single, heavier mixed workload configuration
#   - Separate default CSV path (big_daddy_results.csv)

set -euo pipefail
cd "$(dirname "$0")/.."

CSV_OUT="${1:-big_daddy_results.csv}"
RUNS="${RUNS:-1}"
VERBOSE="${VERBOSE:-}"
BIN="./target/release/tiering_bench"

# ── Build ──────────────────────────────────────────────────────────────────────
echo "Building tiering_bench (release)..."
cargo build --release --bin tiering_bench 2>&1 \
  | grep -E "^(error|warning\[|Compiling|Finished)" || true
echo ""

# ── Policy variants ───────────────────────────────────────────────────────────
# Each name is recognized by make_policy. Variants (e.g. lru_2q_small) have
# pre-baked param defaults; --policy-param overrides always win over those defaults.
POLICIES=(
    basic_lru
    arc
    lfu

    lru_2q           # a1in_fraction=0.25  (paper default; 25% probationary / 75% protected)
    lru_2q_small     # a1in_fraction=0.10  (Am-dominant; best for stable small hot sets)
    lru_2q_large     # a1in_fraction=0.50  (large A1in; better scan / high-churn resistance)

    lecar            # learning_rate=0.45, w_lru=0.5  (paper defaults; balanced start)
    lecar_fast       # learning_rate=0.90              (aggressive adaptation to ghost hits)
    lecar_slow       # learning_rate=0.05              (conservative; stable workloads)

    cacheus          # lr_init=0.45, w_sr_lru=0.5  (paper defaults; balanced expert weights)
    cacheus_lru_biased  # w_sr_lru=0.80  (leans SR-LRU; recency-heavy workloads)
    cacheus_lfu_biased  # w_sr_lru=0.20  (leans CR-LFU; frequency-heavy workloads)

    decision_tree       # retrain_interval=50, tree_max_depth=4  (defaults)
    decision_tree_deep  # tree_max_depth=8  (richer splits; more overfitting risk)
    decision_tree_fast  # retrain_interval=10  (retrains 5× more often; faster adaptation)
)

N_POLICIES=${#POLICIES[@]}
N_TOTAL=$(( N_POLICIES * RUNS ))

# ── CSV header ─────────────────────────────────────────────────────────────────
# Prepend "run" column to the standard tiering_bench header.
{ printf "run,"; "$BIN" --header 2>/dev/null; } > "$CSV_OUT"

echo "Output  → $CSV_OUT"
echo "Configs : ${N_POLICIES} policies × ${RUNS} run(s) = ${N_TOTAL} rows"
echo ""

# ── run_big_daddy ─────────────────────────────────────────────────────────────
# Iterates RUNS × POLICIES for a single mixed workload and appends one CSV row per combination.
# Failures are reported but do not abort the script.
run_big_daddy() {
    local -a args=("$@")

    echo "=== big_daddy_mixed (${N_POLICIES} policies × ${RUNS} run(s)) ==="
    local t0; t0=$(date +%s)

    for run in $(seq 1 "$RUNS"); do
        for policy in "${POLICIES[@]}"; do
            printf "  [run %d] %-26s ... " "$run" "$policy"
            local p0; p0=$(date +%s)

            local stderr_dest="/dev/null"
            [ -n "$VERBOSE" ] && stderr_dest="/dev/stderr"

            local row
            if row=$("$BIN" --csv --policy "$policy" "${args[@]}" 2>"$stderr_dest"); then
                # Prepend run to the CSV row and append to output file.
                printf "%d,%s\n" "$run" "$row" >> "$CSV_OUT"
                printf "%ds\n" "$(( $(date +%s) - p0 ))"
            else
                printf "FAILED\n"
            fi
        done
    done

    printf "  big_daddy total: %ds\n\n" "$(( $(date +%s) - t0 ))"
}

OVERALL=$(date +%s)

# ── Single mixed workload (long run) ──────────────────────────────────────────
#
# Heavier, mixed access pattern intended to exercise:
# - some stable hot set (frequency signal)
# - a steady stream of new files and deletes (recency + churn)
# - moderate skew so both recency- and frequency-aware policies have signal.
#
# Assumptions (documented, can be tweaked later):
# - depth=3 tiers, hot_capacity ~= 20K bytes
# - file sizes 256–2048 bytes
# - 20% creates, 10% deletes, 70% edits
# - skew=0.7  → recency bias but not extreme

run_big_daddy \
    --warmup-ops        20000 \
    --measure-ops      100000 \
    --poll-interval-ops   100 \
    --depth                 3 \
    --hot-capacity     20000 \
    --min-file-size       256 \
    --max-file-size      2048 \
    --create-pct           20 \
    --delete-pct           10 \
    --edit-pct             70 \
    --skew                0.7

# ── Summary ────────────────────────────────────────────────────────────────────
TOTAL_ROWS=$(( $(wc -l < "$CSV_OUT") - 1 ))
echo "────────────────────────────────────────────────────────"
printf "Done.  %d / %d rows written to %s\n" "$TOTAL_ROWS" "$N_TOTAL" "$CSV_OUT"
printf "Total elapsed: %ds\n" "$(( $(date +%s) - OVERALL ))"

