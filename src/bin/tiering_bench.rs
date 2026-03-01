//! CLI for the tiering benchmark: time-based run with warmup, then measure throughput and move counts.
//!
//! Run: cargo run --bin tiering_bench -- --policy basic_lru --measure-sec 30
//! Or:  cargo run --bin tiering_bench -- --header  # print CSV header only

use anyhow::Result;
use clap::Parser;

use filestore_tiering::bench::{CSV_HEADER, WorkloadConfig, run};
use filestore_tiering::capacity::parse_capacity;

#[derive(Parser)]
#[command(about = "Time-based benchmark: warmup, then measure throughput and promotions/demotions")]
struct Cli {
    #[arg(long, default_value = "basic_lru")]
    policy: String,

    /// Warmup duration in seconds before measurement.
    #[arg(long, default_value_t = 5.0)]
    warmup_sec: f64,

    /// Measurement duration in seconds (throughput and move counts during this window).
    #[arg(long, default_value_t = 30.0)]
    measure_sec: f64,

    /// Daemon poll interval in seconds: ingest+reorganize this often (events accumulate between polls). Default 0.2 = 5 peeks/sec.
    #[arg(long, default_value_t = 0.2)]
    poll_interval_sec: f64,

    #[arg(long, short = 'd', default_value_t = 3)]
    depth: usize,

    /// Hot tier capacity. Default 20K keeps ~40 files in hot so creates quickly exceed it and cause turnover.
    #[arg(long, default_value = "20K", value_parser = parse_capacity)]
    hot_capacity: u64,

    #[arg(long, default_value_t = 500)]
    file_size: usize,

    #[arg(long, default_value_t = 50)]
    create_pct: u8,

    #[arg(long, default_value_t = 0)]
    delete_pct: u8,

    #[arg(long, default_value_t = 50)]
    edit_pct: u8,

    #[arg(long, default_value_t = 1)]
    batch_size: usize,

    /// Edit target skew: 1.0 = uniform random, higher = concentrated on a hot subset.
    /// E.g. 2.0 → ~64% of edits hit ~20% of files; 3.0 → ~80% hit ~20%.
    #[arg(long, default_value_t = 1.0)]
    skew: f64,

    /// Hot-tier per-op I/O delay in microseconds. Simulates hot storage latency. 0 = no delay.
    #[arg(long, default_value_t = 0)]
    hot_delay_us: u64,

    /// Cold-tier per-move I/O delay in microseconds. Applied per promote/demote to simulate
    /// slow cold storage. 0 = no delay.
    #[arg(long, default_value_t = 0)]
    cold_delay_us: u64,

    #[arg(long)]
    header: bool,

    /// Output one CSV line (for scripts). Default is a human-readable summary.
    #[arg(long)]
    csv: bool,
}

fn main() -> Result<()> {
    let cli = Cli::parse();

    if cli.header {
        println!("{}", CSV_HEADER);
        return Ok(());
    }

    let config = WorkloadConfig {
        policy: cli.policy,
        warmup_sec: cli.warmup_sec,
        measure_sec: cli.measure_sec,
        poll_interval_sec: cli.poll_interval_sec,
        depth: cli.depth,
        hot_capacity: cli.hot_capacity,
        file_size: cli.file_size,
        create_pct: cli.create_pct,
        delete_pct: cli.delete_pct,
        edit_pct: cli.edit_pct,
        batch_size: cli.batch_size,
        skew: cli.skew,
        hot_delay_us: cli.hot_delay_us,
        cold_delay_us: cli.cold_delay_us,
    };

    let result = run(config)?;
    if cli.csv {
        println!("{}", result.to_csv_row());
    } else {
        println!("{}", result.to_pretty_string());
    }
    Ok(())
}
