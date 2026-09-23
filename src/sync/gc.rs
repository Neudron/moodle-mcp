//! Garbage collection, audit log, receipts and verify/repair (Task 3).
//!
//! Two-step safety (`--prune --apply`): [`preview`] lists state entries
//! whose URL vanished from contents; [`trash`] moves them (plus untracked
//! `.md` companions of `.url` pointers) to
//! `$MOODLE_ROOT/.moodle/trash/<ts>/` preserving relative paths and appends
//! to `audit.jsonl`; [`apply_trash`] permanently deletes a trash dir.
//! Default syncs never delete — stale entries just drop out of state.
//!
//! Also here: per-course `last-sync.json` receipts ([`write_receipt`]),
//! disk-vs-state [`verify`] and mismatch-only [`repair`].

use std::collections::{BTreeMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use tokio::sync::Semaphore;

use crate::errors::redact_token;
use crate::moodle::Moodle;
use crate::state::FileEntry;
use crate::sync::indexer;

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

/// One GC candidate: a state entry whose URL left the contents.
#[derive(Debug, Clone, serde::Serialize)]
pub struct GcCandidate {
    pub url: String,
    pub path: String,
    pub size: i64,
}

/// Pure preview: state entries whose URL is not in `active_urls`.
#[must_use]
pub fn preview(
    old_files: &BTreeMap<String, FileEntry>,
    active_urls: &HashSet<String>,
) -> Vec<GcCandidate> {
    let mut out: Vec<GcCandidate> = old_files
        .iter()
        .filter(|(url, _)| !active_urls.contains(url.as_str()))
        .map(|(url, entry)| GcCandidate {
            url: url.clone(),
            path: entry.path.clone(),
            size: entry.size,
        })
        .collect();
    out.sort_by(|a, b| a.path.cmp(&b.path));
    out
}

#[must_use]
pub fn audit_path(root: &Path) -> PathBuf {
    root.join(".moodle/audit.jsonl")
}

/// Append one audit line (`ts, actor, action, path, size`).
pub async fn append_audit(
    root: &Path,
    actor: &str,
    action: &str,
    path: &str,
    size: i64,
) -> Result<()> {
    let dest = audit_path(root);
    if let Some(parent) = dest.parent() {
        tokio::fs::create_dir_all(parent)
            .await
            .context("creating .moodle dir")?;
    }
    let line = serde_json::json!({
        "ts": iso_now(),
        "actor": actor,
        "action": action,
        "path": path,
        "size": size,
    });
    let mut text = serde_json::to_string(&line).context("serialising audit line")?;
    text.push('\n');
    use tokio::io::AsyncWriteExt as _;
    let mut opts = tokio::fs::OpenOptions::new();
    opts.create(true).append(true).write(true);
    let mut file = opts.open(&dest).await.context("opening audit.jsonl")?;
    file.write_all(text.as_bytes())
        .await
        .context("appending audit.jsonl")?;
    file.flush().await.context("flushing audit.jsonl")?;
    Ok(())
}

/// Move candidates to `.moodle/trash/<ts>/` (best-effort per file) and audit
/// each outcome. Returns the trash dir. `.md` companions of trashed `.url`
/// pointers ride along even though only the `.url` is tracked in state.
pub async fn trash(candidates: &[GcCandidate], root: &Path, actor: &str) -> Result<PathBuf> {
    let ts = unix_secs_now();
    let trash_dir = root.join(format!(".moodle/trash/{ts}"));
    tokio::fs::create_dir_all(&trash_dir)
        .await
        .context("creating trash dir")?;
    for candidate in candidates {
        let mut rels = vec![candidate.path.clone()];
        if let Some(md) = indexer::pointer_md_rel(&candidate.path) {
            rels.push(md);
        }
        for rel in rels {
            // Jail: refuse absolute paths and `..` escapes.
            if Path::new(&rel).is_absolute() || rel.split('/').any(|seg| seg == "..") {
                let _ = append_audit(root, actor, "trash_refused", &rel, candidate.size).await;
                continue;
            }
            let src = root.join(&rel);
            let dst = trash_dir.join(&rel);
            if tokio::fs::try_exists(&src).await.unwrap_or(false) {
                if let Some(parent) = dst.parent() {
                    if tokio::fs::create_dir_all(parent).await.is_err() {
                        let _ =
                            append_audit(root, actor, "trash_failed", &rel, candidate.size).await;
                        continue;
                    }
                }
                match tokio::fs::rename(&src, &dst).await {
                    Ok(()) => {
                        let _ = append_audit(root, actor, "trash", &rel, candidate.size).await;
                    }
                    Err(_) => {
                        let _ =
                            append_audit(root, actor, "trash_failed", &rel, candidate.size).await;
                    }
                }
            } else {
                let _ = append_audit(root, actor, "trash_missing", &rel, candidate.size).await;
            }
        }
    }
    Ok(trash_dir)
}

/// Permanently delete a trash dir created by [`trash`]. Refuses paths
/// outside `$MOODLE_ROOT/.moodle/trash/`.
pub async fn apply_trash(trash_dir: &Path, root: &Path, actor: &str) -> Result<()> {
    let marker = root.join(".moodle/trash");
    let ok = trash_dir
        .strip_prefix(&marker)
        .map(|rest| !rest.as_os_str().is_empty())
        .unwrap_or(false);
    if !ok {
        anyhow::bail!("refusing to delete outside .moodle/trash");
    }
    tokio::fs::remove_dir_all(trash_dir)
        .await
        .context("deleting trash dir")?;
    let display = trash_dir.display().to_string();
    let _ = append_audit(root, actor, "gc_apply", &display, 0).await;
    Ok(())
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

/// One disk-vs-state mismatch found by [`verify`].
#[derive(Debug, Clone)]
pub struct VerifyMismatch {
    pub url: String,
    pub path: String,
    pub expected_sha: String,
    pub actual_sha: String,
    pub reason: String,
}

fn sha256_hex(bytes: &[u8]) -> String {
    use sha2::Digest as _;
    format!("{:x}", sha2::Sha256::digest(bytes))
}

/// Re-hash every tracked file; report missing/size/hash mismatches.
pub async fn verify(root: &Path, files: &BTreeMap<String, FileEntry>) -> Vec<VerifyMismatch> {
    let mut out = Vec::new();
    for (url, entry) in files.iter() {
        if Path::new(&entry.path).is_absolute() || entry.path.split('/').any(|s| s == "..") {
            out.push(VerifyMismatch {
                url: url.clone(),
                path: entry.path.clone(),
                expected_sha: entry.sha256.clone(),
                actual_sha: String::new(),
                reason: "path_escape".to_string(),
            });
            continue;
        }
        let abs = root.join(&entry.path);
        match tokio::fs::read(&abs).await {
            Err(_) => out.push(VerifyMismatch {
                url: url.clone(),
                path: entry.path.clone(),
                expected_sha: entry.sha256.clone(),
                actual_sha: String::new(),
                reason: "missing".to_string(),
            }),
            Ok(bytes) => {
                if bytes.len() as i64 != entry.size {
                    out.push(VerifyMismatch {
                        url: url.clone(),
                        path: entry.path.clone(),
                        expected_sha: entry.sha256.clone(),
                        actual_sha: sha256_hex(&bytes),
                        reason: "size_mismatch".to_string(),
                    });
                } else if sha256_hex(&bytes) != entry.sha256 {
                    out.push(VerifyMismatch {
                        url: url.clone(),
                        path: entry.path.clone(),
                        expected_sha: entry.sha256.clone(),
                        actual_sha: sha256_hex(&bytes),
                        reason: "hash_mismatch".to_string(),
                    });
                }
            }
        }
    }
    out.sort_by(|a, b| a.path.cmp(&b.path));
    out
}

/// Outcome of [`repair`].
#[derive(Debug, Clone, Default)]
pub struct RepairReport {
    pub fixed: usize,
    pub failed: usize,
    pub errors: Vec<String>,
}

/// Re-download only the mismatched URLs (externals are regenerated
/// locally). Shares the one client governor across 8-way tasks.
pub async fn repair(
    moodle: &Moodle,
    root: &Path,
    mismatches: &[VerifyMismatch],
    actor: &str,
) -> RepairReport {
    let mut report = RepairReport::default();
    let sem = Arc::new(Semaphore::new(8));
    let mut handles = Vec::new();
    for mismatch in mismatches.iter() {
        // Jail check first: never write outside root.
        if Path::new(&mismatch.path).is_absolute() || mismatch.path.split('/').any(|s| s == "..") {
            report.failed += 1;
            report.errors.push(redact_token(&format!(
                "{}: refusing path escape",
                mismatch.path
            )));
            continue;
        }
        if !mismatch.url.contains("/pluginfile.php") {
            // External pointer: regenerate locally, no network.
            let body = format!("[InternetShortcut]\nURL={}\n", mismatch.url);
            let dest = root.join(&mismatch.path);
            let ok = match dest.parent() {
                Some(parent) => tokio::fs::create_dir_all(parent).await.is_ok(),
                None => true,
            } && tokio::fs::write(&dest, body.as_bytes()).await.is_ok();
            if ok {
                report.fixed += 1;
                let _ = append_audit(root, actor, "repair", &mismatch.path, 0).await;
            } else {
                report.failed += 1;
                report
                    .errors
                    .push(format!("{}: cannot rewrite pointer", mismatch.path));
            }
            continue;
        }
        let client = moodle.clone();
        let dest = root.join(&mismatch.path);
        let part = {
            let mut owned = dest.as_os_str().to_owned();
            owned.push(".part");
            PathBuf::from(owned)
        };
        let url = mismatch.url.clone();
        let path = mismatch.path.clone();
        let permit_sem = Arc::clone(&sem);
        handles.push(tokio::spawn(async move {
            let _permit = permit_sem.acquire_owned().await.ok();
            if dest.parent().is_some_and(|p| !p.as_os_str().is_empty()) {
                if let Some(parent) = dest.parent() {
                    if tokio::fs::create_dir_all(parent).await.is_err() {
                        return (path, Err("cannot create module dir".to_string()));
                    }
                }
            }
            match client.download_resumable(&url, &part).await {
                Ok(_) => match tokio::fs::rename(&part, &dest).await {
                    Ok(()) => (path, Ok(())),
                    Err(_) => {
                        let _ = tokio::fs::remove_file(&part).await;
                        (path, Err("cannot finalize repair".to_string()))
                    }
                },
                Err(e) => {
                    let _ = tokio::fs::remove_file(&part).await;
                    (path, Err(e.to_string()))
                }
            }
        }));
    }
    for handle in handles {
        match handle.await {
            Ok((path, Ok(()))) => {
                report.fixed += 1;
                let _ = append_audit(root, actor, "repair", &path, 0).await;
            }
            Ok((path, Err(message))) => {
                report.failed += 1;
                report
                    .errors
                    .push(redact_token(&format!("{path}: {message}")));
            }
            Err(join_err) => {
                report.failed += 1;
                report
                    .errors
                    .push(redact_token(&format!("repair task failed: {join_err}")));
            }
        }
    }
    report
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

    #[test]
    fn gc_preview_only_lists_vanished() {
        use crate::state::FileEntry;
        let mut files = BTreeMap::new();
        for (url, path) in [
            ("u1", "C/RA01/a.pdf"),
            ("u2", "C/RA01/b.pdf"),
            ("u3", "C/RA02/c.pdf"),
        ] {
            files.insert(
                url.to_string(),
                FileEntry {
                    path: path.to_string(),
                    sha256: "x".to_string(),
                    size: 1,
                    ..Default::default()
                },
            );
        }
        let active: HashSet<String> = ["u1".to_string(), "u3".to_string()].into_iter().collect();
        let got = preview(&files, &active);
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].url, "u2");
    }
}
