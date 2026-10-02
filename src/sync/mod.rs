//! Concurrent incremental sync engine (Task 3, P2 sync engine).
//!
//! Layout is unchanged: `$MOODLE_ROOT/<shortname>/RAxx/nnn-module/`.
//! Only `pluginfile.php` URLs are downloaded (anything else becomes an
//! external `.url` + `.md` pointer pair); the token never reaches logs.
//!
//! Pipeline per course: `contents` (retry) → [`planner::plan`] (size
//! pre-check + filters) → [`executor::run`] (Semaphore 8 over the single
//! shared client governor, `.part` + rename) → [`indexer`]
//! (frontmatter `index.md`, per-module/section `meta.json`) → receipt
//! `last-sync.json`. Stale entries drop out of state; the file moves to
//! `.moodle/trash/sync-<ts>/` only if its sha still matches (see
//! [`trash_untouched`]), otherwise it stays.
//!

pub mod executor;
pub mod gc;
pub mod indexer;
pub mod planner;
pub mod report;

pub use executor::{run, RunContext, RunOptions, DEFAULT_CONCURRENCY};
pub use gc::{iso_now, unix_secs_now, unix_to_iso8601, write_receipt, Receipt};
pub use indexer::{IndexRow, ModuleFileMeta};
pub use planner::{ConflictStrategy, FileMove, Plan, PlannedFile, StaleEntry, SyncFilters};
pub use report::{Report, SyncReport};

use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

use crate::html2md::html_to_md;
use crate::moodle::{Course, CourseSection, Moodle};
use crate::ra;
use crate::state::State;

/// Folder name from the course's full name: code prefix dropped, first word slugged.
#[must_use]
pub fn default_course_slug(fullname: &str) -> String {
    static RE: std::sync::LazyLock<regex::Regex> = std::sync::LazyLock::new(|| {
        regex::Regex::new(r"^(\s*[A-Za-z]{0,3}\d+[.:]?\s*)+").expect("static regex")
    });
    let rest = RE.replace(fullname, "");
    let word = rest.split_whitespace().next().unwrap_or(fullname);
    ra::slug(word, 40)
}

/// Dirs under the root that are not course folders.
const RESERVED: [&str; 3] = ["submit", "personal", "moodle-mcp"];

/// Course folder: `.moodle/folders.json` (`{"<id>":"<dir>"}`) wins; a missing
/// entry gets the default slug (`-<id>` on collision) and is persisted.
/// A corrupt map is an error, never overwritten.
pub fn course_dir_name(root: &str, course: &Course) -> Result<String> {
    let path = Path::new(root).join(".moodle/folders.json");
    let mut map: BTreeMap<String, String> = match std::fs::read_to_string(&path) {
        Ok(t) => serde_json::from_str(&t).context("parsing .moodle/folders.json")?,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => BTreeMap::new(),
        Err(e) => return Err(e).context("reading .moodle/folders.json"),
    };
    let key = course.id.to_string();
    if let Some(v) = map
        .get(&key)
        .filter(|v| ra::slug(v, 40) == **v && !RESERVED.contains(&v.as_str()))
    {
        return Ok(v.clone());
    }
    let mut dir = default_course_slug(&course.fullname);
    if RESERVED.contains(&dir.as_str()) || map.iter().any(|(k, v)| *k != key && *v == dir) {
        dir = format!("{dir}-{}", course.id);
    }
    map.insert(key, dir.clone());
    if let Ok(t) = serde_json::to_vec_pretty(&map) {
        let _ = std::fs::create_dir_all(path.parent().unwrap_or(Path::new(".")));
        let _ = crate::web::files::write_atomic(&path, t);
    }
    Ok(dir)
}

/// Move `<root>/<rel>` to `.moodle/trash/sync-<ts>/<rel>` iff it exists and its
/// sha256 equals `sha` (untouched). Empty parent dirs are pruned up to `root`.
pub fn trash_untouched(root: &Path, rel: &str, sha: &str, ts: u64) -> bool {
    use sha2::Digest as _;
    let from = root.join(rel);
    let same = !sha.is_empty()
        && std::fs::read(&from).is_ok_and(|b| format!("{:x}", sha2::Sha256::digest(b)) == sha);
    let to = root.join(format!(".moodle/trash/sync-{ts}/{rel}"));
    let moved = same
        && to
            .parent()
            .is_some_and(|d| std::fs::create_dir_all(d).is_ok())
        && std::fs::rename(&from, &to).is_ok();
    if moved {
        let mut d = from.parent();
        while let Some(p) = d.filter(|p| *p != root) {
            if std::fs::remove_dir(p).is_err() {
                break;
            }
            d = p.parent();
        }
    }
    moved
}

async fn fetch_contents(moodle: &Moodle, course_id: i64) -> Result<Vec<CourseSection>> {
    let value = moodle
        .call_with_retry(
            "core_course_get_contents",
            &[("courseid", course_id.to_string())],
        )
        .await
        .map_err(|e| match e {
            // keep the type so callers can downcast and renew auth
            crate::errors::CoreError::Auth(m) => {
                crate::errors::CoreError::Auth(format!("get contents: {m}")).into()
            }
            e => anyhow::anyhow!("get contents: {e}"),
        })?;
    serde_json::from_value(value).context("decoding contents")
}

struct ModuleLayout {
    id: i64,
    name: String,
    folder: String,
    dir: PathBuf,
    readme_md: Option<String>,
}

struct SectionLayout {
    id: i64,
    name: String,
    folder: String,
    dir: PathBuf,
    summary_md: String,
    modules: Vec<ModuleLayout>,
}

/// Section/module directory plan. Mirrors the planner walk (same helpers,
/// same order, same skip rules) so executor destinations always exist.
fn layout_sections(
    contents: &[CourseSection],
    course_dir: &Path,
    keep_empty: bool,
    known: &BTreeMap<String, String>,
    files: &BTreeMap<String, crate::state::FileEntry>,
) -> Vec<SectionLayout> {
    let mut sections = Vec::new();
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
        let sec_folder = planner::section_folder_name(si, &section.name, &module_texts);
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
        let sec_dir = course_dir.join(ra::sanitize(&sec_folder));
        let summary_md = html_to_md(&section.summary);
        let folders = planner::assign_module_folders(section, known, files, |m| {
            planner::module_kept(m, keep_empty)
        });
        let mut modules = Vec::new();
        for (module, mod_folder) in section.modules.iter().zip(folders) {
            let Some(mod_folder) = mod_folder else {
                continue;
            };
            let has_desc = !module
                .description
                .as_deref()
                .unwrap_or("")
                .trim()
                .is_empty();
            modules.push(ModuleLayout {
                id: module.id,
                name: module.name.clone(),
                folder: mod_folder.clone(),
                dir: sec_dir.join(&mod_folder),
                readme_md: has_desc
                    .then(|| html_to_md(module.description.as_deref().unwrap_or(""))),
            });
        }
        sections.push(SectionLayout {
            id: section.id,
            name: section.name.clone(),
            folder: sec_folder,
            dir: sec_dir,
            summary_md,
            modules,
        });
    }
    sections
}

async fn ensure_layout(layout: &[SectionLayout], section_threshold: usize) -> Result<()> {
    for section in layout {
        tokio::fs::create_dir_all(&section.dir)
            .await
            .context("creating section dir")?;
        // Section summaries under the threshold are noise — skip the file.
        if section.summary_md.trim().len() >= section_threshold {
            tokio::fs::write(
                section.dir.join("section-summary.md"),
                section.summary_md.as_bytes(),
            )
            .await
            .context("writing section-summary.md")?;
        }
        for module in &section.modules {
            tokio::fs::create_dir_all(&module.dir)
                .await
                .context("creating module dir")?;
        }
    }
    Ok(())
}

async fn write_readmes(layout: &[SectionLayout]) -> Result<()> {
    for module in layout.iter().flat_map(|s| &s.modules) {
        if let Some(md) = &module.readme_md {
            tokio::fs::write(module.dir.join("README.md"), md.as_bytes())
                .await
                .context("writing README.md")?;
        }
    }
    Ok(())
}

/// `sync_course`, but on an Auth failure renew the token once and retry.
pub async fn sync_course_renewing(
    moodle: &Moodle,
    state: &mut State,
    course: &Course,
    root: &str,
) -> Result<SyncReport> {
    match sync_course(moodle, state, course, root).await {
        Err(e)
            if matches!(
                e.downcast_ref::<crate::errors::CoreError>(),
                Some(crate::errors::CoreError::Auth(_))
            ) && moodle.recover_auth().await == crate::moodle::Renew::Ok =>
        {
            sync_course(moodle, state, course, root).await
        }
        r => r,
    }
}

/// Full sync with default filters and concurrency 8.
pub async fn sync_course(
    moodle: &Moodle,
    state: &mut State,
    course: &Course,
    root: &str,
) -> Result<SyncReport> {
    let filters = SyncFilters::default();
    let opts = RunOptions::default();
    let started = std::time::Instant::now();
    let root_path = Path::new(root).to_path_buf();
    let dir_name = course_dir_name(root, course)?;
    let course_dir = root_path.join(&dir_name);
    tokio::fs::create_dir_all(&course_dir)
        .await
        .context("creating course dir")?;

    let contents = fetch_contents(moodle, course.id).await?;

    let mut cs = state
        .courses
        .get(&course.id.to_string())
        .cloned()
        .unwrap_or_default();
    cs.upsert(course);
    // State v2 (Task 4): ISO8601 on write; v1 epoch values migrate on load.
    cs.synced_at = iso_now();

    let plan = planner::plan(&contents, &filters, &cs, &dir_name);
    let layout = layout_sections(
        &contents,
        &course_dir,
        filters.keep_empty,
        &cs.modules,
        &cs.files,
    );
    for section in &layout {
        cs.sections
            .insert(section.id.to_string(), section.folder.clone());
        for m in &section.modules {
            cs.modules.insert(m.id.to_string(), m.folder.clone());
        }
    }
    for (si, section) in contents.iter().enumerate() {
        for (mi, m) in section.modules.iter().enumerate() {
            cs.order.insert(m.id.to_string(), (si * 1000 + mi) as i64);
        }
    }
    ensure_layout(&layout, 20).await?;
    write_readmes(&layout).await?;

    let old_files = cs.files.clone();
    let ctx = RunContext {
        course_id: course.id,
        shortname: course.shortname.clone(),
        course_dir: course_dir.clone(),
        root: root_path.clone(),
    };
    let (mut report, new_files) = run(moodle, &plan, &ctx, &opts, &old_files).await;
    // Commit state before fallible writes so downloaded files stay tracked.
    cs.files = new_files.clone();
    state.courses.insert(course.id.to_string(), cs);

    let ts = unix_secs_now();
    report.files_removed = plan
        .stale
        .iter()
        .filter(|s| !new_files.values().any(|e| e.path == s.path))
        .filter_map(|s| old_files.get(&s.url))
        .filter(|e| trash_untouched(&root_path, &e.path, &e.sha256, ts))
        .count();

    write_index_and_meta(course, &layout, &plan, &new_files).await?;
    write_course_summary(&course_dir, course).await?;

    let receipt = Receipt {
        course_id: course.id,
        shortname: course.shortname.clone(),
        synced_at: iso_now(),
        files_new: report.files_new,
        files_skipped: report.files_skipped,
        bytes_new: report.bytes_new,
        duration_ms: started.elapsed().as_millis().min(u128::from(u64::MAX)) as u64,
        errors: report.errors.clone(),
    };
    report.duration_ms = receipt.duration_ms;
    write_receipt(&course_dir, &receipt).await?;
    Ok(report.to_sync_report())
}

fn planned_lookup(plan: &Plan) -> HashMap<String, PlannedFile> {
    let mut map = HashMap::new();
    for file in plan.fetch.iter().chain(plan.reuse.iter()) {
        map.insert(file.url.clone(), file.clone());
    }
    for mv in &plan.moves {
        map.insert(mv.url.clone(), mv.file.clone());
    }
    map
}

async fn write_index_and_meta(
    course: &Course,
    layout: &[SectionLayout],
    plan: &Plan,
    new_files: &BTreeMap<String, crate::state::FileEntry>,
) -> Result<()> {
    let lookup = planned_lookup(plan);

    let mut rows: Vec<IndexRow> = Vec::new();
    // (section folder, module folder) → file metas.
    let mut by_module: HashMap<(String, String), Vec<ModuleFileMeta>> = HashMap::new();
    for (url, entry) in new_files.iter() {
        if let Some(info) = lookup.get(url) {
            rows.push(IndexRow {
                section: info.section_folder.clone(),
                module: info.module_name.clone(),
                original: info.filename.clone(),
                stored: info.stored_name.clone(),
                size: entry.size,
                url: url.clone(),
                is_external: info.is_external,
            });
            by_module
                .entry((info.section_folder.clone(), info.module_folder.clone()))
                .or_default()
                .push(ModuleFileMeta {
                    filename: info.filename.clone(),
                    stored: info.stored_name.clone(),
                    url: url.clone(),
                    size: entry.size,
                    timemodified: info.timemodified,
                    mimetype: info.mimetype.clone(),
                });
        } else {
            let leaf = entry.path.rsplit('/').next().unwrap_or("file").to_string();
            rows.push(IndexRow {
                section: String::new(),
                module: String::new(),
                original: leaf.clone(),
                stored: leaf,
                size: entry.size,
                url: url.clone(),
                is_external: false,
            });
        }
    }
    rows.sort_by(|a, b| {
        (a.section.clone(), a.module.clone(), a.stored.clone()).cmp(&(
            b.section.clone(),
            b.module.clone(),
            b.stored.clone(),
        ))
    });

    // Course summary: inline unless huge (then split to summary.md).
    let summary_md = html_to_md(&course.summary);
    let inline_summary = if summary_md.len() > 2000 {
        let excerpt: String = summary_md.chars().take(500).collect();
        format!("{excerpt}\n\n(full summary in `summary.md`)")
    } else {
        summary_md.clone()
    };
    let course_dir = layout
        .first()
        .and_then(|s| s.dir.parent().map(Path::to_path_buf))
        .unwrap_or_else(|| PathBuf::from("."));
    indexer::write_course_index(&course_dir, course, &inline_summary, &rows, &iso_now())?;

    for section in layout {
        let mut section_files = 0usize;
        let mut section_bytes = 0u64;
        for module in &section.modules {
            let key = (section.folder.clone(), module.folder.clone());
            let files = by_module.get(&key);
            let empty: Vec<ModuleFileMeta> = Vec::new();
            let files = files.unwrap_or(&empty);
            section_files += files.len();
            section_bytes += files.iter().map(|f| f.size.max(0) as u64).sum::<u64>();
            indexer::write_module_meta(&module.dir, module.id, &module.name, files)?;
        }
        indexer::write_section_meta(
            &section.dir,
            section.id,
            &section.name,
            &section.folder,
            section.modules.len(),
            section_files,
            section_bytes,
        )?;
    }
    Ok(())
}

async fn write_course_summary(course_dir: &Path, course: &Course) -> Result<()> {
    let summary_md = html_to_md(&course.summary);
    if summary_md.len() > 2000 {
        tokio::fs::write(course_dir.join("summary.md"), summary_md.as_bytes())
            .await
            .context("writing summary.md")?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn readme_follows_description_ra() {
        let dir = std::env::temp_dir().join(format!("smx-t02-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let contents: Vec<CourseSection> = serde_json::from_value(serde_json::json!([{
            "id": 1, "name": "Unit", "summary": "",
            "modules": [{"id": 9, "name": "Task", "modname": "assign",
                         "description": "<p>Delivers in RA3</p>"}]
        }]))
        .unwrap();
        let layout = layout_sections(&contents, &dir, false, &BTreeMap::new(), &BTreeMap::new());
        ensure_layout(&layout, 20).await.unwrap();
        write_readmes(&layout).await.unwrap();
        let readme = dir.join("RA03/001-task/README.md");
        let ok = readme.exists();
        let _ = std::fs::remove_dir_all(&dir);
        assert!(ok, "{readme:?}");
    }

    #[test]
    fn default_course_slugs() {
        for (t, w) in [
            ("0373.Llenguatges de marques", "llenguatges"),
            ("MP03. Programació Estructurada", "programacio"),
            ("M1665. Digitalització aplicada", "digitalitzacio"),
            ("0484: Bases de dades", "bases"),
            ("MP05 Entorns de desenvolupament", "entorns"),
            ("Tutoria DAX1", "tutoria"),
        ] {
            assert_eq!(default_course_slug(t), w, "{t}");
        }
    }

    #[test]
    fn course_dir_name_uses_map_then_persists_default() {
        let r = std::env::temp_dir().join(format!("smx-cdn-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&r);
        std::fs::create_dir_all(r.join(".moodle")).unwrap();
        std::fs::write(r.join(".moodle/folders.json"), r#"{"7":"bbdd"}"#).unwrap();
        let root = r.to_str().unwrap();
        let c = |id, n: &str| Course {
            id,
            fullname: n.into(),
            shortname: String::new(),
            summary: String::new(),
        };
        assert_eq!(
            course_dir_name(root, &c(7, "Bases de dades")).unwrap(),
            "bbdd"
        );
        assert_eq!(
            course_dir_name(root, &c(8, "Other Course")).unwrap(),
            "other"
        );
        assert_eq!(
            course_dir_name(root, &c(9, "Other Course")).unwrap(),
            "other-9"
        );
        assert_eq!(
            course_dir_name(root, &c(10, "Submit things")).unwrap(),
            "submit-10"
        );
        let saved = std::fs::read_to_string(r.join(".moodle/folders.json")).unwrap();
        assert!(saved.contains(r#""8": "other""#) && saved.contains(r#""9": "other-9""#));
        std::fs::write(r.join(".moodle/folders.json"), "{oops").unwrap();
        assert!(course_dir_name(root, &c(11, "Zed")).is_err());
        assert_eq!(
            std::fs::read(r.join(".moodle/folders.json")).unwrap(),
            b"{oops"
        );
        let _ = std::fs::remove_dir_all(&r);
    }
}
