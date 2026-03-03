//! Workload execution and trace-based benchmarking.
//!
//! ## Design
//!
//! The benchmark separates **trace generation** from **trace replay**:
//!
//! 1. `generate_trace` — produces a concrete `Vec<WorkloadOp>` by simulating the
//!    live-list state (just a count) and sampling op types and file sizes from the
//!    config distribution. File sizes are drawn uniformly from
//!    `[min_file_size, max_file_size]`, giving a realistic size distribution.
//!
//! 2. `run_with_trace` — replays an already-generated trace against one policy
//!    on a fresh temporary filesystem. Because every policy in a preset gets the
//!    *same* trace, the comparison is a true head-to-head: same files, same sizes,
//!    same access pattern, same daemon poll schedule.
//!
//! 3. `run` — convenience wrapper: generates a fresh trace then replays it. Used
//!    by the CLI (`tiering_bench`) where fairness across policies isn't needed.
//!
//! ## Daemon simulation
//!
//! The daemon is simulated synchronously: every `poll_interval_ops` workload
//! operations, accumulated `AccessEvent`s are fed to `policy.ingest` and
//! `policy.reorganize` is called. This keeps the benchmark deterministic and
//! decouples policy compute time from the workload metrics.
//!
//! ## Primary metrics
//!
//! - **Hit rate** — fraction of edit operations that land on a hot file (regular
//!   file, not a symlink). A policy that correctly places the working set in hot
//!   approaches 1.0; a policy that always evicts the wrong files approaches 0.0.
//! - **Bytes written to tier** — bytes the daemon wrote to each storage layer
//!   (hot = promotions, cold-i = demotions to tier i). Measures I/O cost of
//!   achieving the observed placement.

use std::fs;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use anyhow::Result;
use rand::Rng;

use crate::daemon::make_policy;
use crate::policy_engine::{AccessEvent, FsEventKind, PolicyEngine};

// ── Configuration ──────────────────────────────────────────────────────────────

/// Parameters shared by trace generation and replay.
#[derive(Debug, Clone)]
pub struct WorkloadConfig {
    pub policy: String,
    /// Operations to run before the measurement window (warms up policy state).
    pub warmup_ops: u64,
    /// Operations to run during the measurement window.
    pub measure_ops: u64,
    /// Daemon wakes (ingest + reorganize) after every this many workload operations.
    pub poll_interval_ops: usize,
    pub depth: usize,
    pub hot_capacity: u64,
    /// Minimum file size in bytes (inclusive). Files are sized uniformly in [min, max].
    pub min_file_size: usize,
    /// Maximum file size in bytes (inclusive).
    pub max_file_size: usize,
    pub create_pct: u8,
    pub delete_pct: u8,
    pub edit_pct: u8,
    /// Edit-target skew. 1.0 = uniform; >1 = concentrated on oldest files (low
    /// index in the live list); <1 = concentrated on newest files (high index).
    /// Sampled as floor(len * u^skew), u ~ Uniform[0,1).
    pub skew: f64,
}

// ── Trace ──────────────────────────────────────────────────────────────────────

/// A single pre-determined workload step. All per-step randomness (op type,
/// target index, file size) is fixed at trace-generation time so the same
/// sequence can be replayed identically for every policy.
#[derive(Debug, Clone)]
pub enum WorkloadOp {
    /// Create a new file with the given ID and size.
    Create { file_id: usize, size: usize },
    /// Delete the file at `live_idx` in the live list.
    Delete { live_idx: usize },
    /// Overwrite the file at `live_idx` with `size` bytes.
    Edit { live_idx: usize, size: usize },
}

/// Pre-generated workload trace: warmup ops followed by measurement ops.
/// Generated once per preset and replayed for every policy.
pub struct WorkloadTrace {
    pub warmup: Vec<WorkloadOp>,
    pub measure: Vec<WorkloadOp>,
}

/// Generate a complete workload trace from `config` using `rng`.
///
/// The live-list size is simulated during generation (we only track the count,
/// not actual paths), which lets us produce valid `live_idx` values without
/// touching the filesystem. Replay uses the same indices against a real `Vec<PathBuf>`.
pub fn generate_trace<R: Rng>(config: &WorkloadConfig, rng: &mut R) -> WorkloadTrace {
    let total = (config.warmup_ops + config.measure_ops) as usize;
    let mut ops = Vec::with_capacity(total);
    let mut live_count = 0usize;
    let mut file_counter = 0usize;

    for _ in 0..total {
        let op = match choose_op_type(config, live_count, rng) {
            OpType::Create => {
                let size = rng.gen_range(config.min_file_size..=config.max_file_size);
                let id = file_counter;
                file_counter += 1;
                live_count += 1;
                WorkloadOp::Create { file_id: id, size }
            }
            OpType::Delete => {
                let idx = rng.gen_range(0..live_count);
                live_count -= 1;
                WorkloadOp::Delete { live_idx: idx }
            }
            OpType::Edit => {
                let idx = skewed_index(live_count, config.skew, rng);
                let size = rng.gen_range(config.min_file_size..=config.max_file_size);
                WorkloadOp::Edit { live_idx: idx, size }
            }
        };
        ops.push(op);
    }

    let measure = ops.split_off(config.warmup_ops as usize);
    WorkloadTrace { warmup: ops, measure }
}

// ── Results ────────────────────────────────────────────────────────────────────

/// Outcome of one benchmark run — all metrics cover the measurement window only
/// (warmup is excluded by diffing policy stats before and after).
#[derive(Debug)]
pub struct BenchResult {
    pub config: WorkloadConfig,
    /// File creates in the measurement window.
    pub total_creates: u64,
    /// File deletes in the measurement window.
    pub total_deletes: u64,
    /// Edit operations in the measurement window.
    pub total_edits: u64,
    /// Edits that landed on a hot file (regular file at hot path).
    pub hot_edits: u64,
    /// Edits that landed on a cold file (symlink to cold tier).
    pub cold_edits: u64,
    /// hot_edits / total_edits. 1.0 = perfect placement, 0.0 = everything cold.
    pub hit_rate: f64,
    /// Promotions (cold → hot) triggered during measurement.
    pub promotions: u64,
    /// Demotions (hot → cold) triggered during measurement.
    pub demotions: u64,
    /// Demotions specifically to cold tier 0.
    pub demotions_tier0: u64,
    /// Bytes the daemon wrote to each storage layer during measurement.
    /// `[0]` = hot tier (promotions); `[i+1]` = cold tier i (demotions).
    pub bytes_written_to_tier: Vec<u64>,
}

/// CSV header matching `to_csv_row`.
pub const CSV_HEADER: &str = "policy,warmup_ops,measure_ops,poll_interval_ops,depth,hot_cap,\
min_file_size,max_file_size,create_pct,delete_pct,edit_pct,skew,\
total_creates,total_deletes,total_edits,hot_edits,cold_edits,hit_rate,\
promotions,demotions,demotions_tier0,bytes_wr_hot,bytes_wr_cold0";

impl BenchResult {
    pub fn to_csv_row(&self) -> String {
        let bw_hot = self.bytes_written_to_tier.first().copied().unwrap_or(0);
        let bw_cold0 = self.bytes_written_to_tier.get(1).copied().unwrap_or(0);
        format!(
            "{},{},{},{},{},{},{},{},{},{},{},{:.1},{},{},{},{},{},{:.6},{},{},{},{},{}",
            self.config.policy,
            self.config.warmup_ops,
            self.config.measure_ops,
            self.config.poll_interval_ops,
            self.config.depth,
            self.config.hot_capacity,
            self.config.min_file_size,
            self.config.max_file_size,
            self.config.create_pct,
            self.config.delete_pct,
            self.config.edit_pct,
            self.config.skew,
            self.total_creates,
            self.total_deletes,
            self.total_edits,
            self.hot_edits,
            self.cold_edits,
            self.hit_rate,
            self.promotions,
            self.demotions,
            self.demotions_tier0,
            bw_hot,
            bw_cold0,
        )
    }

    pub fn to_pretty_string(&self) -> String {
        use crate::capacity::format_capacity;
        let tier_lines: String = self
            .bytes_written_to_tier
            .iter()
            .enumerate()
            .map(|(i, &b)| {
                if i == 0 {
                    format!("  bytes written → hot    {} B\n", b)
                } else {
                    format!("  bytes written → cold{:<2} {} B\n", i - 1, b)
                }
            })
            .collect();
        format!(
            r#"  policy              {}
  warmup_ops          {}
  measure_ops         {}
  poll_interval_ops   {}
  depth               {}
  hot_capacity        {}
  file_size           {}–{} B  (uniform random per file/edit)
  create/delete/edit  {}% / {}% / {}%
  skew                {:.1}  (1.0 = uniform, >1 = old files, <1 = new files)
  ─────────────────────────────────
  creates             {}
  deletes             {}
  total_edits         {}
  hit_rate            {:.2}%  ({} hot / {} cold)
  promotions          {}
  demotions           {}  [tier-0: {}]
{}
"#,
            self.config.policy,
            self.config.warmup_ops,
            self.config.measure_ops,
            self.config.poll_interval_ops,
            self.config.depth,
            format_capacity(self.config.hot_capacity),
            self.config.min_file_size,
            self.config.max_file_size,
            self.config.create_pct,
            self.config.delete_pct,
            self.config.edit_pct,
            self.config.skew,
            self.total_creates,
            self.total_deletes,
            self.total_edits,
            self.hit_rate * 100.0,
            self.hot_edits,
            self.cold_edits,
            self.promotions,
            self.demotions,
            self.demotions_tier0,
            tier_lines.trim_end(),
        )
    }
}

// ── Runners ────────────────────────────────────────────────────────────────────

/// Replay `trace` for `config.policy` on a fresh filesystem. All policies given
/// the same trace see identical operations — same files, same sizes, same access
/// pattern, same daemon poll schedule.
pub fn run_with_trace(config: &WorkloadConfig, trace: &WorkloadTrace) -> Result<BenchResult> {
    let dir = tempfile::tempdir()?;
    let hot_path = dir.path().join("hot");
    let cold_path = dir.path().join("cold");
    fs::create_dir_all(&hot_path)?;
    fs::create_dir_all(&cold_path)?;

    let mut policy = make_policy(
        &config.policy,
        &hot_path,
        std::slice::from_ref(&cold_path),
        config.hot_capacity,
        vec![u64::MAX],
    )?;

    let now = SystemTime::now();
    let mut live: Vec<PathBuf> = Vec::new();

    replay_phase(config, &hot_path, &trace.warmup, &mut *policy, &mut live, now)?;
    let stats_pre = policy.stats();

    let phase = replay_phase(config, &hot_path, &trace.measure, &mut *policy, &mut live, now)?;
    let stats_post = policy.stats();

    let promotions = stats_post.promotions.saturating_sub(stats_pre.promotions);
    let demotions = stats_post.demotions.saturating_sub(stats_pre.demotions);
    let d0_post = stats_post.demotions_to_tier.first().copied().unwrap_or(0);
    let d0_pre = stats_pre.demotions_to_tier.first().copied().unwrap_or(0);
    let demotions_tier0 = d0_post.saturating_sub(d0_pre);

    let n = stats_post
        .bytes_written_to_tier
        .len()
        .max(stats_pre.bytes_written_to_tier.len());
    let bytes_written_to_tier: Vec<u64> = (0..n)
        .map(|i| {
            let post = stats_post.bytes_written_to_tier.get(i).copied().unwrap_or(0);
            let pre = stats_pre.bytes_written_to_tier.get(i).copied().unwrap_or(0);
            post.saturating_sub(pre)
        })
        .collect();

    let total_edits = phase.hot_edits + phase.cold_edits;
    let hit_rate = if total_edits > 0 {
        phase.hot_edits as f64 / total_edits as f64
    } else {
        0.0
    };

    Ok(BenchResult {
        config: config.clone(),
        total_creates: phase.creates,
        total_deletes: phase.deletes,
        total_edits,
        hot_edits: phase.hot_edits,
        cold_edits: phase.cold_edits,
        hit_rate,
        promotions,
        demotions,
        demotions_tier0,
        bytes_written_to_tier,
    })
}

/// Convenience wrapper: generate a fresh trace then replay it. Use this when
/// running a single policy (e.g. from the CLI). For multi-policy comparison,
/// prefer `generate_trace` + `run_with_trace` so all policies share the trace.
pub fn run(config: WorkloadConfig) -> Result<BenchResult> {
    let mut rng = rand::thread_rng();
    let trace = generate_trace(&config, &mut rng);
    run_with_trace(&config, &trace)
}

// ── Internal ───────────────────────────────────────────────────────────────────

struct PhaseResult {
    creates: u64,
    deletes: u64,
    hot_edits: u64,
    cold_edits: u64,
}

fn replay_phase(
    config: &WorkloadConfig,
    hot_path: &Path,
    ops: &[WorkloadOp],
    policy: &mut dyn PolicyEngine,
    live: &mut Vec<PathBuf>,
    now: SystemTime,
) -> Result<PhaseResult> {
    let mut events: Vec<AccessEvent> = Vec::new();
    let mut creates = 0u64;
    let mut deletes = 0u64;
    let mut hot_edits = 0u64;
    let mut cold_edits = 0u64;

    for (i, op) in ops.iter().enumerate() {
        match op {
            WorkloadOp::Create { file_id, size } => {
                creates += 1;
                let path = make_nested_path(hot_path, config.depth, *file_id);
                create_file(&path, *size)?;
                live.push(path.clone());
                events.push(AccessEvent { path: path.clone(), kind: FsEventKind::Create, timestamp: now });
                events.push(AccessEvent { path, kind: FsEventKind::Modify, timestamp: now });
            }
            WorkloadOp::Delete { live_idx } => {
                deletes += 1;
                // live_idx is always valid (generated against the same simulated live count)
                let path = live.remove(*live_idx);
                fs::remove_file(&path).ok();
                events.push(AccessEvent { path, kind: FsEventKind::Remove, timestamp: now });
            }
            WorkloadOp::Edit { live_idx, size } => {
                let path = &live[*live_idx];
                if let Ok(meta) = fs::symlink_metadata(path) {
                    if meta.file_type().is_symlink() {
                        cold_edits += 1;
                    } else {
                        hot_edits += 1;
                    }
                    fs::write(path, vec![b'x'; *size])?;
                    events.push(AccessEvent {
                        path: path.clone(),
                        kind: FsEventKind::Modify,
                        timestamp: now,
                    });
                }
            }
        }

        // Daemon poll: flush accumulated events every poll_interval_ops operations.
        if (i + 1) % config.poll_interval_ops == 0 {
            policy.ingest(&events);
            policy.reorganize().map_err(|e| anyhow::anyhow!("{}", e))?;
            events.clear();
        }
    }

    // Final flush for any remaining events.
    if !events.is_empty() {
        policy.ingest(&events);
        policy.reorganize().map_err(|e| anyhow::anyhow!("{}", e))?;
    }

    Ok(PhaseResult { creates, deletes, hot_edits, cold_edits })
}

enum OpType { Create, Delete, Edit }

fn choose_op_type<R: Rng>(config: &WorkloadConfig, live_count: usize, rng: &mut R) -> OpType {
    if live_count == 0 {
        return OpType::Create;
    }
    let c = config.create_pct as u32;
    let d = config.delete_pct as u32;
    let e = config.edit_pct as u32;
    let total = c + d + e;
    if total == 0 {
        return OpType::Create;
    }
    let x = rng.gen_range(0..total);
    if x < c { OpType::Create } else if x < c + d { OpType::Delete } else { OpType::Edit }
}

fn make_nested_path(root: &Path, depth: usize, file_id: usize) -> PathBuf {
    let mut p = root.to_path_buf();
    for i in 0..depth { p.push(format!("dir_{}", i)); }
    fs::create_dir_all(&p).ok();
    p.push(format!("f_{}.dat", file_id));
    p
}

/// Power-law index: skew=1.0 → uniform; skew>1 → concentrated toward low indices
/// (oldest files); skew<1 → concentrated toward high indices (newest files).
fn skewed_index<R: Rng>(len: usize, skew: f64, rng: &mut R) -> usize {
    let u: f64 = rng.r#gen();
    ((len as f64 * u.powf(skew)) as usize).min(len - 1)
}

fn create_file(path: &Path, size: usize) -> Result<()> {
    if let Some(parent) = path.parent() { fs::create_dir_all(parent)?; }
    fs::write(path, vec![b'x'; size])?;
    Ok(())
}
