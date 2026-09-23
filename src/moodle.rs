//! Moodle webservice client (Task 2: P1 network/robustness + security core).
//!
//! Two API layers: the original single-shot [`Moodle::call`] /
//! [`Moodle::download`] (kept for Task 1 compatibility) and the robust
//! [`Moodle::call_with_retry`] / [`Moodle::download_resumable`] consumed by
//! the sync engine (Task 3).
//!
//! Robustness policy (plan #1001–1050): retries with `500ms x 2^n` backoff
//! capped at 15s + jitter, `Retry-After` honored, retries only for
//! 429 / 5xx / timeouts; connect 5s, API 15s, file 60s timeouts;
//! keep-alive pool (idle 30s, 8 per host); gzip; no cookie jar; manual
//! redirects (max 5) that strip the token cross-host; non-http(s) refused.
//! A shared 8/s (burst 16) governor and a 5-failures/30s-cooldown breaker
//! guard every call. HTTP 401/403 map to [`CoreError::Auth`] (abort),
//! 404 to [`CoreError::NotFound`] (row error, sync continues).
//!
//! Security: the token travels only in the POST form body (API) or a
//! same-host `?token=` query (files). Error strings, spans and `Debug`
//! output never carry it (see [`redact_token`]).

use anyhow::{bail, Context, Result};
use serde::Deserialize;
use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant};

use crate::errors::{redact_token, CoreError};
use crate::retry;

const API_TIMEOUT: Duration = Duration::from_secs(15);
const FILE_TIMEOUT: Duration = Duration::from_secs(60);
const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);
const POOL_IDLE_TIMEOUT: Duration = Duration::from_secs(30);
const POOL_MAX_IDLE_PER_HOST: usize = 8;
const MAX_REDIRECT_HOPS: u32 = 5;
const SLOW_CALL_WARN: Duration = Duration::from_secs(10);
const SLOW_FILE_WARN: Duration = Duration::from_secs(30);

pub struct Moodle {
    base: String,
    token: String,
    http: reqwest::Client,
    limiter: Arc<retry::RateLimiter>,
    breaker: Arc<retry::CircuitBreaker>,
}

// `Debug` must never dump the token (plan #1053): hand-rolled redaction.
impl std::fmt::Debug for Moodle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Moodle")
            .field("base", &self.base)
            .field("token", &"REDACTED")
            .field("http", &self.http)
            .field("limiter", &self.limiter)
            .field("breaker", &self.breaker)
            .finish()
    }
}

impl Clone for Moodle {
    fn clone(&self) -> Self {
        Self {
            base: self.base.clone(),
            token: self.token.clone(),
            http: self.http.clone(),
            limiter: Arc::clone(&self.limiter),
            breaker: Arc::clone(&self.breaker),
        }
    }
}

/// Verified file-download outcome: the `.part` file at the requested path
/// holds `size` bytes whose SHA-256 is `sha256`.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct FileMeta {
    pub sha256: String,
    pub size: u64,
    pub timemodified: i64,
    pub mimetype: String,
}

/// Outcome of one download attempt: done, abort (no retry), or retryable.
enum Attempt {
    Done(FileMeta),
    Abort(CoreError),
    Retryable {
        after: Option<Duration>,
        kind: CoreError,
    },
}

impl Attempt {
    fn retryable(kind: CoreError) -> Self {
        Self::Retryable { after: None, kind }
    }
}

#[derive(Debug, serde::Serialize, Deserialize)]
pub struct Course {
    pub id: i64,
    #[serde(default)]
    pub fullname: String,
    #[serde(default)]
    pub shortname: String,
    #[serde(default)]
    pub summary: String,
}

#[derive(Debug, Clone, serde::Serialize, Deserialize)]
pub struct ModuleFile {
    #[serde(default)]
    pub filename: String,
    #[serde(default)]
    pub filesize: i64,
    #[serde(default)]
    pub mimetype: String,
    #[serde(default)]
    pub fileurl: Option<String>,
    /// Unix seconds from `core_course_get_contents` (`timemodified`).
    /// Absent in old fixtures → 0 (unknown).
    #[serde(default)]
    pub timemodified: i64,
    /// Moodle `contenthash` (content identifier). Absent → empty.
    #[serde(default)]
    pub contenthash: String,
}

#[derive(Debug, serde::Serialize, Deserialize)]
pub struct CourseModule {
    pub id: i64,
    #[serde(default)]
    pub name: String,
    #[serde(default, rename = "modname")]
    pub module_type: String,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub contents: Option<Vec<ModuleFile>>,
}

#[derive(Debug, serde::Serialize, Deserialize)]
pub struct CourseSection {
    pub id: i64,
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub summary: String,
    #[serde(default)]
    pub position: i64,
    #[serde(default)]
    pub modules: Vec<CourseModule>,
}

impl Moodle {
    pub fn new(base: &str, token: &str) -> Self {
        if base.starts_with("http://") {
            tracing::warn!("moodle base uses plain http; prefer https");
        }
        let tuned = reqwest::Client::builder()
            .user_agent("moodle-mcp/0.2")
            .connect_timeout(CONNECT_TIMEOUT)
            .timeout(FILE_TIMEOUT)
            .pool_idle_timeout(POOL_IDLE_TIMEOUT)
            .pool_max_idle_per_host(POOL_MAX_IDLE_PER_HOST)
            .gzip(true)
            .redirect(reqwest::redirect::Policy::none())
            .build();
        // Builder failure is near-impossible (no proxy/cookies configured);
        // fall back to a plain client rather than panic (no expect()).
        let http = tuned.unwrap_or_else(|_| reqwest::Client::new());
        Self {
            base: base.trim_end_matches('/').to_string(),
            token: token.to_string(),
            http,
            limiter: Arc::new(retry::RateLimiter::default_governor()),
            breaker: Arc::new(retry::CircuitBreaker::new()),
        }
    }

    pub fn from_env() -> Result<Self> {
        let base = std::env::var("MOODLE_URL").map_err(|_| {
            anyhow::anyhow!("MOODLE_URL is not set — copy .env.example to .env and fill it in")
        })?;
        let token_file = std::env::var("MOODLE_TOKEN_FILE")
            .unwrap_or_else(|_| format!("{}/.moodle/token", Self::moodle_root()));
        let token = std::fs::read_to_string(&token_file)
            .with_context(|| format!("reading token file {token_file}"))?
            .trim()
            .to_string();
        if token.len() < 16 {
            bail!("token file {token_file} does not look like a token");
        }
        Ok(Self::new(&base, &token))
    }

    pub fn moodle_root() -> String {
        std::env::var("MOODLE_ROOT").unwrap_or_else(|_| {
            let cwd = std::env::current_dir()
                .map(|p| p.display().to_string())
                .unwrap_or_default();
            if cwd.ends_with("/smx") {
                cwd
            } else {
                format!("{cwd}/smx")
            }
        })
    }

    /// Single-shot webservice call (no retry; kept for Task 1 compatibility).
    pub async fn call(
        &self,
        function: &str,
        params: &[(&str, String)],
    ) -> Result<serde_json::Value> {
        let url = format!("{}/webservice/rest/server.php", self.base);
        let mut form: Vec<(&str, String)> = vec![
            ("wstoken", self.token.clone()),
            ("wsfunction", function.to_string()),
            ("moodlewsrestformat", "json".into()),
        ];
        form.extend(params.iter().map(|p| (p.0, p.1.clone())));
        let resp = self
            .http
            .post(&url)
            .form(&form)
            .timeout(API_TIMEOUT)
            .send()
            .await
            .map_err(|e| anyhow::anyhow!("moodle request: {}", redact_token(&e.to_string())))?;
        let status = resp.status();
        let body: serde_json::Value = resp
            .json()
            .await
            .map_err(|e| anyhow::anyhow!("moodle json: {}", redact_token(&e.to_string())))?;
        if !status.is_success() {
            bail!("moodle http {status}: {body}");
        }
        if let Some(exc) = body.get("exception") {
            bail!(
                "moodle exception {exc}: {}",
                body.get("message").unwrap_or_default()
            );
        }
        Ok(body)
    }

    /// Webservice call with retry: exp-backoff `500ms x 2^n` (cap 15s) +
    /// jitter, `Retry-After` honored, retries only for 429 / 5xx / timeouts.
    /// 401/403 abort as [`CoreError::Auth`], 404 as [`CoreError::NotFound`].
    pub async fn call_with_retry(
        &self,
        function: &str,
        params: &[(&str, String)],
    ) -> Result<serde_json::Value, CoreError> {
        if retry::offline_blocked() {
            return Err(CoreError::Net(
                "offline mode (MOODLE_OFFLINE): network blocked".into(),
            ));
        }
        if function.trim().is_empty() {
            return Err(CoreError::Invalid("wsfunction must not be empty".into()));
        }
        if !(self.base.starts_with("http://") || self.base.starts_with("https://")) {
            return Err(CoreError::Invalid("moodle base must be http(s)".into()));
        }
        if self.breaker.is_open() {
            return Err(CoreError::Net(
                "circuit breaker open: backing off the host".into(),
            ));
        }
        let url = format!("{}/webservice/rest/server.php", self.base);
        let mut attempt: u32 = 0;
        loop {
            self.limiter.acquire().await;
            let span = tracing::info_span!("moodle.call", function = %function);
            let _guard = span.enter();
            let started = Instant::now();
            let mut form: Vec<(&str, String)> = Vec::with_capacity(params.len() + 3);
            form.push(("wstoken", self.token.clone()));
            form.push(("wsfunction", function.to_string()));
            form.push(("moodlewsrestformat", "json".into()));
            form.extend(params.iter().map(|p| (p.0, p.1.clone())));
            let send = self
                .http
                .post(&url)
                .form(&form)
                .timeout(API_TIMEOUT)
                .send()
                .await;
            if started.elapsed() > SLOW_CALL_WARN {
                tracing::warn!(
                    function = %function,
                    elapsed_ms = started.elapsed().as_millis() as u64,
                    "slow moodle call"
                );
            }
            match send {
                Err(e) if e.is_timeout() => {
                    if attempt >= retry::MAX_RETRIES {
                        self.breaker.record_failure();
                        return Err(CoreError::Net(format!(
                            "moodle {function} timeout after {} attempts",
                            attempt + 1
                        )));
                    }
                    tokio::time::sleep(retry::backoff_delay(attempt)).await;
                    attempt += 1;
                }
                Err(e) => {
                    // DNS / connect / refused: mapped to NET, no retry.
                    self.breaker.record_failure();
                    return Err(CoreError::Net(format!(
                        "moodle {function} transport: {}",
                        redact_token(&e.to_string())
                    )));
                }
                Ok(resp) => {
                    let status = resp.status();
                    if status == reqwest::StatusCode::UNAUTHORIZED
                        || status == reqwest::StatusCode::FORBIDDEN
                    {
                        return Err(CoreError::Auth(format!(
                            "moodle {function} http {status} (check token)"
                        )));
                    }
                    if status == reqwest::StatusCode::NOT_FOUND {
                        return Err(CoreError::NotFound(format!(
                            "moodle {function} http {status}"
                        )));
                    }
                    if status == reqwest::StatusCode::TOO_MANY_REQUESTS {
                        let after = retry::retry_after_from_headers(resp.headers());
                        if attempt >= retry::MAX_RETRIES {
                            self.breaker.record_failure();
                            return Err(CoreError::RateLimited(
                                after.map(|d| d.as_secs()).unwrap_or(0),
                            ));
                        }
                        let mut wait = retry::backoff_delay(attempt);
                        if let Some(ra) = after {
                            wait = wait.max(ra);
                        }
                        tokio::time::sleep(wait).await;
                        attempt += 1;
                        continue;
                    }
                    if status.is_server_error() {
                        if attempt >= retry::MAX_RETRIES {
                            self.breaker.record_failure();
                            return Err(CoreError::Net(format!(
                                "moodle {function} http {status} after {} attempts",
                                attempt + 1
                            )));
                        }
                        tokio::time::sleep(retry::backoff_delay(attempt)).await;
                        attempt += 1;
                        continue;
                    }
                    if !status.is_success() {
                        if status == reqwest::StatusCode::BAD_REQUEST
                            || status == reqwest::StatusCode::UNPROCESSABLE_ENTITY
                        {
                            return Err(CoreError::Invalid(format!(
                                "moodle {function} http {status}"
                            )));
                        }
                        return Err(CoreError::Net(format!("moodle {function} http {status}")));
                    }
                    let body: serde_json::Value = resp.json().await.map_err(|e| {
                        CoreError::Net(format!(
                            "moodle {function} non-JSON body: {}",
                            redact_token(&e.to_string())
                        ))
                    })?;
                    if body.get("exception").is_some() {
                        let exc = body
                            .get("exception")
                            .and_then(|v| v.as_str())
                            .unwrap_or("moodle_exception");
                        let code = body.get("errorcode").and_then(|v| v.as_str()).unwrap_or("");
                        let msg = body.get("message").and_then(|v| v.as_str()).unwrap_or("");
                        let detail = redact_token(format!("{exc} {code} {msg}").trim());
                        if code.contains("invalidtoken")
                            || code.contains("invalid_token")
                            || msg.contains("Invalid token")
                        {
                            return Err(CoreError::Auth(format!("moodle {function}: {detail}")));
                        }
                        return Err(CoreError::Other(anyhow::anyhow!(
                            "moodle {function} exception: {detail}"
                        )));
                    }
                    self.breaker.record_success();
                    return Ok(body);
                }
            }
        }
    }

    pub async fn site_info(&self) -> Result<serde_json::Value> {
        self.call("core_webservice_get_site_info", &[]).await
    }

    pub async fn courses(&self, user_id: i64) -> Result<Vec<Course>> {
        let v = self
            .call(
                "core_enrol_get_users_courses",
                &[("userid", user_id.to_string())],
            )
            .await?;
        Ok(serde_json::from_value(v)?)
    }

    pub async fn contents(&self, course_id: i64) -> Result<Vec<CourseSection>> {
        let v = self
            .call(
                "core_course_get_contents",
                &[("courseid", course_id.to_string())],
            )
            .await?;
        Ok(serde_json::from_value(v)?)
    }

    /// pluginfile downloads accept token as query param; nothing else must.
    pub async fn download(&self, fileurl: &str) -> Result<Vec<u8>> {
        let normalized =
            Self::normalize_file_url(&self.base, fileurl).map_err(anyhow::Error::new)?;
        self.do_download(&normalized).await
    }

    async fn do_download(&self, url: &str) -> Result<Vec<u8>> {
        let authority = authority_of(url).unwrap_or_default();
        let mut current = url.to_string();
        for _ in 0..=MAX_REDIRECT_HOPS {
            let target = authed_url(&current, &self.token, &authority);
            let resp = self
                .http
                .get(&target)
                .timeout(FILE_TIMEOUT)
                .send()
                .await
                .map_err(|e| anyhow::anyhow!("file download: {}", redact_token(&e.to_string())))?;
            let status = resp.status();
            if status.is_redirection() {
                match redirect_target(&resp, &current)? {
                    Some(next) => {
                        current = next;
                        continue;
                    }
                    None => bail!("download redirect without location"),
                }
            }
            if !status.is_success() {
                bail!("download http {status}");
            }
            let bytes = resp.bytes().await.context("file body")?;
            return Ok(bytes.to_vec());
        }
        bail!("download: too many redirects (max {MAX_REDIRECT_HOPS})");
    }

    /// Resumable pluginfile download into `part`: sends `Range` from the
    /// existing prefix (if any), streams chunks to disk, verifies
    /// content-length / content-range, then returns checksum + meta.
    /// 401/403 abort as [`CoreError::Auth`]; 404 as [`CoreError::NotFound`]
    /// (row error — the sync continues); 429/5xx/timeouts retry.
    pub async fn download_resumable(&self, url: &str, part: &Path) -> Result<FileMeta, CoreError> {
        if retry::offline_blocked() {
            return Err(CoreError::Net(
                "offline mode (MOODLE_OFFLINE): network blocked".into(),
            ));
        }
        let start = Self::normalize_file_url(&self.base, url)?;
        if self.breaker.is_open() {
            return Err(CoreError::Net(
                "circuit breaker open: backing off the host".into(),
            ));
        }
        if let Some(parent) = part.parent() {
            if !parent.as_os_str().is_empty() {
                tokio::fs::create_dir_all(parent).await.map_err(|e| {
                    CoreError::Other(anyhow::anyhow!(
                        "creating download dir {}: {e}",
                        parent.display()
                    ))
                })?;
            }
        }
        let authority = authority_of(&start).unwrap_or_default();
        let mut resume_from = tokio::fs::metadata(part)
            .await
            .map(|m| m.len())
            .unwrap_or(0);
        let mut attempt: u32 = 0;
        loop {
            self.limiter.acquire().await;
            let started = Instant::now();
            match self
                .download_attempt(&start, &authority, part, resume_from)
                .await
            {
                Attempt::Done(meta) => {
                    self.breaker.record_success();
                    if started.elapsed() > SLOW_FILE_WARN {
                        tracing::warn!(
                            size = meta.size,
                            elapsed_ms = started.elapsed().as_millis() as u64,
                            "slow file download"
                        );
                    }
                    return Ok(meta);
                }
                Attempt::Abort(err) => return Err(err),
                Attempt::Retryable { after, kind } => {
                    if attempt >= retry::MAX_RETRIES {
                        self.breaker.record_failure();
                        return Err(kind);
                    }
                    let mut wait = retry::backoff_delay(attempt);
                    if let Some(ra) = after {
                        wait = wait.max(ra);
                    }
                    tokio::time::sleep(wait).await;
                    attempt += 1;
                    // A failed attempt may have appended bytes: resume past them.
                    resume_from = tokio::fs::metadata(part)
                        .await
                        .map(|m| m.len())
                        .unwrap_or(0);
                }
            }
        }
    }

    /// One download attempt: follows redirects (token same-host only),
    /// streams the body, verifies lengths, hashes the result.
    async fn download_attempt(
        &self,
        start: &str,
        authority: &str,
        part: &Path,
        resume_from: u64,
    ) -> Attempt {
        let mut current = start.to_string();
        for _ in 0..=MAX_REDIRECT_HOPS {
            let target = authed_url(&current, &self.token, authority);
            let mut req = self.http.get(&target).timeout(FILE_TIMEOUT);
            if resume_from > 0 {
                req = req.header(reqwest::header::RANGE, format!("bytes={resume_from}-"));
            }
            let resp = match req.send().await {
                Ok(r) => r,
                Err(e) if e.is_timeout() => {
                    return Attempt::retryable(CoreError::Net(
                        "file download timeout, will resume".into(),
                    ));
                }
                Err(e) => {
                    return Attempt::Abort(CoreError::Net(format!(
                        "file download transport: {}",
                        redact_token(&e.to_string())
                    )));
                }
            };
            let status = resp.status();
            if status.is_redirection() {
                match redirect_target(&resp, &current) {
                    Ok(Some(next)) => {
                        current = next;
                        continue;
                    }
                    Ok(None) => {
                        return Attempt::Abort(CoreError::Net(
                            "file download redirect without location".into(),
                        ));
                    }
                    Err(e) => return Attempt::Abort(CoreError::Other(e)),
                }
            }
            if status == reqwest::StatusCode::UNAUTHORIZED
                || status == reqwest::StatusCode::FORBIDDEN
            {
                return Attempt::Abort(CoreError::Auth(format!("file download http {status}")));
            }
            if status == reqwest::StatusCode::NOT_FOUND {
                return Attempt::Abort(CoreError::NotFound(format!(
                    "file download http {status}: {}",
                    redact_token(strip_query(&current))
                )));
            }
            if status == reqwest::StatusCode::TOO_MANY_REQUESTS {
                let after = retry::retry_after_from_headers(resp.headers());
                let secs = after.map(|d| d.as_secs()).unwrap_or(0);
                return Attempt::Retryable {
                    after,
                    kind: CoreError::RateLimited(secs),
                };
            }
            if status.is_server_error() {
                return Attempt::retryable(CoreError::Net(format!(
                    "file download http {status}, will retry"
                )));
            }
            if status == reqwest::StatusCode::RANGE_NOT_SATISFIABLE {
                // Server file shrank: drop the stale prefix, restart clean.
                if let Err(e) = tokio::fs::write(part, &[]).await {
                    return Attempt::Abort(CoreError::Other(anyhow::anyhow!(
                        "truncating stale part {}: {e}",
                        part.display()
                    )));
                }
                return Attempt::retryable(CoreError::Net(
                    "range unsatisfiable, restarting download".into(),
                ));
            }
            if !status.is_success() {
                return Attempt::Abort(CoreError::Net(format!("file download http {status}")));
            }
            return self.stream_body_to_part(resp, part, resume_from).await;
        }
        Attempt::Abort(CoreError::Net(format!(
            "file download: too many redirects (max {MAX_REDIRECT_HOPS})"
        )))
    }

    /// Stream a 200/206 response body into `part` (truncate or append),
    /// verify lengths, hash and return [`FileMeta`].
    async fn stream_body_to_part(
        &self,
        mut resp: reqwest::Response,
        part: &Path,
        resume_from: u64,
    ) -> Attempt {
        let partial = resp.status() == reqwest::StatusCode::PARTIAL_CONTENT;
        let mimetype = resp
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .map(|s| {
                s.split(';')
                    .next()
                    .unwrap_or("application/octet-stream")
                    .trim()
                    .to_string()
            })
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| "application/octet-stream".to_string());
        let last_modified = resp
            .headers()
            .get(reqwest::header::LAST_MODIFIED)
            .and_then(|v| v.to_str().ok())
            .map(str::to_string);
        let content_length = resp.content_length();
        let range_total = if partial {
            resp.headers()
                .get(reqwest::header::CONTENT_RANGE)
                .and_then(|v| v.to_str().ok())
                .and_then(parse_content_range_total)
        } else {
            None
        };
        // Server ignored Range: the prefix is stale, restart the file.
        let from_scratch = !partial && resume_from > 0;
        let mut opts = tokio::fs::OpenOptions::new();
        opts.create(true).write(true);
        if from_scratch || resume_from == 0 {
            opts.truncate(true);
        } else {
            opts.append(true);
        }
        let mut file = match opts.open(part).await {
            Ok(f) => f,
            Err(e) => {
                return Attempt::Abort(CoreError::Other(anyhow::anyhow!(
                    "opening part {}: {e}",
                    part.display()
                )));
            }
        };
        let mut received: u64 = 0;
        loop {
            match resp.chunk().await {
                Ok(Some(chunk)) => {
                    use tokio::io::AsyncWriteExt as _;
                    received += chunk.len() as u64;
                    if let Err(e) = file.write_all(&chunk).await {
                        return Attempt::Abort(CoreError::Other(anyhow::anyhow!(
                            "writing part {}: {e}",
                            part.display()
                        )));
                    }
                }
                Ok(None) => break,
                Err(e) if e.is_timeout() => {
                    use tokio::io::AsyncWriteExt as _;
                    let _ = file.flush().await;
                    return Attempt::retryable(CoreError::Net(
                        "file download timeout mid-body, will resume".into(),
                    ));
                }
                Err(e) => {
                    return Attempt::Abort(CoreError::Net(format!(
                        "file download body: {}",
                        redact_token(&e.to_string())
                    )));
                }
            }
        }
        {
            use tokio::io::AsyncWriteExt as _;
            if let Err(e) = file.flush().await {
                return Attempt::Abort(CoreError::Other(anyhow::anyhow!(
                    "flushing part {}: {e}",
                    part.display()
                )));
            }
        }
        drop(file);
        // Truncation check (content-length / content-range mismatch).
        if partial {
            if let Some(total) = range_total {
                let final_len = tokio::fs::metadata(part)
                    .await
                    .map(|m| m.len())
                    .unwrap_or(0);
                if final_len != total {
                    return Attempt::retryable(CoreError::Net(format!(
                        "truncated resume: got {final_len} of {total} bytes"
                    )));
                }
            } else if let Some(rem) = content_length {
                if received != rem {
                    return Attempt::retryable(CoreError::Net(format!(
                        "truncated resume chunk: got {received} of {rem} bytes"
                    )));
                }
            }
        } else if let Some(expected) = content_length {
            if received != expected {
                return Attempt::retryable(CoreError::Net(format!(
                    "truncated body: got {received} of {expected} bytes"
                )));
            }
        }
        // Checksum-after-download, always (plan #1018).
        let bytes = match tokio::fs::read(part).await {
            Ok(b) => b,
            Err(e) => {
                return Attempt::Abort(CoreError::Other(anyhow::anyhow!(
                    "reading part {}: {e}",
                    part.display()
                )));
            }
        };
        use sha2::Digest as _;
        let sha256 = format!("{:x}", sha2::Sha256::digest(&bytes));
        Attempt::Done(FileMeta {
            sha256,
            size: bytes.len() as u64,
            timemodified: last_modified
                .as_deref()
                .and_then(parse_http_date)
                .unwrap_or(0),
            mimetype,
        })
    }

    /// Validate + canonicalize a file URL: pluginfile only (with the
    /// legacy `/pluginfile.php` → `/webservice/pluginfile.php` rewrite),
    /// http(s) only, query/fragment stripped (the token is added per
    /// request, same-host only).
    fn normalize_file_url(base: &str, fileurl: &str) -> Result<String, CoreError> {
        if !(fileurl.starts_with("http://") || fileurl.starts_with("https://")) {
            return Err(CoreError::Invalid(format!(
                "refusing non-http(s) file url: {}",
                redact_token(strip_query(fileurl))
            )));
        }
        let canonical = format!("{}/webservice/pluginfile.php", base.trim_end_matches('/'));
        if strip_query(fileurl).starts_with(&canonical) {
            return Ok(strip_query(fileurl).to_string());
        }
        if let Some(rest) = fileurl.split("/pluginfile.php").nth(1) {
            let path = rest.split(['?', '#']).next().unwrap_or("");
            return Ok(format!("{canonical}{path}"));
        }
        Err(CoreError::Invalid(format!(
            "refusing non-pluginfile url: {}",
            redact_token(strip_query(fileurl))
        )))
    }
}

/// Lowercased `host[:port]` authority of an http(s) URL, if parseable.
fn authority_of(url: &str) -> Option<String> {
    let rest = url.split("://").nth(1)?;
    let auth = rest.split('/').next()?;
    if auth.is_empty() {
        None
    } else {
        Some(auth.to_ascii_lowercase())
    }
}

/// Path + host without query/fragment (safe to embed in errors/logs).
fn strip_query(url: &str) -> &str {
    url.split(['?', '#']).next().unwrap_or(url)
}

/// Attach `?token=` for same-host URLs; cross-host targets keep no token
/// (plan #1043–1044). `token_authority` is the authority the token belongs to.
fn authed_url(url: &str, token: &str, token_authority: &str) -> String {
    if authority_of(url).as_deref() == Some(token_authority) && !token_authority.is_empty() {
        if url.contains('?') {
            format!("{url}&token={token}")
        } else {
            format!("{url}?token={token}")
        }
    } else {
        url.to_string()
    }
}

/// Resolve a redirect `Location` against the current URL: absolute http(s),
/// protocol-relative, or origin-absolute path. Anything else is refused.
fn resolve_redirect(current: &str, location: &str) -> Result<String, CoreError> {
    if location.starts_with("http://") || location.starts_with("https://") {
        return Ok(location.to_string());
    }
    if let Some(rest) = location.strip_prefix("//") {
        let scheme = current.split("://").next().unwrap_or("https");
        return Ok(format!("{scheme}://{rest}"));
    }
    if let Some(path) = location.strip_prefix('/') {
        let origin = current.split('/').take(3).collect::<Vec<_>>().join("/");
        if origin.starts_with("http://") || origin.starts_with("https://") {
            return Ok(format!("{origin}/{path}"));
        }
    }
    Err(CoreError::Net("unsupported redirect target".into()))
}

/// Next hop for a 3xx response: `Ok(Some(url))` to follow (http(s) only),
/// `Ok(None)` when no `Location` header is present.
fn redirect_target(
    resp: &reqwest::Response,
    current: &str,
) -> Result<Option<String>, anyhow::Error> {
    let loc = resp
        .headers()
        .get(reqwest::header::LOCATION)
        .and_then(|v| v.to_str().ok())
        .map(str::to_string);
    match loc {
        None => Ok(None),
        Some(l) => {
            let next = resolve_redirect(current, &l).map_err(anyhow::Error::new)?;
            if !(next.starts_with("http://") || next.starts_with("https://")) {
                anyhow::bail!("refusing non-http(s) redirect target");
            }
            Ok(Some(next))
        }
    }
}

/// Parse `Content-Range: bytes <first>-<last>/<total>` total length.
fn parse_content_range_total(header: &str) -> Option<u64> {
    header.split('/').nth(1)?.trim().parse().ok()
}

/// Parse an IMF-fixdate `Last-Modified` (`Wed, 21 Oct 2015 07:28:00 GMT`)
/// into unix seconds. Anything else yields `None` (caller uses 0).
fn parse_http_date(value: &str) -> Option<i64> {
    let value = value.strip_suffix(" GMT")?;
    let (_, rest) = value.split_once(", ")?;
    let mut parts = rest.split_whitespace();
    let day: i64 = parts.next()?.parse().ok()?;
    let month = match parts.next()? {
        "Jan" => 1,
        "Feb" => 2,
        "Mar" => 3,
        "Apr" => 4,
        "May" => 5,
        "Jun" => 6,
        "Jul" => 7,
        "Aug" => 8,
        "Sep" => 9,
        "Oct" => 10,
        "Nov" => 11,
        "Dec" => 12,
        _ => return None,
    };
    let year: i64 = parts.next()?.parse().ok()?;
    let time = parts.next()?;
    if parts.next().is_some() {
        return None;
    }
    let mut clock = time.split(':');
    let hour: i64 = clock.next()?.parse().ok()?;
    let min: i64 = clock.next()?.parse().ok()?;
    let sec: i64 = clock.next()?.parse().ok()?;
    if clock.next().is_some() || hour > 23 || min > 59 || sec > 60 {
        return None;
    }
    let leap = (year % 4 == 0 && year % 100 != 0) || year % 400 == 0;
    let max_day = match month {
        2 if leap => 29,
        2 => 28,
        4 | 6 | 9 | 11 => 30,
        _ => 31,
    };
    if day < 1 || day > max_day {
        return None;
    }
    Some(days_from_civil(year, month, day) * 86_400 + hour * 3600 + min * 60 + sec)
}

/// Days since unix epoch for a Gregorian civil date (Hinnant's algorithm).
fn days_from_civil(year: i64, month: i64, day: i64) -> i64 {
    let y = if month <= 2 { year - 1 } else { year };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let mp = (month + 9) % 12;
    let doy = (153 * mp + 2) / 5 + day - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn net_authority_parsing() {
        assert_eq!(
            authority_of("http://127.0.0.1:8080/a/b?x=1").as_deref(),
            Some("127.0.0.1:8080")
        );
        assert_eq!(
            authority_of("https://Example.INVALID/x").as_deref(),
            Some("example.invalid")
        );
        assert_eq!(
            authority_of("ftp://x.invalid/f"),
            Some("x.invalid".to_string())
        );
        assert_eq!(authority_of("not a url"), None);
        assert_eq!(authority_of("http:///path"), None);
    }

    #[test]
    fn net_authed_url_strips_token_cross_host() {
        let same = authed_url("http://h.invalid/f/x.pdf", "TOK", "h.invalid");
        assert_eq!(same, "http://h.invalid/f/x.pdf?token=TOK");
        let same_q = authed_url("http://h.invalid/f?sig=1", "TOK", "h.invalid");
        assert_eq!(same_q, "http://h.invalid/f?sig=1&token=TOK");
        let cross = authed_url("https://cdn.invalid/f/x.pdf", "TOK", "h.invalid");
        assert_eq!(cross, "https://cdn.invalid/f/x.pdf");
        assert!(!cross.contains("TOK"));
    }

    #[test]
    fn net_resolve_redirect_forms() {
        let cur = "http://h.invalid/webservice/pluginfile.php/1/a.pdf";
        assert_eq!(
            resolve_redirect(cur, "https://cdn.invalid/x").expect("abs"),
            "https://cdn.invalid/x"
        );
        assert_eq!(
            resolve_redirect(cur, "/other/b.pdf").expect("origin-abs"),
            "http://h.invalid/other/b.pdf"
        );
        assert_eq!(
            resolve_redirect(cur, "//cdn.invalid/x").expect("proto-rel"),
            "http://cdn.invalid/x"
        );
        assert!(resolve_redirect(cur, "relative/b.pdf").is_err());
        assert!(resolve_redirect("notaurl", "/x").is_err());
    }

    #[test]
    fn net_normalize_file_url_allowlist() {
        let base = "http://h.invalid";
        let ok = Moodle::normalize_file_url(
            base,
            "http://h.invalid/webservice/pluginfile.php/1/a.pdf?token=x",
        )
        .expect("canonical");
        assert_eq!(ok, "http://h.invalid/webservice/pluginfile.php/1/a.pdf");
        let rewritten = Moodle::normalize_file_url(base, "http://h.invalid/pluginfile.php/1/a.pdf")
            .expect("rewrite");
        assert_eq!(
            rewritten,
            "http://h.invalid/webservice/pluginfile.php/1/a.pdf"
        );
        for bad in [
            "ftp://h.invalid/webservice/pluginfile.php/1/a.pdf",
            "file:///etc/passwd",
            "http://h.invalid/theme/image.php/x.png",
        ] {
            assert!(
                Moodle::normalize_file_url(base, bad).is_err(),
                "must refuse {bad}"
            );
        }
        // Foreign-host pluginfile URLs are never fetched: path is re-homed
        // onto our own base (legacy rewrite), token stays same-host only.
        let rehomed = Moodle::normalize_file_url(
            base,
            "https://evil.invalid/webservice/pluginfile.php/1/a.pdf",
        )
        .expect("re-home");
        assert_eq!(
            rehomed,
            "http://h.invalid/webservice/pluginfile.php/1/a.pdf"
        );
    }

    #[test]
    fn net_parse_http_date_known_value() {
        // Wed, 21 Oct 2015 07:28:00 GMT == 1445412480.
        assert_eq!(
            parse_http_date("Wed, 21 Oct 2015 07:28:00 GMT"),
            Some(1_445_412_480)
        );
        assert_eq!(parse_http_date("Thu, 01 Jan 1970 00:00:00 GMT"), Some(0));
        assert!(parse_http_date("Mon, 29 Feb 2016 12:00:00 GMT").is_some());
        for bad in [
            "",
            "yesterday",
            "21 Oct 2015 07:28:00",
            "Wed, 32 Oct 2015 07:28:00 GMT",
            "Wed, 30 Feb 2015 07:28:00 GMT",
            "Wed, 21 Foo 2015 07:28:00 GMT",
            "Wed, 21 Oct 2015 25:28:00 GMT",
            "Wed, 21 Oct 2015 07:28:00 GMT extra",
        ] {
            assert_eq!(parse_http_date(bad), None, "must reject {bad:?}");
        }
    }

    #[test]
    fn net_parse_content_range_total() {
        assert_eq!(parse_content_range_total("bytes 0-99/200"), Some(200));
        assert_eq!(parse_content_range_total("bytes 100-199/200"), Some(200));
        assert_eq!(parse_content_range_total("bytes */200"), Some(200));
        assert_eq!(parse_content_range_total("garbage"), None);
    }
}
