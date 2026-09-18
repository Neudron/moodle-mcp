use anyhow::{bail, Context, Result};
use serde::Deserialize;

#[derive(Debug, Clone)]
pub struct Moodle {
    base: String,
    token: String,
    http: reqwest::Client,
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
        let http = reqwest::Client::builder()
            .user_agent("moodle-mcp/0.1")
            .timeout(std::time::Duration::from_secs(180))
            .build()
            .expect("reqwest client");
        Self {
            base: base.trim_end_matches('/').to_string(),
            token: token.to_string(),
            http,
        }
    }

    pub fn from_env() -> Result<Self> {
        let base = std::env::var("MOODLE_URL")
            .unwrap_or_else(|_| "https://educaciodigital.cat/iespalamos/moodle".into());
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
        form.extend_from_slice(params);
        let resp = self
            .http
            .post(&url)
            .form(&form)
            .send()
            .await
            .context("moodle request")?;
        let status = resp.status();
        let body: serde_json::Value = resp.json().await.context("moodle json")?;
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
        if !fileurl.starts_with(&format!("{}/webservice/pluginfile.php", self.base)) {
            // some fileurls point at /pluginfile.php; rewrite to webservice variant
            if let Some(rest) = fileurl.split("/pluginfile.php").nth(1) {
                let url = format!("{}/webservice/pluginfile.php{rest}", self.base);
                return self.do_download(&url).await;
            }
            bail!("refusing to download non-pluginfile url: {fileurl}");
        }
        self.do_download(fileurl).await
    }

    async fn do_download(&self, url: &str) -> Result<Vec<u8>> {
        let sep = if url.contains('?') { '&' } else { '?' };
        let with_token = format!("{url}{sep}token={}", self.token);
        let resp = self
            .http
            .get(&with_token)
            .send()
            .await
            .context("file download")?;
        let status = resp.status();
        let bytes = resp.bytes().await.context("file body")?;
        if !status.is_success() {
            bail!("download http {status}");
        }
        Ok(bytes.to_vec())
    }
}
