use std::os::unix::fs::PermissionsExt;

fn tmpdir(tag: &str) -> std::path::PathBuf {
    let d = std::env::temp_dir().join(format!("mweb-env-{tag}-{}", std::process::id()));
    std::fs::create_dir_all(&d).unwrap();
    d
}

// One test fn: env vars are process-global.
#[test]
fn env_loading_rules() {
    let empty = tmpdir("empty");
    moodle_mcp::web::env::load(empty.to_str().unwrap()).expect("missing .env ok");

    let wide = tmpdir("wide");
    let p = wide.join(".env");
    std::fs::write(&p, "MWEB_T1=a\n").unwrap();
    std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o644)).unwrap();
    let err = moodle_mcp::web::env::load(wide.to_str().unwrap()).unwrap_err();
    assert!(err.to_string().contains("chmod 600"), "{err}");

    let ok = tmpdir("ok");
    let p = ok.join(".env");
    std::fs::write(&p, "MWEB_T2=fromfile\nMWEB_T3=fromfile\nMWEB_EMPTY=\n").unwrap();
    std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o600)).unwrap();
    std::env::set_var("MWEB_T3", "fromenv");
    moodle_mcp::web::env::load(ok.to_str().unwrap()).unwrap();
    assert_eq!(
        moodle_mcp::web::env::get("MWEB_T2").as_deref(),
        Some("fromfile")
    );
    assert_eq!(
        moodle_mcp::web::env::get("MWEB_T3").as_deref(),
        Some("fromenv")
    );
    assert_eq!(moodle_mcp::web::env::get("MWEB_EMPTY"), None);

    std::env::set_var("MOODLE_TOKEN", "tok-from-env-0123456789");
    std::env::set_var("MOODLE_TOKEN_FILE", "/nonexistent/token");
    assert!(moodle_mcp::moodle::Moodle::from_env().is_ok());
    std::env::remove_var("MOODLE_TOKEN");
    assert!(moodle_mcp::moodle::Moodle::from_env().is_err());
}
