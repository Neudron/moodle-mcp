//! Moodle token renewal: parsers, Firefox session reader, `refresh_token`
//! against a local axum mock. Nothing live.

use axum::{
    extract::{Query, State},
    http::{header, HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    routing::{get, post},
    Form, Router,
};
use moodle_mcp::errors::CoreError;
use moodle_mcp::firefox::{
    b64_decode, default_profile, moodle_session_cookie, mozlz4_decode, FfError,
};
use moodle_mcp::moodle::{parse_launch, Moodle, Renew};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

const OLD: &str = "oldtoken0123456789abcdef";
const NEW: &str = "newtoken0123456789abcdef";
const COOKIE: &str = "sesscookievalue9999";

fn b64(data: &[u8]) -> String {
    const A: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut s = String::new();
    for ch in data.chunks(3) {
        let n = ch
            .iter()
            .enumerate()
            .fold(0u32, |a, (i, b)| a | u32::from(*b) << (16 - 8 * i));
        for i in 0..=ch.len() {
            s.push(A[(n >> (18 - 6 * i) & 63) as usize] as char);
        }
        s.extend(std::iter::repeat_n('=', 3 - ch.len()));
    }
    s
}

fn link(payload: &str) -> String {
    format!("moodlemobile://token={}", b64(payload.as_bytes()))
}

// ---- P2.1 parser ----

#[test]
fn parse_launch_valid() {
    assert_eq!(parse_launch(&link(&format!("site:::{NEW}"))).unwrap(), NEW);
    assert_eq!(
        parse_launch(&link(&format!("site:::{NEW}:::priv"))).unwrap(),
        NEW
    );
    let html = format!(r#"<a href="{}">open</a>"#, link(&format!("s:::{NEW}")));
    assert_eq!(parse_launch(&html).unwrap(), NEW);
}

#[test]
fn parse_launch_rejects_bad() {
    for bad in [
        "".to_string(),
        "moodlemobile://token=!!!notb64".into(),
        "moodlemobile://token=%%%".into(),
        link("no-separator-here-at-all"),
        link("site:::short"),
        link("site:::"),
        "https://example.com".into(),
        "moodlemobile://token=".into(),
    ] {
        assert!(parse_launch(&bad).is_err(), "must reject {bad:?}");
    }
}

#[test]
fn b64_roundtrip_and_reject() {
    for d in [&b""[..], b"a", b"ab", b"abc", b"abcd", &[255, 0, 254, 1]] {
        assert_eq!(b64_decode(&b64(d)).unwrap(), d);
    }
    assert_eq!(b64_decode("a b"), None);
}

// ---- P2.2 mozlz4 ----

fn mozlz4_literal(data: &[u8]) -> Vec<u8> {
    let mut out = b"mozLz40\0".to_vec();
    out.extend((data.len() as u32).to_le_bytes());
    let n = data.len();
    if n < 15 {
        out.push((n as u8) << 4);
    } else {
        out.push(0xF0);
        let mut r = n - 15;
        while r >= 255 {
            out.push(255);
            r -= 255;
        }
        out.push(r as u8);
    }
    out.extend(data);
    out
}

#[test]
fn mozlz4_decodes_match_fixture() {
    // "abc" literals, then a 12-byte match at offset 3, then a literal-free end.
    let mut blob = b"mozLz40\0".to_vec();
    blob.extend(15u32.to_le_bytes());
    blob.extend([0x38, b'a', b'b', b'c', 3, 0, 0x00]);
    assert_eq!(mozlz4_decode(&blob).unwrap(), b"abcabcabcabcabc");
}

#[test]
fn mozlz4_roundtrip_long_literal() {
    let data: Vec<u8> = (0..700u32).map(|i| (i % 251) as u8).collect();
    assert_eq!(mozlz4_decode(&mozlz4_literal(&data)).unwrap(), data);
}

#[test]
fn mozlz4_rejects_bad_input() {
    let good = mozlz4_literal(b"hello world, hello");
    let mut magic = good.clone();
    magic[0] = b'x';
    assert!(mozlz4_decode(&magic).is_err());
    assert!(mozlz4_decode(&good[..good.len() - 3]).is_err());
    assert!(mozlz4_decode(&good[..6]).is_err());
    let mut huge = good.clone();
    huge[8..12].copy_from_slice(&u32::MAX.to_le_bytes());
    assert!(mozlz4_decode(&huge).is_err());
    let mut off0 = b"mozLz40\0".to_vec();
    off0.extend(8u32.to_le_bytes());
    off0.extend([0x14, b'a', 0, 0]);
    assert!(mozlz4_decode(&off0).is_err());
}

// ---- P2.3 session cookie ----

fn session_json(host: &str, name: &str) -> Vec<u8> {
    format!(
        r#"{{"windows":[{{"cookies":[{{"host":"other.org","name":"MoodleSessionx","value":"nope"}},
        {{"host":"{host}","name":"{name}","value":"{COOKIE}"}}]}}]}}"#
    )
    .into_bytes()
}

#[test]
fn session_cookie_picks_moodle_host() {
    let j = session_json(".educaciodigital.cat", "MoodleSessioniespalamos");
    assert_eq!(
        moodle_session_cookie(&j).unwrap(),
        ("MoodleSessioniespalamos".into(), COOKIE.into())
    );
    let top =
        br#"{"cookies":[{"host":"educaciodigital.cat","name":"MoodleSessionA","value":"v"}]}"#;
    assert_eq!(moodle_session_cookie(top).unwrap().1, "v");
}

#[test]
fn session_cookie_missing_is_nosession_without_value() {
    let j = session_json("other.org", "MoodleSessionz");
    assert_eq!(moodle_session_cookie(&j), Err(FfError::NoSession));
    let j = session_json(".educaciodigital.cat", "OtherCookie");
    assert_eq!(moodle_session_cookie(&j), Err(FfError::NoSession));
    let e = moodle_session_cookie(b"not json").unwrap_err();
    assert!(!e.to_string().contains("not json"));
}

fn tmp(tag: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("moodle_auth_{tag}_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
}

fn profile(root: &Path, cookie_json: Option<Vec<u8>>) {
    std::fs::write(
        root.join("installs.ini"),
        "[ABC]\nDefault=Profiles/x.default\nLocked=1\n",
    )
    .unwrap();
    let sb = root.join("Profiles/x.default/sessionstore-backups");
    std::fs::create_dir_all(&sb).unwrap();
    if let Some(j) = cookie_json {
        std::fs::write(sb.join("recovery.jsonlz4"), mozlz4_literal(&j)).unwrap();
    }
}

#[test]
fn profile_discovery_and_missing_file() {
    let root = tmp("prof");
    assert_eq!(default_profile(&root), Err(FfError::NoSession));
    profile(&root, None);
    assert!(default_profile(&root)
        .unwrap()
        .ends_with("Profiles/x.default"));
    assert_eq!(
        moodle_mcp::firefox::session_cookie(&root),
        Err(FfError::NoSession)
    );
}

// ---- P2.4 token holder ----

#[derive(Clone, Copy)]
enum Launch {
    Redirect,
    Confirm,
    ConfirmNoLink,
    Forbidden,
    ToLogin,
}

struct Mock {
    launch: Launch,
    valid: Vec<&'static str>,
    seen: Mutex<Vec<String>>,
    launch_cookie: Mutex<Option<String>>,
}

type S = Arc<Mock>;

async fn ws(State(m): State<S>, Form(f): Form<HashMap<String, String>>) -> Response {
    let t = f.get("wstoken").cloned().unwrap_or_default();
    m.seen.lock().unwrap().push(t.clone());
    if m.valid.contains(&t.as_str()) {
        axum::Json(serde_json::json!({"userid": 1, "policyagreed": 1})).into_response()
    } else {
        axum::Json(serde_json::json!({"exception":"moodle_exception","errorcode":"invalidtoken","message":"Invalid token"})).into_response()
    }
}

async fn launch(
    State(m): State<S>,
    h: HeaderMap,
    Query(q): Query<HashMap<String, String>>,
) -> Response {
    assert_eq!(
        q.get("service").map(String::as_str),
        Some("moodle_mobile_app")
    );
    assert!(q.get("passport").is_some_and(|p| !p.is_empty()));
    *m.launch_cookie.lock().unwrap() = h
        .get(header::COOKIE)
        .and_then(|v| v.to_str().ok())
        .map(String::from);
    let l = link(&format!("site:::{NEW}"));
    match m.launch {
        Launch::Redirect => (StatusCode::FOUND, [(header::LOCATION, l)]).into_response(),
        Launch::Confirm => format!("<html><a href=\"{l}\">go</a></html>").into_response(),
        Launch::ConfirmNoLink => "<html>please confirm in browser</html>".into_response(),
        Launch::Forbidden => StatusCode::FORBIDDEN.into_response(),
        Launch::ToLogin => {
            (StatusCode::FOUND, [(header::LOCATION, "/login/index.php")]).into_response()
        }
    }
}

async fn serve(launch_mode: Launch, valid: Vec<&'static str>) -> (String, S) {
    let m = Arc::new(Mock {
        launch: launch_mode,
        valid,
        seen: Mutex::new(vec![]),
        launch_cookie: Mutex::new(None),
    });
    let app = Router::new()
        .route("/webservice/rest/server.php", post(ws))
        .route("/admin/tool/mobile/launch.php", get(launch))
        .with_state(m.clone());
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", l.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(l, app).await.unwrap() });
    (base, m)
}

#[tokio::test]
async fn set_token_changes_next_request_and_clones() {
    let (base, m) = serve(Launch::Redirect, vec![NEW]).await;
    let c = Moodle::new(&base, OLD);
    let twin = c.clone();
    assert!(c.site_info().await.is_err());
    c.set_token(NEW);
    assert!(twin.site_info().await.is_ok());
    assert_eq!(*m.seen.lock().unwrap(), vec![OLD, NEW]);
}

// ---- P2.5 refresh_token ----

fn ready(tag: &str, with_cookie: bool) -> (PathBuf, PathBuf) {
    let root = tmp(tag);
    profile(
        &root,
        with_cookie.then(|| session_json(".educaciodigital.cat", "MoodleSessionx")),
    );
    let tf = root.join("token");
    std::fs::write(&tf, OLD).unwrap();
    (root, tf)
}

#[tokio::test]
async fn refresh_redirect_success_writes_file_0600_and_swaps() {
    use std::os::unix::fs::PermissionsExt;
    let (base, m) = serve(Launch::Redirect, vec![OLD, NEW]).await;
    let (root, tf) = ready("ok", true);
    let c = Moodle::new(&base, OLD);
    assert_eq!(c.refresh_token_at(&root, &tf).await, Renew::Ok);
    assert_eq!(std::fs::read_to_string(&tf).unwrap(), NEW);
    assert_eq!(
        std::fs::metadata(&tf).unwrap().permissions().mode() & 0o777,
        0o600
    );
    assert_eq!(
        m.launch_cookie.lock().unwrap().as_deref(),
        Some(format!("MoodleSessionx={COOKIE}").as_str())
    );
    m.seen.lock().unwrap().clear();
    c.site_info().await.unwrap();
    assert_eq!(*m.seen.lock().unwrap(), vec![NEW]);
}

#[tokio::test]
async fn refresh_confirmation_page_with_link_succeeds() {
    let (base, _m) = serve(Launch::Confirm, vec![NEW]).await;
    let (root, tf) = ready("confirm", true);
    let c = Moodle::new(&base, OLD);
    assert_eq!(c.refresh_token_at(&root, &tf).await, Renew::Ok);
    assert_eq!(std::fs::read_to_string(&tf).unwrap(), NEW);
}

#[tokio::test]
async fn refresh_confirmation_page_without_link_needs_browser() {
    let (base, _m) = serve(Launch::ConfirmNoLink, vec![]).await;
    let (root, tf) = ready("nolink", true);
    let c = Moodle::new(&base, OLD);
    assert_eq!(c.refresh_token_at(&root, &tf).await, Renew::NeedsBrowser);
    assert_eq!(std::fs::read_to_string(&tf).unwrap(), OLD);
}

#[tokio::test]
async fn refresh_no_session_cases() {
    for (tag, mode, cookie) in [
        ("403", Launch::Forbidden, true),
        ("login", Launch::ToLogin, true),
        ("nocookie", Launch::Redirect, false),
    ] {
        let (base, _m) = serve(mode, vec![NEW]).await;
        let (root, tf) = ready(tag, cookie);
        let c = Moodle::new(&base, OLD);
        assert_eq!(
            c.refresh_token_at(&root, &tf).await,
            Renew::NoSession,
            "{tag}"
        );
        assert_eq!(std::fs::read_to_string(&tf).unwrap(), OLD);
    }
}

#[tokio::test]
async fn refresh_validation_failure_keeps_old_and_leaks_nothing() {
    let (base, m) = serve(Launch::Redirect, vec![OLD]).await; // NEW rejected
    let (root, tf) = ready("reject", true);
    let c = Moodle::new(&base, OLD);
    let r = c.refresh_token_at(&root, &tf).await;
    let Renew::Failed(msg) = r else {
        panic!("expected Failed, got {r:?}")
    };
    assert!(!msg.contains(NEW) && !msg.contains(OLD) && !msg.contains(COOKIE));
    assert_eq!(std::fs::read_to_string(&tf).unwrap(), OLD);
    m.seen.lock().unwrap().clear();
    c.site_info().await.unwrap();
    assert_eq!(*m.seen.lock().unwrap(), vec![OLD]);
}

#[test]
fn core_error_type_used() {
    // parse_launch errors are CoreError::Invalid and carry no input.
    let Err(CoreError::Invalid(m)) = parse_launch("moodlemobile://token=@@") else {
        panic!("expected Invalid")
    };
    assert!(!m.contains("@@"));
}

// ---- P3 wiring ----

/// Env vars are process-global: serialize the tests that set them.
static ENV: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

#[tokio::test]
async fn reload_token_adopts_valid_file_token_only() {
    let _e = ENV.lock().await;
    let (base, _m) = serve(Launch::Redirect, vec![NEW]).await;
    let (_root, tf) = ready("reload", false);
    let c = Moodle::new(&base, OLD);
    std::env::set_var("MOODLE_TOKEN_FILE", &tf);
    assert!(!c.reload_token().await, "same token in file: nothing to do");
    std::fs::write(&tf, "junkjunkjunkjunkjunk").unwrap();
    assert!(
        !c.reload_token().await,
        "rejected token must not be adopted"
    );
    std::fs::write(&tf, NEW).unwrap();
    assert!(c.reload_token().await);
    assert!(c.site_info().await.is_ok());
}

#[tokio::test]
async fn site_info_renewing_uses_reload_before_launch() {
    let _e = ENV.lock().await;
    let (base, m) = serve(Launch::Redirect, vec![NEW]).await;
    let (root, tf) = ready("bin", true);
    std::fs::write(&tf, NEW).unwrap(); // another process already renewed
    std::env::set_var("MOODLE_TOKEN_FILE", &tf);
    std::env::set_var("MOODLE_FIREFOX_ROOT", &root);
    let c = Moodle::new(&base, OLD);
    assert!(c.site_info_renewing().await.is_ok());
    assert!(m.launch_cookie.lock().unwrap().is_none(), "no launch call");
}

#[tokio::test]
async fn run_sync_style_renewal_once_per_hour() {
    let _e = ENV.lock().await;
    use moodle_mcp::web::{db::Db, renewing, App};
    use std::sync::atomic::{AtomicUsize, Ordering::SeqCst};
    let (base, m) = serve(Launch::Redirect, vec![NEW]).await;
    let (root, tf) = ready("web", true);
    std::env::set_var("MOODLE_TOKEN_FILE", &tf);
    std::env::set_var("MOODLE_FIREFOX_ROOT", &root);
    let app = App::new(Db::memory().unwrap(), Moodle::new(&base, OLD), "/tmp", 0);
    let calls = AtomicUsize::new(0);
    let op = || async {
        calls.fetch_add(1, SeqCst);
        app.moodle
            .call_with_retry("core_webservice_get_site_info", &[])
            .await
    };
    assert!(renewing(&app, op).await.is_ok());
    assert_eq!(calls.load(SeqCst), 2, "failed once, retried once");
    assert_eq!(std::fs::read_to_string(&tf).unwrap(), NEW);
    assert_eq!(app.db.kv_get("auth_state").unwrap().as_deref(), Some("ok"));
    // token dies again within the hour: no second renewal
    app.moodle.set_token(OLD);
    *m.launch_cookie.lock().unwrap() = None;
    std::fs::write(&tf, OLD).unwrap();
    let r = renewing(&app, op).await;
    assert!(matches!(r, Err(CoreError::Auth(_))));
    assert!(m.launch_cookie.lock().unwrap().is_none());
    assert_eq!(
        app.db.kv_get("auth_state").unwrap().as_deref(),
        Some("expired")
    );
    let errs = app.db.recent_errors(5).unwrap();
    assert!(errs.iter().any(|e| e.2 == "moodle-auth"));
    assert!(errs
        .iter()
        .all(|e| !e.3.contains(NEW) && !e.3.contains(OLD)));
    // a different outcome maps to its own state
    let (base2, _m2) = serve(Launch::ConfirmNoLink, vec![]).await;
    let app2 = App::new(Db::memory().unwrap(), Moodle::new(&base2, OLD), "/tmp", 0);
    let op2 = || async {
        app2.moodle
            .call_with_retry("core_webservice_get_site_info", &[])
            .await
    };
    assert!(renewing(&app2, op2).await.is_err());
    assert_eq!(
        app2.db.kv_get("auth_state").unwrap().as_deref(),
        Some("needs-browser")
    );
}
