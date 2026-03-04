//! CACHEUS (CAche tHat Efficiently Utilizes Statistical knowledge) policy.
//!
//! Improves LeCaR with three enhancements:
//! - **SR-LRU** (Scan-Resistant LRU): probationary FIFO + protected LRU. New files
//!   enter probation; second access promotes to protected. Evicts from probation
//!   tail first (one-hit scan files), then protected tail (true LRU).
//! - **CR-LFU** (Cache-Resident LFU): exponentially decaying frequency counts
//!   (`freq = freq * decay + 1`). Prevents stale high-frequency files from
//!   blocking new popular files.
//! - **Adaptive learning rate**: `lr = max(0.001, lr_init * |2w - 1|)`. Low when
//!   uncertain (w near 0.5), high when committed (fast correction on mistakes).
//!
//! Reference: Gil et al., "CACHEUS: Utility-Aware Caching for Information
//! Management Workloads", FAST'21.

use std::collections::{HashMap, HashSet, VecDeque};
use std::fs;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use rand::rngs::StdRng;
use rand::{Rng, SeedableRng};

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

/// Which expert chose the eviction.
#[derive(Debug, Clone, Copy)]
enum Expert {
    SrLru,
    CrLfu,
}

#[derive(Debug)]
pub struct CacheusPolicy {
    pub tier_state: TierState,
    hot_sizes: HashMap<PathBuf, u64>,
    cold_sizes: HashMap<PathBuf, u64>,
    touched: Vec<(PathBuf, SystemTime)>,
    last_modified: HashSet<PathBuf>,

    // ── SR-LRU expert ──
    /// Probationary FIFO: new files enter here. Back = oldest (evict first).
    probation: VecDeque<PathBuf>,
    in_probation: HashSet<PathBuf>,
    /// Protected LRU: files promoted from probation on second access.
    /// Front = MRU, back = LRU (evict after probation is empty).
    protected: VecDeque<PathBuf>,
    in_protected: HashSet<PathBuf>,

    // ── CR-LFU expert ──
    /// Exponentially decaying frequency counts.
    cr_freqs: HashMap<PathBuf, f64>,
    /// Decay factor: e^(-1/C) where C = hot file count.
    decay_factor: f64,

    // ── Ghost lists (same framework as LeCaR) ──
    lru_ghost: VecDeque<PathBuf>,
    lfu_ghost: VecDeque<PathBuf>,
    ghost_evict_time: HashMap<PathBuf, u64>,
    ghost_cap: usize,

    // ── Weights ──
    w_sr_lru: f64,
    lr_init: f64,
    discount_rate: f64,
    logical_time: u64,
    rng: StdRng,

    // ── Stats ──
    total_promotions: u64,
    total_demotions: u64,
    demotions_to_tier: Vec<u64>,
}

impl CacheusPolicy {
    pub fn new(tier_state: TierState) -> Self {
        Self {
            tier_state,
            hot_sizes: HashMap::new(),
            cold_sizes: HashMap::new(),
            touched: Vec::new(),
            last_modified: HashSet::new(),
            probation: VecDeque::new(),
            in_probation: HashSet::new(),
            protected: VecDeque::new(),
            in_protected: HashSet::new(),
            cr_freqs: HashMap::new(),
            decay_factor: 0.5,
            lru_ghost: VecDeque::new(),
            lfu_ghost: VecDeque::new(),
            ghost_evict_time: HashMap::new(),
            ghost_cap: 16,
            w_sr_lru: 0.5,
            lr_init: 0.45,
            discount_rate: 0.5,
            logical_time: 0,
            rng: StdRng::seed_from_u64(0xCAFE),
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

    // ── SR-LRU + CR-LFU touch ──

    /// Update both expert structures on a hot-file access.
    fn touch(&mut self, path: &Path) {
        let p = canonical(path);
        // SR-LRU: promote from probation to protected, or bump MRU in protected.
        if self.in_probation.remove(&p) {
            if let Some(pos) = self.probation.iter().position(|x| *x == p) {
                self.probation.remove(pos);
            }
            self.protected.push_front(p.clone());
            self.in_protected.insert(p.clone());
        } else if self.in_protected.contains(&p) {
            if let Some(pos) = self.protected.iter().position(|x| *x == p) {
                self.protected.remove(pos);
            }
            self.protected.push_front(p.clone());
        }
        // CR-LFU: decay and increment.
        let freq = self.cr_freqs.entry(p).or_insert(0.0);
        *freq = *freq * self.decay_factor + 1.0;
        self.logical_time += 1;
    }

    /// Insert a new file: probation front (SR-LRU), freq=1.0 (CR-LFU).
    fn insert_new(&mut self, path: &PathBuf) {
        self.probation.push_front(path.clone());
        self.in_probation.insert(path.clone());
        self.cr_freqs.insert(path.clone(), 1.0);
        self.logical_time += 1;
    }

    /// Insert a promoted file (from ghost hit): protected front (already proven).
    fn insert_promoted(&mut self, path: &PathBuf) {
        self.protected.push_front(path.clone());
        self.in_protected.insert(path.clone());
        self.cr_freqs.insert(path.clone(), 1.0);
        self.logical_time += 1;
    }

    fn remove_from_tracking(&mut self, path: &PathBuf) {
        self.hot_sizes.remove(path);
        if self.in_probation.remove(path) {
            if let Some(pos) = self.probation.iter().position(|p| p == path) {
                self.probation.remove(pos);
            }
        }
        if self.in_protected.remove(path) {
            if let Some(pos) = self.protected.iter().position(|p| p == path) {
                self.protected.remove(pos);
            }
        }
        self.cr_freqs.remove(path);
    }

    fn recalc_params(&mut self) {
        let c = self.hot_sizes.len().max(1);
        self.ghost_cap = c;
        self.discount_rate = 0.005_f64.powf(1.0 / c as f64);
        self.decay_factor = (-1.0 / c as f64).exp();
    }

    // ── Victim selection ──

    /// SR-LRU victim: probation tail first (scan-resistant), then protected tail.
    fn sr_lru_victim(&self, exclude: Option<&Path>) -> Option<PathBuf> {
        for p in self.probation.iter().rev() {
            if self.hot_sizes.contains_key(p)
                && exclude
                    .map(|ex| canonical(p) != canonical(ex))
                    .unwrap_or(true)
            {
                return Some(p.clone());
            }
        }
        for p in self.protected.iter().rev() {
            if self.hot_sizes.contains_key(p)
                && exclude
                    .map(|ex| canonical(p) != canonical(ex))
                    .unwrap_or(true)
            {
                return Some(p.clone());
            }
        }
        None
    }

    /// CR-LFU victim: lowest decaying frequency.
    fn cr_lfu_victim(&self, exclude: Option<&Path>) -> Option<PathBuf> {
        let mut min_freq = f64::MAX;
        let mut victim = None;
        for (p, &freq) in &self.cr_freqs {
            if !self.hot_sizes.contains_key(p) {
                continue;
            }
            if exclude.is_some_and(|ex| canonical(p) == canonical(ex)) {
                continue;
            }
            if freq < min_freq {
                min_freq = freq;
                victim = Some(p.clone());
            }
        }
        victim
    }

    // ── Weight adjustment (adaptive LR) ──

    fn penalize(&mut self, expert: Expert, time_elapsed: u64) {
        let lr = (self.lr_init * (2.0 * self.w_sr_lru - 1.0).abs()).max(0.001);
        let reward = -(self.discount_rate.powi(time_elapsed.min(1000) as i32));
        let mut w_lru = self.w_sr_lru;
        let mut w_lfu = 1.0 - self.w_sr_lru;
        match expert {
            Expert::SrLru => w_lru *= (lr * reward).exp(),
            Expert::CrLfu => w_lfu *= (lr * reward).exp(),
        }
        let sum = w_lru + w_lfu;
        self.w_sr_lru = (w_lru / sum).clamp(0.01, 0.99);
    }

    // ── Ghost list management ──

    fn add_to_ghost(&mut self, path: &PathBuf, expert: Expert) {
        match expert {
            Expert::SrLru => {
                self.lru_ghost.push_front(path.clone());
                while self.lru_ghost.len() > self.ghost_cap {
                    if let Some(old) = self.lru_ghost.pop_back() {
                        self.ghost_evict_time.remove(&old);
                    }
                }
            }
            Expert::CrLfu => {
                self.lfu_ghost.push_front(path.clone());
                while self.lfu_ghost.len() > self.ghost_cap {
                    if let Some(old) = self.lfu_ghost.pop_back() {
                        self.ghost_evict_time.remove(&old);
                    }
                }
            }
        }
        self.ghost_evict_time
            .insert(path.clone(), self.logical_time);
    }

    fn remove_from_ghost(&mut self, path: &PathBuf) -> Option<Expert> {
        if let Some(pos) = self.lru_ghost.iter().position(|p| p == path) {
            self.lru_ghost.remove(pos);
            return Some(Expert::SrLru);
        }
        if let Some(pos) = self.lfu_ghost.iter().position(|p| p == path) {
            self.lfu_ghost.remove(pos);
            return Some(Expert::CrLfu);
        }
        None
    }

    // ── Eviction ──

    fn evict_one(
        &mut self,
        hot_root: &Path,
        cold: &Path,
        exclude: Option<&Path>,
    ) -> Result<bool, Box<dyn std::error::Error + Send + Sync>> {
        let lru_victim = self.sr_lru_victim(exclude);
        let lfu_victim = self.cr_lfu_victim(exclude);

        let (victim, expert) = match (&lru_victim, &lfu_victim) {
            (None, None) => return Ok(false),
            (Some(lru), None) => (lru.clone(), Expert::SrLru),
            (None, Some(lfu)) => (lfu.clone(), Expert::CrLfu),
            (Some(lru), Some(lfu)) => {
                if canonical(lru) == canonical(lfu) {
                    let path = lru.clone();
                    let r: f64 = self.rng.r#gen();
                    let expert = if r < self.w_sr_lru {
                        Expert::SrLru
                    } else {
                        Expert::CrLfu
                    };
                    self.add_to_ghost(&path, expert);
                    self.do_evict(&path, hot_root, cold)?;
                    return Ok(true);
                }
                let r: f64 = self.rng.r#gen();
                if r < self.w_sr_lru {
                    (lru.clone(), Expert::SrLru)
                } else {
                    (lfu.clone(), Expert::CrLfu)
                }
            }
        };

        self.add_to_ghost(&victim, expert);
        self.do_evict(&victim, hot_root, cold)?;
        Ok(true)
    }

    fn do_evict(
        &mut self,
        victim: &PathBuf,
        hot_root: &Path,
        cold: &Path,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        if victim.exists() {
            let rel = victim
                .strip_prefix(hot_root)
                .unwrap_or_else(|_| Path::new(""));
            let cold_path = cold.join(rel);
            self.tier_state.move_to_tier(victim, cold)?;
            let tracked = self.hot_sizes.remove(victim).unwrap_or(0);
            if tracked > 0 {
                self.tier_state.adjust_hot_bytes(tracked, 0);
                self.tier_state.adjust_cold_bytes(0, 0, tracked);
            }
            self.cold_sizes.insert(victim.clone(), tracked);
            self.last_modified.insert(canonical(victim));
            self.last_modified.insert(canonical(&cold_path));
            self.total_demotions += 1;
            self.demotions_to_tier[0] += 1;
        } else if let Some(old) = self.hot_sizes.remove(victim) {
            self.tier_state.adjust_hot_bytes(old, 0);
        }
        self.remove_from_tracking(victim);
        Ok(())
    }

    fn promote(
        &mut self,
        path: &PathBuf,
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
        self.hot_sizes.insert(path.clone(), sz);
        self.cold_sizes.remove(path);
        self.last_modified.insert(canonical(path));
        if let Some(ref cb) = cold_backing {
            self.last_modified.insert(canonical(cb));
        }
        self.total_promotions += 1;
        Ok(sz)
    }

    fn cold_file_size(path: &Path) -> u64 {
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
}

impl PolicyEngine for CacheusPolicy {
    fn validate_config(
        _hot: &Path,
        cold_storage: &[PathBuf],
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        if cold_storage.len() != 1 {
            return Err("cacheus requires exactly one cold_storage tier".into());
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
                        self.remove_from_tracking(&path);
                    }
                }
                _ => {}
            }
        }
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
        policy_log::log_ingest("cacheus", events.len());
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
            }
            self.cr_freqs.remove(&p);
        }

        // ── Reconcile hot_sizes ──
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
            self.tier_state.adjust_hot_bytes(sz, 0);
            self.remove_from_tracking(&p);
        }

        // ── Initial fill ──
        if self.hot_sizes.is_empty() && self.cr_freqs.is_empty() {
            for p in Self::list_hot_files(&hot_root, &hot_root)? {
                if let Ok(meta) = fs::symlink_metadata(&p)
                    && !meta.file_type().is_symlink()
                    && let Ok(sz) = fs::metadata(&p).map(|m| m.len())
                {
                    self.hot_sizes.insert(p.clone(), sz);
                    self.insert_new(&p);
                }
            }
            self.recalc_params();
            let found: u64 = self.hot_sizes.values().sum();
            let current = self.tier_state.hot_bytes();
            if found > current {
                self.tier_state.adjust_hot_bytes(0, found - current);
            } else if current > found {
                self.tier_state.adjust_hot_bytes(current - found, 0);
            }
            policy_log::log_initial_fill(
                "cacheus",
                self.hot_sizes.len(),
                self.tier_state.hot_bytes(),
            );
        }

        // ── Process touches ──
        self.touched.sort_by(|a, b| a.1.cmp(&b.1));
        let touches = std::mem::take(&mut self.touched);
        let had_touches = !touches.is_empty();
        let mut new_in_hot = 0u32;
        let mut promoted = 0u32;
        let mut evicted_room = 0u32;

        for (mut path, _) in touches {
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
                    }
                    self.remove_from_tracking(&path);
                    continue;
                }
            };

            if meta.is_dir() {
                continue;
            }

            if meta.file_type().is_symlink() {
                // Cold file touched — check ghost lists for weight update.
                let was_ghost = if let Some(expert) = self.remove_from_ghost(&path) {
                    let evict_time = self.ghost_evict_time.remove(&path).unwrap_or(0);
                    let elapsed = self.logical_time.saturating_sub(evict_time);
                    self.penalize(expert, elapsed);
                    true
                } else {
                    false
                };

                let need = Self::cold_file_size(&path);
                while self.tier_state.hot_bytes_left() < need {
                    let evicted = self.evict_one(&hot_root, &cold, Some(&path))?;
                    if !evicted {
                        break;
                    }
                    evicted_room += 1;
                }
                if self.tier_state.hot_bytes_left() >= need {
                    self.promote(&path, &hot_root)?;
                    if was_ghost {
                        self.insert_promoted(&path);
                    } else {
                        self.insert_new(&path);
                    }
                    promoted += 1;
                    self.recalc_params();
                }
            } else {
                // Hot file touched (or new regular file).
                if !self.hot_sizes.contains_key(&path) {
                    if let Ok(sz) = fs::metadata(&path).map(|m| m.len()) {
                        self.tier_state.adjust_hot_bytes(0, sz);
                        self.hot_sizes.insert(path.clone(), sz);
                        new_in_hot += 1;
                    }
                    self.insert_new(&path);
                    self.recalc_params();
                } else {
                    self.touch(&path);
                }
            }
        }

        // ── Enforce capacity ──
        let mut evicted = 0u32;
        while self.tier_state.hot_bytes_left() == 0 {
            let did = self.evict_one(&hot_root, &cold, None)?;
            if !did {
                break;
            }
            evicted += 1;
        }

        // Trim ghost lists.
        while self.lru_ghost.len() > self.ghost_cap {
            if let Some(old) = self.lru_ghost.pop_back() {
                self.ghost_evict_time.remove(&old);
            }
        }
        while self.lfu_ghost.len() > self.ghost_cap {
            if let Some(old) = self.lfu_ghost.pop_back() {
                self.ghost_evict_time.remove(&old);
            }
        }

        if had_touches || evicted > 0 {
            policy_log::log_reorganize_done(policy_log::ReorganizeDoneParams {
                policy_name: "cacheus",
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
        let mut policy = CacheusPolicy::new(tier_state);
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
    }

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
        let mut policy = CacheusPolicy::new(tier_state);
        policy.reorganize().unwrap();
        let stats = policy.stats();
        assert_eq!(stats.demotions, 1);
        assert_eq!(stats.demotions_to_tier, vec![1]);
    }

    #[test]
    fn scan_files_evicted_before_protected() {
        let (hot_dir, cold_dir) = setup_dirs();
        let hot_root = fs::canonicalize(hot_dir.path()).unwrap();
        let cold_root = fs::canonicalize(cold_dir.path()).unwrap();
        // f1 is a "core" file touched twice (should be in protected).
        // f2 is a "scan" file touched once (should be in probation).
        fs::write(hot_root.join("f1"), b"aaaaa").unwrap();
        fs::write(hot_root.join("f2"), b"bbbbb").unwrap();
        let mut tier_state = TierState::new(
            hot_root.clone(),
            vec![cold_root.clone()],
            15, // fits both initially
            vec![u64::MAX],
        );
        tier_state.init_bytes().unwrap();
        let mut policy = CacheusPolicy::new(tier_state);
        policy.reorganize().unwrap(); // initial fill → both in probation

        // Touch f1 to promote it to protected.
        let now = SystemTime::now();
        policy.ingest(&[AccessEvent {
            path: hot_root.join("f1"),
            kind: FsEventKind::Modify,
            timestamp: now,
        }]);
        policy.reorganize().unwrap();
        assert!(policy.in_protected.contains(&hot_root.join("f1")));
        assert!(policy.in_probation.contains(&hot_root.join("f2")));

        // Add f3 to push over capacity.
        fs::write(hot_root.join("f3"), b"ccccc").unwrap();
        policy.ingest(&[AccessEvent {
            path: hot_root.join("f3"),
            kind: FsEventKind::Create,
            timestamp: now,
        }]);
        policy.reorganize().unwrap();

        // f2 (probation) should be evicted, not f1 (protected).
        assert!(
            fs::symlink_metadata(hot_root.join("f2"))
                .map(|m| m.file_type().is_symlink())
                .unwrap_or(false),
            "f2 (scan/probation) should be evicted first"
        );
        assert!(
            !fs::symlink_metadata(hot_root.join("f1"))
                .map(|m| m.file_type().is_symlink())
                .unwrap_or(true),
            "f1 (protected) should remain in hot"
        );
    }
}
