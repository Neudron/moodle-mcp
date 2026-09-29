//! JSON API + embedded static files.
use crate::web::{
    db::{Local, STATUSES},
    files, readme, tasks, App,
};
use axum::{
    body::Body,
    extract::{Path, Query, Request, State},
    http::{header, HeaderMap, StatusCode},
    middleware::Next,
    response::{IntoResponse, Response},
    routing::{get, post},
    Json, Router,
};
use serde::Deserialize;
use serde_json::{json, Value};
use std::sync::Arc;

type S = State<Arc<App>>;

pub struct ApiErr(pub StatusCode, pub String);
impl IntoResponse for ApiErr {
    fn into_response(self) -> Response {
        (self.0, self.1).into_response()
    }
}
impl From<anyhow::Error> for ApiErr {
    fn from(e: anyhow::Error) -> Self {
        ApiErr(
            StatusCode::INTERNAL_SERVER_ERROR,
            crate::errors::redact_token(&e.to_string()),
        )
    }
}
pub fn bad(s: &str) -> ApiErr {
    ApiErr(StatusCode::BAD_REQUEST, s.into())
}
pub fn forbid(s: impl ToString) -> ApiErr {
    ApiErr(StatusCode::FORBIDDEN, s.to_string())
}
pub type R<T> = Result<T, ApiErr>;

pub async fn guard_mw(State(app): S, req: Request, next: Next) -> Response {
    let h = req.headers();
    let g = |k: &str| h.get(k).and_then(|v| v.to_str().ok());
    match crate::web::guard::check(
        req.method().as_str(),
        req.uri().path(),
        g("host"),
        g("origin"),
        g("x-neu"),
        app.port,
    ) {
        Ok(()) => next.run(req).await,
        Err(e) => (StatusCode::FORBIDDEN, e).into_response(),
    }
}

pub fn routes() -> Router<Arc<App>> {
    Router::new()
        .route(
            "/",
            get(|| async {
                static_file(
                    "text/html; charset=utf-8",
                    include_str!("../../web/index.html"),
                )
            }),
        )
        .route(
            "/app.css",
            get(|| async {
                static_file("text/css; charset=utf-8", include_str!("../../web/app.css"))
            }),
        )
        .route(
            "/app.js",
            get(|| async {
                static_file(
                    "text/javascript; charset=utf-8",
                    include_str!("../../web/app.js"),
                )
            }),
        )
        .route("/api/tasks", get(list_tasks))
        .route("/api/tasks/{id}", get(one_task))
        .route("/api/tasks/{id}/status", post(set_status))
        .route("/api/tasks/{id}/files", post(set_files))
        .route("/api/tasks/{id}/ai", post(set_ai))
        .route("/api/submit-files", get(submit_files))
        .route("/api/courses", get(courses))
        .route("/api/courses/{short}", get(course))
        .route("/api/files", get(file_info))
        .route("/api/raw", get(raw))
        .route("/api/moodle-file", get(moodle_file))
        .route("/api/grades", get(grades))
        .route("/api/status", get(status))
        .route("/api/sync", post(sync_now))
}

fn static_file(ct: &'static str, body: &'static str) -> Response {
    (
        [
            (header::CONTENT_TYPE, ct),
            (header::CACHE_CONTROL, "no-cache"),
        ],
        body,
    )
        .into_response()
}

pub fn module_of(l: &Local, course: &str) -> String {
    if l.module.is_empty() {
        course.to_string()
    } else {
        l.module.clone()
    }
}

async fn list_tasks(State(app): S) -> R<Json<Value>> {
    let locals: std::collections::HashMap<_, _> = app
        .db
        .all_local()?
        .into_iter()
        .map(|l| (l.task_id.clone(), l))
        .collect();
    let out: Vec<Value> = app
        .db
        .all_tasks()?
        .into_iter()
        .map(|mut t| {
            t.body_html.clear();
            let l = locals.get(&t.id).cloned().unwrap_or(Local {
                task_id: t.id.clone(),
                ..Default::default()
            });
            json!({"task": t, "local": l})
        })
        .collect();
    Ok(Json(Value::Array(out)))
}

async fn one_task(State(app): S, Path(id): Path<String>) -> R<Json<Value>> {
    let mut t = app
        .db
        .task(&id)?
        .ok_or(ApiErr(StatusCode::NOT_FOUND, "no existeix".into()))?;
    t.body_html = tasks::proxy_urls(&t.body_html);
    t.feedback_html = tasks::proxy_urls(&t.feedback_html);
    let l = app.db.local(&id)?;
    let dir = readme::submit_dir(&module_of(&l, &t.course), &t.title);
    Ok(Json(json!({"task": t, "local": l, "submit_dir": dir})))
}

#[derive(Deserialize)]
pub struct StatusIn {
    status: String,
}
async fn set_status(
    State(app): S,
    Path(id): Path<String>,
    Json(b): Json<StatusIn>,
) -> R<Json<Local>> {
    if !b.status.is_empty() && !STATUSES.contains(&b.status.as_str()) {
        return Err(bad("estat no vàlid"));
    }
    let t = app.db.task(&id)?.ok_or(bad("no existeix"))?;
    let mut l = app.db.local(&id)?;
    l.status = b.status;
    app.db.set_local(&l)?;
    if l.status == "lliurat" {
        let from = std::path::Path::new(&app.root)
            .join(readme::submit_dir(&module_of(&l, &t.course), &t.title));
        if from.is_dir() {
            let to = std::path::Path::new(&app.root)
                .join("submit/_lliurat")
                .join(from.file_name().unwrap_or_default());
            std::fs::create_dir_all(to.parent().unwrap_or(&to)).map_err(anyhow::Error::from)?;
            std::fs::rename(&from, &to).map_err(anyhow::Error::from)?;
        }
    }
    readme::write(&app.db, &app.root)?;
    Ok(Json(l))
}

#[derive(Deserialize)]
pub struct FilesIn {
    pub files: Vec<String>,
}
async fn set_files(
    State(app): S,
    Path(id): Path<String>,
    Json(b): Json<FilesIn>,
) -> R<Json<Local>> {
    let root = std::path::Path::new(&app.root);
    for f in &b.files {
        files::jail(root, f).map_err(forbid)?;
    }
    let mut l = app.db.local(&id)?;
    l.files = b.files;
    app.db.set_local(&l)?;
    readme::write(&app.db, &app.root)?;
    Ok(Json(l))
}

#[derive(Deserialize)]
pub struct AiIn {
    keep: bool,
}
async fn set_ai(State(app): S, Path(id): Path<String>, Json(b): Json<AiIn>) -> R<Json<Value>> {
    app.db.set_ai_confirmed(&id, b.keep)?;
    readme::write(&app.db, &app.root)?;
    Ok(Json(json!({})))
}

async fn submit_files(State(app): S) -> R<Json<Vec<String>>> {
    let mut out = Vec::new();
    files::walk(std::path::Path::new(&app.root), "submit", &mut out);
    Ok(Json(out))
}

fn frontmatter(md: &str, key: &str) -> Option<String> {
    md.lines().take(12).find_map(|l| {
        l.strip_prefix(&format!("{key}: "))
            .map(|v| v.trim().trim_matches('"').to_string())
    })
}

fn course_meta(md: &str, short: &str) -> (String, i64) {
    (
        frontmatter(md, "title").unwrap_or_else(|| short.to_string()),
        frontmatter(md, "course_id")
            .and_then(|v| v.parse().ok())
            .unwrap_or(0),
    )
}

async fn courses(State(app): S) -> R<Json<Vec<Value>>> {
    let root = std::path::Path::new(&app.root);
    let mut out = Vec::new();
    for e in files::list(root, "").map_err(forbid)? {
        if !e.is_dir {
            continue;
        }
        let Ok(md) = std::fs::read_to_string(root.join(&e.rel).join("index.md")) else {
            continue;
        };
        let (title, id) = course_meta(&md, &e.name);
        out.push(json!({"shortname": e.name, "title": title, "course_id": id}));
    }
    Ok(Json(out))
}

async fn course(State(app): S, Path(short): Path<String>) -> R<Json<Value>> {
    let root = std::path::Path::new(&app.root);
    let md = std::fs::read_to_string(files::jail(root, &short).map_err(forbid)?.join("index.md"))
        .unwrap_or_default();
    let mut sections = Vec::new();
    let mut loose = Vec::new();
    for s in files::list(root, &short).map_err(forbid)? {
        if !s.is_dir {
            loose.push(s);
            continue;
        }
        let mut modules = Vec::new();
        let mut sfiles = Vec::new();
        for m in files::list(root, &s.rel).map_err(forbid)? {
            if m.is_dir {
                modules.push(json!({"name": m.name, "rel": m.rel, "files": files::list(root, &m.rel).unwrap_or_default()}));
            } else {
                sfiles.push(m);
            }
        }
        if !sfiles.is_empty() {
            modules.insert(0, json!({"name": "Fitxers", "rel": s.rel, "files": sfiles}));
        }
        sections.push(json!({"name": s.name, "rel": s.rel, "modules": modules}));
    }
    if !loose.is_empty() {
        sections.insert(0, json!({"name": "Fitxers", "rel": short, "modules": [{"name": "Fitxers", "rel": short, "files": loose}]}));
    }
    let (title, id) = course_meta(&md, &short);
    Ok(Json(
        json!({"shortname": short, "title": title, "course_id": id, "sections": sections}),
    ))
}

#[derive(Deserialize)]
pub struct PathQ {
    pub path: String,
}
async fn file_info(State(app): S, Query(q): Query<PathQ>) -> R<Json<Value>> {
    let root = std::path::Path::new(&app.root);
    let p = files::jail(root, &q.path).map_err(forbid)?;
    if p.is_dir() {
        return Ok(Json(
            json!({"kind": "dir", "rel": q.path, "entries": files::list(root, &q.path).map_err(forbid)?}),
        ));
    }
    let size = std::fs::metadata(&p).map(|m| m.len()).unwrap_or(0);
    Ok(Json(
        json!({"kind": "file", "rel": q.path, "mime": files::mime(&p), "size": size}),
    ))
}

async fn raw(State(app): S, Query(q): Query<PathQ>) -> R<Response> {
    let p = files::jail(std::path::Path::new(&app.root), &q.path).map_err(forbid)?;
    let bytes = tokio::fs::read(&p)
        .await
        .map_err(|e| ApiErr(StatusCode::NOT_FOUND, e.to_string()))?;
    let ct = files::mime(&p);
    let mut h = HeaderMap::new();
    h.insert(header::CONTENT_TYPE, ct.parse().expect("static mime"));
    h.insert("x-content-type-options", "nosniff".parse().expect("static"));
    h.insert(
        header::CONTENT_DISPOSITION,
        "inline".parse().expect("static"),
    );
    if ct.starts_with("text/html") || ct.starts_with("image/svg") {
        h.insert(
            "content-security-policy",
            "sandbox".parse().expect("static"),
        );
    }
    Ok((h, Body::from(bytes)).into_response())
}

#[derive(Deserialize)]
pub struct UrlQ {
    url: String,
}
async fn moodle_file(State(app): S, Query(q): Query<UrlQ>) -> R<Response> {
    let bytes = app.moodle.download(&q.url).await?;
    let ct = files::mime(std::path::Path::new(q.url.split('?').next().unwrap_or("")));
    Ok((
        [
            (header::CONTENT_TYPE, ct),
            (header::X_CONTENT_TYPE_OPTIONS, "nosniff"),
        ],
        bytes,
    )
        .into_response())
}

async fn grades(State(app): S) -> R<Json<Vec<Value>>> {
    let names: std::collections::HashMap<i64, String> = app
        .db
        .all_tasks()?
        .into_iter()
        .map(|t| (t.course_id, t.course))
        .collect();
    Ok(Json(app.db.grades()?.into_iter().map(|(id, v)| json!({
        "course_id": id, "course": names.get(&id).cloned().unwrap_or_default(), "items": v["gradeitems"].clone()})).collect()))
}

async fn status(State(app): S) -> R<Json<Value>> {
    Ok(Json(json!({
        "last_sync": app.db.kv_get("last_sync")?.and_then(|v| v.parse::<i64>().ok()).unwrap_or(0),
        "syncing": app.syncing.try_lock().is_err(),
        "error": app.db.kv_get("last_error")?.unwrap_or_default(),
    })))
}

async fn sync_now(State(app): S) -> R<Json<Value>> {
    if app.syncing.try_lock().is_err() {
        return Err(ApiErr(
            StatusCode::CONFLICT,
            "ja s'està sincronitzant".into(),
        ));
    }
    let ev = crate::web::run_sync(&app, false)
        .await
        .map_err(|e| ApiErr(StatusCode::BAD_GATEWAY, e.to_string()))?;
    Ok(Json(json!({"events": ev.len()})))
}
