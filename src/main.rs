use std::path::PathBuf;
use std::thread;
use std::time::Duration;

use anyhow::Result;
use clap::Parser;

use filestore_tiering::capacity::parse_capacity;
use filestore_tiering::daemon::{
    ensure_dir_exists, format_capacity, make_policy, resolve_cold_capacities,
};
use filestore_tiering::watcher::FsWatcher;

#[derive(Parser)]
#[command(about = "Storage tiering via access-aware local file migration")]
#[command(
    after_help = "Capacity format: plain bytes, or 1K/1M/1G/1T (decimal), 1Ki/1Mi/1Gi/1Ti (binary), or \"unlimited\". Cold tier order: first cold-storage path = warmest (index 0). Example: --hot-storage /hot -c /warm /cold --hot-capacity 1G --cold-capacities 500M 2G --policy dummy"
)]
struct Cli {
    #[arg(long, short = 'H', required = true)]
    hot_storage: PathBuf,

    #[arg(short, long, num_args = 1.., required = true)]
    cold_storage: Vec<PathBuf>,

    #[arg(long, default_value = "unlimited", value_parser = parse_capacity)]
    hot_capacity: u64,

    #[arg(long, num_args = 1.., value_parser = parse_capacity)]
    cold_capacities: Option<Vec<u64>>,

    #[arg(long, default_value = "dummy")]
    policy: String,

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

    loop {
        thread::sleep(Duration::from_secs(cli.interval));

        let events = fs_watcher.poll();
        policy_engine.ingest(&events);
        policy_engine
            .reorganize()
            .map_err(|e| anyhow::anyhow!("{}", e))?;
    }
}
