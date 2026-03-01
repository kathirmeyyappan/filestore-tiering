//! Benchmark: synthetic create/delete/edit workload to evaluate policy speed and move counts.
//!
//! Run via the `tiering_bench` binary; results are CSV for scripts/bench_eval.sh.

mod workload;

pub use workload::{BenchResult, CSV_HEADER, WorkloadConfig, run};
