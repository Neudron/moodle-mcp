//! `.env` loading (spec: "Secrets and config: .env").
use anyhow::{anyhow, bail, Result};
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
    // Never echo the raw line: it may hold a secret.
    dotenvy::from_path(&path).map_err(|e| match e {
        dotenvy::Error::LineParse(l, i) => anyhow!(
            "{}: parse error at index {i} (key {})",
            path.display(),
            l.split('=').next().unwrap_or("?")
        ),
        other => anyhow!("{}: {other}", path.display()),
    })?;
    Ok(())
}

/// Non-empty env value.
pub fn get(key: &str) -> Option<String> {
    std::env::var(key).ok().filter(|v| !v.trim().is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    #[test]
    fn parse_error_hides_secret() {
        let dir = std::env::temp_dir().join(format!("moodle-env-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let f = dir.join(".env");
        std::fs::write(&f, "K=\"secretvalue\n").unwrap();
        std::fs::set_permissions(&f, std::fs::Permissions::from_mode(0o600)).unwrap();
        let err = load(dir.to_str().unwrap()).unwrap_err().to_string();
        std::fs::remove_dir_all(&dir).ok();
        assert!(!err.contains("secretvalue"), "{err}");
        assert!(err.contains("key K"), "{err}");
    }
}
