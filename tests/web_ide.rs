#[path = "integration/mock.rs"]
mod mock;

use mock::{MockMoodle, TOKEN};
use moodle_mcp::{
    moodle::Moodle,
    state::{CourseState, FileEntry, State},
    web::{self, db::Db, ide::sha_hex},
};
use serde_json::{json, Value};
use std::sync::Arc;

struct T {
    base: String,
    app: Arc<web::App>,
    root: std::path::PathBuf,
    c: reqwest::Client,
    _m: MockMoodle,
}

impl T {
    async fn new(tag: &str) -> T {
        let m = MockMoodle::healthy().await;
        let root = std::env::temp_dir().join(format!("mweb-ide-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("C")).unwrap();
        std::fs::write(root.join("C/teacher.txt"), "original\n").unwrap();
        std::fs::write(root.join("C/mine.txt"), "hola\n").unwrap();
        std::fs::write(root.join("C/bin.dat"), [0xff, 0xfe, 0x00, 0x9f]).unwrap();
        std::fs::write(root.join(".env"), "SECRET=1").unwrap();
        // state.json marks C/teacher.txt as synced from Moodle
        let mut st = State::default();
        let mut cs = CourseState::default();
        cs.files.insert(
            "https://m/teacher.txt".into(),
            FileEntry::new(
                "C/teacher.txt".into(),
                "x".into(),
                9,
                0,
                String::new(),
                "pluginfile",
            ),
        );
        st.courses.insert("1".into(), cs);
        st.save(root.to_str().unwrap()).unwrap();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let app = Arc::new(web::App::new(
            Db::memory().unwrap(),
            Moodle::new(m.base(), TOKEN),
            root.to_str().unwrap(),
            port,
        ));
        tokio::spawn(web::serve_on(listener, Arc::clone(&app)));
        T {
            base: format!("http://127.0.0.1:{port}"),
            app,
            root,
            c: reqwest::Client::new(),
            _m: m,
        }
    }
    async fn get(&self, path: &str) -> reqwest::Response {
        self.c
            .get(format!("{}{path}", self.base))
            .send()
            .await
            .unwrap()
    }
    async fn post(&self, path: &str, body: Value) -> reqwest::Response {
        self.c
            .post(format!("{}{path}", self.base))
            .header("origin", &self.base)
            .header("x-neu", "1")
            .json(&body)
            .send()
            .await
            .unwrap()
    }
    fn read(&self, rel: &str) -> String {
        std::fs::read_to_string(self.root.join(rel)).unwrap()
    }
}

#[tokio::test]
async fn read_returns_content_sha_and_teacher_flag() {
    let t = T::new("read").await;
    let r = t.get("/api/ide/file?path=C/mine.txt").await;
    assert_eq!(r.status(), 200);
    let j: Value = r.json().await.unwrap();
    assert_eq!(j["rel"], "C/mine.txt");
    assert_eq!(j["content"], "hola\n");
    assert_eq!(j["sha"], sha_hex(b"hola\n"));
    assert_eq!(j["teacher"], false);
    let j: Value = t
        .get("/api/ide/file?path=C/teacher.txt")
        .await
        .json()
        .await
        .unwrap();
    assert_eq!(j["teacher"], true);
}

#[tokio::test]
async fn read_refuses_jail_escape_dotfiles_dirs_and_binary() {
    let t = T::new("readbad").await;
    for p in ["../etc/passwd", ".env", ".moodle/state.json", "missing.txt"] {
        assert_eq!(
            t.get(&format!("/api/ide/file?path={p}")).await.status(),
            403,
            "{p}"
        );
    }
    assert_eq!(t.get("/api/ide/file?path=C").await.status(), 400);
    assert_eq!(t.get("/api/ide/file?path=C/bin.dat").await.status(), 415);
}

#[tokio::test]
async fn save_writes_bak_tmp_rename_and_returns_new_sha() {
    use std::os::unix::fs::PermissionsExt;
    let t = T::new("save").await;
    std::fs::set_permissions(
        t.root.join("C/mine.txt"),
        std::fs::Permissions::from_mode(0o640),
    )
    .unwrap();
    let r = t
        .post(
            "/api/save",
            json!({"path": "C/mine.txt", "content": "nou\n", "base_sha": sha_hex(b"hola\n")}),
        )
        .await;
    assert_eq!(r.status(), 200);
    assert_eq!(r.json::<Value>().await.unwrap()["sha"], sha_hex(b"nou\n"));
    assert_eq!(t.read("C/mine.txt"), "nou\n");
    assert_eq!(t.read("C/mine.txt.bak"), "hola\n");
    assert!(!t.root.join("C/mine.txt.tmp").exists());
    let mode = std::fs::metadata(t.root.join("C/mine.txt"))
        .unwrap()
        .permissions()
        .mode();
    assert_eq!(mode & 0o777, 0o640);
    // second save with the returned sha works; .bak now holds the previous version
    let r = t
        .post(
            "/api/save",
            json!({"path": "C/mine.txt", "content": "tres\n", "base_sha": sha_hex(b"nou\n")}),
        )
        .await;
    assert_eq!(r.status(), 200);
    assert_eq!(t.read("C/mine.txt.bak"), "nou\n");
}

#[tokio::test]
async fn save_sha_mismatch_is_conflict_and_touches_nothing() {
    let t = T::new("conflict").await;
    let r = t
        .post(
            "/api/save",
            json!({"path": "C/mine.txt", "content": "x", "base_sha": "deadbeef"}),
        )
        .await;
    assert_eq!(r.status(), 409);
    let j: Value = r.json().await.unwrap();
    assert_eq!(j["error"], "conflict");
    assert_eq!(j["sha"], sha_hex(b"hola\n"));
    assert_eq!(t.read("C/mine.txt"), "hola\n");
    assert!(!t.root.join("C/mine.txt.bak").exists());
}

#[tokio::test]
async fn save_refuses_teacher_original() {
    let t = T::new("teacher").await;
    let r = t
        .post(
            "/api/save",
            json!({"path": "C/teacher.txt", "content": "hacked", "base_sha": sha_hex(b"original\n")}),
        )
        .await;
    assert_eq!(r.status(), 409);
    let j: Value = r.json().await.unwrap();
    assert_eq!(j["error"], "teacher_original");
    assert_eq!(j["path"], "C/teacher.txt");
    assert_eq!(t.read("C/teacher.txt"), "original\n");
    assert!(!t.root.join("C/teacher.txt.bak").exists());
}

#[tokio::test]
async fn save_refuses_jail_escape_binary_and_missing_header() {
    let t = T::new("savebad").await;
    for p in ["../evil.txt", ".env", ".moodle/state.json", "nope.txt"] {
        let r = t
            .post(
                "/api/save",
                json!({"path": p, "content": "x", "base_sha": ""}),
            )
            .await;
        assert_eq!(r.status(), 403, "{p}");
    }
    assert_eq!(t.read(".env"), "SECRET=1");
    let r = t
        .post(
            "/api/save",
            json!({"path": "C/bin.dat", "content": "x", "base_sha": ""}),
        )
        .await;
    assert_eq!(r.status(), 415);
    // guard: mutation without X-Neu is refused before the handler
    let r =
        t.c.post(format!("{}/api/save", t.base))
            .header("origin", &t.base)
            .json(&json!({"path": "C/mine.txt", "content": "x", "base_sha": sha_hex(b"hola\n")}))
            .send()
            .await
            .unwrap();
    assert_eq!(r.status(), 403);
    assert_eq!(t.read("C/mine.txt"), "hola\n");
}

#[tokio::test]
async fn copy_makes_amin_repoints_task_and_original_stays_byte_identical() {
    use moodle_mcp::web::db::Local;
    let t = T::new("copy").await;
    t.app
        .db
        .set_local(&Local {
            task_id: "a".into(),
            files: vec!["C/teacher.txt".into()],
            ..Default::default()
        })
        .unwrap();
    let r = t
        .post("/api/ide/copy", json!({"path": "C/teacher.txt"}))
        .await;
    assert_eq!(r.status(), 200);
    let j: Value = r.json().await.unwrap();
    assert_eq!(j["rel"], "C/teacher_amin.txt");
    assert_eq!(j["sha"], sha_hex(b"original\n"));
    assert_eq!(t.app.db.local("a").unwrap().files, ["C/teacher_amin.txt"]);
    // the copy is editable
    let r = t
        .post(
            "/api/save",
            json!({"path": "C/teacher_amin.txt", "content": "meu\n", "base_sha": j["sha"]}),
        )
        .await;
    assert_eq!(r.status(), 200);
    assert_eq!(t.read("C/teacher_amin.txt"), "meu\n");
    assert_eq!(t.read("C/teacher.txt"), "original\n");
    // second call keeps the edited copy (no clobber)
    let j: Value = t
        .post("/api/ide/copy", json!({"path": "C/teacher.txt"}))
        .await
        .json()
        .await
        .unwrap();
    assert_eq!(j["sha"], sha_hex(b"meu\n"));
    // binary and jail refusals create nothing
    assert_eq!(
        t.post("/api/ide/copy", json!({"path": "C/bin.dat"}))
            .await
            .status(),
        415
    );
    assert!(!t.root.join("C/bin_amin.dat").exists());
    assert_eq!(
        t.post("/api/ide/copy", json!({"path": ".env"}))
            .await
            .status(),
        403
    );
}
