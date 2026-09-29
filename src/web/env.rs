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
    // Never echo any part of the raw line: it may hold a secret. Report line number only.
    dotenvy::from_path(&path).map_err(|e| match e {
        dotenvy::Error::LineParse(l, _) => {
            let n = std::fs::read_to_string(&path)
                .ok()
                .and_then(|c| c.lines().position(|x| x.trim() == l.trim()))
                .map_or("?".to_string(), |i| (i + 1).to_string());
            anyhow!("{}: parse error on line {n}", path.display())
        }
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

    fn err_for(content: &str, tag: &str) -> String {
        let dir = std::env::temp_dir().join(format!("moodle-env-{tag}-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let f = dir.join(".env");
        std::fs::write(&f, content).unwrap();
        std::fs::set_permissions(&f, std::fs::Permissions::from_mode(0o600)).unwrap();
        let err = load(dir.to_str().unwrap()).unwrap_err().to_string();
        std::fs::remove_dir_all(&dir).ok();
        err
    }

    #[test]
    fn parse_error_hides_secret() {
        let e = err_for("K=\"secretvalue\n", "a");
        assert!(!e.contains("secretvalue"), "{e}");
        assert!(e.contains("line 1"), "{e}");
        let e = err_for("MOODLE_URL=x\ntok0123456789abcdefSECRET\n", "b");
        assert!(!e.contains("SECRET"), "{e}");
        assert!(e.contains("line 2"), "{e}");
        let e = err_for("MOODLE_TOKEN abcdef0123SECRET\n", "c");
        assert!(!e.contains("SECRET") && !e.contains("MOODLE_TOKEN"), "{e}");
    }
}
