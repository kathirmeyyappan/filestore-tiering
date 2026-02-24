use std::error::Error;
use std::fs;
use std::path::{Path, PathBuf};
use std::{fmt, io};
use std::os::unix::fs as unix_fs;

/// Handles moving file data between storage directories while keeping the
/// logical path in the hot tier stable (via symlinks). Policies receive this
/// and call `move_logical_to_dir` as needed.
pub struct TierFs {
    hot_root: PathBuf,
}

impl TierFs {
    pub fn new(hot_root: PathBuf) -> Self {
        Self { hot_root }
    }

    /// Move the backing data for `logical` into `target_root`, preserving the
    /// logical path at `logical` by recreating it as a symlink to the new
    /// backing location.
    pub fn move_logical_to_dir(
        &mut self,
        logical: &Path,
        target_root: &Path,
    ) -> Result<(), Box<dyn Error + Send + Sync>> {
        let rel = logical
            .strip_prefix(&self.hot_root)
            .map_err(|_| format!("logical path {logical:?} is not under hot root {:?}", self.hot_root))?;

        // Figure out where the current bytes live.
        let meta = fs::symlink_metadata(logical)
            .map_err(|e| match e.kind() {
                io::ErrorKind::NotFound => format!("logical path {logical:?} does not exist"),
                _ => e.to_string(),
            })?;

        let current_backing = if meta.file_type().is_symlink() {
            let target = fs::read_link(logical)?;
            if target.is_absolute() {
                target
            } else {
                logical
                    .parent()
                    .unwrap_or_else(|| Path::new("/"))
                    .join(target)
            }
        } else if meta.is_file() {
            logical.to_path_buf()
        } else {
            return Err(format!("logical path {logical:?} is neither file nor symlink").into());
        };

        let target_backing = target_root.join(rel);

        if current_backing == target_backing {
            return Ok(());
        }

        if let Some(parent) = target_backing.parent() {
            fs::create_dir_all(parent)?;
        }

        fs::rename(&current_backing, &target_backing)?;

        if meta.file_type().is_symlink() || meta.is_file() {
            let _ = fs::remove_file(logical);
        }

        unix_fs::symlink(&target_backing, logical)?;

        Ok(())
    }
}

impl fmt::Debug for TierFs {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("TierFs")
            .field("hot_root", &self.hot_root)
            .finish()
    }
}
