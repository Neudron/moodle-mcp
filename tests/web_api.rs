#[path = "integration/mock.rs"]
mod mock;

use mock::{MockConfig, MockMoodle, TOKEN};
use moodle_mcp::{
    moodle::Moodle,
    web::{self, db::Db},
};
use std::sync::Arc;

async fn start(tag: &str) -> (String, Arc<web::App>, MockMoodle, std::path::PathBuf) {
    start_with(tag, MockConfig::default()).await
}

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
async fn moodle_file_sandboxes_only_html_svg_and_ignores_fragment_ext() {
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
        // text/plain (like pdf) must not be sandboxed; only html/svg are.
        assert!(
            r.headers()
                .get_all("content-security-policy")
                .iter()
                .all(|v| v != "sandbox"),
            "{u}"
        );
        assert!(
            r.headers()["content-type"]
                .to_str()
                .unwrap()
                .starts_with("text/plain"),
            "{u}"
        );
    }
}

#[tokio::test]
async fn lliurat_moves_folder_under_module_and_rewrites_files() {
    let (base, _app, _m, root) = start("lliurat").await;
    let c = reqwest::Client::new();
    post(&c, &base, "/api/sync").send().await.unwrap();
    let one: serde_json::Value = c
        .get(format!("{base}/api/tasks/assign:7"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let dir = one["submit_dir"].as_str().unwrap().to_string();
    std::fs::create_dir_all(root.join(&dir)).unwrap();
    std::fs::write(root.join(&dir).join("a.txt"), "x").unwrap();
    let file = format!("{dir}/a.txt");
    let r = post(&c, &base, "/api/tasks/assign:7/files")
        .json(&serde_json::json!({"files": [file]}))
        .send()
        .await
        .unwrap();
    assert!(r.status().is_success());
    let r = post(&c, &base, "/api/tasks/assign:7/status")
        .json(&serde_json::json!({"status": "lliurat"}))
        .send()
        .await
        .unwrap();
    assert!(r.status().is_success());
    let to = dir.replacen("submit/", "submit/_lliurat/", 1);
    assert!(!root.join(&dir).exists());
    assert!(root.join(&to).join("a.txt").exists());
    let one: serde_json::Value = c
        .get(format!("{base}/api/tasks/assign:7"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(one["submit_dir"], to);
    assert_eq!(one["local"]["files"][0], format!("{to}/a.txt"));
    assert!(root.join("submit/README.md").exists());
}

#[test]
fn small_tag_font_size_is_16px_multiple() {
    let css = include_str!("../web/app.css");
    assert!(css.contains("small{font-size:16px}"));
}

#[tokio::test]
async fn app_shell_cannot_be_framed_cross_site() {
    let (base, ..) = start("frame").await;
    let r = reqwest::get(format!("{base}/")).await.unwrap();
    assert_eq!(r.headers().get("x-frame-options").unwrap(), "SAMEORIGIN");
    let csp: Vec<_> = r
        .headers()
        .get_all("content-security-policy")
        .iter()
        .map(|v| v.to_str().unwrap().to_string())
        .collect();
    assert!(
        csp.iter().any(|v| v.contains("frame-ancestors 'self'")),
        "{csp:?}"
    );
    let r = reqwest::get(format!("{base}/api/status")).await.unwrap();
    assert_eq!(r.headers().get("x-frame-options").unwrap(), "SAMEORIGIN");
}

#[tokio::test]
async fn static_assets_are_not_cached() {
    let (base, ..) = start("cache").await;
    for p in ["/", "/app.js", "/app.css"] {
        let r = reqwest::get(format!("{base}{p}")).await.unwrap();
        assert_eq!(r.headers().get("cache-control").unwrap(), "no-store", "{p}");
    }
}

#[tokio::test]
async fn upload_route_saves_draft_and_rejects_bad_input() {
    let (base, app, _m, root) = start("upload").await;
    let c = reqwest::Client::new();
    post(&c, &base, "/api/sync").send().await.unwrap();
    std::fs::create_dir_all(root.join("submit/T")).unwrap();
    std::fs::write(root.join("submit/T/a_amin.txt"), "x").unwrap();
    let up = |id: &str, files: serde_json::Value| {
        post(&c, &base, &format!("/api/tasks/{id}/upload"))
            .json(&serde_json::json!({ "files": files }))
    };
    let r = up("assign:7", serde_json::json!(["submit/T/a_amin.txt"]))
        .send()
        .await
        .unwrap();
    assert!(r.status().is_success());
    assert_eq!(
        r.json::<serde_json::Value>().await.unwrap()["status"],
        "draft"
    );
    assert_eq!(
        app.db.task("assign:7").unwrap().unwrap().moodle_status,
        "draft"
    );
    for bad in [".env", "../x", "nope.txt"] {
        let r = up("assign:7", serde_json::json!([bad]))
            .send()
            .await
            .unwrap();
        assert_eq!(r.status(), 403, "{bad}");
    }
    let r = up("quiz:5", serde_json::json!([])).send().await.unwrap();
    assert_eq!(r.status(), 400);
    let r = post(&c, &base, "/api/tasks/assign:7/submit")
        .send()
        .await
        .unwrap();
    assert!(r.status().is_success());
}

#[tokio::test]
async fn moodle_warnings_become_errors() {
    for f in [
        "mod_assign_save_submission",
        "mod_assign_submit_for_grading",
    ] {
        let cfg = MockConfig {
            warn_fn: Some(f),
            ..Default::default()
        };
        let (base, _app, _m, root) = start_with(&format!("warn-{f}"), cfg).await;
        let c = reqwest::Client::new();
        post(&c, &base, "/api/sync").send().await.unwrap();
        std::fs::create_dir_all(root.join("submit/T")).unwrap();
        std::fs::write(root.join("submit/T/a_amin.txt"), "x").unwrap();
        let (path, body) = if f.ends_with("save_submission") {
            (
                "upload",
                serde_json::json!({"files": ["submit/T/a_amin.txt"]}),
            )
        } else {
            ("submit", serde_json::json!({}))
        };
        let r = post(&c, &base, &format!("/api/tasks/assign:7/{path}"))
            .json(&body)
            .send()
            .await
            .unwrap();
        assert_eq!(r.status(), 502, "{f}");
        assert!(r.text().await.unwrap().contains("duedatereached"), "{f}");
    }
}

#[tokio::test]
async fn submit_moves_folder_to_lliurat_and_readme_follows() {
    let cfg = MockConfig {
        status: Some("submitted"),
        ..Default::default()
    };
    let (base, app, _m, root) = start_with("submit", cfg).await;
    let c = reqwest::Client::new();
    post(&c, &base, "/api/sync").send().await.unwrap();
    let one: serde_json::Value = c
        .get(format!("{base}/api/tasks/assign:7"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let dir = one["submit_dir"].as_str().unwrap().to_string();
    std::fs::create_dir_all(root.join(&dir)).unwrap();
    std::fs::write(root.join(&dir).join("a.txt"), "x").unwrap();
    let r = post(&c, &base, "/api/tasks/assign:7/files")
        .json(&serde_json::json!({"files": [format!("{dir}/a.txt")]}))
        .send()
        .await
        .unwrap();
    assert!(r.status().is_success());
    let r = post(&c, &base, "/api/tasks/assign:7/submit")
        .send()
        .await
        .unwrap();
    assert!(r.status().is_success());
    let to = dir.replacen("submit/", "submit/_lliurat/", 1);
    assert!(!root.join(&dir).exists());
    assert!(root.join(&to).join("a.txt").exists());
    let l = app.db.local("assign:7").unwrap();
    assert_eq!(l.status, "lliurat");
    assert_eq!(l.files[0], format!("{to}/a.txt"));
    let readme = std::fs::read_to_string(root.join("submit/README.md")).unwrap();
    assert!(readme.contains(&format!("{to}/a.txt")) || readme.contains("_lliurat"));
}

async fn seed_task_dir(c: &reqwest::Client, base: &str) -> String {
    post(c, base, "/api/sync").send().await.unwrap();
    let one: serde_json::Value = c
        .get(format!("{base}/api/tasks/assign:7"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    one["submit_dir"].as_str().unwrap().to_string()
}

#[tokio::test]
async fn upload_refuses_teacher_originals_outside_submit() {
    let (base, _app, _m, root) = start("orig").await;
    let c = reqwest::Client::new();
    post(&c, &base, "/api/sync").send().await.unwrap();
    std::fs::create_dir_all(root.join("submit/T")).unwrap();
    std::fs::write(root.join("submit/T/plain.txt"), "x").unwrap();
    let up = |f: &str| {
        post(&c, &base, "/api/tasks/assign:7/upload").json(&serde_json::json!({ "files": [f] }))
    };
    let f = "Digitalització aplicada/RA01/Mapa IT·OT.txt";
    let r = up(f).send().await.unwrap();
    assert_eq!(r.status(), 400);
    assert!(r.text().await.unwrap().contains("_amin"));
    // ".." can not smuggle an original under a submit/ prefix
    let r = up(&format!("submit/../{f}")).send().await.unwrap();
    assert!(r.status().is_client_error());
    // anything under submit/ is fine
    assert!(up("submit/T/plain.txt")
        .send()
        .await
        .unwrap()
        .status()
        .is_success());
}

#[tokio::test]
async fn upload_of_directly_submitted_assignment_marks_lliurat() {
    let cfg = MockConfig {
        status: Some("submitted"),
        ..Default::default()
    };
    let (base, app, _m, root) = start_with("updirect", cfg).await;
    let c = reqwest::Client::new();
    let dir = seed_task_dir(&c, &base).await;
    std::fs::create_dir_all(root.join(&dir)).unwrap();
    std::fs::write(root.join(&dir).join("a_amin.txt"), "x").unwrap();
    let f = format!("{dir}/a_amin.txt");
    post(&c, &base, "/api/tasks/assign:7/files")
        .json(&serde_json::json!({ "files": [&f] }))
        .send()
        .await
        .unwrap();
    let r = post(&c, &base, "/api/tasks/assign:7/upload")
        .json(&serde_json::json!({ "files": [&f] }))
        .send()
        .await
        .unwrap();
    assert!(r.status().is_success());
    let l = app.db.local("assign:7").unwrap();
    assert_eq!(l.status, "lliurat");
    let to = dir.replacen("submit/", "submit/_lliurat/", 1);
    assert_eq!(l.files[0], format!("{to}/a_amin.txt"));
    assert!(root.join(&to).join("a_amin.txt").exists());
}

#[tokio::test]
async fn submit_move_failure_is_a_warning_not_a_500() {
    let cfg = MockConfig {
        status: Some("submitted"),
        ..Default::default()
    };
    let (base, app, _m, root) = start_with("movefail", cfg).await;
    let c = reqwest::Client::new();
    let dir = seed_task_dir(&c, &base).await;
    let to = dir.replacen("submit/", "submit/_lliurat/", 1);
    std::fs::create_dir_all(root.join(&dir)).unwrap();
    std::fs::write(root.join(&dir).join("a.txt"), "x").unwrap();
    // Non-empty destination makes the rename fail.
    std::fs::create_dir_all(root.join(&to)).unwrap();
    std::fs::write(root.join(&to).join("old.txt"), "y").unwrap();
    let r = post(&c, &base, "/api/tasks/assign:7/submit")
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    let v: serde_json::Value = r.json().await.unwrap();
    assert_eq!(v["status"], "submitted");
    assert!(v["warning"].as_str().unwrap().contains("carpeta"));
    assert_eq!(app.db.local("assign:7").unwrap().status, "lliurat");
}
