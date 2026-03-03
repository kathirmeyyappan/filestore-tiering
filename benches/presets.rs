//! Standardized benchmark presets. Run with `cargo bench`.
//!
//! ## How to read results
//!
//! - **ops/s**: workload throughput. The only thing that slows the workload is
//!   `cold_access_delay_us` (a policy placement miss). Policy compute time runs
//!   asynchronously in the daemon thread and does NOT show up here.
//! - **promo% / demo%**: percentage of ops that caused a tier move. High values mean
//!   the policy is thrashing — churning files between tiers unnecessarily.
//!
//! ## Preset design philosophy
//!
//! Presets without `cold_access_delay_us` show raw throughput and move counts (useful
//! for understanding policy behaviour). Presets with `cold_access_delay_us` reveal
//! **placement quality**: a policy that keeps the wrong files in hot pays the penalty on
//! every cold edit, directly suppressing ops/s. These are the most diagnostic presets.
//!
//! ## skew parameter
//!
//! `skew` controls which files are edited: index = floor(len * u^skew), u ~ Uniform[0,1).
//!   - skew = 1.0  → uniform (all files equally likely)
//!   - skew > 1.0  → concentrated on LOW indices (oldest files, high historical frequency)
//!   - skew < 1.0  → concentrated on HIGH indices (newest files, most recently created)

use std::time::Instant;

use filestore_tiering::bench::{BenchResult, WorkloadConfig, run};

const POLICIES: &[&str] = &["basic_lru", "arc", "lfu", "lru_2q"];

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
        cold_access_delay_us: 5_000,
    }
}

struct Preset {
    name: &'static str,
    description: &'static str,
    apply: fn(&mut WorkloadConfig),
}

fn presets() -> Vec<Preset> {
    vec![
        // ── General workload characterisation ─────────────────────────────────
        //
        // These presets use a small baseline cold_access_delay (inherited from
        // base_config: 5ms) so placement differences are visible, but they are
        // primarily about characterising workload shape and move counts.

        Preset {
            // All policies should perform similarly. A large hot tier (20K ≈ 40 files)
            // with moderate uniform churn means most files fit in hot; cold misses are
            // rare for any reasonable policy.  Use this as a sanity-check baseline.
            name: "steady_state",
            description: "Baseline: 10/5/85 create/delete/edit, uniform skew, 20K hot tier.",
            apply: |_cfg| {},
        },
        Preset {
            // skew=3.0 concentrates ~80% of edits on the oldest ~20% of files.
            // Frequency-aware policies (LFU, ARC-T2) should keep that hot subset in hot
            // and show fewer cold misses than basic_lru, which can let new-file creates
            // temporarily displace the hot subset to LRU.
            name: "hot_set",
            description: "Concentrated edits on oldest files (skew=3.0). Frequency-aware policies shine.",
            apply: |cfg| {
                cfg.skew = 3.0;
            },
        },
        Preset {
            // Aggressive file turnover: 40% creates, 10% deletes. The working set
            // shifts constantly. No policy has a clear advantage; this tests stability
            // under churn (no single policy should thrash dramatically more than others).
            name: "high_churn",
            description: "Heavy create/delete (40/10/50), uniform. Tests stability under working-set churn.",
            apply: |cfg| {
                cfg.create_pct = 40;
                cfg.delete_pct = 10;
                cfg.edit_pct = 50;
            },
        },

        // ── Placement-quality presets (cold_access_delay_us > 0) ──────────────
        //
        // Each preset is designed so that one or more policies pay significantly more
        // cold_access_delay penalties than others, directly suppressing their ops/s.
        // The penalty is intentional: it models the real cost of fetching data from
        // cold storage during a live request.

        Preset {
            // ── LFU / LRU-2Q win; basic_lru and ARC lose ──────────────────────
            //
            // Hot tier holds ~10 files (5K / 500B). Edits with skew=5 mean the top ~4%
            // of files (by age = index 0..N*0.04) receive ~40% of all edits. These
            // are the files that MUST stay in hot to avoid the 10ms penalty.
            //
            // LFU: accumulates frequency counts; the hot-set files rise to the top and
            //   stay there permanently. Cold misses are rare.
            // LRU-2Q: hot-set files pass through A1in → A1out → Am (protected queue).
            //   Am is stable; new-file creates go to A1in (25% buffer) and do not
            //   displace the hot set from Am.
            // basic_lru: every create pushes a new file to MRU, potentially evicting
            //   a hot-set file to the LRU tail. If enough creates arrive before the
            //   next edit on a hot-set file, that file has been evicted → cold miss.
            // ARC: balances T1 (recency) and T2 (frequency) via adaptive p. With tiny
            //   hot tier and few creates, T2 should dominate, but ARC still sacrifices
            //   some capacity to T1 recency tracking, giving it slightly less protection
            //   for the hot set than pure LFU.
            //
            // Empirically verified: LFU / LRU-2Q ~25-30% faster than basic_lru / ARC.
            name: "frequency_favored",
            description: "Stable hot set (3/0/97, skew=5.0, hot=5K, penalty=10ms). LFU and LRU-2Q keep hot files; basic_lru thrashes.",
            apply: |cfg| {
                cfg.hot_capacity = 5_000;           // ~10 files
                cfg.create_pct = 3;
                cfg.delete_pct = 0;
                cfg.edit_pct = 97;
                cfg.skew = 5.0;
                cfg.cold_access_delay_us = 10_000;
                cfg.warmup_sec = 5.0;
                cfg.measure_sec = 15.0;
            },
        },
        Preset {
            // ── LRU / ARC / LRU-2Q win; LFU loses badly (~3x slower) ──────────
            //
            // Hot tier holds ~8 files (4K / 500B). 40% creates flood `live` with new
            // files. skew=0.3 concentrates edits on HIGH indices (newest files):
            //   u^0.3 > u for u in (0,1), so the sampled index skews toward len-1.
            //
            // LFU: keeps the OLDEST (highest-frequency) files in hot, evicting new files
            //   immediately (freq=1 at creation). Every edit on a just-evicted new file
            //   pays the 20ms cold penalty. LFU's placement is backwards for this workload.
            // basic_lru / LRU-2Q: new files are MRU (basic_lru) or go to A1in (LRU-2Q);
            //   either way they remain in hot for their initial round of edits. Far fewer
            //   cold misses.
            // ARC: T1 (recency) keeps new files warm; ghost-list adaptation reinforces
            //   this over time. ARC performs similarly to basic_lru here.
            //
            // Empirically verified: basic_lru / ARC / LRU-2Q ~3x faster than LFU.
            name: "recency_favored",
            description: "New-file edit storm (40/0/60, skew=0.3, hot=4K, penalty=20ms). LFU evicts every new file instantly.",
            apply: |cfg| {
                cfg.hot_capacity = 4_000;           // ~8 files
                cfg.create_pct = 40;
                cfg.delete_pct = 0;
                cfg.edit_pct = 60;
                cfg.skew = 0.3;                     // u^0.3 > u → newest files dominate edits
                cfg.cold_access_delay_us = 20_000;
                cfg.warmup_sec = 5.0;
                cfg.measure_sec = 15.0;
            },
        },
        Preset {
            // ── ARC adaptive advantage ─────────────────────────────────────────
            //
            // Hot tier = 16K (~32 files). Mixed signal: 25% creates (recency pressure)
            // + 65% edits at skew=2.5 (frequency pressure) + 10% deletes (working-set
            // churn). The optimal policy must balance BOTH recency and frequency.
            //
            // ARC: adapts its T1/T2 split via ghost-list hits. Over time it converges to
            //   the right balance for this specific mix, minimising cold misses for both
            //   the recency and frequency components.
            // LFU: ignores recency entirely. New files from the 25% creates land in cold
            //   almost immediately (freq=1 vs old files' accumulated counts). Those new
            //   files edited soon after creation pay the cold penalty.
            // basic_lru: ignores frequency. Old frequently-edited files can be displaced
            //   by a burst of creates, paying the cold penalty when re-edited.
            // LRU-2Q: A1in buffer absorbs creates (recency), Am protects hot set
            //   (frequency). Better than basic_lru but fixed 25/75 split may not be
            //   optimal; ARC's adaptive split should edge it out.
            name: "arc_adaptive",
            description: "Mixed recency+frequency (25/10/65, skew=2.5, hot=16K, penalty=8ms). ARC's adaptive split outperforms fixed policies.",
            apply: |cfg| {
                cfg.hot_capacity = 16_000;          // ~32 files
                cfg.create_pct = 25;
                cfg.delete_pct = 10;
                cfg.edit_pct = 65;
                cfg.skew = 2.5;
                cfg.cold_access_delay_us = 8_000;
                cfg.warmup_sec = 5.0;
                cfg.measure_sec = 15.0;
            },
        },
        Preset {
            // ── LRU-2Q scan-resistance advantage ──────────────────────────────
            //
            // Hot tier = 6K (~12 files). Very high create rate (70%) floods `live` with
            // new files that are each touched once or twice then never again (scan noise).
            // skew=4.0 means the few edits that DO happen go almost entirely to the oldest
            // files (the true hot set, indices 0..N*0.02). These are the files that matter.
            //
            // basic_lru: every create writes a new file to MRU. With 70% creates, MRU
            //   slots are constantly stolen by scan files. The true hot set (old files)
            //   drifts to the LRU tail and eventually gets evicted → cold penalty.
            // LRU-2Q: scan files land in A1in (25% buffer, ~3 slots). A1in evicts them
            //   to A1out; they age out of A1out without ever touching Am (75%, ~9 slots).
            //   The true hot set reaches Am via the normal A1in→evict→A1out→re-access→Am
            //   path and stays there. Scan noise cannot displace Am entries.
            // LFU: skew=4.0 means the hot-set files have very high counts → LFU keeps
            //   them perfectly. Acts as an upper bound on placement quality here.
            // ARC: T1 absorbs scan files (recency); T2 holds the hot set (frequency).
            //   Adaptive p helps but ARC also dedicates capacity to ghost lists.
            //
            // Expected ordering: LFU ≥ LRU-2Q > ARC > basic_lru.
            name: "scan_resistance",
            description: "Scan noise + stable hot set (70/0/30, skew=4.0, hot=6K, penalty=12ms). LRU-2Q Am queue is scan-resistant; basic_lru thrashes.",
            apply: |cfg| {
                cfg.hot_capacity = 6_000;           // ~12 files; A1in target ~3, Am ~9
                cfg.create_pct = 70;
                cfg.delete_pct = 0;
                cfg.edit_pct = 30;
                cfg.skew = 4.0;
                cfg.cold_access_delay_us = 12_000;
                cfg.warmup_sec = 6.0;
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
        "  base warmup={:.0}s  measure={:.0}s  cold_access_delay={}µs",
        base.warmup_sec, base.measure_sec, base.cold_access_delay_us
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
