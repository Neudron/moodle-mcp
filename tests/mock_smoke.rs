//! Mock-server smoke tests (Task 1, Step 3): the real
//! [`moodle_mcp::moodle::Moodle`] client against the
//! [`mock`][mock_mod] fixture.
//!
//! [mock_mod]: crate::mock

#[path = "integration/mock.rs"]
mod mock;

use mock::{MockConfig, MockMoodle, COURSE_ID, TOKEN, USER_ID};
use moodle_mcp::moodle::Moodle;

fn client(m: &MockMoodle) -> Moodle {
    Moodle::new(m.base(), TOKEN)
}

#[tokio::test]
async fn site_info_and_courses() {
    let m = MockMoodle::healthy().await;
    let c = client(&m);
    let info = c.site_info().await.expect("site_info");
    assert_eq!(info, m.site_info);
    assert_eq!(info["userid"], USER_ID);
    let courses = c.courses(USER_ID).await.expect("courses");
    assert!(courses.iter().any(|x| x.id == COURSE_ID));
    assert_eq!(
        serde_json::to_value(&courses).expect("serialize courses"),
        m.courses
    );
}

#[tokio::test]
async fn contents_has_50_files_and_html_variants() {
    let m = MockMoodle::healthy().await;
    let sections = client(&m).contents(COURSE_ID).await.expect("contents");
    let n_files: usize = sections
        .iter()
        .flat_map(|s| &s.modules)
        .filter_map(|x| x.contents.as_ref())
        .map(Vec::len)
        .sum();
    assert_eq!(n_files, 50);
    assert_eq!(
        serde_json::to_value(&sections).expect("serialize contents"),
        m.contents
    );
    let descs: Vec<&str> = sections
        .iter()
        .flat_map(|s| &s.modules)
        .filter_map(|x| x.description.as_deref())
        .collect();
    for needle in ["<a ", "<table", "<pre>", "<img", "<ul>"] {
        assert!(
            descs.iter().any(|d| d.contains(needle)),
            "missing html variant {needle}"
        );
    }
}

#[tokio::test]
async fn pluginfile_download_roundtrip() {
    let m = MockMoodle::healthy().await;
    let c = client(&m);
    let sections = c.contents(COURSE_ID).await.expect("contents");
    let first = sections
        .iter()
        .flat_map(|s| &s.modules)
        .filter_map(|x| x.contents.as_ref())
        .flatten()
        .find_map(|f| f.fileurl.clone())
        .expect("a fileurl");
    let bytes = c.download(&first).await.expect("download");
    let path = first
        .split("pluginfile.php")
        .nth(1)
        .and_then(|rest| rest.split('?').next())
        .expect("pluginfile path");
    assert_eq!(bytes, m.pluginfile(path).expect("fixture bytes"));
}

#[tokio::test]
async fn missing_file_is_404() {
    let m = MockMoodle::healthy().await;
    let err = client(&m)
        .download(&format!(
            "{}/webservice/pluginfile.php/999/nope.pdf",
            m.base()
        ))
        .await
        .expect_err("unknown file must fail");
    assert!(err.to_string().contains("404"), "unexpected: {err}");
}

#[tokio::test]
async fn bad_token_is_401() {
    let m = MockMoodle::healthy().await;
    let err = Moodle::new(m.base(), "wrong-token")
        .site_info()
        .await
        .expect_err("wrong token must fail");
    assert!(err.to_string().contains("401"), "unexpected: {err}");
    let m2 = MockMoodle::unauthorized().await;
    let err = Moodle::new(m2.base(), TOKEN)
        .site_info()
        .await
        .expect_err("forced 401 must fail");
    assert!(err.to_string().contains("401"), "unexpected: {err}");
}

#[tokio::test]
async fn ratelimit_headers_then_recovers() {
    let m = MockMoodle::ratelimited(1).await;
    assert_eq!(m.ratelimit, 1);
    let raw = reqwest::Client::new()
        .post(format!("{}/webservice/rest/server.php", m.base()))
        .form(&[
            ("wstoken", TOKEN),
            ("wsfunction", "core_webservice_get_site_info"),
            ("moodlewsrestformat", "json"),
        ])
        .send()
        .await
        .expect("post");
    assert_eq!(raw.status(), reqwest::StatusCode::TOO_MANY_REQUESTS);
    assert_eq!(
        raw.headers()
            .get(reqwest::header::RETRY_AFTER)
            .and_then(|v| v.to_str().ok()),
        Some("2")
    );
    let info = client(&m).site_info().await.expect("recovers after 429");
    assert_eq!(info["userid"], USER_ID);
}

#[tokio::test]
async fn flaky_500_then_ok() {
    let m = MockMoodle::flaky(2).await;
    assert_eq!(m.flaky_500, 2);
    let c = client(&m);
    let first = c.site_info().await.expect_err("first call must 500");
    assert!(first.to_string().contains("500"), "unexpected: {first}");
    assert!(c.site_info().await.is_err());
    let info = c.site_info().await.expect("recovers after flakies");
    assert_eq!(info["userid"], USER_ID);
}

#[tokio::test]
async fn combined_ratelimit_before_flaky() {
    let m = MockMoodle::start(MockConfig {
        flaky_500: 1,
        ratelimit: 1,
        ..Default::default()
    })
    .await;
    let c = client(&m);
    // Route order: 429 quota is consumed before the 500 quota.
    let first = c.site_info().await.expect_err("first call must 429");
    assert!(first.to_string().contains("429"), "unexpected: {first}");
    let second = c.site_info().await.expect_err("second call must 500");
    assert!(second.to_string().contains("500"), "unexpected: {second}");
    let info = c.site_info().await.expect("recovers after quotas");
    assert_eq!(info["userid"], USER_ID);
}
