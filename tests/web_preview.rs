#[path = "integration/mock.rs"]
mod mock;

use mock::{MockMoodle, MockOo, OO_SECRET, TOKEN};
use moodle_mcp::{
    moodle::Moodle,
    web::{self, db::Db, office},
};
use sha2::{Digest, Sha256};
use std::{path::PathBuf, sync::Arc};

fn sha(b: &[u8]) -> String {
    Sha256::digest(b)
        .iter()
        .map(|x| format!("{x:02x}"))
        .collect()
}

async fn spawn(tag: &str) -> (String, PathBuf, Arc<web::App>, MockMoodle) {
    let m = MockMoodle::healthy().await;
    let root = std::env::temp_dir().join(format!("mweb-prev-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(root.join("C")).unwrap();
    std::fs::create_dir_all(root.join(".moodle")).unwrap();
    std::fs::write(root.join(".env"), "X=1").unwrap();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let app = Arc::new(web::App::new(
        Db::memory().unwrap(),
        Moodle::new(m.base(), TOKEN),
        root.to_str().unwrap(),
        port,
    ));
    tokio::spawn(web::serve_on(listener, Arc::clone(&app)));
    (format!("http://127.0.0.1:{port}"), root, app, m)
}

async fn get(base: &str, q: &[(&str, &str)]) -> reqwest::Response {
    reqwest::Client::new()
        .get(format!("{base}/api/preview"))
        .query(q)
        .send()
        .await
        .unwrap()
}

fn hdr(r: &reqwest::Response, k: &str) -> String {
    r.headers()
        .get(k)
        .map(|v| v.to_str().unwrap().to_string())
        .unwrap_or_default()
}

#[tokio::test]
async fn passthrough_types_sandbox_and_url_stub() {
    let (base, root, app, _m) = spawn("pass").await;
    std::fs::write(root.join("C/p.html"), "<script>1</script>").unwrap();
    std::fs::write(root.join("C/i.svg"), "<svg onload=alert(1)/>").unwrap();
    std::fs::write(root.join("C/Main.java"), "class Main {}").unwrap();
    std::fs::write(root.join("C/a.zip"), "PK").unwrap();
    let stub = b"[InternetShortcut]\nURL=https://docs.google.com/x\n";
    std::fs::write(root.join("C/l.url"), stub).unwrap();

    let r = get(&base, &[("path", "C/p.html")]).await;
    assert_eq!(r.status(), 200);
    assert_eq!(hdr(&r, "content-type"), "text/html; charset=utf-8");
    assert!(hdr(&r, "content-security-policy").contains("sandbox"));
    let r = get(&base, &[("path", "C/i.svg")]).await;
    assert!(hdr(&r, "content-security-policy").contains("sandbox"));

    let r = get(&base, &[("path", "C/Main.java")]).await;
    assert_eq!(hdr(&r, "content-type"), "text/plain; charset=utf-8");
    assert_eq!(hdr(&r, "content-disposition"), "inline");
    assert_eq!(r.text().await.unwrap(), "class Main {}");

    // zip: no zip crate in Cargo.lock, so download-only
    let r = get(&base, &[("path", "C/a.zip")]).await;
    assert_eq!(hdr(&r, "content-type"), "application/zip");
    assert_eq!(hdr(&r, "content-disposition"), "attachment");

    // .url shows the link, plus cached scanner text when present
    let t = get(&base, &[("path", "C/l.url")])
        .await
        .text()
        .await
        .unwrap();
    assert!(t.contains("URL=https://docs.google.com/x") && !t.contains("scanner"));
    app.db
        .cache_put(&sha(stub), "u", "scanner says: 2 tasks", 1)
        .unwrap();
    let t = get(&base, &[("path", "C/l.url")])
        .await
        .text()
        .await
        .unwrap();
    assert!(t.contains("URL=https://docs.google.com/x") && t.contains("scanner says: 2 tasks"));
}

#[tokio::test]
async fn rejects_escapes_and_bad_params() {
    let (base, _root, _app, _m) = spawn("jail").await;
    for p in [
        "../etc/passwd",
        ".env",
        ".moodle",
        "/etc/passwd",
        "C/../../x",
    ] {
        assert_eq!(get(&base, &[("path", p)]).await.status(), 403, "{p}");
    }
    assert_eq!(get(&base, &[("path", "C/missing.txt")]).await.status(), 403);
    assert_eq!(get(&base, &[]).await.status(), 400);
    assert_eq!(
        get(&base, &[("path", "C"), ("moodle", "http://x/")])
            .await
            .status(),
        400
    );
}

#[tokio::test]
async fn office_converts_once_and_caches() {
    let oo = MockOo::shared();
    let (base, root, _app, _m) = spawn("office").await;
    let body = b"docx bytes unique-1";
    std::fs::write(root.join("C/a.docx"), body).unwrap();
    for _ in 0..2 {
        let r = get(&base, &[("path", "C/a.docx")]).await;
        assert_eq!(r.status(), 200);
        assert_eq!(hdr(&r, "content-type"), "application/pdf");
        assert_eq!(r.bytes().await.unwrap().as_ref(), b"%PDF-mock");
    }
    assert_eq!(oo.converts_of(body), 1, "second request served from cache");
    assert!(root
        .join(format!(".moodle/previews/{}.pdf", sha(body)))
        .exists());
    assert_eq!(std::fs::read(root.join("C/a.docx")).unwrap(), body);
}

#[tokio::test]
async fn failed_conversion_is_502_and_not_cached() {
    MockOo::shared();
    let (base, root, _app, _m) = spawn("fail").await;
    std::fs::write(root.join("C/bad.pptx"), b"corrupt").unwrap();
    let r = get(&base, &[("path", "C/bad.pptx")]).await;
    assert_eq!(r.status(), 502);
    assert!(!root
        .join(format!(".moodle/previews/{}.pdf", sha(b"corrupt")))
        .exists());
}

#[tokio::test]
async fn psrc_only_serves_staged_names() {
    MockOo::shared();
    let (base, root, _app, _m) = spawn("psrc").await;
    std::fs::write(root.join("C/a.docx"), "x").unwrap();
    let c = reqwest::Client::new();
    for name in ["../../.env", "C/a.docx", "../x.docx", "abc.docx"] {
        let t = office::file_token(OO_SECRET, name, web::now());
        let r = c.get(format!("{base}/oo/psrc?t={t}")).send().await.unwrap();
        assert_eq!(r.status(), 403, "{name}");
    }
    let r = c
        .get(format!("{base}/oo/psrc?t=junk"))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 403);
}

#[tokio::test]
async fn moodle_preview_host_check_download_and_convert() {
    let oo = MockOo::shared();
    let (base, root, _app, m) = spawn("moodle").await;
    let pf = |p: &str| format!("{}/webservice/pluginfile.php{p}", m.base());

    // plain text: downloaded, served as text
    let r = get(
        &base,
        &[("moodle", &pf("/101/mod_resource/content/6/s1-m3-0.txt"))],
    )
    .await;
    assert_eq!(r.status(), 200);
    assert_eq!(hdr(&r, "content-type"), "text/plain; charset=utf-8");
    assert!(r.text().await.unwrap().starts_with("mock-bytes:"));

    // pptx: converted to pdf once, then cached (url cache + pdf cache)
    let u = pf("/101/mod_resource/content/1/s1-m0-1.pptx");
    for _ in 0..2 {
        let r = get(&base, &[("moodle", &u)]).await;
        assert_eq!(r.status(), 200);
        assert_eq!(hdr(&r, "content-type"), "application/pdf");
    }
    let src = m
        .pluginfile("/101/mod_resource/content/1/s1-m0-1.pptx")
        .unwrap();
    assert_eq!(oo.converts_of(src), 1);
    assert!(root
        .join(format!(".moodle/previews/{}.pdf", sha(src)))
        .exists());

    // SSRF: other host, prefix trick, non-http scheme, all refused before any fetch
    for bad in [
        "https://evil.example/webservice/pluginfile.php/1/a.txt".to_string(),
        format!("{}0/webservice/pluginfile.php/1/a.txt", m.base()),
        "file:///etc/passwd".to_string(),
        "http://127.0.0.1:1/".to_string(),
    ] {
        assert_eq!(get(&base, &[("moodle", &bad)]).await.status(), 403, "{bad}");
    }
}

#[tokio::test]
async fn huge_file_is_413_not_read() {
    let (base, root, _app, _m) = spawn("big").await;
    let f = std::fs::File::create(root.join("C/big.mp4")).unwrap();
    f.set_len(65 << 20).unwrap();
    assert_eq!(get(&base, &[("path", "C/big.mp4")]).await.status(), 413);
}

#[tokio::test]
async fn locate_jails_and_skips_non_pdf() {
    let (base, root, _app, _m) = spawn("loc").await;
    std::fs::write(root.join("C/a.txt"), "x").unwrap();
    let go = |p: &str| {
        reqwest::Client::new()
            .get(format!("{base}/api/locate"))
            .query(&[("path", p), ("q", "011 Endevinar")])
            .send()
    };
    assert_eq!(go("../etc/passwd").await.unwrap().status(), 403);
    assert_eq!(go(".env").await.unwrap().status(), 403);
    let r = go("C/a.txt").await.unwrap();
    assert_eq!(
        r.json::<serde_json::Value>().await.unwrap()["page"],
        serde_json::Value::Null
    );
}
