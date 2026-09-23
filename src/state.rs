//! Locked atomic state, schema v2 (Task 4, P3 state/config/store).
//!
//! Schema v2 changes since v1 (review finding I2, plan #951–1000):
//! - `synced_at` is ISO8601 (`2023-11-14T22:13:20Z`); v1 epoch seconds
//!   (`"1700000000"`) migrate on load with the original bytes kept as
//!   `.moodle/state.json.bak.<unix_ts>` (5 newest kept).
//! - `FileEntry` gains `timemodified: i64`, `mimetype: String` and
//!   `source: String` (`"pluginfile"` | `"external"`), all
//!   `#[serde(default)]` so old states backfill without a data migration.
//! - New `meta` block records the owning `root` (multi-root guard, #968)
//!   and the `store` layout choice (`"flat"` default, #1000).
//!
//! Concurrency: writers take an exclusive `fs2` lock on
//! `.moodle/state.lock`; readers take a shared lock. The lock gives mutual
//! exclusion (no torn writes), not optimistic-concurrency control — two
//! read-modify-write cycles can still overwrite each other, so callers
//! should load, mutate and save promptly. Disk writes are atomic
//! (tmp + `fsync` file + `fsync` dir + rename), `0600`, with pretty JSON
//! and sorted keys for stable diffs (#959, #986, #1069).
//!
//! Time formatting mirrors `sync::gc::unix_to_iso8601` deliberately instead
//! of calling it: `state` is the bottom layer (`sync` depends on `state`,
//! never the reverse), so the ~15-line civil-date conversion is duplicated
//! here with the same known-value unit tests on both sides.

use anyhow::{Context, Result};
use fs2::FileExt as _;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

/// Current on-disk schema version.
pub const SCHEMA_VERSION: u32 = 2;

/// Rotating migration/repair backups kept in `.moodle/` (#961).
pub const MAX_BACKUPS: usize = 5;

/// `FileEntry.source` for Moodle-hosted downloads.
pub const SOURCE_PLUGINFILE: &str = "pluginfile";
/// `FileEntry.source` for external `.url` pointer entries.
pub const SOURCE_EXTERNAL: &str = "external";

/// Serde default for `schema_version` when the field is absent: absent
/// means "written before versions existed", i.e. v1, so load migrates it.
/// (Fresh in-memory states use [`SCHEMA_VERSION`] via the manual
/// [`Default`] impl below.)
fn default_schema_v1() -> u32 {
    1
}

fn default_store_layout() -> String {
    "flat".to_string()
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct State {
    // Alphabetical field order: serde emits struct fields in declaration
    // order, so this keeps state.json keys sorted for stable diffs (#986).
    // (BTreeMap keys sort themselves.)
    #[serde(default)]
    pub courses: BTreeMap<String, CourseState>,
    /// Owning root + store choice (#968, #1000). Absent in v1 files.
    #[serde(default)]
    pub meta: StateMeta,
    #[serde(default = "default_schema_v1")]
    pub schema_version: u32,
    #[serde(default)]
    pub user_id: i64,
}

impl Default for State {
    fn default() -> Self {
        Self {
            schema_version: SCHEMA_VERSION,
            user_id: 0,
            courses: BTreeMap::new(),
            meta: StateMeta::default(),
        }
    }
}

/// Extra binding (I2 fix): the three v2 fields are all `#[serde(default)]`
/// so v1 states backfill `timemodified = 0`, `mimetype = ""`, `source = ""`
/// (legacy-unknown) with no data migration beyond the version bump.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct FileEntry {
    // Alphabetical field order (see State): sorted keys on disk (#986).
    /// MIME type (`""` = unknown, backfilled).
    #[serde(default)]
    pub mimetype: String,
    pub path: String,
    pub sha256: String,
    pub size: i64,
    /// `"pluginfile"` | `"external"` (`""` = legacy-unknown, backfilled).
    #[serde(default)]
    pub source: String,
    /// Unix seconds from contents (`0` = unknown, backfilled).
    #[serde(default)]
    pub timemodified: i64,
}

impl FileEntry {
    /// Post-v2 constructor; prefer it over struct literals at new call sites
    /// so future fields stay centralised here.
    #[must_use]
    pub fn new(
        path: String,
        sha256: String,
        size: i64,
        timemodified: i64,
        mimetype: String,
        source: &str,
    ) -> Self {
        Self {
            path,
            sha256,
            size,
            timemodified,
            mimetype,
            source: source.to_string(),
        }
    }
}

/// State-level metadata: multi-root guard + store choice (#968, #1000).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StateMeta {
    /// Canonical root this state was written for. Empty = pre-guard file.
    #[serde(default)]
    pub root: String,
    /// Blob-store layout: `"flat"` (default, #999) or `"content-addressed"`.
    #[serde(default = "default_store_layout")]
    pub store: String,
}

impl Default for StateMeta {
    fn default() -> Self {
        Self {
            root: String::new(),
            store: default_store_layout(),
        }
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct CourseState {
    // Alphabetical field order (see State): sorted keys on disk (#986).
    #[serde(default)]
    pub files: BTreeMap<String, FileEntry>,
    #[serde(default)]
    pub fullname: String,
    #[serde(default)]
    pub sections: BTreeMap<String, String>,
    #[serde(default)]
    pub shortname: String,
    /// ISO8601 in v2; v1 epoch seconds migrate on load (#951).
    #[serde(default)]
    pub synced_at: String,
}

/// `$ROOT/.moodle/state.json`.
#[must_use]
pub fn state_path(root: &str) -> PathBuf {
    Path::new(root).join(".moodle/state.json")
}

fn moodle_dir(root: &str) -> PathBuf {
    Path::new(root).join(".moodle")
}

fn lock_path(root: &str) -> PathBuf {
    moodle_dir(root).join("state.lock")
}

/// Seconds since the unix epoch (0 on clock failure — never panics).
fn unix_secs_now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
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
/// Mirrors `sync::gc::unix_to_iso8601` (see module docs for why).
fn unix_to_iso8601(secs: u64) -> String {
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

/// True for v1 `synced_at` values: non-empty all-digit epoch seconds.
fn is_epoch_synced_at(value: &str) -> bool {
    !value.is_empty() && value.bytes().all(|b| b.is_ascii_digit())
}

/// `true` for the exact shape this crate writes (`YYYY-MM-DDTHH:MM:SSZ`).
fn looks_like_iso8601(value: &str) -> bool {
    value.len() == 20 && value.ends_with('Z') && value.as_bytes().get(10) == Some(&b'T')
}

/// Relative, non-escaping entry path (`..` and absolute rejected, #985).
fn path_is_jailed(path: &str) -> bool {
    !Path::new(path).is_absolute() && !path.split('/').any(|seg| seg == "..")
}

/// Best-effort canonical root for the multi-root guard; `None` when the
/// root does not exist yet (fresh machine — guard skipped, not failed).
fn canonical_root(root: &str) -> Option<String> {
    std::fs::canonicalize(root)
        .ok()
        .map(|p| p.display().to_string())
}

/// Newest migration/repair backups first (by mtime, then name).
fn list_backups(dir: &Path) -> Vec<PathBuf> {
    let mut backups: Vec<(std::time::SystemTime, PathBuf)> = Vec::new();
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    for entry in entries.filter_map(|e| e.ok()) {
        let name = entry.file_name().to_string_lossy().into_owned();
        if name.starts_with("state.json.bak.") {
            let mtime = entry
                .metadata()
                .and_then(|m| m.modified())
                .unwrap_or(std::time::UNIX_EPOCH);
            backups.push((mtime, entry.path()));
        }
    }
    backups.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| b.1.cmp(&a.1)));
    backups.into_iter().map(|(_, p)| p).collect()
}

/// Write `raw` as `state.json.bak.<unix_ts>` (0600) and rotate to the
/// newest [`MAX_BACKUPS`]. Best-effort rotation: a failed removal never
/// fails the backup itself.
fn write_backup(dir: &Path, raw: &[u8]) -> Result<PathBuf> {
    let dest = dir.join(format!("state.json.bak.{}", unix_secs_now()));
    std::fs::write(&dest, raw).with_context(|| format!("writing backup {}", dest.display()))?;
    set_private(&dest)?;
    rotate_backups(dir);
    Ok(dest)
}

fn rotate_backups(dir: &Path) {
    for stale in list_backups(dir).into_iter().skip(MAX_BACKUPS) {
        let _ = std::fs::remove_file(stale);
    }
}

#[cfg(unix)]
fn set_private(path: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt as _;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))
        .with_context(|| format!("chmod 600 {}", path.display()))
}

#[cfg(not(unix))]
fn set_private(_path: &Path) -> Result<()> {
    Ok(())
}

/// Open (creating) the lockfile. Success also proves root writability
/// (#970); failure maps to an actionable message, never a raw OS error.
fn open_lock(root: &str) -> Result<std::fs::File> {
    let dir = moodle_dir(root);
    std::fs::create_dir_all(&dir)
        .with_context(|| format!("creating {} (is the root writable?)", dir.display()))?;
    std::fs::OpenOptions::new()
        .create(true)
        .read(true)
        .write(true)
        .truncate(false) // lock file: never truncate existing content
        .open(lock_path(root))
        .with_context(|| {
            format!(
                "opening {} (is the root writable?)",
                lock_path(root).display()
            )
        })
}

/// Parse tolerant JSON: unknown fields are ignored by serde (forward
/// compat, #953). Corrupt files become an error with a restore hint (#967).
fn parse_state(path: &Path, raw: &str, dir: &Path) -> Result<State> {
    serde_json::from_str(raw).map_err(|e| {
        let backups = list_backups(dir);
        let newest = backups
            .first()
            .map(|p| p.display().to_string())
            .unwrap_or_else(|| dir.join("state.json.bak.<ts>").display().to_string());
        anyhow::anyhow!(
            "state file {} is corrupt ({e}). Nothing was overwritten. Restore the newest backup with: cp {} {}",
            path.display(),
            newest,
            path.display(),
        )
    })
}

/// Migrate a parsed v1 state in place. Returns `true` when anything
/// changed (caller persists + backs up). `FileEntry` backfill needs no
/// code: missing fields deserialised to defaults already.
fn migrate_v1_to_v2(state: &mut State) -> bool {
    if state.schema_version >= SCHEMA_VERSION {
        return false;
    }
    for course in state.courses.values_mut() {
        if is_epoch_synced_at(&course.synced_at) {
            if let Ok(secs) = course.synced_at.parse::<u64>() {
                course.synced_at = unix_to_iso8601(secs);
            }
        }
    }
    if state.meta.store.is_empty() {
        state.meta.store = default_store_layout();
    }
    state.schema_version = SCHEMA_VERSION;
    true
}

/// Multi-root guard (#968): a state written for another canonical root is
/// refused rather than silently mixed.
fn check_multi_root(state: &State, root: &str) -> Result<()> {
    if state.meta.root.is_empty() {
        return Ok(());
    }
    if let Some(current) = canonical_root(root) {
        if current != state.meta.root {
            anyhow::bail!(
                "state at {} belongs to root {} (multi-root guard): run with that root, or delete the state to start fresh",
                state_path(root).display(),
                state.meta.root,
            );
        }
    }
    Ok(())
}

/// Fsync a directory so a just-renamed file survives a crash (#959).
fn sync_dir(dir: &Path) {
    if let Ok(file) = std::fs::File::open(dir) {
        let _ = file.sync_all();
    }
}

impl State {
    /// Load, migrating v1 → v2 (epoch `synced_at` → ISO8601) on first sight.
    /// Migration keeps the original bytes as `state.json.bak.<ts>` and
    /// persists the converted state; later loads are no-ops. Missing file
    /// (or missing root) yields a default v2 state.
    pub fn load(root: &str) -> Result<Self> {
        let path = state_path(root);
        if !path.exists() {
            return Ok(State::default());
        }
        let dir = moodle_dir(root);
        let raw = {
            let lock = open_lock(root)?;
            lock.lock_shared().context("locking state for read")?;
            std::fs::read_to_string(&path).with_context(|| format!("reading {}", path.display()))?
        };
        let mut state = parse_state(&path, &raw, &dir)?;
        if migrate_v1_to_v2(&mut state) {
            // Best-effort crash safety around the rewrite: keep the exact
            // pre-migration bytes, then persist through the locked writer.
            // A concurrent migrator may add a second (identical-content)
            // backup; rotation bounds the set. Deterministic content means
            // last-writer-wins is harmless here.
            let _ = write_backup(&dir, raw.as_bytes());
            state.save_locked(root)?;
            return State::load_after_migrate(root);
        }
        check_multi_root(&state, root)?;
        Ok(state)
    }

    /// Re-read after a migration write without re-migrating.
    fn load_after_migrate(root: &str) -> Result<Self> {
        let path = state_path(root);
        let dir = moodle_dir(root);
        let raw = std::fs::read_to_string(&path)
            .with_context(|| format!("re-reading {}", path.display()))?;
        let state: State = parse_state(&path, &raw, &dir)?;
        check_multi_root(&state, root)?;
        Ok(state)
    }

    /// Historic entry point (signature frozen for `src/bin/*` and the sync
    /// engine): delegates to [`State::save_locked`].
    pub fn save(&self, root: &str) -> Result<()> {
        self.save_locked(root)
    }

    /// Locked atomic save: exclusive `fs2` lock, auto-created parents
    /// (#969), tmp + `fsync` (file + dir) + rename (#959), `0600` (#1069),
    /// pretty JSON with sorted keys (#986). Records the canonical root in
    /// `meta.root` for the multi-root guard.
    pub fn save_locked(&self, root: &str) -> Result<()> {
        let root_path = Path::new(root);
        std::fs::create_dir_all(root_path)
            .with_context(|| format!("creating root {root} (is it writable?)"))?;
        let dir = moodle_dir(root);
        std::fs::create_dir_all(&dir)
            .with_context(|| format!("creating {} (is the root writable?)", dir.display()))?;
        let path = state_path(root);
        // Defensive jail: the state file must stay under the canonical
        // root (#985). Both are built from `root` above, so a mismatch
        // means something is seriously wrong — refuse rather than write.
        if let Ok(canonical_root_path) = root_path.canonicalize() {
            if let Ok(canonical_dir) = dir.canonicalize() {
                if !canonical_dir.starts_with(&canonical_root_path) {
                    anyhow::bail!("refusing to save state outside root {root}");
                }
            }
        }

        let lock = open_lock(root)?;
        lock.lock_exclusive()
            .context("locking state for write (another sync running?)")?;

        let snapshot = State {
            schema_version: SCHEMA_VERSION.max(self.schema_version),
            user_id: self.user_id,
            courses: self.courses.clone(),
            meta: StateMeta {
                root: canonical_root(root).unwrap_or_default(),
                store: if self.meta.store.is_empty() {
                    default_store_layout()
                } else {
                    self.meta.store.clone()
                },
            },
        };
        let text = serde_json::to_string_pretty(&snapshot).context("serialising state")?;
        let tmp = dir.join("state.json.tmp");
        std::fs::write(&tmp, text.as_bytes())
            .with_context(|| format!("writing {}", tmp.display()))?;
        set_private(&tmp)?;
        let file = std::fs::File::open(&tmp).with_context(|| format!("fsync {}", tmp.display()))?;
        file.sync_all()
            .with_context(|| format!("fsync {}", tmp.display()))?;
        drop(file);
        std::fs::rename(&tmp, &path).with_context(|| format!("publishing {}", path.display()))?;
        set_private(&path)?;
        sync_dir(&dir);
        Ok(())
    }

    /// Schema check (#957): structural issues as human-readable strings
    /// (`"error: …"` / `"warn: …"`). Empty means valid. Pure — no I/O, so
    /// migrated states must validate clean.
    #[must_use]
    pub fn validate(&self) -> Vec<String> {
        let mut issues = Vec::new();
        if self.schema_version != SCHEMA_VERSION {
            issues.push(format!(
                "error: schema_version {} (expected {SCHEMA_VERSION}; load migrates automatically)",
                self.schema_version
            ));
        }
        if self.user_id == 0 {
            issues
                .push("warn: user_id is 0 (never persisted; re-run sync to record it)".to_string());
        }
        if !self.meta.store.is_empty()
            && self.meta.store != "flat"
            && self.meta.store != "content-addressed"
        {
            issues.push(format!(
                "error: meta.store {:?} (expected \"flat\" or \"content-addressed\")",
                self.meta.store
            ));
        }
        for (id, course) in &self.courses {
            if id.parse::<i64>().is_err() {
                issues.push(format!(
                    "warn: course key {id:?} is not a numeric id (courses are keyed by string id)"
                ));
            }
            if !course.synced_at.is_empty() && !looks_like_iso8601(&course.synced_at) {
                issues.push(format!(
                    "error: course {id} synced_at {:?} is not ISO8601 (load migrates epoch values)",
                    course.synced_at
                ));
            }
            for (url, entry) in &course.files {
                if entry.path.is_empty() {
                    issues.push(format!("error: course {id} url {url}: empty path"));
                } else if !path_is_jailed(&entry.path) {
                    issues.push(format!(
                        "error: course {id} url {url}: path {:?} escapes the root",
                        entry.path
                    ));
                }
                if entry.size < 0 {
                    issues.push(format!(
                        "error: course {id} url {url}: negative size {}",
                        entry.size
                    ));
                }
                if entry.sha256.is_empty() {
                    issues.push(format!("warn: course {id} url {url}: empty sha256"));
                }
                if entry.timemodified < 0 {
                    issues.push(format!(
                        "warn: course {id} url {url}: negative timemodified {}",
                        entry.timemodified
                    ));
                }
                if !entry.source.is_empty()
                    && entry.source != SOURCE_PLUGINFILE
                    && entry.source != SOURCE_EXTERNAL
                {
                    issues.push(format!(
                        "error: course {id} url {url}: source {:?} (expected \"pluginfile\" or \"external\")",
                        entry.source
                    ));
                }
            }
        }
        issues
    }

    /// Human-readable changes from `self` (old) to `other` (new), one line
    /// per change, deterministic order (#954). Empty means no changes.
    #[must_use]
    pub fn diff(&self, other: &Self) -> Vec<String> {
        let mut lines = Vec::new();
        if self.user_id != other.user_id {
            lines.push(format!("user_id: {} -> {}", self.user_id, other.user_id));
        }
        for (id, course) in &other.courses {
            if let Some(prev) = self.courses.get(id) {
                for (url, entry) in &course.files {
                    match prev.files.get(url) {
                        None => lines.push(format!("course {id}: + {}", entry.path)),
                        Some(old)
                            if old.path != entry.path
                                || old.sha256 != entry.sha256
                                || old.size != entry.size
                                || old.timemodified != entry.timemodified
                                || old.mimetype != entry.mimetype
                                || old.source != entry.source =>
                        {
                            let mut fields = Vec::new();
                            if old.path != entry.path {
                                fields.push(format!("path {:?} -> {:?}", old.path, entry.path));
                            }
                            if old.sha256 != entry.sha256 {
                                fields.push("sha256".to_string());
                            }
                            if old.size != entry.size {
                                fields.push(format!("size {} -> {}", old.size, entry.size));
                            }
                            if old.timemodified != entry.timemodified {
                                fields.push(format!(
                                    "timemodified {} -> {}",
                                    old.timemodified, entry.timemodified
                                ));
                            }
                            if old.mimetype != entry.mimetype {
                                fields.push(format!(
                                    "mimetype {:?} -> {:?}",
                                    old.mimetype, entry.mimetype
                                ));
                            }
                            if old.source != entry.source {
                                fields
                                    .push(format!("source {:?} -> {:?}", old.source, entry.source));
                            }
                            lines.push(format!(
                                "course {id}: ~ {} ({})",
                                entry.path,
                                fields.join(", ")
                            ));
                        }
                        Some(_) => {}
                    }
                }
                for (url, entry) in &prev.files {
                    if !course.files.contains_key(url) {
                        lines.push(format!("course {id}: - {}", entry.path));
                    }
                }
                if prev.synced_at != course.synced_at {
                    lines.push(format!(
                        "course {id}: synced_at {:?} -> {:?}",
                        prev.synced_at, course.synced_at
                    ));
                }
            } else {
                lines.push(format!("course {id}: added ({} files)", course.files.len()));
            }
        }
        for (id, course) in &self.courses {
            if !other.courses.contains_key(id) {
                lines.push(format!(
                    "course {id}: removed ({} files)",
                    course.files.len()
                ));
            }
        }
        lines
    }

    /// Rewrite the state compactly through the locked writer (#955):
    /// load (migrating) + save. Returns size before/after for the report.
    pub fn vacuum(root: &str) -> Result<VacuumReport> {
        let path = state_path(root);
        let bytes_before = std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
        let state = State::load(root)?;
        let (courses, files) = (
            state.courses.len(),
            state.courses.values().map(|c| c.files.len()).sum(),
        );
        state.save_locked(root)?;
        let bytes_after = std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
        Ok(VacuumReport {
            courses,
            files,
            bytes_before,
            bytes_after,
        })
    }

    /// Drop entries whose file is missing from disk, plus path-escape
    /// entries that can never resolve inside the root (#956). Backs up
    /// first (destructive), then persists through the locked writer.
    pub fn repair(root: &str) -> Result<RepairSummary> {
        let mut state = State::load(root)?;
        let root_path = Path::new(root);
        let mut summary = RepairSummary::default();
        for course in state.courses.values_mut() {
            let mut kept = BTreeMap::new();
            for (url, entry) in course.files.iter() {
                summary.checked += 1;
                if !path_is_jailed(&entry.path) {
                    summary.dropped_escape += 1;
                    continue;
                }
                if root_path.join(&entry.path).is_file() {
                    kept.insert(url.clone(), entry.clone());
                } else {
                    summary.dropped_missing += 1;
                }
            }
            course.files = kept;
        }
        summary.kept = state.courses.values().map(|c| c.files.len()).sum();
        if summary.dropped_missing > 0 || summary.dropped_escape > 0 {
            let dir = moodle_dir(root);
            let raw = std::fs::read_to_string(state_path(root)).unwrap_or_default();
            let _ = write_backup(&dir, raw.as_bytes());
            state.save_locked(root)?;
        }
        Ok(summary)
    }
}

/// Outcome of [`State::vacuum`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VacuumReport {
    pub courses: usize,
    pub files: usize,
    pub bytes_before: u64,
    pub bytes_after: u64,
}

/// Outcome of [`State::repair`].
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RepairSummary {
    pub checked: usize,
    pub dropped_missing: usize,
    pub dropped_escape: usize,
    pub kept: usize,
}

use crate::moodle::Course;

impl CourseState {
    pub fn upsert(&mut self, c: &Course) {
        self.fullname = c.fullname.clone();
        self.shortname = c.shortname.clone();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn state_iso8601_known_values() {
        assert_eq!(unix_to_iso8601(0), "1970-01-01T00:00:00Z");
        assert_eq!(unix_to_iso8601(1_445_412_480), "2015-10-21T07:28:00Z");
        assert_eq!(unix_to_iso8601(1_700_000_000), "2023-11-14T22:13:20Z");
    }

    #[test]
    fn state_epoch_detection() {
        assert!(is_epoch_synced_at("1700000000"));
        assert!(is_epoch_synced_at("0"));
        assert!(!is_epoch_synced_at("2023-11-14T22:13:20Z"));
        assert!(!is_epoch_synced_at(""));
        assert!(!is_epoch_synced_at("1700000000 "));
        assert!(looks_like_iso8601("2023-11-14T22:13:20Z"));
        assert!(!looks_like_iso8601("1700000000"));
        assert!(!looks_like_iso8601(""));
    }

    #[test]
    fn state_backfill_defaults_for_old_entries() {
        // v1 JSON without the new fields must parse with serde defaults.
        let raw = r#"{"path":"a/b.pdf","sha256":"x","size":3}"#;
        let entry: FileEntry = serde_json::from_str(raw).expect("parse");
        assert_eq!(entry.timemodified, 0);
        assert_eq!(entry.mimetype, "");
        assert_eq!(entry.source, "");
        // Unknown future fields are ignored (forward compat, #953).
        let raw2 = r#"{"path":"a","sha256":"x","size":1,"future_field":true}"#;
        let entry2: FileEntry = serde_json::from_str(raw2).expect("tolerant parse");
        assert_eq!(entry2.path, "a");
    }

    #[test]
    fn state_migrate_v1_epoch_to_iso() {
        let mut state = State {
            schema_version: 1,
            user_id: 7,
            courses: BTreeMap::from([(
                "9".to_string(),
                CourseState {
                    fullname: "C".to_string(),
                    shortname: "C".to_string(),
                    synced_at: "1700000000".to_string(),
                    sections: BTreeMap::new(),
                    files: BTreeMap::from([(
                        "u".to_string(),
                        FileEntry {
                            path: "C/a.pdf".to_string(),
                            sha256: "s".to_string(),
                            size: 1,
                            ..Default::default()
                        },
                    )]),
                },
            )]),
            meta: StateMeta {
                root: String::new(),
                store: String::new(),
            },
        };
        assert!(migrate_v1_to_v2(&mut state));
        assert_eq!(state.schema_version, SCHEMA_VERSION);
        assert_eq!(state.courses["9"].synced_at, "2023-11-14T22:13:20Z");
        assert_eq!(state.meta.store, "flat");
        assert!(!migrate_v1_to_v2(&mut state), "second pass is a no-op");
    }

    #[test]
    fn state_diff_lines() {
        let mut old = State::default();
        old.courses.insert(
            "1".to_string(),
            CourseState {
                fullname: "C".to_string(),
                shortname: "C".to_string(),
                synced_at: "2023-11-14T22:13:20Z".to_string(),
                sections: BTreeMap::new(),
                files: BTreeMap::from([
                    (
                        "keep".to_string(),
                        FileEntry::new("C/k.pdf".into(), "s".into(), 1, 0, String::new(), ""),
                    ),
                    (
                        "gone".to_string(),
                        FileEntry::new("C/g.pdf".into(), "s".into(), 1, 0, String::new(), ""),
                    ),
                    (
                        "chg".to_string(),
                        FileEntry::new("C/c.pdf".into(), "s".into(), 1, 0, String::new(), ""),
                    ),
                ]),
            },
        );
        let mut new = old.clone();
        new.user_id = 5;
        let cs = new.courses.get_mut("1").expect("course");
        cs.files.remove("gone");
        cs.files.insert(
            "fresh".to_string(),
            FileEntry::new("C/f.pdf".into(), "s".into(), 2, 0, String::new(), ""),
        );
        cs.files.insert(
            "chg".to_string(),
            FileEntry::new("C/c.pdf".into(), "s".into(), 9, 0, String::new(), ""),
        );
        let lines = old.diff(&new);
        assert!(lines.iter().any(|l| l == "user_id: 0 -> 5"), "{lines:?}");
        assert!(
            lines.iter().any(|l| l == "course 1: + C/f.pdf"),
            "{lines:?}"
        );
        assert!(
            lines.iter().any(|l| l == "course 1: - C/g.pdf"),
            "{lines:?}"
        );
        assert!(
            lines
                .iter()
                .any(|l| l.contains('~') && l.contains("size 1 -> 9")),
            "{lines:?}"
        );
    }

    #[test]
    fn state_validate_clean_and_flags() {
        let ok = State::default();
        let issues = ok.validate();
        // Fresh default only warns about user_id 0.
        assert_eq!(issues.len(), 1, "{issues:?}");
        assert!(issues[0].starts_with("warn:"));

        let mut bad = State {
            schema_version: 1,
            user_id: 3,
            ..Default::default()
        };
        bad.meta.store = "tape".to_string();
        bad.courses.insert(
            "abc".to_string(),
            CourseState {
                fullname: String::new(),
                shortname: String::new(),
                synced_at: "1700000000".to_string(),
                sections: BTreeMap::new(),
                files: BTreeMap::from([(
                    "u".to_string(),
                    FileEntry {
                        path: "../evil".to_string(),
                        sha256: String::new(),
                        size: -1,
                        timemodified: -5,
                        mimetype: String::new(),
                        source: "ftp".to_string(),
                    },
                )]),
            },
        );
        let issues = bad.validate();
        let joined = issues.join("\n");
        for needle in [
            "schema_version",
            "meta.store",
            "not a numeric id",
            "not ISO8601",
            "escapes the root",
            "negative size",
            "empty sha256",
            "negative timemodified",
            "source",
        ] {
            assert!(joined.contains(needle), "missing {needle}: {joined}");
        }
    }
}
