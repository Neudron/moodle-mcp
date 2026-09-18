//! Course indexer (Task 3, P2 sync engine).
//!
//! Writes the human-readable tree artifacts after the executor finishes:
//! course `index.md` with YAML frontmatter, per-module `meta.json`
//! (`id, urls, sizes, timemodified`), per-section `meta.json` rollups, and
//! the `.md` companions for external `.url` pointers (Drive/YouTube
//! annotated). Only the `.url` files are tracked in `state.files`; the
//! `.md` companions are regenerated every sync.

use std::path::Path;

use anyhow::{Context, Result};

use crate::moodle::Course;

/// One row of the course file table.
#[derive(Debug, Clone)]
pub struct IndexRow {
    pub section: String,
    pub module: String,
    pub original: String,
    pub stored: String,
    pub size: i64,
    pub url: String,
    pub is_external: bool,
}

/// File metadata recorded in per-module `meta.json`.
#[derive(Debug, Clone, serde::Serialize)]
pub struct ModuleFileMeta {
    pub filename: String,
    pub stored: String,
    pub url: String,
    pub size: i64,
    pub timemodified: i64,
    pub mimetype: String,
}

fn yaml_escape(value: &str) -> String {
    value.replace('"', "'")
}

fn file_cell(row: &IndexRow) -> String {
    if row.is_external {
        return format!("[link: {}]({})", row.original, row.url);
    }
    if row.original != row.stored {
        format!("{} (original: {})", row.stored, row.original)
    } else {
        row.original.clone()
    }
}

/// Render the course `index.md` with YAML frontmatter.
#[must_use]
pub fn render_index_md(
    course: &Course,
    summary_md: &str,
    rows: &[IndexRow],
    synced_at_iso: &str,
) -> String {
    let mut out = String::new();
    out.push_str("---\n");
    out.push_str(&format!("title: \"{}\"\n", yaml_escape(&course.fullname)));
    out.push_str(&format!("course_id: {}\n", course.id));
    out.push_str(&format!(
        "shortname: \"{}\"\n",
        yaml_escape(&course.shortname)
    ));
    out.push_str(&format!("synced_at: \"{synced_at_iso}\"\n"));
    out.push_str(&format!("files: {}\n", rows.len()));
    out.push_str("---\n\n");
    out.push_str(&format!("# {}\n\n", course.fullname));
    out.push_str(&format!(
        "Moodle course id: {}. Shortname: `{}`.\n\n",
        course.id, course.shortname
    ));
    if !summary_md.trim().is_empty() {
        out.push_str(summary_md.trim());
        out.push_str("\n\n");
    }
    out.push_str("| Section | Module | File | Size |\n|---|---|---|---|\n");
    for row in rows {
        out.push_str(&format!(
            "| {} | {} | {} | {} |\n",
            row.section,
            row.module,
            file_cell(row),
            row.size
        ));
    }
    out
}

/// Write `index.md` (frontmatter + table) into the course dir.
pub fn write_course_index(
    course_dir: &Path,
    course: &Course,
    summary_md: &str,
    rows: &[IndexRow],
    synced_at_iso: &str,
) -> Result<()> {
    let body = render_index_md(course, summary_md, rows, synced_at_iso);
    std::fs::write(course_dir.join("index.md"), body).context("writing index.md")?;
    Ok(())
}

/// Write per-module `meta.json` (`id, urls, sizes, timemodified`).
pub fn write_module_meta(
    mod_dir: &Path,
    module_id: i64,
    module_name: &str,
    files: &[ModuleFileMeta],
) -> Result<()> {
    let doc = serde_json::json!({
        "id": module_id,
        "name": module_name,
        "files": files,
    });
    let text = serde_json::to_string_pretty(&doc).context("serialising module meta")?;
    std::fs::write(mod_dir.join("meta.json"), text).context("writing module meta.json")?;
    Ok(())
}

/// Write per-section `meta.json` rollup.
pub fn write_section_meta(
    sec_dir: &Path,
    section_id: i64,
    section_name: &str,
    folder: &str,
    modules: usize,
    files: usize,
    bytes: u64,
) -> Result<()> {
    let doc = serde_json::json!({
        "id": section_id,
        "name": section_name,
        "folder": folder,
        "modules": modules,
        "files": files,
        "bytes": bytes,
    });
    let text = serde_json::to_string_pretty(&doc).context("serialising section meta")?;
    std::fs::write(sec_dir.join("meta.json"), text).context("writing section meta.json")?;
    Ok(())
}

/// `.md` companion path for a `.url` pointer rel path.
#[must_use]
pub fn pointer_md_rel(url_rel: &str) -> Option<String> {
    url_rel
        .strip_suffix(".url")
        .map(|base| format!("{base}.md"))
}

/// Source label for an external URL (Drive/YouTube annotated).
#[must_use]
pub fn external_source_label(url: &str) -> &'static str {
    let lower = url.to_ascii_lowercase();
    if lower.contains("drive.google.com") || lower.contains("docs.google.com") {
        "Google Drive/Docs"
    } else if lower.contains("youtube.com") || lower.contains("youtu.be") {
        "YouTube"
    } else {
        "external link"
    }
}

/// Source icon for an external URL.
#[must_use]
pub fn external_icon(url: &str) -> &'static str {
    let lower = url.to_ascii_lowercase();
    if lower.contains("drive.google.com") || lower.contains("docs.google.com") {
        "📄"
    } else if lower.contains("youtube.com") || lower.contains("youtu.be") {
        "🎬"
    } else {
        "🔗"
    }
}

fn youtube_id(url: &str) -> Option<String> {
    // ?v=ID form.
    if let Some(query) = url.split('?').nth(1) {
        for pair in query.split('&') {
            if let Some(id) = pair.strip_prefix("v=") {
                let id: String = id
                    .chars()
                    .filter(|c| c.is_ascii_alphanumeric() || *c == '-' || *c == '_')
                    .take(11)
                    .collect();
                if id.len() == 11 {
                    return Some(id);
                }
            }
        }
    }
    // youtu.be/ID and /embed/ID forms.
    for marker in ["youtu.be/", "/embed/"] {
        if let Some(rest) = url.split(marker).nth(1) {
            let id: String = rest
                .split(['?', '#', '/', '&'])
                .next()
                .unwrap_or("")
                .chars()
                .filter(|c| c.is_ascii_alphanumeric() || *c == '-' || *c == '_')
                .take(11)
                .collect();
            if id.len() == 11 {
                return Some(id);
            }
        }
    }
    None
}

/// Markdown companion for an external `.url` pointer.
#[must_use]
pub fn external_pointer_md(filename: &str, url: &str) -> String {
    let mut out = String::new();
    out.push_str(&format!("# {} {} \n\n", external_icon(url), filename));
    out.push_str(&format!(
        "{} — [open]({url})\n\n",
        external_source_label(url)
    ));
    if external_source_label(url) == "YouTube" {
        if let Some(id) = youtube_id(url) {
            out.push_str(&format!(
                "![thumbnail](https://img.youtube.com/vi/{id}/0.jpg)\n\n"
            ));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn indexer_frontmatter_and_cells() {
        let course = Course {
            id: 7,
            fullname: "Test \"Course\"".to_string(),
            shortname: "TC".to_string(),
            summary: String::new(),
        };
        let rows = vec![
            IndexRow {
                section: "RA01".to_string(),
                module: "m".to_string(),
                original: "a b.pdf".to_string(),
                stored: "a b.pdf".to_string(),
                size: 10,
                url: "http://h.invalid/f/a.pdf".to_string(),
                is_external: false,
            },
            IndexRow {
                section: "RA01".to_string(),
                module: "m".to_string(),
                original: "weird:name.pdf".to_string(),
                stored: "weirdname.pdf".to_string(),
                size: 10,
                url: "http://h.invalid/f/b.pdf".to_string(),
                is_external: false,
            },
            IndexRow {
                section: "RA02".to_string(),
                module: "n".to_string(),
                original: "slides".to_string(),
                stored: "slides.url".to_string(),
                size: 30,
                url: "https://docs.google.com/x".to_string(),
                is_external: true,
            },
        ];
        let md = render_index_md(&course, "hello", &rows, "2026-09-18T00:00:00Z");
        assert!(md.starts_with("---\n"), "frontmatter missing: {md}");
        assert!(md.contains("course_id: 7"));
        assert!(md.contains("Test 'Course'"), "quotes must be escaped");
        assert!(md.contains("weirdname.pdf (original: weird:name.pdf)"));
        assert!(md.contains("[link: slides](https://docs.google.com/x)"));
    }

    #[test]
    fn indexer_pointer_paths_and_labels() {
        assert_eq!(
            pointer_md_rel("C/RA01/001-m/slides.url").as_deref(),
            Some("C/RA01/001-m/slides.md")
        );
        assert_eq!(pointer_md_rel("C/RA01/a.pdf"), None);
        assert_eq!(
            external_source_label("https://drive.google.com/f/1"),
            "Google Drive/Docs"
        );
        assert_eq!(
            external_source_label("https://youtu.be/dQw4w9WgXcQ"),
            "YouTube"
        );
        assert_eq!(
            external_source_label("https://example.invalid/x"),
            "external link"
        );
        let md = external_pointer_md("vid", "https://www.youtube.com/watch?v=dQw4w9WgXcQ");
        assert!(md.contains("img.youtube.com/vi/dQw4w9WgXcQ/0.jpg"), "{md}");
    }
}
