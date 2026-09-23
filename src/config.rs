//! Config file + environment precedence (Task 4, P3 state/config/store).
//!
//! Effective config resolution (documented precedence, #978):
//! `MOODLE_*` environment > `.moodle/config.toml` > built-in defaults.
//!
//! The file is flat TOML without nesting: `key = value` lines with `#`
//! comments. `[section]` headers are accepted but ignored (keys are global),
//! so both flat files and hand-sectioned files read the same. Values are
//! integers, floats, booleans or double/single-quoted strings. Unknown keys
//! are ignored (forward compat); invalid values for known keys fall back to
//! the previous layer with a `tracing::warn`.
//!
//! No TOML crate is used on purpose: `Cargo.toml` is frozen for this task
//! and the schema is a fixed flat set, so a small strict reader keeps the
//! dependency tree unchanged. [`Config::init_template`] is the format
//! contract — the reader is tested to round-trip it exactly.
//!
//! Security: `Config` never holds the token (#983). The token lives only in
//! `MOODLE_TOKEN` / the token file consumed by the client; every dump path
//! ([`Config::show_redacted`]) is therefore redacted by construction, and a
//! regression test asserts a fake token never appears in it.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

/// `$ROOT/.moodle/config.toml` (#971).
#[must_use]
pub fn config_path(root: &str) -> PathBuf {
    Path::new(root).join(".moodle/config.toml")
}

#[derive(Debug, Clone, PartialEq)]
pub struct Timeouts {
    /// API call timeout, seconds (#973).
    pub api_secs: u64,
    /// Single-file download timeout, seconds (#973).
    pub file_secs: u64,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Retries {
    /// Max retries for 429/5xx/timeouts (#974, #1002).
    pub max: u32,
    /// Backoff base, milliseconds: `500ms x 2^n` capped at 15s (#974, #1004).
    pub backoff_base_ms: u64,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Flags {
    /// Full-text index maintenance (#976).
    pub index: bool,
    /// PDF text extraction (#976).
    pub pdf_text: bool,
    /// OCR for scanned PDFs (#976).
    pub ocr: bool,
    /// Fetch link titles for external pointers (#976).
    pub enrich_links: bool,
}

/// Effective configuration: `Config::load(root) -> Config{concurrency,
/// timeouts, retries, flags, locale, theme}` (+ rate/store/mouse).
#[derive(Debug, Clone, PartialEq)]
pub struct Config {
    /// Download concurrency, clamped 1–32 (#972).
    pub concurrency: u32,
    pub timeouts: Timeouts,
    pub retries: Retries,
    /// Global request rate, requests/second (#975).
    pub rate_max_rps: f64,
    pub flags: Flags,
    /// UI locale: `ca` | `es` | `en` (#977).
    pub locale: String,
    /// UI theme: `light` | `dark` | `high-contrast` (#977).
    pub theme: String,
    /// Mouse capture always on for the mouse-only TUI (#977).
    pub mouse: bool,
    /// Blob-store layout: `flat` (default, #999) | `content-addressed`.
    pub store: String,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            concurrency: 8,
            timeouts: Timeouts {
                api_secs: 15,
                file_secs: 60,
            },
            retries: Retries {
                max: 4,
                backoff_base_ms: 500,
            },
            rate_max_rps: 8.0,
            flags: Flags {
                index: true,
                pdf_text: false,
                ocr: false,
                enrich_links: false,
            },
            locale: "ca".to_string(),
            theme: "dark".to_string(),
            mouse: true,
            store: "flat".to_string(),
        }
    }
}

fn clamp_concurrency(value: i64) -> u32 {
    value.clamp(1, 32) as u32
}

fn clamp_timeout(value: i64, fallback: u64) -> u64 {
    if value >= 1 {
        value as u64
    } else {
        fallback
    }
}

fn clamp_retries(value: i64) -> u32 {
    value.clamp(0, 10) as u32
}

fn clamp_rps(value: f64, fallback: f64) -> f64 {
    if value.is_finite() && value > 0.0 {
        value
    } else {
        fallback
    }
}

/// Parsed scalar from the TOML subset.
#[derive(Debug, Clone, PartialEq)]
enum Scalar {
    Int(i64),
    Float(f64),
    Bool(bool),
    Str(String),
}

/// Strip an inline `#` comment, honouring single/double quotes.
fn strip_inline_comment(line: &str) -> &str {
    let mut quote: Option<char> = None;
    let mut prev_backslash = false;
    for (idx, ch) in line.char_indices() {
        if prev_backslash {
            prev_backslash = false;
            continue;
        }
        match quote {
            Some(q) => {
                if ch == '\\' && q == '"' {
                    prev_backslash = true;
                } else if ch == q {
                    quote = None;
                }
            }
            None => {
                if ch == '"' || ch == '\'' {
                    quote = Some(ch);
                } else if ch == '#' {
                    return line[..idx].trim_end();
                }
            }
        }
    }
    line
}

fn unescape_double_quoted(inner: &str) -> String {
    let mut out = String::with_capacity(inner.len());
    let mut chars = inner.chars();
    while let Some(ch) = chars.next() {
        if ch == '\\' {
            match chars.next() {
                Some('n') => out.push('\n'),
                Some('t') => out.push('\t'),
                Some('r') => out.push('\r'),
                Some('\\') => out.push('\\'),
                Some('"') => out.push('"'),
                Some(other) => {
                    out.push('\\');
                    out.push(other);
                }
                None => out.push('\\'),
            }
        } else {
            out.push(ch);
        }
    }
    out
}

fn parse_scalar(raw: &str) -> Option<Scalar> {
    let raw = raw.trim();
    if raw.is_empty() {
        return None;
    }
    if raw.len() >= 2 && raw.starts_with('"') && raw.ends_with('"') && raw.len() > 1 {
        let inner = raw.get(1..raw.len() - 1).unwrap_or("");
        return Some(Scalar::Str(unescape_double_quoted(inner)));
    }
    if raw.len() >= 2 && raw.starts_with('\'') && raw.ends_with('\'') {
        return Some(Scalar::Str(
            raw.get(1..raw.len() - 1).unwrap_or("").to_string(),
        ));
    }
    match raw {
        "true" => return Some(Scalar::Bool(true)),
        "false" => return Some(Scalar::Bool(false)),
        _ => {}
    }
    if let Ok(int) = raw.parse::<i64>() {
        // Reject float-looking input that also parses as int prefix.
        if !raw.contains(['.', 'e', 'E']) {
            return Some(Scalar::Int(int));
        }
    }
    if let Ok(float) = raw.parse::<f64>() {
        if float.is_finite() {
            return Some(Scalar::Float(float));
        }
        return None;
    }
    None
}

/// Parse the flat TOML subset into raw key → scalar. Sections ignored,
/// unknown keys kept (caller filters), invalid lines skipped with warn.
fn parse_toml_subset(text: &str) -> BTreeMap<String, Scalar> {
    let mut map = BTreeMap::new();
    for (lineno, line) in text.lines().enumerate() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        if line.starts_with('[') {
            // Section header: accepted, ignored (keys are global).
            continue;
        }
        let Some(eq) = line.find('=') else {
            tracing::warn!(lineno = lineno + 1, "config: skipping line without '='");
            continue;
        };
        let key = line[..eq].trim().to_string();
        if key.is_empty() || !key.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_') {
            tracing::warn!(lineno = lineno + 1, "config: skipping invalid key");
            continue;
        }
        let value = strip_inline_comment(line[eq + 1..].trim());
        match parse_scalar(value) {
            Some(scalar) => {
                map.insert(key, scalar);
            }
            None => tracing::warn!(key = %key, "config: skipping invalid value"),
        }
    }
    map
}

fn scalar_int(value: &Scalar) -> Option<i64> {
    match value {
        Scalar::Int(v) => Some(*v),
        Scalar::Float(v) if v.fract() == 0.0 => Some(*v as i64),
        _ => None,
    }
}

fn scalar_float(value: &Scalar) -> Option<f64> {
    match value {
        Scalar::Float(v) => Some(*v),
        Scalar::Int(v) => Some(*v as f64),
        _ => None,
    }
}

fn scalar_bool(value: &Scalar) -> Option<bool> {
    match value {
        Scalar::Bool(v) => Some(*v),
        _ => None,
    }
}

fn scalar_str(value: &Scalar) -> Option<&str> {
    match value {
        Scalar::Str(v) => Some(v.as_str()),
        _ => None,
    }
}

impl Config {
    /// Apply one flat key-map layer onto `self` (file layer, then env).
    /// Invalid values are ignored with a warning; the previous layer wins.
    fn apply_layer(&mut self, layer: &BTreeMap<String, Scalar>, origin: &str) {
        let defaults = Config::default();
        for (key, value) in layer {
            let recognised = match key.as_str() {
                "concurrency" => scalar_int(value).map(|v| {
                    self.concurrency = clamp_concurrency(v);
                }),
                "api_timeout_secs" => scalar_int(value).map(|v| {
                    self.timeouts.api_secs = clamp_timeout(v, defaults.timeouts.api_secs);
                }),
                "file_timeout_secs" => scalar_int(value).map(|v| {
                    self.timeouts.file_secs = clamp_timeout(v, defaults.timeouts.file_secs);
                }),
                "max_retries" => scalar_int(value).map(|v| {
                    self.retries.max = clamp_retries(v);
                }),
                "backoff_base_ms" => scalar_int(value).map(|v| {
                    self.retries.backoff_base_ms = if v >= 0 { v as u64 } else { 0 };
                }),
                "max_rps" => scalar_float(value).map(|v| {
                    self.rate_max_rps = clamp_rps(v, defaults.rate_max_rps);
                }),
                "index" => scalar_bool(value).map(|v| self.flags.index = v),
                "pdf_text" => scalar_bool(value).map(|v| self.flags.pdf_text = v),
                "ocr" => scalar_bool(value).map(|v| self.flags.ocr = v),
                "enrich_links" => scalar_bool(value).map(|v| self.flags.enrich_links = v),
                "locale" => scalar_str(value).map(|v| self.locale = v.to_string()),
                "theme" => scalar_str(value).map(|v| self.theme = v.to_string()),
                "mouse" => scalar_bool(value).map(|v| self.mouse = v),
                "store" => scalar_str(value).map(|v| self.store = v.to_string()),
                _ => continue, // unknown keys ignored (forward compat)
            };
            if recognised.is_none() {
                tracing::warn!(key = %key, origin = %origin, "config: invalid value, keeping previous");
            }
        }
    }

    /// Load effective config: env > file > defaults (#978). Never fails:
    /// a missing file means defaults, an unreadable/invalid file warns and
    /// falls back to defaults for the affected values.
    #[must_use]
    pub fn load(root: &str) -> Self {
        let mut config = Config::default();
        let path = config_path(root);
        match std::fs::read_to_string(&path) {
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => {
                tracing::warn!(path = %path.display(), error = %e, "config: unreadable, using defaults")
            }
            Ok(text) => config.apply_layer(&parse_toml_subset(&text), "file"),
        }
        config.apply_layer(&env_layer(), "env");
        config
    }

    /// Write the commented template when absent (#980). Returns `true` when
    /// created, `false` when a config already exists (never overwrites).
    pub fn init(root: &str) -> Result<bool, anyhow::Error> {
        let path = config_path(root);
        if path.exists() {
            return Ok(false);
        }
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|e| anyhow::anyhow!("creating {}: {e}", parent.display()))?;
        }
        std::fs::write(&path, Self::init_template())
            .map_err(|e| anyhow::anyhow!("writing {}: {e}", path.display()))?;
        Ok(true)
    }

    /// The commented template: the format contract for the file (#980).
    /// Parsing it must yield [`Config::default`].
    #[must_use]
    pub fn init_template() -> String {
        let defaults = Config::default();
        format!(
            "# moodle-mcp config — TOML subset (flat `key = value`, `#` comments).\n\
             # Precedence: MOODLE_* environment > this file > built-in defaults.\n\
             # Root selection: pass --root or set MOODLE_ROOT per run (see CLI help).\n\
             # Secrets: the token NEVER goes here — use MOODLE_TOKEN or the token file.\n\
             \n\
             # Download concurrency, 1-32 (clamped). Env: MOODLE_CONCURRENCY\n\
             concurrency = {}\n\
             \n\
             # Timeouts in seconds. Env: MOODLE_API_TIMEOUT_SECS / MOODLE_FILE_TIMEOUT_SECS\n\
             api_timeout_secs = {}\n\
             file_timeout_secs = {}\n\
             \n\
             # Retries for 429/5xx/timeouts; backoff is 500ms x 2^n capped at 15s.\n\
             # Env: MOODLE_MAX_RETRIES / MOODLE_BACKOFF_BASE_MS\n\
             max_retries = {}\n\
             backoff_base_ms = {}\n\
             \n\
             # Global request rate (requests/second). Env: MOODLE_MAX_RPS\n\
             max_rps = {}\n\
             \n\
             # Feature flags. Env: MOODLE_INDEX / MOODLE_PDF_TEXT / MOODLE_OCR / MOODLE_ENRICH_LINKS\n\
             index = {}\n\
             pdf_text = {}\n\
             ocr = {}\n\
             enrich_links = {}\n\
             \n\
             # UI locale (ca|es|en) and theme (light|dark|high-contrast). Env: MOODLE_LOCALE / MOODLE_THEME\n\
             locale = \"{}\"\n\
             theme = \"{}\"\n\
             # Mouse capture for the mouse-only TUI. Env: MOODLE_MOUSE\n\
             mouse = {}\n\
             \n\
             # Blob-store layout: \"flat\" (default, no migration surprise) or \"content-addressed\".\n\
             # Env: MOODLE_STORE\n\
             store = \"{}\"\n",
            defaults.concurrency,
            defaults.timeouts.api_secs,
            defaults.timeouts.file_secs,
            defaults.retries.max,
            defaults.retries.backoff_base_ms,
            defaults.rate_max_rps,
            defaults.flags.index,
            defaults.flags.pdf_text,
            defaults.flags.ocr,
            defaults.flags.enrich_links,
            defaults.locale,
            defaults.theme,
            defaults.mouse,
            defaults.store,
        )
    }

    /// Human-readable effective config for `config show` (#979). Redacted
    /// by construction: no secret field exists on `Config`.
    #[must_use]
    pub fn show_redacted(&self) -> String {
        format!(
            "# effective config (env > config.toml > defaults; no secrets stored)\n\
             concurrency = {}\n\
             api_timeout_secs = {}\n\
             file_timeout_secs = {}\n\
             max_retries = {}\n\
             backoff_base_ms = {}\n\
             max_rps = {}\n\
             index = {}\n\
             pdf_text = {}\n\
             ocr = {}\n\
             enrich_links = {}\n\
             locale = {:?}\n\
             theme = {:?}\n\
             mouse = {}\n\
             store = {:?}\n",
            self.concurrency,
            self.timeouts.api_secs,
            self.timeouts.file_secs,
            self.retries.max,
            self.retries.backoff_base_ms,
            self.rate_max_rps,
            self.flags.index,
            self.flags.pdf_text,
            self.flags.ocr,
            self.flags.enrich_links,
            self.locale,
            self.theme,
            self.mouse,
            self.store,
        )
    }

    /// Config schema check for `config validate`: human-readable issues
    /// (`"error: …"` / `"warn: …"`). Empty means valid.
    #[must_use]
    pub fn validate(&self) -> Vec<String> {
        let mut issues = Vec::new();
        if !(1..=32).contains(&self.concurrency) {
            issues.push(format!(
                "error: concurrency {} outside 1-32",
                self.concurrency
            ));
        }
        if self.timeouts.api_secs == 0 {
            issues.push("error: api_timeout_secs must be >= 1".to_string());
        }
        if self.timeouts.file_secs == 0 {
            issues.push("error: file_timeout_secs must be >= 1".to_string());
        }
        if !self.rate_max_rps.is_finite() || self.rate_max_rps <= 0.0 {
            issues.push(format!(
                "error: max_rps {} must be a positive number",
                self.rate_max_rps
            ));
        }
        if self.locale.trim().is_empty() {
            issues.push("warn: locale is empty (expected ca|es|en)".to_string());
        } else if !["ca", "es", "en"].contains(&self.locale.as_str()) {
            issues.push(format!(
                "warn: locale {:?} (expected ca|es|en)",
                self.locale
            ));
        }
        if !["light", "dark", "high-contrast"].contains(&self.theme.as_str()) {
            issues.push(format!(
                "warn: theme {:?} (expected light|dark|high-contrast)",
                self.theme
            ));
        }
        if self.store != "flat" && self.store != "content-addressed" {
            issues.push(format!(
                "error: store {:?} (expected \"flat\" or \"content-addressed\")",
                self.store
            ));
        }
        issues
    }
}

/// Read the `MOODLE_*` override layer (#978, #982).
fn env_layer() -> BTreeMap<String, Scalar> {
    const MAPPING: &[(&str, &str)] = &[
        ("MOODLE_CONCURRENCY", "concurrency"),
        ("MOODLE_API_TIMEOUT_SECS", "api_timeout_secs"),
        ("MOODLE_FILE_TIMEOUT_SECS", "file_timeout_secs"),
        ("MOODLE_MAX_RETRIES", "max_retries"),
        ("MOODLE_BACKOFF_BASE_MS", "backoff_base_ms"),
        ("MOODLE_MAX_RPS", "max_rps"),
        ("MOODLE_INDEX", "index"),
        ("MOODLE_PDF_TEXT", "pdf_text"),
        ("MOODLE_OCR", "ocr"),
        ("MOODLE_ENRICH_LINKS", "enrich_links"),
        ("MOODLE_LOCALE", "locale"),
        ("MOODLE_THEME", "theme"),
        ("MOODLE_MOUSE", "mouse"),
        ("MOODLE_STORE", "store"),
    ];
    let mut layer = BTreeMap::new();
    for (env, key) in MAPPING {
        if let Ok(raw) = std::env::var(env) {
            match parse_env_value(key, raw.trim()) {
                Some(scalar) => {
                    layer.insert((*key).to_string(), scalar);
                }
                None => tracing::warn!(env = %env, "config: invalid value, ignoring"),
            }
        }
    }
    layer
}

/// Env values reuse TOML scalar syntax, plus loose booleans
/// (`1/0/yes/no/on/off`) and bare strings for string keys.
fn parse_env_value(key: &str, raw: &str) -> Option<Scalar> {
    if let Some(scalar) = parse_scalar(raw) {
        // Bare `1`/`0` should read as booleans for flag keys.
        if let Scalar::Int(n) = scalar {
            match key {
                "index" | "pdf_text" | "ocr" | "enrich_links" | "mouse" => {
                    return match n {
                        1 => Some(Scalar::Bool(true)),
                        0 => Some(Scalar::Bool(false)),
                        _ => None,
                    };
                }
                _ => return Some(Scalar::Int(n)),
            }
        }
        return Some(scalar);
    }
    match key {
        "index" | "pdf_text" | "ocr" | "enrich_links" | "mouse" => {
            match raw.to_ascii_lowercase().as_str() {
                "1" | "true" | "yes" | "on" => Some(Scalar::Bool(true)),
                "0" | "false" | "no" | "off" => Some(Scalar::Bool(false)),
                _ => None,
            }
        }
        "locale" | "theme" | "store" => {
            if raw.is_empty() {
                None
            } else {
                Some(Scalar::Str(raw.to_string()))
            }
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn config_template_round_trips_to_defaults() {
        let parsed = parse_toml_subset(&Config::init_template());
        // Start from non-defaults, then re-apply the template: must restore.
        let mut config = Config {
            concurrency: 3,
            locale: "xx".to_string(),
            ..Default::default()
        };
        config.apply_layer(&parsed, "test");
        assert_eq!(config, Config::default());
    }

    #[test]
    fn config_subset_parser_shapes() {
        let text = "# comment\n\
                    [ignored_section]\n\
                    concurrency = 4 # trailing\n\
                    locale = \"es\" # trailing\n\
                    theme = 'dark'\n\
                    max_rps = 2.5\n\
                    index = false\n\
                    unknown_future_key = 1\n\
                    broken line\n\
                    bad value = [1,2]\n";
        let map = parse_toml_subset(text);
        assert_eq!(map.get("concurrency"), Some(&Scalar::Int(4)));
        assert_eq!(map.get("locale"), Some(&Scalar::Str("es".to_string())));
        assert_eq!(map.get("max_rps"), Some(&Scalar::Float(2.5)));
        assert_eq!(map.get("index"), Some(&Scalar::Bool(false)));
        assert!(map.contains_key("unknown_future_key"));
        assert!(!map.contains_key("broken"));
        assert!(!map.contains_key("bad value"));
    }

    #[test]
    fn config_comment_inside_quotes_kept() {
        let map = parse_toml_subset("locale = \"ca#x\" # real comment\n");
        assert_eq!(map.get("locale"), Some(&Scalar::Str("ca#x".to_string())));
    }

    #[test]
    fn config_clamps_and_invalid_ignored() {
        let mut config = Config::default();
        config.apply_layer(
            &BTreeMap::from([
                ("concurrency".to_string(), Scalar::Int(99)),
                ("max_rps".to_string(), Scalar::Float(f64::NAN)),
                ("locale".to_string(), Scalar::Int(3)),
            ]),
            "test",
        );
        assert_eq!(config.concurrency, 32);
        assert_eq!(config.rate_max_rps, 8.0);
        assert_eq!(config.locale, "ca");
    }

    #[test]
    fn config_validate_flags_bad_store() {
        let mut config = Config::default();
        assert!(config.validate().is_empty());
        config.store = "tape".to_string();
        let issues = config.validate();
        assert_eq!(issues.len(), 1);
        assert!(issues[0].contains("store"));
    }

    #[test]
    fn config_show_has_no_secrets_shape() {
        // By construction: the only string fields are locale/theme/store.
        let shown = Config::default().show_redacted();
        assert!(shown.contains("concurrency = 8"));
        assert!(!shown.to_ascii_lowercase().contains("token"));
    }
}
