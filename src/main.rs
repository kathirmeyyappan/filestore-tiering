mod policies;
mod policy_engine;
mod policy_log;
mod tier_fs;
mod tier_state;
mod watcher;

use std::path::PathBuf;
use std::thread;
use std::time::Duration;

use anyhow::Result;
use clap::Parser;

/// Parse a capacity string into bytes for --hot-capacity and --cold-capacities.
/// Accepts: plain number; 1K/1M/1G/1T (decimal); 1Ki/1Mi/1Gi/1Ti (binary); or "unlimited"/"max".
fn parse_capacity(s: &str) -> Result<u64, String> {
    let s = s.trim();
    if s.eq_ignore_ascii_case("unlimited") || s.eq_ignore_ascii_case("max") {
        return Ok(u64::MAX);
    }
    let s = s.replace(',', "");
    let mut digits_end = 0;
    for c in s.chars() {
        if c.is_ascii_digit() {
            digits_end += 1;
        } else {
            break;
        }
    }
    let (num_str, unit) = s.split_at(digits_end);
    let num: u64 = num_str
        .parse()
        .map_err(|_| format!("invalid number: {:?}", num_str))?;
    let unit = unit.trim();
    if unit.is_empty() {
        return Ok(num);
    }
    let multiplier: u64 = match unit {
        "K" | "k" => 1_000,
        "M" | "m" => 1_000_000,
        "G" | "g" => 1_000_000_000,
        "T" | "t" => 1_000_000_000_000,
        "Ki" | "ki" => 1024,
        "Mi" | "mi" => 1024 * 1024,
        "Gi" | "gi" => 1024 * 1024 * 1024,
        "Ti" | "ti" => 1024 * 1024 * 1024 * 1024,
        _ => {
            return Err(format!(
                "unknown unit: {:?} (use K, M, G, T or Ki, Mi, Gi, Ti)",
                unit
            ));
        }
    };
    num.checked_mul(multiplier)
        .ok_or_else(|| format!("capacity overflow: {} {}", num, unit))
}

use crate::policy_engine::{PolicyEngine, TierState};
use crate::watcher::FsWatcher;

#[derive(Parser)]
#[command(about = "Storage tiering via access-aware local file migration")]
#[command(
    after_help = "Capacity format: plain bytes, or 1K/1M/1G/1T (decimal), 1Ki/1Mi/1Gi/1Ti (binary), or \"unlimited\". Cold tier order: first cold-storage path = warmest (index 0). Example: --hot-storage /hot -c /warm /cold --hot-capacity 1G --cold-capacities 500M 2G --policy dummy"
)]
struct Cli {
    /// Path to the hot (client-facing) directory to watch.
    #[arg(long, short = 'H', required = true)]
    hot_storage: PathBuf,

    /// Cold storage tier directories. At least one required. **Order matters:** index 0 is warmest (e.g. warm), higher indices are colder.
    #[arg(short, long, num_args = 1.., required = true)]
    cold_storage: Vec<PathBuf>,

    /// Hot tier capacity in bytes. Units: 1K/1M/1G/1T (×1000) or 1Ki/1Mi/1Gi/1Ti (×1024); or "unlimited".
    #[arg(long, default_value = "unlimited", value_parser = parse_capacity)]
    hot_capacity: u64,

    /// Cold tier capacities: one value per tier (same order as cold-storage), or one value for all. Omit = unlimited. Same units as --hot-capacity.
    #[arg(long, num_args = 1.., value_parser = parse_capacity)]
    cold_capacities: Option<Vec<u64>>,

    /// Policy to use (e.g. dummy).
    #[arg(long, default_value = "dummy")]
    policy: String,

    /// Polling interval in seconds.
    #[arg(short, long, default_value_t = 5)]
    interval: u64,
}

fn main() -> Result<()> {
    use log::LevelFilter;
    env_logger::Builder::new()
        .filter_level(LevelFilter::Info)
        .parse_default_env()
        .init();
    let cli = Cli::parse();

    ensure_dir_exists(&cli.hot_storage, "hot_storage")?;
    for (i, path) in cli.cold_storage.iter().enumerate() {
        ensure_dir_exists(path, &format!("cold_storage[{}]", i))?;
    }

    let cold_caps = resolve_cold_capacities(&cli.cold_storage, cli.cold_capacities.as_deref())?;
    let cold_caps_formatted: String = cold_caps
        .iter()
        .map(|b| format_capacity(*b))
        .collect::<Vec<_>>()
        .join(", ");
    let mut policy_engine = make_policy(
        &cli.policy,
        &cli.hot_storage,
        &cli.cold_storage,
        cli.hot_capacity,
        cold_caps,
    )?;

    let watch_dirs: Vec<PathBuf> = std::iter::once(cli.hot_storage.clone())
        .chain(cli.cold_storage.iter().cloned())
        .collect();
    let fs_watcher = FsWatcher::new(&watch_dirs)?;

    log::info!(
        "watching {:?}  cold_storage={:?}  hot_capacity={}  cold_capacities=[{}]  policy={}  interval={}s",
        cli.hot_storage,
        cli.cold_storage,
        format_capacity(cli.hot_capacity),
        cold_caps_formatted,
        cli.policy,
        cli.interval
    );

    // main loop: poll for events, ingest them, and reorganize storage per policy.
    loop {
        thread::sleep(Duration::from_secs(cli.interval));

        let events = fs_watcher.poll();
        policy_engine.ingest(&events);
        policy_engine
            .reorganize()
            .map_err(|e| anyhow::anyhow!("{}", e))?;
    }
}

fn ensure_dir_exists(path: &std::path::Path, name: &str) -> Result<()> {
    if !path.exists() {
        anyhow::bail!("{} path does not exist: {:?}", name, path);
    }
    if !path.is_dir() {
        anyhow::bail!("{} is not a directory: {:?}", name, path);
    }
    Ok(())
}

/// Expand cold capacities for TierState: None → all unlimited; one value → use for every tier; N values → one per tier (must match cold_storage length).
fn resolve_cold_capacities(
    cold_storage: &[PathBuf],
    cold_capacities: Option<&[u64]>,
) -> Result<Vec<u64>> {
    let n = cold_storage.len();
    match cold_capacities {
        None => Ok(vec![u64::MAX; n]),
        Some(v) if v.len() == 1 => Ok(vec![v[0]; n]),
        Some(v) if v.len() == n => Ok(v.to_vec()),
        Some(v) => anyhow::bail!(
            "cold-capacities: expected 1 (for all tiers) or {} values (one per cold tier), got {}",
            n,
            v.len()
        ),
    }
}

/// Format byte count for log output (e.g. 1073741824 → "1G", u64::MAX → "unlimited").
fn format_capacity(b: u64) -> String {
    if b == u64::MAX {
        "unlimited".to_string()
    } else if b >= 1_000_000_000_000 {
        format!("{}T", b / 1_000_000_000_000)
    } else if b >= 1_000_000_000 {
        format!("{}G", b / 1_000_000_000)
    } else if b >= 1_000_000 {
        format!("{}M", b / 1_000_000)
    } else if b >= 1_000 {
        format!("{}K", b / 1_000)
    } else {
        format!("{}", b)
    }
}

fn make_policy(
    name: &str,
    hot_storage: &std::path::Path,
    cold_storage: &[PathBuf],
    hot_capacity: u64,
    cold_capacities: Vec<u64>,
) -> Result<Box<dyn policy_engine::PolicyEngine>> {
    let to_err = |e: Box<dyn std::error::Error + Send + Sync>| anyhow::anyhow!("{}", e);
    let hot_root = std::fs::canonicalize(hot_storage).map_err(|e| to_err(e.into()))?;
    let cold_roots: Vec<PathBuf> = cold_storage
        .iter()
        .map(|p| std::fs::canonicalize(p).map_err(|e| to_err(e.into())))
        .collect::<Result<_, _>>()?;
    let mut tier_state =
        TierState::new(hot_root.clone(), cold_roots, hot_capacity, cold_capacities);
    tier_state.init_bytes().map_err(to_err)?;

    // New policy: add a mod in policies/mod.rs, then add a match arm here (key = --policy value).
    match name {
        "basic_lru" => {
            policies::basic_lru::BasicLruPolicy::validate_config(hot_storage, cold_storage)
                .map_err(to_err)?;
            Ok(Box::new(policies::basic_lru::BasicLruPolicy::new(
                tier_state,
            )))
        }
        "dummy" => {
            policies::dummy::DummyPolicy::validate_config(hot_storage, cold_storage)
                .map_err(to_err)?;
            Ok(Box::new(policies::dummy::DummyPolicy::new(tier_state)))
        }
        _ => Err(anyhow::anyhow!("unknown policy: {}", name)),
    }
}
