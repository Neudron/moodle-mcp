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

/// How strongly each `(key, instruction)` holds for `state`, 0..1, in one request.
/// `None` when Jev fails or leaves any key unanswered.
pub async fn nouls(state: &str, qs: &[(&str, &str)]) -> Option<Vec<f64>> {
    let q: serde_json::Map<String, Value> = qs
        .iter()
        .map(|(k, i)| (k.to_string(), json!({"type": "noul", "instructions": i})))
        .collect();
    let a = ask(state, Value::Object(q)).await?;
    qs.iter().map(|(k, _)| a[*k]["noul"].as_f64()).collect()
}

/// How strongly `instr` holds for `state`, 0..1.
pub async fn noul(state: &str, instr: &str) -> Option<f64> {
    nouls(state, &[("q", instr)]).await?.pop()
}

#[cfg(test)]
pub mod mock {
    use axum::{routing::post, Json, Router};
    use serde_json::{json, Value};

    /// Tests that set `JEV_URL` hold this.
    pub static ENV: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

    /// Local Jev: the noul is 0.02 when the state contains "NOPE", else 0.05 for the keys
    /// `quiz` and `notes` and 0.9 for any other.
    pub async fn serve() -> String {
        let app = Router::new()
            .route(
                "/",
                post(|Json(b): Json<Value>| async move {
                    let nope = b["state"].as_str().unwrap_or("").contains("NOPE");
                    let a: serde_json::Map<String, Value> = b["questions"]
                        .as_object()
                        .unwrap()
                        .keys()
                        .map(|k| {
                            let p = match (nope, k.as_str()) {
                                (true, _) => 0.02,
                                (_, "quiz" | "notes") => 0.05,
                                _ => 0.9,
                            };
                            (k.clone(), json!({"type": "noul", "noul": p}))
                        })
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

    #[tokio::test]
    async fn nouls_answers_every_key_in_order() {
        let _g = mock::ENV.lock().await;
        let u = mock::serve().await;
        let qs = [("task", "a"), ("quiz", "b"), ("notes", "c")];
        std::env::set_var("JEV_URL", &u);
        assert_eq!(nouls("hola", &qs).await, Some(vec![0.9, 0.05, 0.05]));
        assert_eq!(nouls("NOPE", &qs).await, Some(vec![0.02; 3]));
        assert_eq!(noul("hola", "a").await, Some(0.9));
        std::env::set_var("JEV_URL", format!("{u}/err"));
        assert_eq!(nouls("x", &qs).await, None);
        std::env::remove_var("JEV_URL");
    }
}
