use moodle_mcp::moodle::Moodle;
use moodle_mcp::web::apple::{AppleErr, Kind};
use moodle_mcp::web::{
    self, apple,
    db::{Db, Local, Task},
};
use std::os::unix::fs::PermissionsExt;

// Tests share process env (ICLOUD_GLASS_*), so they run one at a time.
static ENV: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

struct Fake {
    dir: std::path::PathBuf,
}

impl Fake {
    /// Fake icloud-glass: logs args; todo-new fails when `todo_fail` holds "auth" or "other".
    fn new(tag: &str) -> Self {
        let dir = std::env::temp_dir().join(format!("mweb-apple-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let d = dir.display();
        let sh = dir.join("fake.sh");
        std::fs::write(
            &sh,
            format!(
                "#!/bin/sh\necho \"$@\" >> {d}/log\n[ \"$1\" = list-new ] && rm -f {d}/fail_todo-new\nif [ -f {d}/fail_$1 ]; then\n case \"$(cat {d}/fail_$1)\" in\n auth) echo '{{\"error\":\"x\",\"kind\":\"auth\"}}'; exit 2;;\n nolist) echo '{{\"error\":\"La lista no existe.\",\"kind\":\"config\"}}'; exit 1;;\n config) echo '{{\"error\":\"falta\",\"kind\":\"config\"}}'; exit 1;;\n *) echo oops >&2; exit 1;;\n esac\nfi\necho '{{\"uid\":\"U-'$1'\"}}'\n"
            ),
        )
        .unwrap();
        std::fs::set_permissions(&sh, std::fs::Permissions::from_mode(0o755)).unwrap();
        std::env::set_var("ICLOUD_GLASS_CMD", &sh);
        std::env::set_var("ICLOUD_GLASS_STATE", dir.join("state"));
        // Fake importer: fails unless `imp_ok` exists; success heals marker + todo failures.
        let imp = dir.join("imp.sh");
        std::fs::write(
            &imp,
            format!(
                "#!/bin/sh\necho \"$@\" >> {d}/imp_log\nif [ -f {d}/imp_ok ]; then rm -f {d}/state/reminders-auth-required {d}/fail_todo-new; echo '{{\"ok\":true,\"token_expiry\":1793381126}}'; exit 0; fi\necho '{{\"ok\":false,\"kind\":\"auth\",\"msg\":\"stale\"}}'; exit 2\n"
            ),
        )
        .unwrap();
        std::fs::set_permissions(&imp, std::fs::Permissions::from_mode(0o755)).unwrap();
        std::env::set_var("ICLOUD_IMPORT_CMD", &imp);
        Fake { dir }
    }
    fn import_ok(&self) {
        std::fs::write(self.dir.join("imp_ok"), "").unwrap();
    }
    fn imports(&self) -> usize {
        std::fs::read_to_string(self.dir.join("imp_log"))
            .unwrap_or_default()
            .lines()
            .count()
    }
    fn marker(&self) {
        std::fs::create_dir_all(self.dir.join("state")).unwrap();
        std::fs::write(self.dir.join("state/reminders-auth-required"), "").unwrap();
    }
    fn fail(&self, cmd: &str, kind: &str) {
        std::fs::write(self.dir.join(format!("fail_{cmd}")), kind).unwrap();
    }
    fn heal(&self, cmd: &str) {
        std::fs::remove_file(self.dir.join(format!("fail_{cmd}"))).unwrap();
    }
    fn calls(&self, cmd: &str) -> usize {
        std::fs::read_to_string(self.dir.join("log"))
            .unwrap_or_default()
            .lines()
            .filter(|l| l.starts_with(cmd))
            .count()
    }
    fn app(&self) -> web::App {
        web::App::new(
            Db::memory().unwrap(),
            Moodle::new("http://x", "t"),
            self.dir.to_str().unwrap(),
            0,
        )
    }
}

fn task_n(n: u32, due: i64) -> Task {
    Task {
        id: format!("assign:{n}"),
        title: format!("T{n}"),
        ..task(due, "")
    }
}

#[tokio::test]
async fn cli_types_errors() {
    let _g = ENV.lock().await;
    let f = Fake::new("cli");
    f.fail("a", "auth");
    assert_eq!(
        apple::cli(&["a"]).await,
        Err(AppleErr {
            kind: Kind::Auth,
            msg: "x".into()
        })
    );
    f.fail("b", "other");
    assert_eq!(apple::cli(&["b"]).await.unwrap_err().kind, Kind::Other);
    assert_eq!(apple::cli(&["c"]).await.unwrap(), "U-c");
    // exit code 2 alone means Auth, message capped at 200 chars and first line.
    let sh = f.dir.join("two.sh");
    std::fs::write(
        &sh,
        format!("#!/bin/sh\necho '{}' >&2\nexit 2\n", "y".repeat(300)),
    )
    .unwrap();
    std::fs::set_permissions(&sh, std::fs::Permissions::from_mode(0o755)).unwrap();
    std::env::set_var("ICLOUD_GLASS_CMD", &sh);
    let e = apple::cli(&["z"]).await.unwrap_err();
    assert_eq!((e.kind, e.msg.len()), (Kind::Auth, 200));
}

#[tokio::test]
async fn auth_failure_stops_todos_but_not_events() {
    let _g = ENV.lock().await;
    let f = Fake::new("abort");
    f.fail("todo-new", "auth");
    let app = f.app();
    let due = web::now() + 86400 * 5;
    app.db
        .upsert_tasks(&[task_n(1, due), task_n(2, due), task_n(3, due)])
        .unwrap();
    let r = apple::sync(&app).await;
    assert_eq!((f.calls("todo-new"), f.calls("event-new")), (1, 3));
    assert!(r.auth());
    assert_eq!(r.errors, vec!["x".to_string()]);
    assert_eq!(app.db.kv_get("apple_state").unwrap().unwrap(), "auth");
    let log = app.db.recent_errors(5).unwrap();
    assert_eq!(
        (log[0].1.as_str(), log[0].2.as_str()),
        ("apple", "apple-auth")
    );
    // marker present: no todo spawn at all.
    std::fs::create_dir_all(f.dir.join("state")).unwrap();
    std::fs::write(f.dir.join("state/reminders-auth-required"), "").unwrap();
    f.heal("todo-new");
    let r = apple::sync(&app).await;
    assert_eq!(f.calls("todo-new"), 1, "blocked by marker");
    assert!(r.auth());
}

#[tokio::test]
async fn todo_failure_keeps_event_hash_and_retries_only_todo() {
    let _g = ENV.lock().await;
    let f = Fake::new("hash");
    f.fail("todo-new", "other");
    let app = f.app();
    let due = web::now() + 86400 * 5;
    app.db.upsert_tasks(&[task_n(1, due)]).unwrap();
    let r = apple::sync(&app).await;
    assert!(!r.auth() && r.errors.len() == 1);
    let rows = app.db.caldav_rows().unwrap();
    assert!(!rows[0].1.is_empty(), "real event hash stored");
    assert_eq!(
        (rows[0].2.as_str(), rows[0].3.as_str()),
        ("U-event-new", "")
    );
    assert_eq!(app.db.kv_get("apple_state").unwrap().unwrap(), "err");
    f.heal("todo-new");
    apple::sync(&app).await;
    assert_eq!((f.calls("event-new"), f.calls("todo-new")), (1, 2));
    assert_eq!(app.db.caldav_rows().unwrap()[0].3, "U-todo-new");
    assert_eq!(app.db.kv_get("apple_state").unwrap().unwrap(), "ok");
}

fn task(due: i64, status: &str) -> Task {
    Task {
        id: "assign:1".into(),
        course: "DAW\r\nX".into(),
        kind: "assign".into(),
        title: "Pràctica".into(),
        url: "http://m/1\r\nevil".into(),
        due,
        moodle_status: status.into(),
        ..Default::default()
    }
}

#[tokio::test]
async fn mirrors_lifecycle() {
    let _g = ENV.lock().await;
    let dir = std::env::temp_dir().join(format!("mweb-apple-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let log = dir.join("log");
    let sh = dir.join("fake.sh");
    std::fs::write(
        &sh,
        format!(
            "#!/bin/sh\necho \"$@\" >> {}\necho '{{\"uid\":\"U-'$1'\"}}'\n",
            log.display()
        ),
    )
    .unwrap();
    std::fs::set_permissions(&sh, std::fs::Permissions::from_mode(0o755)).unwrap();
    std::env::set_var("ICLOUD_GLASS_CMD", &sh);
    std::env::set_var("ICLOUD_GLASS_STATE", dir.join("state"));
    let app = web::App::new(
        Db::memory().unwrap(),
        Moodle::new("http://x", "t"),
        dir.to_str().unwrap(),
        0,
    );
    let lines = || std::fs::read_to_string(&log).unwrap_or_default();
    let due = web::now() + 86400 * 5;

    app.db.upsert_tasks(&[task(due, "")]).unwrap();
    let r = apple::sync(&app).await;
    assert_eq!(
        (r.created, r.updated, r.errors.len()),
        (1, 0, 0),
        "{:?}",
        r.errors
    );
    let l = lines();
    assert!(l.contains("event-new") && l.contains("todo-new"));
    assert!(
        l.contains("DAW X: Pràctica") && l.contains("http://m/1 evil"),
        "CR/LF stripped"
    );
    assert!(l.contains("--alarm-minutes 4320 --alarm-minutes 1440 --alarm-minutes 180"));
    assert_eq!(l.lines().count(), 2);

    apple::sync(&app).await;
    assert_eq!(lines().lines().count(), 2, "idempotent");

    app.db.upsert_tasks(&[task(due + 3600, "")]).unwrap();
    let r = apple::sync(&app).await;
    assert_eq!((r.created, r.updated), (0, 1));
    assert!(lines().contains("--uid U-event-new"), "same uid reused");

    app.db
        .upsert_tasks(&[task(due + 3600, "submitted")])
        .unwrap();
    let r = apple::sync(&app).await;
    assert_eq!((r.deleted, r.done), (1, 1));
    assert!(lines().contains("event-delete --calendar Institut U-event-new"));
    let n = lines().lines().count();
    apple::sync(&app).await;
    assert_eq!(lines().lines().count(), n, "row cleared");

    // locally delivered
    app.db.upsert_tasks(&[task(due, "")]).unwrap();
    apple::sync(&app).await;
    app.db
        .set_local(&Local {
            task_id: "assign:1".into(),
            status: "lliurat".into(),
            ..Default::default()
        })
        .unwrap();
    let r = apple::sync(&app).await;
    assert_eq!((r.deleted, r.done), (1, 1));
}

#[tokio::test]
async fn marker_triggers_one_import_then_todos_run() {
    let _g = ENV.lock().await;
    let f = Fake::new("impmarker");
    f.import_ok();
    f.marker();
    let app = f.app();
    let due = web::now() + 86400 * 5;
    app.db
        .upsert_tasks(&[task_n(1, due), task_n(2, due)])
        .unwrap();
    let r = apple::sync(&app).await;
    assert!(r.errors.is_empty(), "{:?}", r.errors);
    assert_eq!((f.imports(), f.calls("todo-new")), (1, 2));
    assert_eq!(app.db.kv_get("apple_state").unwrap().unwrap(), "ok");
    assert_eq!(
        app.db.kv_get("apple_token_expiry").unwrap().unwrap(),
        "1793381126"
    );
}

#[tokio::test]
async fn todo_auth_failure_imports_once_and_retries_once() {
    let _g = ENV.lock().await;
    let f = Fake::new("impretry");
    f.fail("todo-new", "auth");
    f.import_ok();
    let app = f.app();
    let due = web::now() + 86400 * 5;
    app.db
        .upsert_tasks(&[task_n(1, due), task_n(2, due), task_n(3, due)])
        .unwrap();
    let r = apple::sync(&app).await;
    assert!(r.errors.is_empty(), "{:?}", r.errors);
    assert_eq!((f.imports(), f.calls("todo-new")), (1, 4));
    assert!(app
        .db
        .caldav_rows()
        .unwrap()
        .iter()
        .all(|r| r.3 == "U-todo-new"));
}

#[tokio::test]
async fn failed_import_leaves_auth_and_no_retry() {
    let _g = ENV.lock().await;
    let f = Fake::new("impfail");
    f.fail("todo-new", "auth");
    let app = f.app();
    let due = web::now() + 86400 * 5;
    app.db
        .upsert_tasks(&[task_n(1, due), task_n(2, due)])
        .unwrap();
    let r = apple::sync(&app).await;
    assert!(r.auth());
    assert_eq!((f.imports(), f.calls("todo-new")), (1, 1));
    assert_eq!(app.db.kv_get("apple_state").unwrap().unwrap(), "auth");
}

#[tokio::test]
async fn import_rate_limited() {
    let _g = ENV.lock().await;
    let f = Fake::new("imprate");
    f.import_ok();
    f.marker();
    let app = f.app();
    let due = web::now() + 86400 * 5;
    app.db.upsert_tasks(&[task_n(1, due)]).unwrap();
    app.db
        .kv_set("apple_import_at", &(web::now() - 60).to_string())
        .unwrap();
    let r = apple::sync(&app).await;
    assert!(r.auth(), "import skipped inside 15 min window");
    assert_eq!(f.imports(), 0);
    app.db
        .kv_set("apple_import_at", &(web::now() - 1000).to_string())
        .unwrap();
    let r = apple::sync(&app).await;
    assert!(r.errors.is_empty(), "{:?}", r.errors);
    assert_eq!(f.imports(), 1);
    assert!(matches!(apple::import(&app, 60).await, apple::Imp::Limited(n) if n > 0));
}

#[test]
fn proactive_decision() {
    let now = 1_000_000;
    assert!(apple::needs_refresh(now + 86400 * 6, now));
    assert!(!apple::needs_refresh(now + 86400 * 8, now));
    assert!(!apple::needs_refresh(0, now), "unknown expiry: leave alone");
    assert!(apple::needs_refresh(now - 5, now), "already expired");
}

#[tokio::test]
async fn missing_list_is_created_once_then_retried() {
    let _g = ENV.lock().await;
    let f = Fake::new("nolist");
    f.fail("todo-new", "nolist");
    let app = f.app();
    let due = web::now() + 86400 * 5;
    app.db
        .upsert_tasks(&[task_n(1, due), task_n(2, due)])
        .unwrap();
    let r = apple::sync(&app).await;
    assert_eq!((f.calls("list-new"), f.calls("todo-new")), (1, 3));
    assert!(r.errors.is_empty());
    assert_eq!(app.db.kv_get("apple_state").unwrap().unwrap(), "ok");
}

#[tokio::test]
async fn calendar_config_error_stops_calendar_and_is_reported() {
    let _g = ENV.lock().await;
    let f = Fake::new("calcfg");
    f.fail("event-new", "config");
    let app = f.app();
    let due = web::now() + 86400 * 5;
    app.db
        .upsert_tasks(&[task_n(1, due), task_n(2, due), task_n(3, due)])
        .unwrap();
    let r = apple::sync(&app).await;
    assert_eq!(f.calls("event-new"), 1, "no noisy retries");
    assert_eq!(r.errors, vec!["falta".to_string()]);
    assert_eq!(app.db.kv_get("apple_state").unwrap().unwrap(), "config");
    assert_eq!(app.db.kv_get("apple_error").unwrap().unwrap(), "falta");
    // other calendar failure: state "cal", status says calendar error.
    f.fail("event-new", "other");
    apple::sync(&app).await;
    assert_eq!(app.db.kv_get("apple_state").unwrap().unwrap(), "cal");
    assert_eq!(apple::status(&app)["calendar"], "error");
}
