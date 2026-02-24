use std::path::{Path, PathBuf};

use crate::policy_engine::{AccessEvent, PolicyEngine};

pub struct DummyPolicy {
    pub hot_storage: PathBuf,
    pub cold_storage: Vec<PathBuf>,
}

impl DummyPolicy {
    pub fn new(hot_storage: PathBuf, cold_storage: Vec<PathBuf>) -> Self {
        Self {
            hot_storage,
            cold_storage,
        }
    }
}

impl PolicyEngine for DummyPolicy {
    fn validate_config(
        _hot: &Path,
        cold_storage: &[std::path::PathBuf],
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        if cold_storage.is_empty() {
            return Err("dummy policy requires at least one cold_storage tier".into());
        }
        Ok(())
    }

    fn ingest(&mut self, events: &[AccessEvent]) {
        log::info!("[dummy policy] ingest called with {} events", events.len());
    }

    fn reorganize(&mut self) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        log::info!("[dummy policy] reorganize called");

        // Example: evict a file from hot storage into the first cold tier.
        // (real policies would decide *which* logical paths to evict.)
        //
        // use crate::tier_fs::evict_to_tier;
        //
        // let logical_path = self.hot_storage.join("some/relative/path");
        // let first_cold_tier = &self.cold_storage[0];
        // evict_to_tier(
        //     &self.hot_storage,
        //     &logical_path,
        //     first_cold_tier,
        // )?;
        //
        // Example: promote a file back into hot storage from any tier.
        //
        // use crate::tier_fs::promote_to_hot;
        //
        // let logical_path = self.hot_storage.join("some/relative/path");
        // promote_to_hot(
        //     &self.hot_storage,
        //     &logical_path,
        // )?;

        Ok(())
    }
}
