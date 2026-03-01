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

/// Parameters for logging when reorganize finishes. Counts can be 0 for policies that don't use all fields.
#[derive(Debug)]
pub struct ReorganizeDoneParams {
    pub policy_name: &'static str,
    pub should_log: bool,
    pub new_in_hot: u32,
    pub promoted: u32,
    pub evicted_room: u32,
    pub evicted_cap: u32,
    pub hot_bytes: u64,
    pub cold_bytes: u64,
}

/// Log when reorganize finishes and something changed. Call only when `params.should_log` is true
/// (e.g. `had_touches || evicted_cap > 0`).
pub fn log_reorganize_done(params: ReorganizeDoneParams) {
    if params.should_log {
        log::info!(
            "[{}] done: {} new, {} promoted, {} evicted(room), {} evicted(cap), hot={} cold={}",
            params.policy_name,
            params.new_in_hot,
            params.promoted,
            params.evicted_room,
            params.evicted_cap,
            params.hot_bytes,
            params.cold_bytes
        );
    }
}
