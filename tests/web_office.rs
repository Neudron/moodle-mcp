#[path = "integration/mock.rs"]
mod mock;

use mock::{MockMoodle, TOKEN};
use moodle_mcp::{
    moodle::Moodle,
    web::{self, db::Db, office},
};
use serde_json::json;
use std::sync::Arc;

const SECRET: &str = "oo-secret";

fn sign(v: &serde_json::Value) -> String {
    jsonwebtoken::encode(
        &jsonwebtoken::Header::default(),
        v,
        &jsonwebtoken::EncodingKey::from_secret(SECRET.as_bytes()),
    )
    .unwrap()
}

#[tokio::test]
async fn callback_saves_with_bak_and_guards_url() {
    // Fake Document Server serving the edited file.
    let fake = axum::Router::new().route("/cache/f", axum::routing::get(|| async { "edited" }));
    let fl = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let oo = format!("http://127.0.0.1:{}", fl.local_addr().unwrap().port());
    tokio::spawn(async move { axum::serve(fl, fake).await.ok() });
    std::env::set_var("ONLYOFFICE_JWT", SECRET);
    std::env::set_var("ONLYOFFICE_URL", &oo);

    let m = MockMoodle::healthy().await;
    let root = std::env::temp_dir().join(format!("mweb-oocb-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(root.join("C")).unwrap();
    std::fs::write(root.join("C/a.docx"), "orig").unwrap();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let app = Arc::new(web::App::new(
        Db::memory().unwrap(),
        Moodle::new(m.base(), TOKEN),
        root.to_str().unwrap(),
        port,
    ));
    tokio::spawn(web::serve_on(listener, app));
    let base = format!("http://127.0.0.1:{port}");
    let c = reqwest::Client::new();
    let t = office::file_token(SECRET, "C/a_amin.docx", web::now());
    std::fs::write(root.join("C/a_amin.docx"), "mine").unwrap();

    // /oo/file: valid token serves bytes, bad token 403.
    let r = c.get(format!("{base}/oo/file?t={t}")).send().await.unwrap();
    assert_eq!(r.text().await.unwrap(), "mine");
    let r = c.get(format!("{base}/oo/file?t=x")).send().await.unwrap();
    assert_eq!(r.status(), 403);

    let cb = |body: serde_json::Value| c.post(format!("{base}/oo/callback?t={t}")).json(&body);
    // unsigned body
    let r = cb(json!({"status": 2, "url": format!("{oo}/cache/f")}))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 403);
    // signed but download url outside ONLYOFFICE_URL (prefix trick)
    let evil = sign(&json!({"status": 2, "url": format!("{oo}.evil.com/cache/f")}));
    let r = cb(json!({"token": evil})).send().await.unwrap();
    assert_eq!(r.status(), 403);
    assert_eq!(
        std::fs::read_to_string(root.join("C/a_amin.docx")).unwrap(),
        "mine"
    );
    // status 1 (editing): no write
    let r = cb(json!({"token": sign(&json!({"status": 1}))}))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    // status 2 flat body token
    let ok = sign(&json!({"status": 2, "url": format!("{oo}/cache/f")}));
    let r = cb(json!({"token": ok})).send().await.unwrap();
    assert_eq!(r.json::<serde_json::Value>().await.unwrap()["error"], 0);
    assert_eq!(
        std::fs::read_to_string(root.join("C/a_amin.docx")).unwrap(),
        "edited"
    );
    assert_eq!(
        std::fs::read_to_string(root.join("C/a_amin.docx.bak")).unwrap(),
        "mine"
    );
    // header token with {"payload": ...} wrapper
    std::fs::write(root.join("C/a_amin.docx"), "again").unwrap();
    let hdr = sign(&json!({"payload": {"status": 6, "url": format!("{oo}/cache/f")}}));
    let r = c
        .post(format!("{base}/oo/callback?t={t}"))
        .header("authorization", format!("Bearer {hdr}"))
        .json(&json!({"status": 6}))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    assert_eq!(
        std::fs::read_to_string(root.join("C/a_amin.docx")).unwrap(),
        "edited"
    );
    assert_eq!(
        std::fs::read_to_string(root.join("C/a.docx")).unwrap(),
        "orig"
    );
}
