use std::error::Error;
use std::fs;
use std::io;
use std::os::unix::fs as unix_fs;
use std::path::{Path, PathBuf};

/// Evict data from the hot tier into a colder tier.
///
/// Intuition:
/// - Callers think in terms of **\"pointer path\" + \"target dir\"**.
/// - `logical` is the stable path clients use (e.g. `/hot/a/b`).
/// - `target_root` is the tier you want the bytes to live in (e.g. `/cold`).
///
/// Role of `hot_root`:
/// - We need to know **how much of `logical` to mirror** under `target_root`.
/// - Given `hot_root = /hot` and `logical = /hot/a/b`, we strip the prefix
///   to get `rel = a/b`, then store the backing at `/cold/a/b`.
///
/// After eviction:
/// - All reads/writes still go through `logical` (the hot path).
/// - The entry at `logical` is a symlink pointing into `target_root`.
pub fn evict_to_tier(
    hot_root: &Path,
    logical: &Path,
    target_root: &Path,
) -> Result<(), Box<dyn Error + Send + Sync>> {
    let rel = logical
        .strip_prefix(hot_root)
        .map_err(|_| format!("logical path {logical:?} is not under hot root {hot_root:?}"))?;

    // Figure out where the current bytes live.
    let meta = fs::symlink_metadata(logical).map_err(|e| match e.kind() {
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

    let target_backing: PathBuf = target_root.join(rel);

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

/// Promote data back into the hot tier.
///
/// Intuition:
/// - `logical` is still the client-facing path under `hot_root`.
/// - The current backing may live in some colder tier (e.g. `/cold/a/b`).
/// - After promotion, the bytes live directly at `logical` again, and
///   `logical` is a regular file (not a symlink).
pub fn promote_to_hot(
    hot_root: &Path,
    logical: &Path,
) -> Result<(), Box<dyn Error + Send + Sync>> {
    let _rel = logical
        .strip_prefix(hot_root)
        .map_err(|_| format!("logical path {logical:?} is not under hot root {hot_root:?}"))?;

    let meta = fs::symlink_metadata(logical).map_err(|e| match e.kind() {
        io::ErrorKind::NotFound => format!("logical path {logical:?} does not exist"),
        _ => e.to_string(),
    })?;

    // If it's already a regular file at the logical path, nothing to do.
    if meta.is_file() && !meta.file_type().is_symlink() {
        return Ok(());
    }

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
    } else {
        // Unexpected type (e.g. directory); bail.
        return Err(format!("logical path {logical:?} is not a file or symlink").into());
    };

    // We want the bytes to end up directly at `logical`.
    if current_backing == logical {
        // Already there (self-consistent), so nothing to do.
        return Ok(());
    }

    if let Some(parent) = logical.parent() {
        fs::create_dir_all(parent)?;
    }

    // Remove the symlink at `logical` so we can rename into place.
    if meta.file_type().is_symlink() {
        fs::remove_file(logical)?;
    }

    fs::rename(&current_backing, logical)?;

    Ok(())
}
