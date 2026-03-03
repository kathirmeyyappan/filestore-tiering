//! ARC (Adaptive Replacement Cache): self-tuning policy that balances recency and frequency.
//!
//! Maintains four lists — T1 (recency), T2 (frequency), B1/B2 (ghost lists for evicted entries) —
//! and an adaptive parameter `p` that shifts the balance based on observed ghost hits.
//! One hot tier (capacity-limited), one cold tier.

use std::collections::{HashMap, HashSet, VecDeque};
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
pub struct ArcPolicy {
    pub tier_state: TierState,
    /// Hot files accessed once since entering hot (recency). Front = MRU, Back = LRU.
    t1: VecDeque<PathBuf>,
    /// Hot files accessed more than once (frequency). Front = MRU, Back = LRU.
    t2: VecDeque<PathBuf>,
    /// Ghost list: metadata for files evicted from T1 (now in cold). Front = MRU, Back = LRU.
    b1: VecDeque<PathBuf>,
    /// Ghost list: metadata for files evicted from T2 (now in cold). Front = MRU, Back = LRU.
    b2: VecDeque<PathBuf>,
    /// Adaptive target: desired number of entries in T1. Shifts toward recency on B1 hits,
    /// toward frequency on B2 hits.
    p: usize,
    /// File sizes for entries in hot (T1 ∪ T2), keyed by canonical hot path.
    hot_sizes: HashMap<PathBuf, u64>,
    /// File sizes for entries in cold, keyed by canonical hot path.
    cold_sizes: HashMap<PathBuf, u64>,
    /// Remembered file sizes for ghost entries (B1 ∪ B2), keyed by canonical hot path.
    ghost_sizes: HashMap<PathBuf, u64>,
    /// Events to process in reorganize (path, timestamp), built by ingest.
    touched: Vec<(PathBuf, SystemTime)>,
    /// Paths we moved in the last reorganize (evict or promote). Used to filter out
    /// watcher events caused by our own filesystem operations.
    last_modified: HashSet<PathBuf>,
    total_promotions: u64,
    total_demotions: u64,
    demotions_to_tier: Vec<u64>,
}

impl ArcPolicy {
    pub fn new(tier_state: TierState) -> Self {
        Self {
            tier_state,
            t1: VecDeque::new(),
            t2: VecDeque::new(),
            b1: VecDeque::new(),
            b2: VecDeque::new(),
            p: 0,
            hot_sizes: HashMap::new(),
            cold_sizes: HashMap::new(),
            ghost_sizes: HashMap::new(),
            touched: Vec::new(),
            last_modified: HashSet::new(),
            total_promotions: 0,
            total_demotions: 0,
            demotions_to_tier: vec![0; 1],
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

    /// Check if `path` is in `list`; return its index if found.
    fn find_in(list: &VecDeque<PathBuf>, path: &Path) -> Option<usize> {
        list.iter().position(|p| p == path)
    }

    /// Remove `path` from `list` if present.
    fn remove_from(list: &mut VecDeque<PathBuf>, path: &Path) {
        list.retain(|p| p != path);
    }

    /// The ARC REPLACE subroutine. Evicts one file from T1 or T2 to cold based on the
    /// adaptive parameter `p` and whether the triggering request was a B2 ghost hit.
    ///
    /// Returns Ok(true) if a file was evicted, Ok(false) if both T1 and T2 are empty.
    fn replace(
        &mut self,
        in_b2: bool,
        hot_root: &Path,
        cold: &Path,
    ) -> Result<bool, Box<dyn std::error::Error + Send + Sync>> {
        let evict_from_t1 =
            !self.t1.is_empty() && (self.t1.len() > self.p || (in_b2 && self.t1.len() == self.p));

        if evict_from_t1 {
            self.evict_lru_from(&ListId::T1, hot_root, cold)?;
            Ok(true)
        } else if !self.t2.is_empty() {
            self.evict_lru_from(&ListId::T2, hot_root, cold)?;
            Ok(true)
        } else if !self.t1.is_empty() {
            // Fallback: p says "evict from T2" but T2 is empty; evict from T1 instead.
            // This can happen because capacity is byte-based while p is count-based.
            self.evict_lru_from(&ListId::T1, hot_root, cold)?;
            Ok(true)
        } else {
            Ok(false)
        }
    }

    /// Evict the LRU entry from the specified list (T1 or T2), move file to cold,
    /// and add the ghost entry to B1 or B2 respectively.
    fn evict_lru_from(
        &mut self,
        list: &ListId,
        hot_root: &Path,
        cold: &Path,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        let (src, ghost_dst) = match list {
            ListId::T1 => (&mut self.t1, ListId::B1),
            ListId::T2 => (&mut self.t2, ListId::B2),
            _ => return Ok(()),
        };
        let Some(victim) = src.pop_back() else {
            return Ok(());
        };

        if victim.exists() {
            let rel = victim
                .strip_prefix(hot_root)
                .unwrap_or_else(|_| Path::new(""));
            let cold_path = cold.join(rel);
            let sz = self.tier_state.move_to_tier(&victim, cold)?;
            if sz > 0 {
                self.tier_state.adjust_hot_bytes(sz, 0);
                self.tier_state.adjust_cold_bytes(0, 0, sz);
            }
            let sz = self.hot_sizes.remove(&victim).unwrap_or(sz);
            self.cold_sizes.insert(victim.clone(), sz);
            self.ghost_sizes.insert(victim.clone(), sz);
            self.last_modified.insert(canonical(&victim));
            self.last_modified.insert(canonical(&cold_path));
            self.total_demotions += 1;
            self.demotions_to_tier[0] += 1;

            // Add ghost entry
            match ghost_dst {
                ListId::B1 => self.b1.push_front(victim),
                ListId::B2 => self.b2.push_front(victim),
                _ => {}
            }
        } else if let Some(old) = self.hot_sizes.remove(&victim) {
            self.tier_state.adjust_hot_bytes(old, 0);
        }

        Ok(())
    }

    /// Promote a file from cold to hot, updating all state. Returns the size promoted.
    fn promote(
        &mut self,
        path: &Path,
        hot_root: &Path,
    ) -> Result<u64, Box<dyn std::error::Error + Send + Sync>> {
        let cold_backing = fs::read_link(path).ok().map(|t| {
            if t.is_absolute() {
                t
            } else {
                path.parent().unwrap_or(Path::new("/")).join(t)
            }
        });
        let moved = self.tier_state.move_to_tier(path, hot_root)?;
        let sz = fs::metadata(path).map(|m| m.len()).unwrap_or(moved);
        if sz > 0 {
            self.tier_state.adjust_hot_bytes(0, sz);
            self.tier_state.adjust_cold_bytes(0, sz, 0);
        }
        self.hot_sizes.insert(path.to_path_buf(), sz);
        self.cold_sizes.remove(path);
        self.ghost_sizes.remove(path);
        self.last_modified.insert(canonical(path));
        if let Some(ref cb) = cold_backing {
            self.last_modified.insert(canonical(cb));
        }
        self.total_promotions += 1;
        Ok(sz)
    }

    /// Get the size of the file a symlink points to.
    fn symlink_target_size(path: &Path) -> u64 {
        fs::read_link(path)
            .ok()
            .and_then(|t| {
                let abs = if t.is_absolute() {
                    t
                } else {
                    path.parent().unwrap_or(Path::new("/")).join(t)
                };
                fs::metadata(&abs).ok().map(|m| m.len())
            })
            .unwrap_or(0)
    }

    /// Trim ghost lists so each has at most `max(|T1|+|T2|, 16)` entries.
    fn trim_ghost_lists(&mut self) {
        let cap = std::cmp::max(self.t1.len() + self.t2.len(), 16);
        while self.b1.len() > cap {
            if let Some(old) = self.b1.pop_back() {
                self.ghost_sizes.remove(&old);
            }
        }
        while self.b2.len() > cap {
            if let Some(old) = self.b2.pop_back() {
                self.ghost_sizes.remove(&old);
            }
        }
    }

    /// Clamp p to [0, |T1|+|T2|].
    fn clamp_p(&mut self) {
        let c = self.t1.len() + self.t2.len();
        if self.p > c {
            self.p = c;
        }
    }

    #[cfg(test)]
    pub(crate) fn t1_len(&self) -> usize {
        self.t1.len()
    }
    #[cfg(test)]
    pub(crate) fn t2_len(&self) -> usize {
        self.t2.len()
    }
    #[cfg(test)]
    pub(crate) fn p_value(&self) -> usize {
        self.p
    }
    #[cfg(test)]
    pub(crate) fn b1_len(&self) -> usize {
        self.b1.len()
    }
    #[cfg(test)]
    pub(crate) fn b2_len(&self) -> usize {
        self.b2.len()
    }

    /// Validate all ARC invariants. Panics with a descriptive message on any violation.
    #[cfg(test)]
    pub(crate) fn assert_invariants(&self) {
        use std::collections::HashSet;

        let t1_set: HashSet<_> = self.t1.iter().collect();
        let t2_set: HashSet<_> = self.t2.iter().collect();
        let b1_set: HashSet<_> = self.b1.iter().collect();
        let b2_set: HashSet<_> = self.b2.iter().collect();

        // No duplicates within a single list
        assert_eq!(t1_set.len(), self.t1.len(), "T1 has duplicate entries");
        assert_eq!(t2_set.len(), self.t2.len(), "T2 has duplicate entries");
        assert_eq!(b1_set.len(), self.b1.len(), "B1 has duplicate entries");
        assert_eq!(b2_set.len(), self.b2.len(), "B2 has duplicate entries");

        // Pairwise disjoint
        assert!(
            t1_set.is_disjoint(&t2_set),
            "T1 and T2 share entries: {:?}",
            t1_set.intersection(&t2_set).collect::<Vec<_>>()
        );
        assert!(
            t1_set.is_disjoint(&b1_set),
            "T1 and B1 share entries: {:?}",
            t1_set.intersection(&b1_set).collect::<Vec<_>>()
        );
        assert!(
            t1_set.is_disjoint(&b2_set),
            "T1 and B2 share entries: {:?}",
            t1_set.intersection(&b2_set).collect::<Vec<_>>()
        );
        assert!(
            t2_set.is_disjoint(&b1_set),
            "T2 and B1 share entries: {:?}",
            t2_set.intersection(&b1_set).collect::<Vec<_>>()
        );
        assert!(
            t2_set.is_disjoint(&b2_set),
            "T2 and B2 share entries: {:?}",
            t2_set.intersection(&b2_set).collect::<Vec<_>>()
        );
        assert!(
            b1_set.is_disjoint(&b2_set),
            "B1 and B2 share entries: {:?}",
            b1_set.intersection(&b2_set).collect::<Vec<_>>()
        );

        // hot_sizes keys = T1 ∪ T2 (both directions)
        let hot_list_set: HashSet<_> = t1_set.iter().chain(t2_set.iter()).cloned().collect();
        let hot_sizes_set: HashSet<_> = self.hot_sizes.keys().collect();
        for p in &hot_list_set {
            assert!(
                hot_sizes_set.contains(p),
                "path in T1/T2 but not in hot_sizes: {:?}",
                p
            );
        }
        for p in &hot_sizes_set {
            assert!(
                hot_list_set.contains(p),
                "path in hot_sizes but not in T1/T2: {:?}",
                p
            );
        }

        // No path in both hot_sizes and cold_sizes
        for p in self.hot_sizes.keys() {
            assert!(
                !self.cold_sizes.contains_key(p),
                "path in both hot_sizes and cold_sizes: {:?}",
                p
            );
        }

        // p bounded
        let c = self.t1.len() + self.t2.len();
        assert!(self.p <= c, "p ({}) exceeds |T1|+|T2| ({})", self.p, c);

        // Ghost lists bounded
        let ghost_cap = std::cmp::max(c, 16);
        assert!(
            self.b1.len() <= ghost_cap,
            "|B1| ({}) exceeds ghost cap ({})",
            self.b1.len(),
            ghost_cap
        );
        assert!(
            self.b2.len() <= ghost_cap,
            "|B2| ({}) exceeds ghost cap ({})",
            self.b2.len(),
            ghost_cap
        );
    }
}

/// Which ARC list an operation targets.
enum ListId {
    T1,
    T2,
    B1,
    B2,
}

impl PolicyEngine for ArcPolicy {
    fn validate_config(
        _hot: &Path,
        cold_storage: &[PathBuf],
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        if cold_storage.len() != 1 {
            return Err("arc requires exactly one cold_storage tier".into());
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
                        Self::remove_from(&mut self.t1, &path);
                        Self::remove_from(&mut self.t2, &path);
                    }
                }
                _ => {}
            }
        }
        // Build touched list with the same 3-part filter as basic_lru to prevent loops.
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
        policy_log::log_ingest("arc", events.len());
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

        // ── Reconcile cold_sizes ──
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
                // Cold backing is truly gone; remove ghost entry too
                Self::remove_from(&mut self.b1, &p);
                Self::remove_from(&mut self.b2, &p);
                self.ghost_sizes.remove(&p);
            }
        }

        // ── Reconcile hot_sizes and T1/T2 ──
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
            Self::remove_from(&mut self.t1, &p);
            Self::remove_from(&mut self.t2, &p);
        }

        // ── Initial fill: seed T1 from disk if both T1 and T2 are empty ──
        if self.t1.is_empty() && self.t2.is_empty() {
            for p in Self::list_hot_files(&hot_root, &hot_root)? {
                if let Ok(meta) = fs::symlink_metadata(&p)
                    && !meta.file_type().is_symlink()
                    && let Ok(sz) = fs::metadata(&p).map(|m| m.len())
                {
                    self.hot_sizes.insert(p.clone(), sz);
                }
                self.t1.push_back(p);
            }
            policy_log::log_initial_fill("arc", self.t1.len(), self.tier_state.hot_bytes());
        }

        // ── Process touches oldest-first ──
        // Move touched out of self so we can call &mut self methods in the loop.
        let mut touches = std::mem::take(&mut self.touched);
        touches.sort_by(|a, b| a.1.cmp(&b.1));
        let had_touches = !touches.is_empty();
        let mut new_in_hot = 0u32;
        let mut promoted = 0u32;
        let mut evicted_room = 0u32;

        for (mut path, _) in touches.drain(..) {
            // Map cold paths to logical hot paths
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
                    if let Some(old) = self.hot_sizes.remove(&path) {
                        self.tier_state.adjust_hot_bytes(old, 0);
                    } else if self.cold_sizes.remove(&path).is_some() {
                        // Don't subtract cold_bytes: backing may still exist (rename).
                    }
                    Self::remove_from(&mut self.t1, &path);
                    Self::remove_from(&mut self.t2, &path);
                    Self::remove_from(&mut self.b1, &path);
                    Self::remove_from(&mut self.b2, &path);
                    self.ghost_sizes.remove(&path);
                    continue;
                }
            };

            if meta.is_dir() {
                continue;
            }

            // ── Case 1: Cache hit in T1 → promote to MRU of T2 ──
            if Self::find_in(&self.t1, &path).is_some() {
                Self::remove_from(&mut self.t1, &path);
                self.t2.push_front(path);
                continue;
            }

            // ── Case 2: Cache hit in T2 → move to MRU of T2 ──
            if Self::find_in(&self.t2, &path).is_some() {
                Self::remove_from(&mut self.t2, &path);
                self.t2.push_front(path);
                continue;
            }

            // ── Case 3: Ghost hit in B1 → adapt p upward, REPLACE, promote ──
            if Self::find_in(&self.b1, &path).is_some() && meta.file_type().is_symlink() {
                // Adapt: increase p (favor recency more)
                let delta = if self.b1.is_empty() {
                    1
                } else {
                    std::cmp::max(1, self.b2.len() / self.b1.len())
                };
                let c = self.t1.len() + self.t2.len();
                self.p = std::cmp::min(c.saturating_add(1), self.p.saturating_add(delta));

                let need = Self::symlink_target_size(&path);
                // REPLACE loop: evict until we have room
                while self.tier_state.hot_bytes_left() < need {
                    let evicted = self.replace(false, &hot_root, &cold)?;
                    if !evicted {
                        break;
                    }
                    evicted_room += 1;
                }

                if self.tier_state.hot_bytes_left() >= need {
                    Self::remove_from(&mut self.b1, &path);
                    self.promote(&path, &hot_root)?;
                    self.t2.push_front(path);
                    promoted += 1;
                }
                self.clamp_p();
                continue;
            }

            // ── Case 4: Ghost hit in B2 → adapt p downward, REPLACE, promote ──
            if Self::find_in(&self.b2, &path).is_some() && meta.file_type().is_symlink() {
                // Adapt: decrease p (favor frequency more)
                let delta = if self.b2.is_empty() {
                    1
                } else {
                    std::cmp::max(1, self.b1.len() / self.b2.len())
                };
                self.p = self.p.saturating_sub(delta);

                let need = Self::symlink_target_size(&path);
                while self.tier_state.hot_bytes_left() < need {
                    let evicted = self.replace(true, &hot_root, &cold)?;
                    if !evicted {
                        break;
                    }
                    evicted_room += 1;
                }

                if self.tier_state.hot_bytes_left() >= need {
                    Self::remove_from(&mut self.b2, &path);
                    self.promote(&path, &hot_root)?;
                    self.t2.push_front(path);
                    promoted += 1;
                }
                self.clamp_p();
                continue;
            }

            // ── Case 5: Symlink not in any ghost list (untracked cold file) → promote to T2 ──
            if meta.file_type().is_symlink() {
                let need = Self::symlink_target_size(&path);
                while self.tier_state.hot_bytes_left() < need {
                    let evicted = self.replace(false, &hot_root, &cold)?;
                    if !evicted {
                        break;
                    }
                    evicted_room += 1;
                }
                if self.tier_state.hot_bytes_left() >= need {
                    self.promote(&path, &hot_root)?;
                    self.t2.push_front(path);
                    promoted += 1;
                }
                self.clamp_p();
                continue;
            }

            // ── Case 6: Regular file not in T1 or T2 (new file) → add to T1 ──
            if !self.hot_sizes.contains_key(&path) {
                if let Ok(sz) = fs::metadata(&path).map(|m| m.len()) {
                    self.tier_state.adjust_hot_bytes(0, sz);
                    self.hot_sizes.insert(path.clone(), sz);
                    new_in_hot += 1;
                }
                self.t1.push_front(path);
            }
        }

        // ── Evict over capacity ──
        let mut evicted = 0u32;
        while self.tier_state.hot_bytes_left() == 0 {
            let did_evict = self.replace(false, &hot_root, &cold)?;
            if !did_evict {
                break;
            }
            evicted += 1;
        }

        // ── Trim ghost lists ──
        self.trim_ghost_lists();
        self.clamp_p();

        if had_touches || evicted > 0 {
            policy_log::log_reorganize_done(policy_log::ReorganizeDoneParams {
                policy_name: "arc",
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
    use std::fs;
    use std::time::SystemTime;

    use super::*;
    use crate::policy_engine::FsEventKind;

    fn setup_dirs() -> (tempfile::TempDir, tempfile::TempDir) {
        (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap())
    }

    /// Over capacity: reorganize evicts to cold.
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
        let mut policy = ArcPolicy::new(tier_state);
        policy.reorganize().unwrap();
        policy.assert_invariants();
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

    /// Re-accessing a file in T1 promotes it to T2. After that, a new eviction
    /// under capacity pressure should evict from T1 (the file that was never re-accessed).
    #[test]
    fn touch_hot_file_promotes_t1_to_t2() {
        let (hot_dir, cold_dir) = setup_dirs();
        let hot_root = fs::canonicalize(hot_dir.path()).unwrap();
        let cold_root = fs::canonicalize(cold_dir.path()).unwrap();
        // Single file, plenty of capacity — just test T1→T2 move
        fs::write(hot_root.join("a"), b"aaaaaaaaaa").unwrap();
        let mut tier_state = TierState::new(
            hot_root.clone(),
            vec![cold_root.clone()],
            100,
            vec![u64::MAX],
        );
        tier_state.init_bytes().unwrap();
        let mut policy = ArcPolicy::new(tier_state);
        policy.reorganize().unwrap();

        // File starts in T1
        assert_eq!(policy.t1_len(), 1);
        assert_eq!(policy.t2_len(), 0);
        policy.assert_invariants();

        // Touch it → should move to T2
        policy.ingest(&[AccessEvent {
            path: hot_root.join("a"),
            kind: FsEventKind::Modify,
            timestamp: SystemTime::now(),
        }]);
        policy.reorganize().unwrap();

        assert_eq!(policy.t1_len(), 0);
        assert_eq!(policy.t2_len(), 1);
        policy.assert_invariants();
    }

    /// Ghost hit on B1 (evicted from T1) increases p.
    #[test]
    fn ghost_hit_b1_adapts_p_upward() {
        let (hot_dir, cold_dir) = setup_dirs();
        let hot_root = fs::canonicalize(hot_dir.path()).unwrap();
        let cold_root = fs::canonicalize(cold_dir.path()).unwrap();
        // Two files, capacity for one → one gets evicted to T1's ghost (B1)
        fs::write(hot_root.join("a"), b"aaaaaaaaaa").unwrap();
        fs::write(hot_root.join("b"), b"bbbbbbbbbb").unwrap();
        let mut tier_state = TierState::new(
            hot_root.clone(),
            vec![cold_root.clone()],
            15,
            vec![u64::MAX],
        );
        tier_state.init_bytes().unwrap();
        let mut policy = ArcPolicy::new(tier_state);
        policy.reorganize().unwrap();

        // One file evicted → in B1
        assert_eq!(policy.t1.len() + policy.t2.len(), 1);
        assert_eq!(policy.b1.len(), 1);
        let p_before = policy.p_value();

        // Find which file was evicted (is now a symlink)
        let evicted = if fs::symlink_metadata(hot_root.join("a"))
            .map(|m| m.file_type().is_symlink())
            .unwrap_or(false)
        {
            hot_root.join("a")
        } else {
            hot_root.join("b")
        };

        // Touch the evicted file (ghost hit on B1 via cold path)
        let cold_name = evicted.file_name().unwrap();
        let cold_path = fs::canonicalize(cold_root.join(cold_name)).unwrap();
        policy.ingest(&[AccessEvent {
            path: cold_path,
            kind: FsEventKind::Modify,
            timestamp: SystemTime::now(),
        }]);
        policy.reorganize().unwrap();

        assert!(
            policy.p_value() > p_before,
            "p should increase on B1 ghost hit: was {}, now {}",
            p_before,
            policy.p_value()
        );
        policy.assert_invariants();
    }

    /// Ghost hit on B2 (evicted from T2) decreases p.
    ///
    /// Flow: two files, cap for one → one evicted (T1→B1). Touch the B1 ghost →
    /// B1 hit promotes it to T2, evicts the other to B1. Touch the new B1 ghost →
    /// another B1 hit promotes it, evicts the T2 entry to B2. Now touch the B2 ghost → p decreases.
    #[test]
    fn ghost_hit_b2_adapts_p_downward() {
        let (hot_dir, cold_dir) = setup_dirs();
        let hot_root = fs::canonicalize(hot_dir.path()).unwrap();
        let cold_root = fs::canonicalize(cold_dir.path()).unwrap();

        fs::write(hot_root.join("a"), b"aaaaaaaaaa").unwrap();
        fs::write(hot_root.join("b"), b"bbbbbbbbbb").unwrap();
        let mut tier_state = TierState::new(
            hot_root.clone(),
            vec![cold_root.clone()],
            15,
            vec![u64::MAX],
        );
        tier_state.init_bytes().unwrap();
        let mut policy = ArcPolicy::new(tier_state);
        policy.reorganize().unwrap();

        // Step 1: One file evicted from T1 to B1. Identify which.
        let (evicted1, staying1) = if fs::symlink_metadata(hot_root.join("a"))
            .map(|m| m.file_type().is_symlink())
            .unwrap_or(false)
        {
            ("a", "b")
        } else {
            ("b", "a")
        };
        assert_eq!(policy.b1.len(), 1);

        // Step 2: Touch evicted1 via cold → B1 ghost hit. Promotes evicted1 to T2,
        // evicts staying1 from T1 to B1.
        let cold_path1 = fs::canonicalize(cold_root.join(evicted1)).unwrap();
        policy.ingest(&[AccessEvent {
            path: cold_path1,
            kind: FsEventKind::Modify,
            timestamp: SystemTime::now(),
        }]);
        policy.reorganize().unwrap();
        assert!(
            policy.t2.contains(&hot_root.join(evicted1)),
            "{} should be in T2 after B1 ghost hit",
            evicted1
        );
        assert!(
            policy.b1.contains(&hot_root.join(staying1)),
            "{} should be in B1 after eviction",
            staying1
        );

        // Step 3: Touch staying1 via cold → another B1 ghost hit. Promotes staying1 to T2.
        // Now evicted1 in T2 must be evicted. p was increased by B1 hits, so
        // |T1|=0 <= p → REPLACE evicts from T2 → evicted1 goes to B2.
        let cold_path2 = fs::canonicalize(cold_root.join(staying1)).unwrap();
        policy.ingest(&[AccessEvent {
            path: cold_path2,
            kind: FsEventKind::Modify,
            timestamp: SystemTime::now(),
        }]);
        policy.reorganize().unwrap();
        assert!(
            policy.b2.contains(&hot_root.join(evicted1)),
            "{} should be in B2 (evicted from T2)",
            evicted1
        );

        // Step 4: Set p high so we can observe decrease on B2 hit
        policy.p = 10;
        let p_before = policy.p_value();

        // Touch the B2 ghost via cold path
        let cold_path3 = fs::canonicalize(cold_root.join(evicted1)).unwrap();
        policy.ingest(&[AccessEvent {
            path: cold_path3,
            kind: FsEventKind::Modify,
            timestamp: SystemTime::now(),
        }]);
        policy.reorganize().unwrap();

        assert!(
            policy.p_value() < p_before,
            "p should decrease on B2 ghost hit: was {}, now {}",
            p_before,
            policy.p_value()
        );
        policy.assert_invariants();
    }

    /// Events from our own eviction must not cause re-promotion (infinite loop prevention).
    #[test]
    fn no_loop_when_ingest_events_from_our_own_eviction() {
        let (hot_dir, cold_dir) = setup_dirs();
        let hot_root = fs::canonicalize(hot_dir.path()).unwrap();
        let cold_root = fs::canonicalize(cold_dir.path()).unwrap();
        fs::write(hot_root.join("a"), b"aaaaaaaaaa").unwrap();
        fs::write(hot_root.join("b"), b"bbbbbbbbbb").unwrap();
        let mut tier_state = TierState::new(
            hot_root.clone(),
            vec![cold_root.clone()],
            15,
            vec![u64::MAX],
        );
        tier_state.init_bytes().unwrap();
        let mut policy = ArcPolicy::new(tier_state);
        policy.reorganize().unwrap();

        let (evicted_hot, evicted_cold) = if fs::symlink_metadata(hot_root.join("a"))
            .map(|m| m.file_type().is_symlink())
            .unwrap_or(false)
        {
            (hot_root.join("a"), cold_root.join("a"))
        } else {
            (hot_root.join("b"), cold_root.join("b"))
        };
        assert!(evicted_cold.exists(), "one file should be in cold");
        let hot_after_first = policy.tier_state.hot_bytes();
        let cold_after_first = policy.tier_state.cold_bytes(0);

        let ts = SystemTime::now();
        policy.ingest(&[
            AccessEvent {
                path: evicted_cold.clone(),
                kind: FsEventKind::Create,
                timestamp: ts,
            },
            AccessEvent {
                path: evicted_cold.clone(),
                kind: FsEventKind::Modify,
                timestamp: ts,
            },
            AccessEvent {
                path: evicted_hot.clone(),
                kind: FsEventKind::Modify,
                timestamp: ts,
            },
        ]);
        policy.reorganize().unwrap();

        assert!(
            fs::symlink_metadata(&evicted_hot)
                .map(|m| m.file_type().is_symlink())
                .unwrap_or(false),
            "evicted path should still be symlink (no re-promote)"
        );
        assert_eq!(
            policy.tier_state.hot_bytes(),
            hot_after_first,
            "hot bytes should not change"
        );
        assert_eq!(
            policy.tier_state.cold_bytes(0),
            cold_after_first,
            "cold bytes should not change"
        );
        policy.assert_invariants();
    }

    /// Remove event cleans up state correctly.
    #[test]
    fn remove_event_cleans_state() {
        let (hot_dir, cold_dir) = setup_dirs();
        let hot_root = fs::canonicalize(hot_dir.path()).unwrap();
        let cold_root = fs::canonicalize(cold_dir.path()).unwrap();
        fs::write(hot_root.join("f"), b"content").unwrap();
        let mut tier_state =
            TierState::new(hot_root.clone(), vec![cold_root.clone()], 5, vec![u64::MAX]);
        tier_state.init_bytes().unwrap();
        let mut policy = ArcPolicy::new(tier_state);
        policy.reorganize().unwrap();
        let hot_path = hot_root.join("f");
        let cold_before = policy.tier_state.cold_bytes(0);
        fs::remove_file(&hot_path).unwrap();
        fs::remove_file(cold_root.join("f")).unwrap();
        policy.ingest(&[AccessEvent {
            path: hot_path,
            kind: FsEventKind::Remove,
            timestamp: SystemTime::now(),
        }]);
        policy.reorganize().unwrap();
        assert!(policy.tier_state.cold_bytes(0) < cold_before);
        policy.assert_invariants();
    }

    /// Cold-path event (edit via symlink) promotes file back to hot.
    #[test]
    fn touch_via_cold_path_promotes_to_hot() {
        let (hot_dir, cold_dir) = setup_dirs();
        let hot_root = fs::canonicalize(hot_dir.path()).unwrap();
        let cold_root = fs::canonicalize(cold_dir.path()).unwrap();
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
        let mut policy = ArcPolicy::new(tier_state);
        policy.reorganize().unwrap();

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

        assert!(
            !fs::symlink_metadata(&hot_path)
                .unwrap()
                .file_type()
                .is_symlink()
        );
        assert!(!cold_path.exists());
        policy.assert_invariants();
    }

    /// Stats track promotions and demotions correctly.
    #[test]
    fn stats_track_promotions_and_demotions() {
        let (hot_dir, cold_dir) = setup_dirs();
        let hot_root = fs::canonicalize(hot_dir.path()).unwrap();
        let cold_root = fs::canonicalize(cold_dir.path()).unwrap();
        fs::write(hot_root.join("a"), b"aaaaaaaaaa").unwrap();
        fs::write(hot_root.join("b"), b"bbbbbbbbbb").unwrap();
        let mut tier_state = TierState::new(
            hot_root.clone(),
            vec![cold_root.clone()],
            15,
            vec![u64::MAX],
        );
        tier_state.init_bytes().unwrap();
        let mut policy = ArcPolicy::new(tier_state);
        policy.reorganize().unwrap();

        let stats = policy.stats();
        assert_eq!(stats.demotions, 1, "one file should be evicted");
        assert_eq!(stats.demotions_to_tier, vec![1]);
        policy.assert_invariants();
    }

    /// Same file touched multiple times in one batch: no duplicates in lists.
    #[test]
    fn multi_touch_same_file_no_duplicates() {
        let (hot_dir, cold_dir) = setup_dirs();
        let hot_root = fs::canonicalize(hot_dir.path()).unwrap();
        let cold_root = fs::canonicalize(cold_dir.path()).unwrap();
        fs::write(hot_root.join("a"), b"aaaaaaaaaa").unwrap();
        let mut tier_state = TierState::new(
            hot_root.clone(),
            vec![cold_root.clone()],
            100,
            vec![u64::MAX],
        );
        tier_state.init_bytes().unwrap();
        let mut policy = ArcPolicy::new(tier_state);
        policy.reorganize().unwrap();
        policy.assert_invariants();

        // Touch the same file 5 times in one batch
        let ts = SystemTime::now();
        let events: Vec<_> = (0..5)
            .map(|_| AccessEvent {
                path: hot_root.join("a"),
                kind: FsEventKind::Modify,
                timestamp: ts,
            })
            .collect();
        policy.ingest(&events);
        policy.reorganize().unwrap();
        policy.assert_invariants();

        // Should be in T2 (promoted from T1 on second touch), no duplicates
        assert_eq!(policy.t1_len(), 0);
        assert_eq!(policy.t2_len(), 1);
    }

    /// Ghost list trimming keeps B1/B2 bounded.
    #[test]
    fn ghost_lists_stay_bounded() {
        let (hot_dir, cold_dir) = setup_dirs();
        let hot_root = fs::canonicalize(hot_dir.path()).unwrap();
        let cold_root = fs::canonicalize(cold_dir.path()).unwrap();
        // Capacity for 1 file (10 bytes). Create many files to force lots of evictions.
        let mut tier_state = TierState::new(
            hot_root.clone(),
            vec![cold_root.clone()],
            15,
            vec![u64::MAX],
        );
        tier_state.init_bytes().unwrap();
        let mut policy = ArcPolicy::new(tier_state);

        // Create and ingest 50 files one at a time, each causing eviction
        for i in 0..50 {
            let name = format!("f_{}", i);
            fs::write(hot_root.join(&name), b"aaaaaaaaaa").unwrap();
            policy.ingest(&[AccessEvent {
                path: hot_root.join(&name),
                kind: FsEventKind::Create,
                timestamp: SystemTime::now(),
            }]);
            policy.reorganize().unwrap();
            policy.assert_invariants();
        }

        // Ghost lists should be bounded by max(|T1|+|T2|, 16) = 16 (only 1 file in hot)
        assert!(
            policy.b1_len() <= 16,
            "B1 should be bounded: {}",
            policy.b1_len()
        );
        assert!(
            policy.b2_len() <= 16,
            "B2 should be bounded: {}",
            policy.b2_len()
        );
    }

    /// New file touched while already tracked (e.g. create event followed by modify in same batch)
    /// should not create duplicate T1 entries.
    #[test]
    fn create_then_modify_same_batch_no_duplicate() {
        let (hot_dir, cold_dir) = setup_dirs();
        let hot_root = fs::canonicalize(hot_dir.path()).unwrap();
        let cold_root = fs::canonicalize(cold_dir.path()).unwrap();
        fs::write(hot_root.join("a"), b"aaaaaaaaaa").unwrap();
        let mut tier_state = TierState::new(
            hot_root.clone(),
            vec![cold_root.clone()],
            100,
            vec![u64::MAX],
        );
        tier_state.init_bytes().unwrap();
        let mut policy = ArcPolicy::new(tier_state);

        // Simulate watcher: Create + Modify for the same file
        let ts = SystemTime::now();
        policy.ingest(&[
            AccessEvent {
                path: hot_root.join("a"),
                kind: FsEventKind::Create,
                timestamp: ts,
            },
            AccessEvent {
                path: hot_root.join("a"),
                kind: FsEventKind::Modify,
                timestamp: ts,
            },
        ]);
        policy.reorganize().unwrap();
        policy.assert_invariants();
    }
}
