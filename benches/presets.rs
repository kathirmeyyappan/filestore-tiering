//! Standardized benchmark presets.
//!
//! Run `cargo bench` to compare all policies across representative workloads.
//!
//! ## Fairness guarantee
//!
//! Within each preset, a single `WorkloadTrace` is generated once and then
//! replayed identically for every policy. All policies see the same sequence of
//! creates, deletes, edits, file sizes, and access indices — the only variable
//! is the policy's eviction decisions. This removes RNG noise from cross-policy
//! comparisons inside one run (though two separate `cargo bench` invocations will
//! produce different traces, so absolute numbers may vary).
//!
//! ## Primary metric: hit rate
//!
//! Hit rate = hot edits / total edits during the measurement window. An edit is
//! "hot" if the hot-path file is a regular file (content in hot tier) and "cold"
//! if it is a symlink (content evicted to a cold tier). Higher is better.
//!
//! ## Secondary metrics
//!
//! `→hot_KB` and `→cld_KB` are bytes written to each storage layer by the daemon
//! (promotions and demotions respectively). Together with promo/demo counts they
//! show the I/O cost of achieving the observed hit rate.

use std::time::Instant;

use filestore_tiering::bench::{BenchResult, WorkloadConfig, generate_trace, run_with_trace};

const POLICIES: &[&str] = &["basic_lru", "arc", "lfu", "lru_2q"];

fn base_config() -> WorkloadConfig {
    WorkloadConfig {
        policy: String::new(),
        warmup_ops: 1_000,
        measure_ops: 5_000,
        poll_interval_ops: 50,
        depth: 3,
        // ~17 files fit in hot at average file size (1152 B avg of [256, 2048]).
        // Live list grows to ~300+ files over the full run, creating real eviction pressure.
        hot_capacity: 20_000,
        min_file_size: 256,
        max_file_size: 2_048,
        create_pct: 10,
        delete_pct: 5,
        edit_pct: 85,
        skew: 1.0,
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
            // Baseline: uniform access with no exploitable pattern. With skew=1.0 all
            // files are equally likely edit targets, so no policy has an informational
            // advantage. All policies should converge to roughly the same hit rate
            // (≈ hot_capacity / (avg_file_size × live_count)). Any spread here is
            // implementation overhead, not algorithmic.
            name: "steady_state",
            description: "Uniform access, 10/5/85, skew=1.0. Policies should converge.",
            apply: |_cfg| {},
        },
        Preset {
            // Frequency-favored: near-zero file creation, edits concentrated on a tiny
            // stable subset of old files (skew=5.0). Hot tier holds only ~8 files (4K / 512 B avg).
            // LFU keeps the same 8 files in hot permanently — their access counts far
            // outweigh any new arrival. LRU can displace one when a rare create pushes
            // something to MRU, then must re-promote it on the next access. LRU-2Q's Am
            // queue also protects multi-access files. ARC's ghost lists help it partially
            // recover. Result: clear ordering lfu ≥ lru_2q > arc > basic_lru.
            name: "frequency_favored",
            description: "Stable hot set: 3/0/97, skew=5.0, hot=4K. LFU wins.",
            apply: |cfg| {
                cfg.hot_capacity = 4_000;
                cfg.min_file_size = 256;
                cfg.max_file_size = 768; // tight tier: avg ~512 B → ~8 files fit
                cfg.create_pct = 3;
                cfg.delete_pct = 0;
                cfg.edit_pct = 97;
                cfg.skew = 5.0;
                cfg.warmup_ops = 2_000;
                cfg.measure_ops = 10_000;
            },
        },
        Preset {
            // Recency-favored: high create rate floods the live list with new files.
            // skew=0.3 → u^0.3 concentrates edits on HIGH indices (newest files) because
            // u^0.3 > u for u in (0,1). The working set is always the most recently
            // created files — exactly what LRU and ARC are optimised for. LFU retains
            // old high-count files and evicts new arrivals (freq=1) immediately, so the
            // very next edit on a just-created file is a cold miss. LRU-2Q's A1in queue
            // holds new files briefly, so it beats LFU but trails LRU/ARC.
            // Expected: arc ≈ basic_lru > lru_2q >> lfu.
            name: "recency_favored",
            description: "New-file storm: 40/0/60, skew=0.3 (newest), hot=4K. LFU loses.",
            apply: |cfg| {
                cfg.hot_capacity = 4_000;
                cfg.min_file_size = 256;
                cfg.max_file_size = 768;
                cfg.create_pct = 40;
                cfg.delete_pct = 0;
                cfg.edit_pct = 60;
                cfg.skew = 0.3;
                cfg.warmup_ops = 2_000;
                cfg.measure_ops = 10_000;
            },
        },
        Preset {
            // High-churn: heavy create AND delete traffic rapidly cycles the working set.
            // No exploitable access skew. Tests how quickly each policy adapts when the
            // active file population changes. With balanced creates/deletes the live list
            // stabilises, but its composition changes constantly, stressing eviction logic.
            name: "high_churn",
            description: "Heavy turnover: 30/20/50, skew=1.0. Working set shifts constantly.",
            apply: |cfg| {
                cfg.create_pct = 30;
                cfg.delete_pct = 20;
                cfg.edit_pct = 50;
            },
        },
        Preset {
            // Moderate skew: a real-world mixed workload where most edits land on a
            // concentrated but not tiny subset of older files. Hot tier is larger (20K)
            // so more files fit, making the policy decision more nuanced. Tests whether
            // frequency-aware policies (LFU, LRU-2Q) maintain their advantage under a
            // milder signal compared to frequency_favored.
            name: "hot_set",
            description: "Skewed edits on older files (skew=3.0), hot=20K.",
            apply: |cfg| {
                cfg.skew = 3.0;
            },
        },
    ]
}

fn run_preset(preset: &Preset) -> Vec<BenchResult> {
    let mut cfg = base_config();
    (preset.apply)(&mut cfg);

    // Generate the trace once — all policies replay the exact same operations.
    let mut rng = rand::thread_rng();
    let trace = generate_trace(&cfg, &mut rng);

    let mut results = Vec::new();
    for &policy in POLICIES {
        cfg.policy = policy.to_string();
        let start = Instant::now();
        match run_with_trace(&cfg, &trace) {
            Ok(result) => {
                eprintln!("    {} ... {:.1}s", policy, start.elapsed().as_secs_f64());
                results.push(result);
            }
            Err(e) => eprintln!("    {} ... FAILED: {}", policy, e),
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
        "  {:<12} {:>7} {:>7} {:>7} {:>7} {:>8} {:>9} {:>8} {:>9}",
        "policy", "hit%", "creates", "deletes", "edits", "promos", "→hot_KB", "demos", "→cld_KB"
    );
    println!("  {}", "-".repeat(83));
    for r in results {
        let hot_kb = r.bytes_written_to_tier.first().copied().unwrap_or(0) as f64 / 1024.0;
        let cld_kb = r.bytes_written_to_tier.get(1).copied().unwrap_or(0) as f64 / 1024.0;
        println!(
            "  {:<12} {:>7.2} {:>7} {:>7} {:>7} {:>8} {:>9.1} {:>8} {:>9.1}",
            r.config.policy,
            r.hit_rate * 100.0,
            r.total_creates,
            r.total_deletes,
            r.total_edits,
            r.promotions,
            hot_kb,
            r.demotions,
            cld_kb,
        );
    }
}

fn main() {
    let all_presets = presets();
    let base = base_config();

    println!(
        "Benchmark presets: {} presets × {} policies",
        all_presets.len(),
        POLICIES.len()
    );
    println!(
        "  base: warmup={}  measure={}  poll={}ops  hot={}B  files={}–{}B",
        base.warmup_ops,
        base.measure_ops,
        base.poll_interval_ops,
        base.hot_capacity,
        base.min_file_size,
        base.max_file_size,
    );
    println!(
        "  Estimated total: ~{}s (wall clock per policy × {} policies × {} presets)",
        all_presets.len() * POLICIES.len() * 5, // rough estimate
        POLICIES.len(),
        all_presets.len(),
    );

    let overall_start = Instant::now();
    for preset in &all_presets {
        eprintln!("  [preset: {}]", preset.name);
        let results = run_preset(preset);
        print_table(preset.name, preset.description, &results);
    }

    println!();
    println!("Total elapsed: {:.1}s", overall_start.elapsed().as_secs_f64());
}
