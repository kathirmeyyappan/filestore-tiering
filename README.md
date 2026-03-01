# filestore-tiering

Storage tiering via access-aware local file migration. Watches a hot directory, feeds filesystem events to a policy engine, and lets the engine reorganize files across hot and cold storage.

## Run

The project has two binaries: **filestore-tiering** (the daemon, default) and **tiering_bench** (the benchmark). Without `--bin`, `cargo run` runs the daemon.

All of these directories must already exist.

```bash
# Build and run the daemon (use -- so flags go to the binary, not cargo)
cargo run -- --hot-storage /path/to/hot --cold-storage /path/to/cold -i 2

# With logging
RUST_LOG=info cargo run -- --hot-storage /path/to/hot --cold-storage /path/to/cold -i 2

# Explicit binary (optional; same as above when default-run is set)
cargo run --bin filestore-tiering -- --hot-storage ../hot -c ../cold --policy basic_lru
```

**Options**

| Flag | Short | Description |
|------|--------|-------------|
| `--hot-storage` | `-H` | Client-facing directory to watch (required) |
| `--cold-storage` | `-c` | One or more cold-tier directories (required) |
| `--policy` | — | Policy name, e.g. `dummy` (default: `dummy`) |
| `--interval` | `-i` | Poll interval in seconds (default: 5) |

Example with two cold tiers:

```bash
cargo run -- -H ./hot -c ./cold1 ./cold2 -i 2
```

To run the benchmark instead, use `cargo run --bin tiering_bench --` (see Benchmarking below).

## Benchmarking

The benchmark runs for a **fixed time**: **warmup**, then **measurement**. It simulates the **daemon** by peeking at a configurable rate (`--poll-interval-sec`, default 0.2 s = 5 peeks/sec): events accumulate between peeks, then ingest+reorganize runs. The default **hot capacity is 20K** (~40 files at 500 B each) so that one poll’s worth of creates often exceeds the limit and causes evictions/promotions. It reports **throughput** (ops/s) and **promotions/demotions** in the measure window (counts and % of ops).

### Run one benchmark

```bash
# Default: basic_lru, 5s warmup, 30s measure, poll every 0.2s, 20K hot (good turnover)
cargo run --bin tiering_bench --

# Shorter run, more peeks per second
cargo run --bin tiering_bench -- --warmup-sec 3 --measure-sec 15 --poll-interval-sec 0.1

# Stress evictions (smaller hot cap)
cargo run --bin tiering_bench -- --hot-capacity 5K --measure-sec 20

# One-line CSV (for scripting or bench_eval.sh)
cargo run --bin tiering_bench -- --csv --policy basic_lru

# Print CSV header only (for scripting)
cargo run --bin tiering_bench -- --header
```

By default the benchmark prints a **human-readable summary** (measure_ops, throughput, promotions/demotions and % of ops). Use `--csv` to get a single CSV line instead.

**Useful flags**

| Flag | Short | Description |
|------|--------|-------------|
| `--policy` | — | Policy name, e.g. `basic_lru`, `dummy` (default: `basic_lru`) |
| `--warmup-sec` | — | Warmup duration in seconds (default: 5) |
| `--measure-sec` | — | Measurement duration in seconds; throughput and move counts during this window (default: 30) |
| `--poll-interval-sec` | — | Daemon peek interval in seconds; ingest+reorganize this often, events accumulate between peeks (default: 0.2 = 5 peeks/s) |
| `--depth` | `-d` | Directory nesting depth under hot (default: 3) |
| `--hot-capacity` | — | Hot tier capacity; default 20K gives ~40 files so creates exceed it and cause turnover (same units as daemon) |
| `--file-size` | — | Bytes per created file (default: 500) |
| `--create-pct` | — | Weight for create 0–100 (default: 40) |
| `--delete-pct` | — | Weight for delete 0–100 (default: 30) |
| `--edit-pct` | — | Weight for edit 0–100 (default: 30) |
| `--csv` | — | Output one CSV line (for scripts); default is a readable summary |
| `--header` | — | Print CSV header line only (for scripts) |

Use a smaller `--hot-capacity` (e.g. 5K) to stress evictions and see higher demotion rates.

### Run across policies (script)

```bash
./scripts/bench_eval.sh
```

Runs `tiering_bench` for each policy (default: `basic_lru` and `dummy`) with the same parameters and prints a table. Override via environment:

```bash
POLICIES="basic_lru dummy" MEASURE_SEC=20 ./scripts/bench_eval.sh
HOT_CAP=5K MEASURE_SEC=30 ./scripts/bench_eval.sh   # more evictions
```

Supported env vars: `POLICIES`, `WARMUP_SEC`, `MEASURE_SEC`, `POLL_INTERVAL_SEC`, `HOT_CAP`, `DEPTH`, `CREATE_PCT`, `DELETE_PCT`, `EDIT_PCT`, `BATCH_SIZE`.

## Adding a new policy

1. **Create a module** under `src/policies/`, e.g. `src/policies/my_policy.rs`.

2. **Declare it** in `src/policies/mod.rs`:
   ```rust
   pub mod my_policy;
   ```

3. **Implement the policy:**
   - A struct that holds `hot_storage: PathBuf` and `cold_storage: Vec<PathBuf>`.
   - A `new(hot_storage: PathBuf, cold_storage: Vec<PathBuf>) -> Self` that stores them.
   - `impl PolicyEngine for MyPolicy` with:
     - `validate_config(hot, cold_storage)` — return `Err(...)` if the config is invalid (e.g. wrong number of cold tiers). Optional; default accepts any config.
     - `ingest(&mut self, events: &[AccessEvent])` — process new filesystem events (path, kind, timestamp).
     - `reorganize(&mut self) -> Result<(), Box<dyn Error + Send + Sync>>` — run your logic (e.g. count bytes, move files, update symlinks). Use `self.hot_storage` and `self.cold_storage` and `std::fs` as needed.

4. **Wire it up in `src/daemon.rs`** inside `make_policy()`:
   ```rust
   "my_policy" => {
       policies::my_policy::MyPolicy::validate_config(hot_storage, cold_storage).map_err(to_err)?;
       Ok(Box::new(policies::my_policy::MyPolicy::new(
           hot_storage.to_path_buf(),
           cold_storage.to_vec(),
       )))
   }
   ```

5. **Implement `stats()` for benchmarking** (optional but recommended): Override `fn stats(&self) -> PolicyStats` to return promotion and demotion counts so `tiering_bench` and `scripts/bench_eval.sh` can report move counts. See “Benchmarking” above and the “Benchmark compliance” section in `LRU_IMPLEMENTATION.md`.

6. Run with `--policy my_policy`.

Use `src/policies/dummy.rs` as a minimal reference.
