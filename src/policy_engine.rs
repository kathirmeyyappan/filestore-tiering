use std::error::Error;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

pub use crate::tier_state::TierState;

/// What happened to the path (create, change, delete, etc.).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FsEventKind {
    Create,
    Modify,
    Remove,
    Access,
    Other,
}

/// Counts of promotions and demotions for benchmarking and evaluation.
/// Policies that perform moves should track these and return them from `stats()`.
#[derive(Debug, Clone, Default)]
pub struct PolicyStats {
    /// Total number of promotions (cold → hot).
    pub promotions: u64,
    /// Total number of demotions (hot → cold). Sum of `demotions_to_tier`.
    pub demotions: u64,
    /// Demotions per cold tier (index 0 = warmest). Length = number of cold tiers.
    pub demotions_to_tier: Vec<u64>,
}

impl PolicyStats {
    pub fn new(num_cold_tiers: usize) -> Self {
        Self {
            promotions: 0,
            demotions: 0,
            demotions_to_tier: vec![0; num_cold_tiers],
        }
    }
}

/// A single filesystem event observed by the watcher.
#[derive(Debug)]
pub struct AccessEvent {
    pub path: PathBuf,
    pub kind: FsEventKind,
    pub timestamp: SystemTime,
}

/// Trait that all tiering policy engines must implement.
///
/// The main loop calls `ingest` with new FS events, then `reorganize`
/// to let the engine move files between tiers as it sees fit.
///
/// Requires `Send` so the benchmark can run the daemon in a dedicated thread
/// while the workload runs on the main thread (matching production architecture).
pub trait PolicyEngine: Send {
    /// Validate that (hot, cold_storage) is acceptable for this policy.
    /// Called before construction; return `Err` to reject (e.g. wrong number of tiers).
    fn validate_config(
        _hot: &Path,
        _cold_storage: &[PathBuf],
    ) -> Result<(), Box<dyn Error + Send + Sync>>
    where
        Self: Sized,
    {
        Ok(())
    }

    /// Feed a batch of new access events into the engine.
    fn ingest(&mut self, events: &[AccessEvent]);

    /// Examine internal state and perform any file migrations
    /// (promotions / evictions) between hot, warm, and cold tiers.
    /// Tier state is held inside the engine; use `self.tier_state` (or equivalent) for paths,
    /// capacity, and `move_to_tier`. After each move that returns a non-zero size, the policy
    /// must call `adjust_hot_bytes` and/or `adjust_cold_bytes` to keep tier byte counts correct.
    fn reorganize(&mut self) -> Result<(), Box<dyn Error + Send + Sync>>;

    /// Return current promotion/demotion counts for benchmarking. Default: zeros.
    fn stats(&self) -> PolicyStats {
        PolicyStats::default()
    }
}
