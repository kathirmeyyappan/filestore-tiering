use std::error::Error;
use std::fs;
use std::io;
use std::os::unix::fs as unix_fs;
use std::path::{Path, PathBuf};

/// Move the backing data for a hot path into a given tier.
///
/// This is the only function you need for tiering: demotion, promotion to warm,
/// and promotion to hot are all "put the backing for this hot path in this tier."
///
/// # Canonical path
///
/// The **canonical path** is always under `hot_root` (e.g. `/hot/a/b`). Clients
/// always use that path; where the bytes actually live is an implementation detail.
///
/// # Parameters
///
/// - **`hot_root`** — Root of the hot (canonical) namespace, e.g. `/hot`.
/// - **`hot_path`** — Full path under `hot_root` for the file, e.g. `/hot/a/b`.
///   Must be a child of `hot_root` (verified before any other work).
/// - **`target_dir`** — The tier where the backing should live after the call:
///   - If `target_dir == hot_root`: bytes are moved to `hot_path` (promote to hot).
///     No-op if the backing is already at `hot_path`.
///   - Otherwise (warm or cold): bytes are moved to `target_dir` with the same
///     relative path (e.g. `/cold/a/b`), and `hot_path` becomes a symlink to that.
///     No-op if the backing is already at that path.
///
/// # Examples
///
/// ```ignore
/// use crate::tier_fs::move_to_tier;
///
/// let hot = Path::new("/hot");
/// let cold = Path::new("/cold");
/// let warm = Path::new("/warm");
///
/// // Demote: put backing for /hot/a/b into cold tier → /hot/a/b becomes symlink to /cold/a/b
/// move_to_tier(hot, &hot.join("a/b"), cold)?;
///
/// // Promote cold → warm: put backing into warm tier → /hot/a/b becomes symlink to /warm/a/b
/// move_to_tier(hot, &hot.join("a/b"), warm)?;
///
/// // Promote to hot: put backing at the canonical path → /hot/a/b is now a regular file
/// move_to_tier(hot, &hot.join("a/b"), hot)?;
/// ```
pub fn move_to_tier(
    hot_root: &Path,
    hot_path: &Path,
    target_dir: &Path,
) -> Result<(), Box<dyn Error + Send + Sync>> {
    let rel = hot_path
        .strip_prefix(hot_root)
        .map_err(|_| format!("hot path {hot_path:?} is not a child of hot root {hot_root:?}"))?;

    let meta = fs::symlink_metadata(hot_path).map_err(|e| match e.kind() {
        io::ErrorKind::NotFound => format!("hot path {hot_path:?} does not exist"),
        _ => e.to_string(),
    })?;

    let current_backing = if meta.file_type().is_symlink() {
        let target = fs::read_link(hot_path)?;
        if target.is_absolute() {
            target
        } else {
            hot_path
                .parent()
                .unwrap_or_else(|| Path::new("/"))
                .join(target)
        }
    } else if meta.is_file() {
        hot_path.to_path_buf()
    } else {
        return Err(format!("hot path {hot_path:?} is neither file nor symlink").into());
    };

    let promote_to_hot = target_dir == hot_root;

    if promote_to_hot {
        // Target tier is hot: bytes must live at `hot_path`.
        if current_backing == hot_path {
            return Ok(());
        }
        if let Some(parent) = hot_path.parent() {
            fs::create_dir_all(parent)?;
        }
        if meta.file_type().is_symlink() {
            fs::remove_file(hot_path)?;
        }
        fs::rename(&current_backing, hot_path)?;
    } else {
        // Target tier is warm/cold: bytes at target_dir/rel, hot_path is a symlink.
        let target_backing: PathBuf = target_dir.join(rel);
        if current_backing == target_backing {
            return Ok(());
        }
        if let Some(parent) = target_backing.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::rename(&current_backing, &target_backing)?;
        if meta.file_type().is_symlink() || meta.is_file() {
            let _ = fs::remove_file(hot_path);
        }
        unix_fs::symlink(&target_backing, hot_path)?;
    }

    Ok(())
}
