//! Daemon setup: policy construction and path validation for the main tiering process.
//!
//! Used by the main binary (watch loop) and by the benchmark to build a policy instance.

use std::collections::HashMap;
use std::path::PathBuf;

use anyhow::Result;

use crate::policy_engine::{PolicyEngine, TierState};

/// Ensure path exists and is a directory. Used before starting the watcher.
pub fn ensure_dir_exists(path: &std::path::Path, name: &str) -> Result<()> {
    if !path.exists() {
        anyhow::bail!("{} path does not exist: {:?}", name, path);
    }
    if !path.is_dir() {
        anyhow::bail!("{} is not a directory: {:?}", name, path);
    }
    Ok(())
}

/// Build a policy by name. Hot/cold paths are canonicalized; tier state is initialized from disk.
///
/// Named variants (e.g. `lru_2q_small`) are aliases that pre-populate specific
/// `policy_params` defaults. Explicitly passed `--policy-param` values always win.
pub fn make_policy(
    name: &str,
    hot_storage: &std::path::Path,
    cold_storage: &[PathBuf],
    hot_capacity: u64,
    cold_capacities: Vec<u64>,
    policy_params: &HashMap<String, f64>,
) -> Result<Box<dyn PolicyEngine>> {
    let to_err = |e: Box<dyn std::error::Error + Send + Sync>| anyhow::anyhow!("{}", e);
    let hot_root = std::fs::canonicalize(hot_storage).map_err(|e| to_err(e.into()))?;
    let cold_roots: Vec<PathBuf> = cold_storage
        .iter()
        .map(|p| std::fs::canonicalize(p).map_err(|e| to_err(e.into())))
        .collect::<Result<_, _>>()?;
    let mut tier_state =
        TierState::new(hot_root.clone(), cold_roots, hot_capacity, cold_capacities);
    tier_state.init_bytes().map_err(to_err)?;

    // Merge variant-specific defaults into the caller-supplied params map.
    // Caller-supplied values (--policy-param) always take precedence.
    let with_defaults = |defaults: &[(&str, f64)]| -> HashMap<String, f64> {
        let mut p = policy_params.clone();
        for &(k, v) in defaults {
            p.entry(k.to_string()).or_insert(v);
        }
        p
    };

    match name {
        "basic_lru" => {
            crate::policies::basic_lru::BasicLruPolicy::validate_config(hot_storage, cold_storage)
                .map_err(to_err)?;
            Ok(Box::new(crate::policies::basic_lru::BasicLruPolicy::new(
                tier_state,
            )))
        }
        "arc" => {
            crate::policies::arc::ArcPolicy::validate_config(hot_storage, cold_storage)
                .map_err(to_err)?;
            Ok(Box::new(crate::policies::arc::ArcPolicy::new(tier_state)))
        }
        "lfu" => {
            crate::policies::lfu::LfuPolicy::validate_config(hot_storage, cold_storage)
                .map_err(to_err)?;
            Ok(Box::new(crate::policies::lfu::LfuPolicy::new(tier_state)))
        }

        // ── LRU-2Q variants ──────────────────────────────────────────────────
        // a1in_fraction controls how much of hot capacity is reserved for the
        // probationary A1in queue; the remainder goes to the protected Am queue.
        "lru_2q" | "lru_2q_small" | "lru_2q_large" => {
            let p = match name {
                // 10 %: Am dominates — best for workloads with a stable, small hot set.
                "lru_2q_small" => with_defaults(&[("a1in_fraction", 0.10)]),
                // 50 %: large probationary buffer — better scan/churn resistance.
                "lru_2q_large" => with_defaults(&[("a1in_fraction", 0.50)]),
                _ => policy_params.clone(),
            };
            crate::policies::lru_2q::Lru2QPolicy::validate_config(hot_storage, cold_storage)
                .map_err(to_err)?;
            Ok(Box::new(
                crate::policies::lru_2q::Lru2QPolicy::new_with_params(tier_state, &p),
            ))
        }

        // ── LeCaR variants ───────────────────────────────────────────────────
        // learning_rate controls how fast weights shift after a ghost-list hit.
        // w_lru is the initial weight given to the LRU expert (0.5 = balanced).
        "lecar" | "lecar_fast" | "lecar_slow" => {
            let p = match name {
                // 0.90: reacts aggressively to each ghost hit; good for shifting workloads.
                "lecar_fast" => with_defaults(&[("learning_rate", 0.90)]),
                // 0.05: conservative; stabilizes when the optimal expert is clear.
                "lecar_slow" => with_defaults(&[("learning_rate", 0.05)]),
                _ => policy_params.clone(),
            };
            crate::policies::lecar::LeCarPolicy::validate_config(hot_storage, cold_storage)
                .map_err(to_err)?;
            Ok(Box::new(
                crate::policies::lecar::LeCarPolicy::new_with_params(tier_state, &p),
            ))
        }

        // ── CACHEUS variants ─────────────────────────────────────────────────
        // w_sr_lru is the initial weight for the SR-LRU expert vs CR-LFU.
        "cacheus" | "cacheus_lru_biased" | "cacheus_lfu_biased" => {
            let p = match name {
                // 0.80: starts strongly biased toward scan-resistant LRU.
                "cacheus_lru_biased" => with_defaults(&[("w_sr_lru", 0.80)]),
                // 0.20: starts biased toward decaying-frequency LFU.
                "cacheus_lfu_biased" => with_defaults(&[("w_sr_lru", 0.20)]),
                _ => policy_params.clone(),
            };
            crate::policies::cacheus::CacheusPolicy::validate_config(hot_storage, cold_storage)
                .map_err(to_err)?;
            Ok(Box::new(
                crate::policies::cacheus::CacheusPolicy::new_with_params(tier_state, &p),
            ))
        }

        // ── Decision-tree variants ───────────────────────────────────────────
        // retrain_interval: evictions between tree retrains.
        // tree_max_depth: maximum depth of the regression tree.
        "decision_tree" | "decision_tree_deep" | "decision_tree_fast" => {
            let p = match name {
                // Deeper tree — richer feature splits, more overfitting risk.
                "decision_tree_deep" => with_defaults(&[("tree_max_depth", 8.0)]),
                // Retrains 5× more often — adapts faster, higher compute cost.
                "decision_tree_fast" => with_defaults(&[("retrain_interval", 10.0)]),
                _ => policy_params.clone(),
            };
            crate::policies::decision_tree::DecisionTreePolicy::validate_config(
                hot_storage,
                cold_storage,
            )
            .map_err(to_err)?;
            Ok(Box::new(
                crate::policies::decision_tree::DecisionTreePolicy::new_with_params(
                    tier_state,
                    &p,
                ),
            ))
        }

        "dummy" => {
            crate::policies::dummy::DummyPolicy::validate_config(hot_storage, cold_storage)
                .map_err(to_err)?;
            Ok(Box::new(crate::policies::dummy::DummyPolicy::new(
                tier_state,
            )))
        }
        _ => Err(anyhow::anyhow!("unknown policy: {}", name)),
    }
}

// Re-export for callers that want capacity helpers from the same place as make_policy
pub use crate::capacity::{format_capacity, parse_capacity, resolve_cold_capacities};
