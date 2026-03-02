//! Integration tests: run the real binary with temp hot/cold dirs and assert policy behavior.
//!
//! These tests are run on CI (e.g. GitHub Actions). They spawn the daemon, perform file ops,
//! wait for at least one poll cycle, then assert on the filesystem state.

#![cfg(unix)]

use std::fs;
use std::process::{Child, Command, Stdio};
use std::thread;
use std::time::Duration;

// Path to the built binary. Cargo sets CARGO_BIN_EXE_* when running the test; else default path.
fn bin_path() -> std::path::PathBuf {
    std::env::var("CARGO_BIN_EXE_filestore_tiering")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|_| std::path::PathBuf::from("target/debug/filestore-tiering"))
}

fn start_daemon(
    hot: &str,
    cold: &str,
    hot_cap: u64,
    interval_secs: u64,
    policy: &str,
) -> std::io::Result<Child> {
    Command::new(bin_path())
        .args([
            "--hot-storage",
            hot,
            "-c",
            cold,
            "--hot-capacity",
            &hot_cap.to_string(),
            "--policy",
            policy,
            "-i",
            &interval_secs.to_string(),
        ])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
}

/// basic_lru: over capacity causes eviction; hot path becomes symlink, content in cold.
#[test]
fn basic_lru_evicts_when_over_capacity() {
    let hot_dir = tempfile::tempdir().unwrap();
    let cold_dir = tempfile::tempdir().unwrap();
    let hot = hot_dir.path();
    let cold = cold_dir.path();

    // One file larger than hot capacity so first run evicts it
    fs::create_dir_all(hot.join("sub")).unwrap();
    let content = b"xxxxxxxxxxxxxxxxxxxx"; // 20 bytes
    fs::write(hot.join("sub/file"), content).unwrap();

    let mut child = start_daemon(
        hot.to_str().unwrap(),
        cold.to_str().unwrap(),
        15,
        1,
        "basic_lru",
    )
    .expect("start daemon");

    // Allow initial fill + over-cap eviction (2 polls)
    thread::sleep(Duration::from_secs(3));

    let _ = child.kill();
    let _ = child.wait();

    let hot_file = hot.join("sub/file");
    let cold_file = cold.join("sub/file");

    assert!(
        fs::symlink_metadata(&hot_file)
            .map(|m| m.file_type().is_symlink())
            .unwrap_or(false),
        "hot path should be a symlink after eviction"
    );
    assert!(cold_file.exists(), "content should exist in cold");
    assert_eq!(
        fs::read(&cold_file).unwrap(),
        content,
        "cold file content should match"
    );
}

/// arc: over capacity causes eviction; hot path becomes symlink, content in cold.
#[test]
fn arc_evicts_when_over_capacity() {
    let hot_dir = tempfile::tempdir().unwrap();
    let cold_dir = tempfile::tempdir().unwrap();
    let hot = hot_dir.path();
    let cold = cold_dir.path();

    fs::create_dir_all(hot.join("sub")).unwrap();
    let content = b"xxxxxxxxxxxxxxxxxxxx"; // 20 bytes
    fs::write(hot.join("sub/file"), content).unwrap();

    let mut child = start_daemon(hot.to_str().unwrap(), cold.to_str().unwrap(), 15, 1, "arc")
        .expect("start daemon");

    thread::sleep(Duration::from_secs(3));

    let _ = child.kill();
    let _ = child.wait();

    let hot_file = hot.join("sub/file");
    let cold_file = cold.join("sub/file");

    assert!(
        fs::symlink_metadata(&hot_file)
            .map(|m| m.file_type().is_symlink())
            .unwrap_or(false),
        "hot path should be a symlink after eviction"
    );
    assert!(cold_file.exists(), "content should exist in cold");
    assert_eq!(
        fs::read(&cold_file).unwrap(),
        content,
        "cold file content should match"
    );
}

/// lfu: over capacity causes eviction; hot path becomes symlink, content in cold.
#[test]
fn lfu_evicts_when_over_capacity() {
    let hot_dir = tempfile::tempdir().unwrap();
    let cold_dir = tempfile::tempdir().unwrap();
    let hot = hot_dir.path();
    let cold = cold_dir.path();

    fs::create_dir_all(hot.join("sub")).unwrap();
    let content = b"xxxxxxxxxxxxxxxxxxxx"; // 20 bytes
    fs::write(hot.join("sub/file"), content).unwrap();

    let mut child = start_daemon(hot.to_str().unwrap(), cold.to_str().unwrap(), 15, 1, "lfu")
        .expect("start daemon");

    // Allow initial fill + over-cap eviction (2 polls)
    thread::sleep(Duration::from_secs(3));

    let _ = child.kill();
    let _ = child.wait();

    let hot_file = hot.join("sub/file");
    let cold_file = cold.join("sub/file");

    assert!(
        fs::symlink_metadata(&hot_file)
            .map(|m| m.file_type().is_symlink())
            .unwrap_or(false),
        "hot path should be a symlink after eviction"
    );
    assert!(cold_file.exists(), "content should exist in cold");
    assert_eq!(
        fs::read(&cold_file).unwrap(),
        content,
        "cold file content should match"
    );
}
