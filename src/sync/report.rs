//! Sync outcome types (Task 3, P2 sync engine).
//!
//! [`SyncReport`] is the stable per-course outcome returned by
//! [`crate::sync::sync_course`]. Its field set is frozen: `src/bin/mcp.rs`
//! destructures it, so new executor data goes on [`Report`] instead —
//! never extend this struct without updating every shell.
//!
//! [`Report`] is the full executor outcome consumed by Tasks 4–5
//! (planner/executor binding): files/bytes/duration/errors plus
//! `next_cursor` for paged MCP sync (always `None` for a whole-course run).

use serde::{Deserialize, Serialize};

/// Stable per-course sync outcome (frozen for `src/bin/*`).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SyncReport {
    pub course_id: i64,
    pub shortname: String,
    pub files_new: usize,
    pub files_skipped: usize,
    pub errors: Vec<String>,
}

/// Full executor outcome: the binding surface for Tasks 4–5.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Report {
    pub course_id: i64,
    pub shortname: String,
    pub files_new: usize,
    pub files_skipped: usize,
    /// New payload bytes downloaded (moves/reuses contribute 0).
    pub bytes_new: u64,
    /// Wall-clock time for plan + execute + index, in milliseconds.
    pub duration_ms: u64,
    pub errors: Vec<String>,
    /// Paged-sync cursor for MCP chunking; `None` when the course is done.
    #[serde(default)]
    pub next_cursor: Option<String>,
}

impl Report {
    #[must_use]
    pub fn empty(course_id: i64, shortname: &str) -> Self {
        Self {
            course_id,
            shortname: shortname.to_string(),
            files_new: 0,
            files_skipped: 0,
            bytes_new: 0,
            duration_ms: 0,
            errors: Vec::new(),
            next_cursor: None,
        }
    }

    #[must_use]
    pub fn to_sync_report(&self) -> SyncReport {
        SyncReport {
            course_id: self.course_id,
            shortname: self.shortname.clone(),
            files_new: self.files_new,
            files_skipped: self.files_skipped,
            errors: self.errors.clone(),
        }
    }
}
