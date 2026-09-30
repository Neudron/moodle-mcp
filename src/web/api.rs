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
        Ok(()) => {
            // Clickjacking: other sites may not frame the app; our own iframes (PDF, HTML preview) still work.
            let mut res = next.run(req).await;
            let h = res.headers_mut();
            h.insert(
                header::X_FRAME_OPTIONS,
                header::HeaderValue::from_static("SAMEORIGIN"),
            );
            h.append(
                header::CONTENT_SECURITY_POLICY,
                header::HeaderValue::from_static("frame-ancestors 'self'"),
            );
            res
        }
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
        .route("/api/tasks/{id}/upload", post(upload_files))
        .route("/api/tasks/{id}/submit", post(submit))
        .route("/api/submit-files", get(submit_files))
        .route("/api/courses", get(courses))
        .route("/api/courses/{short}", get(course))
        .route("/api/files", get(file_info))
        .route("/api/raw", get(raw))
        .route("/api/moodle-file", get(moodle_file))
        .route("/api/grades", get(grades))
        .route("/api/status", get(status))
        .route("/api/sync", post(sync_now))
        .merge(crate::web::office::oo_router())
        .merge(crate::web::preview::router())
        .merge(crate::web::shots::router())
        .merge(crate::web::scan::router())
        .merge(crate::web::ide::router())
        .merge(crate::web::organize::router())
        .route("/api/apple-sync", post(apple_sync))
        .route("/api/office/config", post(office_config))
        .route("/api/open", post(open_local))
}

fn static_file(ct: &'static str, body: &'static str) -> Response {
    (
        [
            (header::CONTENT_TYPE, ct),
            // no-store: embedded assets carry no ETag, so no-cache let browsers keep a stale app.js after a rebuild.
            (header::CACHE_CONTROL, "no-store"),
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
    let mut dir = readme::submit_dir(&module_of(&l, &t.course), &t.title);
    if l.status == "lliurat" {
        dir = dir.replacen("submit/", "submit/_lliurat/", 1);
    }
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
    if l.status == "lliurat" {
        move_lliurat(&app, &t, &mut l)?;
    }
    app.db.set_local(&l)?;
    readme::write(&app.db, &app.root)?;
    Ok(Json(l))
}

/// `lliurat` moves the task folder to `submit/_lliurat/` and rewrites `l.files`.
fn move_lliurat(app: &App, t: &crate::web::db::Task, l: &mut Local) -> R<()> {
    let from_rel = readme::submit_dir(&module_of(l, &t.course), &t.title);
    let root = std::path::Path::new(&app.root);
    let to_rel = from_rel.replacen("submit/", "submit/_lliurat/", 1);
    if root.join(&from_rel).is_dir() {
        let to = root.join(&to_rel);
        std::fs::create_dir_all(to.parent().unwrap_or(&to)).map_err(anyhow::Error::from)?;
        std::fs::rename(root.join(&from_rel), &to).map_err(anyhow::Error::from)?;
        for f in &mut l.files {
            if f == &from_rel || f.starts_with(&format!("{from_rel}/")) {
                *f = format!("{to_rel}{}", &f[from_rel.len()..]);
            }
        }
    }
    Ok(())
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

fn assign_id(id: &str) -> R<i64> {
    id.strip_prefix("assign:")
        .and_then(|n| n.parse().ok())
        .ok_or(bad("només tasques de Moodle"))
}

fn store_status(app: &App, id: &str, st: &Value) -> R<String> {
    let s = st["lastattempt"]["submission"]["status"]
        .as_str()
        .unwrap_or("")
        .to_string();
    if let Some(mut t) = app.db.task(id)? {
        t.moodle_status = s.clone();
        app.db.upsert_tasks(&[t])?;
    }
    Ok(s)
}

fn upstream(e: crate::errors::CoreError) -> ApiErr {
    ApiErr(
        StatusCode::BAD_GATEWAY,
        crate::errors::redact_token(&e.to_string()),
    )
}

/// Moodle says "submitted": mark lliurat and move the folder. A failed move only warns (Moodle already has it).
pub fn mark_lliurat(app: &App, id: &str) -> R<Option<String>> {
    let mut l = app.db.local(id)?;
    l.status = "lliurat".into();
    let t = app.db.task(id)?.ok_or(bad("no existeix"))?;
    let warning = move_lliurat(app, &t, &mut l).err().map(|e| {
        format!(
            "Enviada a Moodle, però no s'ha pogut moure la carpeta: {}",
            e.1
        )
    });
    app.db.set_local(&l)?;
    readme::write(&app.db, &app.root)?;
    Ok(warning)
}

fn status_json(s: String, warning: Option<String>) -> Json<Value> {
    Json(match warning {
        Some(w) => json!({"status": s, "warning": w}),
        None => json!({"status": s}),
    })
}

async fn upload_files(
    State(app): S,
    Path(id): Path<String>,
    Json(b): Json<FilesIn>,
) -> R<Json<Value>> {
    let aid = assign_id(&id)?;
    let root = std::path::Path::new(&app.root);
    let croot = root.canonicalize().map_err(anyhow::Error::from)?;
    let paths = b
        .files
        .iter()
        .map(|f| {
            crate::web::upload::resolve(root, f)
                .ok_or_else(|| forbid(format!("fitxer no trobat: {f}")))
        })
        .collect::<Result<Vec<_>, _>>()?;
    // Teacher originals live in course folders: only _amin files (or anything under submit/) go up.
    for p in &paths {
        let stem = p
            .file_stem()
            .map(|s| s.to_string_lossy())
            .unwrap_or_default();
        if !stem.ends_with("_amin") && !p.starts_with(croot.join("submit")) {
            return Err(bad("només fitxers _amin o de submit/"));
        }
    }
    let st = crate::web::upload::save_draft(&app.moodle, aid, &paths)
        .await
        .map_err(upstream)?;
    let s = store_status(&app, &id, &st)?;
    // Assignments without drafts (submissiondrafts=0) are final as soon as they are saved.
    let warning = if s == "submitted" {
        mark_lliurat(&app, &id)?
    } else {
        None
    };
    Ok(status_json(s, warning))
}

async fn submit(State(app): S, Path(id): Path<String>) -> R<Json<Value>> {
    let aid = assign_id(&id)?;
    let st = crate::web::upload::submit_for_grading(&app.moodle, aid)
        .await
        .map_err(upstream)?;
    let s = store_status(&app, &id, &st)?;
    let warning = if s == "submitted" {
        mark_lliurat(&app, &id)?
    } else {
        None
    };
    Ok(status_json(s, warning))
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
    let ct = files::mime(std::path::Path::new(
        q.url.split(['?', '#']).next().unwrap_or(""),
    ));
    // Remote bytes on our origin: always sandbox (blocks script in html/svg).
    Ok((
        [
            (header::CONTENT_TYPE, ct),
            (header::X_CONTENT_TYPE_OPTIONS, "nosniff"),
            (header::CONTENT_DISPOSITION, "inline"),
            (header::CONTENT_SECURITY_POLICY, "sandbox"),
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
        "course_id": id,
        "course": v["course_name"].as_str().map(String::from).or_else(|| names.get(&id).cloned()).unwrap_or_default(),
        "items": v["gradeitems"].clone()})).collect()))
}

async fn status(State(app): S) -> R<Json<Value>> {
    Ok(Json(json!({
        "last_sync": app.db.kv_get("last_sync")?.and_then(|v| v.parse::<i64>().ok()).unwrap_or(0),
        "syncing": app.syncing.try_lock().is_err(),
        "error": app.db.kv_get("last_error")?.unwrap_or_default(),
    })))
}

async fn apple_sync(State(app): S) -> Json<Value> {
    Json(json!(crate::web::apple::sync(&app).await))
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

async fn office_config(State(app): S, Json(q): Json<PathQ>) -> R<Json<Value>> {
    let is_office = q
        .path
        .rsplit_once('.')
        .is_some_and(|(_, e)| crate::web::office::doc_type(e).is_some());
    if !is_office {
        return Err(bad("només fitxers ofimàtics"));
    }
    let secret = crate::web::env::get("ONLYOFFICE_JWT").ok_or(ApiErr(
        StatusCode::SERVICE_UNAVAILABLE,
        "OnlyOffice no disponible".into(),
    ))?;
    let root = std::path::Path::new(&app.root);
    let rel = crate::web::office::amin_copy(root, &q.path).map_err(forbid)?;
    let p = files::jail(root, &rel).map_err(forbid)?;
    repoint(&app, &q.path, &rel)?;
    let mtime = std::fs::metadata(&p)
        .ok()
        .and_then(|m| m.modified().ok())
        .and_then(|m| m.duration_since(std::time::UNIX_EPOCH).ok())
        .map_or(0, |d| d.as_secs() as i64);
    let host = format!("http://host.docker.internal:{}", app.port);
    let oo =
        crate::web::env::get("ONLYOFFICE_URL").unwrap_or_else(|| "http://127.0.0.1:8080".into());
    Ok(Json(
        json!({"api_js": format!("{oo}/web-apps/apps/api/documents/api.js"), "rel": rel,
        "config": crate::web::office::editor_config(&secret, &rel, mtime, &host, crate::web::now())}),
    ))
}

/// Tasks linking the teacher original must point at the `_amin` copy instead, or "Puja"
/// would upload the unedited original. Keeps order, drops the duplicate if both were linked.
pub fn repoint(app: &App, from: &str, to: &str) -> R<()> {
    if from == to {
        return Ok(());
    }
    let mut changed = false;
    for mut l in app.db.all_local()? {
        if !l.files.iter().any(|f| f == from) {
            continue;
        }
        let mut seen = std::collections::HashSet::new();
        l.files = l
            .files
            .iter()
            .map(|f| if f == from { to.to_string() } else { f.clone() })
            .filter(|f| seen.insert(f.clone()))
            .collect();
        app.db.set_local(&l)?;
        changed = true;
    }
    if changed {
        readme::write(&app.db, &app.root)?;
    }
    Ok(())
}

/// Office files open as their `_amin` copy so a teacher original is never edited in place.
/// Returns the absolute path and the rel actually opened.
fn open_target(root: &std::path::Path, rel: &str) -> Result<(std::path::PathBuf, String), String> {
    let is_office = rel
        .rsplit_once('.')
        .is_some_and(|(_, e)| crate::web::office::doc_type(e).is_some());
    let rel = if is_office {
        crate::web::office::amin_copy(root, rel)?
    } else {
        rel.to_string()
    };
    Ok((files::jail(root, &rel)?, rel))
}

async fn open_local(State(app): S, Json(b): Json<PathQ>) -> R<Json<Value>> {
    let (p, rel) = open_target(std::path::Path::new(&app.root), &b.path).map_err(forbid)?;
    repoint(&app, &b.path, &rel)?;
    std::process::Command::new("xdg-open")
        .arg(p)
        .spawn()
        .map_err(anyhow::Error::from)?;
    Ok(Json(json!({})))
}

#[cfg(test)]
mod open_tests {
    use super::open_target;

    #[test]
    fn open_office_original_targets_amin_copy() {
        let r = std::env::temp_dir().join(format!("mweb-open-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&r);
        std::fs::create_dir_all(r.join("C")).unwrap();
        std::fs::write(r.join("C/T.docx"), "orig").unwrap();
        std::fs::write(r.join("C/n.pdf"), "pdf").unwrap();
        let (p, rel) = open_target(&r, "C/T.docx").unwrap();
        assert!(p.ends_with("C/T_amin.docx"));
        assert_eq!(rel, "C/T_amin.docx");
        assert_eq!(std::fs::read_to_string(r.join("C/T.docx")).unwrap(), "orig");
        assert!(open_target(&r, "C/n.pdf").unwrap().0.ends_with("C/n.pdf"));
        assert!(!r.join("C/n_amin.pdf").exists());
    }

    #[test]
    fn repoint_replaces_original_without_duplicates() {
        use crate::web::db::{Db, Local};
        let r = std::env::temp_dir().join(format!("mweb-repoint-{}", std::process::id()));
        std::fs::create_dir_all(&r).unwrap();
        let app = crate::web::App::new(
            Db::memory().unwrap(),
            crate::moodle::Moodle::new("http://127.0.0.1:9", "tok-0123456789abcdef"),
            r.to_str().unwrap(),
            0,
        );
        let l = |id: &str, files: &[&str]| Local {
            task_id: id.into(),
            files: files.iter().map(|s| s.to_string()).collect(),
            ..Default::default()
        };
        app.db.set_local(&l("a", &["C/x.docx"])).unwrap();
        app.db
            .set_local(&l("b", &["C/x.docx", "C/x_amin.docx"]))
            .unwrap();
        app.db.set_local(&l("c", &["C/y.docx"])).unwrap();
        assert!(super::repoint(&app, "C/x.docx", "C/x_amin.docx").is_ok());
        assert_eq!(app.db.local("a").unwrap().files, ["C/x_amin.docx"]);
        assert_eq!(app.db.local("b").unwrap().files, ["C/x_amin.docx"]);
        assert_eq!(app.db.local("c").unwrap().files, ["C/y.docx"]);
    }
}
