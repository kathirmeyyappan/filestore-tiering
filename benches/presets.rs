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
        cold_access_delay_us: 0,
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
            name: "tiny_hot_hot_set",
            description: "Tiny hot tier (5K), strong skew, cold_access_penalty=10ms. Frequency-aware policies keep hot set in hot.",
            apply: |cfg| {
                cfg.hot_capacity = 5_000;
                cfg.skew = 5.0;
                cfg.create_pct = 5;
                cfg.delete_pct = 0;
                cfg.edit_pct = 95;
                cfg.cold_access_delay_us = 10_000;
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
            description: "Slow cold storage: skew=3.0, cold_delay=5000us",
            apply: |cfg| {
                cfg.skew = 3.0;
                cfg.cold_delay_us = 5_000;
            },
        },
        // ── Presets designed to expose policy differences via cold_access_delay_us ──
        //
        // Without cold_access_delay_us, all policies look similar because hot and cold
        // live on the same device (same I/O speed). cold_access_delay_us makes accessing
        // a cold file (hot path is a symlink) artificially expensive, directly penalizing
        // any policy that keeps the wrong files in hot.
        Preset {
            // LFU wins; basic_lru and ARC lose.
            // Almost no new files; edits are strongly concentrated on a small stable set
            // of old files (skew=5.0). LFU keeps those files in hot permanently via
            // frequency counts. LRU can displace them temporarily when any create pushes
            // a new file to MRU, causing an expensive cold-access penalty when the old
            // hot file is next edited. ARC also suffers from similar displacement.
            // Empirically verified: LFU ~25-30% faster than LRU/ARC on this preset.
            name: "lfu_favored",
            description: "Stable hot set (3% create, 97% edit, skew=5.0), tiny hot tier, cold_access_delay=15ms. LFU ~25% faster.",
            apply: |cfg| {
                cfg.hot_capacity = 4_000;          // ~8 files
                cfg.create_pct = 3;
                cfg.delete_pct = 0;
                cfg.edit_pct = 97;
                cfg.skew = 5.0;                    // top ~4% of files get ~40% of edits
                cfg.cold_access_delay_us = 15_000;
                cfg.warmup_sec = 5.0;
                cfg.measure_sec = 15.0;
            },
        },
        Preset {
            // LRU and ARC win; LFU loses badly (~3x slower).
            // High create rate floods `live` with new files. skew=0.3 concentrates edits
            // on HIGH indices (newest files). LFU keeps old high-frequency files hot and
            // evicts new files immediately (they have freq=1-2 vs old files' high counts).
            // Every edit on a just-evicted new file incurs the 20ms cold_access_delay.
            // LRU and ARC both keep recent files in hot (MRU / T1 respectively), so they
            // mostly serve new-file edits from hot.
            // Empirically verified: LRU/ARC ~3x faster than LFU on this preset.
            name: "recency_favored",
            description: "High create rate (40%), edits skewed to NEW files (skew=0.3), tiny hot tier, cold_access_delay=20ms. LFU ~3x slower.",
            apply: |cfg| {
                cfg.hot_capacity = 4_000;          // ~8 files
                cfg.create_pct = 40;
                cfg.delete_pct = 0;
                cfg.edit_pct = 60;
                cfg.skew = 0.3;                    // u^0.3 > u → concentrated on high indices (new files)
                cfg.cold_access_delay_us = 20_000;
                cfg.warmup_sec = 5.0;
                cfg.measure_sec = 15.0;
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
        "  {:<12} {:>10} {:>10} {:>10} {:>10} {:>10} {:>10}",
        "policy", "ops/s", "ops", "promos", "demos", "promo%", "demo%"
    );
    println!("  {}", "-".repeat(74));
    for r in results {
        println!(
            "  {:<12} {:>10.1} {:>10} {:>10} {:>10} {:>10.2} {:>10.2}",
            r.config.policy,
            r.throughput,
            r.measure_ops,
            r.promotions,
            r.demotions,
            r.promotions_pct,
            r.demotions_pct,
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
