#[path = "integration/ff.rs"]
mod ff;
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
        .upsert_tasks(&[
            Task {
                id: "assign:evil".into(),
                kind: "assign".into(),
                url: "http://evil.example/mod/assign/view.php?id=1".into(),
                ..Default::default()
            },
            Task {
                id: "assign:auto".into(),
                kind: "assign".into(),
                url: format!("{}/mod/assign/view.php?id=1", m.base()),
                ..Default::default()
            },
        ])
        .unwrap();
    tokio::spawn(web::serve_on(listener, Arc::clone(&app)));
    let base = format!("http://127.0.0.1:{port}");
    let c = reqwest::Client::new();
    let shot = |id: &str| {
        let (c, u) = (c.clone(), format!("{base}/api/tasks/{id}"));
        async move {
            c.get(u)
                .send()
                .await
                .unwrap()
                .json::<serde_json::Value>()
                .await
                .unwrap()["shot"]
                .clone()
        }
    };

    // the manual button is gone
    let r = c
        .post(format!("{base}/api/shot/assign:7"))
        .header("origin", &base)
        .header("x-neu", "1")
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 405);
    // nothing yet
    let r = c
        .get(format!("{base}/api/shot/assign:auto"))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 404);

    let log = root.join("log");
    let png = root.join("fake.png");
    std::env::set_var(
        "SHOT_CMD",
        format!("sh {FAKE} {} {}", png.display(), log.display()),
    );
    // foreign host: never spawn
    assert_eq!(shot("assign:evil").await, "none");
    // no Firefox session: never spawn
    let ffroot = root.join("ff");
    std::fs::create_dir_all(&ffroot).unwrap();
    std::env::set_var("MOODLE_FIREFOX_ROOT", &ffroot);
    assert_eq!(shot("assign:auto").await, "none");
    assert!(!log.exists());

    // session: capture starts by itself
    ff::firefox_session(&ffroot);
    assert_eq!(shot("assign:auto").await, "pending");
    let mut ok = false;
    for _ in 0..50 {
        let r = c
            .get(format!("{base}/api/shot/assign:auto"))
            .send()
            .await
            .unwrap();
        if r.status() == 200 {
            assert_eq!(r.headers()["content-type"], "image/png");
            assert!(r.bytes().await.unwrap().starts_with(b"\x89PNG"));
            ok = true;
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
    assert!(ok, "shot not produced within 5s");
    assert_eq!(shot("assign:auto").await, "ready");
}
