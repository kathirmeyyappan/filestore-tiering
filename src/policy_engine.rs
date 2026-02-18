use std::error::Error;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

/// What happened to the path (create, change, delete, etc.).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FsEventKind {
    Create,
    Modify,
    Remove,
    Access,
    Other,
}

/// A single filesystem event observed by the watcher.
pub struct AccessEvent {
    pub path: PathBuf,
    pub kind: FsEventKind,
    pub timestamp: SystemTime,
}

/// Trait that all tiering policy engines must implement.
///
/// The main loop calls `ingest` with new FS events, then `reorganize`
/// to let the engine move files between tiers as it sees fit.
pub trait PolicyEngine {
    /// Validate that (hot, cold_storage) is acceptable for this policy.
    /// Called before construction; return `Err` to reject (e.g. wrong number of tiers).
    fn validate_config(_hot: &Path, _cold_storage: &[PathBuf]) -> Result<(), Box<dyn Error + Send + Sync>>
    where
        // so that we can only call on concrete type (dyn PolicyEngine)
        Self: Sized,
    {
        // default: accept any config.
        Ok(())
    }

    /// Feed a batch of new access events into the engine.
    fn ingest(&mut self, events: &[AccessEvent]);

    /// Examine internal state and perform any file migrations
    /// (promotions / evictions) between hot, warm, and cold tiers.
    fn reorganize(&mut self) -> Result<(), Box<dyn Error + Send + Sync>>;
}
