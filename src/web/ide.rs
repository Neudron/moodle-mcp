//! Monaco IDE backend (spec 2026-09-30 WP5): read text + sha, guarded save,
//! `_amin` copy for teacher originals.
use crate::state::State as SyncState;
use crate::web::{
    api::{forbid, repoint, ApiErr, R},
    files, office, App,
};
use axum::{
    body::Bytes,
    extract::{DefaultBodyLimit, Query, State},
    http::StatusCode,
    response::{IntoResponse, Response},
    routing::{get, post},
    Json, Router,
};
use serde::Deserialize;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

// ponytail: one global lock around check+write; per-path locks if saves ever queue up.
static SAVE: Mutex<()> = Mutex::new(());

pub fn sha_hex(b: &[u8]) -> String {
    Sha256::digest(b)
        .iter()
        .map(|x| format!("{x:02x}"))
        .collect()
}

#[derive(Deserialize)]
struct PathIn {
    path: String,
}

/// Jail + must be a regular file. Returns the absolute path and the rel under the canonical root.
fn resolve(root: &str, rel: &str) -> R<(PathBuf, String)> {
    let p = files::jail(Path::new(root), rel).map_err(forbid)?;
    if !p.is_file() {
        return Err(ApiErr(StatusCode::BAD_REQUEST, "no és un fitxer".into()));
    }
    let croot = Path::new(root)
        .canonicalize()
        .map_err(anyhow::Error::from)?;
    let rel = p
        .strip_prefix(&croot)
        .map_err(forbid)?
        .to_string_lossy()
        .into_owned();
    Ok((p, rel))
}

/// (content, sha) of a UTF-8 file; binary files are refused.
fn read_text(p: &Path) -> R<(String, String)> {
    let bytes = std::fs::read(p).map_err(anyhow::Error::from)?;
    let sha = sha_hex(&bytes);
    let s = String::from_utf8(bytes)
        .map_err(|_| ApiErr(StatusCode::UNSUPPORTED_MEDIA_TYPE, "no és text".into()))?;
    Ok((s, sha))
}

/// True when `rel` is a file synced from Moodle. Errors (corrupt state) propagate: fail closed.
fn is_teacher(root: &str, rel: &str) -> anyhow::Result<bool> {
    Ok(SyncState::load(root)?
        .courses
        .values()
        .any(|c| c.files.values().any(|f| f.path == rel)))
}

async fn blocking<T: Send + 'static>(f: impl FnOnce() -> R<T> + Send + 'static) -> R<T> {
    tokio::task::spawn_blocking(f)
        .await
        .map_err(anyhow::Error::from)?
}

async fn read(State(app): State<Arc<App>>, Query(q): Query<PathIn>) -> R<Json<Value>> {
    let (rel, content, sha, teacher) = blocking(move || {
        let (p, rel) = resolve(&app.root, &q.path)?;
        let (content, sha) = read_text(&p)?;
        let teacher = is_teacher(&app.root, &rel)?;
        Ok((rel, content, sha, teacher))
    })
    .await?;
    Ok(Json(
        json!({"rel": rel, "content": content, "sha": sha, "teacher": teacher}),
    ))
}

#[derive(Deserialize)]
struct SaveIn {
    path: String,
    content: String,
    base_sha: String,
}

fn conflict(v: Value) -> Response {
    (StatusCode::CONFLICT, Json(v)).into_response()
}

fn io(e: std::io::Error) -> Response {
    ApiErr::from(anyhow::Error::from(e)).into_response()
}

/// Check teacher + sha under the lock, then .bak, tmp, rename. Returns the new sha.
#[allow(clippy::result_large_err)] // Response is the handler's own error body
fn save(root: &str, rel: &str, content: &str, base: &str) -> Result<String, Response> {
    save_bytes(root, rel, content.as_bytes(), Some(base))
}

/// `base` Some: text file whose current sha must match. None: blind overwrite (save-back).
#[allow(clippy::result_large_err)]
fn save_bytes(
    root: &str,
    rel: &str,
    content: &[u8],
    base: Option<&str>,
) -> Result<String, Response> {
    let _g = SAVE.lock().unwrap_or_else(|e| e.into_inner());
    let (p, rel) = resolve(root, rel).map_err(IntoResponse::into_response)?;
    if is_teacher(root, &rel).map_err(|e| ApiErr::from(e).into_response())? {
        return Err(conflict(json!({"error": "teacher_original", "path": rel})));
    }
    if let Some(base) = base {
        let (_, cur) = read_text(&p).map_err(IntoResponse::into_response)?;
        if cur != base {
            return Err(conflict(json!({"error": "conflict", "sha": cur})));
        }
    }
    let mut bak = p.clone().into_os_string();
    bak.push(".bak");
    // ponytail: save-back keeps the first .bak (pre-upload original); timestamped .bak if multi-generation history is needed
    if base.is_some() || !Path::new(&bak).exists() {
        std::fs::copy(&p, &bak).map_err(io)?;
    }
    let mut tmp = p.clone().into_os_string();
    tmp.push(".tmp");
    std::fs::write(&tmp, content).map_err(io)?;
    std::fs::set_permissions(&tmp, std::fs::metadata(&p).map_err(io)?.permissions()).map_err(io)?;
    std::fs::rename(&tmp, &p).map_err(io)?;
    Ok(sha_hex(content))
}

#[allow(clippy::result_large_err)]
async fn save_blocking(app: Arc<App>, b: SaveIn) -> Result<String, Response> {
    tokio::task::spawn_blocking(move || save(&app.root, &b.path, &b.content, &b.base_sha))
        .await
        .unwrap_or_else(|e| Err(ApiErr::from(anyhow::Error::from(e)).into_response()))
}

async fn save_h(State(app): State<Arc<App>>, Json(b): Json<SaveIn>) -> Response {
    let r = save_blocking(app, b).await;
    match r {
        Ok(sha) => Json(json!({"sha": sha})).into_response(),
        Err(r) => r,
    }
}

#[allow(clippy::result_large_err)]
async fn save_back(State(app): State<Arc<App>>, Query(q): Query<PathIn>, body: Bytes) -> Response {
    let r = tokio::task::spawn_blocking(move || save_bytes(&app.root, &q.path, &body, None))
        .await
        .unwrap_or_else(|e| Err(ApiErr::from(anyhow::Error::from(e)).into_response()));
    match r {
        Ok(sha) => Json(json!({"sha": sha})).into_response(),
        Err(r) => r,
    }
}

async fn copy(State(app): State<Arc<App>>, Json(b): Json<PathIn>) -> R<Json<Value>> {
    let a = Arc::clone(&app);
    let (rel, sha) = blocking(move || {
        let (src, _) = resolve(&a.root, &b.path)?;
        read_text(&src)?; // refuse binary before creating anything
        let rel = office::amin_copy(Path::new(&a.root), &b.path).map_err(forbid)?;
        repoint(&a, &b.path, &rel)?;
        let (_, sha) = read_text(&resolve(&a.root, &rel)?.0)?;
        Ok((rel, sha))
    })
    .await?;
    Ok(Json(json!({"rel": rel, "sha": sha})))
}

pub fn router() -> Router<Arc<App>> {
    Router::new()
        .route("/api/ide/file", get(read))
        .route("/api/save", post(save_h))
        .route(
            "/api/save-back",
            post(save_back).layer(DefaultBodyLimit::max(100 << 20)),
        )
        .route("/api/ide/copy", post(copy))
}
