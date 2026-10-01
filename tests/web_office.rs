#[path = "integration/mock.rs"]
mod mock;

use mock::{MockMoodle, TOKEN};
use moodle_mcp::{
    moodle::Moodle,
    web::{self, db::Db, office},
};
use serde_json::json;
use std::sync::Arc;

static ENV_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

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
    let _g = ENV_LOCK.lock().await;
    // Fake Document Server serving the edited file.
    let fake = axum::Router::new()
        .route("/cache/f", axum::routing::get(|| async { "edited" }))
        .route(
            "/cache/big",
            axum::routing::get(|| async { vec![0u8; (64 << 20) + 1] }),
        );
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
        std::fs::read_to_string(root.join("C/.a_amin.docx.bak")).unwrap(),
        "mine"
    );
    // oversize download rejected, file untouched
    let big = sign(&json!({"status": 2, "url": format!("{oo}/cache/big")}));
    let r = cb(json!({"token": big})).send().await.unwrap();
    assert!(!r.status().is_success());
    assert_eq!(
        std::fs::read_to_string(root.join("C/a_amin.docx")).unwrap(),
        "edited"
    );
    // expired ?t= (24h passed) but valid body JWT: callback still saves
    std::fs::write(root.join("C/a_amin.docx"), "late").unwrap();
    let old = office::file_token(SECRET, "C/a_amin.docx", web::now() - 200_000);
    let r = c
        .post(format!("{base}/oo/callback?t={old}"))
        .json(&json!({"token": ok}))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    assert_eq!(
        std::fs::read_to_string(root.join("C/a_amin.docx")).unwrap(),
        "edited"
    );
    // ...but /oo/file with the expired token is 403
    let r = c
        .get(format!("{base}/oo/file?t={old}"))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 403);
    std::fs::write(root.join("C/a_amin.docx"), "again").unwrap();
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

    // file moved to submit/_lliurat/ while the editor was open: callback still saves there
    std::fs::create_dir_all(root.join("submit/_lliurat/T")).unwrap();
    std::fs::write(root.join("submit/_lliurat/T/b_amin.docx"), "mine").unwrap();
    let tb = office::file_token(SECRET, "submit/T/b_amin.docx", web::now());
    let r = c
        .post(format!("{base}/oo/callback?t={tb}"))
        .json(&json!({"token": ok}))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    assert_eq!(
        std::fs::read_to_string(root.join("submit/_lliurat/T/b_amin.docx")).unwrap(),
        "edited"
    );
}

#[tokio::test]
async fn office_config_is_post_only_office_only_and_repoints_task_files() {
    let _g = ENV_LOCK.lock().await;
    std::env::set_var("ONLYOFFICE_JWT", SECRET);
    let m = MockMoodle::healthy().await;
    let root = std::env::temp_dir().join(format!("mweb-oocfg-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(root.join("C")).unwrap();
    std::fs::write(root.join("C/a.docx"), "orig").unwrap();
    std::fs::write(root.join("C/n.pdf"), "pdf").unwrap();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let app = Arc::new(web::App::new(
        Db::memory().unwrap(),
        Moodle::new(m.base(), TOKEN),
        root.to_str().unwrap(),
        port,
    ));
    app.db
        .set_local(&web::db::Local {
            task_id: "assign:7".into(),
            files: vec!["C/a.docx".into(), "C/other.pdf".into()],
            ..Default::default()
        })
        .unwrap();
    tokio::spawn(web::serve_on(listener, Arc::clone(&app)));
    let base = format!("http://127.0.0.1:{port}");
    let c = reqwest::Client::new();
    let url = format!("{base}/api/office/config");
    // a GET (e.g. <img src>) must not create files
    assert_eq!(
        c.get(&url)
            .query(&[("path", "C/a.docx")])
            .send()
            .await
            .unwrap()
            .status(),
        405
    );
    assert!(!root.join("C/a_amin.docx").exists());
    let post = |p: &str| {
        c.post(&url)
            .header("origin", &base)
            .header("x-neu", "1")
            .json(&json!({ "path": p }))
    };
    // cross-site style POST without the app headers
    let r = c
        .post(&url)
        .json(&json!({"path": "C/a.docx"}))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 403);
    assert!(!root.join("C/a_amin.docx").exists());
    assert_eq!(post("C/n.pdf").send().await.unwrap().status(), 400);
    assert!(!root.join("C/n_amin.pdf").exists());
    let r = post("C/a.docx").send().await.unwrap();
    assert_eq!(r.status(), 200);
    assert_eq!(
        r.json::<serde_json::Value>().await.unwrap()["rel"],
        "C/a_amin.docx"
    );
    assert_eq!(
        app.db.local("assign:7").unwrap().files,
        vec!["C/a_amin.docx", "C/other.pdf"]
    );
}
