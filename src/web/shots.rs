//! Enunciat screenshots through a hand-rolled MCP stdio client (no LLM).
//! Spawns `SHOT_CMD` (default `npx -y open-browser-control`), which drives
//! the user's Firefox; one browser means one global lock.
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
use tokio::sync::Mutex;

pub const SELECTOR: &str = "#intro, .activity-description, [role=main]";
pub const DEFAULT_CMD: &str = "npx -y open-browser-control";
const CALL_TIMEOUT: Duration = Duration::from_secs(30);

static LOCK: Mutex<()> = Mutex::const_new(());

/// The one-Firefox lock; the WP3 browser agent takes it too.
pub fn lock() -> &'static Mutex<()> {
    &LOCK
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

    async fn tool(&mut self, name: &str, args: Value) -> Result<Value> {
        let r = self
            .request("tools/call", json!({"name": name, "arguments": args}))
            .await?;
        if r["isError"] == true {
            bail!("tool {name} failed");
        }
        Ok(r)
    }
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

/// Open `url` in a fresh tab, screenshot the enunciat, save the PNG at `dest`.
pub async fn shoot(cmd: &str, url: &str, dest: &Path) -> Result<()> {
    let _g = LOCK.lock().await;
    let mut m = Mcp::spawn(cmd)?;
    m.request(
        "initialize",
        json!({"protocolVersion": "2024-11-05", "capabilities": {},
               "clientInfo": {"name": "moodle-web", "version": "0"}}),
    )
    .await?;
    m.notify("notifications/initialized").await?;
    m.tool("browser_new_tab", json!({"url": url})).await?;
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
        let src = shot_path(&r).context("no screenshot_path in reply")?;
        if let Some(dir) = dest.parent() {
            tokio::fs::create_dir_all(dir).await?;
        }
        tokio::fs::copy(&src, dest).await?;
        Ok(())
    }
    .await;
    m.tool("browser_close_tab", json!({})).await.ok();
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
    let cmd = std::env::var("SHOT_CMD").unwrap_or_else(|_| DEFAULT_CMD.into());
    shoot(&cmd, &t.url, &shot_file(&app.root, &id))
        .await
        .map_err(|e| {
            ApiErr(
                StatusCode::BAD_GATEWAY,
                crate::errors::redact_token(&e.to_string()),
            )
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
            [
                "browser_new_tab",
                "browser_navigate",
                "browser_wait",
                "browser_screenshot",
                "browser_close_tab"
            ]
        );
        shoot_falls_back_to_full_page().await;
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

    #[test]
    fn shot_file_is_sanitised() {
        let p = shot_file("/r", "assign:7/../x");
        assert_eq!(
            p,
            std::path::PathBuf::from("/r/.moodle/shots/assign_7____x.png")
        );
    }
}
