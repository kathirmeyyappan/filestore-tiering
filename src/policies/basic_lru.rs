//! Basic LRU policy: one hot tier (capacity-limited), one cold tier (unlimited).
//! Any touch promotes to hot; evict from oldest-in-hot to cold until under capacity.
//! In-place file growth is tracked via per-file size map: on Modify, we diff old vs new size and
//! call adjust_hot_bytes so hot_bytes stays accurate without a full rescan.
//! Between polls, hot can briefly exceed the limit (e.g. in-place growth); we correct it on the next reorganize.

use std::collections::{HashMap, VecDeque};
use std::fs;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use crate::policy_engine::{AccessEvent, FsEventKind, PolicyEngine, TierState};

/// LRU over hot tier only. Queue = paths currently in hot (front = MRU, back = LRU).
/// On touch: promote if in cold, else move to front. Then evict from back until hot_bytes <= hot_capacity.
#[derive(Debug)]
pub struct BasicLruPolicy {
    pub tier_state: TierState,
    /// Paths currently in hot; front = most recently used, back = least recently used.
    queue: VecDeque<PathBuf>,
    /// Known size of each file currently in hot. Used to compute deltas on Modify events.
    hot_sizes: HashMap<PathBuf, u64>,
    /// Paths that had an event this poll; we process oldest touch first.
    touched: Vec<(PathBuf, SystemTime)>,
}

impl BasicLruPolicy {
    pub fn new(tier_state: TierState) -> Self {
        Self {
            tier_state,
            queue: VecDeque::new(),
            hot_sizes: HashMap::new(),
            touched: Vec::new(),
        }
    }

    /// List all regular-file paths under `dir` (recursive; symlinks skipped).
    fn list_regular_files_under(dir: &Path) -> Result<Vec<PathBuf>, std::io::Error> {
        let mut out = Vec::new();
        for entry in fs::read_dir(dir)? {
            let entry = entry?;
            let path = entry.path();
            let meta = fs::symlink_metadata(&path)?;
            let ft = meta.file_type();
            if ft.is_dir() && !ft.is_symlink() {
                out.extend(Self::list_regular_files_under(&path)?);
            } else if meta.is_file() && !ft.is_symlink() {
                out.push(path);
            }
        }
        Ok(out)
    }

    fn cold_root(&self) -> &Path {
        self.tier_state.cold_root(0).expect("basic_lru requires one cold tier")
    }
}

impl PolicyEngine for BasicLruPolicy {
    fn validate_config(
        _hot: &Path,
        cold_storage: &[std::path::PathBuf],
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        if cold_storage.len() != 1 {
            return Err("basic_lru requires exactly one cold_storage tier".into());
        }
        Ok(())
    }

    fn ingest(&mut self, events: &[AccessEvent]) {
        for event in events {
            if event.kind == FsEventKind::Modify {
                // Hot file modified in place: apply size delta so tier count stays correct.
                if let Some(&old_size) = self.hot_sizes.get(&event.path) {
                    if let Ok(new_size) = fs::metadata(&event.path).map(|m| m.len()) {
                        self.tier_state.adjust_hot_bytes(old_size, new_size);
                        self.hot_sizes.insert(event.path.clone(), new_size);
                    }
                }
            }
        }
        self.touched = events.iter().map(|e| (e.path.clone(), e.timestamp)).collect();
    }

    fn reorganize(&mut self) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        let hot_root = self.tier_state.hot_root().to_path_buf();
        let cold = self.cold_root().to_path_buf();

        // First run: seed queue and hot_sizes from disk. tier_state.hot_bytes already set by init_bytes().
        if self.queue.is_empty() {
            for path in Self::list_regular_files_under(&hot_root)? {
                if let Ok(size) = fs::metadata(&path).map(|m| m.len()) {
                    self.hot_sizes.insert(path.clone(), size);
                }
                self.queue.push_back(path);
            }
        }

        // Process touches oldest-first so evictions and promotions respect event order.
        self.touched.sort_by(|a, b| a.1.cmp(&b.1));
        for (path, _) in self.touched.drain(..) {
            if !path.starts_with(&hot_root) {
                continue;
            }
            let meta = match fs::symlink_metadata(&path) {
                Ok(m) => m,
                Err(_) => {
                    // Path gone: drop from our state and correct hot_bytes if we had a size.
                    if let Some(old) = self.hot_sizes.remove(&path) {
                        self.tier_state.adjust_hot_bytes(old, 0);
                    }
                    self.queue.retain(|p| p != &path);
                    continue;
                }
            };
            if meta.file_type().is_symlink() {
                // PATH IN COLD: make room, then promote to hot.
                let need = fs::read_link(&path)
                    .ok()
                    .and_then(|t| {
                        let abs = if t.is_absolute() { t } else { path.parent().unwrap_or(Path::new("/")).join(t) };
                        fs::metadata(&abs).ok().map(|m| m.len())
                    })
                    .unwrap_or(0);
                while self.tier_state.hot_bytes_left() < need {
                    let Some(back) = self.queue.pop_back() else { break };
                    if back.exists() {
                        let size = self.tier_state.move_to_tier(&back, &cold)?;
                        if size > 0 {
                            self.tier_state.adjust_hot_bytes(size, 0);
                            self.tier_state.adjust_cold_bytes(0, 0, size);
                        }
                        self.hot_sizes.remove(&back);
                    } else if let Some(old) = self.hot_sizes.remove(&back) {
                        self.tier_state.adjust_hot_bytes(old, 0);
                    }
                    // If back is gone (deleted or smn) and not in hot_sizes, we can't fix hot_bytes here; it stays high until next init_bytes.
                }
                if self.tier_state.hot_bytes_left() < need {
                    return Err(format!(
                        "not enough hot capacity to promote {:?} (need {} bytes)",
                        path, need
                    )
                    .into());
                }
                let moved = self.tier_state.move_to_tier(&path, &hot_root)?;
                let size = fs::metadata(&path).map(|m| m.len()).unwrap_or(moved);
                if size > 0 {
                    self.tier_state.adjust_hot_bytes(0, size);
                    self.tier_state.adjust_cold_bytes(0, size, 0);
                }
                self.hot_sizes.insert(path.clone(), size);
                self.queue.retain(|p| p != &path);
                self.queue.push_front(path);
            } else if meta.is_file() {
                // PATH IN HOT: track size if new (e.g. created after startup), then move to front (MRU).
                if !self.hot_sizes.contains_key(&path) {
                    if let Ok(size) = fs::metadata(&path).map(|m| m.len()) {
                        self.tier_state.adjust_hot_bytes(0, size);
                        self.hot_sizes.insert(path.clone(), size);
                    }
                }
                self.queue.retain(|p| p != &path);
                self.queue.push_front(path);
            }
        }

        // Between polls we may have crossed the limit (e.g. in-place growth) because of Modify events; can't be avoided.
        // Evict LRU from back until hot is at or under capacity.
        while self.tier_state.hot_bytes_left() == 0 {
            let Some(back) = self.queue.pop_back() else { break };
            if back.exists() {
                let size = self.tier_state.move_to_tier(&back, &cold)?;
                if size > 0 {
                    self.tier_state.adjust_hot_bytes(size, 0);
                    self.tier_state.adjust_cold_bytes(0, 0, size);
                }
                self.hot_sizes.remove(&back);
            } else if let Some(old) = self.hot_sizes.remove(&back) {
                self.tier_state.adjust_hot_bytes(old, 0);
            }
            // Back gone and not in hot_sizes: hot_bytes stays high until next init_bytes.
        }

        Ok(())
    }
}
