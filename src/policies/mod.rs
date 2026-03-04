//! Policy implementations. Each policy implements the `PolicyEngine` trait.
//!
//! The runner builds a `TierState` and passes it into your policy; each poll it calls
//! `ingest(events)` then `reorganize()`. Tier sizes and capacity come from `self.tier_state`, not from the filesystem.
//!
//! **Adding a new policy:** Copy `dummy.rs` to a new file, implement the trait, then add a match arm in `main.rs`'s `make_policy` (key = `--policy` value).
//!
//! **Logging:** Use `crate::policy_log` for consistent ingest / initial-fill / reorganize-done logs:
//! `log_ingest(name, events.len())`, `log_initial_fill(name, file_count, hot_bytes)`,
//! `log_reorganize_done(ReorganizeDoneParams { policy_name, should_log, new_in_hot, promoted, evicted_room, evicted_cap, hot_bytes, cold_bytes })`.
//!
//! **Tier sizes and limits:** In `reorganize` use `self.tier_state.hot_bytes()`,
//! `self.tier_state.cold_bytes(i)`, `self.tier_state.hot_bytes_left()`,
//! `self.tier_state.cold_bytes_left(i)`, and `self.tier_state.move_to_tier(hot_path, target_dir)`.
//! **Important:** `move_to_tier` only performs the filesystem move; when it returns a non-zero size,
//! the policy must call `adjust_hot_bytes` and/or `adjust_cold_bytes` to keep tier state accurate.
//!
//! Add `#[derive(Debug)]` to your policy struct for logging and debugging.

pub mod arc;
pub mod basic_lru;
pub mod cacheus;
pub mod decision_tree;
pub mod dummy;
pub mod lecar;
pub mod lfu;
pub mod lru_2q;
