// own binary: LOCK is process-global, would starve parallel apple tests
#[path = "integration/mock.rs"]
mod mock;

use mock::{MockConfig, MockMoodle, TOKEN};
use moodle_mcp::{
    moodle::Moodle,
    web::{self, db::Db},
};
use std::sync::Arc;

async fn start_with(
    tag: &str,
    cfg: MockConfig,
) -> (String, Arc<web::App>, MockMoodle, std::path::PathBuf) {
    let m = MockMoodle::start(cfg).await;
    let root = std::env::temp_dir().join(format!("mweb-api-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(root.join("Digitalització aplicada/RA01")).unwrap();
    std::fs::write(
        root.join("Digitalització aplicada/index.md"),
        "---\ntitle: \"Digitalització\"\ncourse_id: 101\n---\n",
    )
    .unwrap();
    std::fs::write(
        root.join("Digitalització aplicada/RA01/Mapa IT·OT.txt"),
        "hola",
    )
    .unwrap();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let app = Arc::new(web::App::new(
        Db::memory().unwrap(),
        Moodle::new(m.base(), TOKEN),
        root.to_str().unwrap(),
        port,
    ));
    tokio::spawn(web::serve_on(listener, Arc::clone(&app)));
    (format!("http://127.0.0.1:{port}"), app, m, root)
}

fn post(c: &reqwest::Client, base: &str, path: &str) -> reqwest::RequestBuilder {
    c.post(format!("{base}{path}"))
        .header("origin", base)
        .header("x-neu", "1")
}

#[tokio::test]
async fn apple_sync_and_reimport_409_while_locked() {
    let (base, _app, _m, root) = start_with("applelock", MockConfig::default()).await;
    let c = reqwest::Client::new();
    let _g = web::apple::LOCK.lock().await;
    for p in ["/api/apple-sync", "/api/apple/reimport"] {
        let r = post(&c, &base, p).send().await.unwrap();
        assert_eq!(r.status(), 409, "{p}");
    }
    let _ = std::fs::remove_dir_all(root);
}
