#[path = "integration/mock.rs"]
mod mock;

use mock::{MockConfig, MockMoodle, MockOo, TOKEN};
use moodle_mcp::{
    moodle::Moodle,
    web::{self, db::Db},
};
use std::sync::Arc;

static ENV_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

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

/// POST /api/sync (202), then wait until the background run has finished.
async fn sync_wait(c: &reqwest::Client, base: &str) -> serde_json::Value {
    let r = post(c, base, "/api/sync").send().await.unwrap();
    assert_eq!(r.status(), 202, "{}", r.text().await.unwrap());
    for _ in 0..600 {
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
            return st;
        }
    }
    panic!(
        "sync did not finish: {}",
        c.get(format!("{base}/api/status"))
            .send()
            .await
            .unwrap()
            .text()
            .await
            .unwrap()
    );
}

#[tokio::test]
async fn sync_is_background_and_409_while_running() {
    let (base, app, _m, _root) = start("syncbg").await;
    let c = reqwest::Client::new();
    {
        let _g = app.syncing.lock().await;
        let r = post(&c, &base, "/api/sync").send().await.unwrap();
        assert_eq!(r.status(), 409);
    }
    let st = sync_wait(&c, &base).await;
    assert!(st["result"]["files_new"].is_number(), "{st}");
    assert!(st["progress"].is_null());
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
async fn guard_blocks_cross_site_and_sets_corp() {
    let (base, ..) = start("guardxs").await;
    let c = reqwest::Client::new();
    let get = |v: Option<&'static str>| {
        let r = c.get(format!("{base}/api/raw"));
        match v {
            Some(v) => r.header("sec-fetch-site", v),
            None => r,
        }
    };
    let r = get(Some("cross-site")).send().await.unwrap();
    assert_eq!(r.status(), 403);
    for v in [Some("same-origin"), Some("none"), None] {
        let r = get(v).send().await.unwrap();
        assert_ne!(r.status(), 403, "{v:?}");
        assert_eq!(
            r.headers()["cross-origin-resource-policy"],
            "same-origin",
            "{v:?}"
        );
    }
    let r = c
        .post(format!("{base}/oo/callback"))
        .header("sec-fetch-site", "cross-site")
        .send()
        .await
        .unwrap();
    assert_ne!(r.status(), 403, "/oo exempt");
}

#[tokio::test]
async fn old_open_task_is_stale() {
    let (base, app, _m, _root) = start("stale").await;
    let c = reqwest::Client::new();
    sync_wait(&c, &base).await;
    app.db.update_task("assign:7", |t| t.due = 86400).unwrap();
    let tasks: serde_json::Value = c
        .get(format!("{base}/api/tasks"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let t = tasks
        .as_array()
        .unwrap()
        .iter()
        .find(|t| t["task"]["id"] == "assign:7")
        .unwrap();
    assert_eq!(t["stale"], true);
}

#[tokio::test]
async fn sync_then_tasks_and_status() {
    let (base, _app, _m, _root) = start("sync").await;
    let c = reqwest::Client::new();
    sync_wait(&c, &base).await;
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
async fn courses_list_teachers_after_sync() {
    let (base, _app, _m, _root) = start("teach").await;
    let c = reqwest::Client::new();
    sync_wait(&c, &base).await;
    let v: serde_json::Value = c
        .get(format!("{base}/api/courses"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let m = v
        .as_array()
        .unwrap()
        .iter()
        .find(|x| x["shortname"] == "MOCK101")
        .unwrap();
    assert_eq!(m["teachers"][0], "Anna Prof");
    assert!(m["dir"].is_string());
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
async fn courses_use_frontmatter_shortname_for_renamed_dir() {
    let (base, _app, _m, root) = start("shortdir").await;
    std::fs::create_dir_all(root.join("bbdd/RA01")).unwrap();
    std::fs::write(
        root.join("bbdd/index.md"),
        "---\nshortname: \"BBDD\"\n---\n",
    )
    .unwrap();
    let c = reqwest::Client::new();
    let v: serde_json::Value = c
        .get(format!("{base}/api/courses"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert!(v
        .as_array()
        .unwrap()
        .iter()
        .any(|x| x["shortname"] == "BBDD"));
    let v: serde_json::Value = c
        .get(format!("{base}/api/courses/BBDD"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert!(v["sections"]
        .as_array()
        .unwrap()
        .iter()
        .any(|s| s["name"] == "RA01"));
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
    sync_wait(&c, &base).await;
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

#[tokio::test]
async fn undo_lliurat_moves_folder_back_and_refuses_nonempty_target() {
    let (base, _app, _m, root) = start("unlliurat").await;
    let c = reqwest::Client::new();
    sync_wait(&c, &base).await;
    let get = |c: &reqwest::Client| c.get(format!("{base}/api/tasks/assign:7")).send();
    let one: serde_json::Value = get(&c).await.unwrap().json().await.unwrap();
    let dir = one["submit_dir"].as_str().unwrap().to_string();
    std::fs::create_dir_all(root.join(&dir)).unwrap();
    std::fs::write(root.join(&dir).join("a.txt"), "x").unwrap();
    let file = format!("{dir}/a.txt");
    post(&c, &base, "/api/tasks/assign:7/files")
        .json(&serde_json::json!({"files": [file]}))
        .send()
        .await
        .unwrap();
    let st = |s: &'static str| {
        post(&c, &base, "/api/tasks/assign:7/status").json(&serde_json::json!({"status": s}))
    };
    assert!(st("lliurat").send().await.unwrap().status().is_success());
    // target now exists and is non-empty: clean 4xx, status unchanged
    std::fs::create_dir_all(root.join(&dir)).unwrap();
    std::fs::write(root.join(&dir).join("b.txt"), "y").unwrap();
    let r = st("per revisar").send().await.unwrap();
    assert!(r.status().is_client_error(), "{}", r.status());
    let one: serde_json::Value = get(&c).await.unwrap().json().await.unwrap();
    assert_eq!(one["local"]["status"], "lliurat");
    std::fs::remove_dir_all(root.join(&dir)).unwrap();
    assert!(st("per revisar")
        .send()
        .await
        .unwrap()
        .status()
        .is_success());
    assert!(root.join(&dir).join("a.txt").exists());
    let one: serde_json::Value = get(&c).await.unwrap().json().await.unwrap();
    assert_eq!(one["submit_dir"], dir);
    assert!(root.join(one["submit_dir"].as_str().unwrap()).is_dir());
    assert_eq!(one["local"]["files"][0], format!("{dir}/a.txt"));
}

#[test]
fn small_tag_font_size_is_13px() {
    let css = include_str!("../web/app.css");
    assert!(css.contains("small{font-size:13px}"));
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
async fn static_assets_use_etag_and_304() {
    let (base, ..) = start("cache").await;
    let c = reqwest::Client::new();
    for p in ["/", "/app.js", "/app.css"] {
        let r = c.get(format!("{base}{p}")).send().await.unwrap();
        assert_eq!(r.headers().get("cache-control").unwrap(), "no-cache", "{p}");
        let et = r
            .headers()
            .get("etag")
            .unwrap()
            .to_str()
            .unwrap()
            .to_string();
        let r = c
            .get(format!("{base}{p}"))
            .header("if-none-match", &et)
            .send()
            .await
            .unwrap();
        assert_eq!(r.status(), 304, "{p}");
        assert_eq!(r.headers().get("etag").unwrap(), et.as_str());
    }
}

#[tokio::test]
async fn tasks_etag_changes_on_write_and_304s_otherwise() {
    let (base, app, ..) = start("tetag").await;
    let c = reqwest::Client::new();
    sync_wait(&c, &base).await;
    let r = c.get(format!("{base}/api/tasks")).send().await.unwrap();
    let et = r
        .headers()
        .get("etag")
        .unwrap()
        .to_str()
        .unwrap()
        .to_string();
    let ask = |et: String| {
        c.get(format!("{base}/api/tasks"))
            .header("if-none-match", et)
            .send()
    };
    assert_eq!(ask(et.clone()).await.unwrap().status(), 304);
    let l = web::db::Local {
        task_id: "assign:7".into(),
        status: "revisat OK".into(),
        ..Default::default()
    };
    app.db.set_local(&l).unwrap();
    let r = ask(et.clone()).await.unwrap();
    assert_eq!(r.status(), 200);
    assert_ne!(r.headers().get("etag").unwrap().to_str().unwrap(), et);
}

#[tokio::test]
async fn ai_task_list_has_evidence_but_no_body_and_detail_is_full() {
    let (base, app, ..) = start("evid").await;
    let t = web::db::Task {
        id: "ai:abc:0".into(),
        kind: "ai".into(),
        source: "ai".into(),
        title: "x".into(),
        body_html: "<p>d</p><blockquote>cal fer &lt;b&gt; &amp; \"més\"</blockquote>".into(),
        ..Default::default()
    };
    app.db.upsert_tasks(&[t]).unwrap();
    let c = reqwest::Client::new();
    let l: serde_json::Value = c
        .get(format!("{base}/api/tasks"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(l[0]["task"]["body_html"], "");
    assert_eq!(l[0]["task"]["evidence"], "cal fer <b> & \"més\"");
    let d: serde_json::Value = c
        .get(format!("{base}/api/tasks/ai:abc:0"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert!(d["task"]["body_html"]
        .as_str()
        .unwrap()
        .contains("<p>d</p>"));
}

#[tokio::test]
async fn upload_route_saves_draft_and_rejects_bad_input() {
    let (base, app, _m, root) = start("upload").await;
    let c = reqwest::Client::new();
    sync_wait(&c, &base).await;
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
        sync_wait(&c, &base).await;
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
    sync_wait(&c, &base).await;
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
    sync_wait(c, base).await;
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
    sync_wait(&c, &base).await;
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

#[tokio::test]
async fn apple_auth_failure_is_409_and_visible_in_status() {
    let _g = ENV_LOCK.lock().await;
    use std::os::unix::fs::PermissionsExt;
    let (base, app, _m, root) = start("apple409").await;
    let sh = root.join("fake.sh");
    std::fs::write(
        &sh,
        "#!/bin/sh\ncase $1 in todo-*) echo '{\"error\":\"caducada\",\"kind\":\"auth\"}'; exit 2;; esac\necho '{\"uid\":\"U\"}'\n",
    )
    .unwrap();
    std::fs::set_permissions(&sh, std::fs::Permissions::from_mode(0o755)).unwrap();
    std::env::set_var("ICLOUD_GLASS_CMD", &sh);
    std::env::set_var("ICLOUD_GLASS_STATE", root.join("state"));
    app.db
        .upsert_tasks(&[web::db::Task {
            id: "assign:1".into(),
            title: "T".into(),
            due: web::now() + 86400,
            ..Default::default()
        }])
        .unwrap();
    let c = reqwest::Client::new();
    let r = post(&c, &base, "/api/apple-sync").send().await.unwrap();
    assert_eq!(r.status(), 409);
    let v: serde_json::Value = r.json().await.unwrap();
    assert_eq!(
        (v["kind"].as_str(), v["error"].as_str()),
        (Some("auth"), Some("caducada"))
    );
    assert_eq!(v["created"], 1, "old Summary fields kept");
    let st: serde_json::Value = c
        .get(format!("{base}/api/status"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(st["apple"]["state"], "auth");
    assert_eq!(st["errors"][0]["kind"], "apple-auth");
    // /api/apple/status + /api/apple/reimport (1 per minute)
    let st: serde_json::Value = c
        .get(format!("{base}/api/apple/status"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(
        (st["reminders"].as_str(), st["calendar"].as_str()),
        (Some("auth"), Some("ok"))
    );
    let imp = root.join("imp.sh");
    std::fs::write(
        &imp,
        "#!/bin/sh\necho '{\"ok\":true,\"token_expiry\":42}'\n",
    )
    .unwrap();
    std::fs::set_permissions(&imp, std::fs::Permissions::from_mode(0o755)).unwrap();
    std::env::set_var("ICLOUD_IMPORT_CMD", &imp);
    app.db.kv_set("apple_import_at", "0").unwrap();
    let r = post(&c, &base, "/api/apple/reimport").send().await.unwrap();
    assert_eq!(r.status(), 200);
    let v: serde_json::Value = r.json().await.unwrap();
    assert_eq!(v["token_expiry"], 42);
    let r = post(&c, &base, "/api/apple/reimport").send().await.unwrap();
    assert_eq!(r.status(), 429);
    let v: serde_json::Value = r.json().await.unwrap();
    assert!(v["next_try"].as_i64().unwrap() > 0);
    let r = c
        .post(format!("{base}/api/apple/reimport"))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 403, "guard applies");
    // pruned to 200 rows
    for i in 0..250 {
        app.db.log_error("t", "k", &i.to_string()).unwrap();
    }
    assert_eq!(app.db.recent_errors(500).unwrap().len(), 200);
}

#[tokio::test]
async fn auth_status_states_and_renew_rate_limit() {
    let _g = ENV_LOCK.lock().await;
    let (base, app, m, root) = start("authst").await;
    let c = reqwest::Client::new();
    let get = |p: &'static str| {
        let (c, base) = (c.clone(), base.clone());
        async move {
            c.get(format!("{base}{p}"))
                .send()
                .await
                .unwrap()
                .json::<serde_json::Value>()
                .await
                .unwrap()
        }
    };
    let v = get("/api/auth/status").await;
    assert_eq!(v["moodle"], "ok");
    assert_eq!(v["apple"], "ok");
    // bad token -> expired (probe cached 60 s, so clear the stamp)
    app.moodle.set_token("badbadbadbadbadbadbad");
    app.db.kv_set("auth_probe_at", "0").unwrap();
    assert_eq!(get("/api/auth/status").await["moodle"], "expired");
    // renewal outcome recorded earlier is what the UI shows
    app.db.kv_set("auth_state", "no-session").unwrap();
    app.db.kv_set("auth_probe_at", "0").unwrap();
    assert_eq!(get("/api/auth/status").await["moodle"], "no-session");
    app.db.kv_set("apple_state", "auth").unwrap();
    app.db.kv_set("last_error", "x").unwrap();
    let v = get("/api/auth/status").await;
    assert_eq!(
        (v["apple"].as_str(), v["last_error"].as_str()),
        (Some("auth"), Some("x"))
    );
    assert!(v["at"].as_i64().unwrap() > 0);
    // no Moodle error: last_error falls back to the Apple one
    app.db.kv_set("last_error", "").unwrap();
    app.db.kv_set("apple_error", "boom").unwrap();
    assert_eq!(get("/api/auth/status").await["last_error"], "boom");
    // manual renew: no Firefox session -> no-session; immediate repeat -> 429
    let ff = root.join("ff");
    std::fs::create_dir_all(&ff).unwrap();
    std::env::set_var("MOODLE_FIREFOX_ROOT", &ff);
    app.db.kv_set("apple_state", "ok").unwrap(); // no Apple reimport here
    let r = post(&c, &base, "/api/auth/renew").send().await.unwrap();
    assert_eq!(r.status(), 200);
    let v: serde_json::Value = r.json().await.unwrap();
    assert_eq!(v["moodle"], "no-session");
    assert_eq!(v["apple"], "ok");
    let r = post(&c, &base, "/api/auth/renew").send().await.unwrap();
    assert_eq!(r.status(), 429);
    assert!(
        r.json::<serde_json::Value>().await.unwrap()["next_try"]
            .as_i64()
            .unwrap()
            > 0
    );
    drop(m);
}

#[tokio::test]
async fn raw_supports_range_and_caps_large_files() {
    let (base, _app, _m, root) = start("range").await;
    std::fs::write(root.join("r.bin"), (0..=255u8).collect::<Vec<_>>()).unwrap();
    std::fs::write(root.join("big.bin"), vec![7u8; 9 << 20]).unwrap();
    let c = reqwest::Client::new();
    let get = |p: &str, r: Option<&str>| {
        let mut q = c.get(format!("{base}/api/raw?path={p}"));
        if let Some(r) = r {
            q = q.header("range", r);
        }
        q.send()
    };
    let r = get("r.bin", Some("bytes=0-9")).await.unwrap();
    assert_eq!(r.status(), 206);
    assert_eq!(r.headers()["content-range"], "bytes 0-9/256");
    assert_eq!(r.headers()["accept-ranges"], "bytes");
    assert_eq!(
        r.bytes().await.unwrap().to_vec(),
        (0..10u8).collect::<Vec<_>>()
    );
    let r = get("r.bin", Some("bytes=-4")).await.unwrap();
    assert_eq!(r.status(), 206);
    assert_eq!(r.bytes().await.unwrap().to_vec(), vec![252, 253, 254, 255]);
    let r = get("r.bin", Some("bytes=300-400")).await.unwrap();
    assert_eq!(r.status(), 416);
    assert_eq!(r.headers()["content-range"], "bytes */256");
    assert_eq!(get("r.bin", Some("bytes=9-2")).await.unwrap().status(), 416);
    assert_eq!(get("r.bin", None).await.unwrap().status(), 200);
    let r = get("big.bin", None).await.unwrap();
    assert_eq!(r.status(), 200);
    assert!(r.headers().get("content-range").is_none());
    assert_eq!(r.bytes().await.unwrap().len(), 9 << 20);
    let r = get("big.bin", Some("bytes=0-")).await.unwrap();
    assert_eq!(r.status(), 206);
    assert_eq!(r.bytes().await.unwrap().len(), 8 << 20);
}

#[tokio::test]
async fn me_returns_fullname_after_sync() {
    let (base, ..) = start("me").await;
    let c = reqwest::Client::new();
    sync_wait(&c, &base).await;
    let j: serde_json::Value = c
        .get(format!("{base}/api/me"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(j["fullname"], "Mock User");
}

#[tokio::test]
async fn file_meta_renames_link_and_repoints() {
    let (base, app, _m, root) = start("fmeta").await;
    let c = reqwest::Client::new();
    sync_wait(&c, &base).await;
    std::fs::create_dir_all(root.join("submit/T")).unwrap();
    let target = "Digitalització aplicada/RA01/Mapa IT·OT.txt";
    std::os::unix::fs::symlink(root.join(target), root.join("submit/T/old.txt")).unwrap();
    std::fs::write(root.join("submit/T/other.txt"), "o").unwrap();
    app.db
        .update_local("assign:7", |l| {
            l.files = vec!["submit/T/old.txt".into(), "submit/T/other.txt".into()];
            l.owners.insert("submit/T/old.txt".into(), "Ana".into());
        })
        .unwrap();
    let go = |path: &str, name: &str| {
        post(&c, &base, "/api/tasks/assign:7/file-meta")
            .json(&serde_json::json!({"path": path, "name": name, "owner": "Pau"}))
            .send()
    };
    for (path, name, code) in [
        ("submit/T/old.txt", "", 400),
        ("submit/T/old.txt", "a/b.txt", 400),
        ("submit/T/old.txt", "..", 400),
        ("submit/T/old.txt", ".hid", 400),
        ("submit/T/old.txt", &"a".repeat(201), 400),
        ("submit/T/old.txt", "other.txt", 409),
        ("submit/T/nope.txt", "z.txt", 400),
        (target, "z.txt", 400),
    ] {
        assert_eq!(
            go(path, name).await.unwrap().status(),
            code,
            "{path} {name}"
        );
    }
    let r = go("submit/T/old.txt", "new_amin.txt").await.unwrap();
    assert!(r.status().is_success(), "{}", r.status());
    let l = app.db.local("assign:7").unwrap();
    assert_eq!(l.files, ["submit/T/new_amin.txt", "submit/T/other.txt"]);
    assert_eq!(l.owners.len(), 1);
    assert_eq!(l.owners["submit/T/new_amin.txt"], "Pau");
    let lk = root.join("submit/T/new_amin.txt");
    assert!(lk.symlink_metadata().unwrap().is_symlink());
    assert!(root.join("submit/T/old.txt").symlink_metadata().is_err());
    assert_eq!(std::fs::read_to_string(root.join(target)).unwrap(), "hola");
}

#[tokio::test]
async fn to_pdf_converts_linked_docx_next_to_it() {
    MockOo::shared();
    let (base, _app, _m, root) = start("topdf").await;
    let c = reqwest::Client::new();
    let dir = seed_task_dir(&c, &base).await;
    let one: serde_json::Value = c
        .get(format!("{base}/api/tasks/assign:7"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(one["needs_pdf"], true);
    std::fs::create_dir_all(root.join(&dir)).unwrap();
    std::fs::write(root.join(&dir).join("a.docx"), "docx-topdf").unwrap();
    std::fs::write(root.join(&dir).join("b.txt"), "x").unwrap();
    std::fs::write(root.join(&dir).join("a_amin.pdf"), "old").unwrap();
    let (d, t) = (format!("{dir}/a.docx"), format!("{dir}/b.txt"));
    post(&c, &base, "/api/tasks/assign:7/files")
        .json(&serde_json::json!({"files": [d, t]}))
        .send()
        .await
        .unwrap();
    let go = |p: &str| {
        post(&c, &base, "/api/tasks/assign:7/to-pdf").json(&serde_json::json!({"path": p}))
    };
    assert_eq!(go(&t).send().await.unwrap().status(), 400);
    assert_eq!(go("x/none.docx").send().await.unwrap().status(), 400);
    let r = go(&d).send().await.unwrap();
    assert_eq!(r.status(), 200);
    let l: serde_json::Value = r.json().await.unwrap();
    let f = l["files"].to_string();
    assert!(f.contains("a-2_amin.pdf") && !f.contains("a.docx"), "{l}");
    assert_eq!(
        std::fs::read(root.join(&dir).join("a-2_amin.pdf")).unwrap(),
        b"%PDF-mock"
    );
    assert_eq!(go(&d).send().await.unwrap().status(), 400);
}

#[tokio::test]
async fn raw_dl_is_attachment() {
    let (base, _app, _m, root) = start("rawdl").await;
    std::fs::write(root.join("Mapa IT·OT.txt"), "hola").unwrap();
    let c = reqwest::Client::new();
    let q = moodle_mcp::web::tasks::pct("Mapa IT·OT.txt");
    let d = |r: reqwest::Response| {
        r.headers()["content-disposition"]
            .to_str()
            .unwrap()
            .to_string()
    };
    let r = c
        .get(format!("{base}/api/raw?path={q}&dl=1"))
        .send()
        .await
        .unwrap();
    assert_eq!(
        d(r),
        "attachment; filename=\"Mapa IT_OT.txt\"; filename*=UTF-8''Mapa%20IT%C2%B7OT.txt"
    );
    let r = c
        .get(format!("{base}/api/raw?path={q}"))
        .send()
        .await
        .unwrap();
    assert_eq!(d(r), "inline");
}

#[tokio::test]
async fn put_writes_links_never_overwrites_and_rejects_bad_names() {
    let (base, _app, _m, root) = start("put").await;
    let c = reqwest::Client::new();
    sync_wait(&c, &base).await;
    let put = |name: &'static str| {
        post(&c, &base, "/api/tasks/assign:7/put")
            .query(&[("name", name)])
            .body("hola")
            .send()
    };
    let r = put("Doc.docx").await.unwrap();
    assert!(r.status().is_success());
    let l: serde_json::Value = r.json().await.unwrap();
    let f = l["files"][0].as_str().unwrap().to_string();
    assert!(f.ends_with("/Doc_amin.docx"), "{f}");
    assert_eq!(std::fs::read(root.join(&f)).unwrap(), b"hola");
    let l: serde_json::Value = put("Doc.docx").await.unwrap().json().await.unwrap();
    assert!(l["files"][1]
        .as_str()
        .unwrap()
        .ends_with("/Doc-2_amin.docx"));
    assert_eq!(put("../a").await.unwrap().status(), 400);
}
