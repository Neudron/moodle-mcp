#[path = "integration/mock.rs"]
mod mock;

use mock::{MockMoodle, TOKEN};
use moodle_mcp::{
    moodle::Moodle,
    web::{self, db::Db},
};
use std::sync::Arc;

fn post(c: &reqwest::Client, base: &str, path: &str) -> reqwest::RequestBuilder {
    c.post(format!("{base}{path}"))
        .header("origin", base)
        .header("x-neu", "1")
}

#[tokio::test]
async fn browse_and_pick() {
    let m = MockMoodle::healthy().await;
    let home = std::env::temp_dir().join(format!("mweb-pick-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&home);
    let root = home.join("smx");
    for d in ["Docs", "Docs2", ".ssh", "smx/Curs", "smx/submit"] {
        std::fs::create_dir_all(home.join(d)).unwrap();
    }
    for f in [
        "Docs/a.pdf",
        "Docs2/a.pdf",
        ".ssh/id",
        "smx/Curs/T.docx",
        "smx/Curs/T_amin.docx",
        "smx/submit/x_amin.docx",
    ] {
        std::fs::write(home.join(f), "x").unwrap();
    }
    std::env::set_var("MOODLE_PICK_HOME", &home);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let app = Arc::new(web::App::new(
        Db::memory().unwrap(),
        Moodle::new(m.base(), TOKEN),
        root.to_str().unwrap(),
        port,
    ));
    tokio::spawn(web::serve_on(listener, Arc::clone(&app)));
    let base = format!("http://127.0.0.1:{port}");
    let c = reqwest::Client::new();
    assert_eq!(
        post(&c, &base, "/api/sync").send().await.unwrap().status(),
        202
    );
    for _ in 0..100 {
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        let st: serde_json::Value = c
            .get(format!("{base}/api/status"))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        if st["syncing"] == false {
            break;
        }
    }

    let get = |q: &str| c.get(format!("{base}/api/browse?path={q}")).send();
    let r = get("").await.unwrap();
    assert_eq!(r.status(), 200);
    let names: Vec<String> = r.json::<serde_json::Value>().await.unwrap()["entries"]
        .as_array()
        .unwrap()
        .iter()
        .map(|e| e["name"].as_str().unwrap().to_string())
        .collect();
    assert!(names.contains(&"Docs".into()) && !names.iter().any(|n| n.starts_with('.')));
    assert_eq!(get("..%2F").await.unwrap().status(), 403);
    assert_eq!(get(".ssh").await.unwrap().status(), 403);

    let pick = |p: &str| {
        post(&c, &base, "/api/tasks/assign:7/pick")
            .json(&serde_json::json!({ "path": p }))
            .send()
    };
    let r = pick("Docs/a.pdf").await.unwrap();
    assert_eq!(r.status(), 200);
    let files = r.json::<serde_json::Value>().await.unwrap()["files"].clone();
    let rel = files[0].as_str().unwrap().to_string();
    assert!(rel.starts_with("submit/") && rel.ends_with("/a.pdf"));
    let ln = root.join(&rel);
    assert!(std::fs::symlink_metadata(&ln).unwrap().is_symlink());
    assert_eq!(
        ln.canonicalize().unwrap(),
        home.join("Docs/a.pdf").canonicalize().unwrap()
    );
    let r = c
        .get(format!("{base}/api/files?path={rel}"))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    assert_eq!(r.json::<serde_json::Value>().await.unwrap()["kind"], "file");

    let files = pick("Docs/a.pdf")
        .await
        .unwrap()
        .json::<serde_json::Value>()
        .await
        .unwrap()["files"]
        .clone();
    assert_eq!(files.as_array().unwrap().len(), 1);
    // A submit/ symlink is linked as given, not followed out of submit/ into a second link.
    let files = pick(&format!("smx/{rel}"))
        .await
        .unwrap()
        .json::<serde_json::Value>()
        .await
        .unwrap()["files"]
        .clone();
    assert_eq!(files, serde_json::json!([rel]));
    let files = pick("Docs2/a.pdf")
        .await
        .unwrap()
        .json::<serde_json::Value>()
        .await
        .unwrap()["files"]
        .clone();
    assert!(files[1].as_str().unwrap().ends_with("/a-2.pdf"));

    assert_eq!(pick("smx/Curs/T.docx").await.unwrap().status(), 400);
    assert_eq!(pick("smx/Curs/T_amin.docx").await.unwrap().status(), 200);
    let files = pick("smx/submit/x_amin.docx")
        .await
        .unwrap()
        .json::<serde_json::Value>()
        .await
        .unwrap()["files"]
        .clone();
    assert!(files
        .as_array()
        .unwrap()
        .iter()
        .any(|f| f == "submit/x_amin.docx"));

    // hand-made submit/ symlink to a teacher original must not upload
    let d = root.join("submit/M/T");
    std::fs::create_dir_all(&d).unwrap();
    std::os::unix::fs::symlink(root.join("Curs/T.docx"), d.join("x.docx")).unwrap();
    let r = post(&c, &base, "/api/tasks/assign:7/upload")
        .json(&serde_json::json!({ "files": ["submit/M/T/x.docx"] }))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 400);
}
