//! File tree under MOODLE_ROOT with a strict jail: no dot components
//! (covers `..`, `.env`, `.moodle`), canonical path must stay under root.
use serde::Serialize;
use std::path::{Component, Path, PathBuf};

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct Entry {
    pub name: String,
    pub rel: String,
    pub is_dir: bool,
    pub size: u64,
    pub modified: i64,
}

fn hidden(p: &Path) -> bool {
    p.components().any(|c| match c {
        Component::Normal(s) => s.to_string_lossy().starts_with('.'),
        Component::ParentDir => true,
        _ => false,
    })
}

/// Browse/pick root: MOODLE_PICK_HOME (tests) else HOME, canonical.
pub fn home() -> PathBuf {
    static H: std::sync::OnceLock<PathBuf> = std::sync::OnceLock::new();
    H.get_or_init(|| {
        let h = std::env::var("MOODLE_PICK_HOME")
            .or_else(|_| std::env::var("HOME"))
            .unwrap_or_default();
        PathBuf::from(&h).canonicalize().unwrap_or_default()
    })
    .clone()
}

pub fn jail(root: &Path, rel: &str) -> Result<PathBuf, String> {
    jail_in(root, rel, &home())
}

/// Like `jail`, but a `submit/...` symlink may point at a non-hidden path under `home`.
pub fn jail_in(root: &Path, rel: &str, home: &Path) -> Result<PathBuf, String> {
    let relp = Path::new(rel);
    if relp.is_absolute() || hidden(relp) {
        return Err("path not allowed".into());
    }
    let root = root.canonicalize().map_err(|e| e.to_string())?;
    let p = root
        .join(relp)
        .canonicalize()
        .map_err(|_| "not found".to_string())?;
    let Ok(inside) = p.strip_prefix(&root) else {
        let in_submit = relp
            .components()
            .next()
            .is_some_and(|c| c.as_os_str() == "submit");
        return match home
            .canonicalize()
            .ok()
            .as_deref()
            .and_then(|h| p.strip_prefix(h).ok())
        {
            Some(in_home) if in_submit && !hidden(in_home) => Ok(p),
            _ => Err("path not allowed".into()),
        };
    };
    if hidden(inside) {
        return Err("path not allowed".into());
    }
    Ok(p)
}

pub fn list(root: &Path, rel: &str) -> Result<Vec<Entry>, String> {
    let dir = jail(root, rel)?;
    let croot = root.canonicalize().map_err(|e| e.to_string())?;
    let mut out = Vec::new();
    for e in std::fs::read_dir(&dir)
        .map_err(|e| e.to_string())?
        .flatten()
    {
        let name = e.file_name().to_string_lossy().into_owned();
        // symlinks skipped: no cycles in walk, no metadata leak from targets
        if name.starts_with('.') || e.file_type().is_ok_and(|t| t.is_symlink()) {
            continue;
        }
        let Ok(meta) = std::fs::metadata(e.path()) else {
            continue;
        };
        let modified = meta
            .modified()
            .ok()
            .and_then(|m| m.duration_since(std::time::UNIX_EPOCH).ok())
            .map_or(0, |d| d.as_secs() as i64);
        let full = dir.join(&name);
        out.push(Entry {
            rel: full
                .strip_prefix(&croot)
                .unwrap_or(&full)
                .to_string_lossy()
                .into_owned(),
            name,
            is_dir: meta.is_dir(),
            size: meta.len(),
            modified,
        });
    }
    out.sort_by(|a, b| b.is_dir.cmp(&a.is_dir).then_with(|| a.name.cmp(&b.name)));
    Ok(out)
}

pub fn walk(root: &Path, rel: &str, out: &mut Vec<String>) {
    let Ok(entries) = list(root, rel) else { return };
    for e in entries {
        if e.is_dir {
            walk(root, &e.rel, out)
        } else {
            out.push(e.rel)
        }
    }
}

/// Write via unique tmp (pid + counter) then rename, so readers never see partial files.
pub fn write_atomic(path: &Path, data: impl AsRef<[u8]>) -> std::io::Result<()> {
    use std::sync::atomic::{AtomicU64, Ordering};
    static N: AtomicU64 = AtomicU64::new(0);
    let mut name = path.file_name().unwrap_or_default().to_os_string();
    name.push(format!(
        ".{}.{}.tmp",
        std::process::id(),
        N.fetch_add(1, Ordering::Relaxed)
    ));
    let tmp = path.with_file_name(name);
    std::fs::write(&tmp, data)?;
    std::fs::rename(&tmp, path).inspect_err(|_| {
        let _ = std::fs::remove_file(&tmp);
    })
}

pub fn mime(path: &Path) -> &'static str {
    let ext = path
        .extension()
        .map(|e| e.to_string_lossy().to_lowercase())
        .unwrap_or_default();
    match ext.as_str() {
        "pdf" => "application/pdf",
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        "svg" => "image/svg+xml",
        "webp" => "image/webp",
        "mp4" | "m4v" => "video/mp4",
        "webm" => "video/webm",
        "mp3" => "audio/mpeg",
        "wav" => "audio/wav",
        "ogg" => "audio/ogg",
        "zip" => "application/zip",
        "html" | "htm" => "text/html; charset=utf-8",
        "md" | "txt" | "csv" | "tsv" | "json" | "xml" | "css" | "js" | "ts" | "rs" | "py"
        | "sql" | "url" | "sh" | "java" | "toml" | "yaml" | "yml" | "log" | "ini" | "c" | "h"
        | "cpp" | "php" | "go" | "srt" | "ipynb" => "text/plain; charset=utf-8",
        "bmp" => "image/bmp",
        "avif" => "image/avif",
        "m4a" => "audio/mp4",
        "doc" => "application/msword",
        "rtf" => "application/rtf",
        "ppt" => "application/vnd.ms-powerpoint",
        "odp" => "application/vnd.oasis.opendocument.presentation",
        "xls" => "application/vnd.ms-excel",
        "ods" => "application/vnd.oasis.opendocument.spreadsheet",
        "docx" => "application/vnd.openxmlformats-officedocument.wordprocessingml.document",
        "xlsx" => "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet",
        "pptx" => "application/vnd.openxmlformats-officedocument.presentationml.presentation",
        "odt" => "application/vnd.oasis.opendocument.text",
        _ => "application/octet-stream",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mime_covers_preview_types() {
        let m = |n: &str| mime(Path::new(n));
        assert_eq!(m("a.doc"), "application/msword");
        assert_eq!(m("a.rtf"), "application/rtf");
        assert_eq!(m("a.ppt"), "application/vnd.ms-powerpoint");
        assert_eq!(
            m("a.odp"),
            "application/vnd.oasis.opendocument.presentation"
        );
        assert_eq!(m("a.xls"), "application/vnd.ms-excel");
        assert_eq!(m("a.ods"), "application/vnd.oasis.opendocument.spreadsheet");
        assert_eq!(m("a.bmp"), "image/bmp");
        assert_eq!(m("a.log"), "text/plain; charset=utf-8");
        assert_eq!(m("a.cpp"), "text/plain; charset=utf-8");
        assert_eq!(m("a.bin"), "application/octet-stream");
    }

    #[test]
    fn mime_media() {
        assert_eq!(mime(Path::new("a.MP4")), "video/mp4");
        assert_eq!(mime(Path::new("a.zip")), "application/zip");
    }

    fn root(tag: &str) -> std::path::PathBuf {
        let r = std::env::temp_dir().join(format!("mweb-jail-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&r);
        std::fs::create_dir_all(r.join("Digitalització aplicada/RA01")).unwrap();
        std::fs::create_dir_all(r.join(".moodle")).unwrap();
        std::fs::write(r.join(".moodle/token"), "secret").unwrap();
        std::fs::write(r.join(".env"), "X=1").unwrap();
        std::fs::write(
            r.join("Digitalització aplicada/RA01/Mapa IT·OT l'any.docx"),
            "d",
        )
        .unwrap();
        std::os::unix::fs::symlink("/etc", r.join("out")).unwrap();
        std::os::unix::fs::symlink(r.join(".moodle"), r.join("sneaky")).unwrap();
        r
    }

    #[test]
    fn jail_allows_submit_symlink_into_home_only() {
        let r = root("symhome");
        let home = r
            .parent()
            .unwrap()
            .join(format!("home-{}", std::process::id()));
        std::fs::create_dir_all(home.join("Docs")).unwrap();
        std::fs::create_dir_all(home.join(".ssh")).unwrap();
        std::fs::write(home.join("Docs/a.pdf"), "x").unwrap();
        std::fs::write(home.join(".ssh/id"), "k").unwrap();
        std::fs::create_dir_all(r.join("submit/M/T")).unwrap();
        let ln = |n: &str, t: &Path| {
            std::os::unix::fs::symlink(t, r.join("submit/M/T").join(n)).unwrap()
        };
        ln("a.pdf", &home.join("Docs/a.pdf"));
        ln("id", &home.join(".ssh/id"));
        ln("pw", Path::new("/etc/passwd"));
        ln("tok", &r.join(".moodle/token"));
        std::os::unix::fs::symlink(
            home.join("Docs/a.pdf"),
            r.join("Digitalització aplicada/a.pdf"),
        )
        .unwrap();
        assert!(jail_in(&r, "submit/M/T/a.pdf", &home).is_ok());
        for bad in [
            "submit/M/T/id",
            "submit/M/T/pw",
            "submit/M/T/tok",
            "Digitalització aplicada/a.pdf",
        ] {
            assert!(jail_in(&r, bad, &home).is_err(), "{bad}");
        }
    }

    #[test]
    fn jail_rules() {
        let r = root("rules");
        assert!(jail(&r, "../etc/passwd").is_err());
        assert!(jail(&r, ".env").is_err());
        assert!(jail(&r, ".moodle/token").is_err());
        assert!(jail(&r, "out/passwd").is_err());
        assert!(jail(&r, "sneaky/token").is_err());
        assert!(jail(&r, "/etc/passwd").is_err());
        assert!(jail(&r, "missing.txt").is_err());
    }

    #[test]
    fn jail_accepts_accents_and_spaces() {
        let r = root("accents");
        let p = jail(&r, "Digitalització aplicada/RA01/Mapa IT·OT l'any.docx").unwrap();
        assert!(p.ends_with("Mapa IT·OT l'any.docx"));
        assert!(jail(&r, "").is_ok());
    }

    #[test]
    fn write_atomic_no_tmp_concurrent() {
        let r = root("atomic");
        let p = r.join("out.bin");
        let hs: Vec<_> = (0..8)
            .map(|i| {
                let p = p.clone();
                std::thread::spawn(move || write_atomic(&p, [i; 64]).unwrap())
            })
            .collect();
        hs.into_iter().for_each(|h| h.join().unwrap());
        assert_eq!(std::fs::read(&p).unwrap().len(), 64);
        let n = std::fs::read_dir(&r).unwrap().flatten();
        assert!(!n
            .into_iter()
            .any(|e| e.file_name().to_string_lossy().contains(".tmp")));
    }

    #[test]
    fn list_hides_dotfiles_dirs_first() {
        let r = root("list");
        std::fs::write(r.join("a.txt"), "x").unwrap();
        let names: Vec<_> = list(&r, "").unwrap().into_iter().map(|e| e.name).collect();
        assert!(!names.iter().any(|n| n.starts_with('.')));
        assert_eq!(
            names.first().map(String::as_str),
            Some("Digitalització aplicada")
        );
        let mut all = Vec::new();
        walk(&r, "", &mut all);
        assert!(all.iter().any(|p| p.ends_with("l'any.docx")));
        assert!(!all.iter().any(|p| p.contains(".moodle")));
    }

    #[test]
    fn walk_survives_symlink_loop() {
        let r = root("loop");
        std::fs::create_dir_all(r.join("a")).unwrap();
        std::os::unix::fs::symlink(r.join("a"), r.join("a/loop")).unwrap();
        let mut out = Vec::new();
        walk(&r, "", &mut out);
        assert!(!list(&r, "a").unwrap().iter().any(|e| e.name == "loop"));
    }
}
