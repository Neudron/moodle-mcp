//! Blob store with flat and content-addressed layouts (Task 4).
//!
//! Layouts (plan #996–#1000):
//! - `flat` (default, #999): blobs live directly in `.moodle/blobs/<sha>`;
//!   [`Store::link_into`] copies bytes to the destination. Historic sync
//!   behaviour is unchanged — nothing is shared between paths.
//! - `content-addressed`: blobs live in `.moodle/blobs/ab/cd/<sha>`
//!   (`ab`/`cd` = first nibbles, #997); `link_into` hardlinks the blob
//!   into place and falls back to a copy across devices.
//!
//! Blobs are content-named (SHA-256 hex) and stored read-only (`0444`):
//! every link to a blob must observe identical bytes, so mutation through
//! a linked path is refused by permissions. Note this means hardlinked
//! destinations are read-only too (same inode); the flat layout copies
//! with `0644` instead and never sets the executable bit (#1063).
//! [`Store::gc_orphans`] deletes blobs no live state entry references (#996).
//! The chosen layout is recorded in state meta (#1000) so later runs and
//! the CLI `--store` switch (Task 9) can resolve one effective layout via
//! [`resolve_layout`]: explicit flag > config file > state meta > `flat`.

use std::collections::{BTreeSet, HashSet};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

/// Blob-store layout switch (`--store flat|content-addressed`, #998).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StoreLayout {
    Flat,
    ContentAddressed,
}

impl StoreLayout {
    /// Parse a layout name; `None` for anything else.
    #[must_use]
    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "flat" => Some(Self::Flat),
            "content-addressed" => Some(Self::ContentAddressed),
            _ => None,
        }
    }

    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Flat => "flat",
            Self::ContentAddressed => "content-addressed",
        }
    }
}

/// Resolve the effective layout: explicit CLI flag > config file value >
/// state meta value > `flat` default. Invalid names are skipped.
/// (CLI wiring is Task 9; this pure function is the shared contract.)
#[must_use]
pub fn resolve_layout(flag: Option<&str>, config_store: &str, meta_store: &str) -> StoreLayout {
    for candidate in [flag.unwrap_or(""), config_store, meta_store] {
        if let Some(layout) = StoreLayout::parse(candidate) {
            return layout;
        }
    }
    StoreLayout::Flat
}

/// Outcome of [`Store::gc_orphans`].
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct GcReport {
    pub removed: usize,
    pub bytes: u64,
}

pub struct Store {
    root: PathBuf,
    layout: StoreLayout,
}

fn sha256_hex(bytes: &[u8]) -> String {
    use sha2::Digest as _;
    format!("{:x}", sha2::Sha256::digest(bytes))
}

/// True for a blob name: 64 lowercase hex chars.
fn is_sha_name(name: &str) -> bool {
    name.len() == 64 && name.bytes().all(|b| b.is_ascii_hexdigit())
}

fn normalise_sha(sha: &str) -> Result<String> {
    let lower = sha.to_ascii_lowercase();
    if is_sha_name(&lower) {
        Ok(lower)
    } else {
        anyhow::bail!("invalid blob sha (expected 64 hex chars)")
    }
}

#[cfg(unix)]
fn set_mode(path: &Path, mode: u32) -> Result<()> {
    use std::os::unix::fs::PermissionsExt as _;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode))
        .with_context(|| format!("chmod {}", path.display()))
}

#[cfg(not(unix))]
fn set_mode(_path: &Path, _mode: u32) -> Result<()> {
    Ok(())
}

impl Store {
    fn blobs_dir(root: &Path) -> PathBuf {
        root.join(".moodle/blobs")
    }

    /// Open with the layout recorded in state meta, `flat` when the state
    /// is missing or records nothing (#999, #1000). Never creates a state.
    pub fn open(root: &str) -> Result<Self> {
        let state = crate::state::State::load(root)?;
        let layout = StoreLayout::parse(state.meta.store.as_str()).unwrap_or(StoreLayout::Flat);
        Ok(Self {
            root: PathBuf::from(root),
            layout,
        })
    }

    /// Open with an explicit layout (already resolved, e.g. from a CLI flag
    /// via [`resolve_layout`]).
    #[must_use]
    pub fn open_with_layout(root: &str, layout: StoreLayout) -> Self {
        Self {
            root: PathBuf::from(root),
            layout,
        }
    }

    #[must_use]
    pub fn layout(&self) -> StoreLayout {
        self.layout
    }

    /// On-disk blob path for `sha` under this store's layout.
    pub fn blob_path(&self, sha: &str) -> Result<PathBuf> {
        let sha = normalise_sha(sha)?;
        let base = Self::blobs_dir(&self.root);
        match self.layout {
            StoreLayout::Flat => Ok(base.join(sha)),
            StoreLayout::ContentAddressed => Ok(base.join(&sha[..2]).join(&sha[2..4]).join(sha)),
        }
    }

    /// Store bytes under their SHA-256 name (idempotent: an existing blob
    /// is trusted — blobs are immutable). Returns the hex digest.
    pub fn put_blob(&self, bytes: &[u8]) -> Result<String> {
        let sha = sha256_hex(bytes);
        let dest = self.blob_path(&sha)?;
        if dest.is_file() {
            return Ok(sha);
        }
        if let Some(parent) = dest.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("creating {}", parent.display()))?;
        }
        let tmp = dest.with_extension("tmp");
        std::fs::write(&tmp, bytes).with_context(|| format!("writing {}", tmp.display()))?;
        set_mode(&tmp, 0o444)?;
        // A racing put_blob may win the rename first; either way the name
        // holds identical bytes, so ignore "already exists" outcomes.
        match std::fs::rename(&tmp, &dest) {
            Ok(()) => {}
            Err(e) if dest.is_file() => {
                let _ = std::fs::remove_file(&tmp);
                let _ = e;
            }
            Err(e) => {
                let _ = std::fs::remove_file(&tmp);
                anyhow::bail!("publishing {}: {e}", dest.display());
            }
        }
        set_mode(&dest, 0o444)?;
        Ok(sha)
    }

    /// Place blob `sha` at `dest_rel` (relative to the root, jailed):
    /// hardlink in content-addressed mode (copy fallback across devices),
    /// plain copy in flat mode. Returns the absolute destination.
    pub fn link_into(&self, sha: &str, dest_rel: &str) -> Result<PathBuf> {
        let blob = self.blob_path(sha)?;
        if !blob.is_file() {
            anyhow::bail!("unknown blob {sha}");
        }
        if Path::new(dest_rel).is_absolute() || dest_rel.split('/').any(|seg| seg == "..") {
            anyhow::bail!("refusing to link outside the root: {dest_rel}");
        }
        let dest = self.root.join(dest_rel);
        if let Some(parent) = dest.parent() {
            if !parent.as_os_str().is_empty() {
                std::fs::create_dir_all(parent)
                    .with_context(|| format!("creating {}", parent.display()))?;
            }
        }
        // Replace any previous file at the destination.
        let _ = std::fs::remove_file(&dest);
        match self.layout {
            StoreLayout::ContentAddressed => {
                if std::fs::hard_link(&blob, &dest).is_err() {
                    // Cross-device or unsupported FS: copy fallback.
                    std::fs::copy(&blob, &dest)
                        .with_context(|| format!("copying blob to {}", dest.display()))?;
                    set_mode(&dest, 0o644)?;
                }
            }
            StoreLayout::Flat => {
                std::fs::copy(&blob, &dest)
                    .with_context(|| format!("copying blob to {}", dest.display()))?;
                set_mode(&dest, 0o644)?;
            }
        }
        Ok(dest)
    }

    /// Delete blobs not in `live_shas` (#996). Files whose names are not
    /// blob SHAs are left alone (forward compat); emptied `ab/cd` dirs are
    /// removed best-effort.
    pub fn gc_orphans(&self, live_shas: &HashSet<String>) -> Result<GcReport> {
        let live: BTreeSet<String> = live_shas.iter().map(|s| s.to_ascii_lowercase()).collect();
        let mut report = GcReport::default();
        let base = Self::blobs_dir(&self.root);
        let mut stack = vec![base.clone()];
        let mut dirs = Vec::new();
        while let Some(dir) = stack.pop() {
            let Ok(entries) = std::fs::read_dir(&dir) else {
                continue;
            };
            dirs.push(dir);
            for entry in entries.filter_map(|e| e.ok()) {
                let file_type = entry.file_type().map(|t| t.is_dir()).unwrap_or(false);
                if file_type {
                    stack.push(entry.path());
                    continue;
                }
                let name = entry.file_name().to_string_lossy().into_owned();
                if is_sha_name(&name) && !live.contains(&name) {
                    let size = entry.metadata().map(|m| m.len()).unwrap_or(0);
                    if std::fs::remove_file(entry.path()).is_ok() {
                        report.removed += 1;
                        report.bytes += size;
                    }
                }
            }
        }
        for dir in dirs {
            if dir != base {
                let _ = std::fs::remove_dir(dir);
            }
        }
        Ok(report)
    }

    /// Persist this store's layout into state meta (#1000) through the
    /// locked state writer.
    pub fn record_layout(&self) -> Result<()> {
        let root = self.root.display().to_string();
        let mut state = crate::state::State::load(&root)?;
        state.meta.store = self.layout.as_str().to_string();
        state.save_locked(&root)?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn store_layout_parse_and_resolve() {
        assert_eq!(StoreLayout::parse("flat"), Some(StoreLayout::Flat));
        assert_eq!(
            StoreLayout::parse("content-addressed"),
            Some(StoreLayout::ContentAddressed)
        );
        assert_eq!(StoreLayout::parse("tape"), None);
        assert_eq!(StoreLayout::parse(""), None);
        // Explicit flag wins over config over meta over default.
        assert_eq!(
            resolve_layout(Some("flat"), "content-addressed", "content-addressed"),
            StoreLayout::Flat
        );
        assert_eq!(
            resolve_layout(None, "content-addressed", "flat"),
            StoreLayout::ContentAddressed
        );
        assert_eq!(
            resolve_layout(None, "bogus", "content-addressed"),
            StoreLayout::ContentAddressed
        );
        assert_eq!(resolve_layout(None, "", ""), StoreLayout::Flat);
    }

    #[test]
    fn store_blob_paths_per_layout() {
        let sha = "abcd".to_string() + &"ef01".repeat(15);
        assert_eq!(sha.len(), 64);
        let flat = Store::open_with_layout("/r", StoreLayout::Flat);
        assert_eq!(
            flat.blob_path(&sha).expect("path"),
            PathBuf::from(format!("/r/.moodle/blobs/{sha}"))
        );
        let ca = Store::open_with_layout("/r", StoreLayout::ContentAddressed);
        assert_eq!(
            ca.blob_path(&sha).expect("path"),
            PathBuf::from(format!("/r/.moodle/blobs/ab/cd/{sha}"))
        );
        assert!(flat.blob_path("xyz").is_err());
        assert!(flat.blob_path(&"g".repeat(64)).is_err());
    }
}
