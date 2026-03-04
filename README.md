# filestore-tiering

Storage tiering via access-aware local file migration. Watches a hot directory, feeds filesystem events to a policy engine, and lets the engine reorganize files across hot and cold storage.

## Policies

| Name | Algorithm | Tunables |
|------|-----------|----------|
| `basic_lru` | Least Recently Used | — |
| `arc` | Adaptive Replacement Cache (Megiddo & Modha) | — |
| `lfu` | Least Frequently Used (hot/cold partitioning) | — |
| `lru_2q` | Two-Queue LRU (Johnson & Shasha) | `a1in_fraction` |
| `lecar` | Learning Cache Replacement (Vietri et al.) | `learning_rate`, `w_lru` |
| `cacheus` | CACHEUS (Gil et al.) | `lr_init`, `w_sr_lru` |
| `decision_tree` | Decision tree regressor (LRB-inspired) | `retrain_interval`, `min_training_samples`, `tree_max_depth`, `tree_min_samples_leaf` |
| `dummy` | No-op (pass-through) | — |

## Run (daemon)

The project has two binaries: **filestore-tiering** (the daemon, default) and **tiering_bench** (standalone benchmark). Without `--bin`, `cargo run` runs the daemon.

All directories must already exist.

```bash
# Basic usage
cargo run -- -H /path/to/hot -c /path/to/cold --policy basic_lru -i 2

# With logging
RUST_LOG=info cargo run -- -H /path/to/hot -c /path/to/cold --policy arc -i 2

# Two cold tiers with capacity limits
cargo run -- -H ./hot -c ./cold1 ./cold2 --hot-capacity 1G --cold-capacities 500M 2G

# With tunable policy parameters
cargo run -- -H ./hot -c ./cold --policy lecar --policy-param learning_rate=0.3 --policy-param w_lru=0.7
```

**Options**

| Flag | Short | Description |
|------|--------|-------------|
| `--hot-storage` | `-H` | Client-facing directory to watch (required) |
| `--cold-storage` | `-c` | One or more cold-tier directories (required) |
| `--hot-capacity` | — | Hot tier capacity (default: `unlimited`). Accepts bytes, `1K`/`1M`/`1G`/`1T` (decimal), `1Ki`/`1Mi`/`1Gi`/`1Ti` (binary) |
| `--cold-capacities` | — | Per-cold-tier capacities (same format; defaults to unlimited) |
| `--policy` | — | Policy name (default: `dummy`) |
| `--policy-param` | — | Per-policy tunable: `key=value` (repeatable) |
| `--interval` | `-i` | Poll interval in seconds (default: 5) |

## Benchmarking

Benchmarks run a deterministic **operation-based** workload: a fixed number of create/delete/edit operations with configurable access skew. The daemon is simulated synchronously (ingest + reorganize every `poll_interval_ops` operations). The primary metric is **hit rate** (fraction of edits landing on hot files).

### Preset benchmarks (recommended)

Preset benchmarks generate one trace per preset and replay it for every policy, ensuring a fair head-to-head comparison.

```bash
# Run all presets across all policies
cargo bench --bench presets

# Quick mode (ops/5, faster iteration)
cargo bench --bench presets -- --quick

# Filter by preset name and/or policy
cargo bench --bench presets -- scan_heavy -p lru_2q -p cacheus

# Save results to CSV
cargo bench --bench presets -- --csv results.csv

# With tunable parameters (applied to all policies; each extracts what it recognizes)
cargo bench --bench presets -- --policy-param a1in_fraction=0.4 --policy-param lr_init=0.8
```

**Available presets:** `steady_state`, `frequency_favored`, `recency_favored`, `high_churn`, `hot_set`, `scan_flood`, `scan_heavy`, `phase_shift`

**Preset flags**

| Flag | Short | Description |
|------|--------|-------------|
| `--quick` | `-q` | Run with ops/5 for faster iteration |
| `-p` / `--policy` | `-p` | Filter to specific policies (repeatable) |
| `--policy-param` | — | Tunable: `key=value` (repeatable) |
| `--csv` | — | Save all results to a CSV file |
| (positional) | — | Filter to presets whose name contains the argument |

### Standalone single-policy benchmark

```bash
# Default: basic_lru, 1K warmup, 5K measure, 20K hot capacity
cargo run --bin tiering_bench --

# Custom workload
cargo run --bin tiering_bench -- --policy lru_2q --hot-capacity 4K \
    --create-pct 30 --edit-pct 65 --delete-pct 5 --skew 3.0 \
    --warmup-ops 2000 --measure-ops 10000

# With tunable parameters
cargo run --bin tiering_bench -- --policy cacheus --policy-param lr_init=0.8

# CSV output (for scripting)
cargo run --bin tiering_bench -- --csv --policy arc

# Print CSV header only
cargo run --bin tiering_bench -- --header
```

**Flags**

| Flag | Short | Description |
|------|--------|-------------|
| `--policy` | — | Policy name (default: `basic_lru`) |
| `--warmup-ops` | — | Warmup operations (default: 1000) |
| `--measure-ops` | — | Measurement operations (default: 5000) |
| `--poll-interval-ops` | — | Daemon polls every N ops (default: 50) |
| `--depth` | `-d` | Directory nesting depth (default: 3) |
| `--hot-capacity` | — | Hot tier capacity (default: `20K`) |
| `--min-file-size` | — | Min file size in bytes (default: 256) |
| `--max-file-size` | — | Max file size in bytes (default: 2048) |
| `--create-pct` | — | Create weight 0-100 (default: 10) |
| `--delete-pct` | — | Delete weight 0-100 (default: 5) |
| `--edit-pct` | — | Edit weight 0-100 (default: 85) |
| `--skew` | — | Edit-target skew: 1.0 = uniform, >1 = oldest files, <1 = newest (default: 1.0) |
| `--policy-param` | — | Tunable: `key=value` (repeatable) |
| `--csv` | — | Output one CSV line |
| `--header` | — | Print CSV header only |

## Policy parameters

Policies with tunables accept parameters via `--policy-param key=value` (repeatable). Each policy extracts the keys it recognizes; unknown keys are silently ignored.

| Policy | Key | Default | Description |
|--------|-----|---------|-------------|
| `lru_2q` | `a1in_fraction` | 0.25 | Probation queue fraction of hot capacity |
| `lecar` | `learning_rate` | 0.45 | Weight adaptation speed |
| `lecar` | `w_lru` | 0.5 | Initial LRU vs LFU balance |
| `cacheus` | `lr_init` | 0.45 | Base learning rate |
| `cacheus` | `w_sr_lru` | 0.5 | Initial SR-LRU vs CR-LFU balance |
| `decision_tree` | `retrain_interval` | 50 | Evictions between retraining |
| `decision_tree` | `min_training_samples` | 20 | Samples needed before first train |
| `decision_tree` | `tree_max_depth` | 4 | Decision tree depth |
| `decision_tree` | `tree_min_samples_leaf` | 2 | Leaf minimum samples |

## Adding a new policy

For a **minimal** policy (e.g. no tiering), copy `src/policies/dummy.rs` and wire it up. For a **tiering** policy (hot/cold moves, events, eviction), read **`LRU_IMPLEMENTATION.md`** first: it defines path canonicalization, byte accounting, touch filters, loop prevention, and **benchmark compliance** (§12 and §12.1) so your policy works with the benchmark and reports promotions/demotions correctly.

1. **Create a module** under `src/policies/`, e.g. `src/policies/my_policy.rs`.

2. **Declare it** in `src/policies/mod.rs`:
   ```rust
   pub mod my_policy;
   ```

3. **Implement the policy:**
   - A struct that holds **`tier_state: TierState`** (hot/cold roots, capacities, and byte counts are inside `TierState`; the daemon builds it and calls `init_bytes()` before passing it to your policy).
   - A constructor `new(tier_state: TierState) -> Self` that stores it. For tunables, also add `new_with_params(tier_state: TierState, params: &HashMap<String, f64>) -> Self` and have `new` delegate to it with an empty map.
   - `impl PolicyEngine for MyPolicy` with:
     - `validate_config(hot, cold_storage)` — return `Err(...)` if the config is invalid (e.g. wrong number of cold tiers). Optional; default accepts any config.
     - `ingest(&mut self, events: &[AccessEvent])` — process new filesystem events (path, kind, timestamp). Use **canonical** paths when storing event-derived state (e.g. `touched`) so reorganize can match `hot_root`; see `LRU_IMPLEMENTATION.md` §2 and §12.1.
     - `reorganize(&mut self) -> Result<(), Box<dyn Error + Send + Sync>>` — run your logic (evict/promote, update queue, call `tier_state.move_to_tier` and `adjust_hot_bytes` / `adjust_cold_bytes`). Use `self.tier_state.hot_root()`, `cold_root(i)`, and `std::fs` as needed.
   - **Bench compliance:** Override `fn stats(&self) -> PolicyStats` and track promotions/demotions so `tiering_bench` can report move counts. See `LRU_IMPLEMENTATION.md` §12 and §12.1.

4. **Wire it up in `src/daemon.rs`** inside `make_policy()`:
   ```rust
   "my_policy" => {
       crate::policies::my_policy::MyPolicy::validate_config(hot_storage, cold_storage)
           .map_err(to_err)?;
       Ok(Box::new(
           crate::policies::my_policy::MyPolicy::new_with_params(tier_state, policy_params),
       ))
   }
   ```

   Policies receive a single `TierState` (hot/cold already canonicalized, `init_bytes()` already called) and a `&HashMap<String, f64>` of tunable parameters.

5. Run with `--policy my_policy`.

Use `src/policies/dummy.rs` as a minimal reference and `src/policies/basic_lru.rs` plus **`LRU_IMPLEMENTATION.md`** for a full tiering implementation and bench-compliant rules.
