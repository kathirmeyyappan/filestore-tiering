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

use std::io::Write;
use std::time::Instant;

use filestore_tiering::bench::{
    BenchResult, CSV_HEADER, WorkloadConfig, WorkloadPhase, generate_trace, run_with_trace,
};

const POLICIES: &[&str] = &[
    "basic_lru",
    "arc",
    "lfu",
    "lru_2q",
    "lecar",
    "cacheus",
    "decision_tree",
];

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
        phases: vec![],
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
        Preset {
            // Scan resistance exploit for LRU-2Q. Heavy create rate (30%) floods the
            // live list with one-time files while edits concentrate on a tiny stable core
            // (skew=3.0). LRU-2Q's A1in FIFO (25% = ~2 files) absorbs the entire scan
            // without touching Am, which holds the ~6-file core untouched. ARC's adaptive
            // p can oscillate: occasional B1 ghost hits from scan files that happen to get
            // one re-edit push p upward (enlarging T1 — counterproductive since scan files
            // don't deserve hot space), then B2 ghost hits correct downward. This oscillation
            // costs ARC a few misses each cycle. Basic LRU gets catastrophically thrashed —
            // every new file enters as MRU, evicting a core file from the LRU tail; the next
            // edit on that core file is a guaranteed cold miss. LFU also handles this well
            // (frequency counts protect the core), but LRU-2Q's zero-learning-period fixed
            // split gives it a slight edge during warmup and transitions.
            // Expected: lru_2q ≈ lfu > arc >> basic_lru.
            name: "scan_flood",
            description: "Scan noise + stable core: 30/5/65, skew=3.0, hot=4K. LRU-2Q's A1in shines.",
            apply: |cfg| {
                cfg.hot_capacity = 4_000;
                cfg.min_file_size = 256;
                cfg.max_file_size = 768;
                cfg.create_pct = 30;
                cfg.delete_pct = 5;
                cfg.edit_pct = 65;
                cfg.skew = 3.0;
                cfg.warmup_ops = 2_000;
                cfg.measure_ops = 10_000;
            },
        },
        Preset {
            // Aggressive scan resistance for LRU-2Q. Same concept as scan_flood but more
            // extreme: higher create rate (35%) with very strong frequency skew (4.0) and
            // the tightest possible tier (2.5K ≈ 5 files). The massive create rate generates
            // a firehose of one-time files that never get re-accessed (skew=4.0 sends edits
            // to the oldest ~5 files exclusively). LRU-2Q's A1in (25% = ~625B ≈ 1 file)
            // absorbs one scan file at a time via FIFO while Am's 4 protected slots hold
            // the entire hot core. Every other policy struggles:
            //
            // Basic LRU: every new file pushes a core file off the LRU tail — catastrophic.
            // ARC: p starts at 0 and needs time to learn; the massive create rate floods T1
            // with entries that never generate B1 ghost hits (never re-accessed), so p stays
            // low. LFU: handles the core well (high frequency counts) but may misplace files
            // during initial convergence. LRU-2Q wins from the very first poll.
            // Expected: lru_2q > lfu >> arc > basic_lru.
            name: "scan_heavy",
            description: "Extreme scan: 35/5/60, skew=4.0, hot=2.5K. A1in vs the firehose.",
            apply: |cfg| {
                cfg.hot_capacity = 2_500;
                cfg.min_file_size = 256;
                cfg.max_file_size = 768;
                cfg.create_pct = 35;
                cfg.delete_pct = 5;
                cfg.edit_pct = 60;
                cfg.skew = 4.0;
                cfg.warmup_ops = 2_000;
                cfg.measure_ops = 10_000;
            },
        },
        Preset {
            // Phase-shift: the measurement window cycles between frequency-dominated and
            // recency-dominated access patterns. The create rate stays constant (10%) so
            // the live list grows at a steady rate — only the SKEW changes, shifting which
            // files get edited.
            //
            // Phase A (skew=4.0): edits concentrated on oldest ~5% of files — a stable core.
            // Phase B (skew=0.3): edits concentrated on newest ~50% of files — recency rules.
            //
            // Four alternating phases (A→B→A→B, 25% each) give ARC two full adaptation cycles.
            // During phase A, B2 ghost hits (evicted T2 files being re-accessed) push p down,
            // protecting the frequency core in T2. During phase B, B1 ghost hits (evicted T1
            // files = recently created files being re-accessed) push p up, enlarging T1.
            //
            // LRU-2Q: fixed 25/75 split can't adapt. LFU: stale frequency counts from
            // phase A block phase B's newest files from entering hot. Basic LRU: no frequency
            // memory, loses the phase A core immediately when phase B starts.
            // Expected: arc > lru_2q ≈ basic_lru > lfu.
            name: "phase_shift",
            description: "Cycling freq↔recency (skew 4.0↔0.3). ARC adapts p each transition.",
            apply: |cfg| {
                cfg.hot_capacity = 20_000;
                cfg.min_file_size = 256;
                cfg.max_file_size = 768; // avg ~512 B → ~40 files fit, p granularity ~2.5%
                cfg.warmup_ops = 3_000;
                cfg.measure_ops = 10_000;
                cfg.phases = vec![
                    WorkloadPhase {
                        fraction: 0.25,
                        skew: 4.0,
                        create_pct: 10,
                        delete_pct: 0,
                        edit_pct: 90,
                    },
                    WorkloadPhase {
                        fraction: 0.25,
                        skew: 0.3,
                        create_pct: 10,
                        delete_pct: 0,
                        edit_pct: 90,
                    },
                    WorkloadPhase {
                        fraction: 0.25,
                        skew: 4.0,
                        create_pct: 10,
                        delete_pct: 0,
                        edit_pct: 90,
                    },
                    WorkloadPhase {
                        fraction: 0.25,
                        skew: 0.3,
                        create_pct: 10,
                        delete_pct: 0,
                        edit_pct: 90,
                    },
                ];
            },
        },
    ]
}

fn run_preset(preset: &Preset, policies: &[&str], quick: bool) -> Vec<BenchResult> {
    let mut cfg = base_config();
    (preset.apply)(&mut cfg);

    if quick {
        cfg.warmup_ops = (cfg.warmup_ops / 5).max(100);
        cfg.measure_ops = (cfg.measure_ops / 5).max(200);
    }

    // Generate the trace once — all policies replay the exact same operations.
    let mut rng = rand::thread_rng();
    let trace = generate_trace(&cfg, &mut rng);

    let mut results = Vec::new();
    for &policy in policies {
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
        "  {:<12} {:>7} {:>7} {:>7} {:>7} {:>8} {:>9} {:>8} {:>9} {:>10} {:>10}",
        "policy",
        "hit%",
        "creates",
        "deletes",
        "edits",
        "promos",
        "→hot_KB",
        "demos",
        "→cld_KB",
        "ingest_ms",
        "reorg_ms"
    );
    println!("  {}", "-".repeat(105));
    for r in results {
        let hot_kb = r.bytes_written_to_tier.first().copied().unwrap_or(0) as f64 / 1024.0;
        let cld_kb = r.bytes_written_to_tier.get(1).copied().unwrap_or(0) as f64 / 1024.0;
        let ingest_ms = r.ingest_us as f64 / 1000.0;
        let reorg_ms = r.reorganize_us as f64 / 1000.0;
        println!(
            "  {:<12} {:>7.2} {:>7} {:>7} {:>7} {:>8} {:>9.1} {:>8} {:>9.1} {:>10.1} {:>10.1}",
            r.config.policy,
            r.hit_rate * 100.0,
            r.total_creates,
            r.total_deletes,
            r.total_edits,
            r.promotions,
            hot_kb,
            r.demotions,
            cld_kb,
            ingest_ms,
            reorg_ms,
        );
    }
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let quick = args.iter().any(|a| a == "--quick" || a == "-q");
    let mut preset_filters: Vec<String> = Vec::new();
    let mut policy_filters: Vec<String> = Vec::new();
    let mut csv_path: Option<String> = None;
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--quick" | "-q" => {}
            "--policy" | "-p" => {
                i += 1;
                if i < args.len() {
                    policy_filters.push(args[i].clone());
                }
            }
            "--csv" => {
                i += 1;
                if i < args.len() {
                    csv_path = Some(args[i].clone());
                } else {
                    csv_path = Some("bench_results.csv".to_string());
                }
            }
            other if !other.starts_with('-') => preset_filters.push(other.to_string()),
            "--bench" => {} // injected by `cargo bench`; ignore
            _ => eprintln!("Unknown flag: {}", args[i]),
        }
        i += 1;
    }

    let all_presets = presets();
    let selected_presets: Vec<&Preset> = if preset_filters.is_empty() {
        all_presets.iter().collect()
    } else {
        all_presets
            .iter()
            .filter(|p| preset_filters.iter().any(|f| p.name.contains(f.as_str())))
            .collect()
    };
    let policies: Vec<&str> = if policy_filters.is_empty() {
        POLICIES.to_vec()
    } else {
        POLICIES
            .iter()
            .copied()
            .filter(|p| policy_filters.iter().any(|f| p.contains(f.as_str())))
            .collect()
    };

    if selected_presets.is_empty() {
        eprintln!("No presets matched filters: {:?}", preset_filters);
        return;
    }

    let base = base_config();
    println!(
        "Benchmark: {} presets × {} policies{}",
        selected_presets.len(),
        policies.len(),
        if quick { " (QUICK mode: ops/5)" } else { "" },
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

    let overall_start = Instant::now();
    let mut all_results: Vec<(&str, Vec<BenchResult>)> = Vec::new();
    for preset in &selected_presets {
        eprintln!("  [preset: {}]", preset.name);
        let results = run_preset(preset, &policies, quick);
        print_table(preset.name, preset.description, &results);
        all_results.push((preset.name, results));
    }

    println!();
    println!(
        "Total elapsed: {:.1}s",
        overall_start.elapsed().as_secs_f64()
    );

    if let Some(path) = &csv_path {
        let mut f = std::fs::File::create(path).expect("failed to create CSV file");
        writeln!(f, "preset,{}", CSV_HEADER).expect("failed to write CSV header");
        for (preset_name, results) in &all_results {
            for r in results {
                writeln!(f, "{},{}", preset_name, r.to_csv_row()).expect("failed to write CSV row");
            }
        }
        println!("CSV saved to {}", path);
    }

    if selected_presets.is_empty() || policies.is_empty() {
        eprintln!("\nUsage: cargo bench -- [preset_name...] [-p policy] [-q|--quick]");
        eprintln!(
            "  preset names: {:?}",
            all_presets.iter().map(|p| p.name).collect::<Vec<_>>()
        );
        eprintln!("  policies: {:?}", POLICIES);
    }
}
