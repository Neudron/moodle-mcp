//! SQLite state for moodle-web. Tasks and local state are JSON blobs keyed
//! by id (no migrations needed when fields are added).
use anyhow::Result;
use rusqlite::{params, Connection, OptionalExtension};
use serde::{Deserialize, Serialize};
use std::path::Path;
use std::sync::Mutex;

pub const STATUSES: [&str; 4] = ["per revisar", "revisat OK", "lliurat", "llegit"];

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct Attachment {
    pub filename: String,
    pub fileurl: String,
    pub mimetype: String,
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
    pub url: String,
    pub opens: i64,
    pub due: i64,
    pub cutoff: i64,
    pub moodle_status: String,
    pub grade: String,
    pub feedback_html: String,
    pub source: String,
    pub ai_confirmed: Option<bool>,
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

pub struct Db(Mutex<Connection>);

const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS task(id TEXT PRIMARY KEY, due INT NOT NULL, json TEXT NOT NULL);
CREATE TABLE IF NOT EXISTS local(task_id TEXT PRIMARY KEY, json TEXT NOT NULL);
CREATE TABLE IF NOT EXISTS grades(course_id INT PRIMARY KEY, json TEXT NOT NULL);
CREATE TABLE IF NOT EXISTS extract_cache(sha TEXT PRIMARY KEY, source_url TEXT, result_json TEXT, at INT);
CREATE TABLE IF NOT EXISTS notify_log(key TEXT PRIMARY KEY, at INT);
CREATE TABLE IF NOT EXISTS caldav_map(task_id TEXT PRIMARY KEY, hash TEXT NOT NULL);
CREATE TABLE IF NOT EXISTS kv(k TEXT PRIMARY KEY, v TEXT NOT NULL);
CREATE TABLE IF NOT EXISTS manual_rows(n INTEGER PRIMARY KEY AUTOINCREMENT, json TEXT NOT NULL);
";

/// Idempotent: ADD COLUMN errors when the column already exists, which is fine.
fn migrate(c: &Connection) {
    for col in ["ev_uid", "todo_uid"] {
        c.execute(
            &format!("ALTER TABLE caldav_map ADD COLUMN {col} TEXT NOT NULL DEFAULT ''"),
            [],
        )
        .ok();
    }
}

fn json_rows(c: &Connection, sql: &str) -> Result<Vec<String>> {
    let mut st = c.prepare(sql)?;
    let rows = st.query_map([], |r| r.get::<_, String>(0))?;
    Ok(rows.collect::<Result<_, _>>()?)
}

impl Db {
    pub fn open(path: &Path) -> Result<Db> {
        let c = Connection::open(path)?;
        c.execute_batch("PRAGMA journal_mode=WAL;")?;
        c.execute_batch(SCHEMA)?;
        migrate(&c);
        Ok(Db(Mutex::new(c)))
    }

    pub fn memory() -> Result<Db> {
        let c = Connection::open_in_memory()?;
        c.execute_batch(SCHEMA)?;
        migrate(&c);
        Ok(Db(Mutex::new(c)))
    }

    fn c(&self) -> std::sync::MutexGuard<'_, Connection> {
        // Poison only means another request panicked; SQLite stays consistent.
        self.0.lock().unwrap_or_else(|e| e.into_inner())
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
        Ok(())
    }

    pub fn upsert_ai_tasks(&self, tasks: &[Task]) -> Result<()> {
        let mut merged = Vec::with_capacity(tasks.len());
        for t in tasks {
            let mut t = t.clone();
            if let Some(old) = self.task(&t.id)? {
                t.ai_confirmed = old.ai_confirmed;
            }
            merged.push(t);
        }
        self.upsert_tasks(&merged)
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

    pub fn set_ai_confirmed(&self, id: &str, keep: bool) -> Result<()> {
        if let Some(mut t) = self.task(id)? {
            t.ai_confirmed = Some(keep);
            self.upsert_tasks(&[t])?;
        }
        Ok(())
    }

    pub fn local(&self, id: &str) -> Result<Local> {
        let j: Option<String> = self
            .c()
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

    pub fn all_local(&self) -> Result<Vec<Local>> {
        json_rows(&self.c(), "SELECT json FROM local ORDER BY task_id")?
            .iter()
            .map(|j| Ok(serde_json::from_str(j)?))
            .collect()
    }

    pub fn set_local(&self, l: &Local) -> Result<()> {
        self.c().execute(
            "INSERT INTO local(task_id,json) VALUES(?1,?2) ON CONFLICT(task_id) DO UPDATE SET json=excluded.json",
            params![l.task_id, serde_json::to_string(l)?],
        )?;
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

    pub fn notified(&self, key: &str) -> Result<bool> {
        Ok(self
            .c()
            .query_row("SELECT 1 FROM notify_log WHERE key=?1", [key], |_| Ok(()))
            .optional()?
            .is_some())
    }

    pub fn mark_notified(&self, key: &str, at: i64) -> Result<()> {
        self.c().execute(
            "INSERT OR IGNORE INTO notify_log(key,at) VALUES(?1,?2)",
            params![key, at],
        )?;
        Ok(())
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

    pub fn cache_put(&self, sha: &str, url: &str, json: &str, at: i64) -> Result<()> {
        self.c().execute(
            "INSERT OR REPLACE INTO extract_cache(sha,source_url,result_json,at) VALUES(?1,?2,?3,?4)",
            params![sha, url, json, at],
        )?;
        Ok(())
    }

    pub fn caldav_hash(&self, id: &str) -> Result<Option<String>> {
        Ok(self
            .c()
            .query_row("SELECT hash FROM caldav_map WHERE task_id=?1", [id], |r| {
                r.get(0)
            })
            .optional()?)
    }

    pub fn set_caldav_hash(&self, id: &str, hash: &str) -> Result<()> {
        self.c().execute(
            "INSERT OR REPLACE INTO caldav_map(task_id,hash) VALUES(?1,?2)",
            params![id, hash],
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
    fn notify_cache_kv() {
        let db = Db::memory().unwrap();
        assert!(!db.notified("k").unwrap());
        db.mark_notified("k", 1).unwrap();
        db.mark_notified("k", 2).unwrap();
        assert!(db.notified("k").unwrap());
        db.cache_put("s", "u", "[]", 1).unwrap();
        assert_eq!(db.cache_get("s").unwrap().as_deref(), Some("[]"));
        db.kv_set("a", "1").unwrap();
        db.kv_set("a", "2").unwrap();
        assert_eq!(db.kv_get("a").unwrap().as_deref(), Some("2"));
        db.set_caldav_hash("t", "h").unwrap();
        assert_eq!(db.caldav_hash("t").unwrap().as_deref(), Some("h"));
    }
}
