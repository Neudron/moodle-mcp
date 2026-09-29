//! Sync planner (Task 3, P2 sync engine).
//!
//! Pure function [`plan`] (no I/O, no network): folds Moodle
//! `core_course_get_contents` output + [`SyncFilters`] + per-course
//! [`CourseState`] into a [`Plan`] of `{fetch, reuse, moves, stale}`.
//!
//! Binding for Tasks 4–5: `planner::plan(contents, filters, state) -> Plan`.
//! The extra `course_dir_name` argument carries the sanitized course folder
//! (layout `$MOODLE_ROOT/<shortname>/RAxx/nnn-module/`) so the planner can
//! compute stable `dest_rel` paths (`<shortname>/RAxx/nnn-module/file`)
//! without touching the filesystem.
//!
//! Skip rule (no state-schema change, so no stored mtime): for a known URL,
//! equal `filesize` against the stored entry means *reuse*; the executor
//! then applies the timemodified check against the on-disk mtime
//! (disk newer than `timemodified` → still skipped, else re-download).
//! Unknown `timemodified` (0, e.g. old fixtures) skips the mtime check.
//! Filters never produce `stale`: staleness is computed against the
//! unfiltered URL set, so `--only` views cannot GC the hidden sections.

use std::collections::{BTreeMap, HashSet};

use serde::{Deserialize, Serialize};

use crate::moodle::{CourseSection, ModuleFile};
use crate::ra;
use crate::state::CourseState;

/// Filename conflict strategy (`--conflict suffix|skip|overwrite`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ConflictStrategy {
    /// Append `-2`, `-3`, … (historic behaviour, default).
    #[default]
    Suffix,
    /// Drop later duplicates that map to the same destination.
    Skip,
    /// Let later duplicates overwrite (last wins).
    Overwrite,
}

impl ConflictStrategy {
    #[must_use]
    pub fn parse(s: &str) -> Option<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "suffix" => Some(Self::Suffix),
            "skip" => Some(Self::Skip),
            "overwrite" => Some(Self::Overwrite),
            _ => None,
        }
    }
}

/// Planner filters (`--only/--except/min/max/after/before/mime/module-re`,
/// dry-run). All `None`/empty means “no filtering”.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct SyncFilters {
    /// Keep only these section folders (e.g. `["RA01", "RA03"]`).
    #[serde(default)]
    pub only: Vec<String>,
    /// Glob-exclude on the original filename (e.g. `["*.mp4", "*.zip"]`).
    #[serde(default)]
    pub except: Vec<String>,
    #[serde(default)]
    pub min_size: Option<i64>,
    #[serde(default)]
    pub max_size: Option<i64>,
    /// Keep files with `timemodified >= after` (unix secs). Unknown (0) passes.
    #[serde(default)]
    pub after: Option<i64>,
    /// Keep files with `timemodified <= before` (unix secs). Unknown (0) passes.
    #[serde(default)]
    pub before: Option<i64>,
    /// Mimetype filter: exact (`application/pdf`) or prefix (`video/`).
    #[serde(default)]
    pub mime: Option<String>,
    /// Regex on the module name. Invalid regex matches nothing.
    #[serde(default)]
    pub module_re: Option<String>,
    #[serde(default)]
    pub conflict: ConflictStrategy,
    /// Keep empty modules on disk instead of skipping them.
    #[serde(default)]
    pub keep_empty: bool,
    /// Dry run: compute the plan and serialise it, download nothing.
    #[serde(default)]
    pub dry_run: bool,
}

/// One file the executor must download (or materialise, for externals).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PlannedFile {
    /// Moodle `fileurl` (pluginfile) or external URL (pointer files).
    pub url: String,
    /// Original Moodle filename.
    pub filename: String,
    /// Sanitised, uniquified leaf name on disk.
    pub stored_name: String,
    pub filesize: i64,
    pub timemodified: i64,
    #[serde(default)]
    pub contenthash: String,
    #[serde(default)]
    pub mimetype: String,
    pub section_folder: String,
    pub module_name: String,
    pub module_folder: String,
    /// Path relative to `$MOODLE_ROOT` (`<short>/<sec>/<mod>/<file>`).
    pub dest_rel: String,
    /// `true` for non-pluginfile links (`.url` + `.md` pointers, no download).
    #[serde(default)]
    pub is_external: bool,
}

/// Same-URL path change: rename on disk, no download.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FileMove {
    pub url: String,
    pub from: String,
    pub to: String,
    pub file: PlannedFile,
}

/// State entry whose URL vanished from contents (GC candidate, never auto-deleted).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StaleEntry {
    pub url: String,
    pub path: String,
}

/// Planner outcome. `Serialize`d for `--dry-run` JSON output.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Plan {
    pub fetch: Vec<PlannedFile>,
    pub reuse: Vec<PlannedFile>,
    pub moves: Vec<FileMove>,
    pub stale: Vec<StaleEntry>,
}

impl Plan {
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.fetch.is_empty() && self.reuse.is_empty() && self.moves.is_empty()
    }

    /// Dry-run JSON (pretty). Never fails loudly: serialisation of these
    /// plain structs is infallible in practice; errors surface as `Err`.
    pub fn to_json(&self) -> Result<String, serde_json::Error> {
        serde_json::to_string_pretty(self)
    }
}

/// Section folder with the historic `00-intro` / `99-misc` fallback.
#[must_use]
pub fn section_folder_name(index: usize, name: &str, module_texts: &[&str]) -> String {
    ra::detect(name, module_texts).unwrap_or_else(|| {
        if index == 0 {
            "00-intro".to_string()
        } else {
            "99-misc".to_string()
        }
    })
}

/// `001-name` module folder with collision suffixes (historic behaviour).
#[must_use]
pub fn module_folder(index: usize, name: &str, taken: &mut Vec<String>) -> String {
    let mut base = ra::sanitize(name);
    if base.len() > 60 {
        base = base.chars().take(60).collect();
    }
    let mut candidate = format!("{:03}-{base}", index + 1);
    let mut n = 2;
    while taken.contains(&candidate) {
        candidate = format!("{:03}-{base}-{n}", index + 1);
        n += 1;
    }
    taken.push(candidate.clone());
    candidate
}

fn sha8_hex(input: &str) -> String {
    use sha2::Digest as _;
    let digest = sha2::Sha256::digest(input.as_bytes());
    format!("{digest:x}").chars().take(8).collect()
}

fn split_stem_ext(name: &str) -> (String, String) {
    let path = std::path::Path::new(name);
    let stem = path
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("file")
        .to_string();
    let ext = path
        .extension()
        .and_then(|s| s.to_str())
        .map(|e| format!(".{e}"))
        .unwrap_or_default();
    (stem, ext)
}

/// Shorten over-long relative paths (>200 chars) with a content hash suffix
/// so filesystems with 255-byte limits keep working.
fn shorten_long_path(course_dir: &str, dir_rel: &str, leaf: &str) -> String {
    let full = format!("{course_dir}/{dir_rel}/{leaf}");
    if full.len() <= 200 {
        return full;
    }
    let (stem, ext) = split_stem_ext(leaf);
    let hash = sha8_hex(&full);
    let keep = 200usize
        .saturating_sub(course_dir.len() + dir_rel.len() + hash.len() + ext.len() + 3)
        .max(8)
        .min(stem.chars().count());
    let short_stem: String = stem.chars().take(keep).collect();
    format!("{course_dir}/{dir_rel}/{short_stem}-{hash}{ext}")
}

fn unique_leaf(
    assigned: &mut HashSet<String>,
    dir_key: &str,
    sanitized: &str,
    conflict: ConflictStrategy,
) -> Option<String> {
    let key = format!("{dir_key}/{sanitized}");
    if !assigned.contains(&key) {
        assigned.insert(key);
        return Some(sanitized.to_string());
    }
    match conflict {
        ConflictStrategy::Overwrite => Some(sanitized.to_string()),
        ConflictStrategy::Skip => None,
        ConflictStrategy::Suffix => {
            let (stem, ext) = split_stem_ext(sanitized);
            let mut n = 2usize;
            loop {
                let candidate = format!("{stem}-{n}{ext}");
                let key = format!("{dir_key}/{candidate}");
                if !assigned.contains(&key) {
                    assigned.insert(key);
                    return Some(candidate);
                }
                n += 1;
            }
        }
    }
}

/// Case-insensitive glob with `*` (any run) and `?` (one char).
#[must_use]
pub fn glob_match(pattern: &str, name: &str) -> bool {
    let pat = pattern.to_ascii_lowercase();
    let txt = name.to_ascii_lowercase();
    let (px, nx) = (pat.as_bytes(), txt.as_bytes());
    let (mut p, mut n) = (0usize, 0usize);
    let (mut star, mut mark) = (None::<usize>, 0usize);
    while n < nx.len() {
        if p < px.len() && (px[p] == b'?' || px[p] == nx[n]) {
            p += 1;
            n += 1;
        } else if p < px.len() && px[p] == b'*' {
            star = Some(p);
            mark = n;
            p += 1;
        } else if let Some(s) = star {
            p = s + 1;
            mark += 1;
            n = mark;
        } else {
            return false;
        }
    }
    while p < px.len() && px[p] == b'*' {
        p += 1;
    }
    p == px.len()
}

#[must_use]
fn mime_matches(filter: &str, mimetype: &str) -> bool {
    if mimetype.is_empty() {
        return false;
    }
    if mimetype == filter {
        return true;
    }
    filter.ends_with('/') && mimetype.starts_with(filter)
}

fn file_passes_filters(
    f: &ModuleFile,
    module_name: &str,
    section_folder: &str,
    filters: &SyncFilters,
    module_re: Option<&regex::Regex>,
    module_re_invalid: bool,
) -> bool {
    if module_re_invalid {
        return false;
    }
    if let Some(re) = module_re {
        if !re.is_match(module_name) {
            return false;
        }
    }
    if !filters.only.is_empty() && !filters.only.iter().any(|o| o == section_folder) {
        return false;
    }
    if filters
        .except
        .iter()
        .any(|pat| glob_match(pat, &f.filename))
    {
        return false;
    }
    if let Some(min) = filters.min_size {
        if f.filesize < min {
            return false;
        }
    }
    if let Some(max) = filters.max_size {
        if f.filesize > max {
            return false;
        }
    }
    // Unknown mtime (0) passes date filters: safer to fetch than to skip.
    if f.timemodified != 0 {
        if let Some(after) = filters.after {
            if f.timemodified < after {
                return false;
            }
        }
        if let Some(before) = filters.before {
            if f.timemodified > before {
                return false;
            }
        }
    }
    if let Some(mime) = filters.mime.as_deref() {
        if !mime_matches(mime, &f.mimetype) {
            return false;
        }
    }
    true
}

fn is_pluginfile(url: &str) -> bool {
    url.contains("/pluginfile.php")
}

fn external_leaf(filename: &str) -> String {
    let mut base = ra::sanitize(filename);
    if base.is_empty() {
        base = "link".to_string();
    }
    if base.to_ascii_lowercase().ends_with(".url") {
        base
    } else {
        format!("{base}.url")
    }
}

/// Build the [`Plan`]. See the module docs for the skip/stale contract.
#[must_use]
pub fn plan(
    contents: &[CourseSection],
    filters: &SyncFilters,
    state: &CourseState,
    course_dir_name: &str,
) -> Plan {
    let mut out = Plan::default();
    let module_re = filters
        .module_re
        .as_deref()
        .and_then(|pat| regex::Regex::new(pat).ok());
    let module_re_invalid = filters.module_re.is_some() && module_re.is_none();
    let mut assigned: HashSet<String> = HashSet::new();
    let mut seen: HashSet<String> = HashSet::new();
    // Old destinations by URL for move detection.
    let old: &BTreeMap<String, crate::state::FileEntry> = &state.files;

    for (si, section) in contents.iter().enumerate() {
        let module_texts: Vec<&str> = section
            .modules
            .iter()
            .map(|m| m.name.as_str())
            .chain(
                section
                    .modules
                    .iter()
                    .filter_map(|m| m.description.as_deref()),
            )
            .collect();
        let sec_folder = section_folder_name(si, &section.name, &module_texts);
        let has_content = !section.summary.trim().is_empty()
            || section.modules.iter().any(|m| {
                !m.description.as_deref().unwrap_or("").trim().is_empty()
                    || m.contents
                        .as_ref()
                        .map(|c| {
                            c.iter()
                                .any(|f| f.fileurl.is_some() || !f.filename.is_empty())
                        })
                        .unwrap_or(false)
            });
        if !has_content {
            continue;
        }
        let mut taken: Vec<String> = Vec::new();
        for (mi, module) in section.modules.iter().enumerate() {
            let files = module.contents.clone().unwrap_or_default();
            let has_files = !files.is_empty();
            let has_desc = !module
                .description
                .as_deref()
                .unwrap_or("")
                .trim()
                .is_empty();
            if !has_files && !has_desc {
                continue;
            }
            let mod_folder = module_folder(mi, &module.name, &mut taken);
            let dir_key = format!("{sec_folder}/{mod_folder}");
            for f in &files {
                let Some(url) = f.fileurl.clone() else {
                    continue;
                };
                if url.trim().is_empty() {
                    continue;
                }
                if f.filesize == 0 && f.filename.is_empty() {
                    continue;
                }
                seen.insert(url.clone());
                if !file_passes_filters(
                    f,
                    &module.name,
                    &sec_folder,
                    filters,
                    module_re.as_ref(),
                    module_re_invalid,
                ) {
                    continue;
                }
                let external = !is_pluginfile(&url);
                let sanitized = if external {
                    external_leaf(&f.filename)
                } else {
                    let s = ra::sanitize(&f.filename);
                    if s.is_empty() {
                        "unnamed".to_string()
                    } else {
                        s
                    }
                };
                let Some(leaf) = unique_leaf(&mut assigned, &dir_key, &sanitized, filters.conflict)
                else {
                    continue; // Conflict::Skip duplicate.
                };
                let dest_rel = shorten_long_path(course_dir_name, &dir_key, &leaf);
                let planned = PlannedFile {
                    url: url.clone(),
                    filename: f.filename.clone(),
                    stored_name: leaf,
                    filesize: f.filesize,
                    timemodified: f.timemodified,
                    contenthash: f.contenthash.clone(),
                    mimetype: f.mimetype.clone(),
                    section_folder: sec_folder.clone(),
                    module_name: module.name.clone(),
                    module_folder: mod_folder.clone(),
                    dest_rel: dest_rel.clone(),
                    is_external: external,
                };
                match old.get(&url) {
                    Some(prev) if prev.size == f.filesize => {
                        if prev.path == dest_rel {
                            out.reuse.push(planned);
                        } else {
                            out.moves.push(FileMove {
                                url: url.clone(),
                                from: prev.path.clone(),
                                to: dest_rel,
                                file: planned,
                            });
                        }
                    }
                    _ => out.fetch.push(planned),
                }
            }
        }
    }

    for (url, entry) in old.iter() {
        if !seen.contains(url) {
            out.stale.push(StaleEntry {
                url: url.clone(),
                path: entry.path.clone(),
            });
        }
    }
    resolve_cross_url_moves_by_sha(old, &mut out);
    // Deterministic output for dry-run JSON diffs.
    out.fetch.sort_by(|a, b| a.dest_rel.cmp(&b.dest_rel));
    out.reuse.sort_by(|a, b| a.dest_rel.cmp(&b.dest_rel));
    out.moves.sort_by(|a, b| a.to.cmp(&b.to));
    out.stale.sort_by(|a, b| a.path.cmp(&b.path));
    out
}

/// Cross-URL content moves ("move/rename detect by sha", brief Step 2).
///
/// Same-URL path changes are detected in [`plan`] proper; this pass handles
/// the content-identity case: a fetch candidate whose Moodle `contenthash`
/// matches the stored sha256 of an old entry whose URL vanished from
/// contents is a move (rename on disk, no download), not a fresh download.
/// Only otherwise-stale entries participate (a still-visible URL keeps its
/// own reuse/move/fetch verdict); each stale entry is consumed at most
/// once, deterministically by (`path`, `url`) order. Empty hashes on either
/// side never match (old fixtures, unhashed externals).
fn resolve_cross_url_moves_by_sha(old: &BTreeMap<String, crate::state::FileEntry>, out: &mut Plan) {
    use std::collections::VecDeque;
    // sha256 → queue of stale indices in deterministic (path, url) order
    // (final output sorting happens after this pass, so order locally).
    let mut order: Vec<usize> = (0..out.stale.len()).collect();
    order.sort_by(|&a, &b| {
        (out.stale[a].path.clone(), out.stale[a].url.clone())
            .cmp(&(out.stale[b].path.clone(), out.stale[b].url.clone()))
    });
    let mut by_sha: std::collections::HashMap<String, VecDeque<usize>> =
        std::collections::HashMap::new();
    for idx in order {
        let stale = &out.stale[idx];
        let Some(entry) = old.get(&stale.url) else {
            continue;
        };
        if entry.sha256.is_empty() {
            continue;
        }
        by_sha
            .entry(entry.sha256.clone())
            .or_default()
            .push_back(idx);
    }
    if by_sha.is_empty() {
        return;
    }
    let mut consumed = vec![false; out.stale.len()];
    let mut kept_fetch: Vec<PlannedFile> = Vec::with_capacity(out.fetch.len());
    // Contents-walk order is deterministic, so in-place iteration is stable.
    let fetch = std::mem::take(&mut out.fetch);
    for file in fetch {
        // Externals never download (pointer files are regenerated), so
        // content matching only applies to real pluginfile downloads.
        let matched = if file.is_external || file.contenthash.is_empty() {
            None
        } else {
            by_sha.get_mut(&file.contenthash).and_then(|queue| {
                while let Some(&idx) = queue.front() {
                    if consumed[idx] {
                        queue.pop_front();
                        continue;
                    }
                    return queue.pop_front();
                }
                None
            })
        };
        match matched {
            Some(idx) => {
                consumed[idx] = true;
                out.moves.push(FileMove {
                    url: file.url.clone(),
                    from: out.stale[idx].path.clone(),
                    to: file.dest_rel.clone(),
                    file,
                });
            }
            None => kept_fetch.push(file),
        }
    }
    out.fetch = kept_fetch;
    let mut kept_stale = Vec::with_capacity(out.stale.len());
    for (idx, stale) in out.stale.drain(..).enumerate() {
        if !consumed[idx] {
            kept_stale.push(stale);
        }
    }
    out.stale = kept_stale;
}
/// Convenience: plan + serialise for `--dry-run` output.
pub fn plan_json(
    contents: &[CourseSection],
    filters: &SyncFilters,
    state: &CourseState,
    course_dir_name: &str,
) -> String {
    let p = plan(contents, filters, state, course_dir_name);
    p.to_json().unwrap_or_else(|_| "{}".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn planner_glob_filters() {
        assert!(glob_match("*.mp4", "lecture.MP4"));
        assert!(glob_match("*.zip", "a.ZIP"));
        assert!(!glob_match("*.mp4", "notes.pdf"));
        assert!(glob_match("s01-*.pdf", "s01-intro.pdf"));
        assert!(glob_match("?.pdf", "a.pdf"));
        assert!(!glob_match("?.pdf", "ab.pdf"));
        assert!(glob_match("*", "anything.bin"));
    }

    #[test]
    fn planner_mime_matching() {
        assert!(mime_matches("application/pdf", "application/pdf"));
        assert!(mime_matches("video/", "video/mp4"));
        assert!(!mime_matches("video/", "application/pdf"));
        assert!(!mime_matches("application/pdf", ""));
    }

    #[test]
    fn planner_conflict_parse() {
        assert_eq!(
            ConflictStrategy::parse("suffix"),
            Some(ConflictStrategy::Suffix)
        );
        assert_eq!(
            ConflictStrategy::parse("SKIP"),
            Some(ConflictStrategy::Skip)
        );
        assert_eq!(
            ConflictStrategy::parse("overwrite"),
            Some(ConflictStrategy::Overwrite)
        );
        assert_eq!(ConflictStrategy::parse("bogus"), None);
    }

    #[test]
    fn planner_unique_suffix() {
        let mut assigned = HashSet::new();
        let a = unique_leaf(
            &mut assigned,
            "RA01/001-m",
            "a.pdf",
            ConflictStrategy::Suffix,
        );
        let b = unique_leaf(
            &mut assigned,
            "RA01/001-m",
            "a.pdf",
            ConflictStrategy::Suffix,
        );
        assert_eq!(a.as_deref(), Some("a.pdf"));
        assert_eq!(b.as_deref(), Some("a-2.pdf"));
        let c = unique_leaf(&mut assigned, "RA01/001-m", "a.pdf", ConflictStrategy::Skip);
        assert_eq!(c, None);
    }

    #[test]
    fn planner_reuse_vs_fetch_by_size() {
        use crate::state::FileEntry;
        let mut files = BTreeMap::new();
        files.insert(
            "http://h.invalid/webservice/pluginfile.php/1/a.pdf".to_string(),
            FileEntry {
                path: "C/RA01/001-m/a.pdf".to_string(),
                sha256: "abc".to_string(),
                size: 100,
                ..Default::default()
            },
        );
        let state = CourseState {
            shortname: "C".to_string(),
            files,
            ..Default::default()
        };
        let contents = vec![CourseSection {
            id: 1,
            name: "RA01".to_string(),
            summary: "s".to_string(),
            position: 1,
            modules: vec![crate::moodle::CourseModule {
                id: 2,
                name: "m".to_string(),
                module_type: "resource".to_string(),
                description: None,
                contents: Some(vec![
                    ModuleFile {
                        filename: "a.pdf".to_string(),
                        filesize: 100,
                        mimetype: "application/pdf".to_string(),
                        fileurl: Some(
                            "http://h.invalid/webservice/pluginfile.php/1/a.pdf".to_string(),
                        ),
                        timemodified: 0,
                        contenthash: String::new(),
                    },
                    ModuleFile {
                        filename: "b.pdf".to_string(),
                        filesize: 200,
                        mimetype: "application/pdf".to_string(),
                        fileurl: Some(
                            "http://h.invalid/webservice/pluginfile.php/1/b.pdf".to_string(),
                        ),
                        timemodified: 0,
                        contenthash: String::new(),
                    },
                ]),
            }],
        }];
        // Note: module folder is 001-m (index 0 → 001), not 001-m verbatim
        // matching the state path "C/RA01/001-m/a.pdf".
        let p = plan(&contents, &SyncFilters::default(), &state, "C");
        assert_eq!(p.reuse.len(), 1, "same size → reuse: {p:?}");
        assert_eq!(p.fetch.len(), 1, "unknown url → fetch: {p:?}");
        assert!(p.moves.is_empty());
        assert!(p.stale.is_empty());
    }

    #[test]
    fn planner_move_on_path_change() {
        use crate::state::FileEntry;
        let mut files = BTreeMap::new();
        files.insert(
            "http://h.invalid/webservice/pluginfile.php/1/a.pdf".to_string(),
            FileEntry {
                path: "C/RA01/001-old/a.pdf".to_string(),
                sha256: "abc".to_string(),
                size: 100,
                ..Default::default()
            },
        );
        let state = CourseState {
            shortname: "C".to_string(),
            files,
            ..Default::default()
        };
        let contents = vec![CourseSection {
            id: 1,
            name: "RA01".to_string(),
            summary: "s".to_string(),
            position: 1,
            modules: vec![crate::moodle::CourseModule {
                id: 2,
                name: "m".to_string(),
                module_type: "resource".to_string(),
                description: None,
                contents: Some(vec![ModuleFile {
                    filename: "a.pdf".to_string(),
                    filesize: 100,
                    mimetype: "application/pdf".to_string(),
                    fileurl: Some("http://h.invalid/webservice/pluginfile.php/1/a.pdf".to_string()),
                    timemodified: 0,
                    contenthash: String::new(),
                }]),
            }],
        }];
        let p = plan(&contents, &SyncFilters::default(), &state, "C");
        assert!(p.reuse.is_empty());
        assert!(p.fetch.is_empty());
        assert_eq!(p.moves.len(), 1, "same url new path → move: {p:?}");
    }

    #[test]
    fn planner_cross_url_move_by_sha() {
        use crate::state::FileEntry;
        // Old state: URL-A stored at 001-m/a.pdf with sha S.
        let mut files = BTreeMap::new();
        files.insert(
            "http://h.invalid/webservice/pluginfile.php/1/a.pdf".to_string(),
            FileEntry {
                path: "C/RA01/001-m/a.pdf".to_string(),
                sha256: "somesha256".to_string(),
                size: 100,
                ..Default::default()
            },
        );
        let state = CourseState {
            shortname: "C".to_string(),
            files,
            ..Default::default()
        };
        // New contents: different URL-B, same bytes (Moodle contenthash S),
        // laid out under a renamed module folder.
        let contents = vec![CourseSection {
            id: 1,
            name: "RA01".to_string(),
            summary: "s".to_string(),
            position: 1,
            modules: vec![crate::moodle::CourseModule {
                id: 2,
                name: "renamed".to_string(),
                module_type: "resource".to_string(),
                description: None,
                contents: Some(vec![ModuleFile {
                    filename: "a.pdf".to_string(),
                    filesize: 100,
                    mimetype: "application/pdf".to_string(),
                    fileurl: Some(
                        "http://h.invalid/webservice/pluginfile.php/1/a-v2.pdf".to_string(),
                    ),
                    timemodified: 0,
                    contenthash: "somesha256".to_string(),
                }]),
            }],
        }];
        let p = plan(&contents, &SyncFilters::default(), &state, "C");
        assert!(p.fetch.is_empty(), "sha match must not download: {p:?}");
        assert!(p.stale.is_empty(), "matched stale entry is consumed: {p:?}");
        assert_eq!(p.moves.len(), 1, "cross-URL sha match → move: {p:?}");
        let mv = &p.moves[0];
        assert_eq!(
            mv.url,
            "http://h.invalid/webservice/pluginfile.php/1/a-v2.pdf"
        );
        assert_eq!(mv.from, "C/RA01/001-m/a.pdf");
        assert!(
            mv.to.contains("001-renamed"),
            "destination follows new layout: {p:?}"
        );
    }

    #[test]
    fn planner_cross_url_no_match_without_hash() {
        use crate::state::FileEntry;
        let mut files = BTreeMap::new();
        files.insert(
            "http://h.invalid/webservice/pluginfile.php/1/a.pdf".to_string(),
            FileEntry {
                path: "C/RA01/001-m/a.pdf".to_string(),
                sha256: "somesha256".to_string(),
                size: 100,
                ..Default::default()
            },
        );
        let state = CourseState {
            shortname: "C".to_string(),
            files,
            ..Default::default()
        };
        // Same shape as above but the new file carries no contenthash:
        // empty hashes never match, so it stays fetch + stale.
        let contents = vec![CourseSection {
            id: 1,
            name: "RA01".to_string(),
            summary: "s".to_string(),
            position: 1,
            modules: vec![crate::moodle::CourseModule {
                id: 2,
                name: "renamed".to_string(),
                module_type: "resource".to_string(),
                description: None,
                contents: Some(vec![ModuleFile {
                    filename: "a.pdf".to_string(),
                    filesize: 100,
                    mimetype: "application/pdf".to_string(),
                    fileurl: Some(
                        "http://h.invalid/webservice/pluginfile.php/1/a-v2.pdf".to_string(),
                    ),
                    timemodified: 0,
                    contenthash: String::new(),
                }]),
            }],
        }];
        let p = plan(&contents, &SyncFilters::default(), &state, "C");
        assert_eq!(p.fetch.len(), 1, "no hash → download: {p:?}");
        assert_eq!(p.stale.len(), 1, "old URL stays a GC candidate: {p:?}");
        assert!(p.moves.is_empty());
    }

    #[test]
    fn planner_filters_only_and_except() {
        let state = CourseState::default();
        let mk = |sec: &str, name: &str| CourseSection {
            id: 1,
            name: sec.to_string(),
            summary: "s".to_string(),
            position: 1,
            modules: vec![crate::moodle::CourseModule {
                id: 2,
                name: "m".to_string(),
                module_type: "resource".to_string(),
                description: None,
                contents: Some(vec![ModuleFile {
                    filename: name.to_string(),
                    filesize: 10,
                    mimetype: "video/mp4".to_string(),
                    fileurl: Some(format!(
                        "http://h.invalid/webservice/pluginfile.php/1/{name}"
                    )),
                    timemodified: 0,
                    contenthash: String::new(),
                }]),
            }],
        };
        let contents = vec![mk("RA01", "a.mp4"), mk("RA02", "b.mp4")];
        let f = SyncFilters {
            only: vec!["RA01".to_string()],
            ..Default::default()
        };
        let p = plan(&contents, &f, &state, "C");
        assert_eq!(p.fetch.len(), 1);
        let f2 = SyncFilters {
            except: vec!["*.mp4".to_string()],
            ..Default::default()
        };
        let p2 = plan(&contents, &f2, &state, "C");
        assert!(p2.fetch.is_empty(), "except glob must filter all");
    }
}
