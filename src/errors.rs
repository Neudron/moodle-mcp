use thiserror::Error;

/// Stable error codes surfaced over MCP tool responses and the CLI.
/// Wire format is [`McpCode::as_str`]; never rename a variant's string.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum McpCode {
    Auth,
    NotFound,
    RateLimit,
    Net,
    Invalid,
}

impl McpCode {
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Auth => "AUTH",
            Self::NotFound => "NOT_FOUND",
            Self::RateLimit => "RATE_LIMIT",
            Self::Net => "NET",
            Self::Invalid => "INVALID",
        }
    }
}

/// Core fallible-operation error shared by all shells (MCP/CLI/TUI).
///
/// Contract: these strings never carry secret material. Keep tokens,
/// query strings and raw request bodies out of every variant.
#[derive(Debug, Error)]
pub enum CoreError {
    #[error("auth: {0}")]
    Auth(String),
    #[error("not found: {0}")]
    NotFound(String),
    #[error("rate limited, retry after {0}s")]
    RateLimited(u64),
    #[error("network: {0}")]
    Net(String),
    #[error("invalid params: {0}")]
    Invalid(String),
    #[error(transparent)]
    Other(#[from] anyhow::Error),
}

impl CoreError {
    #[must_use]
    pub fn code(&self) -> McpCode {
        match self {
            Self::Auth(_) => McpCode::Auth,
            Self::NotFound(_) => McpCode::NotFound,
            Self::RateLimited(_) => McpCode::RateLimit,
            Self::Net(_) => McpCode::Net,
            Self::Invalid(_) => McpCode::Invalid,
            Self::Other(_) => McpCode::Net,
        }
    }
}

/// Scrub token material from a string before it reaches logs, errors or
/// snapshots (plan #1053): every `token=<value>` / `wstoken=<value>`
/// occurrence becomes `token=REDACTED` / `wstoken=REDACTED`. Over-redacts
/// rather than leaks: any key ending in `token=` is scrubbed.
#[must_use]
pub fn redact_token(input: &str) -> String {
    const TAG: &str = "REDACTED";
    let bytes = input.as_bytes();
    let mut out = String::with_capacity(input.len());
    let mut i = 0;
    while i < bytes.len() {
        let rest = &input[i..];
        // Longest key first so `wstoken=` is not partially matched.
        let key_len = if rest.starts_with("wstoken=") {
            Some("wstoken=".len())
        } else if rest.starts_with("token=") {
            Some("token=".len())
        } else {
            None
        };
        match key_len {
            Some(k) => {
                let key = &rest[..k];
                out.push_str(key);
                out.push_str(TAG);
                i += k;
                while i < bytes.len() && !is_token_delim(bytes[i]) {
                    i += 1;
                }
            }
            None => {
                // `bytes[i]` is a single byte of a UTF-8 boundary-safe walk:
                // ASCII fast path, otherwise copy the whole char.
                let ch = rest.chars().next().unwrap_or_default();
                out.push(ch);
                i += ch.len_utf8().max(1);
            }
        }
    }
    out
}

fn is_token_delim(b: u8) -> bool {
    matches!(
        b,
        b'&' | b' ' | b'\t' | b'\n' | b'\r' | b'"' | b'\'' | b'<' | b'>' | b')' | b',' | b';'
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn codes_and_display() {
        assert_eq!(CoreError::Auth("bad token".into()).code(), McpCode::Auth);
        assert_eq!(
            CoreError::NotFound("course 7".into()).code(),
            McpCode::NotFound
        );
        assert_eq!(CoreError::RateLimited(30).code(), McpCode::RateLimit);
        assert_eq!(CoreError::Net("dns".into()).code(), McpCode::Net);
        assert_eq!(
            CoreError::Invalid("course_id".into()).code(),
            McpCode::Invalid
        );
        assert_eq!(
            CoreError::Other(anyhow::anyhow!("boom")).code(),
            McpCode::Net
        );
        assert_eq!(McpCode::Auth.as_str(), "AUTH");
        assert_eq!(McpCode::NotFound.as_str(), "NOT_FOUND");
        assert_eq!(McpCode::RateLimit.as_str(), "RATE_LIMIT");
        assert_eq!(McpCode::Net.as_str(), "NET");
        assert_eq!(McpCode::Invalid.as_str(), "INVALID");
        assert_eq!(
            CoreError::RateLimited(30).to_string(),
            "rate limited, retry after 30s"
        );
    }

    #[test]
    fn redaction_scrubs_token_forms() {
        let fake = "FAKETOKEN-0123456789abcdef";
        let cases = [
            format!("https://x.invalid/f?token={fake}&a=1"),
            format!("https://x.invalid/f?a=1&token={fake}"),
            format!("wstoken={fake}&wsfunction=y"),
            format!("download http 500: https://x.invalid/f?token={fake}"),
            format!("url=https://x.invalid/f?token={fake}')"),
        ];
        for raw in cases {
            let redacted = redact_token(&raw);
            assert!(!redacted.contains(fake), "leak: {redacted}");
            assert!(redacted.contains("REDACTED"), "over-scrubbed: {redacted}");
        }
        // Innocent text passes through untouched.
        assert_eq!(redact_token("no secrets here"), "no secrets here");
        assert_eq!(redact_token(""), "");
    }
}
