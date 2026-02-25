use std::path::{Path, PathBuf};

use crate::policy_engine::{AccessEvent, PolicyEngine};

#[allow(dead_code)]
#[derive(Debug)]
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

        // Example: single mover for demotion and promotion.
        //
        // use crate::tier_fs::move_to_tier;
        //
        // let hot_path = self.hot_storage.join("some/relative/path");
        //
        // // Demote to cold
        // move_to_tier(&self.hot_storage, &hot_path, &self.cold_storage[0])?;
        //
        // // Promote to hot (pass hot as target)
        // move_to_tier(&self.hot_storage, &hot_path, &self.hot_storage)?;

        Ok(())
    }
}
