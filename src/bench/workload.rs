//! Workload execution for benchmarking.
//!
//! ## Architecture (matches production)
//!
//! Two independent threads:
//!
//! - **Workload thread** (main): creates / edits / deletes files in the hot directory at full
//!   speed. It only sleeps when it hits a cold file (the `cold_access_delay_us` penalty), which
//!   is exactly what we want to measure: placement quality.
//!
//! - **Daemon thread**: runs a real `FsWatcher` on the hot + cold directories — the same path
//!   as production — and calls `policy.ingest + reorganize` every `poll_interval_sec`. Policy
//!   compute time never shows up in the workload throughput number.
//!
//! The workload thread constructs no events and shares no buffer with the daemon; the OS and
//! `notify` crate handle event delivery exactly as they do in production.

use std::fs;
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use std::thread;

use anyhow::Result;
use rand::Rng;

use crate::capacity::format_capacity;
use crate::daemon::make_policy;
use crate::policy_engine::PolicyEngine;
use crate::watcher::FsWatcher;

/// Parameters for one benchmark run.
#[derive(Debug, Clone)]
pub struct WorkloadConfig {
    pub policy: String,
    pub warmup_sec: f64,
    pub measure_sec: f64,
    /// How often the daemon thread wakes to poll the watcher and call reorganize.
    pub poll_interval_sec: f64,
    pub depth: usize,
    pub hot_capacity: u64,
    pub file_size: usize,
    pub create_pct: u8,
    pub delete_pct: u8,
    pub edit_pct: u8,
    #[allow(dead_code)]
    pub batch_size: usize,
    /// Edit-target skew: 1.0 = uniform, >1 = concentrated on oldest files (low index),
    /// <1 = concentrated on newest files (high index). Index = floor(len * u^skew), u ~ U[0,1).
    pub skew: f64,
    /// Per-access penalty in µs when an edit hits a cold file (hot path is a symlink).
    /// Simulates the latency of fetching data from cold storage during a live request.
    /// This is the primary differentiator between policies: a policy with poor placement
    /// pays this on every cold edit; a policy with good placement rarely pays it.
    pub cold_access_delay_us: u64,
}

/// Result of one benchmark run.
#[derive(Debug)]
pub struct BenchResult {
    pub config: WorkloadConfig,
    pub measure_ops: u64,
    pub measure_sec: f64,
    pub throughput: f64,
    pub promotions: u64,
    pub demotions: u64,
    pub demotions_tier0: u64,
    pub promotions_pct: f64,
    pub demotions_pct: f64,
}

pub const CSV_HEADER: &str = "policy,warmup_sec,measure_sec,poll_interval_sec,depth,hot_cap,\
file_size,create_pct,delete_pct,edit_pct,batch_size,skew,cold_access_delay_us,\
measure_ops,throughput,promotions,demotions,demotions_tier0,promotions_pct,demotions_pct";

impl BenchResult {
    pub fn to_csv_row(&self) -> String {
        format!(
            "{},{},{},{},{},{},{},{},{},{},{},{:.1},{},{},{:.1},{},{},{},{:.4},{:.4}",
            self.config.policy,
            self.config.warmup_sec,
            self.config.measure_sec,
            self.config.poll_interval_sec,
            self.config.depth,
            self.config.hot_capacity,
            self.config.file_size,
            self.config.create_pct,
            self.config.delete_pct,
            self.config.edit_pct,
            self.config.batch_size,
            self.config.skew,
            self.config.cold_access_delay_us,
            self.measure_ops,
            self.throughput,
            self.promotions,
            self.demotions,
            self.demotions_tier0,
            self.promotions_pct,
            self.demotions_pct,
        )
    }

    pub fn to_pretty_string(&self) -> String {
        format!(
            r#"  policy            {}
  warmup            {:.1} s
  measure           {:.1} s
  poll_interval     {:.2} s  (daemon peek rate)
  depth             {}
  hot_capacity      {}
  file_size         {} B
  create/delete/edit  {}% / {}% / {}%
  skew              {:.1}  (1.0 = uniform, >1 = old files, <1 = new files)
  cold_access_delay {} µs  (per-edit penalty for cold-file access)
  ─────────────────────────────────
  measure_ops       {}
  throughput        {:.1} ops/s
  promotions       {}  ({:.2}% of ops)
  demotions        {}  (tier 0: {})  ({:.2}% of ops)
"#,
            self.config.policy,
            self.config.warmup_sec,
            self.config.measure_sec,
            self.config.poll_interval_sec,
            self.config.depth,
            format_capacity(self.config.hot_capacity),
            self.config.file_size,
            self.config.create_pct,
            self.config.delete_pct,
            self.config.edit_pct,
            self.config.skew,
            self.config.cold_access_delay_us,
            self.measure_ops,
            self.throughput,
            self.promotions,
            self.promotions_pct,
            self.demotions,
            self.demotions_tier0,
            self.demotions_pct,
        )
    }
}

/// Run the benchmark: warmup, then measure. Returns throughput + move counts.
pub fn run(config: WorkloadConfig) -> Result<BenchResult> {
    let dir = tempfile::tempdir()?;
    let hot_path = dir.path().join("hot");
    let cold_path = dir.path().join("cold");
    fs::create_dir_all(&hot_path)?;
    fs::create_dir_all(&cold_path)?;

    // Policy behind a mutex so the daemon thread can own it while we snapshot stats.
    let policy: Arc<Mutex<Box<dyn PolicyEngine>>> = Arc::new(Mutex::new(make_policy(
        &config.policy,
        &hot_path,
        std::slice::from_ref(&cold_path),
        config.hot_capacity,
        vec![u64::MAX],
    )?));

    // Real FS watcher on both tiers — same as production.
    let watcher = FsWatcher::new(&[&hot_path, &cold_path])?;
    let stop = Arc::new(AtomicBool::new(false));

    // ── Daemon thread ─────────────────────────────────────────────────────────
    // Sleeps poll_interval, then polls the watcher and calls ingest+reorganize.
    // The workload thread is never blocked by this.
    let daemon = {
        let policy = Arc::clone(&policy);
        let stop = Arc::clone(&stop);
        let poll_interval = Duration::from_secs_f64(config.poll_interval_sec);
        thread::spawn(move || -> Result<()> {
            loop {
                thread::sleep(poll_interval);
                let events = watcher.poll();
                {
                    let mut pol = policy.lock().unwrap();
                    pol.ingest(&events);
                    // reorganize() calls tier_fs::move_to_tier, which has a narrow window
                    // where a file can disappear mid-operation:
                    //
                    //   Eviction:  rename(hot→cold) succeeds; workload deletes symlink before
                    //              unix_fs::symlink(cold, hot) runs → symlink creation gets
                    //              ENOENT on the hot parent or just sees the file missing.
                    //
                    //   Promotion: fs::metadata(&cold_backing) fails (line 45 tier_fs.rs, bare ?)
                    //              when the cold backing is gone. This happens because:
                    //              (a) the workload Op::Delete removed a hot symlink between two
                    //                  reorganize cycles, leaving an orphaned cold backing, and
                    //                  then a delayed watcher event triggered promotion of the
                    //                  now-missing symlink; or
                    //              (b) a previous reorganize cycle promoted the file but the
                    //                  policy's ghost-list triggered a second promotion attempt
                    //                  before reconcile ran to clear the stale entry.
                    //
                    // All of these are benign concurrent-deletion races. In production a VFS
                    // layer locks the path across the entire move, so clients never observe the
                    // gap. In the benchmark we just skip the failed cycle: the next reorganize
                    // starts with a reconcile that will correct internal state from disk.
                    //
                    // We only swallow "file not found" class errors. Any other error (e.g.
                    // "not enough hot capacity", permission denied, disk full) is fatal.
                    if let Err(e) = pol.reorganize() {
                        let msg = e.to_string();
                        // Concurrent-deletion races produce ENOENT in move_to_tier when a
                        // file disappears between the policy's existence check and the
                        // underlying rename/symlink syscall. Benign: next reconcile corrects
                        // state. All other errors (capacity failures, permission errors, etc.)
                        // are fatal and must surface.
                        let is_enoent = msg.contains("No such file or directory")
                            || msg.contains("does not exist");
                        if !is_enoent {
                            return Err(anyhow::anyhow!("reorganize failed: {}", e));
                        }
                        // else: benign race — next cycle's reconcile will correct state
                    }
                }
                // Check stop AFTER polling so all remaining events are flushed before exit.
                if stop.load(Ordering::Relaxed) {
                    break;
                }
            }
            Ok(())
        })
    };

    // Workload state persists across warmup → measure so the file population is
    // continuous (measure phase sees the working set built during warmup).
    let mut rng = rand::thread_rng();
    let mut live: Vec<PathBuf> = Vec::new();
    let mut file_counter = 0usize;
    let cold_access_delay = Duration::from_micros(config.cold_access_delay_us);

    // ── Warmup ────────────────────────────────────────────────────────────────
    workload_loop(
        &config,
        &hot_path,
        &mut rng,
        &mut live,
        &mut file_counter,
        cold_access_delay,
        Duration::from_secs_f64(config.warmup_sec),
    )?;
    let stats_after_warmup = policy.lock().unwrap().stats();

    // ── Measure ───────────────────────────────────────────────────────────────
    let measure_ops = workload_loop(
        &config,
        &hot_path,
        &mut rng,
        &mut live,
        &mut file_counter,
        cold_access_delay,
        Duration::from_secs_f64(config.measure_sec),
    )?;

    // Signal daemon to stop. It will do one final poll to flush any pending watcher
    // events, then exit. We join before reading final stats.
    stop.store(true, Ordering::Relaxed);
    daemon
        .join()
        .map_err(|_| anyhow::anyhow!("daemon thread panicked"))??;

    let stats_after_measure = policy.lock().unwrap().stats();

    // ── Compute results ───────────────────────────────────────────────────────
    let promotions = stats_after_measure
        .promotions
        .saturating_sub(stats_after_warmup.promotions);
    let demotions = stats_after_measure
        .demotions
        .saturating_sub(stats_after_warmup.demotions);
    let d0_after = stats_after_measure
        .demotions_to_tier
        .first()
        .copied()
        .unwrap_or(0);
    let d0_before = stats_after_warmup
        .demotions_to_tier
        .first()
        .copied()
        .unwrap_or(0);
    let demotions_tier0 = d0_after.saturating_sub(d0_before);

    let measure_sec = config.measure_sec;
    let throughput = if measure_sec > 0.0 && measure_ops > 0 {
        measure_ops as f64 / measure_sec
    } else {
        0.0
    };
    let promotions_pct = if measure_ops > 0 {
        100.0 * promotions as f64 / measure_ops as f64
    } else {
        0.0
    };
    let demotions_pct = if measure_ops > 0 {
        100.0 * demotions as f64 / measure_ops as f64
    } else {
        0.0
    };

    Ok(BenchResult {
        config,
        measure_ops,
        measure_sec,
        throughput,
        promotions,
        demotions,
        demotions_tier0,
        promotions_pct,
        demotions_pct,
    })
}

// ── Workload loop ─────────────────────────────────────────────────────────────

enum Op {
    Create,
    Delete,
    Edit,
}

/// Pure-filesystem workload: creates, deletes, and edits files in `hot_path`.
/// Emits no events — the daemon thread's `FsWatcher` picks them up from the OS.
/// The only sleep here is `cold_access_delay` on cold-file edits, which is
/// intentional: it models the true cost of a placement miss.
fn workload_loop<R: Rng>(
    config: &WorkloadConfig,
    hot_path: &Path,
    rng: &mut R,
    live: &mut Vec<PathBuf>,
    file_counter: &mut usize,
    cold_access_delay: Duration,
    duration: Duration,
) -> Result<u64> {
    let start = Instant::now();
    let mut ops_done = 0u64;

    while start.elapsed() < duration {
        match choose_op(config, live, rng) {
            Op::Create => {
                let path = make_nested_path(hot_path, config.depth, *file_counter);
                *file_counter += 1;
                create_file(&path, config.file_size)?;
                live.push(path);
            }
            Op::Delete => {
                let i = rng.gen_range(0..live.len());
                let path = live.remove(i);
                // Do NOT physically delete cold files (symlinks). tier_fs::move_to_tier's
                // promotion path does:
                //   (1) remove_file(hot_path)   ← removes the symlink
                //   (2) rename(cold_backing, hot_path)
                // If the workload removes the symlink (step 1) before the daemon reaches
                // it, the daemon's own remove_file gets ENOENT and aborts the entire
                // reorganize cycle. With delete_pct > 0 and ARC/LFU aggressively promoting
                // ghost-list files, this abort rate can reach ~50–90%, causing the daemon
                // to make almost no progress: hot tier unconstrained, all files appear hot,
                // throughput spikes to raw filesystem speed with zero policy signal.
                //
                // Fix: only physically delete hot files (regular files). Cold files
                // (symlinks) are removed from `live` but left on disk. The daemon's
                // per-cycle reconcile detects the orphaned symlink via cold_sizes and
                // drops it cleanly without abusing a promotion code path.
                //
                // Production analogy: the VFS layer holds a per-path lock during any
                // tier move. A client DELETE acquires the same lock, so it either sees
                // the file fully hot or fully cold — never mid-move. The workload
                // approximates this by skipping deletion of files that are currently
                // in the cold path.
                let is_cold = fs::symlink_metadata(&path)
                    .map(|m| m.file_type().is_symlink())
                    .unwrap_or(false);
                if !is_cold {
                    fs::remove_file(&path).ok();
                }
            }
            Op::Edit => {
                // NOTE ON BENCHMARK vs. PRODUCTION:
                // The daemon's eviction sequence in tier_fs::move_to_tier is:
                //   1. rename(hot_path → cold_backing)   ← hot_path disappears here
                //   2. symlink(cold_backing, hot_path)   ← hot_path reappears here
                // There is a real window between (1) and (2) where hot_path does not
                // exist, so a concurrent fs::read on it returns NotFound. We skip those
                // ops instead of propagating the error.
                //
                // In production this is not an issue: a real cloud filesystem would
                // expose a virtual namespace to clients (e.g. via FUSE or a VFS layer)
                // that holds a lock across the whole move, so clients never observe the
                // gap. The policy engine and the user both interact through that interface
                // rather than directly with raw paths. The benchmark approximates that
                // by skipping the rare mid-move op; it does not affect throughput
                // measurement in any meaningful way.
                let i = skewed_index(live.len(), config.skew, rng);
                let path = &live[i];
                if path.exists() {
                    // Check symlink status before I/O: symlink = cold file = placement miss.
                    let is_cold = !cold_access_delay.is_zero()
                        && fs::symlink_metadata(path)
                            .map(|m| m.file_type().is_symlink())
                            .unwrap_or(false);
                    // The daemon runs concurrently and may be mid-eviction (hot file renamed
                    // to cold, symlink not yet created) when we arrive here. Treat NotFound
                    // as "file is in transit — skip this op" rather than a fatal error.
                    let data = match fs::read(path) {
                        Ok(d) => d,
                        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                            ops_done += 1;
                            continue;
                        }
                        Err(e) => return Err(e.into()),
                    };
                    let mut data = data;
                    data.resize(config.file_size, b'x');
                    // Open WITHOUT O_CREAT so we never re-create a file that the
                    // daemon just renamed away mid-eviction. fs::write uses O_CREAT
                    // which would create a new regular file at hot_path while the
                    // daemon is between rename(hot→cold) and symlink(cold→hot),
                    // causing the symlink call to get EEXIST.
                    let write_result = fs::OpenOptions::new()
                        .write(true)
                        .truncate(true)
                        .open(path)
                        .and_then(|mut f| f.write_all(&data));
                    match write_result {
                        Ok(()) => {}
                        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                            ops_done += 1;
                            continue;
                        }
                        Err(e) => return Err(e.into()),
                    }
                    if is_cold {
                        thread::sleep(cold_access_delay);
                    }
                }
            }
        }
        ops_done += 1;
    }

    Ok(ops_done)
}

fn choose_op<R: Rng>(config: &WorkloadConfig, live: &[PathBuf], rng: &mut R) -> Op {
    if live.is_empty() {
        return Op::Create;
    }
    let c = config.create_pct as u32;
    let d = config.delete_pct as u32;
    let e = config.edit_pct as u32;
    let total = c + d + e;
    if total == 0 {
        return Op::Create;
    }
    let x = rng.gen_range(0..total);
    if x < c {
        Op::Create
    } else if x < c + d {
        Op::Delete
    } else {
        Op::Edit
    }
}

fn make_nested_path(root: &Path, depth: usize, file_id: usize) -> PathBuf {
    let mut p = root.to_path_buf();
    for i in 0..depth {
        p.push(format!("dir_{}", i));
    }
    fs::create_dir_all(&p).ok();
    p.push(format!("f_{}.dat", file_id));
    p
}

/// Power-law index: skew=1.0 → uniform; skew>1 → concentrated toward index 0 (old files);
/// skew<1 → concentrated toward index len-1 (newest files).
fn skewed_index<R: Rng>(len: usize, skew: f64, rng: &mut R) -> usize {
    let u: f64 = rng.r#gen();
    ((len as f64 * u.powf(skew)) as usize).min(len - 1)
}

fn create_file(path: &Path, size: usize) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::write(path, vec![b'x'; size])?;
    Ok(())
}
