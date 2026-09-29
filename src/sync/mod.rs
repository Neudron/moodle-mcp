//! Concurrent incremental sync engine (Task 3, P2 sync engine).
//!
//! Layout is unchanged: `$MOODLE_ROOT/<shortname>/RAxx/nnn-module/`.
//! Only `pluginfile.php` URLs are downloaded (anything else becomes an
//! external `.url` + `.md` pointer pair); the token never reaches logs.
//!
//! Pipeline per course: `contents` (retry) → [`planner::plan`] (size
//! pre-check + filters) → [`executor::run`] (Semaphore 8 over the single
//! shared client governor, `.part` + rename, Mbps cap) → [`indexer`]
//! (frontmatter `index.md`, per-module/section `meta.json`) → receipt
//! `last-sync.json`. Stale entries drop out of state but are never deleted
//! here — deletion is the two-step [`gc`] flow (`preview → trash → apply`).
//!
//! [`sync_course`] keeps the historic signature so `src/bin/*` are
//! untouched; [`sync_course_with_options`] and [`plan_course`] carry the
//! new filters/concurrency/dry-run surface for the CLI/MCP tasks.

pub mod executor;
pub mod gc;
pub mod indexer;
pub mod planner;
pub mod report;

pub use executor::{run, RunContext, RunOptions, DEFAULT_CONCURRENCY};
pub use gc::{
    append_audit, apply_trash, iso_now, preview, repair, trash, unix_secs_now, unix_to_iso8601,
    verify, write_receipt, GcCandidate, Receipt, RepairReport, VerifyMismatch,
};
pub use indexer::{IndexRow, ModuleFileMeta};
pub use planner::{ConflictStrategy, FileMove, Plan, PlannedFile, StaleEntry, SyncFilters};
pub use report::{Report, SyncReport};

use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

use crate::html2md::html_to_md;
use crate::moodle::{Course, CourseSection, Moodle};
use crate::ra;
use crate::state::State;

fn course_dir_name(course: &Course) -> String {
    if course.shortname.is_empty() {
        return format!("course-{}", course.id);
    }
    let clean = ra::sanitize(&course.shortname);
    if clean.is_empty() {
        format!("course-{}", course.id)
    } else {
        clean
    }
}

async fn fetch_contents(moodle: &Moodle, course_id: i64) -> Result<Vec<CourseSection>> {
    let value = moodle
        .call_with_retry(
            "core_course_get_contents",
            &[("courseid", course_id.to_string())],
        )
        .await
        .map_err(|e| anyhow::anyhow!("get contents: {e}"))?;
    serde_json::from_value(value).context("decoding contents")
}

struct ModuleLayout {
    id: i64,
    name: String,
    folder: String,
    dir: PathBuf,
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
        let mut taken: Vec<String> = Vec::new();
        let mut modules = Vec::new();
        for (mi, module) in section.modules.iter().enumerate() {
            let has_files = module
                .contents
                .as_ref()
                .map(|c| !c.is_empty())
                .unwrap_or(false);
            let has_desc = !module
                .description
                .as_deref()
                .unwrap_or("")
                .trim()
                .is_empty();
            if !has_files && !has_desc && !keep_empty {
                continue;
            }
            let mod_folder = planner::module_folder(mi, &module.name, &mut taken);
            modules.push(ModuleLayout {
                id: module.id,
                name: module.name.clone(),
                folder: mod_folder.clone(),
                dir: sec_dir.join(&mod_folder),
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

async fn ensure_layout(
    layout: &[SectionLayout],
    keep_empty: bool,
    section_threshold: usize,
) -> Result<()> {
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
            let _ = keep_empty;
        }
    }
    Ok(())
}

async fn write_readmes(contents: &[CourseSection], layout: &[SectionLayout]) -> Result<()> {
    // Map (section folder, module folder) → description via the same walk.
    let mut descs: HashMap<(String, String), String> = HashMap::new();
    for (si, section) in contents.iter().enumerate() {
        let module_texts: Vec<&str> = section.modules.iter().map(|m| m.name.as_str()).collect();
        let sec_folder = planner::section_folder_name(si, &section.name, &module_texts);
        let mut taken: Vec<String> = Vec::new();
        for (mi, module) in section.modules.iter().enumerate() {
            let mod_folder = planner::module_folder(mi, &module.name, &mut taken);
            if let Some(desc) = module.description.as_deref() {
                if !desc.trim().is_empty() {
                    descs.insert((sec_folder.clone(), mod_folder), html_to_md(desc));
                }
            }
        }
    }
    for section in layout {
        for module in &section.modules {
            if let Some(md) = descs.get(&(section.folder.clone(), module.folder.clone())) {
                tokio::fs::write(module.dir.join("README.md"), md.as_bytes())
                    .await
                    .context("writing README.md")?;
            }
        }
    }
    Ok(())
}

/// Dry-run helper for `--dry-run` JSON output: fetch + plan, no writes.
pub async fn plan_course(
    moodle: &Moodle,
    state: &State,
    course: &Course,
    filters: &SyncFilters,
) -> Result<Plan> {
    let contents = fetch_contents(moodle, course.id).await?;
    let dir_name = course_dir_name(course);
    let previous = state.courses.get(&course.id.to_string());
    let plan = match previous {
        Some(cs) => planner::plan(&contents, filters, cs, &dir_name),
        None => planner::plan(
            &contents,
            filters,
            &crate::state::CourseState::default(),
            &dir_name,
        ),
    };
    Ok(plan)
}

/// Historic entry point (signature frozen for `src/bin/*`): full sync with
/// default filters and concurrency 8.
pub async fn sync_course(
    moodle: &Moodle,
    state: &mut State,
    course: &Course,
    root: &str,
) -> Result<SyncReport> {
    let report = sync_course_with_options(
        moodle,
        state,
        course,
        root,
        &SyncFilters::default(),
        &RunOptions::default(),
        "sync",
    )
    .await?;
    Ok(report.to_sync_report())
}

/// Full sync with explicit filters, run options and audit actor.
pub async fn sync_course_with_options(
    moodle: &Moodle,
    state: &mut State,
    course: &Course,
    root: &str,
    filters: &SyncFilters,
    opts: &RunOptions,
    actor: &str,
) -> Result<Report> {
    let started = std::time::Instant::now();
    let root_path = Path::new(root).to_path_buf();
    let dir_name = course_dir_name(course);
    let course_dir = root_path.join(&dir_name);
    tokio::fs::create_dir_all(&course_dir)
        .await
        .context("creating course dir")?;

    let contents = fetch_contents(moodle, course.id).await?;

    let mut cs = state
        .courses
        .remove(&course.id.to_string())
        .unwrap_or_default();
    cs.upsert(course);
    let prev_synced_at = cs.synced_at.clone();
    // State v2 (Task 4): ISO8601 on write; v1 epoch values migrate on load.
    cs.synced_at = iso_now();

    let plan = planner::plan(&contents, filters, &cs, &dir_name);
    if filters.dry_run {
        let mut report = Report::empty(course.id, &course.shortname);
        report.files_skipped = plan.reuse.len() + plan.moves.len();
        report.duration_ms = started.elapsed().as_millis().min(u128::from(u64::MAX)) as u64;
        cs.synced_at = prev_synced_at; // dry runs leave state alone
        state.courses.insert(course.id.to_string(), cs);
        return Ok(report);
    }

    let layout = layout_sections(&contents, &course_dir, filters.keep_empty);
    for section in &layout {
        cs.sections
            .insert(section.id.to_string(), section.folder.clone());
    }
    ensure_layout(&layout, filters.keep_empty, 20).await?;
    write_readmes(&contents, &layout).await?;

    let old_files = cs.files.clone();
    let ctx = RunContext {
        course_id: course.id,
        shortname: course.shortname.clone(),
        course_dir: course_dir.clone(),
        root: root_path.clone(),
    };
    let (mut report, new_files) = run(moodle, &plan, &ctx, opts, &old_files).await;

    write_index_and_meta(course, &contents, &layout, &plan, &new_files).await?;
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
    let _ = actor;

    cs.files = new_files;
    state.courses.insert(course.id.to_string(), cs);
    Ok(report)
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
    contents: &[CourseSection],
    layout: &[SectionLayout],
    plan: &Plan,
    new_files: &BTreeMap<String, crate::state::FileEntry>,
) -> Result<()> {
    let lookup = planned_lookup(plan);
    let active: HashSet<String> = new_files.keys().cloned().collect();
    let _ = active;

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

    // Module ids for meta.json.
    let mut module_ids: HashMap<(String, String), (i64, String)> = HashMap::new();
    for (si, section) in contents.iter().enumerate() {
        let module_texts: Vec<&str> = section.modules.iter().map(|m| m.name.as_str()).collect();
        let sec_folder = planner::section_folder_name(si, &section.name, &module_texts);
        let mut taken: Vec<String> = Vec::new();
        for (mi, module) in section.modules.iter().enumerate() {
            let mod_folder = planner::module_folder(mi, &module.name, &mut taken);
            module_ids.insert(
                (sec_folder.clone(), mod_folder),
                (module.id, module.name.clone()),
            );
        }
    }
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
            let (module_id, module_name) = module_ids
                .get(&key)
                .cloned()
                .unwrap_or((module.id, module.name.clone()));
            indexer::write_module_meta(&module.dir, module_id, &module_name, files)?;
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
