//! Integration tests: run the real binary with temp hot/cold dirs and assert policy behavior.
//!
//! These tests are run on CI (e.g. GitHub Actions). They spawn the daemon, perform file ops,
//! wait for at least one poll cycle, then assert on the filesystem state.

#![cfg(unix)]

use std::fs;
use std::process::{Child, Command, Stdio};
use std::thread;
use std::time::{Duration, SystemTime};

use filestore_tiering::policy_engine::{AccessEvent, FsEventKind};

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

// ── Cross-policy oscillation regression test ─────────────────────────────────
// Eviction creates a symlink at the hot path. The FS watcher fires Create on
// that path. The touched filter must suppress the event so the file is NOT
// immediately re-promoted. Without the fix, a "symlink exception" would
// re-include the event and cause an evict→promote→evict oscillation loop.

/// Policies that perform eviction and should be tested for oscillation.
const EVICTING_POLICIES: &[&str] = &[
    "basic_lru",
    "arc",
    "lfu",
    "lru_2q",
    "lecar",
    "cacheus",
    "decision_tree",
];

fn assert_no_oscillation(policy_name: &str) {
    let hot_dir = tempfile::tempdir().unwrap();
    let cold_dir = tempfile::tempdir().unwrap();
    let hot_root = fs::canonicalize(hot_dir.path()).unwrap();
    let cold_root = fs::canonicalize(cold_dir.path()).unwrap();

    // Two files × 10 bytes, capacity = 15 → one must be evicted.
    fs::write(hot_root.join("f1"), b"tenbytes!!").unwrap();
    fs::write(hot_root.join("f2"), b"tenbytes!!").unwrap();

    let mut policy = filestore_tiering::daemon::make_policy(
        policy_name,
        &hot_root,
        &[cold_root.clone()],
        15,
        vec![u64::MAX],
    )
    .unwrap_or_else(|e| panic!("[{policy_name}] make_policy failed: {e}"));

    policy
        .reorganize()
        .unwrap_or_else(|e| panic!("[{policy_name}] first reorganize failed: {e}"));

    let stats_after = policy.stats();
    assert!(
        stats_after.demotions >= 1,
        "[{policy_name}] should evict at least one file, got {} demotions",
        stats_after.demotions
    );

    // Find which file was evicted (is now a symlink in hot).
    let evicted = ["f1", "f2"]
        .iter()
        .map(|n| hot_root.join(n))
        .find(|p| {
            fs::symlink_metadata(p)
                .map(|m| m.file_type().is_symlink())
                .unwrap_or(false)
        })
        .unwrap_or_else(|| panic!("[{policy_name}] no evicted file found"));

    let promotions_before = policy.stats().promotions;

    // Simulate the FS events that the eviction would generate:
    // Create on hot path (now a symlink) + Create on cold backing.
    let cold_backing = cold_root.join(evicted.file_name().unwrap());
    policy.ingest(&[
        AccessEvent {
            path: evicted.clone(),
            kind: FsEventKind::Create,
            timestamp: SystemTime::now(),
        },
        AccessEvent {
            path: cold_backing.clone(),
            kind: FsEventKind::Create,
            timestamp: SystemTime::now(),
        },
    ]);
    policy
        .reorganize()
        .unwrap_or_else(|e| panic!("[{policy_name}] second reorganize failed: {e}"));

    // The file must STILL be a symlink (cold). No spurious promotion.
    assert!(
        fs::symlink_metadata(&evicted)
            .map(|m| m.file_type().is_symlink())
            .unwrap_or(false),
        "[{policy_name}] evicted file must stay in cold — no oscillation",
    );
    assert_eq!(
        policy.stats().promotions,
        promotions_before,
        "[{policy_name}] no new promotions should have occurred",
    );
}

#[test]
fn eviction_does_not_oscillate_any_policy() {
    for &policy_name in EVICTING_POLICIES {
        assert_no_oscillation(policy_name);
    }
}
