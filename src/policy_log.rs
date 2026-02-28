//! Shared logging helpers for policy engines.
//!
//! Use these so all policies log ingest / initial-fill / reorganize-done in the same format
//! without duplicating format strings or conditions.

/// Log after ingesting events. Logs only when `events_len > 0`.
pub fn log_ingest(policy_name: &str, events_len: usize) {
    if events_len > 0 {
        log::info!("[{}] ingest {} events", policy_name, events_len);
    }
}

/// Log after the first-time fill of the hot tier (e.g. queue seeded from disk).
pub fn log_initial_fill(policy_name: &str, file_count: usize, hot_bytes: u64) {
    log::info!(
        "[{}] initial fill {} files, hot_bytes={}",
        policy_name,
        file_count,
        hot_bytes
    );
}

/// Log when reorganize finishes and something changed. Call only when `should_log` is true
/// (e.g. `had_touches || evicted_cap > 0`). Counts can be 0 for policies that don't use all fields.
pub fn log_reorganize_done(
    policy_name: &str,
    should_log: bool,
    new_in_hot: u32,
    promoted: u32,
    evicted_room: u32,
    evicted_cap: u32,
    hot_bytes: u64,
    cold_bytes: u64,
) {
    if should_log {
        log::info!(
            "[{}] done: {} new, {} promoted, {} evicted(room), {} evicted(cap), hot={} cold={}",
            policy_name,
            new_in_hot,
            promoted,
            evicted_room,
            evicted_cap,
            hot_bytes,
            cold_bytes
        );
    }
}
