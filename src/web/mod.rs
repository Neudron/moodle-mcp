//! moodle-web: localhost school dashboard (spec 2026-09-29-moodle-web-design.md).
pub mod api;
pub mod apple;
pub mod db;
pub mod env;
pub mod files;
pub mod folders;
pub mod guard;
pub mod ide;
pub mod jev;
pub mod lan;
pub mod office;
pub mod organize;
pub mod preview;
pub mod readme;
pub mod scan;
pub mod shots;
pub mod tasks;
pub mod upload;

use crate::errors::CoreError;
use crate::moodle::Moodle;
use std::sync::Arc;

pub struct App {
    pub db: db::Db,
    pub moodle: Moodle,
    pub root: String,
    pub port: u16,
    pub syncing: tokio::sync::Mutex<()>,
}

impl App {
    pub fn new(db: db::Db, moodle: Moodle, root: &str, port: u16) -> Self {
        Self {
            db,
            moodle,
            root: root.to_string(),
            port,
            syncing: tokio::sync::Mutex::new(()),
        }
    }
}

pub fn now() -> i64 {
    chrono::Utc::now().timestamp()
}

pub fn renew_state(r: &crate::moodle::Renew) -> &'static str {
    use crate::moodle::Renew::*;
    match r {
        Ok => "ok",
        NeedsBrowser => "needs-browser",
        NoSession => "no-session",
        Failed(_) => "expired",
    }
}

/// Renew the Moodle token unless one was tried less than `gap` seconds ago
/// (Err = seconds to wait). Records kv `auth_state` and an error_log row.
pub async fn renew_now(app: &App, gap: i64) -> Result<crate::moodle::Renew, i64> {
    let n = now();
    let last = app
        .db
        .kv_get("auth_renew_at")
        .ok()
        .flatten()
        .and_then(|v| v.parse::<i64>().ok())
        .unwrap_or(0);
    if n - last < gap {
        return Err(gap - (n - last));
    }
    app.db.kv_set("auth_renew_at", &n.to_string()).ok();
    let r = app.moodle.recover_auth().await;
    let state = renew_state(&r);
    app.db.kv_set("auth_state", state).ok();
    app.db
        .log_error("moodle", "moodle-auth", &format!("renovació: {state}"))
        .ok();
    Ok(r)
}

/// Run `op`; on Auth, renew the token (at most hourly) and retry once.
pub async fn renewing<T, F, Fut>(app: &App, op: F) -> Result<T, CoreError>
where
    F: Fn() -> Fut,
    Fut: std::future::Future<Output = Result<T, CoreError>>,
{
    let r = op().await;
    if !matches!(r, Err(CoreError::Auth(_))) {
        if r.is_ok() {
            app.db.kv_set("auth_state", "ok").ok();
        }
        return r;
    }
    match renew_now(app, 3600).await {
        Ok(crate::moodle::Renew::Ok) => op().await,
        Ok(_) => r,
        Err(_) => {
            if app
                .db
                .kv_get("auth_state")
                .ok()
                .flatten()
                .as_deref()
                .unwrap_or("ok")
                == "ok"
            {
                app.db.kv_set("auth_state", "expired").ok();
            }
            r
        }
    }
}

/// One sync pass: Moodle -> DB, README import/rewrite, error kept for the banner.
pub async fn run_sync(app: &App, files: bool) -> Result<Vec<tasks::Event>, CoreError> {
    let _g = app.syncing.lock().await;
    let res = renewing(app, || {
        tasks::sync_all(&app.moodle, &app.db, &app.root, files)
    })
    .await;
    let msg = match &res {
        Ok(_) => String::new(),
        Err(CoreError::Auth(_)) => "Token caducat, renova'l".into(),
        Err(e) => format!("Error de sincronització: {e}"),
    };
    app.db.kv_set("last_error", &msg).ok();
    if let Err(e) = &res {
        let kind = if matches!(e, CoreError::Auth(_)) {
            "moodle-auth"
        } else {
            "moodle"
        };
        app.db.log_error("moodle", kind, &msg).ok();
    }
    if res.is_ok() {
        readme::import_once(&app.db, &app.root).map_err(CoreError::Other)?;
        readme::write(&app.db, &app.root).map_err(CoreError::Other)?;
    }
    res
}

pub fn router(app: Arc<App>) -> axum::Router {
    api::routes()
        .layer(axum::middleware::from_fn_with_state(
            Arc::clone(&app),
            api::guard_mw,
        ))
        .with_state(app)
}

pub async fn serve_on(listener: tokio::net::TcpListener, app: Arc<App>) -> anyhow::Result<()> {
    axum::serve(listener, router(app)).await?;
    Ok(())
}

/// Rename `path` to `path.1` when it exceeds `max` bytes (one generation kept).
pub fn rotate_log(path: &std::path::Path, max: u64) {
    if std::fs::metadata(path).is_ok_and(|m| m.len() > max) {
        let mut to = path.as_os_str().to_owned();
        to.push(".1");
        let _ = std::fs::rename(path, to);
    }
}

/// Like `serve_on`, but stops accepting when `stop` flips to true.
pub async fn serve_until(
    listener: tokio::net::TcpListener,
    app: Arc<App>,
    mut stop: tokio::sync::watch::Receiver<bool>,
) -> anyhow::Result<()> {
    axum::serve(listener, router(app))
        .with_graceful_shutdown(async move {
            let _ = stop.wait_for(|s| *s).await;
        })
        .await?;
    Ok(())
}
