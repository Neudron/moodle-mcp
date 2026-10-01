use moodle_mcp::web::db::Db;
use rusqlite::Connection;

fn tmp(tag: &str) -> std::path::PathBuf {
    let d = std::env::temp_dir().join(format!("mweb-db-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d.join("web.db")
}

fn cols(c: &Connection, t: &str) -> Vec<String> {
    let mut st = c
        .prepare(&format!("SELECT name FROM pragma_table_info('{t}')"))
        .unwrap();
    st.query_map([], |r| r.get(0))
        .unwrap()
        .map(|r| r.unwrap())
        .collect()
}

fn ver(c: &Connection) -> i64 {
    c.query_row("PRAGMA user_version", [], |r| r.get(0))
        .unwrap()
}

#[test]
fn migrates_old_caldav_map_keeps_data_and_backs_up() {
    let p = tmp("old");
    {
        let c = Connection::open(&p).unwrap();
        c.execute_batch(
            "CREATE TABLE caldav_map(task_id TEXT PRIMARY KEY, hash TEXT NOT NULL);
             INSERT INTO caldav_map VALUES('t1','h1');
             CREATE TABLE notify_log(key TEXT PRIMARY KEY, at INT);",
        )
        .unwrap();
    }
    drop(Db::open(&p).unwrap());
    let c = Connection::open(&p).unwrap();
    assert!(cols(&c, "caldav_map").contains(&"ev_uid".into()));
    assert!(cols(&c, "caldav_map").contains(&"todo_uid".into()));
    assert_eq!(cols(&c, "notify_log").len(), 0);
    let h: String = c
        .query_row("SELECT hash FROM caldav_map WHERE task_id='t1'", [], |r| {
            r.get(0)
        })
        .unwrap();
    assert_eq!(h, "h1");
    let v = ver(&c);
    assert!(v >= 4);
    let bak = p.with_file_name("web.db.pre-migrate-v0");
    assert!(bak.exists());
    // second run: no-op, backup untouched
    let m = std::fs::metadata(&bak).unwrap().modified().unwrap();
    drop(Db::open(&p).unwrap());
    assert_eq!(ver(&Connection::open(&p).unwrap()), v);
    assert_eq!(std::fs::metadata(&bak).unwrap().modified().unwrap(), m);
}

#[test]
fn legacy_db_with_columns_and_fresh_db() {
    let p = tmp("legacy");
    drop(Db::open(&p).unwrap()); // fresh: no backup
    assert!(!p.with_file_name("web.db.pre-migrate-v0").exists());
    let c = Connection::open(&p).unwrap();
    let v = ver(&c);
    c.execute_batch("PRAGMA user_version=0").unwrap(); // pre-versioning DB that already has the columns
    drop(c);
    drop(Db::open(&p).unwrap());
    assert_eq!(ver(&Connection::open(&p).unwrap()), v);
}

#[test]
fn pragmas_applied() {
    let db = Db::open(&tmp("pragma")).unwrap();
    assert_eq!(db.pragma("busy_timeout").unwrap(), 5000);
    assert_eq!(db.pragma("synchronous").unwrap(), 1);
    assert_eq!(db.pragma("foreign_keys").unwrap(), 1);
}

#[test]
fn checkpoint_truncates_wal() {
    let p = tmp("ckpt");
    let db = Db::open(&p).unwrap();
    db.backup(&p.with_extension("bak")).unwrap();
    let wal = p.with_extension("db-wal");
    assert!(std::fs::metadata(&wal).unwrap().len() > 0);
    db.checkpoint().unwrap();
    assert_eq!(std::fs::metadata(&wal).unwrap().len(), 0);
}

#[test]
fn rotate_log_moves_big_file_only() {
    let d = tmp("rot").parent().unwrap().to_path_buf();
    let log = d.join("web-server.log");
    std::fs::write(&log, b"small").unwrap();
    moodle_mcp::web::rotate_log(&log, 10);
    assert!(log.exists() && !d.join("web-server.log.1").exists());
    std::fs::write(&log, b"0123456789abc").unwrap();
    moodle_mcp::web::rotate_log(&log, 10);
    assert!(!log.exists());
    assert_eq!(
        std::fs::read(d.join("web-server.log.1")).unwrap(),
        b"0123456789abc"
    );
}
