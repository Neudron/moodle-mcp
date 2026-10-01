//! OnlyOffice Document Server integration (spec: Editing).
use crate::web::{env, files, App};
use axum::{
    extract::{Query, State},
    http::{HeaderMap, StatusCode},
    routing::{get, post},
    Json, Router,
};
use jsonwebtoken::{decode, encode, DecodingKey, EncodingKey, Header, Validation};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::path::Path;
use std::sync::Arc;

pub fn doc_type(ext: &str) -> Option<&'static str> {
    match ext.to_lowercase().as_str() {
        "docx" | "doc" | "odt" | "rtf" | "txt" => Some("word"),
        "xlsx" | "xls" | "ods" | "csv" => Some("cell"),
        "pptx" | "ppt" | "odp" => Some("slide"),
        _ => None,
    }
}

#[derive(Serialize, Deserialize)]
struct FileClaims {
    path: String,
    exp: i64,
}

pub fn file_token(secret: &str, rel: &str, now: i64) -> String {
    let c = FileClaims {
        path: rel.into(),
        exp: now + 86_400,
    };
    encode(
        &Header::default(),
        &c,
        &EncodingKey::from_secret(secret.as_bytes()),
    )
    .unwrap_or_default()
}

pub fn verify_file_token(secret: &str, t: &str) -> Result<String, String> {
    verify_token(secret, t, true)
}

fn verify_token(secret: &str, t: &str, check_exp: bool) -> Result<String, String> {
    let mut v = Validation::default();
    v.validate_exp = check_exp;
    decode::<FileClaims>(t, &DecodingKey::from_secret(secret.as_bytes()), &v)
        .map(|d| d.claims.path)
        .map_err(|e| e.to_string())
}

pub fn amin_copy(root: &Path, rel: &str) -> Result<String, String> {
    amin_copy_in(root, rel, &files::home())
}

fn amin_copy_in(root: &Path, rel: &str, home: &Path) -> Result<String, String> {
    let src = files::jail_in(root, rel, home)?;
    let stem = src
        .file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_default();
    let croot = root.canonicalize().map_err(|e| e.to_string())?;
    let rel_of = |p: &Path| -> Result<String, String> {
        Ok(p.strip_prefix(&croot)
            .map_err(|e| e.to_string())?
            .to_string_lossy()
            .into_owned())
    };
    if !src.starts_with(&croot) {
        return Ok(rel.to_string()); // home pick via submit/ link: edit in place
    }
    if stem.ends_with("_amin") {
        return rel_of(&src);
    }
    let ext = src
        .extension()
        .map(|e| e.to_string_lossy().into_owned())
        .unwrap_or_default();
    let dst = src.with_file_name(format!("{stem}_amin.{ext}"));
    if !dst.exists() {
        std::fs::copy(&src, &dst).map_err(|e| e.to_string())?;
    }
    rel_of(&dst)
}

fn hex_sha(s: &str) -> String {
    Sha256::digest(s.as_bytes())
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

pub fn editor_config(secret: &str, rel: &str, mtime: i64, host_base: &str, now: i64) -> Value {
    let ext = rel.rsplit('.').next().unwrap_or("");
    let key: String = hex_sha(&format!("{rel}:{mtime}"))
        .chars()
        .take(20)
        .collect();
    let t = file_token(secret, rel, now);
    let mut cfg = json!({
        "document": { "fileType": ext.to_lowercase(), "key": key, "title": rel.rsplit('/').next().unwrap_or(rel),
                      "url": format!("{host_base}/oo/file?t={t}") },
        "documentType": doc_type(ext).unwrap_or("word"),
        "editorConfig": { "callbackUrl": format!("{host_base}/oo/callback?t={t}"), "lang": "ca", "mode": "edit",
                          "user": { "id": "neu", "name": "neu" }, "customization": { "forcesave": true } },
        "height": "100%", "width": "100%",
    });
    let token = encode(
        &Header::default(),
        &cfg,
        &EncodingKey::from_secret(secret.as_bytes()),
    )
    .unwrap_or_default();
    cfg["token"] = Value::String(token);
    cfg
}

type E = (StatusCode, String);
fn secret() -> Result<String, E> {
    env::get("ONLYOFFICE_JWT").ok_or((
        StatusCode::SERVICE_UNAVAILABLE,
        "ONLYOFFICE_JWT no configurat".into(),
    ))
}

#[derive(Deserialize)]
struct TQ {
    t: String,
}

async fn oo_file(State(app): State<Arc<App>>, Query(q): Query<TQ>) -> Result<Vec<u8>, E> {
    let rel = verify_file_token(&secret()?, &q.t).map_err(|e| (StatusCode::FORBIDDEN, e))?;
    let p = files::jail(Path::new(&app.root), &rel).map_err(|e| (StatusCode::FORBIDDEN, e))?;
    tokio::fs::read(p)
        .await
        .map_err(|e| (StatusCode::NOT_FOUND, e.to_string()))
}

/// The file may have moved to `submit/_lliurat/` while the editor was open.
fn locate(root: &Path, rel: &str) -> Result<std::path::PathBuf, String> {
    files::jail(root, rel).or_else(|e| match rel.strip_prefix("submit/") {
        Some(r) if !r.starts_with("_lliurat/") => {
            files::jail(root, &format!("submit/_lliurat/{r}"))
        }
        _ => Err(e),
    })
}

async fn save_edit(app: &App, rel: &str, url: &str) -> Result<(), E> {
    let oo = env::get("ONLYOFFICE_URL").unwrap_or_else(|| "http://127.0.0.1:8080".into());
    // Trailing slash: "http://host:8080" must not match "http://host:80801/..." or "http://host:8080.evil.com".
    if !url.starts_with(&format!("{}/", oo.trim_end_matches('/'))) {
        return Err((
            StatusCode::FORBIDDEN,
            "download url outside ONLYOFFICE_URL".into(),
        ));
    }
    let gw = |e: reqwest::Error| (StatusCode::BAD_GATEWAY, e.without_url().to_string());
    let mut r = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(120))
        .build()
        .map_err(gw)?
        .get(url)
        .send()
        .await
        .and_then(|r| r.error_for_status())
        .map_err(gw)?;
    let mut bytes = Vec::new();
    while let Some(c) = r.chunk().await.map_err(gw)? {
        if (bytes.len() + c.len()) as u64 > super::preview::MAX_BYTES {
            return Err((StatusCode::PAYLOAD_TOO_LARGE, "download too large".into()));
        }
        bytes.extend_from_slice(&c);
    }
    let p = locate(Path::new(&app.root), rel).map_err(|e| (StatusCode::FORBIDDEN, e))?;
    let ioe = |e: std::io::Error| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string());
    let name = p.file_name().map(|n| n.to_string_lossy().into_owned());
    let name = name.unwrap_or_default();
    let bak = p.with_file_name(format!(".{name}.bak"));
    std::fs::copy(&p, &bak).map_err(ioe)?;
    let tmp = p.with_file_name(format!(".{name}.tmp"));
    std::fs::write(&tmp, &bytes).map_err(ioe)?;
    std::fs::rename(&tmp, &p).map_err(ioe)?;
    Ok(())
}

async fn oo_callback(
    State(app): State<Arc<App>>,
    Query(q): Query<TQ>,
    headers: HeaderMap,
    Json(body): Json<Value>,
) -> Result<Json<Value>, E> {
    let secret = secret()?;
    // Editing sessions outlive the 24 h URL token; the body JWT is the real auth here.
    let rel = verify_token(&secret, &q.t, false).map_err(|e| (StatusCode::FORBIDDEN, e))?;
    let mut val = Validation::default();
    val.required_spec_claims.clear();
    val.validate_exp = false;
    // Document Server sends the JWT in the body and in the Authorization header; accept either.
    let tok = body["token"]
        .as_str()
        .or_else(|| {
            headers
                .get("authorization")
                .and_then(|v| v.to_str().ok())
                .and_then(|v| v.strip_prefix("Bearer "))
        })
        .unwrap_or("");
    let claims = decode::<Value>(tok, &DecodingKey::from_secret(secret.as_bytes()), &val)
        .map_err(|e| (StatusCode::FORBIDDEN, e.to_string()))?
        .claims;
    // Header tokens wrap the body as {"payload": {...}}; body tokens carry it flat.
    let payload = if claims["status"].is_null() {
        claims["payload"].clone()
    } else {
        claims
    };
    let status = payload["status"].as_i64().unwrap_or(0);
    if status == 2 || status == 6 {
        if let Err((code, why)) = save_edit(&app, &rel, payload["url"].as_str().unwrap_or("")).await
        {
            tracing::warn!(file = %rel, reason = %why, "onlyoffice save failed");
            return Err((code, why));
        }
        tracing::info!(file = %rel, "onlyoffice save");
    }
    Ok(Json(json!({"error": 0})))
}

pub fn oo_router() -> Router<Arc<App>> {
    Router::new()
        .route("/oo/file", get(oo_file))
        .route("/oo/callback", post(oo_callback))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn doc_types() {
        assert_eq!(doc_type("docx"), Some("word"));
        assert_eq!(doc_type("XLSX"), Some("cell"));
        assert_eq!(doc_type("pptx"), Some("slide"));
        assert_eq!(doc_type("pdf"), None);
    }

    #[test]
    fn file_token_roundtrip_and_tamper() {
        let t = file_token("s3cret", "submit/a_amin.docx", 1_000);
        assert!(verify_file_token("s3cret", &t).is_err(), "expired");
        let t = file_token(
            "s3cret",
            "submit/a_amin.docx",
            chrono::Utc::now().timestamp(),
        );
        assert_eq!(
            verify_file_token("s3cret", &t).unwrap(),
            "submit/a_amin.docx"
        );
        assert!(verify_file_token("other", &t).is_err());
    }

    #[test]
    fn amin_copy_never_touches_original() {
        let r = std::env::temp_dir().join(format!("mweb-oo-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&r);
        std::fs::create_dir_all(r.join("C")).unwrap();
        std::fs::write(r.join("C/Tasca RA1.docx"), "orig").unwrap();
        let rel = amin_copy(&r, "C/Tasca RA1.docx").unwrap();
        assert_eq!(rel, "C/Tasca RA1_amin.docx");
        std::fs::write(r.join(&rel), "edited").unwrap();
        assert_eq!(amin_copy(&r, "C/Tasca RA1.docx").unwrap(), rel);
        assert_eq!(
            std::fs::read_to_string(r.join(&rel)).unwrap(),
            "edited",
            "existing copy never overwritten"
        );
        assert_eq!(
            amin_copy(&r, "C/Tasca RA1_amin.docx").unwrap(),
            "C/Tasca RA1_amin.docx"
        );
        assert_eq!(
            std::fs::read_to_string(r.join("C/Tasca RA1.docx")).unwrap(),
            "orig"
        );
    }

    #[test]
    fn config_is_signed() {
        let v = editor_config(
            "s",
            "C/x_amin.docx",
            5,
            "http://host.docker.internal:8787",
            chrono::Utc::now().timestamp(),
        );
        assert_eq!(v["documentType"], "word");
        assert_eq!(v["editorConfig"]["lang"], "ca");
        assert!(v["document"]["url"]
            .as_str()
            .unwrap()
            .starts_with("http://host.docker.internal:8787/oo/file?t="));
        let mut val = jsonwebtoken::Validation::default();
        val.required_spec_claims.clear();
        val.validate_exp = false;
        assert!(jsonwebtoken::decode::<serde_json::Value>(
            v["token"].as_str().unwrap(),
            &jsonwebtoken::DecodingKey::from_secret(b"s"),
            &val
        )
        .is_ok());
    }

    #[test]
    fn amin_copy_leaves_home_pick_alone() {
        let h = std::env::temp_dir().join(format!("mweb-amin-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&h);
        std::fs::create_dir_all(h.join("smx/submit/M/T")).unwrap();
        std::fs::write(h.join("a.docx"), "x").unwrap();
        std::os::unix::fs::symlink(h.join("a.docx"), h.join("smx/submit/M/T/a.docx")).unwrap();
        let rel = "submit/M/T/a.docx";
        assert_eq!(amin_copy_in(&h.join("smx"), rel, &h).unwrap(), rel);
        assert!(!h.join("a_amin.docx").exists());
    }
}
