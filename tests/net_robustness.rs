//! Task 2 (P1 network/robustness + security core) integration tests.
//!
//! Exercises the real [`moodle_mcp::moodle::Moodle`] client against the
//! [`mock`][mock_mod] fixture: retry with backoff + `Retry-After`, typed
//! AUTH/NOT_FOUND mapping, resumable downloads, URL allowlist, token
//! redaction.
//!
//! [mock_mod]: crate::mock

#[path = "integration/mock.rs"]
mod mock;

use mock::{MockConfig, MockMoodle, COURSE_ID, LAST_MODIFIED_UNIX, TOKEN, USER_ID};
use moodle_mcp::moodle::Moodle;
use sha2::Digest as _;
use std::path::PathBuf;

fn client(m: &MockMoodle) -> Moodle {
    Moodle::new(m.base(), TOKEN)
}

/// Brief Step 1 verbatim intent: flaky 500s then success via retry.
#[tokio::test]
async fn retries_flaky_500_then_ok() {
    let m = MockMoodle::flaky(2).await;
    let c = client(&m);
    let v = c
        .call_with_retry("core_webservice_get_site_info", &[])
        .await
        .expect("retry must recover after 2x500");
    assert_eq!(v["userid"], USER_ID);
}

#[tokio::test]
async fn retry_honors_ratelimit_then_ok() {
    let m = MockMoodle::ratelimited(1).await;
    let v = client(&m)
        .call_with_retry("core_webservice_get_site_info", &[])
        .await
        .expect("retry must recover after 429+Retry-After");
    assert_eq!(v["userid"], USER_ID);
}

#[tokio::test]
async fn auth_maps_to_typed_error_and_aborts() {
    let m = MockMoodle::healthy().await;
    let err = Moodle::new(m.base(), "wrong-token-0123456789abcdef")
        .call_with_retry("core_webservice_get_site_info", &[])
        .await
        .expect_err("bad token must fail without retry");
    assert!(
        matches!(err, moodle_mcp::errors::CoreError::Auth(_)),
        "unexpected: {err}"
    );
    // No retry storm on auth: unauthorized mock would 401 forever; the
    // call above must return promptly (no 4x backoff sleep).
}

#[tokio::test]
async fn not_found_maps_to_typed_error() {
    let m = MockMoodle::healthy().await;
    let c = client(&m);
    let err = c
        .download_resumable(
            &format!("{}/webservice/pluginfile.php/999/nope.pdf", m.base()),
            &PathBuf::from("/tmp/moodle-mcp-test-nope.part"),
        )
        .await
        .expect_err("unknown file must fail");
    assert!(
        matches!(err, moodle_mcp::errors::CoreError::NotFound(_)),
        "unexpected: {err}"
    );
}

#[tokio::test]
async fn download_resumable_roundtrip_with_meta() {
    let m = MockMoodle::healthy().await;
    let c = client(&m);
    let sections = c.contents(COURSE_ID).await.expect("contents");
    let first = sections
        .iter()
        .flat_map(|s| &s.modules)
        .filter_map(|x| x.contents.as_ref())
        .flatten()
        .find(|f| f.filename.ends_with(".pdf"))
        .expect("a pdf fileurl");
    let url = first.fileurl.clone().expect("fileurl");
    let dir = std::env::temp_dir().join(format!("moodle-mcp-t2-{}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("tmpdir");
    let part = dir.join("roundtrip.pdf.part");
    let _ = std::fs::remove_file(&part);
    let meta = c.download_resumable(&url, &part).await.expect("download");
    let bytes = std::fs::read(&part).expect("part file");
    assert_eq!(meta.size, bytes.len() as u64);
    assert_eq!(meta.mimetype, "application/pdf");
    assert_eq!(meta.timemodified, LAST_MODIFIED_UNIX);
    let digest = sha2::Sha256::digest(&bytes);
    assert_eq!(meta.sha256, format!("{digest:x}"));
    let _ = std::fs::remove_file(&part);
}

#[tokio::test]
async fn download_resumes_from_partial_part_file() {
    let m = MockMoodle::healthy().await;
    let c = client(&m);
    let sections = c.contents(COURSE_ID).await.expect("contents");
    let first = sections
        .iter()
        .flat_map(|s| &s.modules)
        .filter_map(|x| x.contents.as_ref())
        .flatten()
        .next()
        .expect("a fileurl");
    let url = first.fileurl.clone().expect("fileurl");
    let path = url
        .split("pluginfile.php")
        .nth(1)
        .and_then(|rest| rest.split('?').next())
        .expect("pluginfile path");
    let full = m.pluginfile(path).expect("fixture bytes").to_vec();
    assert!(full.len() > 4, "fixture must be resumable");
    let dir = std::env::temp_dir().join(format!("moodle-mcp-t2-{}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("tmpdir");
    let part = dir.join("resume.pdf.part");
    // Seed a truncated prefix: the client must Range-resume, not restart.
    std::fs::write(&part, &full[..full.len() / 2]).expect("seed part");
    let meta = c.download_resumable(&url, &part).await.expect("resume");
    let bytes = std::fs::read(&part).expect("part file");
    assert_eq!(bytes, full, "resumed bytes must equal full fixture");
    assert_eq!(meta.size, full.len() as u64);
    let _ = std::fs::remove_file(&part);
}

#[tokio::test]
async fn refuses_non_http_and_non_pluginfile_urls() {
    let m = MockMoodle::healthy().await;
    let c = client(&m);
    let part = std::env::temp_dir().join(format!("moodle-mcp-t2-nope-{}.part", std::process::id()));
    for bad in [
        "ftp://example.invalid/x.pdf".to_string(),
        "file:///etc/passwd".to_string(),
        format!("{}/theme/image.php/x.png", m.base()),
    ] {
        let err = c
            .download_resumable(&bad, &part)
            .await
            .expect_err("must refuse {bad}");
        assert!(
            matches!(err, moodle_mcp::errors::CoreError::Invalid(_)),
            "unexpected for {bad}: {err}"
        );
    }
    // Foreign-host pluginfile URL: never fetched there — re-homed onto our
    // base, where the path is unknown → NOT_FOUND (row error, no token leak).
    let err = c
        .download_resumable(
            "https://evil.invalid/webservice/pluginfile.php/9/unknown.pdf",
            &part,
        )
        .await
        .expect_err("re-homed unknown path must 404");
    assert!(
        matches!(err, moodle_mcp::errors::CoreError::NotFound(_)),
        "unexpected: {err}"
    );
}

#[tokio::test]
async fn redaction_token_absent_from_errors() {
    let fake = "FAKETOKEN-redact-me-0123456789abcdef";
    let m = MockMoodle::healthy().await;
    // 401 path carries no token.
    let err = Moodle::new(m.base(), fake)
        .call_with_retry("core_webservice_get_site_info", &[])
        .await
        .expect_err("bad token must fail");
    assert!(
        !err.to_string().contains(fake),
        "token leaked into error: {err}"
    );
    assert!(
        !format!("{err:?}").contains(fake),
        "token leaked into debug: {err:?}"
    );
    // Refused URLs echo redacted, never raw.
    let c = Moodle::new(m.base(), fake);
    let part =
        std::env::temp_dir().join(format!("moodle-mcp-t2-redact-{}.part", std::process::id()));
    let err = c
        .download_resumable(&format!("ftp://example.invalid/x.pdf?token={fake}"), &part)
        .await
        .expect_err("ftp must be refused");
    assert!(
        !err.to_string().contains(fake),
        "token leaked into error: {err}"
    );
    // Debug formatting of the client itself must not dump the token.
    assert!(
        !format!("{c:?}").contains(fake),
        "token leaked into Moodle debug"
    );
    // Helper scrubs both query and form encodings.
    for raw in [
        format!("https://x.invalid/f?token={fake}&a=1"),
        format!("wstoken={fake}&wsfunction=y"),
        format!("download http 500: https://x.invalid/f?token={fake}"),
    ] {
        assert!(
            !moodle_mcp::errors::redact_token(&raw).contains(fake),
            "redact_token missed: {raw}"
        );
    }
}

#[tokio::test]
async fn combined_ratelimit_flaky_recovers_with_retry() {
    let m = MockMoodle::start(MockConfig {
        flaky_500: 1,
        ratelimit: 1,
        ..Default::default()
    })
    .await;
    let v = client(&m)
        .call_with_retry("core_webservice_get_site_info", &[])
        .await
        .expect("must recover after 429 then 500");
    assert_eq!(v["userid"], USER_ID);
}

#[tokio::test]
async fn contents_retries_flaky_500() {
    let m = MockMoodle::flaky(1).await;
    client(&m)
        .contents(COURSE_ID)
        .await
        .expect("contents retries");
}

/// Raw one-shot-per-connection server. `cuts` = how many initial requests
/// get their body cut after `step` bytes (full Content-Length promised).
async fn raw_server(
    body: Vec<u8>,
    cuts: usize,
    step: usize,
    status_line: &'static str,
    extra: &'static str,
) -> String {
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;
    use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
    let l = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind");
    let base = format!("http://{}", l.local_addr().expect("addr"));
    let n = Arc::new(AtomicUsize::new(0));
    tokio::spawn(async move {
        loop {
            let Ok((mut s, _)) = l.accept().await else {
                break;
            };
            let (body, n) = (body.clone(), Arc::clone(&n));
            tokio::spawn(async move {
                let mut buf = vec![0u8; 4096];
                let k = s.read(&mut buf).await.unwrap_or(0);
                let req = String::from_utf8_lossy(&buf[..k]).to_lowercase();
                let start = req
                    .split("range: bytes=")
                    .nth(1)
                    .and_then(|r| r.split('-').next())
                    .and_then(|r| r.parse::<usize>().ok())
                    .unwrap_or(0);
                if status_line != "200 OK" {
                    let _ = s.write_all(format!("HTTP/1.1 {status_line}\r\n{extra}Content-Length: 0\r\nConnection: close\r\n\r\n").as_bytes()).await;
                    return;
                }
                let rest = &body[start..];
                let (code, cr) = if start > 0 {
                    (
                        "206 Partial Content",
                        format!(
                            "Content-Range: bytes {start}-{}/{}\r\n",
                            body.len() - 1,
                            body.len()
                        ),
                    )
                } else {
                    ("200 OK", String::new())
                };
                let _ = s.write_all(format!("HTTP/1.1 {code}\r\n{cr}Content-Length: {}\r\nConnection: close\r\n\r\n", rest.len()).as_bytes()).await;
                let send = if n.fetch_add(1, Ordering::SeqCst) < cuts {
                    step.min(rest.len() - 1)
                } else {
                    rest.len()
                };
                let _ = s.write_all(&rest[..send]).await;
            });
        }
    });
    base
}

/// M8: a body cut 6 times, each with progress, still completes (the
/// retry budget resets on progress).
#[tokio::test]
async fn download_survives_many_cuts_with_progress() {
    let body: Vec<u8> = (0..70_000u32).map(|i| (i % 251) as u8).collect();
    let base = raw_server(body.clone(), 6, 10_000, "200 OK", "").await;
    let part = std::env::temp_dir().join(format!("moodle-mcp-t17-{}.part", std::process::id()));
    let _ = std::fs::remove_file(&part);
    let meta = Moodle::new(&base, TOKEN)
        .download_resumable(
            &format!("{base}/webservice/pluginfile.php/1/big.bin"),
            &part,
        )
        .await
        .expect("cuts with progress must complete");
    assert_eq!(meta.size, body.len() as u64);
    assert_eq!(std::fs::read(&part).expect("part"), body);
    let _ = std::fs::remove_file(&part);
}

/// M9: Retry-After above the 60s cap fails fast, at both retry sites.
#[tokio::test]
async fn huge_retry_after_returns_ratelimited_immediately() {
    let base = raw_server(
        vec![],
        0,
        0,
        "429 Too Many Requests",
        "Retry-After: 3600\r\n",
    )
    .await;
    let c = Moodle::new(&base, TOKEN);
    let t = std::time::Instant::now();
    let e = c
        .call_with_retry("core_webservice_get_site_info", &[])
        .await
        .expect_err("429");
    assert!(
        matches!(e, moodle_mcp::errors::CoreError::RateLimited(3600)),
        "{e}"
    );
    let part = std::env::temp_dir().join(format!("moodle-mcp-t17b-{}.part", std::process::id()));
    let e = c
        .download_resumable(&format!("{base}/webservice/pluginfile.php/1/x.bin"), &part)
        .await
        .expect_err("429");
    assert!(
        matches!(e, moodle_mcp::errors::CoreError::RateLimited(3600)),
        "{e}"
    );
    assert!(t.elapsed() < std::time::Duration::from_secs(5));
}
