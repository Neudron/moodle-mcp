use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

#[derive(Debug, Default, Serialize, Deserialize)]
pub struct State {
    #[serde(default = "default_schema")]
    pub schema_version: u32,
    #[serde(default)]
    pub user_id: i64,
    #[serde(default)]
    pub courses: BTreeMap<String, CourseState>,
}

fn default_schema() -> u32 {
    1
}

#[derive(Debug, Default, Serialize, Deserialize)]
pub struct CourseState {
    #[serde(default)]
    pub fullname: String,
    #[serde(default)]
    pub shortname: String,
    #[serde(default)]
    pub synced_at: String,
    #[serde(default)]
    pub sections: BTreeMap<String, String>,
    #[serde(default)]
    pub files: BTreeMap<String, FileEntry>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FileEntry {
    pub path: String,
    pub sha256: String,
    pub size: i64,
}

pub fn state_path(root: &str) -> PathBuf {
    Path::new(root).join(".moodle/state.json")
}

impl State {
    pub fn load(root: &str) -> Result<Self> {
        let p = state_path(root);
        if !p.exists() {
            return Ok(State::default());
        }
        let raw =
            std::fs::read_to_string(&p).with_context(|| format!("reading {}", p.display()))?;
        serde_json::from_str(&raw).with_context(|| format!("parsing {}", p.display()))
    }

    pub fn save(&self, root: &str) -> Result<()> {
        let p = state_path(root);
        if let Some(dir) = p.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let tmp = p.with_extension("json.tmp");
        std::fs::write(&tmp, serde_json::to_string_pretty(self)?)?;
        std::fs::rename(&tmp, &p)?;
        Ok(())
    }
}

use crate::moodle::Course;

impl CourseState {
    pub fn upsert(&mut self, c: &Course) {
        self.fullname = c.fullname.clone();
        self.shortname = c.shortname.clone();
    }
}
