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
