#[path = "integration/mock.rs"]
mod mock;

use mock::{MockConfig, MockMoodle, TOKEN};
use moodle_mcp::{moodle::Moodle, web::upload};

#[tokio::test]
async fn upload_then_save_draft() {
    let m = MockMoodle::healthy().await;
    let c = Moodle::new(m.base(), TOKEN);
    assert_eq!(
        c.upload_draft(0, "a_amin.docx", b"x".to_vec())
            .await
            .unwrap(),
        777
    );
    let dir = std::env::temp_dir().join(format!("mweb-up-{}", std::process::id()));
    std::fs::create_dir_all(dir.join("submit/LMSGI/T")).unwrap();
    let f = dir.join("submit/LMSGI/T/a_amin.docx");
    std::fs::write(&f, b"doc").unwrap();
    let st = upload::save_draft(&c, 7, &[f]).await.unwrap();
    assert_eq!(st["lastattempt"]["submission"]["status"], "draft");
    assert!(upload::resolve(&dir, "submit/LMSGI/T/a_amin.docx")
        .unwrap()
        .ends_with("submit/LMSGI/T/a_amin.docx"));
    assert!(
        upload::resolve(&dir, "a_amin.docx").is_none(),
        "no bare-name guessing"
    );
    assert!(
        upload::resolve(&dir, "submit/LMSGI").is_none(),
        "dirs refused"
    );
    assert!(upload::resolve(&dir, "../etc/passwd").is_none());
    assert!(upload::save_draft(&c, 7, &[]).await.is_err());
}

#[tokio::test]
async fn upload_auth_error() {
    let m = MockMoodle::start(MockConfig {
        unauthorized: true,
        ..Default::default()
    })
    .await;
    let err = Moodle::new(m.base(), TOKEN)
        .upload_draft(0, "a", vec![1])
        .await
        .unwrap_err();
    assert!(
        matches!(err, moodle_mcp::errors::CoreError::Auth(_)),
        "{err}"
    );
}
