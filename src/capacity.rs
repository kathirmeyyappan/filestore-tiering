//! Capacity parsing and formatting for CLI and config.
//!
//! Used by the daemon (hot/cold tier limits) and the benchmark (--hot-capacity).

use std::path::PathBuf;

use anyhow::Result;

/// Parse a capacity string into bytes.
/// Accepts: plain number; 1K/1M/1G/1T (decimal); 1Ki/1Mi/1Gi/1Ti (binary); or "unlimited"/"max".
pub fn parse_capacity(s: &str) -> Result<u64, String> {
    let s = s.trim();
    if s.eq_ignore_ascii_case("unlimited") || s.eq_ignore_ascii_case("max") {
        return Ok(u64::MAX);
    }
    let s = s.replace(',', "");
    let mut digits_end = 0;
    for c in s.chars() {
        if c.is_ascii_digit() {
            digits_end += 1;
        } else {
            break;
        }
    }
    let (num_str, unit) = s.split_at(digits_end);
    let num: u64 = num_str
        .parse()
        .map_err(|_| format!("invalid number: {:?}", num_str))?;
    let unit = unit.trim();
    if unit.is_empty() {
        return Ok(num);
    }
    let multiplier: u64 = match unit {
        "K" | "k" => 1_000,
        "M" | "m" => 1_000_000,
        "G" | "g" => 1_000_000_000,
        "T" | "t" => 1_000_000_000_000,
        "Ki" | "ki" => 1024,
        "Mi" | "mi" => 1024 * 1024,
        "Gi" | "gi" => 1024 * 1024 * 1024,
        "Ti" | "ti" => 1024 * 1024 * 1024 * 1024,
        _ => {
            return Err(format!(
                "unknown unit: {:?} (use K, M, G, T or Ki, Mi, Gi, Ti)",
                unit
            ));
        }
    };
    num.checked_mul(multiplier)
        .ok_or_else(|| format!("capacity overflow: {} {}", num, unit))
}

/// Expand cold capacities: None → all unlimited; one value → every tier; N values → one per tier.
pub fn resolve_cold_capacities(
    cold_storage: &[PathBuf],
    cold_capacities: Option<&[u64]>,
) -> Result<Vec<u64>> {
    let n = cold_storage.len();
    match cold_capacities {
        None => Ok(vec![u64::MAX; n]),
        Some(v) if v.len() == 1 => Ok(vec![v[0]; n]),
        Some(v) if v.len() == n => Ok(v.to_vec()),
        Some(v) => anyhow::bail!(
            "cold-capacities: expected 1 (for all tiers) or {} values (one per cold tier), got {}",
            n,
            v.len()
        ),
    }
}

/// Format byte count for log output (e.g. 1073741824 → "1G", u64::MAX → "unlimited").
pub fn format_capacity(b: u64) -> String {
    if b == u64::MAX {
        "unlimited".to_string()
    } else if b >= 1_000_000_000_000 {
        format!("{}T", b / 1_000_000_000_000)
    } else if b >= 1_000_000_000 {
        format!("{}G", b / 1_000_000_000)
    } else if b >= 1_000_000 {
        format!("{}M", b / 1_000_000)
    } else if b >= 1_000 {
        format!("{}K", b / 1_000)
    } else {
        format!("{}", b)
    }
}
