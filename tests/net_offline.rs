//! `--offline` gate (plan #1028): with `MOODLE_OFFLINE=1` every network
//! path aborts before connecting. Separate binary so the process-global env
//! mutation cannot race other tests.

use moodle_mcp::moodle::Moodle;

#[tokio::test]
async fn offline_blocks_api_and_download() {
    std::env::set_var("MOODLE_OFFLINE", "1");
    let c = Moodle::new("http://127.0.0.1:9", "fake-token-0123456789abcdef");
    let err = c
        .call_with_retry("core_webservice_get_site_info", &[])
        .await
        .expect_err("offline must block api");
    assert!(err.to_string().contains("offline"), "unexpected: {err}");
    let err = c
        .download_resumable(
            "http://127.0.0.1:9/webservice/pluginfile.php/1/a.pdf",
            &std::path::PathBuf::from("/tmp/moodle-mcp-offline.part"),
        )
        .await
        .expect_err("offline must block download");
    assert!(err.to_string().contains("offline"), "unexpected: {err}");
    std::env::remove_var("MOODLE_OFFLINE");
}
