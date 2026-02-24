use std::error::Error;
use std::fs;
use std::io;
use std::os::unix::fs as unix_fs;
use std::path::{Path, PathBuf};

/// Move the backing data for `logical` into `target_root`, preserving the
/// logical path at `logical` by recreating it as a symlink to the new
/// backing location.
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
/// Summary:
/// - All reads/writes go through `logical` (the hot path).
/// - This helper just moves the underlying bytes and rewires `logical`
///   to point at the new backing path while mirroring the directory
///   structure from `hot_root` under `target_root`.
pub fn move_logical_to_dir(
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
