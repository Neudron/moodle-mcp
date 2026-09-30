#[path = "integration/mock.rs"]
mod mock;

use mock::{MockConfig, MockMoodle, TOKEN};
use moodle_mcp::{
    moodle::Moodle,
    state::{CourseState, FileEntry, State},
    web::{
        self,
        db::{Attachment, Db, Local, Task},
        organize,
    },
};
use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
    sync::Arc,
};

fn root(tag: &str) -> PathBuf {
    let r = std::env::temp_dir().join(format!("mweb-org-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&r);
    std::fs::create_dir_all(&r).unwrap();
    r
}

fn app_on(r: &Path, base: &str, port: u16) -> Arc<web::App> {
    Arc::new(web::App::new(
        Db::memory().unwrap(),
        Moodle::new(base, TOKEN),
        r.to_str().unwrap(),
        port,
    ))
}

fn app_at(r: &Path, base: &str) -> Arc<web::App> {
    app_on(r, base, 0)
}

fn put(r: &Path, rel: &str, body: &str) {
    let p = r.join(rel);
    std::fs::create_dir_all(p.parent().unwrap()).unwrap();
    std::fs::write(p, body).unwrap();
}

fn task(id: &str, course: &str, title: &str) -> Task {
    Task {
        id: id.into(),
        course: course.into(),
        title: title.into(),
        kind: "assign".into(),
        moodle_status: "new".into(),
        ..Default::default()
    }
}

fn local(id: &str, module: &str, status: &str, files: &[&str]) -> Local {
    Local {
        task_id: id.into(),
        module: module.into(),
        status: status.into(),
        files: files.iter().map(|s| s.to_string()).collect(),
        ..Default::default()
    }
}

fn read(r: &Path, rel: &str) -> String {
    std::fs::read_to_string(r.join(rel)).unwrap_or_else(|e| panic!("{rel}: {e}"))
}

fn attachment(m: &MockMoodle, name: &str, file: &str) -> Attachment {
    let path = format!("/101/mod_resource/content/{file}");
    Attachment {
        filename: name.into(),
        fileurl: format!("{}/webservice/pluginfile.php{path}", m.base()),
        filesize: m.pluginfile(&path).unwrap().len() as i64,
        ..Default::default()
    }
}

#[tokio::test]
async fn delivered_sync_downloads_and_keeps_differing_copy() {
    let m = MockMoodle::start(MockConfig::default()).await;
    let r = root("delivered");
    let app = app_at(&r, m.base());
    let pdf = m
        .pluginfile("/101/mod_resource/content/0/s1-m0-0.pdf")
        .unwrap()
        .to_vec();
    let pptx = m
        .pluginfile("/101/mod_resource/content/1/s1-m0-1.pptx")
        .unwrap()
        .to_vec();
    let mut t = task("assign:7", "C1", "Entrega A");
    t.moodle_status = "submitted".into();
    t.submission_files = vec![
        attachment(&m, "s1-m0-0.pdf", "0/s1-m0-0.pdf"),
        attachment(&m, "s1-m0-1.pptx", "1/s1-m0-1.pptx"),
        attachment(&m, "../evil.txt", "0/s1-m0-0.pdf"),
    ];
    app.db.upsert_tasks(&[t]).unwrap();
    app.db
        .set_local(&local(
            "assign:7",
            "Mod A",
            "per revisar",
            &[
                "submit/Mod A/Entrega A/s1-m0-0.pdf",
                "C1/loose/s1-m0-1.pptx",
            ],
        ))
        .unwrap();
    put(&r, "submit/Mod A/Entrega A/s1-m0-0.pdf", "mine");
    put(&r, "C1/loose/s1-m0-1.pptx", "loose copy");
    let rep = organize_delivered(&app).await;
    assert_eq!(
        (rep.tasks, rep.downloaded, rep.moodle_copies),
        (1, 2, 1),
        "{:?}",
        rep.errors
    );
    let d = "submit/_lliurat/Mod A/Entrega A";
    assert_eq!(read(&r, &format!("{d}/s1-m0-0.pdf")), "mine");
    assert_eq!(
        std::fs::read(r.join(format!("{d}/s1-m0-0.moodle.pdf"))).unwrap(),
        pdf
    );
    assert_eq!(
        std::fs::read(r.join(format!("{d}/s1-m0-1.pptx"))).unwrap(),
        pptx
    );
    assert!(r.join(format!("{d}/evil.txt")).exists());
    assert!(!r.join("submit/_lliurat/Mod A/evil.txt").exists());
    let l = app.db.local("assign:7").unwrap();
    assert_eq!(l.status, "lliurat");
    assert!(l.files.contains(&format!("{d}/s1-m0-0.pdf")));
    assert!(l.files.contains(&format!("{d}/s1-m0-1.pptx")));
    assert!(
        !l.files.iter().any(|f| f.starts_with("C1/loose")),
        "{:?}",
        l.files
    );
    assert!(std::fs::read_to_string(r.join("submit/README.md"))
        .unwrap()
        .contains("lliurat"));
    // idempotent: nothing new, nothing rewritten
    let again = organize_delivered(&app).await;
    assert_eq!(
        (again.tasks, again.downloaded, again.moodle_copies),
        (0, 0, 0),
        "{:?}",
        again.errors
    );
}

async fn organize_delivered(app: &Arc<web::App>) -> organize::Delivered {
    organize::delivered_sync(app).await.unwrap()
}

#[tokio::test]
async fn delivered_sync_ignores_unsubmitted_and_reports_download_errors() {
    let m = MockMoodle::start(MockConfig::default()).await;
    let r = root("delivered-err");
    let app = app_at(&r, m.base());
    let mut draft = task("assign:1", "C1", "Esborrany");
    draft.submission_files = vec![attachment(&m, "a.pdf", "0/s1-m0-0.pdf")];
    let mut bad = task("assign:2", "C1", "Trencada");
    bad.moodle_status = "submitted".into();
    bad.submission_files = vec![Attachment {
        filename: "gone.pdf".into(),
        fileurl: format!(
            "{}/webservice/pluginfile.php/101/mod_resource/content/9999/gone.pdf",
            m.base()
        ),
        ..Default::default()
    }];
    app.db.upsert_tasks(&[draft, bad]).unwrap();
    let rep = organize_delivered(&app).await;
    assert_eq!(rep.tasks, 0);
    assert_eq!(rep.errors.len(), 1, "{:?}", rep.errors);
    assert!(app.db.local("assign:1").unwrap().status.is_empty());
    assert!(!r.join("submit/_lliurat/C1/Esborrany").exists());
    assert!(!rep.errors[0].contains(TOKEN));
}
#[tokio::test]
async fn delivered_route_needs_guard_headers() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let app = app_on(&root("dr"), "http://127.0.0.1:9", port);
    tokio::spawn(web::serve_on(listener, Arc::clone(&app)));
    let base = format!("http://127.0.0.1:{port}");
    let c = reqwest::Client::new();
    let url = format!("{base}/api/delivered-sync");
    assert_eq!(c.post(&url).send().await.unwrap().status(), 403);
    let ok = c
        .post(&url)
        .header("origin", &base)
        .header("x-neu", "1")
        .send()
        .await
        .unwrap();
    assert_eq!(ok.status(), 200);
    let v: serde_json::Value = ok.json().await.unwrap();
    assert_eq!(v["tasks"], 0);
}

/// Every file under `r` (hidden included) except files the app rewrites on its own.
fn snapshot(r: &Path) -> BTreeMap<String, Vec<u8>> {
    fn go(r: &Path, d: &Path, out: &mut BTreeMap<String, Vec<u8>>) {
        for e in std::fs::read_dir(d).unwrap().flatten() {
            let p = e.path();
            let rel = p.strip_prefix(r).unwrap().to_string_lossy().into_owned();
            if p.is_dir() {
                go(r, &p, out);
            } else if !rel.starts_with(".moodle/moves.jsonl") && !rel.contains("README.md") {
                out.insert(rel, std::fs::read(&p).unwrap());
            }
        }
    }
    let mut out = BTreeMap::new();
    go(r, r, &mut out);
    out
}

/// A tree that exercises rules 1 to 6. Returns the app.
fn messy(tag: &str) -> (Arc<web::App>, PathBuf) {
    messy_on(tag, 0)
}

fn messy_on(tag: &str, port: u16) -> (Arc<web::App>, PathBuf) {
    let r = root(tag);
    let app = app_on(&r, "http://127.0.0.1:9", port);
    put(&r, "C1/index.md", "---\ntitle: C1\n---\n");
    put(&r, "C1/RA1/enunciat.pdf", "teacher");
    put(&r, "C1/RA1/Tasca 1 final_amin.docx", "final");
    put(&r, "C1/RA1/loose_amin.txt", "loose");
    put(&r, "C1/lliuraments/dup_amin.docx", "same");
    put(&r, "C1/lliuraments/entrega 2 nova_amin.docx", "nova");
    put(&r, "submit/_lliurat/Mod A/Entrega 2/dup_amin.docx", "same");
    put(&r, "submit/Old/Tasca 1/Tasca_amin.docx", "t1");
    put(&r, "submit/_lliurat/Mod B/Altra/x_amin.txt", "x");
    put(&r, "submit/misc/E1_Amin.pdf", "pdf");
    put(&r, "submit/_prova/prova.docx", "test");
    put(&r, "decks/a.txt", "deck");
    put(&r, "marques/sub/b.txt", "marca");
    put(&r, "sistemas/c.txt", "sis");
    put(&r, "school-verify.skill", "skill");
    for (id, title) in [
        ("assign:1", "Tasca 1"),
        ("assign:2", "Entrega 2"),
        ("assign:3", "Altra"),
    ] {
        app.db.upsert_tasks(&[task(id, "C1", title)]).unwrap();
    }
    let l = [
        local(
            "assign:1",
            "Mod A",
            "per revisar",
            &["submit/Old/Tasca 1/Tasca_amin.docx"],
        ),
        local(
            "assign:2",
            "Mod A",
            "lliurat",
            &["submit/_lliurat/Mod A/Entrega 2/dup_amin.docx"],
        ),
        local(
            "assign:3",
            "Mod B",
            "lliurat",
            &["submit/_lliurat/Mod B/Altra/x_amin.txt"],
        ),
    ];
    for l in &l {
        app.db.set_local(l).unwrap();
    }
    (app, r)
}

#[tokio::test]
async fn organize_rules_1_to_6() {
    let (app, r) = messy("rules");
    let rep = organize::run(&app).await.unwrap();
    // rule 3
    assert_eq!(read(&r, "submit/misc/E1_amin.pdf"), "pdf");
    assert!(!r.join("submit/misc/E1_Amin.pdf").exists());
    // rule 4
    assert!(!r.join("submit/_prova").exists());
    assert_eq!(
        read(
            &r,
            &format!(".moodle/trash/{}/submit/_prova/prova.docx", rep.run)
        ),
        "test"
    );
    // rule 1: title match, else _solts
    assert_eq!(
        read(&r, "submit/Mod A/Tasca 1/Tasca 1 final_amin.docx"),
        "final"
    );
    assert_eq!(read(&r, "submit/_solts/loose_amin.txt"), "loose");
    // rule 2: duplicate by sha trashed, the rest joins the delivered task folder
    assert!(!r.join("C1/lliuraments/dup_amin.docx").exists());
    assert_eq!(
        read(
            &r,
            &format!(".moodle/trash/{}/C1/lliuraments/dup_amin.docx", rep.run)
        ),
        "same"
    );
    assert_eq!(
        read(
            &r,
            "submit/_lliurat/Mod A/Entrega 2/entrega 2 nova_amin.docx"
        ),
        "nova"
    );
    assert!(!r.join("C1/lliuraments").exists());
    // rule 5
    for f in [
        "decks/a.txt",
        "marques/sub/b.txt",
        "sistemas/c.txt",
        "school-verify.skill",
    ] {
        assert!(!r.join(f).exists(), "{f}");
        assert!(r.join("_personal").join(f).exists(), "{f}");
    }
    assert!(!r.join("marques").exists());
    // rule 6: Mod A is the canonical label (2 tasks vs 1), files and Local follow
    assert_eq!(read(&r, "submit/Mod A/Tasca 1/Tasca_amin.docx"), "t1");
    assert!(!r.join("submit/Old").exists());
    assert_eq!(read(&r, "submit/_lliurat/Mod A/Altra/x_amin.txt"), "x");
    assert_eq!(
        app.db.local("assign:1").unwrap().files,
        ["submit/Mod A/Tasca 1/Tasca_amin.docx"]
    );
    let l3 = app.db.local("assign:3").unwrap();
    assert_eq!(l3.module, "Mod A");
    assert_eq!(l3.files, ["submit/_lliurat/Mod A/Altra/x_amin.txt"]);
    // teacher file untouched
    assert_eq!(read(&r, "C1/RA1/enunciat.pdf"), "teacher");
    assert!(
        rep.errors.iter().all(|e| e.starts_with("courses.json")),
        "{:?}",
        rep.errors
    );
}

#[tokio::test]
async fn teacher_and_system_files_never_touched() {
    let r = root("teacher");
    let app = app_at(&r, "http://127.0.0.1:9");
    put(&r, "C1/index.md", "x");
    put(&r, "C1/RA1/Teacher_amin.docx", "t1");
    put(&r, "C1/RA1/Cap_Amin.docx", "t2");
    put(&r, "C1/lliuraments/T3.pdf", "t3");
    put(&r, "moodle-mcp/x_Amin.txt", "m");
    put(&r, ".claude/y_amin.txt", "c");
    put(&r, ".env", "SECRET=1");
    put(&r, ".moodle/z_Amin.txt", "d");
    let mut st = State::default();
    let files = [
        "C1/RA1/Teacher_amin.docx",
        "C1/RA1/Cap_Amin.docx",
        "C1/lliuraments/T3.pdf",
    ]
    .iter()
    .map(|p| {
        (
            format!("https://m/{p}"),
            FileEntry {
                path: p.to_string(),
                ..Default::default()
            },
        )
    })
    .collect();
    st.courses.insert(
        "1".into(),
        CourseState {
            files,
            ..Default::default()
        },
    );
    st.save(r.to_str().unwrap()).unwrap();
    let before = snapshot(&r);
    let rep = organize::run(&app).await.unwrap();
    assert_eq!(rep.ops, 0);
    assert_eq!(snapshot(&r), before);
}

#[tokio::test]
async fn second_run_is_a_noop() {
    let (app, _r) = messy("twice");
    organize::run(&app).await.unwrap();
    assert_eq!(organize::run(&app).await.unwrap().ops, 0);
}

#[tokio::test]
async fn undo_restores_byte_identical_tree_and_links() {
    let (app, r) = messy("undo");
    let before = snapshot(&r);
    let links: Vec<Local> = app.db.all_local().unwrap();
    let rep = organize::run(&app).await.unwrap();
    assert!(rep.ops > 10);
    assert_ne!(snapshot(&r), before);
    let log = organize::last_log(r.to_str().unwrap()).unwrap();
    assert_eq!(log["run"], rep.run);
    assert_eq!(log["ops"].as_array().unwrap().len(), rep.ops);
    let u = organize::undo(&app).await.unwrap();
    assert_eq!(u.restored, rep.ops);
    assert!(u.skipped.is_empty(), "{:?}", u.skipped);
    assert_eq!(snapshot(&r), before);
    assert_eq!(app.db.all_local().unwrap(), links);
    assert_eq!(organize::last_log(r.to_str().unwrap()).unwrap()["run"], 0);
    assert!(organize::undo(&app).await.is_err());
}

#[tokio::test]
async fn undo_skips_files_edited_after_organize() {
    let (app, r) = messy("undo-edit");
    organize::run(&app).await.unwrap();
    std::fs::write(r.join("submit/_solts/loose_amin.txt"), "edited").unwrap();
    let u = organize::undo(&app).await.unwrap();
    assert_eq!(u.skipped.len(), 1, "{:?}", u.skipped);
    assert_eq!(read(&r, "submit/_solts/loose_amin.txt"), "edited");
    assert_eq!(read(&r, "C1/RA1/enunciat.pdf"), "teacher");
    assert_eq!(read(&r, "C1/lliuraments/dup_amin.docx"), "same");
    // the stuck op stays in the log for a later undo
    assert_eq!(
        organize::last_log(r.to_str().unwrap()).unwrap()["ops"]
            .as_array()
            .unwrap()
            .len(),
        1
    );
}

#[tokio::test]
async fn courses_json_rebuilt_and_undone() {
    let m = MockMoodle::start(MockConfig::default()).await;
    let r = root("courses");
    let app = app_at(&r, m.base());
    put(
        &r,
        ".moodle/courses.json",
        "[{\"id\":1,\"shortname\":\"OLD\"}]",
    );
    let rep = organize::run(&app).await.unwrap();
    assert!(rep.errors.is_empty(), "{:?}", rep.errors);
    let v: serde_json::Value = serde_json::from_str(&read(&r, ".moodle/courses.json")).unwrap();
    assert_eq!(v.as_array().unwrap().len(), 2);
    assert_eq!(v[0]["shortname"], "MOCK101");
    organize::undo(&app).await.unwrap();
    assert_eq!(
        read(&r, ".moodle/courses.json"),
        "[{\"id\":1,\"shortname\":\"OLD\"}]"
    );
}

#[tokio::test]
async fn routes_run_log_undo_and_delivered() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let (app, _r) = messy_on("routes", port);
    tokio::spawn(web::serve_on(listener, Arc::clone(&app)));
    let base = format!("http://127.0.0.1:{port}");
    let c = reqwest::Client::new();
    let post = |p: &str| {
        c.post(format!("{base}{p}"))
            .header("origin", &base)
            .header("x-neu", "1")
    };
    let run: serde_json::Value = post("/api/organize")
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert!(run["ops"].as_u64().unwrap() > 10);
    let log: serde_json::Value = c
        .get(format!("{base}/api/organize"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(log["run"], run["run"]);
    let u: serde_json::Value = post("/api/organize/undo")
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(u["restored"], run["ops"]);
    let d = post("/api/delivered-sync").send().await.unwrap();
    assert_eq!(d.status(), 200);
    assert_eq!(
        c.post(format!("{base}/api/organize"))
            .send()
            .await
            .unwrap()
            .status(),
        403
    );
}

#[tokio::test]
async fn stale_link_does_not_abort_modules_rule() {
    let r = root("stale");
    let app = app_at(&r, "http://127.0.0.1:9");
    app.db.upsert_tasks(&[task("t1", "C1", "Act")]).unwrap();
    app.db
        .set_local(&local(
            "t1",
            "RA1",
            "started",
            &["submit/Old/ghost.docx", "submit/Old/real.docx"],
        ))
        .unwrap();
    put(&r, "submit/Old/real.docx", "x");
    let rep = organize::run(&app).await.unwrap();
    assert!(
        rep.errors.iter().all(|e| e.starts_with("courses.json")),
        "{:?}",
        rep.errors
    );
    assert!(rep.ops >= 1, "real file must still be placed");
    assert!(!r.join("submit/Old/real.docx").exists());
}
