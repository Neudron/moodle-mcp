//! Time helpers and per-course `last-sync.json` receipts.

use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};

/// Seconds since the unix epoch (0 on clock failure — never panics).
#[must_use]
pub fn unix_secs_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    (if m <= 2 { y + 1 } else { y }, m as u32, d as u32)
}

/// Unix seconds → `YYYY-MM-DDTHH:MM:SSZ` without extra dependencies.
#[must_use]
pub fn unix_to_iso8601(secs: u64) -> String {
    let days = (secs / 86_400) as i64;
    let clock = secs % 86_400;
    let (y, m, d) = civil_from_days(days);
    format!(
        "{y:04}-{m:02}-{d:02}T{:02}:{:02}:{:02}Z",
        clock / 3600,
        (clock % 3600) / 60,
        clock % 60
    )
}

/// Human-readable “now” for receipts and frontmatter.
#[must_use]
pub fn iso_now() -> String {
    unix_to_iso8601(unix_secs_now())
}

/// Per-course sync receipt written to `<course>/last-sync.json`.
#[derive(Debug, Clone, serde::Serialize)]
pub struct Receipt {
    pub course_id: i64,
    pub shortname: String,
    pub synced_at: String,
    pub files_new: usize,
    pub files_skipped: usize,
    pub bytes_new: u64,
    pub duration_ms: u64,
    pub errors: Vec<String>,
}

pub async fn write_receipt(course_dir: &Path, receipt: &Receipt) -> Result<()> {
    let text = serde_json::to_string_pretty(receipt).context("serialising receipt")?;
    tokio::fs::write(course_dir.join("last-sync.json"), text.as_bytes())
        .await
        .context("writing last-sync.json")?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn gc_iso8601_known_values() {
        assert_eq!(unix_to_iso8601(0), "1970-01-01T00:00:00Z");
        assert_eq!(unix_to_iso8601(1_445_412_480), "2015-10-21T07:28:00Z");
        assert_eq!(unix_to_iso8601(1_700_000_000), "2023-11-14T22:13:20Z");
    }
}
