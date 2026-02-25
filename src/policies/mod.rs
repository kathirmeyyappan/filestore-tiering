//! Policy implementations. Each policy implements the `PolicyEngine` trait.
//!
//! The runner builds a `TierState` and passes it into your policy; each poll it calls
//! `ingest(events)` then `reorganize()`. Tier sizes and capacity come from `self.tier_state`, not from the filesystem.
//!
//! **Adding a new policy:** Copy `dummy.rs` to a new file, implement the trait, then add a match arm in `main.rs`'s `make_policy` (key = `--policy` value).
//!
//! **Tier sizes and limits:** In `reorganize` use `self.tier_state.hot_bytes()`,
//! `self.tier_state.cold_bytes(i)`, `self.tier_state.hot_bytes_left()`,
//! `self.tier_state.cold_bytes_left(i)`, and `self.tier_state.move_to_tier(hot_path, target_dir)`.
//!
//! Add `#[derive(Debug)]` to your policy struct for logging and debugging.

pub mod dummy;
// pub mod lru_2q;
// pub mod lfu;
