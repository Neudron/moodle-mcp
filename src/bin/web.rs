use anyhow::Result;
use moodle_mcp::{moodle::Moodle, web};
use std::sync::Arc;
use std::time::Duration;

#[tokio::main]
async fn main() -> Result<()> {
    let root = Moodle::moodle_root();
    web::env::load(&root)?;
    let log = std::path::Path::new(&root).join(".moodle/web-server.log");
    web::rotate_log(&log, 1 << 20);
    use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
    let file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .mode(0o600)
        .open(&log)?;
    file.set_permissions(std::fs::Permissions::from_mode(0o600))?;
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .with_ansi(false)
        .with_writer(std::sync::Mutex::new(file))
        .init();
    std::panic::set_hook(Box::new(|i| tracing::error!("panic: {i}")));
    let port: u16 = web::env::get("WEB_PORT")
        .and_then(|p| p.parse().ok())
        .unwrap_or(8787);
    let db = web::db::Db::open(&std::path::Path::new(&root).join(".moodle/web.db"))?;
    let app = Arc::new(web::App::new(db, Moodle::from_env()?, &root, port));
    let (stop_tx, stop) = tokio::sync::watch::channel(false);
    tokio::spawn(async move {
        let mut term = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
            .expect("SIGTERM handler");
        tokio::select! {
            _ = tokio::signal::ctrl_c() => {}
            _ = term.recv() => {}
        }
        let _ = stop_tx.send(true);
    });
    let bg = Arc::clone(&app);
    let mut bg_stop = stop.clone();
    tokio::spawn(async move {
        loop {
            // A panic in one pass must not kill the loop: spawn and log the JoinError.
            let pass = Arc::clone(&bg);
            if let Err(e) = tokio::spawn(async move {
                let t = std::time::Instant::now();
                match web::run_sync(&pass, true).await {
                    Ok(ev) => {
                        tracing::info!(
                            ms = t.elapsed().as_millis() as u64,
                            events = ev.len(),
                            "sync done"
                        );
                        if let Err(e) = web::organize::delivered_sync(&pass).await {
                            tracing::warn!(error = %e, "delivered sync failed");
                        }
                    }
                    Err(e) => tracing::warn!(error = %e, "background sync failed"),
                }
                web::apple::proactive(&pass).await;
                let r = web::apple::sync(&pass).await;
                if !r.errors.is_empty() {
                    tracing::warn!(errors = ?r.errors, "apple sync");
                }
                web::scan::rescore_idle(&pass).await;
            })
            .await
            {
                tracing::error!(error = %e, "background pass failed");
            }
            tokio::select! {
                _ = tokio::time::sleep(Duration::from_secs(30 * 60)) => {}
                _ = bg_stop.wait_for(|s| *s) => break,
            }
        }
    });
    let listener = tokio::net::TcpListener::bind(("127.0.0.1", port)).await?;
    if let Some(bind) = web::env::get("WEB_DOCKER_BIND") {
        // Degrade to "OnlyOffice no disponible" instead of refusing to start.
        match tokio::net::TcpListener::bind((bind.as_str(), port)).await {
            Ok(l) => {
                let oo = web::office::oo_router()
                    .merge(web::preview::psrc_router())
                    .with_state(Arc::clone(&app));
                let mut oo_stop = stop.clone();
                tokio::spawn(async move {
                    axum::serve(l, oo)
                        .with_graceful_shutdown(async move {
                            let _ = oo_stop.wait_for(|s| *s).await;
                        })
                        .await
                        .ok()
                });
                tracing::info!("onlyoffice callbacks on {bind}:{port}");
            }
            Err(e) => {
                tracing::warn!(error = %e, %bind, "docker bind failed, OnlyOffice unavailable")
            }
        }
    }
    if let (Some(bind), Some(peer)) = (web::env::get("WEB_LAN_BIND"), web::env::get("WEB_LAN_PEER"))
    {
        web::lan::spawn(Arc::clone(&app), &bind, &peer, std::path::Path::new(&root)).await;
    }
    tracing::info!("moodle-web on http://localhost:{port}");
    let served = tokio::spawn(web::serve_until(listener, Arc::clone(&app), stop.clone()));
    let mut wait = stop.clone();
    let _ = wait.wait_for(|s| *s).await;
    tracing::info!("shutting down");
    // Open keep-alive connections can stall graceful shutdown (hyper#2787): cap the wait.
    let _ = tokio::time::timeout(Duration::from_secs(5), served).await;
    app.db.checkpoint()?;
    Ok(())
}
