use std::error::Error;
use std::fs;
use std::io;
use std::os::unix::fs as unix_fs;
use std::path::{Path, PathBuf};

/// Internal: move the backing for a hot path into a tier. Used by [`TierState::move_to_tier`](crate::tier_state::TierState::move_to_tier);
/// policies should call that method, not this one.
///
/// - **`hot_root`** — Hot namespace root; `hot_path` must be a child (checked first).
/// - **`hot_path`** — The stable path clients use; must exist (file or symlink).
/// - **`target_dir`** — Where the backing should live: if equal to `hot_root`, backing moves to `hot_path` (promote); otherwise backing moves to `target_dir/rel` and `hot_path` becomes a symlink (evict).
///
/// Returns size in bytes moved, or 0 if no-op.
pub fn move_to_tier(
    hot_root: &Path,
    hot_path: &Path,
    target_dir: &Path,
) -> Result<u64, Box<dyn Error + Send + Sync>> {
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

    let size = fs::metadata(&current_backing)?.len();

    let promote_to_hot = target_dir == hot_root;

    if promote_to_hot {
        // Target tier is hot: bytes must live at `hot_path`.
        if current_backing == hot_path {
            return Ok(0);
        }
        if let Some(parent) = hot_path.parent() {
            fs::create_dir_all(parent)?;
        }
        if meta.file_type().is_symlink() {
            fs::remove_file(hot_path)?;
        }
        fs::rename(&current_backing, hot_path)?;
        Ok(size)
    } else {
        // Target tier is warm/cold: bytes at target_dir/rel, hot_path is a symlink.
        let target_backing: PathBuf = target_dir.join(rel);
        if current_backing == target_backing {
            return Ok(0);
        }
        if let Some(parent) = target_backing.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::rename(&current_backing, &target_backing)?;
        if meta.file_type().is_symlink() || meta.is_file() {
            let _ = fs::remove_file(hot_path);
        }
        unix_fs::symlink(&target_backing, hot_path)?;
        Ok(size)
    }
}

/// Size in bytes of all **regular files** under `dir`. Symlinks are **ignored** (count 0)
/// so hot tier size doesn't double-count content in another tier. Used by [`TierState::init_bytes`](crate::tier_state::TierState::init_bytes); call once at startup, not every reorganize (O(n) stats).
pub fn tier_size_bytes(dir: &Path) -> Result<u64, Box<dyn Error + Send + Sync>> {
    let meta = fs::symlink_metadata(dir)?;
    if !meta.is_dir() {
        return Err(format!("not a directory: {dir:?}").into());
    }
    let mut total: u64 = 0;
    for entry in fs::read_dir(dir)? {
        let entry = entry?;
        let path = entry.path();
        let meta = fs::symlink_metadata(&path)?;
        let ft = meta.file_type();
        if ft.is_dir() && !ft.is_symlink() {
            total += tier_size_bytes(&path)?;
        } else if meta.is_file() {
            total += meta.len();
        }
        // symlinks skipped — content lives in another tier
    }
    Ok(total)
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

        let size = move_to_tier(hot_root, &hot_path, cold_root).unwrap();
        assert_eq!(size, 10); // "hello tier".len()

        // hot/a/b should be a symlink to cold/a/b
        assert!(
            fs::symlink_metadata(&hot_path)
                .unwrap()
                .file_type()
                .is_symlink()
        );
        let target = fs::read_link(&hot_path).unwrap();
        assert_eq!(target, cold_root.join("a/b"));
        // Content should be in cold
        assert_eq!(
            fs::read_to_string(cold_root.join("a/b")).unwrap(),
            "hello tier"
        );
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
        assert!(
            !fs::symlink_metadata(&hot_path)
                .unwrap()
                .file_type()
                .is_symlink()
        );
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
        let size2 = move_to_tier(hot_root, &hot_path, cold_root).unwrap(); // no-op
        assert_eq!(size2, 0);

        assert_eq!(fs::read_to_string(cold_root.join("a/b")).unwrap(), "data");
    }

    #[test]
    fn tier_size_bytes_ignores_symlinks() {
        let (hot_dir, cold_dir) = setup_hot_cold();
        let hot_root = hot_dir.path();
        let cold_root = cold_dir.path();

        fs::write(hot_root.join("f1"), b"hello").unwrap();
        fs::write(hot_root.join("f2"), b"world").unwrap();
        fs::create_dir_all(cold_root.join("a")).unwrap();
        fs::write(cold_root.join("a/b"), b"x").unwrap();
        std::os::unix::fs::symlink(cold_root.join("a/b"), hot_root.join("link")).unwrap();

        assert_eq!(tier_size_bytes(hot_root).unwrap(), 5 + 5);
        assert_eq!(tier_size_bytes(cold_root).unwrap(), 1);
    }
}
