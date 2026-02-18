mod policy_engine;
mod policies;
mod watcher;

use std::path::PathBuf;
use std::thread;
use std::time::Duration;

use anyhow::Result;
use clap::Parser;

use crate::policy_engine::PolicyEngine;
use crate::watcher::FsWatcher;

#[derive(Parser)]
#[command(about = "Storage tiering via access-aware local file migration")]
struct Cli {
    /// Path to the hot (client-facing) directory to watch.
    #[arg(long, short = 'H', required = true)]
    hot_storage: PathBuf,

    /// Cold storage tier directories. At least one required. Order matters: earlier paths in list will be treated as warmer by policy engines.
    #[arg(short, long, num_args = 1.., required = true)]
    cold_storage: Vec<PathBuf>,

    /// Policy to use (e.g. dummy).
    #[arg(long, default_value = "dummy")]
    policy: String,

    /// Polling interval in seconds.
    #[arg(short, long, default_value_t = 5)]
    interval: u64,
}

fn main() -> Result<()> {
    env_logger::init();
    let cli = Cli::parse();

    ensure_dir_exists(&cli.hot_storage, "hot_storage")?;
    for (i, path) in cli.cold_storage.iter().enumerate() {
        ensure_dir_exists(path, &format!("cold_storage[{}]", i))?;
    }

    let mut policy_engine = make_policy(&cli.policy, &cli.hot_storage, &cli.cold_storage)?;

    let fs_watcher = FsWatcher::new(&cli.hot_storage)?;

    log::info!(
        "watching {:?}  cold_storage={:?}  policy={}  interval={}s",
        cli.hot_storage,
        cli.cold_storage,
        cli.policy,
        cli.interval
    );

    // main loop: poll for events, ingest them, and reorganize storage based on access patterns according to policy.
    loop {
        thread::sleep(Duration::from_secs(cli.interval));

        let events = fs_watcher.poll();
        if !events.is_empty() {
            // log::info!("observed {} access events", events.len());
        }
        policy_engine.ingest(&events);
        policy_engine.reorganize().map_err(|e| anyhow::anyhow!("{}", e))?;
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

fn make_policy(
    name: &str,
    hot_storage: &std::path::Path,
    cold_storage: &[PathBuf],
) -> Result<Box<dyn policy_engine::PolicyEngine>> {
    let to_err = |e: Box<dyn std::error::Error + Send + Sync>| anyhow::anyhow!("{}", e);
    match name {
        "dummy" => {
            policies::dummy::DummyPolicy::validate_config(hot_storage, cold_storage).map_err(to_err)?;
            Ok(Box::new(policies::dummy::DummyPolicy::new(
                hot_storage.to_path_buf(),
                cold_storage.to_vec(),
            )))
        }
        // more policies here...
        _ => Err(anyhow::anyhow!("unknown policy: {}", name)),
    }
}
