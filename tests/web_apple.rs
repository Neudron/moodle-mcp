use moodle_mcp::moodle::Moodle;
use moodle_mcp::web::{
    self, apple,
    db::{Db, Local, Task},
};
use std::os::unix::fs::PermissionsExt;

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
