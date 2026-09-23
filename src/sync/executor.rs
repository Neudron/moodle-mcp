//! Concurrent download executor (Task 3, P2 sync engine).
//!
//! Binding for Tasks 4–5: `executor::run(plan, sem(8), governor) -> Report`.
//! Concretely: [`run`] takes the [`Plan`][crate::sync::planner::Plan], a
//! [`RunContext`], [`RunOptions`] (concurrency defaults to 8, clamped
//! 1–32; optional `--max-mbps` byte cap) and the previous file map for
//! reuse/move bookkeeping. It returns the full [`Report`] plus the new
//! `url → FileEntry` map the caller stores.
//!
//! Sharing rule: exactly ONE [`Moodle`][crate::moodle::Moodle] client is
//! used for the whole course — every file task clones it, and clones share
//! the same rate governor + circuit breaker (clone-group). No task builds
//! its own client. Downloads stream through
//! [`Moodle::download_resumable`][crate::moodle::Moodle::download_resumable]
//! into `.part` files (Range resume) followed by an atomic rename.
//! 404 is a row error (sync continues); 401/403 aborts the remaining queue.

use std::collections::{BTreeMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::{
    atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering},
    Arc,
};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use tokio::sync::{Mutex, Semaphore};

use crate::errors::redact_token;
use crate::moodle::Moodle;
use crate::state::FileEntry;
use crate::sync::indexer;
use crate::sync::planner::{FileMove, Plan, PlannedFile};
use crate::sync::report::Report;

/// Default (and plan-mandated) per-course file concurrency.
pub const DEFAULT_CONCURRENCY: usize = 8;
const MIN_CONCURRENCY: usize = 1;
const MAX_CONCURRENCY: usize = 32;

/// Knobs for [`run`].
#[derive(Debug, Clone)]
pub struct RunOptions {
    /// File-task concurrency (clamped 1–32, default 8).
    pub concurrency: usize,
    /// Global byte-rate cap in Mbit/s (`--max-mbps`). `None` = uncapped.
    pub max_mbps: Option<f64>,
}

impl Default for RunOptions {
    fn default() -> Self {
        Self {
            concurrency: DEFAULT_CONCURRENCY,
            max_mbps: None,
        }
    }
}

impl RunOptions {
    #[must_use]
    pub fn concurrency_clamped(&self) -> usize {
        self.concurrency.clamp(MIN_CONCURRENCY, MAX_CONCURRENCY)
    }

    #[must_use]
    pub fn max_bytes_per_sec(&self) -> Option<f64> {
        self.max_mbps
            .filter(|m| m.is_finite() && *m > 0.0)
            .map(|m| m * 1_000_000.0 / 8.0)
    }
}

/// Fixed per-run context (paths + course identity).
#[derive(Debug, Clone)]
pub struct RunContext {
    pub course_id: i64,
    pub shortname: String,
    pub course_dir: PathBuf,
    pub root: PathBuf,
}

/// Global byte-rate cap shared by all file tasks of one run.
struct ByteCap {
    max_bps: Option<f64>,
    state: Mutex<(Instant, u64)>,
}

impl ByteCap {
    fn new(max_bps: Option<f64>) -> Self {
        Self {
            max_bps,
            state: Mutex::new((Instant::now(), 0)),
        }
    }

    async fn consume(&self, n: u64) {
        let Some(max) = self.max_bps else {
            return;
        };
        if max <= 0.0 || n == 0 {
            return;
        }
        loop {
            let wait = {
                let mut guard = self.state.lock().await;
                let now = Instant::now();
                if now.duration_since(guard.0) >= Duration::from_secs(1) {
                    *guard = (now, n.min(max as u64));
                    None
                } else if (guard.1 as f64) + (n as f64) <= max {
                    guard.1 += n;
                    None
                } else {
                    Some((guard.0 + Duration::from_secs(1)).saturating_duration_since(now))
                }
            };
            match wait {
                Some(d) => tokio::time::sleep(d).await,
                None => return,
            }
        }
    }
}

fn system_secs(time: SystemTime) -> i64 {
    time.duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

fn sha256_hex(bytes: &[u8]) -> String {
    use sha2::Digest as _;
    format!("{:x}", sha2::Sha256::digest(bytes))
}

async fn path_is_file(path: &Path) -> bool {
    tokio::fs::metadata(path)
        .await
        .map(|m| m.is_file())
        .unwrap_or(false)
}

/// Disk mtime in unix seconds, or 0 when unavailable (caller treats 0 as
/// “unknown → keep the reuse”).
async fn disk_mtime_secs(path: &Path) -> i64 {
    let modified = tokio::fs::metadata(path)
        .await
        .ok()
        .and_then(|m| m.modified().ok());
    modified.map(system_secs).unwrap_or(0)
}

fn part_for(dest: &Path) -> PathBuf {
    let mut owned = dest.as_os_str().to_owned();
    owned.push(".part");
    PathBuf::from(owned)
}

/// Suffix `dest` while an *untracked* file occupies it (historic
/// `unique_path` safety for user-dropped files). Tracked destinations
/// (previous sync output) are overwritten in place.
async fn unique_dest_on_disk(
    dest_abs: &Path,
    dest_rel: &str,
    tracked: &HashSet<String>,
) -> (PathBuf, String) {
    if tracked.contains(dest_rel) {
        return (dest_abs.to_path_buf(), dest_rel.to_string());
    }
    let exists = tokio::fs::try_exists(dest_abs).await.unwrap_or(false);
    if !exists {
        return (dest_abs.to_path_buf(), dest_rel.to_string());
    }
    let (dir, leaf) = match dest_rel.rsplit_once('/') {
        Some((d, l)) => (d.to_string(), l.to_string()),
        None => (String::new(), dest_rel.to_string()),
    };
    let (stem, ext) = {
        let p = Path::new(&leaf);
        let stem = p
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or("file")
            .to_string();
        let ext = p
            .extension()
            .and_then(|s| s.to_str())
            .map(|e| format!(".{e}"))
            .unwrap_or_default();
        (stem, ext)
    };
    let parent = dest_abs.parent().map(Path::to_path_buf);
    let mut n = 2u32;
    loop {
        let candidate_leaf = format!("{stem}-{n}{ext}");
        let candidate_rel = if dir.is_empty() {
            candidate_leaf.clone()
        } else {
            format!("{dir}/{candidate_leaf}")
        };
        if tracked.contains(&candidate_rel) {
            n += 1;
            continue;
        }
        // Re-root under the original parent (dest_abs is already rooted).
        let rooted = match &parent {
            Some(p) => p.join(&candidate_leaf),
            None => PathBuf::from(&candidate_leaf),
        };
        let taken = tokio::fs::try_exists(&rooted).await.unwrap_or(true);
        if !taken {
            return (rooted, candidate_rel);
        }
        n += 1;
        if n > 9999 {
            // Practically unreachable; fall back to a hash leaf.
            let hash: String = sha256_hex(dest_rel.as_bytes()).chars().take(8).collect();
            let leaf = format!("{stem}-{hash}{ext}");
            let rel = if dir.is_empty() {
                leaf.clone()
            } else {
                format!("{dir}/{leaf}")
            };
            let abs = match dest_abs.parent() {
                Some(parent) => parent.join(&leaf),
                None => PathBuf::from(&leaf),
            };
            return (abs, rel);
        }
    }
}

enum TaskOutcome {
    New {
        url: String,
        entry: FileEntry,
        bytes: u64,
    },
    Skipped {
        url: String,
        entry: FileEntry,
    },
    Failed(String),
    Aborted,
}

async fn write_external(
    file: &PlannedFile,
    root: &Path,
    old_files: &BTreeMap<String, FileEntry>,
) -> TaskOutcome {
    let dest_abs = root.join(&file.dest_rel);
    let body = format!("[InternetShortcut]\nURL={}\n", file.url);
    if let Some(parent) = dest_abs.parent() {
        if tokio::fs::create_dir_all(parent).await.is_err() {
            return TaskOutcome::Failed(format!("{}: cannot create module dir", file.filename));
        }
    }
    let reuse = tokio::fs::read_to_string(&dest_abs)
        .await
        .map(|current| current == body)
        .unwrap_or(false);
    if reuse {
        if let Some(prev) = old_files.get(&file.url) {
            let mut entry = prev.clone();
            entry.path = file.dest_rel.clone();
            return TaskOutcome::Skipped {
                url: file.url.clone(),
                entry,
            };
        }
    }
    if tokio::fs::write(&dest_abs, body.as_bytes()).await.is_err() {
        return TaskOutcome::Failed(format!("{}: cannot write pointer file", file.filename));
    }
    // Companion `.md` pointer (Drive/YouTube annotated). Regenerated every
    // sync; only the `.url` is tracked in state.
    if let Some(md_rel) = indexer::pointer_md_rel(&file.dest_rel) {
        let md_abs = root.join(&md_rel);
        let md = indexer::external_pointer_md(&file.filename, &file.url);
        let _ = tokio::fs::write(&md_abs, md.as_bytes()).await;
    }
    TaskOutcome::New {
        url: file.url.clone(),
        entry: FileEntry {
            path: file.dest_rel.clone(),
            sha256: sha256_hex(body.as_bytes()),
            size: body.len() as i64,
            // State v2 (Task 4): externals carry contents metadata + source.
            timemodified: file.timemodified,
            mimetype: file.mimetype.clone(),
            source: crate::state::SOURCE_EXTERNAL.to_string(),
        },
        bytes: body.len() as u64,
    }
}

async fn download_one(
    client: Moodle,
    file: PlannedFile,
    dest_abs: PathBuf,
    dest_rel: String,
    sem: Arc<Semaphore>,
    cap: Arc<ByteCap>,
    abort: Arc<AtomicBool>,
) -> TaskOutcome {
    // Hold the permit for the whole download (binding: sem(8)).
    let _permit = sem.acquire_owned().await.ok();
    if abort.load(Ordering::SeqCst) {
        return TaskOutcome::Aborted;
    }
    if let Some(parent) = dest_abs.parent() {
        if tokio::fs::create_dir_all(parent).await.is_err() {
            return TaskOutcome::Failed(format!("{}: cannot create module dir", file.filename));
        }
    }
    let part = part_for(&dest_abs);
    match client.download_resumable(&file.url, &part).await {
        Ok(meta) => {
            cap.consume(meta.size).await;
            if tokio::fs::rename(&part, &dest_abs).await.is_err() {
                let _ = tokio::fs::remove_file(&part).await;
                return TaskOutcome::Failed(format!("{}: cannot finalize download", file.filename));
            }
            TaskOutcome::New {
                url: file.url.clone(),
                entry: FileEntry {
                    path: dest_rel,
                    sha256: meta.sha256,
                    size: meta.size as i64,
                    // State v2 (Task 4): verified download metadata + source.
                    timemodified: meta.timemodified,
                    mimetype: meta.mimetype,
                    source: if file.is_external {
                        crate::state::SOURCE_EXTERNAL.to_string()
                    } else {
                        crate::state::SOURCE_PLUGINFILE.to_string()
                    },
                },
                bytes: meta.size,
            }
        }
        Err(e) if matches!(e, crate::errors::CoreError::Auth(_)) => {
            let _ = tokio::fs::remove_file(&part).await;
            abort.store(true, Ordering::SeqCst);
            TaskOutcome::Failed(format!("{}: {e}", redact_token(&file.filename)))
        }
        Err(e) => {
            let _ = tokio::fs::remove_file(&part).await;
            TaskOutcome::Failed(format!("{}: {e}", redact_token(&file.filename)))
        }
    }
}

/// Execute a [`Plan`][crate::sync::planner::Plan] with `sem(8)` file
/// concurrency over the shared client governor. Returns the [`Report`]
/// plus the new `url → FileEntry` map for the caller to persist.
pub async fn run(
    moodle: &Moodle,
    plan: &Plan,
    ctx: &RunContext,
    opts: &RunOptions,
    old_files: &BTreeMap<String, FileEntry>,
) -> (Report, BTreeMap<String, FileEntry>) {
    let started = Instant::now();
    let mut report = Report::empty(ctx.course_id, &ctx.shortname);
    let mut new_files: BTreeMap<String, FileEntry> = BTreeMap::new();
    let tracked: HashSet<String> = old_files.values().map(|e| e.path.clone()).collect();

    let files_new = Arc::new(AtomicUsize::new(0));
    let files_skipped = Arc::new(AtomicUsize::new(0));
    let bytes_new = Arc::new(AtomicU64::new(0));
    let errors = Arc::new(Mutex::new(Vec::<String>::new()));
    let collected = Arc::new(Mutex::new(BTreeMap::<String, FileEntry>::new()));
    let abort = Arc::new(AtomicBool::new(false));

    // Phase 1 — moves: same-URL renames without downloads.
    let mut downloads: Vec<(PlannedFile, PathBuf, String)> = Vec::new();
    for mv in &plan.moves {
        record_move(
            mv,
            &ctx.root,
            old_files,
            &mut new_files,
            &mut report,
            &mut downloads,
        )
        .await;
    }

    // Phase 2 — reuse verification (existence + mtime, no network).
    for file in &plan.reuse {
        if abort.load(Ordering::SeqCst) {
            break;
        }
        let dest_abs = ctx.root.join(&file.dest_rel);
        if !path_is_file(&dest_abs).await {
            downloads.push((file.clone(), dest_abs, file.dest_rel.clone()));
            continue;
        }
        if file.timemodified != 0 {
            let mtime = disk_mtime_secs(&dest_abs).await;
            if mtime != 0 && mtime < file.timemodified {
                downloads.push((file.clone(), dest_abs, file.dest_rel.clone()));
                continue;
            }
        }
        if let Some(prev) = old_files.get(&file.url) {
            let mut entry = prev.clone();
            entry.path = file.dest_rel.clone();
            new_files.insert(file.url.clone(), entry);
            report.files_skipped += 1;
        } else {
            downloads.push((file.clone(), dest_abs, file.dest_rel.clone()));
        }
    }

    // Phase 3a — externals first (local pointer writes, no governor use).
    for file in &plan.fetch {
        if file.is_external {
            match write_external(file, &ctx.root, old_files).await {
                TaskOutcome::New { url, entry, bytes } => {
                    new_files.insert(url, entry);
                    report.files_new += 1;
                    report.bytes_new += bytes;
                }
                TaskOutcome::Skipped { url, entry } => {
                    new_files.insert(url, entry);
                    report.files_skipped += 1;
                }
                TaskOutcome::Failed(message) => {
                    report.errors.push(redact_token(&message));
                }
                TaskOutcome::Aborted => {}
            }
        }
    }

    // Phase 3b — pluginfile downloads, 8-way over the shared governor.
    let sem = Arc::new(Semaphore::new(opts.concurrency_clamped()));
    let cap = Arc::new(ByteCap::new(opts.max_bytes_per_sec()));
    let mut handles = Vec::new();
    for file in &plan.fetch {
        if file.is_external || abort.load(Ordering::SeqCst) {
            continue;
        }
        let dest_abs = ctx.root.join(&file.dest_rel);
        let (dest_abs, dest_rel) = unique_dest_on_disk(&dest_abs, &file.dest_rel, &tracked).await;
        let client = moodle.clone();
        let task = download_one(
            client,
            file.clone(),
            dest_abs,
            dest_rel,
            Arc::clone(&sem),
            Arc::clone(&cap),
            Arc::clone(&abort),
        );
        handles.push(tokio::spawn(task));
    }
    // Reuse fallbacks that missed verification join the same queue.
    let mut fallback_tasks = Vec::new();
    for (file, dest_abs, dest_rel) in downloads {
        if file.is_external {
            match write_external(&file, &ctx.root, old_files).await {
                TaskOutcome::New { url, entry, bytes } => {
                    new_files.insert(url, entry);
                    report.files_new += 1;
                    report.bytes_new += bytes;
                }
                TaskOutcome::Skipped { url, entry } => {
                    new_files.insert(url, entry);
                    report.files_skipped += 1;
                }
                TaskOutcome::Failed(message) => {
                    report.errors.push(redact_token(&message));
                }
                TaskOutcome::Aborted => {}
            }
            continue;
        }
        if abort.load(Ordering::SeqCst) {
            report
                .errors
                .push(format!("{}: aborted after auth failure", file.filename));
            continue;
        }
        let (dest_abs, dest_rel) = unique_dest_on_disk(&dest_abs, &dest_rel, &tracked).await;
        let client = moodle.clone();
        let task = download_one(
            client,
            file,
            dest_abs,
            dest_rel,
            Arc::clone(&sem),
            Arc::clone(&cap),
            Arc::clone(&abort),
        );
        fallback_tasks.push(tokio::spawn(task));
    }
    handles.extend(fallback_tasks);

    for handle in handles {
        match handle.await {
            Ok(TaskOutcome::New { url, entry, bytes }) => {
                files_new.fetch_add(1, Ordering::SeqCst);
                bytes_new.fetch_add(bytes, Ordering::SeqCst);
                collected.lock().await.insert(url, entry);
            }
            Ok(TaskOutcome::Skipped { url, entry }) => {
                files_skipped.fetch_add(1, Ordering::SeqCst);
                collected.lock().await.insert(url, entry);
            }
            Ok(TaskOutcome::Failed(message)) => {
                errors.lock().await.push(redact_token(&message));
            }
            Ok(TaskOutcome::Aborted) => {}
            Err(join_err) => {
                errors
                    .lock()
                    .await
                    .push(redact_token(&format!("download task failed: {join_err}")));
            }
        }
    }

    report.files_new += files_new.load(Ordering::SeqCst);
    report.files_skipped += files_skipped.load(Ordering::SeqCst);
    report.bytes_new += bytes_new.load(Ordering::SeqCst);
    report.errors.extend(errors.lock().await.drain(..));
    for (url, entry) in collected.lock().await.iter() {
        new_files.insert(url.clone(), entry.clone());
    }
    report.duration_ms = started.elapsed().as_millis().min(u128::from(u64::MAX)) as u64;
    report.next_cursor = None;
    (report, new_files)
}

async fn record_move(
    mv: &FileMove,
    root: &Path,
    old_files: &BTreeMap<String, FileEntry>,
    new_files: &mut BTreeMap<String, FileEntry>,
    report: &mut Report,
    downloads: &mut Vec<(PlannedFile, PathBuf, String)>,
) {
    // Same-URL moves are keyed by URL; cross-URL content moves (planner
    // matched by sha) carry the new URL, so locate the stored entry by its
    // recorded source path instead. Paths are unique per course state.
    let prev = old_files
        .get(&mv.url)
        .or_else(|| old_files.values().find(|entry| entry.path == mv.from));
    let Some(prev) = prev else {
        downloads.push((mv.file.clone(), root.join(&mv.to), mv.to.clone()));
        return;
    };
    let src_abs = root.join(&mv.from);
    let dst_abs = root.join(&mv.to);
    if !path_is_file(&src_abs).await {
        downloads.push((mv.file.clone(), dst_abs, mv.to.clone()));
        return;
    }
    if src_abs == dst_abs {
        let mut entry = prev.clone();
        entry.path = mv.to.clone();
        new_files.insert(mv.url.clone(), entry);
        report.files_skipped += 1;
        return;
    }
    if let Some(parent) = dst_abs.parent() {
        if tokio::fs::create_dir_all(parent).await.is_err() {
            downloads.push((mv.file.clone(), dst_abs, mv.to.clone()));
            return;
        }
    }
    // Case-only renames on case-insensitive filesystems: copy + remove.
    let renamed = tokio::fs::rename(&src_abs, &dst_abs).await.is_ok();
    if renamed {
        let mut entry = prev.clone();
        entry.path = mv.to.clone();
        new_files.insert(mv.url.clone(), entry);
        report.files_skipped += 1;
    } else {
        downloads.push((mv.file.clone(), dst_abs, mv.to.clone()));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn run_options_clamp_and_cap() {
        let low = RunOptions {
            concurrency: 0,
            max_mbps: None,
        };
        assert_eq!(low.concurrency_clamped(), 1);
        let high = RunOptions {
            concurrency: 99,
            max_mbps: None,
        };
        assert_eq!(high.concurrency_clamped(), 32);
        let capped = RunOptions {
            concurrency: 8,
            max_mbps: Some(8.0),
        };
        assert_eq!(capped.max_bytes_per_sec(), Some(1_000_000.0));
        let bad = RunOptions {
            concurrency: 8,
            max_mbps: Some(f64::NAN),
        };
        assert_eq!(bad.max_bytes_per_sec(), None);
    }

    #[test]
    fn part_path_appends_suffix() {
        let dest = Path::new("/root/C/RA01/001-m/a.pdf");
        assert_eq!(
            part_for(dest),
            PathBuf::from("/root/C/RA01/001-m/a.pdf.part")
        );
    }
}
