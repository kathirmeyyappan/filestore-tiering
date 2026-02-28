//! Tier paths and byte accounting. Policies hold a `TierState` and use it in `reorganize`
//! to move files and query sizes/capacity. Byte counts are updated by the policy: `init_bytes`
//! at startup, and `adjust_hot_bytes` / `adjust_cold_bytes` after each move (since `move_to_tier`
//! only performs the filesystem move).

use std::error::Error;
use std::path::{Path, PathBuf};

use crate::tier_fs;

/// Tier paths and byte accounting for policies. **Tracked inside the policy engine**:
/// the runner creates it once, passes it into the policy at construction, and the
/// policy uses it in `reorganize` via `self` (no separate argument).
///
/// **Cold tier order:** Index 0 is the **warmest** cold tier; higher indices are colder.
#[allow(dead_code)]
#[derive(Debug)]
pub struct TierState {
    hot_root: PathBuf,
    cold_roots: Vec<PathBuf>,
    hot_bytes: u64,
    cold_bytes: Vec<u64>,
    hot_capacity: u64,
    cold_capacities: Vec<u64>,
}

#[allow(dead_code)]
impl TierState {
    /// Create tier state. Call `init_bytes()` once after creation to set current sizes.
    pub fn new(
        hot_root: PathBuf,
        cold_roots: Vec<PathBuf>,
        hot_capacity: u64,
        cold_capacities: Vec<u64>,
    ) -> Self {
        let n = cold_roots.len();
        let mut cold_capacities = cold_capacities;
        cold_capacities.resize(n, u64::MAX);
        Self {
            hot_root,
            cold_roots,
            hot_bytes: 0,
            cold_bytes: vec![0; n],
            hot_capacity,
            cold_capacities,
        }
    }

    /// Set current byte counts from disk (one-time or occasional). Call once after `new`.
    pub fn init_bytes(&mut self) -> Result<(), Box<dyn Error + Send + Sync>> {
        self.hot_bytes = tier_fs::tier_size_bytes(&self.hot_root)?;
        for (i, root) in self.cold_roots.iter().enumerate() {
            self.cold_bytes[i] = tier_fs::tier_size_bytes(root)?;
        }
        Ok(())
    }

    pub fn hot_root(&self) -> &Path {
        &self.hot_root
    }

    /// Root directory for cold tier at index `i`. Index 0 is warmest, higher indices colder.
    pub fn cold_root(&self, i: usize) -> Option<&Path> {
        self.cold_roots.get(i).map(PathBuf::as_path)
    }

    /// Total bytes currently in the hot tier (regular files only; symlinks ignored).
    pub fn hot_bytes(&self) -> u64 {
        self.hot_bytes
    }

    /// Total bytes currently in the given cold tier. Index 0 is warmest, higher indices colder.
    pub fn cold_bytes(&self, i: usize) -> u64 {
        self.cold_bytes.get(i).copied().unwrap_or(0)
    }

    /// Bytes remaining before hot tier reaches capacity.
    pub fn hot_bytes_left(&self) -> u64 {
        self.hot_capacity.saturating_sub(self.hot_bytes)
    }

    /// Adjust hot byte count by a size change (old_size → new_size) for an in-place file edit.
    /// Call when a hot file grows or shrinks without being moved.
    pub fn adjust_hot_bytes(&mut self, old_size: u64, new_size: u64) {
        self.hot_bytes = self.hot_bytes.saturating_sub(old_size).saturating_add(new_size);
    }

    /// Adjust cold tier byte count by a size change (old_size → new_size) for an in-place file edit.
    /// Call when a file in cold tier `i` grows or shrinks without being moved. No-op if `i` is out of range.
    pub fn adjust_cold_bytes(&mut self, i: usize, old_size: u64, new_size: u64) {
        if let Some(used) = self.cold_bytes.get_mut(i) {
            *used = used.saturating_sub(old_size).saturating_add(new_size);
        }
    }

    /// Bytes remaining before the given cold tier reaches capacity.
    pub fn cold_bytes_left(&self, i: usize) -> u64 {
        let cap = self.cold_capacities.get(i).copied().unwrap_or(u64::MAX);
        let used = self.cold_bytes(i);
        cap.saturating_sub(used)
    }

    /// Move the backing for `hot_path` into `target_dir`. This is the main API policies use for tiering.
    ///
    /// **What gets moved:** The actual file content (backing) for the logical path `hot_path`.
    /// Clients always use `hot_path` (e.g. `/hot/a/b`); this method only changes *where* the bytes
    /// live. After the call, `hot_path` still exists: either as a regular file (content at hot) or
    /// as a symlink pointing into another tier.
    ///
    /// **Byte accounting:** This method does *not* update `hot_bytes` or `cold_bytes`. The caller
    /// must update tier sizes after a move (e.g. via `adjust_hot_bytes` and `adjust_cold_bytes`).
    ///
    /// # Parameters
    ///
    /// - **`hot_path`** — The stable path clients use, e.g. `self.hot_root().join("a/b")`. Must exist (file or symlink) and be under this state's hot root.
    /// - **`target_dir`** — Where the backing should live:
    ///   - **`self.hot_root()`** (promote to hot): backing moves to `hot_path`; `hot_path` becomes a regular file. No-op if already there.
    ///   - **`self.cold_root(i)`** (evict to warm/cold): backing moves to that tier with the same relative path; `hot_path` is replaced by a symlink. No-op if already there. Cold index 0 is warmest, higher indices colder.
    ///
    /// # Return value
    ///
    /// Size in bytes of the file that was moved, or **0** if no-op.
    ///
    /// # Example
    ///
    /// In a policy's `reorganize`, using `self.tier_state`:
    ///
    /// ```ignore
    /// let hot_path = self.tier_state.hot_root().join("foo/bar");
    /// let size = self.tier_state.move_to_tier(&hot_path, self.tier_state.cold_root(0).unwrap())?;
    /// if size > 0 {
    ///     self.tier_state.adjust_hot_bytes(size, 0);
    ///     self.tier_state.adjust_cold_bytes(0, 0, size);
    /// }
    /// ```
    pub fn move_to_tier(
        &mut self,
        hot_path: &Path,
        target_dir: &Path,
    ) -> Result<u64, Box<dyn Error + Send + Sync>> {
        let size = tier_fs::move_to_tier(&self.hot_root, hot_path, target_dir)?;
        Ok(size)
    }
}
