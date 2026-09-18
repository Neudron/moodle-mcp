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
}
