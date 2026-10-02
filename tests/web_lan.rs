#[path = "integration/mock.rs"]
mod mock;

use mock::{MockMoodle, TOKEN};
use moodle_mcp::{
    moodle::Moodle,
    web::{self, db::Db},
};
use serde_json::{json, Value};
use std::sync::Arc;

#[tokio::test]
async fn lan_listener_marks_open_as_remote() {
    let m = MockMoodle::healthy().await;
    let root = std::env::temp_dir().join(format!("mweb-lan-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(root.join(".moodle")).unwrap();
    std::fs::write(root.join("a.txt"), "x").unwrap();
    let port = std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port();
    let app = Arc::new(web::App::new(
        Db::memory().unwrap(),
        Moodle::new(m.base(), TOKEN),
        root.to_str().unwrap(),
        port,
    ));
    web::lan::spawn(app, "127.0.0.1", "127.0.0.1", &root).await;
    let tok = web::lan::token(&root).unwrap();
    let r = reqwest::Client::new()
        .post(format!("http://127.0.0.1:{port}/api/open"))
        .header("cookie", format!("neu_lan={tok}"))
        .header("origin", format!("http://127.0.0.1:{port}"))
        .header("x-neu", "1")
        .json(&json!({"path": "a.txt"}))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    let j: Value = r.json().await.unwrap();
    assert_eq!(j["remote"], true);
    assert!(j["rel"].is_string());
}
