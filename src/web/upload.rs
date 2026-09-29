//! Moodle submission: draft upload + explicit submit (spec: Upload to Moodle).
use crate::errors::CoreError;
use crate::moodle::Moodle;
use crate::web::files;
use serde_json::Value;
use std::path::{Path, PathBuf};

/// Only jailed rel paths that are files; a bare name is never guessed (it could hit another task's file).
pub fn resolve(root: &Path, rel: &str) -> Option<PathBuf> {
    files::jail(root, rel).ok().filter(|p| p.is_file())
}

/// Moodle reports refusals as HTTP 200 + `warnings`, not as exceptions.
fn check(res: &Value) -> Result<(), CoreError> {
    match res.as_array().filter(|a| !a.is_empty()) {
        None => Ok(()),
        Some(w) => Err(CoreError::Invalid(crate::errors::redact_token(
            &w.iter()
                .map(|x| x["message"].as_str().unwrap_or("avís de Moodle"))
                .collect::<Vec<_>>()
                .join("; "),
        ))),
    }
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
    let res = m
        .call_with_retry(
            "mod_assign_save_submission",
            &[
                ("assignmentid", assign_id.to_string()),
                ("plugindata[files_filemanager]", itemid.to_string()),
            ],
        )
        .await?;
    check(&res)?;
    m.call_with_retry(
        "mod_assign_get_submission_status",
        &[("assignid", assign_id.to_string())],
    )
    .await
}

pub async fn submit_for_grading(m: &Moodle, assign_id: i64) -> Result<Value, CoreError> {
    let res = m
        .call_with_retry(
            "mod_assign_submit_for_grading",
            &[
                ("assignmentid", assign_id.to_string()),
                ("acceptsubmissionstatement", "1".into()),
            ],
        )
        .await?;
    check(&res)?;
    m.call_with_retry(
        "mod_assign_get_submission_status",
        &[("assignid", assign_id.to_string())],
    )
    .await
}
