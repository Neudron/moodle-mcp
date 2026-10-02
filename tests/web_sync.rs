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

#[tokio::test]
async fn sync_all_writes_result_and_clears_progress() {
    let m = MockMoodle::healthy().await;
    let c = Moodle::new(m.base(), TOKEN);
    let db = Db::memory().unwrap();
    let r = root("result");
    sync_all(&c, &db, &r, true).await.unwrap();
    m.set_drop_s1(true);
    sync_all(&c, &db, &r, true).await.unwrap();
    let res: serde_json::Value =
        serde_json::from_str(&db.kv_get("sync_result").unwrap().unwrap()).unwrap();
    assert_eq!(res["files_removed"], 20); // mock serves 10 s1 files to each of 2 courses
    assert!(res["at"].is_number() && res["errors"].is_array());
    assert!(db
        .kv_get("sync_progress")
        .unwrap()
        .unwrap_or_default()
        .is_empty());
}

#[tokio::test]
async fn empty_course_list_trashes_nothing() {
    let m = MockMoodle::healthy().await;
    let c = Moodle::new(m.base(), TOKEN);
    let db = Db::memory().unwrap();
    let r = root("nocourses");
    sync_all(&c, &db, &r, true).await.unwrap();
    m.set_courses(1);
    sync_all(&c, &db, &r, true).await.unwrap();
    let st = moodle_mcp::state::State::load(&r).unwrap();
    let f = &st.courses["101"].files.values().next().unwrap().path;
    assert!(std::path::Path::new(&r).join(f).is_file());
    assert!(!std::path::Path::new(&r).join(".moodle/trash").exists());
}

#[tokio::test]
async fn unenrolled_course_files_are_trashed() {
    let m = MockMoodle::healthy().await;
    let c = Moodle::new(m.base(), TOKEN);
    let db = Db::memory().unwrap();
    let r = root("unenrol");
    sync_all(&c, &db, &r, true).await.unwrap();
    let f = moodle_mcp::state::State::load(&r).unwrap().courses["101"]
        .files
        .values()
        .next()
        .unwrap()
        .path
        .clone();
    assert!(std::path::Path::new(&r).join(&f).is_file());
    m.set_courses(2);
    sync_all(&c, &db, &r, true).await.unwrap();
    let st = moodle_mcp::state::State::load(&r).unwrap();
    assert!(!st.courses.contains_key("101"));
    assert!(!std::path::Path::new(&r).join(&f).exists());
    assert!(std::path::Path::new(&r).join(".moodle/trash").is_dir());
}

#[tokio::test]
async fn stale_duplicate_module_folders_are_trashed_only_if_pure_sync_output() {
    let m = MockMoodle::healthy().await;
    let c = Moodle::new(m.base(), TOKEN);
    let db = Db::memory().unwrap();
    let r = root("dupes");
    sync_all(&c, &db, &r, true).await.unwrap();
    let root_p = std::path::Path::new(&r);
    let f = moodle_mcp::state::State::load(&r).unwrap().courses["101"]
        .files
        .values()
        .next()
        .unwrap()
        .path
        .clone();
    let live = root_p.join(&f).parent().unwrap().to_path_buf();
    let sec = live.parent().unwrap();
    let meta = std::fs::read_to_string(live.join("meta.json")).unwrap();
    for (name, extra) in [("zz-dup", None), ("zz-mine", Some("notes.txt"))] {
        let d = sec.join(name);
        std::fs::create_dir_all(&d).unwrap();
        std::fs::write(d.join("meta.json"), &meta).unwrap();
        std::fs::write(d.join("README.md"), "x").unwrap();
        if name == "zz-dup" {
            std::fs::copy(root_p.join(&f), d.join("copy.bin")).unwrap();
        }
        if let Some(e) = extra {
            std::fs::write(d.join(e), "mine").unwrap();
        }
    }
    sync_all(&c, &db, &r, true).await.unwrap();
    assert!(!sec.join("zz-dup").exists());
    assert!(sec.join("zz-mine/notes.txt").is_file());
    assert!(live.join("meta.json").is_file());
    let res: serde_json::Value =
        serde_json::from_str(&db.kv_get("sync_result").unwrap().unwrap()).unwrap();
    assert!(res["files_removed"].as_u64().unwrap() >= 1, "{res}");
    assert!(root_p.join(".moodle/trash").is_dir());
}

#[tokio::test]
async fn module_removed_upstream_with_tracked_file_is_trashed_whole() {
    let m = MockMoodle::healthy().await;
    let c = Moodle::new(m.base(), TOKEN);
    let db = Db::memory().unwrap();
    let r = root("gone");
    sync_all(&c, &db, &r, true).await.unwrap();
    let root_p = std::path::Path::new(&r);
    let (f, dir) = {
        let st = moodle_mcp::state::State::load(&r).unwrap();
        let f = st.courses["101"]
            .files
            .values()
            .next()
            .unwrap()
            .path
            .clone();
        let dir = root_p.join(&f).parent().unwrap().to_path_buf();
        (f, dir)
    };
    assert!(dir.join("meta.json").is_file());
    m.set_drop_s1(true);
    sync_all(&c, &db, &r, true).await.unwrap();
    assert!(!dir.exists(), "module folder should be gone");
    let rel = std::path::Path::new(&f).parent().unwrap();
    let tr = root_p.join(".moodle/trash");
    let found = std::fs::read_dir(&tr).unwrap().flatten().any(|e| {
        let d = e.path().join(rel);
        d.join("meta.json").is_file()
            && d.join(std::path::Path::new(&f).file_name().unwrap())
                .is_file()
    });
    assert!(found, "tracked file and meta.json under one sync-<ts>");
}
