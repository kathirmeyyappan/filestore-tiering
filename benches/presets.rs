//! Standardized benchmark presets: run `cargo bench` to compare policies across representative workloads.

use std::time::Instant;

use filestore_tiering::bench::{BenchResult, WorkloadConfig, run};

const POLICIES: &[&str] = &["basic_lru", "arc", "lfu"];

fn base_config() -> WorkloadConfig {
    WorkloadConfig {
        policy: String::new(),
        warmup_sec: 3.0,
        measure_sec: 10.0,
        poll_interval_sec: 0.2,
        depth: 3,
        hot_capacity: 20_000,
        file_size: 500,
        create_pct: 10,
        delete_pct: 5,
        edit_pct: 85,
        batch_size: 1,
        skew: 1.0,
        hot_delay_us: 0,
        cold_delay_us: 0,
    }
}

struct Preset {
    name: &'static str,
    description: &'static str,
    apply: fn(&mut WorkloadConfig),
}

fn presets() -> Vec<Preset> {
    vec![
        Preset {
            name: "steady_state",
            description: "Baseline uniform access: 10/5/85 create/delete/edit, skew=1.0",
            apply: |_cfg| {},
        },
        Preset {
            name: "hot_set",
            description: "Concentrated edits on hot subset: skew=3.0",
            apply: |cfg| {
                cfg.skew = 3.0;
            },
        },
        Preset {
            name: "high_churn",
            description: "Heavy file creation/deletion: 40/10/50, skew=1.0",
            apply: |cfg| {
                cfg.create_pct = 40;
                cfg.delete_pct = 10;
                cfg.edit_pct = 50;
            },
        },
        Preset {
            name: "slow_cold",
            description: "Slow cold storage: skew=3.0, cold_delay=500us",
            apply: |cfg| {
                cfg.skew = 3.0;
                cfg.cold_delay_us = 500;
            },
        },
    ]
}

fn run_preset(preset: &Preset) -> Vec<BenchResult> {
    let mut results = Vec::new();
    for &policy in POLICIES {
        let mut cfg = base_config();
        (preset.apply)(&mut cfg);
        cfg.policy = policy.to_string();

        let start = Instant::now();
        match run(cfg) {
            Ok(result) => {
                eprintln!("    {} ... {:.1}s", policy, start.elapsed().as_secs_f64());
                results.push(result);
            }
            Err(e) => {
                eprintln!("    {} ... FAILED: {}", policy, e);
            }
        }
    }
    results
}

fn print_table(preset_name: &str, description: &str, results: &[BenchResult]) {
    println!();
    println!("=== {} ===", preset_name);
    println!("    {}", description);
    println!();
    println!(
        "  {:<12} {:>10} {:>10} {:>10} {:>10} {:>10} {:>10} {:>10}",
        "policy", "ops/s", "ops", "promos", "demos", "promo%", "demo%", "hit%"
    );
    println!("  {}", "-".repeat(84));
    for r in results {
        println!(
            "  {:<12} {:>10.1} {:>10} {:>10} {:>10} {:>10.2} {:>10.2} {:>10.2}",
            r.config.policy,
            r.throughput,
            r.measure_ops,
            r.promotions,
            r.demotions,
            r.promotions_pct,
            r.demotions_pct,
            r.hit_rate * 100.0,
        );
    }
}

fn main() {
    let all_presets = presets();
    let base = base_config();

    println!(
        "Benchmark presets: {} presets x {} policies",
        all_presets.len(),
        POLICIES.len()
    );
    println!(
        "  warmup={:.0}s, measure={:.0}s per run",
        base.warmup_sec, base.measure_sec
    );
    println!(
        "  Total estimated time: ~{:.0}s",
        all_presets.len() as f64 * POLICIES.len() as f64 * (base.warmup_sec + base.measure_sec)
    );

    let overall_start = Instant::now();

    for preset in &all_presets {
        eprintln!("  [preset: {}]", preset.name);
        let results = run_preset(preset);
        print_table(preset.name, preset.description, &results);
    }

    println!();
    println!(
        "Total elapsed: {:.1}s",
        overall_start.elapsed().as_secs_f64()
    );
}
