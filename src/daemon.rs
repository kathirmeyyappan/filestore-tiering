//! Daemon setup: policy construction and path validation for the main tiering process.
//!
//! Used by the main binary (watch loop) and by the benchmark to build a policy instance.

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
pub fn make_policy(
    name: &str,
    hot_storage: &std::path::Path,
    cold_storage: &[PathBuf],
    hot_capacity: u64,
    cold_capacities: Vec<u64>,
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
        "lru_2q" => {
            crate::policies::lru_2q::Lru2QPolicy::validate_config(hot_storage, cold_storage)
                .map_err(to_err)?;
            Ok(Box::new(crate::policies::lru_2q::Lru2QPolicy::new(
                tier_state,
            )))
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
