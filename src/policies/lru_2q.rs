//! LRU-2Q: two-queue cache that separates probationary from protected files.
//!
//! ## Algorithm (Johnson & Shasha, 1994)
//!
//! Files flow through three queues:
//!
//!   - **A1in** (FIFO, hot)  — probationary buffer for files accessed for the first time (or
//!     re-accessed after having been in cold without an A1out ghost entry). Capped at
//!     `A1IN_FRACTION` of hot_capacity; LRU excess is evicted to cold and its key recorded
//!     in A1out.
//!
//!   - **Am** (LRU, hot)  — "main" protected queue for files that have proven repeated
//!     interest. Gets the remaining `(1 - A1IN_FRACTION)` of hot_capacity. Evictions to cold
//!     have no ghost entry (unlike ARC's B2).
//!
//!   - **A1out** (ghost, cold)  — remembers keys of files recently evicted from A1in. When
//!     such a file is accessed again, it is promoted directly into Am (the "second chance"),
//!     bypassing A1in and getting long-term protection.
//!
//! ## Key difference from basic_lru
//!
//! basic_lru treats every access as MRU: a one-time sequential scan over N > hot_capacity
//! unique files flushes the entire LRU queue, evicting all genuinely-hot files. In 2Q,
//! scan files enter only A1in (a small FIFO buffer), so Am remains clean and stable.
//!
//! ## Key difference from ARC
//!
//! ARC maintains ghost lists for *both* T1 (recency) and T2 (frequency) evictions and
//! adapts its T1/T2 split dynamically based on ghost hits. 2Q has a fixed `A1IN_FRACTION`
//! split and no ghost list for Am evictions. On workloads where the optimal recency/
//! frequency balance shifts over time, ARC self-tunes while 2Q does not. On workloads
//! with a clear, stable hot set + scan noise, both perform similarly.

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

/// Fraction of hot_capacity reserved for the probationary A1in queue.
/// The original 2Q paper suggests 25%; this gives Am the majority of the capacity
/// so the stable hot set has maximum room.
const A1IN_FRACTION: f64 = 0.25;

#[derive(Debug)]
pub struct Lru2QPolicy {
    pub tier_state: TierState,

    // ── hot-tier queues ───────────────────────────────────────────────────────
    // A1in: FIFO probationary buffer. Front = newest, Back = oldest (evict from back).
    a1_in: VecDeque<PathBuf>,
    // O(1) membership tests for the queues.
    in_a1in: HashSet<PathBuf>,
    // Running sum of hot_sizes for A1in entries, used to decide when A1in is over target.
    a1_in_bytes: u64,

    // Am: LRU protected queue. Front = MRU, Back = LRU (evict from back).
    am: VecDeque<PathBuf>,
    in_am: HashSet<PathBuf>,

    // ── ghost list ────────────────────────────────────────────────────────────
    // A1out: keys (no data) of files recently evicted from A1in. A file whose key
    // is found here on re-access goes straight to Am ("second chance").
    // Front = most recently evicted, Back = evicted longest ago (drop when full).
    a1_out: VecDeque<PathBuf>,
    in_a1out: HashSet<PathBuf>,

    // ── size / state tracking ─────────────────────────────────────────────────
    hot_sizes: HashMap<PathBuf, u64>,
    cold_sizes: HashMap<PathBuf, u64>,
    touched: Vec<(PathBuf, SystemTime)>,
    /// Paths we moved in the last reorganize (evicted or promoted).
    last_modified: HashSet<PathBuf>,

    total_promotions: u64,
    total_demotions: u64,
    demotions_to_tier: Vec<u64>,
}

impl Lru2QPolicy {
    pub fn new(tier_state: TierState) -> Self {
        Self {
            tier_state,
            a1_in: VecDeque::new(),
            in_a1in: HashSet::new(),
            a1_in_bytes: 0,
            am: VecDeque::new(),
            in_am: HashSet::new(),
            a1_out: VecDeque::new(),
            in_a1out: HashSet::new(),
            hot_sizes: HashMap::new(),
            cold_sizes: HashMap::new(),
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

    /// Byte target for A1in: 25% of total hot capacity.
    fn a1_in_target_bytes(&self) -> u64 {
        let total = self.tier_state.hot_bytes() + self.tier_state.hot_bytes_left();
        (total as f64 * A1IN_FRACTION) as u64
    }

    /// Maximum entries in A1out ghost list, scaled to ~2× the number of files that
    /// fit in hot (assuming average file ≈ 500 B). Floors at 32 entries.
    fn a1_out_max(&self) -> usize {
        let total = self.tier_state.hot_bytes() + self.tier_state.hot_bytes_left();
        ((total / 500) * 2).max(32) as usize
    }

    /// Remove a path from whichever hot queue it belongs to, correcting `a1_in_bytes`.
    fn remove_from_hot_queues(&mut self, path: &Path) {
        if self.in_a1in.remove(path) {
            self.a1_in.retain(|p| p != path);
            let sz = self.hot_sizes.get(path).copied().unwrap_or(0);
            self.a1_in_bytes = self.a1_in_bytes.saturating_sub(sz);
        } else if self.in_am.remove(path) {
            self.am.retain(|p| p != path);
        }
    }

    /// Evict one file from hot to cold. Prefers A1in when it is over the byte target,
    /// otherwise evicts from Am. If the preferred queue's LRU entry equals `exclude`,
    /// tries the other queue. Returns `Ok(true)` if a file was successfully evicted.
    fn evict_one(
        &mut self,
        hot_root: &Path,
        cold: &Path,
        exclude: Option<&Path>,
    ) -> Result<bool, Box<dyn std::error::Error + Send + Sync>> {
        let prefer_a1in = self.a1_in_bytes > self.a1_in_target_bytes();

        // Try up to two passes: preferred queue first, then the other.
        for pass in 0..2u8 {
            let try_a1in = if pass == 0 { prefer_a1in } else { !prefer_a1in };
            let queue = if try_a1in { &self.a1_in } else { &self.am };

            // Find the LRU entry that isn't `exclude`.
            let victim = queue
                .iter()
                .rev()
                .find(|p| {
                    exclude
                        .map(|ex| canonical(p) != canonical(ex))
                        .unwrap_or(true)
                })
                .cloned();

            let Some(path) = victim else {
                continue;
            };

            // Physically move to cold.
            if path.exists() {
                let rel = path
                    .strip_prefix(hot_root)
                    .unwrap_or_else(|_| Path::new(""));
                let cold_path = cold.join(rel);
                let sz = self.tier_state.move_to_tier(&path, cold)?;
                if sz > 0 {
                    self.tier_state.adjust_hot_bytes(sz, 0);
                    self.tier_state.adjust_cold_bytes(0, 0, sz);
                }
                self.hot_sizes.remove(&path);
                self.cold_sizes.insert(path.clone(), sz);
                self.last_modified.insert(canonical(&path));
                self.last_modified.insert(canonical(&cold_path));
                self.total_demotions += 1;
                self.demotions_to_tier[0] += 1;

                // Update queue membership.
                if try_a1in {
                    self.a1_in.retain(|p| p != &path);
                    self.in_a1in.remove(&path);
                    self.a1_in_bytes = self.a1_in_bytes.saturating_sub(sz);
                    // Add key to A1out ghost list (the "second chance" record).
                    self.a1_out.push_front(path.clone());
                    self.in_a1out.insert(path.clone());
                    // Trim ghost list if over capacity.
                    while self.a1_out.len() > self.a1_out_max() {
                        if let Some(old) = self.a1_out.pop_back() {
                            self.in_a1out.remove(&old);
                        }
                    }
                } else {
                    self.am.retain(|p| p != &path);
                    self.in_am.remove(&path);
                    // Am evictions are NOT recorded in A1out (unlike ARC's B2).
                    // They re-enter A1in if accessed again.
                }
            } else if let Some(old) = self.hot_sizes.remove(&path) {
                self.tier_state.adjust_hot_bytes(old, 0);
                if try_a1in {
                    self.a1_in.retain(|p| p != &path);
                    self.in_a1in.remove(&path);
                    self.a1_in_bytes = self.a1_in_bytes.saturating_sub(old);
                } else {
                    self.am.retain(|p| p != &path);
                    self.in_am.remove(&path);
                }
            } else if self.cold_sizes.remove(&path).is_some() {
                // Backing still in cold; no byte adjustment needed.
            }
            return Ok(true);
        }
        Ok(false)
    }
}

impl PolicyEngine for Lru2QPolicy {
    fn validate_config(
        _hot: &Path,
        cold_storage: &[std::path::PathBuf],
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        if cold_storage.len() != 1 {
            return Err("lru_2q requires exactly one cold_storage tier".into());
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
                        // Keep a1_in_bytes in sync when a hot file changes size.
                        if self.in_a1in.contains(&path) {
                            self.a1_in_bytes =
                                self.a1_in_bytes.saturating_sub(old).saturating_add(new);
                        }
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
                        self.remove_from_hot_queues(&path);
                    }
                }
                _ => {}
            }
        }
        // Build touched list — identical filtering logic as basic_lru.
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
                    return false;
                }
                true
            })
            .map(|e| (canonical(&e.path), e.timestamp))
            .collect();
        policy_log::log_ingest("lru_2q", events.len());
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

        // ── Reconcile cold_sizes ────────────────────────────────────────────────
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
            // Clean up ghost list if a cold file was deleted externally.
            if self.in_a1out.remove(&p) {
                self.a1_out.retain(|x| x != &p);
            }
            self.remove_from_hot_queues(&p);
        }

        // ── Reconcile hot_sizes ─────────────────────────────────────────────────
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
            self.remove_from_hot_queues(&p);
        }

        // ── First run: seed from disk ───────────────────────────────────────────
        // Seed existing hot files into A1in (probationary), consistent with ARC's
        // T1-seeding. History is unknown on startup, so we let the normal 2Q flow
        // promote files that prove themselves: A1in eviction → A1out ghost → re-access → Am.
        if self.am.is_empty() && self.a1_in.is_empty() {
            for p in Self::list_hot_files(&hot_root, &hot_root)? {
                if let Ok(meta) = fs::symlink_metadata(&p)
                    && !meta.file_type().is_symlink()
                    && let Ok(sz) = fs::metadata(&p).map(|m| m.len())
                {
                    self.hot_sizes.insert(p.clone(), sz);
                    self.a1_in.push_back(p.clone());
                    self.in_a1in.insert(p.clone());
                    self.a1_in_bytes = self.a1_in_bytes.saturating_add(sz);
                }
            }
            let found: u64 = self.hot_sizes.values().sum();
            let current = self.tier_state.hot_bytes();
            if found > current {
                self.tier_state.adjust_hot_bytes(0, found - current);
            } else if current > found {
                self.tier_state.adjust_hot_bytes(current - found, 0);
            }
            policy_log::log_initial_fill(
                "lru_2q",
                self.am.len() + self.a1_in.len(),
                self.tier_state.hot_bytes(),
            );
        }

        // ── Process touch events ────────────────────────────────────────────────
        self.touched.sort_by(|a, b| a.1.cmp(&b.1));
        let mut touches = std::mem::take(&mut self.touched);
        let had_touches = !touches.is_empty();
        let mut new_in_hot = 0u32;
        let mut promoted = 0u32;
        let mut evicted_room = 0u32;

        for (mut path, _) in touches.drain(..) {
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
                    } else {
                        self.cold_sizes.remove(&path);
                    }
                    self.remove_from_hot_queues(&path);
                    continue;
                }
            };
            if meta.is_dir() {
                continue;
            }

            if meta.file_type().is_symlink() {
                // ── Cold file accessed: promote ─────────────────────────────────
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
                promoted += 1;
                self.total_promotions += 1;

                // ── 2Q placement decision ───────────────────────────────────────
                // If this path was in A1out (evicted from A1in before), promote to Am
                // (the "second chance": it has shown interest beyond a one-time scan).
                // Otherwise add to front of A1in (probationary first-time access).
                if self.in_a1out.remove(&path) {
                    self.a1_out.retain(|p| p != &path);
                    self.am.push_front(path.clone());
                    self.in_am.insert(path);
                } else {
                    let sz_bytes = sz;
                    self.a1_in.push_front(path.clone());
                    self.in_a1in.insert(path.clone());
                    self.a1_in_bytes = self.a1_in_bytes.saturating_add(sz_bytes);
                }
            } else {
                // ── Hot file accessed ───────────────────────────────────────────
                if !self.hot_sizes.contains_key(&path) {
                    // Brand-new file, not yet tracked: add to A1in (first access).
                    if let Ok(sz) = fs::metadata(&path).map(|m| m.len()) {
                        self.tier_state.adjust_hot_bytes(0, sz);
                        self.hot_sizes.insert(path.clone(), sz);
                        self.a1_in.push_front(path.clone());
                        self.in_a1in.insert(path.clone());
                        self.a1_in_bytes = self.a1_in_bytes.saturating_add(sz);
                        new_in_hot += 1;
                    }
                } else if self.in_am.contains(&path) {
                    // File in Am: MRU bump (move to front of protected queue).
                    self.am.retain(|p| p != &path);
                    self.am.push_front(path.clone());
                }
                // File in A1in: no action — A1in is FIFO, not LRU; position stays.
            }
        }

        // ── Over-capacity eviction ──────────────────────────────────────────────
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
                policy_name: "lru_2q",
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

    /// Over capacity: reorganize must evict one file to cold.
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
        let mut policy = Lru2QPolicy::new(tier_state);
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

    /// Stats: demotions are counted.
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
        let mut policy = Lru2QPolicy::new(tier_state);
        policy.reorganize().unwrap();
        let stats = policy.stats();
        assert_eq!(stats.demotions, 1);
        assert_eq!(stats.demotions_to_tier, vec![1]);
    }

    /// Second-chance: a file evicted from A1in (into A1out ghost list) then re-accessed
    /// should be promoted directly into Am, NOT back into A1in.
    #[test]
    fn second_chance_evicted_a1in_file_goes_to_am() {
        use crate::policy_engine::FsEventKind;
        use std::time::SystemTime;

        let (hot_dir, cold_dir) = setup_dirs();
        let hot_root = fs::canonicalize(hot_dir.path()).unwrap();
        let cold_root = fs::canonicalize(cold_dir.path()).unwrap();

        // 4 files × 10 bytes; capacity = 15 → only 1 fits in hot after first reorganize.
        for name in ["a", "b", "c"] {
            fs::write(hot_root.join(name), b"tenbytes!!").unwrap();
        }
        let mut tier_state = TierState::new(
            hot_root.clone(),
            vec![cold_root.clone()],
            15,
            vec![u64::MAX],
        );
        tier_state.init_bytes().unwrap();
        let mut policy = Lru2QPolicy::new(tier_state);
        policy.reorganize().unwrap();

        // Find one file that ended up in cold (a symlink in hot).
        let cold_victim = ["a", "b", "c"]
            .iter()
            .map(|n| hot_root.join(n))
            .find(|p| {
                fs::symlink_metadata(p)
                    .map(|m| m.file_type().is_symlink())
                    .unwrap_or(false)
            })
            .expect("at least one file should be evicted");

        // Re-access the cold victim: ingest a Modify event on its hot path.
        policy.ingest(&[AccessEvent {
            path: cold_victim.clone(),
            kind: FsEventKind::Modify,
            timestamp: SystemTime::now(),
        }]);
        policy.reorganize().unwrap();

        // After promotion, the victim must be a regular file in hot.
        let meta = fs::symlink_metadata(&cold_victim).unwrap();
        assert!(
            !meta.file_type().is_symlink(),
            "re-accessed file should be back in hot"
        );

        // And it should be in Am (in_am), not A1in.
        assert!(
            policy.in_am.contains(&cold_victim),
            "second-chance promoted file should be in Am, not A1in"
        );
        assert!(
            !policy.in_a1in.contains(&cold_victim),
            "second-chance file must not be in A1in"
        );
    }
}
