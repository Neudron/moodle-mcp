//! moodle-web: localhost school dashboard (spec 2026-09-29-moodle-web-design.md).
pub mod api;
pub mod apple;
pub mod db;
pub mod env;
pub mod files;
pub mod guard;
pub mod ide;
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

/// One sync pass: Moodle -> DB, README import/rewrite, error kept for the banner.
pub async fn run_sync(app: &App, files: bool) -> Result<Vec<tasks::Event>, CoreError> {
    let _g = app.syncing.lock().await;
    let res = tasks::sync_all(&app.moodle, &app.db, &app.root, files).await;
    let msg = match &res {
        Ok(_) => String::new(),
        Err(CoreError::Auth(_)) => "Token caducat, renova'l".into(),
        Err(e) => format!("Error de sincronització: {e}"),
    };
    app.db.kv_set("last_error", &msg).ok();
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
