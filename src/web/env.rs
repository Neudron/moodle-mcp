//! `.env` loading (spec: "Secrets and config: .env").
use anyhow::{bail, Context, Result};
use std::os::unix::fs::PermissionsExt;
use std::path::Path;

/// Load `<root>/.env` if it exists. Existing process env vars win
/// (dotenvy never overrides). Refuses a file readable by group/other.
pub fn load(root: &str) -> Result<()> {
    let path = Path::new(root).join(".env");
    let Ok(meta) = std::fs::metadata(&path) else {
        return Ok(());
    };
    let mode = meta.permissions().mode() & 0o777;
    if mode & 0o077 != 0 {
        bail!(
            "{} has mode {mode:o}; run: chmod 600 {}",
            path.display(),
            path.display()
        );
    }
    dotenvy::from_path(&path).with_context(|| format!("parsing {}", path.display()))?;
    Ok(())
}

/// Non-empty env value.
pub fn get(key: &str) -> Option<String> {
    std::env::var(key).ok().filter(|v| !v.trim().is_empty())
}
