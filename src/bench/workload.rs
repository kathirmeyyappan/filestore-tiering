//! Workload execution: create/delete/edit mix, ingest+reorganize. Time-based with warmup + measurement.

use std::fs;
use std::path::Path;
use std::time::{Duration, Instant, SystemTime};

use anyhow::Result;
use rand::Rng;

use crate::daemon::make_policy;
use crate::policy_engine::{AccessEvent, FsEventKind, PolicyEngine};

/// Parameters for one benchmark run (matches CLI of tiering_bench).
#[derive(Debug, Clone)]
pub struct WorkloadConfig {
    pub policy: String,
    pub warmup_sec: f64,
    pub measure_sec: f64,
    /// How often the "daemon" polls: ingest + reorganize every this many seconds. Events accumulate between polls.
    pub poll_interval_sec: f64,
    pub depth: usize,
    pub hot_capacity: u64,
    pub file_size: usize,
    pub create_pct: u8,
    pub delete_pct: u8,
    pub edit_pct: u8,
    #[allow(dead_code)] // kept for CSV/display compatibility
    pub batch_size: usize,
    /// Edit target skew: 1.0 = uniform, higher = more concentrated on a hot subset.
    /// Index chosen as floor(len * u^skew) where u ~ Uniform[0,1).
    pub skew: f64,
    /// Artificial per-op I/O delay in microseconds (simulates hot-tier latency). 0 = no delay.
    pub hot_delay_us: u64,
    /// Artificial per-access I/O delay in microseconds applied when an edit targets a cold file
    /// (hot path is a symlink). Simulates the latency of reading/writing cold storage in-request,
    /// directly penalizing policies that keep the wrong files in hot. This is the primary knob
    /// for exposing policy differences: a policy with poor placement pays this on every cold edit.
    pub cold_access_delay_us: u64,
}

/// Result of a workload run: throughput and move counts during the measurement window.
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

/// CSV header line for benchmark output (use with --header).
pub const CSV_HEADER: &str = "policy,warmup_sec,measure_sec,poll_interval_sec,depth,hot_cap,file_size,create_pct,delete_pct,edit_pct,batch_size,skew,hot_delay_us,cold_access_delay_us,measure_ops,throughput,promotions,demotions,demotions_tier0,promotions_pct,demotions_pct";

impl BenchResult {
    /// Single CSV row for this result.
    pub fn to_csv_row(&self) -> String {
        format!(
            "{},{},{},{},{},{},{},{},{},{},{},{:.1},{},{},{},{:.1},{},{},{},{:.4},{:.4}",
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
            self.config.hot_delay_us,
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

    /// Human-readable summary (default output when not using --csv).
    pub fn to_pretty_string(&self) -> String {
        use crate::capacity::format_capacity;
        format!(
            r#"  policy            {}
  warmup            {:.1} s
  measure           {:.1} s
  poll_interval     {:.2} s  (daemon peek rate)
  depth             {}
  hot_capacity      {}
  file_size         {} B  (created and edited file size)
  create/delete/edit  {}% / {}% / {}%
  skew              {:.1}  (1.0 = uniform, higher = hot subset)
  hot_delay         {} µs  (per-op hot-tier I/O delay)
  cold_access_delay {} µs  (per-edit cold-file access penalty)
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
            self.config.hot_delay_us,
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

/// Run the synthetic workload: warmup for warmup_sec, then measure for measure_sec. Reports throughput and move counts during the measurement window.
pub fn run(config: WorkloadConfig) -> Result<BenchResult> {
    let dir = tempfile::tempdir()?;
    let hot_path = dir.path().join("hot");
    let cold_path = dir.path().join("cold");
    fs::create_dir_all(&hot_path)?;
    fs::create_dir_all(&cold_path)?;

    let cold_caps = vec![u64::MAX];
    let mut policy = make_policy(
        &config.policy,
        &hot_path,
        std::slice::from_ref(&cold_path),
        config.hot_capacity,
        cold_caps,
    )?;

    let mut rng = rand::thread_rng();
    let mut live: Vec<std::path::PathBuf> = Vec::new();
    let mut file_counter: usize = 0;
    let now = SystemTime::now();

    let warmup_duration = Duration::from_secs_f64(config.warmup_sec);
    let measure_duration = Duration::from_secs_f64(config.measure_sec);

    // Warmup: run workload for warmup_sec (daemon peeks every poll_interval_sec)
    run_phase(
        &config,
        &hot_path,
        &mut *policy,
        &mut PhaseState {
            rng: &mut rng,
            live: &mut live,
            file_counter: &mut file_counter,
        },
        now,
        warmup_duration,
    )?;

    let stats_after_warmup = policy.stats();

    // Measurement: run for measure_sec, count ops
    let measure_ops = run_phase(
        &config,
        &hot_path,
        &mut *policy,
        &mut PhaseState {
            rng: &mut rng,
            live: &mut live,
            file_counter: &mut file_counter,
        },
        now,
        measure_duration,
    )?;
    let stats_after_measure = policy.stats();

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

/// Mutable state for one `run_phase` invocation (avoids too many arguments).
struct PhaseState<'a, R: Rng> {
    rng: &'a mut R,
    live: &'a mut Vec<std::path::PathBuf>,
    file_counter: &'a mut usize,
}

/// Run workload for up to `duration`. Returns number of ops completed.
/// Simulates the daemon: events accumulate; every poll_interval_sec we call ingest+reorganize (daemon "peek").
fn run_phase<R: Rng>(
    config: &WorkloadConfig,
    hot_path: &Path,
    policy: &mut dyn PolicyEngine,
    phase: &mut PhaseState<'_, R>,
    now: SystemTime,
    duration: Duration,
) -> Result<u64, anyhow::Error> {
    let start = Instant::now();
    let poll_interval = Duration::from_secs_f64(config.poll_interval_sec);
    let hot_delay = Duration::from_micros(config.hot_delay_us);
    let cold_access_delay = Duration::from_micros(config.cold_access_delay_us);
    let mut last_poll = Instant::now();
    let mut events: Vec<AccessEvent> = Vec::new();
    let mut ops_done: u64 = 0;

    while start.elapsed() < duration {
        let op = choose_op(config, phase.live, phase.rng);
        match op {
            Op::Create => {
                let path = make_nested_path(hot_path, config.depth, *phase.file_counter);
                *phase.file_counter += 1;
                create_file(&path, config.file_size)?;
                phase.live.push(path.clone());
                events.push(AccessEvent {
                    path: path.clone(),
                    kind: FsEventKind::Create,
                    timestamp: now,
                });
                events.push(AccessEvent {
                    path,
                    kind: FsEventKind::Modify,
                    timestamp: now,
                });
            }
            Op::Delete => {
                let i = phase.rng.gen_range(0..phase.live.len());
                let path = phase.live.remove(i);
                if path.exists() {
                    fs::remove_file(&path).ok();
                }
                events.push(AccessEvent {
                    path: path.clone(),
                    kind: FsEventKind::Remove,
                    timestamp: now,
                });
            }
            Op::Edit => {
                let i = skewed_index(phase.live.len(), config.skew, phase.rng);
                let path = &phase.live[i];
                if path.exists() {
                    // Check before I/O: is this file currently cold (symlink to cold tier)?
                    let is_cold = !cold_access_delay.is_zero()
                        && fs::symlink_metadata(path)
                            .map(|m| m.file_type().is_symlink())
                            .unwrap_or(false);
                    let mut data = fs::read(path)?;
                    data.resize(config.file_size, b'x');
                    fs::write(path, &data)?;
                    events.push(AccessEvent {
                        path: path.clone(),
                        kind: FsEventKind::Modify,
                        timestamp: now,
                    });
                    // Penalize cold file access: policy paid the price of missing the hot set.
                    if is_cold {
                        std::thread::sleep(cold_access_delay);
                    }
                }
            }
        }

        if !hot_delay.is_zero() {
            std::thread::sleep(hot_delay);
        }

        ops_done += 1;

        // Daemon peek: ingest + reorganize every poll_interval (events accumulate between polls)
        if last_poll.elapsed() >= poll_interval {
            policy.ingest(&events);
            policy.reorganize().map_err(|e| anyhow::anyhow!("{}", e))?;
            events.clear();
            last_poll = Instant::now();
        }
    }

    if !events.is_empty() {
        policy.ingest(&events);
        policy.reorganize().map_err(|e| anyhow::anyhow!("{}", e))?;
    }

    Ok(ops_done)
}

enum Op {
    Create,
    Delete,
    Edit,
}

fn choose_op<R: Rng>(config: &WorkloadConfig, live: &[std::path::PathBuf], rng: &mut R) -> Op {
    let c = config.create_pct as u32;
    let d = config.delete_pct as u32;
    let e = config.edit_pct as u32;
    let total = c + d + e;
    if total == 0 {
        return Op::Create;
    }
    let x = rng.gen_range(0..total);
    if live.is_empty() {
        return Op::Create;
    }
    if x < c {
        Op::Create
    } else if x < c + d {
        Op::Delete
    } else {
        Op::Edit
    }
}

fn make_nested_path(root: &Path, depth: usize, file_id: usize) -> std::path::PathBuf {
    let mut p = root.to_path_buf();
    for i in 0..depth {
        p.push(format!("dir_{}", i));
    }
    fs::create_dir_all(&p).ok();
    p.push(format!("f_{}.dat", file_id));
    p
}

/// Pick an index in [0, len) with power-law skew.
/// skew=1.0 → uniform; skew=2.0 → quadratic concentration toward index 0; etc.
fn skewed_index<R: Rng>(len: usize, skew: f64, rng: &mut R) -> usize {
    let u: f64 = rng.r#gen();
    let i = (len as f64 * u.powf(skew)) as usize;
    i.min(len - 1)
}

fn create_file(path: &Path, size: usize) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let data = vec![b'x'; size];
    fs::write(path, data)?;
    Ok(())
}
