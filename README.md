# filestore-tiering

Storage tiering via access-aware local file migration. Watches a hot directory, feeds filesystem events to a policy engine, and lets the engine reorganize files across hot and cold storage.

## Run

All of these directories must already exist.

```bash
# Build and run (use -- so flags go to the binary, not cargo)
cargo run -- --hot-storage /path/to/hot --cold-storage /path/to/cold -i 2

# With logging
RUST_LOG=info cargo run -- --hot-storage /path/to/hot --cold-storage /path/to/cold -i 2
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

4. **Wire it up in `src/main.rs`** inside `make_policy()`:
   ```rust
   "my_policy" => {
       policies::my_policy::MyPolicy::validate_config(hot_storage, cold_storage).map_err(to_err)?;
       Ok(Box::new(policies::my_policy::MyPolicy::new(
           hot_storage.to_path_buf(),
           cold_storage.to_vec(),
       )))
   }
   ```

5. Run with `--policy my_policy`.

Use `src/policies/dummy.rs` as a minimal reference.
