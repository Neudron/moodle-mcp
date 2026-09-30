//! Jev (opencode.ai systemone): small zero-cost classifier used as a gate and verifier.
//! Every call may fail (free tier, offline): `None` always means "carry on the old way".
use crate::web::env;
use serde_json::{json, Value};
use std::time::Duration;

const URL: &str = "https://opencode.ai/zen/v1/systemone";
/// 100k chars work, 200k give HTTP 400 (probe); stay well under.
const MAX_CHARS: usize = 60_000;

/// Float env knob (`JEV_GATE`, `JEV_KEEP`).
pub fn thr(key: &str, default: f64) -> f64 {
    env::get(key)
        .and_then(|v| v.parse().ok())
        .unwrap_or(default)
}

/// `answers` of one call. The UA must not look like urllib (Cloudflare 1010).
pub async fn ask(state: &str, questions: Value) -> Option<Value> {
    let url = env::get("JEV_URL").unwrap_or_else(|| URL.into());
    post(&url, state, questions).await
}

async fn post(url: &str, state: &str, questions: Value) -> Option<Value> {
    let body = json!({
        "model": env::get("JEV_MODEL").unwrap_or_else(|| "jev-1.13-free".into()),
        "state": state.chars().take(MAX_CHARS).collect::<String>(),
        "questions": questions,
    });
    let r = reqwest::Client::builder()
        .user_agent("moodle-web/0")
        .timeout(Duration::from_secs(30))
        .build()
        .ok()?
        .post(url)
        .json(&body)
        .send()
        .await
        .ok()?;
    if !r.status().is_success() {
        return None;
    }
    let mut v: Value = r.json().await.ok()?;
    Some(v["answers"].take())
}

/// How strongly `instr` holds for `state`, 0..1.
pub async fn noul(state: &str, instr: &str) -> Option<f64> {
    ask(state, json!({"q": {"type": "noul", "instructions": instr}})).await?["q"]["noul"].as_f64()
}

#[cfg(test)]
pub mod mock {
    use axum::{routing::post, Json, Router};
    use serde_json::{json, Value};

    /// Local Jev: the noul is 0.02 when the state contains "NOPE", else 0.9.
    pub async fn serve() -> String {
        let app = Router::new()
            .route(
                "/",
                post(|Json(b): Json<Value>| async move {
                    let p = if b["state"].as_str().unwrap_or("").contains("NOPE") {
                        0.02
                    } else {
                        0.9
                    };
                    let a: serde_json::Map<String, Value> = b["questions"]
                        .as_object()
                        .unwrap()
                        .keys()
                        .map(|k| (k.clone(), json!({"type": "noul", "noul": p})))
                        .collect();
                    Json(json!({"answers": a}))
                }),
            )
            .route(
                "/err",
                post(|| async { axum::http::StatusCode::BAD_REQUEST }),
            );
        let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = l.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(l, app).await.ok() });
        format!("http://{addr}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn post_reads_answers_and_maps_failures_to_none() {
        let u = mock::serve().await;
        let q = json!({"q": {"type": "noul", "instructions": "x"}});
        assert_eq!(post(&u, "hola", q.clone()).await.unwrap()["q"]["noul"], 0.9);
        assert_eq!(
            post(&u, "NOPE", q.clone()).await.unwrap()["q"]["noul"],
            0.02
        );
        assert!(post(&format!("{u}/err"), "x", q.clone()).await.is_none());
        assert!(post("http://127.0.0.1:1/", "x", q).await.is_none());
    }
}
