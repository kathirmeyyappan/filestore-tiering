//! Policy implementations. Each policy implements the PolicyEngine trait.
//!
//! The runner constructs policy engine with `(hot_storage: PathBuf, cold_storage: Vec<PathBuf>)`
//! and calls `ingest(events)` then `reorganize()` each poll. You have full access to the paths
//! and can use `std::fs` (e.g. `read_dir`, `metadata`) in `reorganize` to count bytes, list files, etc.

pub mod dummy;
// pub mod lru_2q;
// pub mod lfu;
