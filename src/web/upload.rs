//! Moodle submission: draft upload + explicit submit (spec: Upload to Moodle).
use crate::errors::CoreError;
use crate::moodle::Moodle;
use crate::web::files;
use serde_json::Value;
use std::path::{Path, PathBuf};

pub fn resolve(root: &Path, rel_or_name: &str) -> Option<PathBuf> {
    if let Ok(p) = files::jail(root, rel_or_name) {
        return p.is_file().then_some(p);
    }
    if rel_or_name.contains('/') || rel_or_name.starts_with('.') {
        return None;
    }
    let mut all = Vec::new();
    files::walk(root, "submit", &mut all);
    all.into_iter()
        .find(|r| r.rsplit('/').next() == Some(rel_or_name))
        .and_then(|r| files::jail(root, &r).ok())
}

pub async fn save_draft(m: &Moodle, assign_id: i64, paths: &[PathBuf]) -> Result<Value, CoreError> {
    if paths.is_empty() {
        return Err(CoreError::Invalid("cap fitxer".into()));
    }
    let mut itemid = 0;
    for p in paths {
        let bytes = tokio::fs::read(p)
            .await
            .map_err(|e| CoreError::Invalid(e.to_string()))?;
        let name = p
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();
        itemid = m.upload_draft(itemid, &name, bytes).await?;
    }
    m.call_with_retry(
        "mod_assign_save_submission",
        &[
            ("assignmentid", assign_id.to_string()),
            ("plugindata[files_filemanager]", itemid.to_string()),
        ],
    )
    .await?;
    m.call_with_retry(
        "mod_assign_get_submission_status",
        &[("assignid", assign_id.to_string())],
    )
    .await
}

pub async fn submit_for_grading(m: &Moodle, assign_id: i64) -> Result<Value, CoreError> {
    m.call_with_retry(
        "mod_assign_submit_for_grading",
        &[
            ("assignmentid", assign_id.to_string()),
            ("acceptsubmissionstatement", "1".into()),
        ],
    )
    .await?;
    m.call_with_retry(
        "mod_assign_get_submission_status",
        &[("assignid", assign_id.to_string())],
    )
    .await
}
