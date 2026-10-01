//! Enunciat screenshots through a hand-rolled MCP stdio client (no LLM).
//! Spawns `SHOT_CMD` (default `npx -y open-browser-control`), which drives
//! the user's Firefox; one browser means one shared session behind a lock.
use crate::web::{
    api::{bad, ApiErr, R},
    App,
};
use anyhow::{bail, Context, Result};
use axum::{
    extract::{Path as AxPath, State},
    http::{header, StatusCode},
    response::{IntoResponse, Response},
    routing::get,
    Json, Router,
};
use serde_json::{json, Value};
use std::path::Path;
use std::path::PathBuf;
use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader, Lines};
use tokio::process::{Child, ChildStdin, ChildStdout, Command};
use tokio::sync::{Mutex, MutexGuard};

pub const SELECTOR: &str = "#intro, .activity-description, [role=main]";
pub const LOGIN: &str = "Moodle demana iniciar sessió al navegador";
pub const DEFAULT_CMD: &str = "npx -y open-browser-control";
const CALL_TIMEOUT: Duration = Duration::from_secs(30);
/// The extension connection flaps: "not connected" replies are retried.
const ATTEMPTS: u32 = 5;
const RETRY_WAIT: Duration = Duration::from_secs(2);

/// The one shared MCP session (keyed by command). The server has no close-tab tool and opens a
/// tab per session on first use, so keeping one session alive means one tab in total.
static SESSION: Mutex<Option<(String, Mcp)>> = Mutex::const_new(None);

/// Locked session for `cmd`, spawned on first use. Callers clear it (`*g = None`) on error.
async fn acquire(cmd: &str) -> Result<MutexGuard<'static, Option<(String, Mcp)>>> {
    let mut g = SESSION.lock().await;
    if g.as_ref().is_none_or(|(c, _)| c != cmd) {
        *g = Some((cmd.into(), session(cmd).await?));
    }
    Ok(g)
}

/// Same scheme family, host and port as the Moodle base URL (SSRF guard).
pub fn moodle_only(base: &str, url: &str) -> bool {
    let (Ok(b), Ok(u)) = (reqwest::Url::parse(base), reqwest::Url::parse(url)) else {
        return false;
    };
    matches!(u.scheme(), "http" | "https")
        && u.host_str() == b.host_str()
        && u.port_or_known_default() == b.port_or_known_default()
}

struct Mcp {
    _child: Child, // kill_on_drop
    tx: ChildStdin,
    rx: Lines<BufReader<ChildStdout>>,
    next: u64,
}

impl Mcp {
    fn spawn(cmd: &str) -> Result<Mcp> {
        let mut parts = cmd.split_whitespace();
        let prog = parts.next().context("empty SHOT_CMD")?;
        let mut child = Command::new(prog)
            .args(parts)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .kill_on_drop(true)
            .spawn()
            .with_context(|| format!("cannot start {prog}"))?;
        let tx = child.stdin.take().context("no stdin")?;
        let rx = BufReader::new(child.stdout.take().context("no stdout")?).lines();
        Ok(Mcp {
            _child: child,
            tx,
            rx,
            next: 0,
        })
    }

    async fn write(&mut self, line: String) -> Result<()> {
        self.tx.write_all(line.as_bytes()).await?;
        self.tx.write_all(b"\n").await?;
        Ok(self.tx.flush().await?)
    }

    async fn notify(&mut self, method: &str) -> Result<()> {
        self.write(format!(r#"{{"jsonrpc":"2.0","method":"{method}"}}"#))
            .await
    }

    async fn request(&mut self, method: &str, params: Value) -> Result<Value> {
        self.next += 1;
        let id = self.next;
        self.write(format!(
            r#"{{"jsonrpc":"2.0","id":{id},"method":"{method}","params":{params}}}"#
        ))
        .await?;
        let rx = &mut self.rx;
        tokio::time::timeout(CALL_TIMEOUT, async {
            loop {
                let l = rx.next_line().await?.context("mcp server closed")?;
                let Ok(v) = serde_json::from_str::<Value>(&l) else {
                    continue;
                };
                if v["id"] != id {
                    continue; // notification or log line
                }
                if let Some(e) = v.get("error") {
                    bail!("mcp {method}: {}", e["message"].as_str().unwrap_or("error"));
                }
                return Ok(v["result"].clone());
            }
        })
        .await
        .context("mcp timeout")?
    }

    async fn call(&mut self, name: &str, args: &Value) -> Result<Value> {
        self.request("tools/call", json!({"name": name, "arguments": args}))
            .await
    }

    /// `request` for a tool call: `isError` replies become errors carrying the tool's own text.
    async fn tool(&mut self, name: &str, args: Value) -> Result<Value> {
        for left in (0..ATTEMPTS).rev() {
            let r = self.call(name, &args).await?;
            if r["isError"] != true {
                return Ok(r);
            }
            let why = texts(&r, "\n");
            if left == 0 || !why.contains("not connected") {
                bail!("tool {name} failed: {why}");
            }
            tokio::time::sleep(RETRY_WAIT).await;
        }
        unreachable!()
    }
}

/// Every `content[].text` of a tool reply.
fn texts(r: &Value, sep: &str) -> String {
    r["content"]
        .as_array()
        .map(|a| {
            a.iter()
                .filter_map(|c| c["text"].as_str())
                .collect::<Vec<_>>()
                .join(sep)
        })
        .unwrap_or_default()
}

/// `{screenshot_path}` either at the top level or JSON-encoded in `content[].text`.
fn shot_path(r: &Value) -> Option<String> {
    r["screenshot_path"].as_str().map(String::from).or_else(|| {
        r["content"].as_array()?.iter().find_map(|c| {
            let v: Value = serde_json::from_str(c["text"].as_str()?).ok()?;
            v["screenshot_path"].as_str().map(String::from)
        })
    })
}

/// `pageState.url` of a screenshot reply: the page that was really captured.
fn page_url(r: &Value) -> String {
    r["content"]
        .as_array()
        .and_then(|a| {
            a.iter().find_map(|c| {
                let v: Value = serde_json::from_str(c["text"].as_str()?).ok()?;
                v["pageState"]["url"].as_str().map(String::from)
            })
        })
        .unwrap_or_default()
}

/// Spawn the MCP server and run the handshake.
async fn session(cmd: &str) -> Result<Mcp> {
    let mut m = Mcp::spawn(cmd)?;
    m.request(
        "initialize",
        json!({"protocolVersion": "2024-11-05", "capabilities": {},
               "clientInfo": {"name": "moodle-web", "version": "0"}}),
    )
    .await?;
    m.notify("notifications/initialized").await?;
    Ok(m)
}

/// HTML of `url` for the scanner: fixed tool sequence, no LLM, so page content can never steer
/// the browser. `get_dom` needs `simplified:false` (the default keeps only interactive elements)
/// and replies `{"html":..}`. Google pages block `execute_js`, so the DOM is the only route.
pub async fn page_dom(cmd: &str, url: &str) -> Result<String> {
    let mut g = acquire(cmd).await?;
    let m = &mut g.as_mut().expect("acquired").1;
    let r = async {
        m.tool("browser_navigate", json!({"url": url})).await?;
        m.tool("browser_wait", json!({"selector": "body"}))
            .await
            .ok();
        m.tool("browser_get_dom", json!({"simplified": false}))
            .await
    }
    .await;
    if r.is_err() {
        *g = None;
    }
    let t = texts(&r?, "\n");
    Ok(serde_json::from_str::<Value>(&t)
        .ok()
        .and_then(|v| v["html"].as_str().map(String::from))
        .unwrap_or(t))
}

/// File download through the browser's own login: opening an export URL makes Firefox save the
/// file into `dir`; the path of the new complete file is returned (the caller copies it; the
/// original is never deleted, since ownership is unprovable).
/// The DOM cannot do this (Docs editors draw on a canvas, `get_dom` cuts HTML at 50k chars).
/// Only new files with extension `ext` count ("" = any); a new `.part` extends the wait to 120 s.
/// ponytail: assumes Firefox saves without asking into `dir`; a same-extension file the user
/// downloads meanwhile would be mistaken for ours; with ext "" (Drive, type unknown) any new file
/// is taken. Upgrade path: a dedicated Firefox profile or download dir.
pub async fn download(cmd: &str, url: &str, dir: &Path, ext: &str) -> Result<PathBuf> {
    let (tick, base, cap) = (
        Duration::from_millis(500),
        Duration::from_secs(15),
        Duration::from_secs(120),
    );
    download_with(cmd, url, dir, ext, (tick, base, cap)).await
}

async fn download_with(
    cmd: &str,
    url: &str,
    dir: &Path,
    ext: &str,
    (tick, base, cap): (Duration, Duration, Duration),
) -> Result<PathBuf> {
    let ls = || -> Result<Vec<std::ffi::OsString>> {
        std::fs::read_dir(dir)
            .with_context(|| format!("no downloads dir {}", dir.display()))?
            .map(|e| Ok(e?.file_name()))
            .collect()
    };
    let mut g = acquire(cmd).await?; // held while polling: one download at a time
    let before = ls()?; // after the lock: the previous download's file is already in here
    if let Err(e) = g
        .as_mut()
        .expect("acquired")
        .1
        .tool("browser_navigate", json!({"url": url}))
        .await
    {
        *g = None;
        return Err(e);
    }
    let t0 = std::time::Instant::now();
    loop {
        tokio::time::sleep(tick).await;
        let all: Vec<PathBuf> = ls()?
            .into_iter()
            .filter(|n| !before.contains(n))
            .map(|n| dir.join(n))
            .collect();
        // Firefox writes `name.part` and renames when complete, so a match is a whole file.
        let new = all.iter().find(|p| {
            p.extension()
                .is_some_and(|e| e != "part" && (ext.is_empty() || e.eq_ignore_ascii_case(ext)))
                && p.metadata().is_ok_and(|m| m.len() > 0) // an empty placeholder is not done
        });
        if let Some(p) = new {
            return Ok(p.clone());
        }
        let el = t0.elapsed();
        let busy = all
            .iter()
            .any(|p| p.extension().is_some_and(|e| e == "part"));
        if el >= cap || (el >= base && !busy) {
            break;
        }
    }
    *g = None; // the tab may sit on an error page
    bail!("la baixada no ha arribat a {}", dir.display())
}

/// Open `url` in the shared tab, screenshot the enunciat, save the PNG at `dest`.
pub async fn shoot(cmd: &str, url: &str, dest: &Path) -> Result<()> {
    let mut g = acquire(cmd).await?;
    let m = &mut g.as_mut().expect("acquired").1;
    let res = async {
        m.tool("browser_navigate", json!({"url": url})).await?;
        // ponytail: wait args are unverified against the live server; a failed wait is ignored.
        m.tool("browser_wait", json!({"selector": SELECTOR}))
            .await
            .ok();
        let r = match m
            .tool("browser_screenshot", json!({"selector": SELECTOR}))
            .await
        {
            Ok(r) => r,
            Err(_) => {
                m.tool("browser_screenshot", json!({"fullPage": true}))
                    .await?
            }
        };
        if page_url(&r).contains("/login/") {
            bail!(LOGIN);
        }
        let src = shot_path(&r).context("no screenshot_path in reply")?;
        if let Some(dir) = dest.parent() {
            tokio::fs::create_dir_all(dir).await?;
        }
        tokio::fs::copy(&src, dest).await?;
        Ok(())
    }
    .await;
    if res.is_err() {
        *g = None;
    }
    res
}

pub fn shot_file(root: &str, id: &str) -> PathBuf {
    let n: String = id
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' {
                c
            } else {
                '_'
            }
        })
        .collect();
    Path::new(root)
        .join(".moodle/shots")
        .join(format!("{n}.png"))
}

pub fn router() -> Router<Arc<App>> {
    Router::new().route("/api/shot/{id}", get(get_shot).post(make_shot))
}

async fn get_shot(State(app): State<Arc<App>>, AxPath(id): AxPath<String>) -> R<Response> {
    let bytes = tokio::fs::read(shot_file(&app.root, &id))
        .await
        .map_err(|_| ApiErr(StatusCode::NOT_FOUND, "no shot".into()))?;
    Ok((
        [
            (header::CONTENT_TYPE, "image/png"),
            (header::X_CONTENT_TYPE_OPTIONS, "nosniff"),
            (header::CACHE_CONTROL, "no-store"),
        ],
        bytes,
    )
        .into_response())
}

async fn make_shot(State(app): State<Arc<App>>, AxPath(id): AxPath<String>) -> R<Json<Value>> {
    let t = app
        .db
        .task(&id)?
        .ok_or_else(|| ApiErr(StatusCode::NOT_FOUND, "no such task".into()))?;
    if !moodle_only(app.moodle.base(), &t.url) {
        return Err(bad("url fora de Moodle"));
    }
    const HINT: &str =
        "Firefox no té sessió de Moodle: inicia sessió a Moodle al Firefox i torna-ho a provar";
    if crate::firefox::session_cookie(&crate::firefox::root(), None).is_err() {
        return Err(ApiErr(StatusCode::CONFLICT, HINT.into()));
    }
    let cmd = std::env::var("SHOT_CMD").unwrap_or_else(|_| DEFAULT_CMD.into());
    shoot(&cmd, &t.url, &shot_file(&app.root, &id))
        .await
        .map_err(|e| {
            let m = e.to_string();
            if m.contains(LOGIN) {
                return ApiErr(StatusCode::CONFLICT, HINT.into());
            }
            ApiErr(StatusCode::BAD_GATEWAY, crate::errors::redact_token(&m))
        })?;
    Ok(Json(json!({"ok": true})))
}

#[cfg(test)]
mod tests {
    use super::*;

    const FAKE: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/fake_mcp.sh");

    fn tmp(tag: &str) -> std::path::PathBuf {
        let d = std::env::temp_dir().join(format!("shots-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn only_moodle_host() {
        let b = "https://aula.example.cat";
        assert!(moodle_only(
            b,
            "https://aula.example.cat/mod/assign/view.php?id=1"
        ));
        assert!(!moodle_only(b, "https://evil.example/x"));
        assert!(!moodle_only(b, "https://aula.example.cat.evil.example/x"));
        assert!(!moodle_only(b, "https://aula.example.cat:8443/x"));
        assert!(!moodle_only(b, "http://aula.example.cat@evil.example/x"));
        assert!(!moodle_only(b, "file:///etc/passwd"));
        assert!(!moodle_only(b, ""));
    }

    #[tokio::test]
    async fn shoot_runs_the_tool_sequence_and_copies_png() {
        let d = tmp("ok");
        let (png, log, dest) = (d.join("s.png"), d.join("log"), d.join("out/x.png"));
        let cmd = format!("sh {FAKE} {} {}", png.display(), log.display());
        shoot(&cmd, "http://127.0.0.1:1/mod/assign/view.php?id=1", &dest)
            .await
            .unwrap();
        assert!(std::fs::read(&dest).unwrap().starts_with(b"\x89PNG"));
        let calls: Vec<String> = std::fs::read_to_string(&log)
            .unwrap()
            .lines()
            .map(String::from)
            .collect();
        assert_eq!(
            calls,
            ["browser_navigate", "browser_wait", "browser_screenshot"]
        );
        shoot_falls_back_to_full_page().await;
        shoot_refuses_a_login_page_and_keeps_the_old_shot().await;
    }

    // same body as above: FAKE_MCP_FAIL_SELECTOR is process-global and would leak into the parallel test
    async fn shoot_falls_back_to_full_page() {
        let d = tmp("fb");
        let (png, log, dest) = (d.join("s.png"), d.join("log"), d.join("x.png"));
        std::env::set_var("FAKE_MCP_FAIL_SELECTOR", "1");
        let cmd = format!("sh {FAKE} {} {}", png.display(), log.display());
        let r = shoot(&cmd, "http://127.0.0.1:1/x", &dest).await;
        std::env::remove_var("FAKE_MCP_FAIL_SELECTOR");
        r.unwrap();
        assert!(dest.exists());
        let n = std::fs::read_to_string(&log)
            .unwrap()
            .matches("browser_screenshot")
            .count();
        assert_eq!(n, 2, "selector attempt + fullPage attempt");
    }

    // called from the first test: FAKE_MCP_URL is process-global
    async fn shoot_refuses_a_login_page_and_keeps_the_old_shot() {
        let d = tmp("login");
        let (png, log, dest) = (d.join("s.png"), d.join("log"), d.join("x.png"));
        std::fs::write(&dest, "good").unwrap();
        std::env::set_var("FAKE_MCP_URL", "https://aula.example.cat/login/index.php");
        let cmd = format!("sh {FAKE} {} {}", png.display(), log.display());
        let r = shoot(&cmd, "http://127.0.0.1:1/x", &dest).await;
        std::env::remove_var("FAKE_MCP_URL");
        assert!(r.unwrap_err().to_string().contains("iniciar sessió"));
        assert_eq!(std::fs::read(&dest).unwrap(), b"good");
    }

    #[tokio::test]
    async fn shoot_errors_when_server_dies_or_is_missing() {
        let d = tmp("bad");
        let dest = d.join("x.png");
        assert!(shoot("true", "http://127.0.0.1:1/x", &dest).await.is_err());
        assert!(shoot("/nonexistent/mcp-bin", "http://127.0.0.1:1/x", &dest)
            .await
            .is_err());
        assert!(shoot("  ", "http://127.0.0.1:1/x", &dest).await.is_err());
        assert!(!dest.exists());
    }

    #[tokio::test]
    async fn tool_errors_carry_the_tool_text() {
        let d = tmp("err");
        std::fs::write(d.join("flap"), "").unwrap(); // flap file present: no "not connected" reply
        let cmd = format!(
            "sh {FAKE} {} {} {} Error:_boom",
            d.join("s.png").display(),
            d.join("log").display(),
            d.join("flap").display()
        );
        let e = shoot(&cmd, "http://127.0.0.1:1/x", &d.join("x.png"))
            .await
            .unwrap_err();
        assert!(
            e.to_string()
                .contains("tool browser_navigate failed: Error:_boom"),
            "{e}"
        );
    }

    #[tokio::test]
    async fn not_connected_is_retried_and_dom_html_is_unwrapped() {
        let d = tmp("flap");
        let cmd = format!(
            "sh {FAKE} {} {} {}",
            d.join("s.png").display(),
            d.join("log").display(),
            d.join("flap").display()
        );
        std::env::set_var("FAKE_MCP_DOM", r#"{\"html\":\"<p>hola</p>\"}"#);
        let t = page_dom(&cmd, "https://docs.google.com/document/d/x/mobilebasic").await;
        std::env::remove_var("FAKE_MCP_DOM");
        assert_eq!(t.unwrap(), "<p>hola</p>");
        assert!(d.join("flap").exists());
    }

    #[tokio::test]
    async fn download_returns_the_new_whole_file_of_any_type() {
        let d = tmp("dl");
        let dir = d.join("dl");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("old.pdf"), "no").unwrap(); // already there: not ours
        let (p, q) = (dir.join("doc.docx.part"), dir.join("doc.docx"));
        let (q2, log) = (q.clone(), d.join("log"));
        tokio::spawn(async move {
            // wait until download() really navigated (it may queue behind the shared SESSION lock)
            for _ in 0..500 {
                if std::fs::read_to_string(&log).is_ok_and(|l| l.contains("browser_navigate")) {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
            tokio::time::sleep(Duration::from_millis(300)).await;
            std::fs::write(&p, "PK").unwrap();
            std::fs::rename(&p, &q2).unwrap();
            let done = std::fs::read(&q2).unwrap();
            std::fs::write(&q2, "").unwrap(); // a zero-byte placeholder is not whole yet
            tokio::time::sleep(Duration::from_millis(1200)).await;
            std::fs::write(&q2, done).unwrap();
        });
        let cmd = format!(
            "sh {FAKE} {} {}",
            d.join("s.png").display(),
            d.join("log").display()
        );
        let got = download(
            &cmd,
            "https://docs.google.com/document/d/x/export?format=docx",
            &dir,
            "docx",
        )
        .await
        .unwrap();
        assert_eq!(got, q);
        assert_eq!(std::fs::read(&q).unwrap(), b"PK");
        assert!(dir.join("old.pdf").exists());
        let e = download(&cmd, "https://x", &d.join("missing"), "")
            .await
            .unwrap_err();
        assert!(e.to_string().contains("no downloads dir"), "{e}");
    }

    #[tokio::test]
    async fn download_ignores_foreign_files_and_waits_for_part() {
        let d = tmp("dl2");
        let dir = d.join("dl");
        std::fs::create_dir_all(&dir).unwrap();
        let (dir2, log) = (dir.clone(), d.join("log"));
        tokio::spawn(async move {
            for _ in 0..500 {
                if std::fs::read_to_string(&log).is_ok_and(|l| l.contains("browser_navigate")) {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
            std::fs::write(dir2.join("user.pdf"), "mine").unwrap();
            std::fs::write(dir2.join("x.docx.part"), "PK").unwrap();
            tokio::time::sleep(Duration::from_millis(900)).await; // past the base timeout
            std::fs::rename(dir2.join("x.docx.part"), dir2.join("x.docx")).unwrap();
        });
        let cmd = format!(
            "sh {FAKE} {} {}",
            d.join("s.png").display(),
            d.join("log").display()
        );
        let t = (
            Duration::from_millis(50),
            Duration::from_millis(400),
            Duration::from_secs(10),
        );
        let got = download_with(&cmd, "https://x", &dir, "docx", t)
            .await
            .unwrap();
        assert_eq!(got, dir.join("x.docx"));
        assert!(dir.join("user.pdf").exists());
    }

    #[test]
    fn shot_file_is_sanitised() {
        let p = shot_file("/r", "assign:7/../x");
        assert_eq!(
            p,
            std::path::PathBuf::from("/r/.moodle/shots/assign_7____x.png")
        );
    }
}
