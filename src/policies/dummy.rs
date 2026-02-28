//! **Template policy:** copy this file when implementing a new policy. All calls you may need
//! from this repo are documented in comments below with example usage.
//!
//! **Register:** Add a match arm in `main.rs`'s `make_policy` for your policy name; see the existing `dummy` arm for the pattern.
//!
//! ## Imports you'll use
//!
//! ```ignore
//! use std::path::{Path, PathBuf};
//! use crate::policy_engine::{AccessEvent, FsEventKind, PolicyEngine, TierState};
//! // Optional, for listing paths or checking symlink in reorganize:
//! use std::fs;
//! ```
//!
//! ## Reference (calls from this repo)
//!
//! **PolicyEngine trait** (you implement): `validate_config(hot, cold_storage)`, `ingest(&mut self, events)`, `reorganize(&mut self)`.
//!
//! **TierState** (field `self.tier_state`):
//! - `hot_root() -> &Path` — hot tier root; build logical paths with `.join("a/b")`.
//! - `cold_root(i: usize) -> Option<&Path>` — cold tier i (0 = warmest).
//! - `hot_bytes() -> u64`, `cold_bytes(i) -> u64` — current bytes in tier.
//! - `hot_bytes_left() -> u64`, `cold_bytes_left(i) -> u64` — remaining until capacity.
//! - `adjust_hot_bytes(old_size, new_size)`, `adjust_cold_bytes(i, old_size, new_size)` — update tier counts (call after a move or in-place file change).
//! - `move_to_tier(hot_path: &Path, target_dir: &Path) -> Result<u64, ...>` — move backing to `target_dir` (use `hot_root()` to promote, `cold_root(i)` to evict); returns bytes moved or 0 if no-op. **Does not update tier counts;** after a non-zero return, call `adjust_hot_bytes` and `adjust_cold_bytes` as needed.
//!
//! **AccessEvent** (in `ingest`): `event.path` (`PathBuf`), `event.kind` (`FsEventKind`), `event.timestamp` (`SystemTime`).
//! **FsEventKind**: `Create` | `Modify` | `Remove` | `Access` | `Other`.
//!
//! **std::fs** (optional, in reorganize): `read_dir(hot_root())` to list paths under hot (to pick eviction candidates); `symlink_metadata(path).file_type().is_symlink()` to see if a path is already a link to cold. Tier sizes are in `self.tier_state`, not from the filesystem.

use std::path::Path;

use crate::policy_engine::{AccessEvent, PolicyEngine, TierState};
use crate::policy_log;

/// Example policy: accepts config, does no tiering. Copy and replace with your logic.
#[derive(Debug)]
pub struct DummyPolicy {
    pub tier_state: TierState,
}

impl DummyPolicy {
    /// Runner creates `TierState`, calls `init_bytes()`, then passes it here. You store it and use it in `reorganize`.
    pub fn new(tier_state: TierState) -> Self {
        Self { tier_state }
    }
}

impl PolicyEngine for DummyPolicy {
    /// Called once before construction. Reject bad config with `Err(...)`.
    /// You receive `hot: &Path` and `cold_storage: &[PathBuf]`; e.g. `cold_storage.len()`, `cold_storage.is_empty()`.
    fn validate_config(
        _hot: &Path,
        cold_storage: &[std::path::PathBuf],
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        if cold_storage.is_empty() {
            return Err("dummy policy requires at least one cold_storage tier".into());
        }
        Ok(())
    }

    /// Called each poll with new events. Use them to update your internal state (e.g. access order); that state is then used in `reorganize` to decide what to move.
    ///
    /// Each event has `path` (PathBuf under hot root), `kind` (FsEventKind), and `timestamp`:
    ///
    /// ```ignore
    /// for event in events {
    ///     let path = &event.path;
    ///     match event.kind {
    ///         FsEventKind::Access | FsEventKind::Modify => { /* e.g. mark path recently used */ }
    ///         FsEventKind::Create | FsEventKind::Remove | FsEventKind::Other => {}
    ///     }
    /// }
    /// ```
    fn ingest(&mut self, events: &[AccessEvent]) {
        policy_log::log_ingest("dummy", events.len());
    }

    /// Called every poll after ingest. Use `self.tier_state` for paths, sizes, capacity, and moves.
    ///
    /// Evict to cold (then update tier counts):
    /// ```ignore
    /// let size = self.tier_state.move_to_tier(&hot_path, self.tier_state.cold_root(i).unwrap())?;
    /// if size > 0 {
    ///     self.tier_state.adjust_hot_bytes(size, 0);
    ///     self.tier_state.adjust_cold_bytes(i, 0, size);
    /// }
    /// ```
    ///
    /// Promote to hot (then update tier counts):
    /// ```ignore
    /// let size = self.tier_state.move_to_tier(&hot_path, self.tier_state.hot_root())?;
    /// if size > 0 {
    ///     self.tier_state.adjust_hot_bytes(0, size);
    ///     self.tier_state.adjust_cold_bytes(source_cold_i, size, 0);
    /// }
    /// ```
    ///
    /// Build `hot_path` under hot root:
    /// ```ignore
    /// let hot_path = self.tier_state.hot_root().join("rel/path");
    /// ```
    ///
    /// Optional: list paths or check if already a symlink
    /// (shouldn't be needed — we only care about this during promotion, which is handled internally):
    /// ```ignore
    /// std::fs::read_dir(self.tier_state.hot_root())
    /// symlink_metadata(p).file_type().is_symlink()
    /// ```
    fn reorganize(&mut self) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        policy_log::log_reorganize_done(policy_log::ReorganizeDoneParams {
            policy_name: "dummy",
            should_log: true,
            new_in_hot: 0,
            promoted: 0,
            evicted_room: 0,
            evicted_cap: 0,
            hot_bytes: self.tier_state.hot_bytes(),
            cold_bytes: self.tier_state.cold_bytes(0),
        });
        // Example eviction:
        //
        // let hot_path = self.tier_state.hot_root().join("some/file");
        // if let Some(cold) = self.tier_state.cold_root(0) {
        //     let size = self.tier_state.move_to_tier(&hot_path, cold)?;
        //     if size > 0 {
        //         self.tier_state.adjust_hot_bytes(size, 0);
        //         self.tier_state.adjust_cold_bytes(0, 0, size);
        //     }
        // }
        //
        // Example promotion:
        //
        // let size = self.tier_state.move_to_tier(&hot_path, self.tier_state.hot_root())?;
        // if size > 0 {
        //     self.tier_state.adjust_hot_bytes(0, size);
        //     self.tier_state.adjust_cold_bytes(0, size, 0);  // 0 = cold tier index if single cold tier
        // }
        Ok(())
    }
}
