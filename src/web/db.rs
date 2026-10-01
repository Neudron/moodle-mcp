//! SQLite state for moodle-web. Tasks and local state are JSON blobs keyed
//! by id (no migrations needed when fields are added).
use anyhow::Result;
use rusqlite::{params, Connection, OptionalExtension};
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;

pub const STATUSES: [&str; 4] = ["per revisar", "revisat OK", "lliurat", "llegit"];

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct Attachment {
    pub filename: String,
    pub fileurl: String,
    pub mimetype: String,
    pub filesize: i64,
    pub timemodified: i64,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct Task {
    pub id: String,
    pub course_id: i64,
    pub course: String,
    pub kind: String,
    pub title: String,
    pub body_html: String,
    pub attachments: Vec<Attachment>,
    pub submission_files: Vec<Attachment>,
    pub url: String,
    pub opens: i64,
    pub due: i64,
    pub cutoff: i64,
    pub moodle_status: String,
    pub grade: String,
    pub feedback_html: String,
    pub source: String,
    pub ai_confirmed: Option<bool>,
    /// Jev's 0..1 verdict that this AI proposal is a real task.
    pub ai_score: Option<f64>,
    /// Version of the Jev prompt that produced `ai_score` (0 = old single question).
    pub ai_v: u8,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct Local {
    pub task_id: String,
    pub status: String,
    pub module: String,
    pub files: Vec<String>,
    pub verified: i64,
    pub chat_session: String,
}

pub struct Db(Mutex<Connection>, AtomicU64);

/// Applied in order; `PRAGMA user_version` = number applied. Append only.
const MIGRATIONS: &[&str] = &[
    "
CREATE TABLE IF NOT EXISTS task(id TEXT PRIMARY KEY, due INT NOT NULL, json TEXT NOT NULL);
CREATE TABLE IF NOT EXISTS local(task_id TEXT PRIMARY KEY, json TEXT NOT NULL);
CREATE TABLE IF NOT EXISTS grades(course_id INT PRIMARY KEY, json TEXT NOT NULL);
CREATE TABLE IF NOT EXISTS extract_cache(sha TEXT PRIMARY KEY, source_url TEXT, result_json TEXT, at INT);
CREATE TABLE IF NOT EXISTS caldav_map(task_id TEXT PRIMARY KEY, hash TEXT NOT NULL);
CREATE TABLE IF NOT EXISTS error_log(at INT NOT NULL, source TEXT NOT NULL, kind TEXT NOT NULL, msg TEXT NOT NULL);
CREATE TABLE IF NOT EXISTS kv(k TEXT PRIMARY KEY, v TEXT NOT NULL);
CREATE TABLE IF NOT EXISTS manual_rows(n INTEGER PRIMARY KEY AUTOINCREMENT, json TEXT NOT NULL);
CREATE TABLE IF NOT EXISTS scan_job(id INTEGER PRIMARY KEY AUTOINCREMENT, started INT NOT NULL, finished INT, state TEXT NOT NULL);
CREATE TABLE IF NOT EXISTS scan_item(job INT NOT NULL, src TEXT NOT NULL, course TEXT NOT NULL, kind TEXT NOT NULL, sha TEXT NOT NULL DEFAULT '', status TEXT NOT NULL DEFAULT 'pending', error TEXT NOT NULL DEFAULT '', at INT, PRIMARY KEY(job, src));
",
    "ALTER TABLE caldav_map ADD COLUMN ev_uid TEXT NOT NULL DEFAULT ''",
    "ALTER TABLE caldav_map ADD COLUMN todo_uid TEXT NOT NULL DEFAULT ''",
    "DROP TABLE IF EXISTS notify_log",
    // Legacy DBs jumped to v3 before error_log was added to migration 0.
    "CREATE TABLE IF NOT EXISTS error_log(at INT NOT NULL, source TEXT NOT NULL, kind TEXT NOT NULL, msg TEXT NOT NULL)",
];

fn version(c: &Connection) -> Result<usize> {
    Ok(c.query_row("PRAGMA user_version", [], |r| r.get::<_, i64>(0))? as usize)
}

fn migrate(c: &Connection) -> Result<()> {
    let mut v = version(c)?;
    if v > MIGRATIONS.len() {
        anyhow::bail!(
            "database schema v{v} is newer than this build (v{}); refusing to open",
            MIGRATIONS.len()
        );
    }
    // Pre-versioning DBs (user_version 0) already have the ev_uid/todo_uid columns.
    let legacy: bool = c
        .query_row(
            "SELECT 1 FROM pragma_table_info('caldav_map') WHERE name='todo_uid'",
            [],
            |_| Ok(()),
        )
        .optional()?
        .is_some();
    if v == 0 && legacy {
        v = 3;
    }
    for (i, sql) in MIGRATIONS.iter().enumerate().skip(v) {
        c.execute_batch(&format!(
            "BEGIN;{sql};PRAGMA user_version={};COMMIT;",
            i + 1
        ))?;
    }
    Ok(())
}

/// Best effort 0600; the file may not exist (no -wal yet).
fn private(p: &Path) {
    use std::os::unix::fs::PermissionsExt;
    let _ = std::fs::set_permissions(p, std::fs::Permissions::from_mode(0o600));
}

const PRAGMAS: &str =
    "PRAGMA busy_timeout=5000; PRAGMA synchronous=NORMAL; PRAGMA foreign_keys=ON;";

fn json_rows(c: &Connection, sql: &str) -> Result<Vec<String>> {
    let mut st = c.prepare(sql)?;
    let rows = st.query_map([], |r| r.get::<_, String>(0))?;
    Ok(rows.collect::<Result<_, _>>()?)
}

impl Db {
    pub fn open(path: &Path) -> Result<Db> {
        let existed = path.exists();
        let c = Connection::open(path)?;
        if existed && version(&c)? < MIGRATIONS.len() {
            // VACUUM INTO captures WAL contents; versioned name keeps the oldest state.
            let mut bak = path.as_os_str().to_owned();
            bak.push(format!(".pre-migrate-v{}", version(&c)?));
            let _ = std::fs::remove_file(&bak);
            c.execute("VACUUM INTO ?1", [bak.to_string_lossy().as_ref()])?;
            private(Path::new(&bak));
        }
        c.execute_batch("PRAGMA journal_mode=WAL;")?;
        c.execute_batch(PRAGMAS)?;
        migrate(&c)?;
        for suf in ["", "-wal", "-shm"] {
            let mut f = path.as_os_str().to_owned();
            f.push(suf);
            private(Path::new(&f));
        }
        Ok(Db(Mutex::new(c), AtomicU64::new(Self::boot())))
    }

    pub fn memory() -> Result<Db> {
        let c = Connection::open_in_memory()?;
        c.execute_batch(PRAGMAS)?;
        migrate(&c)?;
        Ok(Db(Mutex::new(c), AtomicU64::new(Self::boot())))
    }

    /// Start value for the data version: unique per process so an ETag from a
    /// previous run never matches.
    fn boot() -> u64 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_nanos() as u64)
    }

    /// Bumped on every task/local write; backs the /api/tasks ETag.
    pub fn version(&self) -> u64 {
        self.1.load(Ordering::Relaxed)
    }

    fn c(&self) -> std::sync::MutexGuard<'_, Connection> {
        // Poison only means another request panicked; SQLite stays consistent.
        self.0.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Read back a numeric PRAGMA (tests).
    pub fn pragma(&self, name: &str) -> Result<i64> {
        Ok(self
            .c()
            .query_row(&format!("PRAGMA {name}"), [], |r| r.get(0))?)
    }

    pub fn upsert_tasks(&self, tasks: &[Task]) -> Result<()> {
        let mut c = self.c();
        let tx = c.transaction()?;
        for t in tasks {
            tx.execute(
                "INSERT INTO task(id,due,json) VALUES(?1,?2,?3)
                 ON CONFLICT(id) DO UPDATE SET due=excluded.due, json=excluded.json",
                params![t.id, t.due, serde_json::to_string(t)?],
            )?;
        }
        tx.commit()?;
        self.1.fetch_add(1, Ordering::Relaxed);
        Ok(())
    }

    /// One transaction: the read of the old row and the write can't interleave with other writers.
    pub fn upsert_ai_tasks(&self, tasks: &[Task]) -> Result<()> {
        let mut c = self.c();
        let tx = c.transaction()?;
        for t in tasks {
            let mut t = t.clone();
            if let Some(old) = get_task(&tx, &t.id)? {
                t.ai_confirmed = old.ai_confirmed;
                if t.ai_score.is_none() {
                    (t.ai_score, t.ai_v) = (old.ai_score, old.ai_v);
                }
            }
            put_task(&tx, &t)?;
        }
        tx.commit()?;
        self.1.fetch_add(1, Ordering::Relaxed);
        Ok(())
    }

    /// Read-modify-write of one task under the mutex; None when the row doesn't exist.
    pub fn update_task(&self, id: &str, f: impl FnOnce(&mut Task)) -> Result<Option<Task>> {
        let c = self.c();
        let Some(mut t) = get_task(&c, id)? else {
            return Ok(None);
        };
        f(&mut t);
        put_task(&c, &t)?;
        self.1.fetch_add(1, Ordering::Relaxed);
        Ok(Some(t))
    }

    pub fn all_tasks(&self) -> Result<Vec<Task>> {
        json_rows(&self.c(), "SELECT json FROM task ORDER BY due, id")?
            .iter()
            .map(|j| Ok(serde_json::from_str(j)?))
            .collect()
    }

    pub fn task(&self, id: &str) -> Result<Option<Task>> {
        let j: Option<String> = self
            .c()
            .query_row("SELECT json FROM task WHERE id=?1", [id], |r| r.get(0))
            .optional()?;
        Ok(j.map(|j| serde_json::from_str(&j)).transpose()?)
    }

    /// Only for unconfirmed AI proposals that Jev rejected.
    pub fn delete_task(&self, id: &str) -> Result<()> {
        self.c().execute("DELETE FROM task WHERE id=?1", [id])?;
        self.1.fetch_add(1, Ordering::Relaxed);
        Ok(())
    }

    /// Fold the WAL into the main file and empty it (clean shutdown).
    pub fn checkpoint(&self) -> Result<()> {
        self.c()
            .query_row("PRAGMA wal_checkpoint(TRUNCATE)", [], |_| Ok(()))?;
        Ok(())
    }

    /// Consistent copy of the whole database (WAL-safe).
    pub fn backup(&self, to: &Path) -> Result<()> {
        self.c()
            .execute("VACUUM INTO ?1", [to.to_string_lossy().as_ref()])?;
        private(to);
        Ok(())
    }

    pub fn set_ai_score(&self, id: &str, p: f64) -> Result<()> {
        self.update_task(id, |t| {
            (t.ai_score, t.ai_v) = (Some(p), crate::web::scan::JEV_V)
        })?;
        Ok(())
    }

    pub fn set_ai_confirmed(&self, id: &str, keep: bool) -> Result<()> {
        self.update_task(id, |t| t.ai_confirmed = Some(keep))?;
        Ok(())
    }

    pub fn local(&self, id: &str) -> Result<Local> {
        get_local(&self.c(), id)
    }

    /// Read-modify-write of one local row under the mutex.
    pub fn update_local(&self, id: &str, f: impl FnOnce(&mut Local)) -> Result<Local> {
        self.try_update_local(id, |l| {
            f(l);
            Ok::<_, anyhow::Error>(())
        })
    }

    /// Like `update_local`, but an `Err` from `f` aborts without writing.
    pub fn try_update_local<E: From<anyhow::Error>>(
        &self,
        id: &str,
        f: impl FnOnce(&mut Local) -> Result<(), E>,
    ) -> Result<Local, E> {
        let c = self.c();
        let mut l = get_local(&c, id)?;
        f(&mut l)?;
        put_local(&c, &l)?;
        self.1.fetch_add(1, Ordering::Relaxed);
        Ok(l)
    }

    pub fn all_local(&self) -> Result<Vec<Local>> {
        json_rows(&self.c(), "SELECT json FROM local ORDER BY task_id")?
            .iter()
            .map(|j| Ok(serde_json::from_str(j)?))
            .collect()
    }

    pub fn set_local(&self, l: &Local) -> Result<()> {
        put_local(&self.c(), l)?;
        self.1.fetch_add(1, Ordering::Relaxed);
        Ok(())
    }

    pub fn set_grades(&self, course_id: i64, v: &serde_json::Value) -> Result<()> {
        self.c().execute(
            "INSERT INTO grades(course_id,json) VALUES(?1,?2) ON CONFLICT(course_id) DO UPDATE SET json=excluded.json",
            params![course_id, v.to_string()],
        )?;
        Ok(())
    }

    pub fn grades(&self) -> Result<Vec<(i64, serde_json::Value)>> {
        let c = self.c();
        let mut st = c.prepare("SELECT course_id, json FROM grades ORDER BY course_id")?;
        let rows = st.query_map([], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?)))?;
        let mut out = Vec::new();
        for r in rows {
            let (id, j) = r?;
            out.push((id, serde_json::from_str(&j)?));
        }
        Ok(out)
    }

    pub fn cache_get(&self, sha: &str) -> Result<Option<String>> {
        Ok(self
            .c()
            .query_row(
                "SELECT result_json FROM extract_cache WHERE sha=?1",
                [sha],
                |r| r.get(0),
            )
            .optional()?)
    }

    /// True when `sha` is cached and younger than `ttl` seconds.
    pub fn cache_fresh(&self, sha: &str, ttl: i64, now: i64) -> Result<bool> {
        let at: Option<i64> = self
            .c()
            .query_row("SELECT at FROM extract_cache WHERE sha=?1", [sha], |r| {
                r.get(0)
            })
            .optional()?;
        Ok(at.is_some_and(|a| now - a < ttl))
    }

    pub fn cache_put(&self, sha: &str, url: &str, json: &str, at: i64) -> Result<()> {
        self.c().execute(
            "INSERT OR REPLACE INTO extract_cache(sha,source_url,result_json,at) VALUES(?1,?2,?3,?4)",
            params![sha, url, json, at],
        )?;
        Ok(())
    }

    /// (task_id, hash, event uid, reminder uid) for every mirrored task.
    pub fn caldav_rows(&self) -> Result<Vec<(String, String, String, String)>> {
        let c = self.c();
        let mut st = c.prepare("SELECT task_id,hash,ev_uid,todo_uid FROM caldav_map")?;
        let rows = st.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))?;
        Ok(rows.collect::<Result<_, _>>()?)
    }

    pub fn set_caldav(&self, id: &str, hash: &str, ev: &str, todo: &str) -> Result<()> {
        self.c().execute(
            "INSERT OR REPLACE INTO caldav_map(task_id,hash,ev_uid,todo_uid) VALUES(?1,?2,?3,?4)",
            params![id, hash, ev, todo],
        )?;
        Ok(())
    }

    pub fn del_caldav(&self, id: &str) -> Result<()> {
        self.c()
            .execute("DELETE FROM caldav_map WHERE task_id=?1", [id])?;
        Ok(())
    }

    /// Append to error_log, keeping the newest 200 rows.
    pub fn log_error(&self, source: &str, kind: &str, msg: &str) -> Result<()> {
        let c = self.c();
        c.execute(
            "INSERT INTO error_log(at,source,kind,msg) VALUES(?1,?2,?3,?4)",
            params![super::now(), source, kind, msg],
        )?;
        c.execute(
            "DELETE FROM error_log WHERE rowid NOT IN (SELECT rowid FROM error_log ORDER BY rowid DESC LIMIT 200)",
            [],
        )?;
        Ok(())
    }

    /// Newest first: (at, source, kind, msg).
    pub fn recent_errors(&self, n: usize) -> Result<Vec<(i64, String, String, String)>> {
        let c = self.c();
        let mut st =
            c.prepare("SELECT at,source,kind,msg FROM error_log ORDER BY rowid DESC LIMIT ?1")?;
        let rows = st.query_map([n as i64], |r| {
            Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?))
        })?;
        Ok(rows.collect::<Result<_, _>>()?)
    }

    pub fn kv_get(&self, k: &str) -> Result<Option<String>> {
        Ok(self
            .c()
            .query_row("SELECT v FROM kv WHERE k=?1", [k], |r| r.get(0))
            .optional()?)
    }

    pub fn kv_set(&self, k: &str, v: &str) -> Result<()> {
        self.c().execute(
            "INSERT INTO kv(k,v) VALUES(?1,?2) ON CONFLICT(k) DO UPDATE SET v=excluded.v",
            params![k, v],
        )?;
        Ok(())
    }

    pub fn manual_rows(&self) -> Result<Vec<serde_json::Value>> {
        json_rows(&self.c(), "SELECT json FROM manual_rows ORDER BY n")?
            .iter()
            .map(|j| Ok(serde_json::from_str(j)?))
            .collect()
    }

    pub fn add_manual_row(&self, v: &serde_json::Value) -> Result<()> {
        self.c()
            .execute("INSERT INTO manual_rows(json) VALUES(?1)", [v.to_string()])?;
        Ok(())
    }

    /// New job; marks stale running jobs interrupted and keeps items of the last 5 jobs only.
    pub fn scan_new_job(&self, now: i64) -> Result<i64> {
        let mut c = self.c();
        let tx = c.transaction()?;
        tx.execute(
            "UPDATE scan_job SET state='interrupted' WHERE state='running'",
            [],
        )?;
        tx.execute(
            "INSERT INTO scan_job(started,state) VALUES(?1,'running')",
            [now],
        )?;
        let id = tx.last_insert_rowid();
        tx.execute(
            "DELETE FROM scan_item WHERE job NOT IN (SELECT id FROM scan_job ORDER BY id DESC LIMIT 5)",
            [],
        )?;
        tx.commit()?;
        Ok(id)
    }

    pub fn scan_add_items<'a>(
        &self,
        job: i64,
        items: impl IntoIterator<Item = (&'a str, &'a str, &'a str)>,
    ) -> Result<()> {
        let mut c = self.c();
        let tx = c.transaction()?;
        for (src, course, kind) in items {
            tx.execute(
                "INSERT OR IGNORE INTO scan_item(job,src,course,kind) VALUES(?1,?2,?3,?4)",
                params![job, src, course, kind],
            )?;
        }
        Ok(tx.commit()?)
    }

    pub fn scan_add_item(&self, job: i64, src: &str, course: &str, kind: &str) -> Result<()> {
        self.scan_add_items(job, [(src, course, kind)])
    }

    /// Unconfirmed AI proposals.
    pub fn proposed_count(&self) -> Result<i64> {
        Ok(self.c().query_row(
            "SELECT COUNT(*) FROM task WHERE json_extract(json,'$.source')='ai' AND json_extract(json,'$.ai_confirmed') IS NULL",
            [],
            |r| r.get(0),
        )?)
    }

    pub fn scan_item_done(
        &self,
        job: i64,
        src: &str,
        sha: &str,
        status: &str,
        error: &str,
        at: i64,
    ) -> Result<()> {
        self.c().execute(
            "UPDATE scan_item SET sha=?3,status=?4,error=?5,at=?6 WHERE job=?1 AND src=?2",
            params![job, src, sha, status, error, at],
        )?;
        Ok(())
    }

    pub fn scan_finish(&self, job: i64, state: &str, at: i64) -> Result<()> {
        self.c().execute(
            "UPDATE scan_job SET state=?2,finished=?3 WHERE id=?1",
            params![job, state, at],
        )?;
        Ok(())
    }

    pub fn scan_progress(&self) -> Result<Option<serde_json::Value>> {
        let c = self.c();
        let Some((id, started, finished, state)) = c
            .query_row(
                "SELECT id,started,COALESCE(finished,0),state FROM scan_job ORDER BY id DESC LIMIT 1",
                [],
                |r| {
                    Ok((
                        r.get::<_, i64>(0)?,
                        r.get::<_, i64>(1)?,
                        r.get::<_, i64>(2)?,
                        r.get::<_, String>(3)?,
                    ))
                },
            )
            .optional()?
        else {
            return Ok(None);
        };
        let mut counts = serde_json::Map::new();
        let mut st =
            c.prepare("SELECT status,COUNT(*) FROM scan_item WHERE job=?1 GROUP BY status")?;
        for r in st.query_map([id], |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?)))? {
            let (k, n) = r?;
            counts.insert(k, n.into());
        }
        let mut it = c.prepare(
            "SELECT src,course,status,error FROM scan_item WHERE job=?1 AND status IN ('needs_review','error','broken') ORDER BY src LIMIT 200",
        )?;
        let items = it
            .query_map([id], |r| {
                Ok(
                    json!({"src": r.get::<_, String>(0)?, "course": r.get::<_, String>(1)?,
                          "status": r.get::<_, String>(2)?, "error": r.get::<_, String>(3)?}),
                )
            })?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(Some(
            json!({"job": id, "state": state, "started": started, "finished": finished,
                   "counts": counts, "items": items}),
        ))
    }
}
fn get_task(c: &Connection, id: &str) -> Result<Option<Task>> {
    let j: Option<String> = c
        .query_row("SELECT json FROM task WHERE id=?1", [id], |r| r.get(0))
        .optional()?;
    Ok(j.map(|j| serde_json::from_str(&j)).transpose()?)
}

fn put_task(c: &Connection, t: &Task) -> Result<()> {
    c.execute(
        "INSERT INTO task(id,due,json) VALUES(?1,?2,?3)
         ON CONFLICT(id) DO UPDATE SET due=excluded.due, json=excluded.json",
        params![t.id, t.due, serde_json::to_string(t)?],
    )?;
    Ok(())
}

fn get_local(c: &Connection, id: &str) -> Result<Local> {
    let j: Option<String> = c
        .query_row("SELECT json FROM local WHERE task_id=?1", [id], |r| {
            r.get(0)
        })
        .optional()?;
    Ok(match j {
        Some(j) => serde_json::from_str(&j)?,
        None => Local {
            task_id: id.into(),
            ..Default::default()
        },
    })
}

fn put_local(c: &Connection, l: &Local) -> Result<()> {
    c.execute(
        "INSERT INTO local(task_id,json) VALUES(?1,?2) ON CONFLICT(task_id) DO UPDATE SET json=excluded.json",
        params![l.task_id, serde_json::to_string(l)?],
    )?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn t(id: &str, due: i64) -> Task {
        Task {
            id: id.into(),
            course_id: 1,
            course: "C".into(),
            kind: "assign".into(),
            title: format!("T {id}"),
            due,
            source: "moodle".into(),
            ..Default::default()
        }
    }

    #[test]
    fn tasks_roundtrip_sorted_by_due() {
        let db = Db::memory().unwrap();
        db.upsert_tasks(&[t("assign:2", 200), t("assign:1", 100)])
            .unwrap();
        let all = db.all_tasks().unwrap();
        assert_eq!(
            all.iter().map(|x| x.id.as_str()).collect::<Vec<_>>(),
            ["assign:1", "assign:2"]
        );
        let mut changed = t("assign:1", 150);
        changed.title = "nou".into();
        db.upsert_tasks(&[changed.clone()]).unwrap();
        assert_eq!(db.task("assign:1").unwrap().unwrap(), changed);
        assert!(db.task("nope").unwrap().is_none());
    }

    #[test]
    fn ai_upsert_keeps_confirmation() {
        let db = Db::memory().unwrap();
        let mut a = t("ai:abc:0", 0);
        a.source = "ai".into();
        db.upsert_ai_tasks(&[a.clone()]).unwrap();
        db.set_ai_confirmed("ai:abc:0", false).unwrap();
        db.upsert_ai_tasks(&[a]).unwrap();
        assert_eq!(
            db.task("ai:abc:0").unwrap().unwrap().ai_confirmed,
            Some(false)
        );
    }

    #[test]
    fn local_default_and_set() {
        let db = Db::memory().unwrap();
        assert_eq!(
            db.local("x").unwrap(),
            Local {
                task_id: "x".into(),
                ..Default::default()
            }
        );
        let l = Local {
            task_id: "x".into(),
            status: "lliurat".into(),
            files: vec!["a_amin.docx".into()],
            ..Default::default()
        };
        db.set_local(&l).unwrap();
        assert_eq!(db.local("x").unwrap(), l);
        assert_eq!(db.all_local().unwrap(), vec![l]);
    }

    #[test]
    fn cache_kv() {
        let db = Db::memory().unwrap();
        db.cache_put("s", "u", "[]", 1).unwrap();
        assert_eq!(db.cache_get("s").unwrap().as_deref(), Some("[]"));
        assert!(db.cache_fresh("s", 10, 5).unwrap());
        assert!(!db.cache_fresh("s", 10, 11).unwrap());
        assert!(!db.cache_fresh("x", 10, 5).unwrap());
        db.kv_set("a", "1").unwrap();
        db.kv_set("a", "2").unwrap();
        assert_eq!(db.kv_get("a").unwrap().as_deref(), Some("2"));
    }

    #[test]
    fn scan_job_progress_counts() {
        let db = Db::memory().unwrap();
        assert!(db.scan_progress().unwrap().is_none());
        let j = db.scan_new_job(10).unwrap();
        db.scan_add_item(j, "A/x.pdf", "A", "file").unwrap();
        db.scan_add_item(j, "A/x.pdf", "A", "file").unwrap(); // duplicate ignored
        db.scan_add_item(j, "A/y.png", "A", "file").unwrap();
        db.scan_add_item(j, "A/z.txt", "A", "file").unwrap();
        db.scan_item_done(j, "A/x.pdf", "sha1", "done", "", 11)
            .unwrap();
        db.scan_item_done(j, "A/y.png", "", "needs_review", "sense text", 12)
            .unwrap();
        db.scan_add_item(j, "A/w.url", "A", "link").unwrap();
        db.scan_item_done(j, "A/w.url", "", "broken", "enllaç trencat: x", 13)
            .unwrap();
        let p = db.scan_progress().unwrap().unwrap();
        assert_eq!(p["job"], j);
        assert_eq!(p["state"], "running");
        assert_eq!(p["counts"]["done"], 1);
        assert_eq!(p["counts"]["needs_review"], 1);
        assert_eq!(p["counts"]["pending"], 1);
        assert_eq!(p["counts"]["broken"], 1);
        assert_eq!(p["items"].as_array().unwrap().len(), 2);
        assert_eq!(p["items"][0]["src"], "A/w.url");
        assert_eq!(p["items"][1]["error"], "sense text");
        db.scan_finish(j, "done", 20).unwrap();
        let p = db.scan_progress().unwrap().unwrap();
        assert_eq!(
            (p["state"].as_str(), p["finished"].as_i64()),
            (Some("done"), Some(20))
        );
        let j2 = db.scan_new_job(30).unwrap();
        assert!(j2 > j);
        assert_eq!(db.scan_progress().unwrap().unwrap()["job"], j2);
    }

    #[test]
    fn scan_new_job_prunes_and_interrupts() {
        let db = Db::memory().unwrap();
        let mut js = vec![];
        for i in 0..7 {
            let j = db.scan_new_job(i).unwrap();
            db.scan_add_item(j, "A/x", "A", "file").unwrap();
            js.push(j);
        }
        let n = |j: i64| -> i64 {
            db.c()
                .query_row("SELECT COUNT(*) FROM scan_item WHERE job=?1", [j], |r| {
                    r.get(0)
                })
                .unwrap()
        };
        assert_eq!((n(js[1]), n(js[2]), n(js[6])), (0, 1, 1));
        let st: i64 = db
            .c()
            .query_row(
                "SELECT COUNT(*) FROM scan_job WHERE state='running'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(st, 1);
    }

    #[test]
    fn proposed_count_sql_matches_filter() {
        let db = Db::memory().unwrap();
        let mk = |id: &str, src: &str| Task {
            id: id.into(),
            source: src.into(),
            ..Default::default()
        };
        db.upsert_tasks(&[mk("a", "ai"), mk("b", "ai"), mk("c", "moodle")])
            .unwrap();
        db.set_ai_confirmed("b", true).unwrap();
        assert_eq!(db.proposed_count().unwrap(), 1);
    }
}
