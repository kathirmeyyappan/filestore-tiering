//! Decision Tree per-file eviction scorer policy: one hot (capacity-limited), one cold.
//!
//! Uses a decision tree regressor to learn which per-file features predict eviction
//! regret. Features: recency, frequency, file size, and average inter-access time.
//! A ghost list tracks evicted files; ghost hits/expires generate training samples
//! that periodically retrain the tree.
//!
//! Reference: Inspired by LRB (Song et al., NSDI '20).

use std::collections::{HashMap, HashSet, VecDeque};
use std::fs;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use smartcore::linalg::basic::matrix::DenseMatrix;
use smartcore::tree::decision_tree_regressor::{
    DecisionTreeRegressor, DecisionTreeRegressorParameters,
};

use crate::policy_engine::{AccessEvent, FsEventKind, PolicyEngine, PolicyStats, TierState};
use crate::policy_log;

// ── Constants ──

const NUM_FEATURES: usize = 4;
const NORM_DECAY_RATE: f64 = 0.05; // fraction to contract bounds toward center per reorganize

// ── Ghost entry ──

struct GhostEntry {
    path: PathBuf,
    features: [f64; NUM_FEATURES], // raw (un-normalized) features at eviction time
}

// ── Canonical path helper ──

fn canonical(path: &Path) -> PathBuf {
    fs::canonicalize(path).unwrap_or_else(|_| {
        path.parent()
            .and_then(|p| fs::canonicalize(p).ok())
            .and_then(|p| path.file_name().map(|n| p.join(n)))
            .unwrap_or_else(|| path.to_path_buf())
    })
}

// ── Policy struct ──

pub struct DecisionTreePolicy {
    // Standard fields
    pub tier_state: TierState,
    hot_sizes: HashMap<PathBuf, u64>,
    cold_sizes: HashMap<PathBuf, u64>,
    touched: Vec<(PathBuf, SystemTime)>,
    last_modified: HashSet<PathBuf>,

    // Per-file tracking
    last_access: HashMap<PathBuf, u64>,
    last_access_time: HashMap<PathBuf, SystemTime>,
    access_count: HashMap<PathBuf, u64>,
    inter_access_ms_sum: HashMap<PathBuf, u64>,

    // Feature normalization (running min/max)
    feat_min: [f64; NUM_FEATURES],
    feat_max: [f64; NUM_FEATURES],

    // Decision tree (None before first training)
    tree: Option<DecisionTreeRegressor<f64, f64, DenseMatrix<f64>, Vec<f64>>>,

    // Ghost list + training
    ghost: VecDeque<GhostEntry>,
    ghost_set: HashSet<PathBuf>,
    ghost_cap: usize,
    ghost_cap_min: usize,
    training_samples: Vec<([f64; NUM_FEATURES], f64)>, // (features, label)
    eviction_count: u64,
    retrain_interval: u64,
    min_training_samples: usize,
    tree_max_depth: u16,
    tree_min_samples_leaf: usize,

    logical_time: u64,
    total_promotions: u64,
    total_demotions: u64,
    demotions_to_tier: Vec<u64>,
}

impl std::fmt::Debug for DecisionTreePolicy {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DecisionTreePolicy")
            .field("hot_files", &self.hot_sizes.len())
            .field("logical_time", &self.logical_time)
            .field("eviction_count", &self.eviction_count)
            .field("tree_trained", &self.tree.is_some())
            .field("training_samples", &self.training_samples.len())
            .finish()
    }
}

impl DecisionTreePolicy {
    pub fn new(tier_state: TierState) -> Self {
        Self::new_with_params(tier_state, &HashMap::new())
    }

    pub fn new_with_params(tier_state: TierState, params: &HashMap<String, f64>) -> Self {
        let retrain_interval = params.get("retrain_interval").copied().unwrap_or(50.0) as u64;
        let min_training_samples =
            params.get("min_training_samples").copied().unwrap_or(20.0) as usize;
        let tree_max_depth = params.get("tree_max_depth").copied().unwrap_or(4.0) as u16;
        let tree_min_samples_leaf =
            params.get("tree_min_samples_leaf").copied().unwrap_or(2.0) as usize;
        let ghost_cap_min = params.get("ghost_cap_min").copied().unwrap_or(64.0) as usize;
        Self {
            tier_state,
            hot_sizes: HashMap::new(),
            cold_sizes: HashMap::new(),
            touched: Vec::new(),
            last_modified: HashSet::new(),

            last_access: HashMap::new(),
            last_access_time: HashMap::new(),
            access_count: HashMap::new(),
            inter_access_ms_sum: HashMap::new(),

            feat_min: [f64::MAX; NUM_FEATURES],
            feat_max: [f64::MIN; NUM_FEATURES],

            tree: None,

            ghost: VecDeque::new(),
            ghost_set: HashSet::new(),
            ghost_cap: ghost_cap_min,
            ghost_cap_min,
            training_samples: Vec::new(),
            eviction_count: 0,
            retrain_interval,
            min_training_samples,
            tree_max_depth,
            tree_min_samples_leaf,

            logical_time: 0,
            total_promotions: 0,
            total_demotions: 0,
            demotions_to_tier: vec![0; 1],
        }
    }

    // ── Feature extraction ──

    fn extract_raw_features(&self, path: &Path) -> [f64; NUM_FEATURES] {
        let recency = self
            .logical_time
            .saturating_sub(self.last_access.get(path).copied().unwrap_or(0))
            as f64;
        let frequency = self.access_count.get(path).copied().unwrap_or(1) as f64;
        let size = self.hot_sizes.get(path).copied().unwrap_or(0) as f64;
        let freq_u64 = self.access_count.get(path).copied().unwrap_or(1);
        let avg_inter_ms = if freq_u64 > 1 {
            self.inter_access_ms_sum.get(path).copied().unwrap_or(0) as f64 / (freq_u64 - 1) as f64
        } else {
            // No inter-access data yet; use a large sentinel.
            // Normalization will handle it, and decay prevents permanent distortion.
            u64::MAX as f64
        };
        [recency, frequency, size, avg_inter_ms]
    }

    fn update_min_max(&mut self, raw: &[f64; NUM_FEATURES]) {
        for (i, &val) in raw.iter().enumerate() {
            if val < self.feat_min[i] {
                self.feat_min[i] = val;
            }
            if val > self.feat_max[i] {
                self.feat_max[i] = val;
            }
        }
    }

    fn normalize(&self, raw: &[f64; NUM_FEATURES]) -> [f64; NUM_FEATURES] {
        let mut out = [0.0; NUM_FEATURES];
        for i in 0..NUM_FEATURES {
            let range = self.feat_max[i] - self.feat_min[i];
            out[i] = if range > 0.0 {
                ((raw[i] - self.feat_min[i]) / range).clamp(0.0, 1.0)
            } else {
                0.5
            };
        }
        out
    }

    // ── Scoring ──

    fn score_file(&self, path: &Path) -> f64 {
        let raw = self.extract_raw_features(path);
        let norm = self.normalize(&raw);

        if let Some(ref tree) = self.tree {
            let x = DenseMatrix::from_2d_vec(&vec![norm.to_vec()]);
            match tree.predict(&x) {
                Ok(pred) => pred[0],
                Err(_) => Self::fallback_score(&norm),
            }
        } else {
            Self::fallback_score(&norm)
        }
    }

    /// Fallback heuristic when tree is not yet trained.
    /// Higher score = more valuable (less likely to evict).
    fn fallback_score(norm: &[f64; NUM_FEATURES]) -> f64 {
        // norm[0] = recency (lower = more recent = more valuable)
        // norm[1] = frequency (higher = more frequent = more valuable)
        0.5 * norm[1] + 0.5 * (1.0 - norm[0])
    }

    fn find_victim(&mut self, exclude: Option<&Path>) -> Option<PathBuf> {
        // First pass: update normalization bounds for all hot files.
        let paths: Vec<PathBuf> = self.hot_sizes.keys().cloned().collect();
        for path in &paths {
            let raw = self.extract_raw_features(path);
            self.update_min_max(&raw);
        }

        // Second pass: score and find lowest (least valuable).
        let mut best_path = None;
        let mut best_score = f64::MAX;
        for path in &paths {
            if exclude.is_some_and(|ex| canonical(path) == canonical(ex)) {
                continue;
            }
            let score = self.score_file(path);
            if score < best_score {
                best_score = score;
                best_path = Some(path.clone());
            }
        }
        best_path
    }

    // ── Ghost list ──

    fn record_ghost(&mut self, path: &PathBuf, features: [f64; NUM_FEATURES]) {
        if self.ghost_set.contains(path) {
            return;
        }
        self.ghost.push_front(GhostEntry {
            path: path.clone(),
            features,
        });
        self.ghost_set.insert(path.clone());
    }

    fn process_ghost_hit(&mut self, path: &Path) -> bool {
        if !self.ghost_set.remove(path) {
            return false;
        }
        // Find and remove from ghost deque, collecting features.
        if let Some(pos) = self.ghost.iter().position(|g| g.path == path) {
            let entry = self.ghost.remove(pos).unwrap();
            self.training_samples.push((entry.features, 1.0));
            return true;
        }
        false
    }

    fn expire_ghosts(&mut self) {
        while self.ghost.len() > self.ghost_cap {
            if let Some(entry) = self.ghost.pop_back() {
                self.ghost_set.remove(&entry.path);
                self.training_samples.push((entry.features, 0.0));
            }
        }
    }

    // ── Tree retraining ──

    fn maybe_retrain(&mut self) {
        if !self.eviction_count.is_multiple_of(self.retrain_interval) {
            return;
        }
        if self.training_samples.len() < self.min_training_samples {
            return;
        }

        // Normalize all training samples using current min/max.
        let rows: Vec<Vec<f64>> = self
            .training_samples
            .iter()
            .map(|(raw, _)| self.normalize(raw).to_vec())
            .collect();
        let labels: Vec<f64> = self
            .training_samples
            .iter()
            .map(|(_, label)| *label)
            .collect();

        let x = DenseMatrix::from_2d_vec(&rows);
        let params = DecisionTreeRegressorParameters::default()
            .with_max_depth(self.tree_max_depth)
            .with_min_samples_leaf(self.tree_min_samples_leaf);

        if let Ok(new_tree) = DecisionTreeRegressor::fit(&x, &labels, params) {
            self.tree = Some(new_tree);
            // Keep most recent half to avoid unbounded growth.
            let keep = self.training_samples.len() / 2;
            self.training_samples
                .drain(..self.training_samples.len() - keep);
        }
    }

    // ── Normalization decay ──

    /// Contract normalization bounds toward the center so ancient outliers
    /// don't permanently compress the feature space.
    fn decay_normalization(&mut self) {
        for i in 0..NUM_FEATURES {
            let mid = (self.feat_min[i] + self.feat_max[i]) / 2.0;
            self.feat_min[i] += (mid - self.feat_min[i]) * NORM_DECAY_RATE;
            self.feat_max[i] += (mid - self.feat_max[i]) * NORM_DECAY_RATE;
        }
    }

    // ── Per-file tracking ──

    fn touch_file(&mut self, path: &Path, ts: SystemTime) {
        let p = canonical(path);

        // Update inter-access gap using real time (milliseconds).
        if let Some(prev_time) = self.last_access_time.get(&p) {
            let gap_ms = ts
                .duration_since(*prev_time)
                .unwrap_or_default()
                .as_millis() as u64;
            *self.inter_access_ms_sum.entry(p.clone()).or_insert(0) += gap_ms;
        }

        // Update access tracking.
        self.last_access_time.insert(p.clone(), ts);
        self.last_access.insert(p.clone(), self.logical_time);
        *self.access_count.entry(p).or_insert(0) += 1;
        self.logical_time += 1;
    }

    fn insert_new_file(&mut self, path: &Path, ts: SystemTime) {
        self.last_access
            .insert(path.to_path_buf(), self.logical_time);
        self.last_access_time.insert(path.to_path_buf(), ts);
        self.access_count.insert(path.to_path_buf(), 1);
        self.inter_access_ms_sum.insert(path.to_path_buf(), 0);
        self.logical_time += 1;
    }

    fn remove_from_tracking(&mut self, path: &PathBuf) {
        self.hot_sizes.remove(path);
        self.last_access.remove(path);
        self.last_access_time.remove(path);
        self.access_count.remove(path);
        self.inter_access_ms_sum.remove(path);
    }

    fn recalc_params(&mut self) {
        let c = self.hot_sizes.len().max(1);
        self.ghost_cap = (2 * c).max(self.ghost_cap_min);
    }

    // ── Filesystem helpers (same pattern as lecar) ──

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

    // ── Eviction ──

    fn evict_one(
        &mut self,
        hot_root: &Path,
        cold: &Path,
        exclude: Option<&Path>,
    ) -> Result<bool, Box<dyn std::error::Error + Send + Sync>> {
        let victim = match self.find_victim(exclude) {
            Some(v) => v,
            None => return Ok(false),
        };

        // Record features for ghost tracking before eviction.
        let features = self.extract_raw_features(&victim);
        self.update_min_max(&features);
        self.record_ghost(&victim, features);

        self.do_evict(&victim, hot_root, cold)?;
        self.eviction_count += 1;
        self.maybe_retrain();

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

impl PolicyEngine for DecisionTreePolicy {
    fn validate_config(
        _hot: &Path,
        cold_storage: &[PathBuf],
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        if cold_storage.len() != 1 {
            return Err("decision_tree requires exactly one cold_storage tier".into());
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
                        self.last_access.remove(&path);
                        self.last_access_time.remove(&path);
                        self.access_count.remove(&path);
                        self.inter_access_ms_sum.remove(&path);
                    }
                }
                _ => {}
            }
        }

        // Build touched list — same 3-part filter as other policies.
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
        policy_log::log_ingest("decision_tree", events.len());
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
            self.hot_sizes.remove(&p);
            self.tier_state.adjust_hot_bytes(sz, 0);
            self.last_access.remove(&p);
            self.last_access_time.remove(&p);
            self.access_count.remove(&p);
            self.inter_access_ms_sum.remove(&p);
        }

        // ── Initial fill ──
        let fill_time = SystemTime::now();
        if self.hot_sizes.is_empty() && self.access_count.is_empty() {
            for p in Self::list_hot_files(&hot_root, &hot_root)? {
                if let Ok(meta) = fs::symlink_metadata(&p)
                    && !meta.file_type().is_symlink()
                    && let Ok(sz) = fs::metadata(&p).map(|m| m.len())
                {
                    self.hot_sizes.insert(p.clone(), sz);
                    self.insert_new_file(&p, fill_time);
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
                "decision_tree",
                self.hot_sizes.len(),
                self.tier_state.hot_bytes(),
            );
        }

        // ── Decay normalization bounds before scoring ──
        self.decay_normalization();

        // ── Two-pass touch processing ──
        //
        // Pass 1: Update access tracking for ALL touched files so that
        //         eviction decisions in pass 2 see fully-updated features.
        // Pass 2: Perform promotions and evictions.
        self.touched.sort_by(|a, b| a.1.cmp(&b.1));
        let touches = std::mem::take(&mut self.touched);
        let had_touches = !touches.is_empty();
        let mut new_in_hot = 0u32;
        let mut promoted = 0u32;
        let mut evicted_room = 0u32;

        let mut to_promote: Vec<PathBuf> = Vec::new();
        let mut promote_set: HashSet<PathBuf> = HashSet::new();

        // ── Pass 1: classify touches and update access tracking ──
        for (mut path, ts) in touches {
            // Map cold paths to logical hot paths.
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
                    self.last_access.remove(&path);
                    self.last_access_time.remove(&path);
                    self.access_count.remove(&path);
                    self.inter_access_ms_sum.remove(&path);
                    continue;
                }
            };

            if meta.is_dir() {
                continue;
            }

            if meta.file_type().is_symlink() {
                // Cold file: update tracking, queue for promotion (deduped).
                self.touch_file(&path, ts);
                if promote_set.insert(path.clone()) {
                    to_promote.push(path);
                }
            } else if !self.hot_sizes.contains_key(&path) {
                // New hot file.
                if let Ok(sz) = fs::metadata(&path).map(|m| m.len()) {
                    self.tier_state.adjust_hot_bytes(0, sz);
                    self.hot_sizes.insert(path.clone(), sz);
                    new_in_hot += 1;
                }
                self.insert_new_file(&path, ts);
                self.recalc_params();
            } else {
                // Known hot file.
                self.touch_file(&path, ts);
            }
        }

        // ── Pass 2: promote cold files (features now fully updated) ──
        for path in to_promote {
            self.process_ghost_hit(&path);

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
                // Access tracking already updated in pass 1 — no insert_new_file.
                promoted += 1;
                self.recalc_params();
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

        // ── Expire old ghosts ──
        self.expire_ghosts();

        if had_touches || evicted > 0 {
            policy_log::log_reorganize_done(policy_log::ReorganizeDoneParams {
                policy_name: "decision_tree",
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
        let mut policy = DecisionTreePolicy::new(tier_state);
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

    #[test]
    fn ghost_hit_adds_training_sample() {
        let (hot_dir, cold_dir) = setup_dirs();
        let hot_root = fs::canonicalize(hot_dir.path()).unwrap();
        let cold_root = fs::canonicalize(cold_dir.path()).unwrap();
        // Create 3 files, capacity for 2 → 1 gets evicted.
        fs::write(hot_root.join("f1"), b"aaaaa").unwrap();
        fs::write(hot_root.join("f2"), b"bbbbb").unwrap();
        fs::write(hot_root.join("f3"), b"ccccc").unwrap();
        let mut tier_state = TierState::new(
            hot_root.clone(),
            vec![cold_root.clone()],
            12, // fits 2 files of 5 bytes
            vec![u64::MAX],
        );
        tier_state.init_bytes().unwrap();
        let mut policy = DecisionTreePolicy::new(tier_state);
        policy.reorganize().unwrap();

        assert_eq!(policy.total_demotions, 1);
        assert_eq!(policy.ghost.len(), 1);

        // Touch the evicted file to trigger ghost hit.
        let evicted_path = ["f1", "f2", "f3"]
            .iter()
            .map(|f| hot_root.join(f))
            .find(|p| {
                fs::symlink_metadata(p)
                    .map(|m| m.file_type().is_symlink())
                    .unwrap_or(false)
            })
            .expect("one file should be evicted");

        let now = SystemTime::now();
        policy.ingest(&[AccessEvent {
            path: evicted_path.clone(),
            kind: FsEventKind::Modify,
            timestamp: now,
        }]);
        policy.reorganize().unwrap();

        // Ghost hit should have added a training sample with label=1.0.
        assert!(
            policy
                .training_samples
                .iter()
                .any(|(_, label)| *label == 1.0),
            "should have a training sample with label=1.0 from ghost hit"
        );
    }

    #[test]
    fn tree_retrains_after_interval() {
        let (hot_dir, cold_dir) = setup_dirs();
        let hot_root = fs::canonicalize(hot_dir.path()).unwrap();
        let cold_root = fs::canonicalize(cold_dir.path()).unwrap();
        let mut tier_state = TierState::new(hot_root, vec![cold_root], 1000, vec![u64::MAX]);
        tier_state.init_bytes().unwrap();
        let mut policy = DecisionTreePolicy::new(tier_state);

        // Set up valid normalization bounds.
        policy.feat_min = [0.0; NUM_FEATURES];
        policy.feat_max = [100.0, 50.0, 10000.0, 200.0];

        // Add enough training samples.
        for i in 0..25 {
            let recency = (i * 4) as f64;
            let freq = (i % 10 + 1) as f64;
            let size = ((i + 1) * 100) as f64;
            let inter = (i * 3) as f64;
            let label = if i % 2 == 0 { 1.0 } else { 0.0 };
            policy
                .training_samples
                .push(([recency, freq, size, inter], label));
        }

        assert!(policy.tree.is_none());

        // Set eviction count to trigger retrain.
        policy.eviction_count = policy.retrain_interval;
        policy.maybe_retrain();

        assert!(
            policy.tree.is_some(),
            "tree should have been trained after reaching retrain interval with enough samples"
        );
    }
}
