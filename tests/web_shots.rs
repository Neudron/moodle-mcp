#[path = "integration/mock.rs"]
mod mock;

use mock::{MockMoodle, TOKEN};
use moodle_mcp::{
    moodle::Moodle,
    web::{
        self,
        db::{Db, Task},
        tasks::sync_all,
    },
};
use std::sync::Arc;

const FAKE: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/fake_mcp.sh");

#[tokio::test]
async fn shot_routes() {
    let m = MockMoodle::healthy().await;
    let root = std::env::temp_dir().join(format!("mweb-shots-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&root).unwrap();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let app = Arc::new(web::App::new(
        Db::memory().unwrap(),
        Moodle::new(m.base(), TOKEN),
        root.to_str().unwrap(),
        port,
    ));
    sync_all(&app.moodle, &app.db, &app.root, false)
        .await
        .unwrap();
    app.db
        .upsert_tasks(&[Task {
            id: "assign:evil".into(),
            kind: "assign".into(),
            url: "http://evil.example/mod/assign/view.php?id=1".into(),
            ..Default::default()
        }])
        .unwrap();
    tokio::spawn(web::serve_on(listener, Arc::clone(&app)));
    let base = format!("http://127.0.0.1:{port}");
    let c = reqwest::Client::new();
    let post = |p: &str| {
        c.post(format!("{base}{p}"))
            .header("origin", &base)
            .header("x-neu", "1")
    };

    // no shot yet
    let r = c
        .get(format!("{base}/api/shot/assign:7"))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 404);

    // unknown task, foreign host: never spawn
    let log = root.join("log");
    let png = root.join("fake.png");
    std::env::set_var(
        "SHOT_CMD",
        format!("sh {FAKE} {} {}", png.display(), log.display()),
    );
    assert_eq!(
        post("/api/shot/assign:nope").send().await.unwrap().status(),
        404
    );
    assert_eq!(
        post("/api/shot/assign:evil").send().await.unwrap().status(),
        400
    );
    assert!(!log.exists(), "foreign URL must not reach the browser");

    // POST needs the mutation headers
    let r = c
        .post(format!("{base}/api/shot/assign:7"))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 403);

    // happy path
    let r = post("/api/shot/assign:7").send().await.unwrap();
    assert_eq!(r.status(), 200);
    assert!(root.join(".moodle/shots/assign_7.png").exists());
    let r = c
        .get(format!("{base}/api/shot/assign:7"))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    assert_eq!(r.headers()["content-type"], "image/png");
    assert!(r.bytes().await.unwrap().starts_with(b"\x89PNG"));

    // broken command -> 502, previous shot still served
    std::env::set_var("SHOT_CMD", "/nonexistent/mcp-bin");
    assert_eq!(
        post("/api/shot/assign:7").send().await.unwrap().status(),
        502
    );
    let r = c
        .get(format!("{base}/api/shot/assign:7"))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
}
