//! GET /api/preview: any file as something a browser can show (spec WP2).
use crate::web::{
    api::{bad, forbid, ApiErr, R},
    env, files, office, App,
};
use axum::{
    extract::{Query, State},
    http::{header, HeaderValue, StatusCode},
    response::{IntoResponse, Response},
    routing::get,
    Router,
};
use serde::Deserialize;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::{
    path::{Path, PathBuf},
    sync::Arc,
};

type S = State<Arc<App>>;

pub fn sha_hex(b: &[u8]) -> String {
    Sha256::digest(b)
        .iter()
        .map(|x| format!("{x:02x}"))
        .collect()
}

fn ext_of(name: &str) -> String {
    Path::new(name)
        .extension()
        .map(|e| e.to_string_lossy().to_lowercase())
        .unwrap_or_default()
}

fn reply(ct: &'static str, body: Vec<u8>) -> Response {
    // zip and unknown binaries are download-only
    let dl = ct == "application/octet-stream" || ct == "application/zip";
    let mut r = (
        [
            (header::CONTENT_TYPE, ct),
            (header::X_CONTENT_TYPE_OPTIONS, "nosniff"),
            (
                header::CONTENT_DISPOSITION,
                if dl { "attachment" } else { "inline" },
            ),
        ],
        body,
    )
        .into_response();
    if ct.starts_with("text/html") || ct.starts_with("image/svg") {
        r.headers_mut().insert(
            header::CONTENT_SECURITY_POLICY,
            HeaderValue::from_static("sandbox"),
        );
    }
    r
}

/// Cap for anything held in memory (previews, DS conversion sources).
const MAX_BYTES: u64 = 64 << 20;

fn too_big() -> ApiErr {
    ApiErr(StatusCode::PAYLOAD_TOO_LARGE, "fitxer massa gran".into())
}

const OFFICE: &[&str] = &[
    "doc", "docx", "odt", "rtf", "ppt", "pptx", "odp", "xls", "xlsx", "ods",
];

fn previews(app: &App) -> PathBuf {
    Path::new(&app.root).join(".moodle/previews")
}

fn oo_base() -> String {
    env::get("ONLYOFFICE_URL")
        .unwrap_or_else(|| "http://127.0.0.1:8080".into())
        .trim_end_matches('/')
        .to_string()
}

fn secret() -> Result<String, ApiErr> {
    env::get("ONLYOFFICE_JWT").ok_or(ApiErr(
        StatusCode::SERVICE_UNAVAILABLE,
        "OnlyOffice no disponible".into(),
    ))
}

/// `<64 hex>.<ext>`: the only names `/oo/psrc` will serve.
fn staged_name(n: &str) -> bool {
    n.split_once('.').is_some_and(|(s, e)| {
        s.len() == 64
            && s.bytes().all(|b| b.is_ascii_hexdigit())
            && (1..=5).contains(&e.len())
            && e.bytes().all(|b| b.is_ascii_alphanumeric())
    })
}

/// Bytes -> `output` ("pdf" | "txt") through OnlyOffice ConvertService.
/// Cached at `.moodle/previews/<sha>.<output>`. DS fetches the source from `/oo/psrc`.
pub async fn convert(app: &App, bytes: &[u8], ext: &str, output: &str) -> Result<Vec<u8>, ApiErr> {
    let secret = secret()?;
    let sha = sha_hex(bytes);
    let dir = previews(app);
    let cached = dir.join(format!("{sha}.{output}"));
    if let Ok(b) = tokio::fs::read(&cached).await {
        return Ok(b);
    }
    let ioe = |e: std::io::Error| ApiErr(StatusCode::INTERNAL_SERVER_ERROR, e.to_string());
    tokio::fs::create_dir_all(dir.join("src"))
        .await
        .map_err(ioe)?;
    let name = format!("{sha}.{ext}");
    let src = dir.join("src").join(&name);
    if !src.exists() {
        tokio::fs::write(&src, bytes).await.map_err(ioe)?;
    }
    let host = env::get("OO_HOST").unwrap_or_else(|| "host.docker.internal".into());
    let url = format!(
        "http://{host}:{}/oo/psrc?t={}",
        app.port,
        office::file_token(&secret, &name, crate::web::now())
    );
    let mut body = json!({"async": false, "filetype": ext, "outputtype": output,
        "key": &sha_hex(format!("{sha}:{output}").as_bytes())[..20],
        "title": format!("preview.{ext}"), "url": url});
    let sign = |v: &Value| {
        jsonwebtoken::encode(
            &jsonwebtoken::Header::default(),
            v,
            &jsonwebtoken::EncodingKey::from_secret(secret.as_bytes()),
        )
        .unwrap_or_default()
    };
    let (token, auth) = (sign(&body), sign(&json!({ "payload": &body })));
    body["token"] = token.into();
    let gw = |e: reqwest::Error| ApiErr(StatusCode::BAD_GATEWAY, e.without_url().to_string());
    let http = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(120))
        .build()
        .map_err(gw)?;
    let r: Value = http
        .post(format!("{}/ConvertService.ashx", oo_base()))
        .bearer_auth(auth)
        .json(&body)
        .send()
        .await
        .and_then(|r| r.error_for_status())
        .map_err(gw)?
        .json()
        .await
        .map_err(gw)?;
    let fail = |why: String| {
        ApiErr(
            StatusCode::BAD_GATEWAY,
            format!("conversió fallida ({why})"),
        )
    };
    if let Some(e) = r["error"].as_i64() {
        return Err(fail(e.to_string()));
    }
    let file_url = r["fileUrl"]
        .as_str()
        .filter(|_| r["endConvert"] == true)
        .ok_or_else(|| fail("sense resultat".into()))?;
    // DS reports its own public host: keep only the path, re-anchored on ONLYOFFICE_URL.
    let path = file_url.splitn(4, '/').nth(3).unwrap_or("");
    let out = http
        .get(format!("{}/{path}", oo_base()))
        .send()
        .await
        .and_then(|r| r.error_for_status())
        .map_err(gw)?
        .bytes()
        .await
        .map_err(gw)?
        .to_vec();
    tokio::fs::write(&cached, &out).await.map_err(ioe)?;
    Ok(out)
}

#[derive(Deserialize)]
struct T {
    t: String,
}

async fn psrc(State(app): S, Query(q): Query<T>) -> R<Vec<u8>> {
    let name = office::verify_file_token(&secret()?, &q.t).map_err(forbid)?;
    if !staged_name(&name) {
        return Err(forbid("nom no permès"));
    }
    tokio::fs::read(previews(&app).join("src").join(name))
        .await
        .map_err(|e| ApiErr(StatusCode::NOT_FOUND, e.to_string()))
}

#[derive(Deserialize)]
struct Q {
    path: Option<String>,
    moodle: Option<String>,
}

/// Moodle file bytes, cached by URL (pluginfile URLs carry the item id, so a changed file gets a new URL).
/// ponytail: no revalidation of an unchanged URL; delete `.moodle/previews/src/u-*` to force a refetch.
async fn moodle_bytes(app: &App, url: &str) -> R<Vec<u8>> {
    let base = app.moodle.base().trim_end_matches('/');
    if !url.starts_with(&format!("{base}/")) {
        return Err(forbid("url fora de Moodle"));
    }
    let clean = url.split(['?', '#']).next().unwrap_or("");
    let cached = previews(app)
        .join("src")
        .join(format!("u-{}", sha_hex(clean.as_bytes())));
    if let Ok(b) = tokio::fs::read(&cached).await {
        return Ok(b);
    }
    // ponytail: download buffers before this check; stream to disk if huge Moodle files matter.
    let b = app.moodle.download(url).await?;
    if b.len() as u64 > MAX_BYTES {
        return Err(too_big());
    }
    if tokio::fs::create_dir_all(cached.parent().unwrap_or(Path::new(".")))
        .await
        .is_ok()
    {
        tokio::fs::write(&cached, &b).await.ok();
    }
    Ok(b)
}

async fn preview(State(app): S, Query(q): Query<Q>) -> R<Response> {
    let (bytes, name) = match (q.path, q.moodle) {
        (Some(p), None) => {
            let f = files::jail(Path::new(&app.root), &p).map_err(forbid)?;
            let nf = |e: std::io::Error| ApiErr(StatusCode::NOT_FOUND, e.to_string());
            if tokio::fs::metadata(&f).await.map_err(nf)?.len() > MAX_BYTES {
                return Err(too_big());
            }
            (tokio::fs::read(&f).await.map_err(nf)?, p)
        }
        (None, Some(u)) => {
            let b = moodle_bytes(&app, &u).await?;
            (b, u.split(['?', '#']).next().unwrap_or("").to_string())
        }
        _ => return Err(bad("cal path o moodle")),
    };
    let ext = ext_of(&name);
    if OFFICE.contains(&ext.as_str()) {
        return Ok(reply(
            "application/pdf",
            convert(&app, &bytes, &ext, "pdf").await?,
        ));
    }
    if ext == "url" {
        let mut t = bytes;
        if let Ok(Some(c)) = app.db.cache_get(&sha_hex(&t)) {
            t.extend(b"\n\n");
            t.extend(c.as_bytes());
        }
        return Ok(reply("text/plain; charset=utf-8", t));
    }
    Ok(reply(files::mime(Path::new(&name)), bytes))
}

pub fn router() -> Router<Arc<App>> {
    Router::new()
        .route("/api/preview", get(preview))
        .route("/oo/psrc", get(psrc))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn staged_names() {
        let h = "a".repeat(64);
        assert!(staged_name(&format!("{h}.docx")));
        assert!(!staged_name(&format!("{h}.docx/x")));
        assert!(!staged_name(&format!("../{h}.docx")));
        assert!(!staged_name(&format!("{}.docx", "a".repeat(63))));
        assert!(!staged_name(&h));
    }
}
