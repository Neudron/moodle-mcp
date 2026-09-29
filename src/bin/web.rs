use anyhow::Result;
use moodle_mcp::{moodle::Moodle, web};
use std::sync::Arc;
use std::time::Duration;

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .init();
    let root = Moodle::moodle_root();
    web::env::load(&root)?;
    let port: u16 = web::env::get("WEB_PORT")
        .and_then(|p| p.parse().ok())
        .unwrap_or(8787);
    let db = web::db::Db::open(&std::path::Path::new(&root).join(".moodle/web.db"))?;
    let app = Arc::new(web::App::new(db, Moodle::from_env()?, &root, port));
    let bg = Arc::clone(&app);
    tokio::spawn(async move {
        loop {
            if let Err(e) = web::run_sync(&bg, true).await {
                tracing::warn!(error = %e, "background sync failed");
            }
            tokio::time::sleep(Duration::from_secs(30 * 60)).await;
        }
    });
    let listener = tokio::net::TcpListener::bind(("127.0.0.1", port)).await?;
    if let Some(bind) = web::env::get("WEB_DOCKER_BIND") {
        let l = tokio::net::TcpListener::bind((bind.as_str(), port)).await?;
        let oo = web::office::oo_router().with_state(Arc::clone(&app));
        tokio::spawn(async move { axum::serve(l, oo).await.ok() });
        tracing::info!("onlyoffice callbacks on {bind}:{port}");
    }
    tracing::info!("moodle-web on http://localhost:{port}");
    web::serve_on(listener, app).await
}
