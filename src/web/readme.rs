//! `submit/README.md` generator. Format is frozen: the school-verify skill
//! and the student read this file. Round-trip tested against the real file.
use crate::web::db::{Db, Local, Task};
use anyhow::Result;
use chrono::{Local as Tz, TimeZone};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct Row {
    pub module: String,
    pub task: String,
    pub due: String,
    pub files: String,
    pub status: String,
    pub verified: String,
}

const HEAD: &str = "# Submit: tasques per revisar i lliurar

Aquí hi ha les tasques acabades. Abans de lliurar-ne cap, revisa cada fitxer tu mateix.
Estat: `per revisar` (acabada, falta la teva revisió) · `revisat OK` (la pots lliurar) · `lliurat` (pujada i confirmada a Moodle, carpeta `_lliurat/`).

| Mòdul | Tasca | Venciment | Fitxers | Estat | Verificat |
|---|---|---|---|---|---|
";
const PENDING_HEAD: &str = "
## Pendents sense començar (Moodle)
| Mòdul | Tasca | Venciment |
|---|---|---|
";

fn cells(line: &str) -> Vec<String> {
    line.trim()
        .trim_matches('|')
        .split('|')
        .map(|c| c.trim().to_string())
        .collect()
}

pub fn parse(md: &str) -> (Vec<Row>, Vec<Row>) {
    let (mut done, mut pending) = (Vec::new(), Vec::new());
    let mut in_pending = false;
    for line in md.lines() {
        if line.starts_with("## Pendents") {
            in_pending = true;
            continue;
        }
        if !line.starts_with('|') || line.starts_with("|---") || line.starts_with("| Mòdul") {
            continue;
        }
        let c = cells(line);
        if in_pending && c.len() >= 3 {
            pending.push(Row {
                module: c[0].clone(),
                task: c[1].clone(),
                due: c[2].clone(),
                ..Default::default()
            });
        } else if !in_pending && c.len() >= 6 {
            done.push(Row {
                module: c[0].clone(),
                task: c[1].clone(),
                due: c[2].clone(),
                files: c[3].clone(),
                status: c[4].clone(),
                verified: c[5].clone(),
            });
        }
    }
    (done, pending)
}

pub fn render(done: &[Row], pending: &[Row]) -> String {
    let mut s = String::from(HEAD);
    for r in done {
        s.push_str(&format!(
            "| {} | {} | {} | {} | {} | {} |\n",
            r.module, r.task, r.due, r.files, r.status, r.verified
        ));
    }
    s.push_str(PENDING_HEAD);
    for r in pending {
        s.push_str(&format!("| {} | {} | {} |\n", r.module, r.task, r.due));
    }
    s
}

pub fn fmt_due(ts: i64, with_time: bool) -> String {
    if ts == 0 {
        return String::new();
    }
    let Some(d) = Tz.timestamp_opt(ts, 0).single() else {
        return String::new();
    };
    d.format(if with_time {
        "%d/%m/%Y %H:%M"
    } else {
        "%d/%m/%Y"
    })
    .to_string()
}

fn file_name(rel: &str) -> &str {
    rel.rsplit('/').next().unwrap_or(rel)
}

pub fn submit_dir(module: &str, title: &str) -> String {
    let t = title.replace('/', "-");
    let t = t.trim_start_matches('.');
    format!(
        "submit/{}/{}",
        module.replace('/', "-"),
        if t.is_empty() { "_" } else { t }
    )
}

pub fn build(db: &Db, now: i64) -> Result<String> {
    let tasks = db.all_tasks()?;
    let locals: std::collections::HashMap<String, Local> = db
        .all_local()?
        .into_iter()
        .map(|l| (l.task_id.clone(), l))
        .collect();
    let mut done: Vec<Row> = db
        .manual_rows()?
        .into_iter()
        .filter_map(|v| serde_json::from_value(v).ok())
        .collect();
    // The student's own module label for a course (e.g. "LMSGI 0373"), reused for its other rows.
    let labels: std::collections::HashMap<String, String> = tasks
        .iter()
        .filter_map(|t| {
            let m = &locals.get(&t.id)?.module;
            (!m.is_empty()).then(|| (t.course.clone(), m.clone()))
        })
        .collect();
    let mut pending = Vec::new();
    for t in tasks
        .iter()
        .filter(|t| t.kind != "post" && (t.source != "ai" || t.ai_confirmed == Some(true)))
    {
        let l = locals.get(&t.id);
        let module = l
            .map(|l| l.module.clone())
            .filter(|m| !m.is_empty())
            .or_else(|| labels.get(&t.course).cloned())
            .unwrap_or_else(|| t.course.clone());
        match l.filter(|l| !l.status.is_empty() && l.status != "llegit") {
            Some(l) => done.push(Row {
                module,
                task: t.title.clone(),
                due: fmt_due(t.due, true),
                files: l
                    .files
                    .iter()
                    .map(|f| file_name(f))
                    .collect::<Vec<_>>()
                    .join(", "),
                status: l.status.clone(),
                verified: fmt_due(l.verified, false),
            }),
            None if t.moodle_status != "submitted" && (t.due == 0 || t.due > now) => {
                pending.push(Row {
                    module,
                    task: t.title.clone(),
                    due: fmt_due(t.due, false),
                    ..Default::default()
                })
            }
            None => {}
        }
    }
    let rank = |s: &str| match s {
        "per revisar" => 0,
        "revisat OK" => 1,
        _ => 2,
    };
    done.sort_by_key(|r| rank(&r.status)); // stable: keeps import order inside a group
    Ok(render(&done, &pending))
}

pub fn write(db: &Db, root: &str) -> Result<()> {
    let dir = std::path::Path::new(root).join("submit");
    std::fs::create_dir_all(&dir)?;
    let path = dir.join("README.md");
    let md = build(db, chrono::Utc::now().timestamp())?;
    if path.exists() {
        std::fs::copy(&path, dir.join("README.md.bak"))?;
    }
    crate::web::files::write_atomic(&path, md)?;
    Ok(())
}

fn parse_date(s: &str) -> i64 {
    chrono::NaiveDate::parse_from_str(s.trim(), "%d/%m/%Y")
        .ok()
        .and_then(|d| d.and_hms_opt(12, 0, 0))
        .and_then(|d| Tz.from_local_datetime(&d).single())
        .map_or(0, |d| d.timestamp())
}

pub fn import_once(db: &Db, root: &str) -> Result<()> {
    if db.kv_get("readme_imported")?.is_some() {
        return Ok(());
    }
    let path = std::path::Path::new(root).join("submit/README.md");
    if let Ok(md) = std::fs::read_to_string(&path) {
        let (done, _) = parse(&md);
        let tasks: Vec<Task> = db.all_tasks()?;
        // README lists bare names; the rest of the app wants paths under root.
        // ponytail: first match wins if two submit/ folders hold the same name.
        let mut under_submit = Vec::new();
        crate::web::files::walk(std::path::Path::new(root), "submit", &mut under_submit);
        let resolve = |name: &str| {
            under_submit
                .iter()
                .find(|rel| file_name(rel) == name)
                .cloned()
                .unwrap_or_else(|| name.to_string())
        };
        for r in done {
            match tasks
                .iter()
                .find(|t| t.title.eq_ignore_ascii_case(r.task.trim()))
            {
                Some(t) => db.set_local(&Local {
                    task_id: t.id.clone(),
                    status: r.status.clone(),
                    module: r.module.clone(),
                    files: r
                        .files
                        .split(", ")
                        .filter(|f| !f.is_empty())
                        .map(resolve)
                        .collect(),
                    verified: parse_date(&r.verified),
                    chat_session: String::new(),
                })?,
                None => db.add_manual_row(&serde_json::to_value(&r)?)?,
            }
        }
    }
    db.kv_set("readme_imported", "1")?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::web::db::{Db, Task};

    const CURRENT: &str = include_str!("../../tests/fixtures/submit_readme.md");

    #[test]
    fn roundtrip_current_readme() {
        let (done, pending) = parse(CURRENT);
        assert!(
            done.len() >= 5 && pending.len() >= 7,
            "{} {}",
            done.len(),
            pending.len()
        );
        assert_eq!(done[0].status, "per revisar");
        assert_eq!(render(&done, &pending).trim_end(), CURRENT.trim_end());
    }

    #[test]
    fn import_keeps_unmatched_rows() {
        let db = Db::memory().unwrap();
        db.upsert_tasks(&[Task {
            id: "assign:1".into(),
            kind: "assign".into(),
            title: "M4_A2_A_RA1".into(),
            course: "2627_DAX_0373".into(),
            due: 1_790_000_000,
            source: "moodle".into(),
            ..Default::default()
        }])
        .unwrap();
        let root = std::env::temp_dir().join(format!("mweb-readme-{}", std::process::id()));
        std::fs::create_dir_all(root.join("submit")).unwrap();
        std::fs::write(root.join("submit/README.md"), CURRENT).unwrap();
        let done_dir = root.join("submit/_lliurat/LMSGI/M4_A2_A_RA1");
        std::fs::create_dir_all(&done_dir).unwrap();
        std::fs::write(done_dir.join("M4_A2_A_RA1_amin.docx"), "d").unwrap();
        import_once(&db, root.to_str().unwrap()).unwrap();
        let l = db.local("assign:1").unwrap();
        assert_eq!(
            l.files,
            vec!["submit/_lliurat/LMSGI/M4_A2_A_RA1/M4_A2_A_RA1_amin.docx"],
            "bare README names must resolve to root-relative paths"
        );
        assert_eq!(l.status, "lliurat");
        assert_eq!(l.module, "LMSGI 0373");
        let n = db.manual_rows().unwrap().len();
        assert!(n > 0, "unmatched rows must be kept");
        import_once(&db, root.to_str().unwrap()).unwrap();
        assert_eq!(db.manual_rows().unwrap().len(), n, "idempotent");
        let md = build(&db, 0).unwrap();
        assert!(md.contains("Tasca-RA1-Unilever_amin.docx"), "{md}");
        assert!(md.contains("| LMSGI 0373 | M4_A2_A_RA1 |"));
    }

    #[test]
    fn pending_rows_reuse_course_label() {
        let db = Db::memory().unwrap();
        let t = |id: &str, title: &str| Task {
            id: id.into(),
            kind: "assign".into(),
            title: title.into(),
            course: "2627_DAX_0373".into(),
            due: 2_000_000_000,
            source: "moodle".into(),
            ..Default::default()
        };
        db.upsert_tasks(&[t("assign:1", "Fet"), t("assign:2", "Pendent")])
            .unwrap();
        db.set_local(&Local {
            task_id: "assign:1".into(),
            status: "lliurat".into(),
            module: "LMSGI 0373".into(),
            ..Default::default()
        })
        .unwrap();
        let md = build(&db, 0).unwrap();
        assert!(md.contains("| LMSGI 0373 | Pendent |"), "{md}");
    }

    #[test]
    fn submit_dir_sanitises() {
        assert_eq!(
            submit_dir("LMSGI 0373", "M4_A2 o A3/P"),
            "submit/LMSGI 0373/M4_A2 o A3-P"
        );
    }

    #[test]
    fn submit_dir_no_leading_dot() {
        for t in ["..", ".x"] {
            let d = submit_dir("M", t);
            assert!(!d.rsplit('/').next().unwrap().starts_with('.'), "{d}");
        }
    }

    #[test]
    fn due_format() {
        assert_eq!(fmt_due(0, true), "");
        assert_eq!(fmt_due(1_790_000_000, true).len(), "29/09/2026 23:59".len());
    }
}
