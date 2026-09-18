//! In-process mock Moodle server fixture (Task 1, P0 baseline).
//!
//! Speaks just enough HTTP/1.1 (one request per `Connection: close`
//! connection) to exercise the real [`moodle_mcp::moodle::Moodle`] client:
//! the three webservice calls the core uses plus `pluginfile.php`
//! downloads. Only tokio (already a dependency) and serde_json are needed;
//! no third-party HTTP server.
//!
//! Fixture: 1 course (plus 1 decoy), 5 sections x 5 modules x 2 files =
//! 50 files, module descriptions cycling through HTML variants (link,
//! table, code block, image, list/emphasis). Failure modes: unknown
//! fileurl -> 404, bad token -> 401, [`MockConfig::ratelimit`] -> 429 +
//! `Retry-After`, [`MockConfig::flaky_500`] -> 500, then success.
//!
//! Included from `tests/mock_smoke.rs` via `#[path]`; later tasks reuse
//! this for retry/governor/resume harnesses.

use std::collections::HashMap;
use std::sync::{
    atomic::{AtomicUsize, Ordering},
    Arc,
};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt};

pub const COURSE_ID: i64 = 101;
pub const USER_ID: i64 = 42;
pub const TOKEN: &str = "mock-token-0123456789abcdef";
/// Fixed `Last-Modified` served on pluginfile hits (Task 2 timemodified).
/// `Wed, 21 Oct 2015 07:28:00 GMT` == 1445412480 unix.
pub const LAST_MODIFIED_HTTP: &str = "Wed, 21 Oct 2015 07:28:00 GMT";
#[allow(dead_code)]
pub const LAST_MODIFIED_UNIX: i64 = 1445412480;

#[derive(Debug, Clone, Copy, Default)]
pub struct MockConfig {
    /// First N webservice calls fail with HTTP 500, then succeed.
    pub flaky_500: usize,
    /// First N webservice calls fail with HTTP 429 + `Retry-After: 2`.
    pub ratelimit: usize,
    /// Every webservice call fails with HTTP 401.
    pub unauthorized: bool,
}

struct Shared {
    site_info: serde_json::Value,
    courses: serde_json::Value,
    contents: serde_json::Value,
    files: HashMap<String, Vec<u8>>,
    flaky_left: AtomicUsize,
    ratelimit_left: AtomicUsize,
    unauthorized: bool,
}

pub struct MockMoodle {
    base: String,
    // Introspection fields: each test binary uses a subset; shared fixture.
    #[allow(dead_code)]
    pub site_info: serde_json::Value,
    #[allow(dead_code)]
    pub courses: serde_json::Value,
    #[allow(dead_code)]
    pub contents: serde_json::Value,
    /// Configured fail-first counts, for test introspection.
    #[allow(dead_code)]
    pub flaky_500: usize,
    #[allow(dead_code)]
    pub ratelimit: usize,
    files: HashMap<String, Vec<u8>>,
    /// Accepted TCP connections since start (one HTTP request per
    /// `Connection: close` connection). Task 3 incremental-sync tests use
    /// this to assert skipped files cause zero file-download hits.
    #[allow(dead_code)]
    hits: Arc<AtomicUsize>,
}

impl MockMoodle {
    pub async fn start(config: MockConfig) -> Self {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("mock bind");
        let base = format!("http://{}", listener.local_addr().expect("mock addr"));
        let (site_info, courses, contents, files) = fixtures(&base);
        let shared = Arc::new(Shared {
            site_info: site_info.clone(),
            courses: courses.clone(),
            contents: contents.clone(),
            files: files.clone(),
            flaky_left: AtomicUsize::new(config.flaky_500),
            ratelimit_left: AtomicUsize::new(config.ratelimit),
            unauthorized: config.unauthorized,
        });
        let hits = Arc::new(AtomicUsize::new(0));
        let hits_accept = Arc::clone(&hits);
        tokio::spawn(async move {
            loop {
                let Ok((stream, _)) = listener.accept().await else {
                    break;
                };
                hits_accept.fetch_add(1, Ordering::SeqCst);
                let shared = Arc::clone(&shared);
                tokio::spawn(async move {
                    handle(stream, &shared).await;
                });
            }
        });
        Self {
            base,
            site_info,
            courses,
            contents,
            flaky_500: config.flaky_500,
            ratelimit: config.ratelimit,
            files,
            hits,
        }
    }

    pub async fn healthy() -> Self {
        Self::start(MockConfig::default()).await
    }

    #[allow(dead_code)]
    pub async fn flaky(n: usize) -> Self {
        Self::start(MockConfig {
            flaky_500: n,
            ..Default::default()
        })
        .await
    }

    #[allow(dead_code)]
    pub async fn ratelimited(n: usize) -> Self {
        Self::start(MockConfig {
            ratelimit: n,
            ..Default::default()
        })
        .await
    }

    #[allow(dead_code)]
    pub async fn unauthorized() -> Self {
        Self::start(MockConfig {
            unauthorized: true,
            ..Default::default()
        })
        .await
    }

    #[must_use]
    pub fn base(&self) -> &str {
        &self.base
    }

    /// Accepted connections (HTTP hits) since start or last reset.
    #[allow(dead_code)]
    #[must_use]
    pub fn hits(&self) -> usize {
        self.hits.load(Ordering::SeqCst)
    }

    /// Zero the hit counter (e.g. between a seeding sync and the sync
    /// under test).
    #[allow(dead_code)]
    pub fn reset_hits(&self) {
        self.hits.store(0, Ordering::SeqCst);
    }

    #[allow(dead_code)]
    #[must_use]
    pub fn pluginfile(&self, path: &str) -> Option<&[u8]> {
        self.files.get(path).map(Vec::as_slice)
    }
}

fn mime_for(ext: &str) -> &'static str {
    match ext {
        "pdf" => "application/pdf",
        "pptx" => "application/vnd.openxmlformats-officedocument.presentationml.presentation",
        "xlsx" => "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet",
        "csv" => "text/csv",
        "mp4" => "video/mp4",
        "zip" => "application/zip",
        "txt" => "text/plain",
        "srt" => "application/x-subrip",
        "docx" => "application/vnd.openxmlformats-officedocument.wordprocessingml.document",
        "png" => "image/png",
        _ => "application/octet-stream",
    }
}

#[allow(clippy::type_complexity)]
fn fixtures(
    base: &str,
) -> (
    serde_json::Value,
    serde_json::Value,
    serde_json::Value,
    HashMap<String, Vec<u8>>,
) {
    let site_info = serde_json::json!({
        "userid": USER_ID,
        "username": "mock-user",
        "sitename": "Mock Moodle",
        "version": "4.1.0",
    });
    let courses = serde_json::json!([
        {"id": COURSE_ID, "fullname": "Mock Course 101", "shortname": "MOCK101",
         "summary": "Mock course for integration tests."},
        {"id": 102, "fullname": "Other Course", "shortname": "OTHER102", "summary": ""},
    ]);
    let variants = [
        r#"<p>Read <a href="https://example.invalid/notes">the notes</a> before class.</p>"#,
        r#"<table><tr><th>Proto</th><th>Port</th></tr><tr><td>HTTP</td><td>80</td></tr></table>"#,
        "<pre><code class=\"language-bash\">ip addr show\nping -c1 192.0.2.1</code></pre>",
        r#"<p>Diagram: <img src="https://example.invalid/diag.png" alt="topology" /></p>"#,
        r#"<p><b>Important</b> and <i>emphasised</i>:</p><ul><li>one</li><li>two</li></ul>"#,
    ];
    let exts = [
        "pdf", "pptx", "xlsx", "csv", "mp4", "zip", "txt", "srt", "docx", "png",
    ];
    let mut files: HashMap<String, Vec<u8>> = HashMap::new();
    let mut sections = vec![serde_json::json!({
        "id": 1000, "name": "General",
        "summary": "<p>Welcome to <b>Mock Course 101</b>.</p>",
        "position": 0, "modules": [],
    })];
    let mut n = 0usize;
    for s in 1..=5 {
        let mut modules = vec![];
        for m in 0..5 {
            let desc = variants[(s + m) % variants.len()];
            let mut contents = vec![];
            for k in 0..2 {
                let ext = exts[n % exts.len()];
                let filename = format!("s{s}-m{m}-{k}.{ext}");
                let path = format!("/{COURSE_ID}/mod_resource/content/{n}/{filename}");
                let fileurl = format!("{base}/webservice/pluginfile.php{path}");
                // Like production, the served bytes are exactly `filesize`
                // long so size-based skip logic can trust the declaration.
                let size = 64 + n;
                let pattern = format!("mock-bytes:{path}\n");
                let mut bytes = Vec::with_capacity(size);
                while bytes.len() < size {
                    let take = (size - bytes.len()).min(pattern.len());
                    bytes.extend_from_slice(&pattern.as_bytes()[..take]);
                }
                files.insert(path.clone(), bytes);
                contents.push(serde_json::json!({
                    "filename": filename,
                    "filesize": size as i64,
                    "mimetype": mime_for(ext),
                    "fileurl": fileurl,
                    "timemodified": LAST_MODIFIED_UNIX,
                    "contenthash": format!("{n:040x}"),
                }));
                n += 1;
            }
            modules.push(serde_json::json!({
                "id": 2000 + n as i64,
                "name": format!("Module {s}.{m}"),
                "modname": "resource",
                "description": desc,
                "contents": contents,
            }));
        }
        sections.push(serde_json::json!({
            "id": 1000 + s as i64,
            "name": format!("RA0{s} - Topic {s}"),
            "summary": format!("<p>Summary for RA0{s}.</p>"),
            "position": s as i64,
            "modules": modules,
        }));
    }
    (
        site_info,
        courses,
        serde_json::Value::Array(sections),
        files,
    )
}

fn hex_val(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        b'A'..=b'F' => Some(b - b'A' + 10),
        _ => None,
    }
}

fn decode(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        match b[i] {
            b'+' => {
                out.push(b' ');
                i += 1;
            }
            b'%' if i + 2 < b.len() => match (hex_val(b[i + 1]), hex_val(b[i + 2])) {
                (Some(h), Some(l)) => {
                    out.push(h << 4 | l);
                    i += 3;
                }
                _ => {
                    out.push(b'%');
                    i += 1;
                }
            },
            c => {
                out.push(c);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn form_decode(body: &str) -> HashMap<String, String> {
    body.split('&')
        .filter_map(|pair| {
            let (k, v) = pair.split_once('=')?;
            Some((decode(k), decode(v)))
        })
        .collect()
}

fn take(counter: &AtomicUsize) -> bool {
    let mut cur = counter.load(Ordering::SeqCst);
    while cur > 0 {
        match counter.compare_exchange(cur, cur - 1, Ordering::SeqCst, Ordering::SeqCst) {
            Ok(_) => return true,
            Err(v) => cur = v,
        }
    }
    false
}

fn exc_body(code: &str, message: &str) -> Vec<u8> {
    serde_json::to_vec(&serde_json::json!({
        "exception": "moodle_exception",
        "errorcode": code,
        "message": message,
    }))
    .unwrap_or_default()
}

/// Returns (status, content-type, extra-headers, body).
fn route(
    method: &str,
    target: &str,
    body: &[u8],
    range: Option<&str>,
    shared: &Shared,
) -> (u16, String, String, Vec<u8>) {
    const JSON: &str = "application/json";
    let path = target.split('?').next().unwrap_or("/");
    let query = target.split('?').nth(1).unwrap_or("");
    if method == "POST" && path == "/webservice/rest/server.php" {
        let form = form_decode(std::str::from_utf8(body).unwrap_or(""));
        if shared.unauthorized || !form.get("wstoken").is_some_and(|t| t == TOKEN) {
            return (
                401,
                JSON.to_string(),
                String::new(),
                exc_body("invalidtoken", "Invalid token"),
            );
        }
        if take(&shared.ratelimit_left) {
            return (
                429,
                JSON.to_string(),
                "Retry-After: 2\r\n".to_string(),
                exc_body("ratelimited", "Slow down"),
            );
        }
        if take(&shared.flaky_left) {
            return (
                500,
                JSON.to_string(),
                String::new(),
                exc_body("servererror", "Flaky failure"),
            );
        }
        let payload = match form.get("wsfunction").map(String::as_str) {
            Some("core_webservice_get_site_info") => {
                serde_json::to_vec(&shared.site_info).unwrap_or_default()
            }
            Some("core_enrol_get_users_courses") => {
                serde_json::to_vec(&shared.courses).unwrap_or_default()
            }
            Some("core_course_get_contents") => {
                serde_json::to_vec(&shared.contents).unwrap_or_default()
            }
            Some(other) => exc_body("invalidfunction", &format!("unknown wsfunction {other}")),
            None => exc_body("invalidrequest", "missing wsfunction"),
        };
        (200, JSON.to_string(), String::new(), payload)
    } else if method == "GET"
        && (path.starts_with("/webservice/pluginfile.php") || path.starts_with("/pluginfile.php"))
    {
        // pluginfile downloads require the token query param, like production.
        let authed = !shared.unauthorized
            && query
                .split('&')
                .filter_map(|p| p.strip_prefix("token="))
                .any(|t| t == TOKEN);
        if !authed {
            return (
                401,
                JSON.to_string(),
                String::new(),
                exc_body("invalidtoken", "Invalid token"),
            );
        }
        let rest = path
            .strip_prefix("/webservice/pluginfile.php")
            .or_else(|| path.strip_prefix("/pluginfile.php"))
            .unwrap_or(path);
        match shared.files.get(rest) {
            Some(bytes) => {
                let ext = rest.rsplit('.').next().unwrap_or("");
                let ctype = mime_for(ext).to_string();
                let base_extra =
                    format!("Accept-Ranges: bytes\r\nLast-Modified: {LAST_MODIFIED_HTTP}\r\n");
                // Task 2 resume: honor `Range: bytes=N-` with 206, else full 200.
                if let Some(start) = parse_range(range) {
                    if start >= bytes.len() as u64 {
                        return (416, ctype, String::new(), Vec::new());
                    }
                    let start = start as usize;
                    let end = bytes.len() - 1;
                    let extra = format!(
                        "{base_extra}Content-Range: bytes {start}-{end}/{}\r\n",
                        bytes.len()
                    );
                    (206, ctype, extra, bytes[start..].to_vec())
                } else {
                    (200, ctype, base_extra, bytes.clone())
                }
            }
            None => (
                404,
                JSON.to_string(),
                String::new(),
                exc_body("filenotfound", "File not found"),
            ),
        }
    } else {
        (
            404,
            JSON.to_string(),
            String::new(),
            exc_body("notfound", "Unknown mock route"),
        )
    }
}

/// Parse `bytes=N-` suffix-open ranges; `None` means "no (usable) Range".
fn parse_range(header: Option<&str>) -> Option<u64> {
    let v = header?.trim();
    let rest = v.strip_prefix("bytes=")?;
    let (start, _) = rest.split_once('-').unwrap_or((rest, ""));
    if start.is_empty() {
        return None; // suffix ranges (`bytes=-N`) are not honored here
    }
    start.parse().ok()
}

async fn handle(stream: tokio::net::TcpStream, shared: &Shared) {
    let mut reader = tokio::io::BufReader::new(stream);
    let mut request_line = String::new();
    if reader.read_line(&mut request_line).await.is_err() || request_line.trim().is_empty() {
        return;
    }
    let mut parts = request_line.split_whitespace();
    let method = parts.next().unwrap_or("");
    let target = parts.next().unwrap_or("").to_string();
    let mut content_length = 0usize;
    let mut range: Option<String> = None;
    loop {
        let mut line = String::new();
        match reader.read_line(&mut line).await {
            Ok(0) | Err(_) => return,
            Ok(_) => {
                let line = line.trim();
                if line.is_empty() {
                    break;
                }
                let lower = line.to_ascii_lowercase();
                if let Some(v) = lower.strip_prefix("content-length:") {
                    content_length = v.trim().parse().unwrap_or(0);
                } else if let Some(v) = lower.strip_prefix("range:") {
                    range = Some(v.trim().to_string());
                }
            }
        }
    }
    let mut body = vec![0u8; content_length];
    if content_length > 0 && reader.read_exact(&mut body).await.is_err() {
        return;
    }
    let (status, content_type, extra, payload) =
        route(method, &target, &body, range.as_deref(), shared);
    let mut stream = reader.into_inner();
    let head = format!(
        "HTTP/1.1 {status}\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\n{extra}\r\n",
        payload.len()
    );
    let _ = stream.write_all(head.as_bytes()).await;
    let _ = stream.write_all(&payload).await;
}
