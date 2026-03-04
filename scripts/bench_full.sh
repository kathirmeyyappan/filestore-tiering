#!/usr/bin/env bash
# bench_full.sh — Exhaustive benchmark: all presets × all policy variants → CSV.
#
# Usage:
#   ./scripts/bench_full.sh                        # output: bench_results.csv
#   ./scripts/bench_full.sh my_results.csv         # custom output file
#   RUNS=3 ./scripts/bench_full.sh                 # 3 independent runs per config
#   VERBOSE=1 ./scripts/bench_full.sh              # show stderr from each run
#
# Output CSV columns:
#   preset, run, <standard tiering_bench columns>
#
# Each (preset, policy, run) combination is one row. RUNS > 1 gives multiple
# independent traces per config for downstream averaging.

set -euo pipefail
cd "$(dirname "$0")/.."

CSV_OUT="${1:-bench_results.csv}"
RUNS="${RUNS:-1}"
VERBOSE="${VERBOSE:-}"
BIN="./target/release/tiering_bench"

# ── Build ──────────────────────────────────────────────────────────────────────
echo "Building tiering_bench (release)..."
cargo build --release --bin tiering_bench 2>&1 \
  | grep -E "^(error|warning\[|Compiling|Finished)" || true
echo ""

# ── Policy variants ────────────────────────────────────────────────────────────
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
N_TOTAL=$(( N_POLICIES * RUNS * 5 ))   # 5 presets

# ── CSV header ─────────────────────────────────────────────────────────────────
# Prepend "preset" and "run" columns to the standard tiering_bench header.
{ printf "preset,run,"; "$BIN" --header 2>/dev/null; } > "$CSV_OUT"

echo "Output  → $CSV_OUT"
echo "Configs : ${N_POLICIES} policies × ${RUNS} run(s) × 5 presets = ${N_TOTAL} rows"
echo ""

# ── run_preset ─────────────────────────────────────────────────────────────────
# run_preset <preset_name> [tiering_bench args ...]
#
# Iterates RUNS × POLICIES and appends one CSV row per combination.
# Failures are reported but do not abort the script.
run_preset() {
    local preset="$1"; shift
    local -a args=("$@")

    echo "=== ${preset} (${N_POLICIES} policies × ${RUNS} run(s)) ==="
    local t0; t0=$(date +%s)

    for run in $(seq 1 "$RUNS"); do
        for policy in "${POLICIES[@]}"; do
            printf "  [run %d] %-26s ... " "$run" "$policy"
            local p0; p0=$(date +%s)

            local stderr_dest="/dev/null"
            [ -n "$VERBOSE" ] && stderr_dest="/dev/stderr"

            local row
            if row=$("$BIN" --csv --policy "$policy" "${args[@]}" 2>"$stderr_dest"); then
                # Prepend preset+run to the CSV row and append to output file.
                printf "%s,%d,%s\n" "$preset" "$run" "$row" >> "$CSV_OUT"
                printf "%ds\n" "$(( $(date +%s) - p0 ))"
            else
                printf "FAILED\n"
            fi
        done
    done

    printf "  preset total: %ds\n\n" "$(( $(date +%s) - t0 ))"
}

OVERALL=$(date +%s)

# ── Presets ────────────────────────────────────────────────────────────────────

# Baseline: uniform access (skew=1.0), no exploitable pattern.
# All policies should converge to the same hit rate (~hot_cap / avg_working_set).
# Spread here reflects implementation overhead, not algorithmic quality.
run_preset steady_state \
    --warmup-ops     1000 \
    --measure-ops    5000 \
    --poll-interval-ops 50 \
    --depth          3 \
    --hot-capacity   20000 \
    --min-file-size  256 \
    --max-file-size  2048 \
    --create-pct     10 \
    --delete-pct     5 \
    --edit-pct       85 \
    --skew           1.0

# Frequency-favored: tiny stable hot set, heavy edit skew toward oldest files (skew=5.0).
# Hot tier holds ~8 files at avg 512 B; the same 8 files are accessed repeatedly.
# LFU keeps them permanently; LRU can be displaced by rare creates then must re-promote.
# Expected ordering: lfu >= lru_2q > arc > basic_lru.
run_preset frequency_favored \
    --warmup-ops     2000 \
    --measure-ops    10000 \
    --poll-interval-ops 50 \
    --depth          3 \
    --hot-capacity   4000 \
    --min-file-size  256 \
    --max-file-size  768 \
    --create-pct     3 \
    --delete-pct     0 \
    --edit-pct       97 \
    --skew           5.0

# Recency-favored: slow trickle of new files + extremely sharp recency skew (u^0.05).
# 88% of edits land on the newest 10% of files. Smaller files give ~12 hot slots.
# LRU keeps the newest files hot; LFU locks onto old high-count incumbents and
# immediately evicts new arrivals (freq=1) — missing them on every subsequent edit.
run_preset recency_favored \
    --warmup-ops     2000 \
    --measure-ops    10000 \
    --poll-interval-ops 50 \
    --depth          3 \
    --hot-capacity   4000 \
    --min-file-size  128 \
    --max-file-size  512 \
    --create-pct     8 \
    --delete-pct     0 \
    --edit-pct       92 \
    --skew           0.05

# High-churn: heavy create+delete cycles the live list + mild recency skew (0.35).
# ~48% of edits land on the newest 20% of files — a working set that shifts constantly.
# LRU/ARC adapt immediately as new files arrive; LFU's accumulated counts anchor it
# to files that may already be deleted, so it evicts new popular arrivals too soon.
run_preset high_churn \
    --warmup-ops     1000 \
    --measure-ops    5000 \
    --poll-interval-ops 50 \
    --depth          3 \
    --hot-capacity   20000 \
    --min-file-size  256 \
    --max-file-size  2048 \
    --create-pct     30 \
    --delete-pct     20 \
    --edit-pct       50 \
    --skew           0.35

# Hot-set: moderate skew toward older files (skew=3.0), larger hot tier.
# Tests whether frequency-aware policies maintain an edge under a weaker signal
# and with more files fitting in hot (more nuanced eviction decisions).
run_preset hot_set \
    --warmup-ops     1000 \
    --measure-ops    5000 \
    --poll-interval-ops 50 \
    --depth          3 \
    --hot-capacity   20000 \
    --min-file-size  256 \
    --max-file-size  2048 \
    --create-pct     10 \
    --delete-pct     5 \
    --edit-pct       85 \
    --skew           3.0

# ── Summary ────────────────────────────────────────────────────────────────────
TOTAL_ROWS=$(( $(wc -l < "$CSV_OUT") - 1 ))
echo "────────────────────────────────────────────────────────"
printf "Done.  %d / %d rows written to %s\n" "$TOTAL_ROWS" "$N_TOTAL" "$CSV_OUT"
printf "Total elapsed: %ds\n" "$(( $(date +%s) - OVERALL ))"
