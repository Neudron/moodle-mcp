#[path = "integration/mock.rs"]
mod mock;

use mock::{MockConfig, MockMoodle, TOKEN};
use moodle_mcp::{
    moodle::Moodle,
    web::{
        db::Db,
        tasks::{sync_all, Event},
    },
};

fn root(tag: &str) -> String {
    let r = std::env::temp_dir().join(format!("mweb-sync-{tag}-{}", std::process::id()));
    std::fs::create_dir_all(&r).unwrap();
    r.to_string_lossy().into_owned()
}

#[tokio::test]
async fn first_sync_no_events_then_diff() {
    let m = MockMoodle::healthy().await;
    let c = Moodle::new(m.base(), TOKEN);
    let db = Db::memory().unwrap();
    let r = root("first");
    let ev = sync_all(&c, &db, &r, false).await.unwrap();
    assert!(ev.is_empty(), "first sync must not emit events: {ev:?}");
    let ids: Vec<String> = db.all_tasks().unwrap().into_iter().map(|t| t.id).collect();
    for want in ["assign:7", "assign:8", "quiz:5", "post:30"] {
        assert!(ids.iter().any(|i| i == want), "missing {want} in {ids:?}");
    }
    assert!(!db.grades().unwrap().is_empty());
    assert!(
        db.grades()
            .unwrap()
            .iter()
            .all(|(_, v)| v["course_name"].as_str().is_some_and(|s| !s.is_empty())),
        "grades must carry the course name for courses without tasks"
    );
    let mut t = db.task("assign:7").unwrap().unwrap();
    t.due = 1;
    db.upsert_tasks(&[t]).unwrap();
    let ev = sync_all(&c, &db, &r, false).await.unwrap();
    assert!(ev.contains(&Event::DueChanged("assign:7".into())), "{ev:?}");
}

#[tokio::test]
async fn sync_auth_error_keeps_tasks() {
    let ok = MockMoodle::healthy().await;
    let db = Db::memory().unwrap();
    let r = root("auth");
    sync_all(&Moodle::new(ok.base(), TOKEN), &db, &r, false)
        .await
        .unwrap();
    let before = db.all_tasks().unwrap().len();
    let bad = MockMoodle::start(MockConfig {
        unauthorized: true,
        ..Default::default()
    })
    .await;
    let err = sync_all(&Moodle::new(bad.base(), TOKEN), &db, &r, false)
        .await
        .unwrap_err();
    assert!(
        matches!(err, moodle_mcp::errors::CoreError::Auth(_)),
        "{err}"
    );
    assert_eq!(db.all_tasks().unwrap().len(), before);
}

#[tokio::test]
async fn transient_status_failure_keeps_cache_no_false_grade() {
    let db = Db::memory().unwrap();
    let r = root("transient");
    let ok = MockMoodle::healthy().await;
    sync_all(&Moodle::new(ok.base(), TOKEN), &db, &r, false)
        .await
        .unwrap();
    let want = db.task("assign:7").unwrap().unwrap();
    assert!(!want.grade.is_empty());
    let bad = MockMoodle::start(MockConfig {
        fail_fn: Some("mod_assign_get_submission_status"),
        ..Default::default()
    })
    .await;
    sync_all(&Moodle::new(bad.base(), TOKEN), &db, &r, false)
        .await
        .unwrap();
    let got = db.task("assign:7").unwrap().unwrap();
    assert_eq!(
        (&got.grade, &got.moodle_status, &got.feedback_html),
        (&want.grade, &want.moodle_status, &want.feedback_html)
    );
    let ev = sync_all(&Moodle::new(ok.base(), TOKEN), &db, &r, false)
        .await
        .unwrap();
    assert!(
        !ev.iter().any(|e| matches!(e, Event::NewGrade(_))),
        "{ev:?}"
    );
}

#[tokio::test]
async fn sync_stores_submission_files_and_intro_sizes() {
    let m = MockMoodle::healthy().await;
    let db = Db::memory().unwrap();
    sync_all(&Moodle::new(m.base(), TOKEN), &db, &root("subfiles"), false)
        .await
        .unwrap();
    let t = db.task("assign:7").unwrap().unwrap();
    assert_eq!(t.attachments[0].filesize, 4096);
    assert_eq!(t.submission_files.len(), 1);
    assert_eq!(t.submission_files[0].filename, "M4_A1_A_RA2_amin.docx");
    assert_eq!(t.submission_files[0].filesize, 2048);
}

#[tokio::test]
async fn transient_status_failure_keeps_submission_files() {
    let db = Db::memory().unwrap();
    let r = root("subkeep");
    let ok = MockMoodle::healthy().await;
    sync_all(&Moodle::new(ok.base(), TOKEN), &db, &r, false)
        .await
        .unwrap();
    let want = db.task("assign:7").unwrap().unwrap().submission_files;
    assert!(!want.is_empty());
    let bad = MockMoodle::start(MockConfig {
        fail_fn: Some("mod_assign_get_submission_status"),
        ..Default::default()
    })
    .await;
    sync_all(&Moodle::new(bad.base(), TOKEN), &db, &r, false)
        .await
        .unwrap();
    assert_eq!(db.task("assign:7").unwrap().unwrap().submission_files, want);
}

fn ghost(id: &str, course_id: i64) -> moodle_mcp::web::db::Task {
    moodle_mcp::web::db::Task {
        id: id.into(),
        course_id,
        source: "moodle".into(),
        ..Default::default()
    }
}

#[tokio::test]
async fn task_removed_on_moodle_is_deleted() {
    let m = MockMoodle::healthy().await;
    let c = Moodle::new(m.base(), TOKEN);
    let db = Db::memory().unwrap();
    let r = root("gone");
    sync_all(&c, &db, &r, false).await.unwrap();
    let cid = db.task("assign:7").unwrap().unwrap().course_id;
    db.upsert_tasks(&[ghost("assign:999", cid)]).unwrap();
    sync_all(&c, &db, &r, false).await.unwrap();
    assert!(db.task("assign:999").unwrap().is_none());
    assert!(db.task("assign:7").unwrap().is_some());
}

#[tokio::test]
async fn failed_course_call_deletes_nothing() {
    let ok = MockMoodle::healthy().await;
    let db = Db::memory().unwrap();
    let r = root("gonefail");
    sync_all(&Moodle::new(ok.base(), TOKEN), &db, &r, false)
        .await
        .unwrap();
    let cid = db.task("assign:7").unwrap().unwrap().course_id;
    db.upsert_tasks(&[ghost("assign:999", cid)]).unwrap();
    let bad = MockMoodle::start(MockConfig {
        fail_fn: Some("mod_assign_get_assignments"),
        ..Default::default()
    })
    .await;
    sync_all(&Moodle::new(bad.base(), TOKEN), &db, &r, false)
        .await
        .unwrap();
    assert!(db.task("assign:999").unwrap().is_some());
    assert!(db.task("assign:7").unwrap().is_some());
}
