//! CLI for the tiering benchmark.
//!
//! Run:    cargo run --bin tiering_bench -- --policy basic_lru
//! Header: cargo run --bin tiering_bench -- --header

use anyhow::Result;
use clap::Parser;

use filestore_tiering::bench::{CSV_HEADER, WorkloadConfig, run};
use filestore_tiering::capacity::parse_capacity;

#[derive(Parser)]
#[command(about = "Tiering benchmark: hit rate, promotion/demotion counts, and bytes written per tier")]
struct Cli {
    #[arg(long, default_value = "basic_lru")]
    policy: String,

    /// Number of operations for the warmup phase (warms up policy state; excluded from metrics).
    #[arg(long, default_value_t = 1_000)]
    warmup_ops: u64,

    /// Number of operations for the measurement phase.
    #[arg(long, default_value_t = 5_000)]
    measure_ops: u64,

    /// Daemon wakes (ingest + reorganize) after every N workload operations.
    #[arg(long, default_value_t = 50)]
    poll_interval_ops: usize,

    #[arg(long, short = 'd', default_value_t = 3)]
    depth: usize,

    /// Hot tier capacity in bytes. With avg file size ~1152 B, "20K" ≈ 17 files in hot.
    #[arg(long, default_value = "20K", value_parser = parse_capacity)]
    hot_capacity: u64,

    /// Minimum file size in bytes (inclusive). Files are sized uniformly in [min, max].
    #[arg(long, default_value_t = 256)]
    min_file_size: usize,

    /// Maximum file size in bytes (inclusive).
    #[arg(long, default_value_t = 2_048)]
    max_file_size: usize,

    #[arg(long, default_value_t = 10)]
    create_pct: u8,

    #[arg(long, default_value_t = 5)]
    delete_pct: u8,

    #[arg(long, default_value_t = 85)]
    edit_pct: u8,

    /// Edit-target skew. 1.0 = uniform; >1 = concentrated on oldest files; <1 = newest.
    #[arg(long, default_value_t = 1.0)]
    skew: f64,

    /// Print CSV header and exit.
    #[arg(long)]
    header: bool,

    /// Output a single CSV row (for scripting). Default is human-readable.
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
        warmup_ops: cli.warmup_ops,
        measure_ops: cli.measure_ops,
        poll_interval_ops: cli.poll_interval_ops,
        depth: cli.depth,
        hot_capacity: cli.hot_capacity,
        min_file_size: cli.min_file_size,
        max_file_size: cli.max_file_size,
        create_pct: cli.create_pct,
        delete_pct: cli.delete_pct,
        edit_pct: cli.edit_pct,
        skew: cli.skew,
    };

    let result = run(config)?;
    if cli.csv {
        println!("{}", result.to_csv_row());
    } else {
        println!("{}", result.to_pretty_string());
    }
    Ok(())
}
