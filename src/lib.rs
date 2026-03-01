//! Filestore tiering: access-aware policy engine and tier state.
//!
//! - **Main binary** (`filestore-tiering`): watch loop, uses `daemon` and `watcher`.
//! - **Bench binary** (`tiering_bench`): synthetic workload, uses `bench` and `daemon`.

pub mod bench;
pub mod capacity;
pub mod daemon;
pub mod policies;
pub mod policy_engine;
pub mod policy_log;
pub mod tier_fs;
pub mod tier_state;
pub mod watcher;
