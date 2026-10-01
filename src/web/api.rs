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
    response::{AppendHeaders, IntoResponse, Response},
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
        g("sec-fetch-site"),
    ) {
        Ok(()) => {
            // Clickjacking: other sites may not frame the app; our own iframes (PDF, HTML preview) still work.
            let mut res = next.run(req).await;
            let h = res.headers_mut();
            h.insert(
                header::X_FRAME_OPTIONS,
                header::HeaderValue::from_static("SAMEORIGIN"),
            );
            h.insert(
                header::HeaderName::from_static("cross-origin-resource-policy"),
                header::HeaderValue::from_static("same-origin"),
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
            get(|h: HeaderMap| async move {
                static_file(
                    &h,
                    "text/html; charset=utf-8",
                    include_str!("../../web/index.html"),
                )
            }),
        )
        .route(
            "/app.css",
            get(|h: HeaderMap| async move {
                static_file(
                    &h,
                    "text/css; charset=utf-8",
                    include_str!("../../web/app.css"),
                )
            }),
        )
        .route(
            "/app.js",
            get(|h: HeaderMap| async move {
                static_file(
                    &h,
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
        .route("/api/browse", get(browse))
        .route("/api/tasks/{id}/pick", post(pick))
        .route("/api/courses", get(courses))
        .route("/api/courses/{short}", get(course))
        .route("/api/files", get(file_info))
        .route("/api/raw", get(raw))
        .route("/api/moodle-file", get(moodle_file))
        .route("/api/grades", get(grades))
        .route("/api/status", get(status))
        .route("/api/sync", post(sync_now))
        .route("/api/auth/status", get(auth_status))
        .route("/api/auth/renew", post(auth_renew))
        .merge(crate::web::office::oo_router())
        .merge(crate::web::preview::router())
        .merge(crate::web::shots::router())
        .merge(crate::web::scan::router())
        .merge(crate::web::ide::router())
        .merge(crate::web::organize::router())
        .route("/api/apple-sync", post(apple_sync))
        .route("/api/apple/status", get(apple_status))
        .route("/api/apple/reimport", post(apple_reimport))
        .route("/api/office/config", post(office_config))
        .route("/api/open", post(open_local))
}

fn etag_of(parts: &[&[u8]]) -> String {
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    parts.hash(&mut h);
    format!("\"{:x}\"", h.finish())
}

/// 304 when `If-None-Match` equals `etag`, else `body` with the ETag.
fn cached(h: &HeaderMap, etag: String, ct: &'static str, body: impl IntoResponse) -> Response {
    let hit = h
        .get(header::IF_NONE_MATCH)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| v == etag);
    let heads = [
        (header::ETAG, etag),
        (header::CACHE_CONTROL, "no-cache".into()),
    ];
    if hit {
        (StatusCode::NOT_MODIFIED, heads).into_response()
    } else {
        (heads, [(header::CONTENT_TYPE, ct)], body).into_response()
    }
}

// ponytail: hashed per request (~100 KB, microseconds); OnceLock per asset if it ever shows up in a profile.
fn static_file(h: &HeaderMap, ct: &'static str, body: &'static str) -> Response {
    cached(h, etag_of(&[body.as_bytes()]), ct, body)
}

/// Text of the first `<blockquote>` (scan.rs escapes `& < > "`), capped to ~200 chars.
fn evidence_of(html: &str) -> String {
    let inner = html
        .split_once("<blockquote>")
        .and_then(|(_, r)| r.split_once("</blockquote>"))
        .map_or("", |(q, _)| q);
    inner
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&amp;", "&")
        .chars()
        .take(200)
        .collect()
}

pub fn module_of(l: &Local, course: &str) -> String {
    if l.module.is_empty() {
        course.to_string()
    } else {
        l.module.clone()
    }
}

async fn list_tasks(State(app): S, h: HeaderMap) -> R<Response> {
    let etag = format!("\"t{:x}\"", app.db.version());
    if h.get(header::IF_NONE_MATCH).and_then(|v| v.to_str().ok()) == Some(etag.as_str()) {
        return Ok(cached(&h, etag, "application/json", ()));
    }
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
            let ev = (t.source == "ai").then(|| evidence_of(&t.body_html));
            // List view never reads these; the detail endpoint stays complete.
            t.body_html.clear();
            t.feedback_html.clear();
            t.attachments.clear();
            t.submission_files.clear();
            let l = locals.get(&t.id).cloned().unwrap_or(Local {
                task_id: t.id.clone(),
                ..Default::default()
            });
            let mut tv = serde_json::to_value(&t).unwrap_or(Value::Null);
            if let Some(ev) = ev {
                tv["evidence"] = ev.into();
            }
            json!({"task": tv, "local": l})
        })
        .collect();
    Ok(cached(
        &h,
        etag,
        "application/json",
        Json(Value::Array(out)),
    ))
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
    let shot = crate::web::shots::ensure(&app, &t);
    Ok(Json(
        json!({"task": t, "local": l, "submit_dir": dir, "shot": shot}),
    ))
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
    let l = app.db.try_update_local(&id, |l| {
        let was = l.status == "lliurat";
        l.status = b.status;
        if l.status == "lliurat" && !was {
            move_lliurat(&app, &t, l, "submit/", "submit/_lliurat/")?;
        } else if was && l.status != "lliurat" {
            move_lliurat(&app, &t, l, "submit/_lliurat/", "submit/")?;
        }
        Ok::<_, ApiErr>(())
    })?;
    readme::write(&app.db, &app.root)?;
    Ok(Json(l))
}

/// Moves the task folder between `submit/` and `submit/_lliurat/` (prefixes) and rewrites `l.files`.
fn move_lliurat(
    app: &App,
    t: &crate::web::db::Task,
    l: &mut Local,
    from_p: &str,
    to_p: &str,
) -> R<()> {
    let base = readme::submit_dir(&module_of(l, &t.course), &t.title);
    let from_rel = base.replacen("submit/", from_p, 1);
    let root = std::path::Path::new(&app.root);
    let to_rel = base.replacen("submit/", to_p, 1);
    if root.join(&from_rel).is_dir() {
        let to = root.join(&to_rel);
        if to.is_dir()
            && std::fs::read_dir(&to)
                .map_err(anyhow::Error::from)?
                .next()
                .is_some()
        {
            return Err(bad("la carpeta destí ja existeix i no és buida"));
        }
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
    let l = app.db.update_local(&id, |l| l.files = b.files)?;
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
    app.db.update_task(id, |t| t.moodle_status = s.clone())?;
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
    let t = app.db.task(id)?.ok_or(bad("no existeix"))?;
    let mut warning = None;
    app.db.update_local(id, |l| {
        l.status = "lliurat".into();
        warning = move_lliurat(app, &t, l, "submit/", "submit/_lliurat/")
            .err()
            .map(|e| {
                format!(
                    "Enviada a Moodle, però no s'ha pogut moure la carpeta: {}",
                    e.1
                )
            });
    })?;
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
    let paths = b
        .files
        .iter()
        .map(|f| {
            crate::web::upload::resolve(root, f)
                .ok_or_else(|| forbid(format!("fitxer no trobat: {f}")))
        })
        .collect::<Result<Vec<_>, _>>()?;
    // Teacher originals live in course folders: only _amin files (or anything under submit/) go up.
    // A submit/ link outside root is a home pick (jail limits it to non-hidden home paths).
    let croot = root.canonicalize().map_err(anyhow::Error::from)?;
    for (f, p) in b.files.iter().zip(&paths) {
        let stem = p
            .file_stem()
            .map(|s| s.to_string_lossy())
            .unwrap_or_default();
        if !stem.ends_with("_amin")
            && !p.starts_with(croot.join("submit"))
            && !(f.starts_with("submit/") && !p.starts_with(&croot))
        {
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

async fn browse(Query(q): Query<PathQ>) -> R<Json<Value>> {
    let es = files::list(&files::home(), &q.path).map_err(forbid)?;
    Ok(Json(json!({"rel": q.path, "entries": es})))
}

#[derive(Deserialize)]
struct PickIn {
    path: String,
}
/// Link a file picked from $HOME into the task's submit folder (symlink, never a copy).
async fn pick(State(app): S, Path(id): Path<String>, Json(b): Json<PickIn>) -> R<Json<Local>> {
    let p = files::jail(&files::home(), &b.path).map_err(forbid)?;
    if !p.is_file() {
        return Err(bad("no és un fitxer"));
    }
    let root = std::path::Path::new(&app.root);
    let croot = root.canonicalize().map_err(anyhow::Error::from)?;
    let t = app
        .db
        .task(&id)?
        .ok_or(ApiErr(StatusCode::NOT_FOUND, "no existeix".into()))?;
    let l = app.db.local(&id)?;
    let rel = if let (true, Ok(r)) = (p.starts_with(croot.join("submit")), p.strip_prefix(&croot)) {
        r.to_string_lossy().into_owned()
    } else {
        let stem = p
            .file_stem()
            .map(|s| s.to_string_lossy())
            .unwrap_or_default();
        if p.starts_with(&croot) && !stem.ends_with("_amin") {
            return Err(bad(
                "original del professor: obre'l per crear la còpia _amin",
            ));
        }
        let mut dir = readme::submit_dir(&module_of(&l, &t.course), &t.title);
        if l.status == "lliurat" {
            dir = dir.replacen("submit/", "submit/_lliurat/", 1);
        }
        let abs = root.join(&dir);
        std::fs::create_dir_all(&abs).map_err(anyhow::Error::from)?;
        let ext = p
            .extension()
            .map(|e| format!(".{}", e.to_string_lossy()))
            .unwrap_or_default();
        let mut n = 1;
        loop {
            let name = if n == 1 {
                format!("{stem}{ext}")
            } else {
                format!("{stem}-{n}{ext}")
            };
            let ln = abs.join(&name);
            match std::fs::symlink_metadata(&ln) {
                Err(_) => {
                    std::os::unix::fs::symlink(&p, &ln).map_err(anyhow::Error::from)?;
                    break format!("{dir}/{name}");
                }
                Ok(_) if ln.canonicalize().is_ok_and(|c| c == p) => break format!("{dir}/{name}"),
                Ok(_) => n += 1,
            }
        }
    };
    let l = app.db.update_local(&id, |l| {
        if !l.files.contains(&rel) {
            l.files.push(rel.clone())
        }
    })?;
    readme::write(&app.db, &app.root)?;
    Ok(Json(l))
}

fn frontmatter(md: &str, key: &str) -> Option<String> {
    md.lines().take(12).find_map(|l| {
        l.strip_prefix(&format!("{key}: ")).map(|v| {
            let v = v.trim();
            let Some(q) = v.strip_prefix('"').and_then(|x| x.strip_suffix('"')) else {
                return v.to_string();
            };
            let (mut out, mut it) = (String::new(), q.chars());
            while let Some(c) = it.next() {
                out.push(match (c, if c == '\\' { it.next() } else { None }) {
                    ('\\', Some('n')) => '\n',
                    ('\\', Some(x)) => x,
                    ('\\', None) => '\\',
                    _ => c,
                });
            }
            out
        })
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

/// Top dir whose `index.md` frontmatter shortname is `short`.
pub(crate) fn course_dir_of(root: &std::path::Path, short: &str) -> Option<String> {
    files::list(root, "")
        .ok()?
        .into_iter()
        .filter(|e| e.is_dir)
        .find(|e| {
            course_short_of(root, &e.name) == short && root.join(&e.rel).join("index.md").is_file()
        })
        .map(|e| e.rel)
}

/// Frontmatter shortname of the course in `dir`, else the dir name.
pub(crate) fn course_short_of(root: &std::path::Path, dir: &str) -> String {
    std::fs::read_to_string(root.join(dir).join("index.md"))
        .ok()
        .and_then(|md| frontmatter(&md, "shortname"))
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| dir.to_string())
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
        out.push(
            json!({"shortname": course_short_of(root, &e.name), "title": title, "course_id": id}),
        );
    }
    Ok(Json(out))
}

async fn course(State(app): S, Path(short): Path<String>) -> R<Json<Value>> {
    let root = std::path::Path::new(&app.root);
    let dir = course_dir_of(root, &short).unwrap_or_else(|| short.clone());
    let md = std::fs::read_to_string(files::jail(root, &dir).map_err(forbid)?.join("index.md"))
        .unwrap_or_default();
    let mut sections = Vec::new();
    let mut loose = Vec::new();
    for s in files::list(root, &dir).map_err(forbid)? {
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
        sections.insert(0, json!({"name": "Fitxers", "rel": dir, "modules": [{"name": "Fitxers", "rel": dir, "files": loose}]}));
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

const RAW_MAX: u64 = 8 << 20;

/// Single `bytes=a-b` / `a-` / `-n` range -> inclusive (start, end); None = unsatisfiable/invalid.
fn parse_range(h: &str, size: u64) -> Option<(u64, u64)> {
    let (a, b) = h.strip_prefix("bytes=")?.split_once('-')?;
    let (s, e) = match (a.trim(), b.trim()) {
        ("", n) => (
            size.checked_sub(n.parse().ok().filter(|&n| n > 0)?)?,
            size.checked_sub(1)?,
        ),
        (a, "") => (a.parse().ok()?, size.checked_sub(1)?),
        (a, b) => (
            a.parse().ok()?,
            b.parse::<u64>().ok()?.min(size.checked_sub(1)?),
        ),
    };
    (s <= e && e < size).then_some((s, e.min(s + RAW_MAX - 1)))
}

async fn raw(State(app): S, Query(q): Query<PathQ>, req_h: HeaderMap) -> R<Response> {
    use tokio::io::{AsyncReadExt, AsyncSeekExt};
    let nf = |e: std::io::Error| ApiErr(StatusCode::NOT_FOUND, e.to_string());
    let p = files::jail(std::path::Path::new(&app.root), &q.path).map_err(forbid)?;
    let mut f = tokio::fs::File::open(&p).await.map_err(nf)?;
    let size = f.metadata().await.map_err(nf)?.len();
    let range = req_h.get(header::RANGE).and_then(|v| v.to_str().ok());
    let mut h = HeaderMap::new();
    h.insert(header::ACCEPT_RANGES, "bytes".parse().expect("static"));
    let (status, s, e) = match range {
        Some(r) => match parse_range(r, size) {
            Some((s, e)) => (StatusCode::PARTIAL_CONTENT, s, e),
            None => {
                h.insert(
                    header::CONTENT_RANGE,
                    format!("bytes */{size}").parse().expect("ascii"),
                );
                return Ok((StatusCode::RANGE_NOT_SATISFIABLE, h).into_response());
            }
        },
        // ponytail: no-Range responses buffer the whole file; stream them with tokio_util ReaderStream once it is approved.
        None => (StatusCode::OK, 0, size.saturating_sub(1)),
    };
    let len = if size == 0 { 0 } else { e - s + 1 };
    f.seek(std::io::SeekFrom::Start(s)).await.map_err(nf)?;
    let mut bytes = Vec::with_capacity(len as usize);
    f.take(len).read_to_end(&mut bytes).await.map_err(nf)?;
    if status == StatusCode::PARTIAL_CONTENT {
        h.insert(
            header::CONTENT_RANGE,
            format!("bytes {s}-{e}/{size}").parse().expect("ascii"),
        );
    }
    let ct = files::mime(&p);
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
    Ok((status, h, Body::from(bytes)).into_response())
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
    // Remote bytes on our origin: sandbox html/svg (blocks script); PDFs must not be sandboxed or Chrome will not render them.
    let sandbox = ct.starts_with("text/html") || ct.starts_with("image/svg");
    Ok((
        [
            (header::CONTENT_TYPE, ct),
            (header::X_CONTENT_TYPE_OPTIONS, "nosniff"),
            (header::CONTENT_DISPOSITION, "inline"),
        ],
        AppendHeaders(sandbox.then_some((header::CONTENT_SECURITY_POLICY, "sandbox"))),
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
        "errors": app.db.recent_errors(5)?.into_iter()
            .map(|(at, source, kind, msg)| json!({"at": at, "source": source, "kind": kind, "msg": msg}))
            .collect::<Vec<_>>(),
        "apple": {
            "state": app.db.kv_get("apple_state")?.unwrap_or_else(|| "ok".into()),
            "since": app.db.kv_get("apple_since")?.and_then(|v| v.parse::<i64>().ok()).unwrap_or(0),
        },
    })))
}

async fn auth_status(State(app): S) -> R<Json<Value>> {
    let n = crate::web::now();
    let kv = |k: &str| app.db.kv_get(k).ok().flatten();
    let probed = kv("auth_probe_at")
        .and_then(|v| v.parse::<i64>().ok())
        .unwrap_or(0);
    if n - probed >= 60 {
        app.db.kv_set("auth_probe_at", &n.to_string())?;
        match app.moodle.probe().await {
            Ok(_) => app.db.kv_set("auth_state", "ok")?,
            Err(crate::errors::CoreError::Auth(_)) => {
                if kv("auth_state").as_deref().unwrap_or("ok") == "ok" {
                    app.db.kv_set("auth_state", "expired")?;
                }
            }
            Err(_) => {} // network trouble says nothing about the token
        }
    }
    Ok(Json(json!({
        "moodle": kv("auth_state").unwrap_or_else(|| "ok".into()),
        "apple": kv("apple_state").unwrap_or_else(|| "ok".into()),
        "apple_auto": kv("apple_auto").unwrap_or_else(|| "ok".into()),
        "apple_error": kv("apple_error").unwrap_or_default(),
        "apple_since": kv("apple_since").and_then(|v| v.parse::<i64>().ok()).unwrap_or(0),
        "last_error": kv("last_error")
            .filter(|e| !e.is_empty())
            .or_else(|| kv("apple_error"))
            .unwrap_or_default(),
        "at": n,
    })))
}

async fn auth_renew(State(app): S) -> (StatusCode, Json<Value>) {
    use crate::web::apple::{self, Imp};
    let moodle = match crate::web::renew_now(&app, 60).await {
        Ok(r) => crate::web::renew_state(&r),
        Err(wait) => {
            return (
                StatusCode::TOO_MANY_REQUESTS,
                Json(json!({"error": "massa aviat", "next_try": wait})),
            )
        }
    };
    app.db.kv_set("auth_probe_at", "0").ok();
    let bad = app
        .db
        .kv_get("apple_state")
        .ok()
        .flatten()
        .unwrap_or_default();
    let apple = if bad == "auth" || bad == "err" {
        match apple::import(&app, 60).await {
            Imp::Ok => "imported",
            Imp::Failed => "failed",
            Imp::Limited(_) => "limited",
        }
    } else {
        "ok"
    };
    (
        StatusCode::OK,
        Json(json!({"moodle": moodle, "apple": apple})),
    )
}

async fn apple_sync(State(app): S) -> (StatusCode, Json<Value>) {
    if crate::web::apple::LOCK.try_lock().is_err() {
        return (
            StatusCode::CONFLICT,
            Json(json!({"error": "ja s'està sincronitzant"})),
        );
    }
    let r = crate::web::apple::sync(&app).await;
    let mut v = json!(r);
    let code = if r.auth() {
        v["error"] = json!(r
            .errs
            .iter()
            .find(|e| e.kind == crate::web::apple::Kind::Auth)
            .map_or("", |e| &e.msg));
        v["kind"] = json!("auth");
        StatusCode::CONFLICT
    } else {
        StatusCode::OK
    };
    (code, Json(v))
}

async fn apple_status(State(app): S) -> Json<Value> {
    Json(crate::web::apple::status(&app))
}

async fn apple_reimport(State(app): S) -> (StatusCode, Json<Value>) {
    use crate::web::apple::Imp;
    if crate::web::apple::LOCK.try_lock().is_err() {
        return (
            StatusCode::CONFLICT,
            Json(json!({"error": "ja s'està sincronitzant"})),
        );
    }
    match crate::web::apple::import(&app, 60).await {
        Imp::Limited(n) => (
            StatusCode::TOO_MANY_REQUESTS,
            Json(json!({"error": "massa aviat", "next_try": n})),
        ),
        Imp::Failed => (
            StatusCode::CONFLICT,
            Json(json!({"error": "no s'ha pogut importar", "kind": "auth"})),
        ),
        Imp::Ok => {
            crate::web::apple::sync(&app).await;
            (StatusCode::OK, Json(crate::web::apple::status(&app)))
        }
    }
}

async fn sync_now(State(app): S) -> R<Json<Value>> {
    if app.syncing.try_lock().is_err() {
        return Err(ApiErr(
            StatusCode::CONFLICT,
            "ja s'està sincronitzant".into(),
        ));
    }
    let ev = crate::web::run_sync(&app, false).await.map_err(upstream)?;
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
    for l in app.db.all_local()? {
        if !l.files.iter().any(|f| f == from) {
            continue;
        }
        app.db.update_local(&l.task_id, |l| {
            let mut seen = std::collections::HashSet::new();
            l.files = l
                .files
                .iter()
                .map(|f| if f == from { to.to_string() } else { f.clone() })
                .filter(|f| seen.insert(f.clone()))
                .collect();
        })?;
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
    let root = app.root.clone();
    let q = b.path.clone();
    let (p, rel) =
        tokio::task::spawn_blocking(move || open_target(std::path::Path::new(&root), &q))
            .await
            .map_err(anyhow::Error::from)?
            .map_err(forbid)?;
    repoint(&app, &b.path, &rel)?;
    let mut ch = tokio::process::Command::new("xdg-open")
        .arg(p)
        .spawn()
        .map_err(anyhow::Error::from)?;
    tokio::spawn(async move {
        let _ = ch.wait().await;
    });
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

#[cfg(test)]
mod frontmatter_tests {
    use super::frontmatter;
    use crate::moodle::Course;
    use crate::sync::indexer::render_index_md;

    #[test]
    fn frontmatter_unescapes_title() {
        let c = Course {
            id: 7,
            fullname: "a\"b\\c\nd \\n".into(),
            shortname: String::new(),
            summary: String::new(),
        };
        let md = render_index_md(&c, "", &[], "t");
        assert_eq!(frontmatter(&md, "title").unwrap(), "a\"b\\c\nd \\n");
        assert_eq!(frontmatter(&md, "course_id").unwrap(), "7");
    }
}
