#[path = "integration/mock.rs"]
mod mock;

use mock::{MockMoodle, TOKEN};
use moodle_mcp::{
    moodle::Moodle,
    web::{self, db::Db},
};
use std::sync::Arc;

async fn start(tag: &str) -> (String, Arc<web::App>, MockMoodle, std::path::PathBuf) {
    let m = MockMoodle::healthy().await;
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
async fn guard_blocks_bad_requests() {
    let (base, ..) = start("guard").await;
    let c = reqwest::Client::new();
    let r = c
        .get(format!("{base}/api/tasks"))
        .header("host", "evil.com")
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 403);
    let r = c
        .post(format!("{base}/api/sync"))
        .header("origin", &base)
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 403);
}

#[tokio::test]
async fn sync_then_tasks_and_status() {
    let (base, _app, _m, _root) = start("sync").await;
    let c = reqwest::Client::new();
    let r = post(&c, &base, "/api/sync").send().await.unwrap();
    assert!(r.status().is_success(), "{}", r.text().await.unwrap());
    let tasks: serde_json::Value = c
        .get(format!("{base}/api/tasks"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert!(tasks
        .as_array()
        .unwrap()
        .iter()
        .any(|t| t["task"]["id"] == "assign:7"));
    let one: serde_json::Value = c
        .get(format!("{base}/api/tasks/assign:7"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert!(one["task"]["body_html"].as_str().unwrap().contains("això"));
    let r = post(&c, &base, "/api/tasks/assign:7/status")
        .json(&serde_json::json!({"status": "revisat OK"}))
        .send()
        .await
        .unwrap();
    assert!(r.status().is_success());
    let bad = post(&c, &base, "/api/tasks/assign:7/status")
        .json(&serde_json::json!({"status": "hack"}))
        .send()
        .await
        .unwrap();
    assert_eq!(bad.status(), 400);
    let st: serde_json::Value = c
        .get(format!("{base}/api/status"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert!(st["last_sync"].as_i64().unwrap() > 0);
    assert_eq!(st["error"], "");
}

#[tokio::test]
async fn raw_serves_accented_path_and_refuses_dotfiles() {
    let (base, _app, _m, root) = start("raw").await;
    std::fs::write(root.join(".env"), "X=1").unwrap();
    let c = reqwest::Client::new();
    let url = format!(
        "{base}/api/raw?path={}",
        moodle_mcp::web::tasks::pct("Digitalització aplicada/RA01/Mapa IT·OT.txt")
    );
    let r = c.get(url).send().await.unwrap();
    assert_eq!(r.status(), 200);
    assert_eq!(r.text().await.unwrap(), "hola");
    let r = c
        .get(format!("{base}/api/raw?path=.env"))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 403);
    let courses: serde_json::Value = c
        .get(format!("{base}/api/courses"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(courses[0]["course_id"], 101);
}

#[tokio::test]
async fn moodle_file_is_sandboxed_and_ignores_fragment_ext() {
    let (base, _app, m, _root) = start("mfile").await;
    let c = reqwest::Client::new();
    let f = format!(
        "{}/webservice/pluginfile.php/101/mod_resource/content/6/s1-m3-0.txt",
        m.base()
    );
    for u in [f.clone(), format!("{f}#.html")] {
        let r = c
            .get(format!(
                "{base}/api/moodle-file?url={}",
                moodle_mcp::web::tasks::pct(&u)
            ))
            .send()
            .await
            .unwrap();
        assert_eq!(r.status(), 200, "{u}");
        assert_eq!(r.headers()["content-security-policy"], "sandbox");
        assert!(
            r.headers()["content-type"]
                .to_str()
                .unwrap()
                .starts_with("text/plain"),
            "{u}"
        );
    }
}
