//! LFU (Least Frequently Used) policy: one hot (capacity-limited), one cold.
//! Very closely mirrors `basic_lru`, but evicts the path with the lowest
//! touch count instead of the least-recently used. All path canonicalization,
//! byte accounting, and loop-prevention logic is copied from `basic_lru` to
//! keep behavior consistent and avoid subtle bugs.

use std::cmp::Reverse;
use std::collections::{BinaryHeap, HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use crate::policy_engine::{AccessEvent, FsEventKind, PolicyEngine, PolicyStats, TierState};
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
pub struct LfuPolicy {
    pub tier_state: TierState,
    hot_sizes: HashMap<PathBuf, u64>,
    cold_sizes: HashMap<PathBuf, u64>,
    touched: Vec<(PathBuf, SystemTime)>,
    /// Paths we modified in the last reorganize (evicted or promoted).
    last_modified: HashSet<PathBuf>,
    /// Touch counts for files currently in hot.
    freqs: HashMap<PathBuf, u64>,
    /// Min-heap (via Reverse) of (freq, seq, path) for eviction. We use
    /// lazy deletion: on pop, skip entries whose freq no longer matches
    /// or whose path is no longer in hot.
    heap: BinaryHeap<Reverse<(u64, u64, PathBuf)>>,
    /// Monotonic counter to break ties in the heap ordering.
    seq_counter: u64,
    total_promotions: u64,
    total_demotions: u64,
    demotions_to_tier: Vec<u64>,
}

impl LfuPolicy {
    pub fn new(tier_state: TierState) -> Self {
        let n_cold = 1;
        Self {
            tier_state,
            hot_sizes: HashMap::new(),
            cold_sizes: HashMap::new(),
            touched: Vec::new(),
            last_modified: HashSet::new(),
            freqs: HashMap::new(),
            heap: BinaryHeap::new(),
            seq_counter: 0,
            total_promotions: 0,
            total_demotions: 0,
            demotions_to_tier: vec![0; n_cold],
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
                let rel = p
                    .strip_prefix(hot_root)
                    .or_else(|_| p.strip_prefix(dir))
                    .unwrap_or(p.as_path());
                out.push(hot_root.join(rel));
            }
        }
        Ok(out)
    }

    fn path_modified_last_reorganize(&self, path: &Path) -> bool {
        if self.last_modified.contains(path) {
            return true;
        }
        if let Some(cold) = self.tier_state.cold_root(0) {
            let cold_abs = canonical(cold);
            if path.starts_with(&cold_abs)
                && let Ok(rel) = path.strip_prefix(&cold_abs)
            {
                let logical_hot = canonical(self.tier_state.hot_root()).join(rel);
                if self.last_modified.contains(&logical_hot) {
                    return true;
                }
            }
        }
        false
    }

    fn bump_freq(&mut self, path: &Path) {
        let p = canonical(path);
        let entry = self.freqs.entry(p.clone()).or_insert(0);
        *entry = entry.saturating_add(1);
        self.seq_counter = self.seq_counter.wrapping_add(1);
        self.heap
            .push(Reverse((*entry, self.seq_counter, p.clone())));
    }

    /// Evict one LFU victim from hot to cold, skipping `exclude` if provided.
    /// Returns Ok(true) if a file was evicted, Ok(false) if no suitable victim.
    fn evict_one(
        &mut self,
        hot_root: &Path,
        cold: &Path,
        exclude: Option<&Path>,
    ) -> Result<bool, Box<dyn std::error::Error + Send + Sync>> {
        loop {
            let Some(Reverse((freq, _seq, path))) = self.heap.pop() else {
                return Ok(false);
            };
            if let Some(ex) = exclude
                && canonical(&path) == canonical(ex)
            {
                continue;
            }
            // Skip if path is no longer in hot_sizes or freq is stale.
            match self.hot_sizes.get(&path) {
                None => continue,
                Some(_) => {
                    if self.freqs.get(&path).copied().unwrap_or(0) != freq {
                        continue;
                    }
                }
            }

            let back = path.clone();
            if back.exists() {
                let rel = back
                    .strip_prefix(hot_root)
                    .unwrap_or_else(|_| Path::new(""));
                let cold_path = cold.join(rel);
                let sz = self.tier_state.move_to_tier(&back, cold)?;
                if sz > 0 {
                    self.tier_state.adjust_hot_bytes(sz, 0);
                    self.tier_state.adjust_cold_bytes(0, 0, sz);
                }
                self.hot_sizes.remove(&back);
                self.cold_sizes.insert(back.clone(), sz);
                self.last_modified.insert(canonical(&back));
                self.last_modified.insert(canonical(&cold_path));
                self.freqs.remove(&back);
                self.total_demotions += 1;
                self.demotions_to_tier[0] += 1;
            } else if let Some(old) = self.hot_sizes.remove(&back) {
                self.tier_state.adjust_hot_bytes(old, 0);
                self.freqs.remove(&back);
            } else if self.cold_sizes.remove(&back).is_some() {
                // Don't subtract cold_bytes: backing may still be in cold (e.g. path renamed).
                self.freqs.remove(&back);
            }
            return Ok(true);
        }
    }
}

impl PolicyEngine for LfuPolicy {
    fn validate_config(
        _hot: &Path,
        cold_storage: &[std::path::PathBuf],
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        if cold_storage.len() != 1 {
            return Err("lfu requires exactly one cold_storage tier".into());
        }
        Ok(())
    }

    fn ingest(&mut self, events: &[AccessEvent]) {
        for event in events {
            let path = canonical(&event.path);
            match event.kind {
                FsEventKind::Modify => {
                    if let Some(&old) = self.hot_sizes.get(&path)
                        && let Ok(new) = fs::metadata(&path).map(|m| m.len())
                    {
                        self.tier_state.adjust_hot_bytes(old, new);
                        self.hot_sizes.insert(path.clone(), new);
                    }
                }
                FsEventKind::Remove => {
                    if self.path_modified_last_reorganize(&path) {
                        continue;
                    }
                    if let Some(old) = self.hot_sizes.remove(&path) {
                        self.tier_state.adjust_hot_bytes(old, 0);
                        self.freqs.remove(&path);
                    }
                    // Leave cold_sizes as-is on Remove; reconcile will drop the entry and only
                    // subtract cold_bytes when the cold backing file is actually gone (not rename).
                }
                _ => {}
            }
        }
        // Build touched. Same 3-part filter as basic_lru to avoid loops.
        let hot_root = canonical(self.tier_state.hot_root());
        let cold_abs = self.tier_state.cold_root(0).map(canonical);
        let created_this_batch: HashSet<_> = events
            .iter()
            .filter(|e| e.kind == FsEventKind::Create)
            .map(|e| canonical(&e.path))
            .collect();
        self.touched = events
            .iter()
            .filter(|e| {
                let p = canonical(&e.path);
                let modify_after_create_on_cold = e.kind == FsEventKind::Modify
                    && created_this_batch.contains(&p)
                    && cold_abs.as_ref().is_some_and(|c| p.starts_with(c));
                if modify_after_create_on_cold {
                    return false;
                }
                let in_modified = self.last_modified.contains(&p);
                let under_hot = p.starts_with(&hot_root);
                let logical_hot_in_modified = cold_abs
                    .as_ref()
                    .and_then(|c| p.strip_prefix(c).ok().map(|rel| hot_root.join(rel)))
                    .as_ref()
                    .is_some_and(|h| self.last_modified.contains(h));

                let our_move = matches!(e.kind, FsEventKind::Create | FsEventKind::Remove);
                let our_symlink_modify = e.kind == FsEventKind::Modify && in_modified && under_hot;

                if (in_modified && (our_move || our_symlink_modify))
                    || (logical_hot_in_modified && our_move)
                {
                    // Exception: Create on a path we evicted (now in last_modified) can be a
                    // rename: symlink was moved to this path, so path exists as symlink — count as touch so we promote.
                    if e.kind == FsEventKind::Create
                        && under_hot
                        && fs::symlink_metadata(&p)
                            .map(|m| m.file_type().is_symlink())
                            .unwrap_or(false)
                    {
                        return true;
                    }
                    return false;
                }
                true
            })
            .map(|e| (canonical(&e.path), e.timestamp))
            .collect();
        policy_log::log_ingest("lfu", events.len());
    }

    fn reorganize(&mut self) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        let hot_root = canonical(self.tier_state.hot_root());
        let cold = self
            .tier_state
            .cold_root(0)
            .expect("one cold tier")
            .to_path_buf();
        let cold_abs = canonical(&cold);

        self.last_modified.clear();

        // Reconcile cold_sizes with filesystem so drift doesn't persist and affect policy decisions.
        let mut to_drop: Vec<(PathBuf, u64)> = Vec::new();
        for (p, &sz) in self.cold_sizes.iter() {
            if fs::symlink_metadata(p).is_err() {
                to_drop.push((p.clone(), sz));
            }
        }
        for (p, sz) in to_drop {
            self.cold_sizes.remove(&p);
            let rel = p.strip_prefix(&hot_root).unwrap_or_else(|_| Path::new(""));
            let cold_backing = cold.join(rel);
            if fs::metadata(&cold_backing).is_err() {
                self.tier_state.adjust_cold_bytes(0, sz, 0);
            }
            self.freqs.remove(&p);
        }

        // Reconcile hot_sizes: drop entries whose path no longer exists or is a symlink.
        let mut hot_drop: Vec<(PathBuf, u64)> = Vec::new();
        for (p, &sz) in self.hot_sizes.iter() {
            let drop = match fs::symlink_metadata(p) {
                Err(_) => true,
                Ok(m) => m.file_type().is_symlink(),
            };
            if drop {
                hot_drop.push((p.clone(), sz));
            }
        }
        for (p, sz) in hot_drop {
            self.hot_sizes.remove(&p);
            self.tier_state.adjust_hot_bytes(sz, 0);
            self.freqs.remove(&p);
        }

        // First run: seed hot_sizes/freqs from disk.
        if self.hot_sizes.is_empty() && self.freqs.is_empty() {
            for p in Self::list_hot_files(&hot_root, &hot_root)? {
                if let Ok(meta) = fs::symlink_metadata(&p)
                    && !meta.file_type().is_symlink()
                    && let Ok(sz) = fs::metadata(&p).map(|m| m.len())
                {
                    self.hot_sizes.insert(p.clone(), sz);
                    self.bump_freq(&p);
                }
            }
            policy_log::log_initial_fill("lfu", self.hot_sizes.len(), self.tier_state.hot_bytes());
        }

        // Process touches oldest-first so behavior is deterministic w.r.t event order.
        self.touched.sort_by(|a, b| a.1.cmp(&b.1));
        let mut touches = std::mem::take(&mut self.touched);
        let had_touches = !touches.is_empty();
        let mut new_in_hot = 0u32;
        let mut promoted = 0u32;
        let mut evicted_room = 0u32;

        for (mut path, _) in touches.drain(..) {
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
                    } else if self.cold_sizes.remove(&path).is_some() {
                        // Don't subtract cold_bytes: backing may still be in cold (e.g. path renamed).
                    }
                    self.freqs.remove(&path);
                    continue;
                }
            };

            if meta.is_dir() {
                continue;
            }

            // Path in cold (symlink): make room by evicting LFU, then promote to hot.
            if meta.file_type().is_symlink() {
                let need = fs::read_link(&path)
                    .ok()
                    .and_then(|t| {
                        let abs = if t.is_absolute() {
                            t
                        } else {
                            path.parent().unwrap_or(Path::new("/")).join(t)
                        };
                        fs::metadata(&abs).ok().map(|m| m.len())
                    })
                    .unwrap_or(0);
                while self.tier_state.hot_bytes_left() < need {
                    let evicted = self.evict_one(&hot_root, &cold, Some(&path))?;
                    if !evicted {
                        break;
                    }
                    evicted_room += 1;
                }
                if self.tier_state.hot_bytes_left() < need {
                    return Err(format!(
                        "not enough hot capacity to promote {:?} (need {} bytes)",
                        path, need
                    )
                    .into());
                }
                let cold_backing = fs::read_link(&path).ok().map(|t| {
                    if t.is_absolute() {
                        t
                    } else {
                        path.parent().unwrap_or(Path::new("/")).join(t)
                    }
                });
                let moved = self.tier_state.move_to_tier(&path, &hot_root)?;
                let sz = fs::metadata(&path).map(|m| m.len()).unwrap_or(moved);
                if sz > 0 {
                    self.tier_state.adjust_hot_bytes(0, sz);
                    self.tier_state.adjust_cold_bytes(0, sz, 0);
                }
                self.hot_sizes.insert(path.clone(), sz);
                self.cold_sizes.remove(&path);
                self.last_modified.insert(canonical(&path));
                if let Some(ref cb) = cold_backing {
                    self.last_modified.insert(canonical(cb));
                }
                self.bump_freq(&path);
                promoted += 1;
                self.total_promotions += 1;
            } else {
                // Path in hot (regular file): track size if new, then bump frequency.
                if !self.hot_sizes.contains_key(&path)
                    && let Ok(sz) = fs::metadata(&path).map(|m| m.len())
                {
                    self.tier_state.adjust_hot_bytes(0, sz);
                    self.hot_sizes.insert(path.clone(), sz);
                    new_in_hot += 1;
                }
                self.bump_freq(&path);
            }
        }

        // Over capacity (e.g. in-place growth): evict LFU until under cap.
        let mut evicted = 0u32;
        while self.tier_state.hot_bytes_left() == 0 {
            let did = self.evict_one(&hot_root, &cold, None)?;
            if !did {
                break;
            }
            evicted += 1;
        }

        if had_touches || evicted > 0 {
            policy_log::log_reorganize_done(policy_log::ReorganizeDoneParams {
                policy_name: "lfu",
                should_log: true,
                new_in_hot,
                promoted,
                evicted_room,
                evicted_cap: evicted,
                hot_bytes: self.tier_state.hot_bytes(),
                cold_bytes: self.tier_state.cold_bytes(0),
            });
        }

        Ok(())
    }

    fn stats(&self) -> PolicyStats {
        PolicyStats {
            promotions: self.total_promotions,
            demotions: self.total_demotions,
            demotions_to_tier: self.demotions_to_tier.clone(),
            bytes_written_to_tier: self.tier_state.bytes_written_to_tier().to_vec(),
        }
    }
}

#[cfg(test)]
#[cfg(unix)]
mod tests {
    use super::*;
    use std::fs;

    fn setup_dirs() -> (tempfile::TempDir, tempfile::TempDir) {
        (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap())
    }

    /// Over capacity: reorganize evicts something to cold.
    #[test]
    fn over_capacity_evicts_to_cold() {
        let (hot_dir, cold_dir) = setup_dirs();
        let hot_root = fs::canonicalize(hot_dir.path()).unwrap();
        let cold_root = fs::canonicalize(cold_dir.path()).unwrap();
        fs::write(hot_root.join("f1"), b"tenbytes!!").unwrap();
        fs::write(hot_root.join("f2"), b"tenbytes!!").unwrap();
        let mut tier_state = TierState::new(
            hot_root.clone(),
            vec![cold_root.clone()],
            15,
            vec![u64::MAX],
        );
        tier_state.init_bytes().unwrap();
        let mut policy = LfuPolicy::new(tier_state);
        policy.reorganize().unwrap();
        assert!(policy.tier_state.hot_bytes() <= 15);
        let symlink_count = [hot_root.join("f1"), hot_root.join("f2")]
            .iter()
            .filter(|p| {
                fs::symlink_metadata(p)
                    .map(|m| m.file_type().is_symlink())
                    .unwrap_or(false)
            })
            .count();
        assert_eq!(symlink_count, 1);
        assert!(cold_root.join("f1").exists() || cold_root.join("f2").exists());
    }

    /// Stats track demotions on over-capacity eviction.
    #[test]
    fn stats_track_demotions() {
        let (hot_dir, cold_dir) = setup_dirs();
        let hot_root = fs::canonicalize(hot_dir.path()).unwrap();
        let cold_root = fs::canonicalize(cold_dir.path()).unwrap();
        fs::write(hot_root.join("f1"), b"tenbytes!!").unwrap();
        fs::write(hot_root.join("f2"), b"tenbytes!!").unwrap();
        let mut tier_state = TierState::new(
            hot_root.clone(),
            vec![cold_root.clone()],
            15,
            vec![u64::MAX],
        );
        tier_state.init_bytes().unwrap();
        let mut policy = LfuPolicy::new(tier_state);
        policy.reorganize().unwrap();

        let stats = policy.stats();
        assert_eq!(stats.demotions, 1);
        assert_eq!(stats.demotions_to_tier, vec![1]);
    }
}
