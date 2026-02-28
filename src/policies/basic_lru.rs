//! Basic LRU: one hot (capacity-limited), one cold. Touch = promote or MRU. Evict LRU until under capacity.

use std::collections::{HashMap, VecDeque};
use std::fs;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use crate::policy_engine::{AccessEvent, FsEventKind, PolicyEngine, TierState};
use crate::policy_log;

fn canonical(path: &Path) -> PathBuf {
    fs::canonicalize(path).unwrap_or_else(|_| {
        path.parent()
            .and_then(|p| fs::canonicalize(p).ok())
            .and_then(|p| path.file_name().map(|n| p.join(n)))
            .unwrap_or_else(|| path.to_path_buf())
    })
}

#[derive(Debug)]
pub struct BasicLruPolicy {
    pub tier_state: TierState,
    queue: VecDeque<PathBuf>,
    hot_sizes: HashMap<PathBuf, u64>,
    cold_sizes: HashMap<PathBuf, u64>,
    touched: Vec<(PathBuf, SystemTime)>,
}

impl BasicLruPolicy {
    pub fn new(tier_state: TierState) -> Self {
        Self {
            tier_state,
            queue: VecDeque::new(),
            hot_sizes: HashMap::new(),
            cold_sizes: HashMap::new(),
            touched: Vec::new(),
        }
    }

    fn list_hot_files(dir: &Path, hot_root: &Path) -> Result<Vec<PathBuf>, std::io::Error> {
        let mut out = Vec::new();
        for e in fs::read_dir(dir)? {
            let p = e?.path();
            let m = fs::symlink_metadata(&p)?;
            if m.file_type().is_dir() && !m.file_type().is_symlink() {
                out.extend(Self::list_hot_files(&p, hot_root)?);
            } else if m.is_file() && !m.file_type().is_symlink() {
                let rel = p.strip_prefix(hot_root).or_else(|_| p.strip_prefix(dir)).unwrap_or_else(|_| p.as_path());
                out.push(hot_root.join(rel));
            }
        }
        Ok(out)
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
            let path = canonical(&event.path);
            match event.kind {
                FsEventKind::Modify => {
                    if let Some(&old) = self.hot_sizes.get(&path) {
                        if let Ok(new) = fs::metadata(&path).map(|m| m.len()) {
                            self.tier_state.adjust_hot_bytes(old, new);
                            self.hot_sizes.insert(path.clone(), new);
                        }
                    }
                }
                FsEventKind::Remove => {
                    if let Some(old) = self.hot_sizes.remove(&path) {
                        self.tier_state.adjust_hot_bytes(old, 0);
                        self.queue.retain(|p| p != &path);
                    } else if let Some(sz) = self.cold_sizes.remove(&path) {
                        self.tier_state.adjust_cold_bytes(0, sz, 0);
                    }
                }
                _ => {}
            }
        }
        self.touched = events.iter().map(|e| (e.path.clone(), e.timestamp)).collect();
        policy_log::log_ingest("basic_lru", events.len());
    }

    fn reorganize(&mut self) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        let hot_root = canonical(self.tier_state.hot_root());
        let cold = self.tier_state.cold_root(0).expect("one cold tier").to_path_buf();
        let cold_abs = canonical(&cold);

        // First run: seed queue and hot_sizes from disk (tier_state.hot_bytes already set by init_bytes).
        if self.queue.is_empty() {
            for p in Self::list_hot_files(&hot_root, &hot_root)? {
                if let Ok(sz) = fs::metadata(&p).map(|m| m.len()) {
                    self.hot_sizes.insert(p.clone(), sz);
                }
                self.queue.push_back(p);
            }
            policy_log::log_initial_fill("basic_lru", self.queue.len(), self.tier_state.hot_bytes());
        }

        // Process touches oldest-first so evict/promote order is consistent with event order.
        self.touched.sort_by(|a, b| a.1.cmp(&b.1));
        let had_touches = !self.touched.is_empty();
        let mut new_in_hot = 0u32;
        let mut promoted = 0u32;
        let mut evicted_room = 0u32;

        for (mut path, _) in self.touched.drain(..) {
            // Watcher may report cold path when user edits via symlink; map to hot path.
            if path.starts_with(&cold_abs) {
                if let Ok(rel) = path.strip_prefix(&cold_abs) {
                    path = hot_root.join(rel);
                }
            } else if !path.is_absolute() {
                path = hot_root.join(path);
            }
            if !path.starts_with(&hot_root) {
                continue;
            }

            let meta = match fs::symlink_metadata(&path) {
                Ok(m) => m,
                Err(_) => {
                    // Path gone (e.g. deleted after event): correct bytes and drop from state.
                    if let Some(old) = self.hot_sizes.remove(&path) {
                        self.tier_state.adjust_hot_bytes(old, 0);
                    } else if let Some(sz) = self.cold_sizes.remove(&path) {
                        self.tier_state.adjust_cold_bytes(0, sz, 0);
                    }
                    self.queue.retain(|p| p != &path);
                    continue;
                }
            };

            if meta.is_dir() {
                continue;
            }
            // Path in cold (symlink): make room by evicting LRU, then promote to hot.
            if meta.file_type().is_symlink() {
                let need = fs::read_link(&path)
                    .ok()
                    .and_then(|t| {
                        let abs = if t.is_absolute() { t } else { path.parent().unwrap_or(Path::new("/")).join(t) };
                        fs::metadata(&abs).ok().map(|m| m.len())
                    })
                    .unwrap_or(0);
                while self.tier_state.hot_bytes_left() < need {
                    // Evict one LRU; if back was already deleted, just correct bytes.
                    let Some(back) = self.queue.pop_back() else { break };
                    if back.exists() {
                        let sz = self.tier_state.move_to_tier(&back, &cold)?;
                        if sz > 0 {
                            self.tier_state.adjust_hot_bytes(sz, 0);
                            self.tier_state.adjust_cold_bytes(0, 0, sz);
                        }
                        self.hot_sizes.remove(&back);
                        self.cold_sizes.insert(back.clone(), sz);
                        evicted_room += 1;
                    } else if let Some(old) = self.hot_sizes.remove(&back) {
                        self.tier_state.adjust_hot_bytes(old, 0);
                    } else if let Some(sz) = self.cold_sizes.remove(&back) {
                        self.tier_state.adjust_cold_bytes(0, sz, 0);
                    }
                }
                if self.tier_state.hot_bytes_left() < need {
                    return Err(format!("not enough hot capacity to promote {:?} (need {} bytes)", path, need).into());
                }
                let moved = self.tier_state.move_to_tier(&path, &hot_root)?;
                let sz = fs::metadata(&path).map(|m| m.len()).unwrap_or(moved);
                if sz > 0 {
                    self.tier_state.adjust_hot_bytes(0, sz);
                    self.tier_state.adjust_cold_bytes(0, sz, 0);
                }
                self.hot_sizes.insert(path.clone(), sz);
                self.cold_sizes.remove(&path);
                self.queue.retain(|p| p != &path);
                promoted += 1;
                self.queue.push_front(path);
            } else {
                // Path in hot (regular file): track size if new, then move to front (MRU).
                if !self.hot_sizes.contains_key(&path) {
                    if let Ok(sz) = fs::metadata(&path).map(|m| m.len()) {
                        self.tier_state.adjust_hot_bytes(0, sz);
                        self.hot_sizes.insert(path.clone(), sz);
                        new_in_hot += 1;
                    }
                }
                self.queue.retain(|p| p != &path);
                self.queue.push_front(path);
            }
        }

        // Over capacity (e.g. in-place growth): evict LRU until under cap. Back may be gone — correct bytes if tracked.
        let mut evicted = 0u32;
        while self.tier_state.hot_bytes_left() == 0 {
            let Some(back) = self.queue.pop_back() else { break };
            if back.exists() {
                let sz = self.tier_state.move_to_tier(&back, &cold)?;
                if sz > 0 {
                    self.tier_state.adjust_hot_bytes(sz, 0);
                    self.tier_state.adjust_cold_bytes(0, 0, sz);
                }
                self.hot_sizes.remove(&back);
                self.cold_sizes.insert(back.clone(), sz);
                evicted += 1;
            } else if let Some(old) = self.hot_sizes.remove(&back) {
                self.tier_state.adjust_hot_bytes(old, 0);
            } else if let Some(sz) = self.cold_sizes.remove(&back) {
                self.tier_state.adjust_cold_bytes(0, sz, 0);
            }
        }

        if had_touches || evicted > 0 {
            policy_log::log_reorganize_done(
                "basic_lru",
                true,
                new_in_hot,
                promoted,
                evicted_room,
                evicted,
                self.tier_state.hot_bytes(),
                self.tier_state.cold_bytes(0),
            );
        }

        Ok(())
    }
}

#[cfg(test)]
#[cfg(unix)]
mod tests {
    use std::fs;
    use std::time::SystemTime;

    use super::*;
    use crate::policy_engine::FsEventKind;

    /// Tests feed the policy the same event stream the main loop would: we never run the watcher.
    /// "Touch via hot" = ingest an event with path = hot path; "touch via cold" = ingest an
    /// event with path = cold path (what the watcher reports when the user edits via the
    /// hot symlink). The policy only sees paths and kinds; it maps cold→hot and promotes.

    fn setup_dirs() -> (tempfile::TempDir, tempfile::TempDir) {
        (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap())
    }

    /// Over capacity: reorganize evicts LRU to cold.
    #[test]
    fn over_capacity_evicts_lru_to_cold() {
        let (hot_dir, cold_dir) = setup_dirs();
        let hot_root = fs::canonicalize(hot_dir.path()).unwrap();
        let cold_root = fs::canonicalize(cold_dir.path()).unwrap();
        fs::write(hot_root.join("f1"), b"tenbytes!!").unwrap();
        fs::write(hot_root.join("f2"), b"tenbytes!!").unwrap();
        let mut tier_state = TierState::new(hot_root.clone(), vec![cold_root.clone()], 15, vec![u64::MAX]);
        tier_state.init_bytes().unwrap();
        let mut policy = BasicLruPolicy::new(tier_state);
        policy.reorganize().unwrap();
        assert!(policy.tier_state.hot_bytes() <= 15);
        let symlink_count = [hot_root.join("f1"), hot_root.join("f2")]
            .iter()
            .filter(|p| fs::symlink_metadata(p).map(|m| m.file_type().is_symlink()).unwrap_or(false))
            .count();
        assert_eq!(symlink_count, 1);
        assert!(cold_root.join("f1").exists() || cold_root.join("f2").exists());
    }

    /// Touch hot path (ingest event with hot path) makes that file MRU; it stays in hot on next reorganize.
    #[test]
    fn touch_hot_path_mru_then_evict_lru() {
        let (hot_dir, cold_dir) = setup_dirs();
        let hot_root = fs::canonicalize(hot_dir.path()).unwrap();
        let cold_root = fs::canonicalize(cold_dir.path()).unwrap();
        fs::write(hot_root.join("a"), b"aaaaaaaaaa").unwrap();
        fs::write(hot_root.join("b"), b"bbbbbbbbbb").unwrap();
        let mut tier_state = TierState::new(hot_root.clone(), vec![cold_root.clone()], 15, vec![u64::MAX]);
        tier_state.init_bytes().unwrap();
        let mut policy = BasicLruPolicy::new(tier_state);
        policy.reorganize().unwrap();
        let hot_a = hot_root.join("a");
        let hot_b = hot_root.join("b");
        let (mru_path, lru_path) = if fs::symlink_metadata(&hot_a).map(|m| m.file_type().is_symlink()).unwrap_or(false) {
            (hot_b.clone(), hot_a)
        } else {
            (hot_a.clone(), hot_b)
        };
        policy.ingest(&[AccessEvent { path: mru_path.clone(), kind: FsEventKind::Modify, timestamp: SystemTime::now() }]);
        policy.reorganize().unwrap();
        // Touched file (MRU) stayed in hot; we did not evict it (still under cap).
        assert!(!fs::symlink_metadata(&mru_path).unwrap().file_type().is_symlink());
        // LRU (untouched) is in cold.
        assert!(fs::symlink_metadata(&lru_path).map(|m| m.file_type().is_symlink()).unwrap_or(false));
    }

    /// Remove event: policy drops path from cold_sizes / cold_bytes.
    #[test]
    fn remove_event_cleans_state() {
        let (hot_dir, cold_dir) = setup_dirs();
        let hot_root = fs::canonicalize(hot_dir.path()).unwrap();
        let cold_root = fs::canonicalize(cold_dir.path()).unwrap();
        fs::write(hot_root.join("f"), b"content").unwrap();
        let mut tier_state = TierState::new(hot_root.clone(), vec![cold_root.clone()], 5, vec![u64::MAX]);
        tier_state.init_bytes().unwrap();
        let mut policy = BasicLruPolicy::new(tier_state);
        policy.reorganize().unwrap();
        let hot_path = hot_root.join("f");
        let cold_before = policy.tier_state.cold_bytes(0);
        fs::remove_file(&hot_path).unwrap();
        fs::remove_file(cold_root.join("f")).unwrap();
        policy.ingest(&[AccessEvent { path: hot_path, kind: FsEventKind::Remove, timestamp: SystemTime::now() }]);
        policy.reorganize().unwrap();
        assert!(policy.tier_state.cold_bytes(0) < cold_before);
    }

    /// Cold-path event (simulate "edit via symlink") → policy maps to hot path and promotes.
    #[test]
    fn touch_via_cold_path_promotes_to_hot() {
        let (hot_dir, cold_dir) = setup_dirs();
        let hot_root = fs::canonicalize(hot_dir.path()).unwrap();
        let cold_root = fs::canonicalize(cold_dir.path()).unwrap();

        // Two files; capacity 15 so one stays in hot, one evicted
        fs::create_dir_all(hot_root.join("sub")).unwrap();
        fs::write(hot_root.join("sub/a"), b"aaaaaaaaaa").unwrap();
        fs::write(hot_root.join("sub/b"), b"bbbbbbbbbb").unwrap();

        let mut tier_state = TierState::new(
            hot_root.clone(),
            vec![cold_root.clone()],
            15,
            vec![u64::MAX],
        );
        tier_state.init_bytes().unwrap();

        let mut policy = BasicLruPolicy::new(tier_state);
        policy.reorganize().unwrap();

        // One file in cold (evicted), one in hot
        let cold_a = cold_root.join("sub/a");
        let cold_b = cold_root.join("sub/b");
        let (cold_path, hot_path) = if cold_a.exists() {
            (cold_a, hot_root.join("sub/a"))
        } else {
            (cold_b, hot_root.join("sub/b"))
        };

        let cold_path_canonical = fs::canonicalize(&cold_path).unwrap();
        policy.ingest(&[AccessEvent {
            path: cold_path_canonical,
            kind: FsEventKind::Modify,
            timestamp: SystemTime::now(),
        }]);

        policy.reorganize().unwrap();

        assert!(!fs::symlink_metadata(&hot_path).unwrap().file_type().is_symlink());
        assert!(!cold_path.exists());
    }
}
