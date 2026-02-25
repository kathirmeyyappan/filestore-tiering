//! Tier paths and byte accounting. Policies hold a `TierState` and use it in `reorganize`
//! to move files and query sizes/capacity.

use std::error::Error;
use std::fs;
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
    /// as a symlink pointing into another tier. Internal byte counts (`hot_bytes`, `cold_bytes`) are
    /// updated automatically; no need to pass `hot_root` — it's in this state.
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
    /// self.tier_state.move_to_tier(&hot_path, self.tier_state.cold_root(0).unwrap())?;  // evict
    /// self.tier_state.move_to_tier(&hot_path, self.tier_state.hot_root())?;              // promote
    /// ```
    pub fn move_to_tier(
        &mut self,
        hot_path: &Path,
        target_dir: &Path,
    ) -> Result<u64, Box<dyn Error + Send + Sync>> {
        let cold_source_i = if target_dir == self.hot_root {
            self.cold_index_containing(hot_path)
        } else {
            None
        };
        let size = tier_fs::move_to_tier(&self.hot_root, hot_path, target_dir)?;
        if size == 0 {
            return Ok(0);
        }
        if target_dir == self.hot_root {
            self.hot_bytes += size;
            if let Some(i) = cold_source_i {
                self.cold_bytes[i] = self.cold_bytes[i].saturating_sub(size);
            }
        } else if let Some(i) = self
            .cold_roots
            .iter()
            .position(|r| r.as_path() == target_dir)
        {
            self.hot_bytes = self.hot_bytes.saturating_sub(size);
            self.cold_bytes[i] += size;
        }
        Ok(size)
    }

    fn cold_index_containing(&self, hot_path: &Path) -> Option<usize> {
        let meta = fs::symlink_metadata(hot_path).ok()?;
        if !meta.file_type().is_symlink() {
            return None;
        }
        let target = fs::read_link(hot_path).ok()?;
        let abs = if target.is_absolute() {
            target
        } else {
            hot_path.parent().unwrap_or(Path::new("/")).join(target)
        };
        for (i, root) in self.cold_roots.iter().enumerate() {
            if abs.starts_with(root) {
                return Some(i);
            }
        }
        None
    }
}
