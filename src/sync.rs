use crate::html2md::html_to_md;
use crate::ra;
use crate::state::{FileEntry, State};
use anyhow::{Context, Result};
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

#[derive(serde::Serialize)]
pub struct SyncReport {
    pub course_id: i64,
    pub shortname: String,
    pub files_new: usize,
    pub files_skipped: usize,
    pub errors: Vec<String>,
}

fn sha256_hex(bytes: &[u8]) -> String {
    let mut h = Sha256::new();
    h.update(bytes);
    format!("{:x}", h.finalize())
}

fn unique_path(dir: &Path, name: &str) -> PathBuf {
    let mut candidate = dir.join(name);
    let mut n = 2;
    while candidate.exists() {
        let stem = Path::new(name)
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or("file");
        let ext = Path::new(name)
            .extension()
            .and_then(|s| s.to_str())
            .map(|e| format!(".{e}"))
            .unwrap_or_default();
        candidate = dir.join(format!("{stem}-{n}{ext}"));
        n += 1;
    }
    candidate
}

fn module_folder(index: usize, name: &str, taken: &mut Vec<String>) -> String {
    let mut base = ra::sanitize(name);
    if base.len() > 60 {
        base = base.chars().take(60).collect();
    }
    let mut candidate = format!("{:03}-{}", index + 1, base);
    let mut n = 2;
    while taken.contains(&candidate) {
        candidate = format!("{:03}-{}-{n}", index + 1, base);
        n += 1;
    }
    taken.push(candidate.clone());
    candidate
}

pub async fn sync_course(
    moodle: &crate::moodle::Moodle,
    state: &mut State,
    course: &crate::moodle::Course,
    root: &str,
) -> Result<SyncReport> {
    let mut report = SyncReport {
        course_id: course.id,
        shortname: course.shortname.clone(),
        files_new: 0,
        files_skipped: 0,
        errors: vec![],
    };
    let course_dir_name = {
        let sn = if course.shortname.is_empty() {
            format!("course-{}", course.id)
        } else {
            ra::sanitize(&course.shortname)
        };
        sn
    };
    let course_dir = Path::new(root).join(&course_dir_name);
    std::fs::create_dir_all(&course_dir)?;

    let sections = moodle.contents(course.id).await.context("get contents")?;

    let mut cs = state
        .courses
        .remove(&course.id.to_string())
        .unwrap_or_default();
    cs.upsert(course);
    cs.synced_at = chrono_now();
    let mut new_files: BTreeMap<String, FileEntry> = BTreeMap::new();
    let mut kept_paths: Vec<String> = vec![];
    let mut index_rows: Vec<String> = vec![];

    for (si, section) in sections.iter().enumerate() {
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
        let sec_folder_name = ra::detect(&section.name, &module_texts).unwrap_or_else(|| {
            if si == 0 {
                "00-intro".into()
            } else {
                "99-misc".into()
            }
        });
        // skip sections with nothing to place (empty dirs pollute the tree)
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
        let sec_dir = course_dir.join(ra::sanitize(&sec_folder_name));
        std::fs::create_dir_all(&sec_dir)?;
        cs.sections
            .insert(section.id.to_string(), sec_folder_name.clone());

        if !section.summary.trim().is_empty() {
            std::fs::write(
                sec_dir.join("section-summary.md"),
                html_to_md(&section.summary),
            )?;
        }

        let mut taken: Vec<String> = vec![];
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
            if !has_files && !has_desc {
                continue;
            }
            let mod_dir = sec_dir.join(module_folder(mi, &module.name, &mut taken));
            std::fs::create_dir_all(&mod_dir)?;

            if let Some(desc) = &module.description {
                if !desc.trim().is_empty() {
                    std::fs::write(mod_dir.join("README.md"), html_to_md(desc))?;
                }
            }

            let files = module.contents.clone().unwrap_or_default();
            for f in files {
                let Some(url) = f.fileurl.clone() else {
                    continue;
                };
                if f.filesize == 0 && f.filename.is_empty() {
                    continue;
                }
                if !url.contains("/pluginfile.php") {
                    // external link (Google Docs/Drive, etc.) — pointer file, not error
                    let mut pname = ra::sanitize(&f.filename);
                    if !pname.ends_with(".url") {
                        pname.push_str(".url");
                    }
                    let dest = unique_path(&mod_dir, &pname);
                    std::fs::write(&dest, format!("[InternetShortcut]\nURL={url}\n"))?;
                    index_rows.push(format!(
                        "| {} | {} | [link: {}]({}) | - |",
                        sec_folder_name, module.name, f.filename, url
                    ));
                    continue;
                }
                match moodle.download(&url).await {
                    Ok(bytes) => {
                        let hash = sha256_hex(&bytes);
                        let prev = cs.files.get(&url);
                        let same = prev.map(|e| e.sha256 == hash).unwrap_or(false)
                            && Path::new(root).join(&prev.unwrap().path).exists();
                        let dest_rel = if same {
                            // reuse existing path
                            cs.files.get(&url).unwrap().path.clone()
                        } else {
                            let dest = unique_path(&mod_dir, &ra::sanitize(&f.filename));
                            std::fs::write(&dest, &bytes)?;
                            let rel = dest
                                .strip_prefix(root)
                                .unwrap_or(&dest)
                                .display()
                                .to_string();
                            kept_paths.push(rel.clone());
                            rel
                        };
                        if same {
                            report.files_skipped += 1;
                        } else {
                            report.files_new += 1;
                        }
                        new_files.insert(
                            url.clone(),
                            FileEntry {
                                path: dest_rel,
                                sha256: hash,
                                size: f.filesize,
                            },
                        );
                        index_rows.push(format!(
                            "| {} | {} | {} | {} |",
                            sec_folder_name, module.name, f.filename, f.filesize
                        ));
                    }
                    Err(e) => {
                        report.errors.push(format!("{}: {e}", f.filename));
                    }
                }
            }
        }
    }

    // index.md
    let mut idx = String::new();
    idx.push_str(&format!("# {}\n\n", course.fullname));
    idx.push_str(&format!(
        "Moodle course id: {}. Shortname: `{}`.\n\n",
        course.id, course.shortname
    ));
    if !course.summary.trim().is_empty() {
        idx.push_str(&html_to_md(&course.summary));
        idx.push_str("\n\n");
    }
    idx.push_str("| Section | Module | File | Size |\n|---|---|---|---|\n");
    for row in &index_rows {
        idx.push_str(row);
        idx.push('\n');
    }
    std::fs::write(course_dir.join("index.md"), idx)?;

    cs.files = new_files;
    state.courses.insert(course.id.to_string(), cs);
    Ok(report)
}

fn chrono_now() -> String {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default();
    format!("{}", now.as_secs())
}
