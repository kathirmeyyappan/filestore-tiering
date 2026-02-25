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

#[cfg(test)]
#[cfg(unix)]
mod tests {
    use super::*;
    use std::fs;

    fn setup_hot_cold() -> (tempfile::TempDir, tempfile::TempDir) {
        let hot = tempfile::tempdir().unwrap();
        let cold = tempfile::tempdir().unwrap();
        (hot, cold)
    }

    #[test]
    fn evict_file_from_hot_to_cold() {
        let (hot_dir, cold_dir) = setup_hot_cold();
        let hot_root = hot_dir.path();
        let cold_root = cold_dir.path();

        // Create file at hot/a/b
        let hot_path = hot_root.join("a/b");
        fs::create_dir_all(hot_path.parent().unwrap()).unwrap();
        fs::write(&hot_path, b"hello tier").unwrap();

        move_to_tier(hot_root, &hot_path, cold_root).unwrap();

        // hot/a/b should be a symlink to cold/a/b
        assert!(fs::symlink_metadata(&hot_path).unwrap().file_type().is_symlink());
        let target = fs::read_link(&hot_path).unwrap();
        assert_eq!(target, cold_root.join("a/b"));
        // Content should be in cold
        assert_eq!(fs::read_to_string(cold_root.join("a/b")).unwrap(), "hello tier");
        // Reading via hot path should still work (follows symlink)
        assert_eq!(fs::read_to_string(&hot_path).unwrap(), "hello tier");
    }

    #[test]
    fn promote_from_cold_back_to_hot() {
        let (hot_dir, cold_dir) = setup_hot_cold();
        let hot_root = hot_dir.path();
        let cold_root = cold_dir.path();

        // Start with file in cold, symlink at hot
        fs::create_dir_all(cold_root.join("a").as_path()).unwrap();
        fs::write(cold_root.join("a/b"), b"cold content").unwrap();
        let hot_path = hot_root.join("a/b");
        fs::create_dir_all(hot_path.parent().unwrap()).unwrap();
        std::os::unix::fs::symlink(cold_root.join("a/b"), &hot_path).unwrap();

        move_to_tier(hot_root, &hot_path, hot_root).unwrap();

        // hot/a/b should now be a regular file with the content
        assert!(!fs::symlink_metadata(&hot_path).unwrap().file_type().is_symlink());
        assert_eq!(fs::read_to_string(&hot_path).unwrap(), "cold content");
        // Content was moved, not copied; cold path should be gone
        assert!(!cold_root.join("a/b").exists());
    }

    #[test]
    fn hot_path_must_be_under_hot_root() {
        let (hot_dir, cold_dir) = setup_hot_cold();
        let hot_root = hot_dir.path();
        let cold_root = cold_dir.path();

        // hot_path is under cold, not hot
        let bad_path = cold_root.join("a/b");
        fs::create_dir_all(bad_path.parent().unwrap()).unwrap();
        fs::write(&bad_path, b"x").unwrap();

        let err = move_to_tier(hot_root, &bad_path, cold_root).unwrap_err();
        assert!(err.to_string().contains("not a child of hot root"));
    }

    #[test]
    fn noop_when_already_at_target() {
        let (hot_dir, cold_dir) = setup_hot_cold();
        let hot_root = hot_dir.path();
        let cold_root = cold_dir.path();

        let hot_path = hot_root.join("a/b");
        fs::create_dir_all(hot_path.parent().unwrap()).unwrap();
        fs::write(&hot_path, b"data").unwrap();

        move_to_tier(hot_root, &hot_path, cold_root).unwrap();
        move_to_tier(hot_root, &hot_path, cold_root).unwrap(); // no-op

        assert_eq!(fs::read_to_string(cold_root.join("a/b")).unwrap(), "data");
    }
}
