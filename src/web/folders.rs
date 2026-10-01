//! One-off course-folder migration (`moodle-sync --migrate-folders`): moves the
//! old shortname dirs to the `.moodle/folders.json` names, merging into existing
//! dirs without clobbering, then rewrites every stored path.
use crate::moodle::Course;
use crate::ra;
use crate::state::State;
use crate::sync::course_dir_name;
use crate::web::db::Db;
use anyhow::{Context, Result};
use serde::Serialize;
use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};

#[derive(Debug, Default, Serialize)]
pub struct Report {
    pub pairs: Vec<(String, String)>,
    pub merged_dupes: usize,
    pub renamed_conflicts: Vec<(String, String)>,
    pub pruned_dirs: usize,
}

fn sha(p: &Path) -> std::io::Result<Vec<u8>> {
    Ok(Sha256::digest(std::fs::read(p)?).to_vec())
}

fn exists(p: &Path) -> bool {
    std::fs::symlink_metadata(p).is_ok()
}

fn is_file(p: &Path) -> bool {
    std::fs::symlink_metadata(p).is_ok_and(|m| m.is_file())
}

/// `stem-N.ext` (N from 2) = first name that is free.
fn free_name(dst: &Path) -> PathBuf {
    let stem = dst.file_stem().unwrap_or_default().to_string_lossy();
    let ext = dst
        .extension()
        .map_or(String::new(), |e| format!(".{}", e.to_string_lossy()));
    (2..)
        .map(|n| dst.with_file_name(format!("{stem}-{n}{ext}")))
        .find(|c| !exists(c))
        .expect("unbounded range")
}

fn rel(root: &Path, p: &Path) -> String {
    p.strip_prefix(root).unwrap_or(p).to_string_lossy().into()
}

/// Move `src` to `dst`, never overwriting. `exact` collects file-level pairs
/// (dropped duplicates map onto the kept file, conflicts onto `-N`).
fn merge_move(
    root: &Path,
    src: &Path,
    dst: &Path,
    dry: bool,
    r: &mut Report,
    exact: &mut Vec<(String, String)>,
) -> Result<()> {
    if !exists(src) {
        return Ok(()); // resume after a partial run: already moved
    }
    if !exists(dst) {
        if !dry {
            std::fs::rename(src, dst).with_context(|| format!("moving {}", src.display()))?;
        }
        return Ok(());
    }
    if src.is_dir() && !src.is_symlink() && dst.is_dir() {
        for e in std::fs::read_dir(src)? {
            let n = e?.file_name();
            merge_move(root, &src.join(&n), &dst.join(&n), dry, r, exact)?;
        }
        if !dry {
            let _ = std::fs::remove_dir(src);
        }
    } else if is_file(src) && is_file(dst) && sha(src)? == sha(dst)? {
        if !dry {
            std::fs::remove_file(src)?;
        }
        r.merged_dupes += 1;
        exact.push((rel(root, src), rel(root, dst)));
    } else {
        let to = free_name(dst);
        if !dry {
            std::fs::rename(src, &to)?;
        }
        let pair = (rel(root, src), rel(root, &to));
        r.renamed_conflicts.push(pair.clone());
        exact.push(pair);
    }
    Ok(())
}

/// Remove empty dirs below `dir`, deepest first; `dir` itself stays.
fn prune(dir: &Path) -> usize {
    let mut n = 0;
    for e in std::fs::read_dir(dir).into_iter().flatten().flatten() {
        let p = e.path();
        if p.is_dir() && !p.is_symlink() {
            n += prune(&p);
            n += usize::from(std::fs::remove_dir(&p).is_ok());
        }
    }
    n
}

/// `007-Java Llenguatge` -> `007-java-llenguatge`.
fn new_module(name: &str) -> Option<String> {
    let (n, rest) = name.split_once('-')?;
    (!n.is_empty() && n.bytes().all(|b| b.is_ascii_digit()))
        .then(|| format!("{n}-{}", ra::slug(rest, 40)))
}

/// First pair whose `old` is `p` or a dir prefix of it (pairs sorted longest-first).
fn remap(pairs: &[(String, String)], p: &str) -> String {
    for (o, n) in pairs {
        if p == o {
            return n.clone();
        }
        if let Some(t) = p.strip_prefix(o.as_str()).filter(|t| t.starts_with('/')) {
            return format!("{n}{t}");
        }
    }
    p.to_string()
}

fn repoint_symlinks(root: &Path, dir: &Path, fix: &dyn Fn(&str) -> String) -> Result<()> {
    for e in std::fs::read_dir(dir)? {
        let p = e?.path();
        let m = std::fs::symlink_metadata(&p)?;
        if m.is_dir() {
            repoint_symlinks(root, &p, fix)?;
        } else if m.is_symlink() {
            let t = std::fs::read_link(&p)?;
            let Ok(t_rel) = t.strip_prefix(root) else {
                continue;
            };
            let new = fix(&t_rel.to_string_lossy());
            if new != t_rel.to_string_lossy() {
                std::fs::remove_file(&p)?;
                std::os::unix::fs::symlink(root.join(new), &p)?;
            }
        }
    }
    Ok(())
}

pub fn migrate(root: &Path, db: &Db, dry: bool) -> Result<Report> {
    let rs = root.to_string_lossy().to_string();
    let mut state = State::load(&rs)?;
    let mut r = Report::default();
    let mut exact = Vec::new();
    // (old course dir, new course dir, section dirs)
    let mut courses = Vec::new();
    let mut modules: Vec<(String, String)> = Vec::new(); // old path -> new path
    let mut names = std::collections::HashMap::new(); // old module folder -> new
    for (id, cs) in &state.courses {
        let old = ra::sanitize(&cs.shortname);
        let c = Course {
            id: id.parse().unwrap_or(0),
            fullname: cs.fullname.clone(),
            shortname: cs.shortname.clone(),
            summary: String::new(),
        };
        let new = course_dir_name(&rs, &c)?;
        // ponytail: on resume (dir moved, state not yet rewritten) conflict-renamed `-N`
        // exact pairs are lost; those entries map to the plain new name.
        let pre = format!("{old}/");
        if old == new
            || (!exists(&root.join(&old)) && !cs.files.values().any(|f| f.path.starts_with(&pre)))
        {
            continue;
        }
        for f in cs.files.values() {
            let mut it = f
                .path
                .strip_prefix(&format!("{old}/"))
                .unwrap_or("")
                .split('/');
            if let (Some(sec), Some(m), Some(_)) = (it.next(), it.next(), it.next()) {
                if let Some(nm) = new_module(m).filter(|nm| nm != m) {
                    modules.push((format!("{old}/{sec}/{m}"), format!("{old}/{sec}/{nm}")));
                    names.insert(m.to_string(), nm);
                }
            }
        }
        let secs = cs
            .sections
            .values()
            .map(|s| ra::sanitize(s))
            .collect::<Vec<_>>();
        courses.push((old, new, secs));
    }
    modules.sort();
    modules.dedup();
    let mut pairs: Vec<(String, String)> = courses
        .iter()
        .map(|(o, n, _)| (o.clone(), n.clone()))
        .chain(modules.iter().cloned())
        .collect();
    let personal = exists(&root.join("_personal"));
    if personal {
        pairs.push(("_personal".into(), "personal".into()));
    }
    if pairs.is_empty() {
        return Ok(r);
    }
    r.pairs = pairs.clone();

    if !dry {
        let dir = root.join(format!(
            ".moodle/trash/pre-folders-{}",
            chrono::Utc::now().format("%Y%m%d-%H%M%S")
        ));
        std::fs::create_dir_all(&dir)?;
        db.backup(&dir.join("web.db"))?;
        for f in ["state.json", "moves.jsonl"] {
            if root.join(".moodle").join(f).exists() {
                std::fs::copy(root.join(".moodle").join(f), dir.join(f))?;
            }
        }
    }
    // Modules first, inside the old dirs, so exact file pairs below stay old-prefixed.
    if !dry {
        for (old, _, secs) in &courses {
            for s in secs {
                r.pruned_dirs += prune(&root.join(old).join(s));
            }
        }
        for (o, n) in &modules {
            if exists(&root.join(o)) {
                merge_move(root, &root.join(o), &root.join(n), dry, &mut r, &mut exact)?;
            }
        }
    }
    let mod_exact = std::mem::take(&mut exact);
    for (old, new, _) in &courses {
        merge_move(
            root,
            &root.join(old),
            &root.join(new),
            dry,
            &mut r,
            &mut exact,
        )?;
    }
    if personal {
        merge_move(
            root,
            &root.join("_personal"),
            &root.join("personal"),
            dry,
            &mut r,
            &mut exact,
        )?;
    }
    if dry {
        return Ok(r);
    }
    let _ = std::fs::remove_dir(root.join("_scan"));

    // Stage 1 = module renames (+ their exact pairs), stage 2 = course dirs.
    let mut stage1: Vec<_> = modules.iter().cloned().chain(mod_exact).collect();
    let mut stage2: Vec<_> = courses
        .iter()
        .map(|(o, n, _)| (o.clone(), n.clone()))
        .collect();
    if personal {
        stage2.push(("_personal".into(), "personal".into()));
    }
    stage2.extend(exact);
    for v in [&mut stage1, &mut stage2] {
        v.sort_by_key(|(o, _)| std::cmp::Reverse(o.len()));
    }
    let fix = |p: &str| remap(&stage2, &remap(&stage1, p));
    for cs in state.courses.values_mut() {
        for f in cs.files.values_mut() {
            f.path = fix(&f.path);
        }
        for m in cs.modules.values_mut() {
            if let Some(n) = names.get(m.as_str()) {
                *m = n.clone();
            }
        }
    }
    for (o, n) in stage1.iter().chain(&stage2) {
        db.rewrite_prefix(o, n)?;
    }
    if let Ok(s) = std::fs::read_to_string(root.join(".moodle/moves.jsonl")) {
        let body: String = s
            .lines()
            .map(|l| match serde_json::from_str::<serde_json::Value>(l) {
                Ok(mut v) => {
                    for k in ["from", "to"] {
                        if let Some(p) = v[k].as_str() {
                            v[k] = fix(p).into();
                        }
                    }
                    v.to_string() + "\n"
                }
                Err(_) => format!("{l}\n"),
            })
            .collect();
        crate::web::files::write_atomic(&root.join(".moodle/moves.jsonl"), body)?;
    }
    if root.join("submit").is_dir() {
        repoint_symlinks(root, &root.join("submit"), &fix)?;
    }
    crate::web::readme::write(db, &rs)?;
    state.save(&rs)?; // commit marker: a failed run above is retried
    Ok(r)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn tmp(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("folders-{tag}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&d);
        fs::create_dir_all(d.join(".moodle")).unwrap();
        d
    }
    fn put(p: &Path, c: &str) {
        fs::create_dir_all(p.parent().unwrap()).unwrap();
        fs::write(p, c).unwrap();
    }
    /// state.json with (id, shortname, [paths]) courses + folders.json.
    fn setup(r: &Path, cs: &[(i64, &str, &str, &[&str])]) {
        let mut courses = serde_json::Map::new();
        let mut map = serde_json::Map::new();
        for (id, short, dir, paths) in cs {
            let files: serde_json::Map<_, _> = paths
                .iter()
                .enumerate()
                .map(|(i, p)| {
                    (
                        format!("u{id}{i}"),
                        serde_json::json!({"path": p, "sha256": "x", "size": 1}),
                    )
                })
                .collect();
            courses.insert(
                id.to_string(),
                serde_json::json!({"shortname": short, "fullname": short, "files": files, "sections": {"1": "RA01"}}),
            );
            map.insert(id.to_string(), (*dir).into());
        }
        put(
            &r.join(".moodle/state.json"),
            &serde_json::json!({"courses": courses}).to_string(),
        );
        put(
            &r.join(".moodle/folders.json"),
            &serde_json::Value::Object(map).to_string(),
        );
    }
    fn state_paths(r: &Path) -> Vec<String> {
        let s = State::load(&r.to_string_lossy()).unwrap();
        s.courses
            .values()
            .flat_map(|c| c.files.values().map(|f| f.path.clone()))
            .collect()
    }
    const OLD: &str = "BBDD/RA01/001-PPT Introducció a les BBDD/a.pdf";
    const NEW: &str = "bbdd/RA01/001-ppt-introduccio-a-les-bbdd/a.pdf";

    #[test]
    fn migrate_moves_and_rewrites() {
        let r = tmp("mv");
        setup(&r, &[(1, "BBDD", "bbdd", &[OLD])]);
        put(&r.join(OLD), "pdf");
        let db = Db::memory().unwrap();
        db.upsert_ai_tasks(&[crate::web::db::Task {
            id: "ai:1:0".into(),
            source: "ai".into(),
            url: OLD.into(),
            ..Default::default()
        }])
        .unwrap();
        db.cache_put("s", OLD, "{}", 1).unwrap();
        let j = db.scan_new_job(1).unwrap();
        db.scan_add_item(j, OLD, "BBDD", "pdf").unwrap();
        db.scan_add_item(j, NEW, "BBDD", "pdf").unwrap(); // collision: old row is dropped
        let rep = migrate(&r, &db, false).unwrap();
        assert!(!rep.pairs.is_empty());
        assert_eq!(fs::read_to_string(r.join(NEW)).unwrap(), "pdf");
        assert!(!r.join("BBDD").exists());
        assert_eq!(state_paths(&r), [NEW]);
        assert_eq!(db.task("ai:1:0").unwrap().unwrap().url, NEW);
        let q = |sql: &str| {
            let f = r.join(format!("q{}.db", sql.len()));
            db.backup(&f).unwrap();
            rusqlite::Connection::open(f)
                .unwrap()
                .query_row(sql, [], |x| x.get::<_, i64>(0))
                .unwrap()
        };
        assert_eq!(
            q(&format!("SELECT count(*) FROM scan_item WHERE src='{NEW}'")),
            1
        );
        assert_eq!(
            q("SELECT count(*) FROM scan_item WHERE src GLOB 'BBDD/*'"),
            0
        );
        assert_eq!(
            q(&format!(
                "SELECT count(*) FROM extract_cache WHERE source_url='{NEW}'"
            )),
            1
        );
        assert!(r.join(".moodle/trash").read_dir().unwrap().next().is_some());
    }

    #[test]
    fn migrate_merges_into_existing_dir() {
        let r = tmp("merge");
        let o = "DAM-MP03-26-27/RA01/001-X/f.pdf";
        setup(&r, &[(1, "DAM-MP03-26-27", "programacio", &[o])]);
        put(&r.join(o), "f");
        put(&r.join("programacio/src/Main.java"), "m");
        put(&r.join("programacio/README.md"), "mine");
        put(&r.join("DAM-MP03-26-27/README.md"), "theirs");
        put(&r.join("programacio/same.txt"), "s");
        put(&r.join("DAM-MP03-26-27/same.txt"), "s");
        let rep = migrate(&r, &Db::memory().unwrap(), false).unwrap();
        assert_eq!(
            fs::read_to_string(r.join("programacio/src/Main.java")).unwrap(),
            "m"
        );
        assert_eq!(
            fs::read_to_string(r.join("programacio/RA01/001-x/f.pdf")).unwrap(),
            "f"
        );
        assert_eq!(
            fs::read_to_string(r.join("programacio/README.md")).unwrap(),
            "mine"
        );
        assert_eq!(
            fs::read_to_string(r.join("programacio/README-2.md")).unwrap(),
            "theirs"
        );
        assert!(!r.join("programacio/same-2.txt").exists());
        assert_eq!(rep.merged_dupes, 1);
        assert_eq!(rep.renamed_conflicts.len(), 1);
        assert!(!r.join("DAM-MP03-26-27").exists());
    }

    #[test]
    fn migrate_prunes_empty_module_dupes() {
        let r = tmp("prune");
        let o = "MP05/RA01/015-JVM/a.txt";
        setup(&r, &[(1, "MP05", "entorns", &[o])]);
        put(&r.join(o), "a");
        fs::create_dir_all(r.join("MP05/RA01/013-JVM")).unwrap();
        migrate(&r, &Db::memory().unwrap(), false).unwrap();
        let l: Vec<_> = fs::read_dir(r.join("entorns/RA01"))
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .collect();
        assert_eq!(l, ["015-jvm"]);
    }

    #[test]
    fn migrate_repoints_submit_symlinks() {
        let r = tmp("sym");
        setup(&r, &[(1, "BBDD", "bbdd", &[OLD])]);
        put(&r.join(OLD), "x");
        let ln = r.join("submit/M/T/x_amin.docx");
        fs::create_dir_all(ln.parent().unwrap()).unwrap();
        std::os::unix::fs::symlink(r.join(OLD), &ln).unwrap();
        migrate(&r, &Db::memory().unwrap(), false).unwrap();
        assert_eq!(fs::read_link(&ln).unwrap(), r.join(NEW));
        assert_eq!(fs::read_to_string(&ln).unwrap(), "x");
    }

    #[test]
    fn migrate_is_idempotent() {
        let r = tmp("idem");
        setup(&r, &[(1, "BBDD", "bbdd", &[OLD])]);
        put(&r.join(OLD), "x");
        put(&r.join("_personal/p.txt"), "p");
        let db = Db::memory().unwrap();
        migrate(&r, &db, false).unwrap();
        let before = (state_paths(&r), fs::read_dir(&r).unwrap().count());
        let rep = migrate(&r, &db, false).unwrap();
        assert!(rep.pairs.is_empty());
        assert_eq!(before, (state_paths(&r), fs::read_dir(&r).unwrap().count()));
        assert!(r.join("personal/p.txt").exists());
    }

    #[test]
    fn migrate_dry_run_touches_nothing() {
        let r = tmp("dry");
        setup(&r, &[(1, "BBDD", "bbdd", &[OLD])]);
        put(&r.join(OLD), "x");
        state_paths(&r); // load normalises the file once
        let st = fs::read_to_string(r.join(".moodle/state.json")).unwrap();
        let rep = migrate(&r, &Db::memory().unwrap(), true).unwrap();
        assert!(!rep.pairs.is_empty());
        assert!(r.join(OLD).exists() && !r.join("bbdd").exists());
        assert_eq!(
            st,
            fs::read_to_string(r.join(".moodle/state.json")).unwrap()
        );
        assert!(!r.join(".moodle/trash").exists());
    }

    #[test]
    fn migrate_never_touches_protected() {
        let r = tmp("prot");
        setup(&r, &[(1, "BBDD", "bbdd", &[OLD])]);
        put(&r.join(OLD), "x");
        put(&r.join("submit/BBDD/t/f.docx"), "s");
        put(&r.join("moodle-mcp/BBDD/z"), "z");
        put(&r.join(".claude/BBDD/z"), "z");
        migrate(&r, &Db::memory().unwrap(), false).unwrap();
        for p in [
            "submit/BBDD/t/f.docx",
            "moodle-mcp/BBDD/z",
            ".claude/BBDD/z",
        ] {
            assert!(r.join(p).exists(), "{p}");
        }
    }

    #[test]
    fn migrate_resumes_after_partial_run() {
        let r = tmp("resume");
        setup(&r, &[(1, "BBDD", "bbdd", &[OLD])]);
        put(&r.join(NEW), "x"); // files already moved, state untouched
        let db = Db::memory().unwrap();
        db.upsert_ai_tasks(&[crate::web::db::Task {
            id: "ai:1:0".into(),
            source: "ai".into(),
            url: OLD.into(),
            ..Default::default()
        }])
        .unwrap();
        migrate(&r, &db, false).unwrap();
        assert_eq!(state_paths(&r), [NEW]);
        assert_eq!(db.task("ai:1:0").unwrap().unwrap().url, NEW);
    }

    #[test]
    fn migrate_remaps_module_then_merge_dupe_and_conflict() {
        let r = tmp("two");
        let (oa, ob) = ("BBDD/RA01/007-Java X/a.pdf", "BBDD/RA01/007-Java X/b.pdf");
        setup(&r, &[(1, "BBDD", "bbdd", &[oa, ob])]);
        put(&r.join(oa), "same");
        put(&r.join(ob), "old");
        put(&r.join("bbdd/RA01/007-java-x/a.pdf"), "same");
        put(&r.join("bbdd/RA01/007-java-x/b.pdf"), "other");
        let db = Db::memory().unwrap();
        db.upsert_ai_tasks(&[crate::web::db::Task {
            id: "ai:1:0".into(),
            source: "ai".into(),
            url: ob.into(),
            ..Default::default()
        }])
        .unwrap();
        migrate(&r, &db, false).unwrap();
        let mut p = state_paths(&r);
        p.sort();
        assert_eq!(
            p,
            ["bbdd/RA01/007-java-x/a.pdf", "bbdd/RA01/007-java-x/b-2.pdf"]
        );
        assert_eq!(
            db.task("ai:1:0").unwrap().unwrap().url,
            "bbdd/RA01/007-java-x/b-2.pdf"
        );
    }
}
