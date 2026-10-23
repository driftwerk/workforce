use std::fmt;

use serde::Deserialize;

/// OAuth-side errors. Wrapped for `eyre::Result` interop at boundaries.
#[derive(Debug)]
pub enum OAuthError {
    /// HTTP transport failure (network, TLS, timeout).
    Transport(String),
    /// Provider returned a non-2xx status with an `error` body.
    Provider {
        status: u16,
        code: String,
        description: Option<String>,
    },
    /// Response body could not be parsed as the expected JSON shape.
    Malformed(String),
    /// Caller passed invalid input (e.g. empty scope list).
    InvalidInput(String),
}

impl fmt::Display for OAuthError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Transport(e) => write!(f, "oauth transport error: {e}"),
            Self::Provider {
                status,
                code,
                description,
            } => write!(
                f,
                "oauth provider error (HTTP {status}): {code} {}",
                description.as_deref().unwrap_or("")
            ),
            Self::Malformed(e) => write!(f, "oauth response malformed: {e}"),
            Self::InvalidInput(e) => write!(f, "oauth invalid input: {e}"),
        }
    }
}

impl OAuthError {
    /// Build a [`OAuthError::Provider`] from a non-2xx HTTP response,
    /// decoding the standard `{error, error_description}` body (RFC 6749 §5.2,
    /// RFC 7591 §3.2.2) and falling back to the raw body when it isn't JSON.
    pub(crate) fn from_response(status: u16, body: &str) -> Self {
        let parsed: ErrorBody = serde_json::from_str(body).unwrap_or_else(|_| ErrorBody {
            error: format!("http {status}"),
            error_description: Some(body.to_string()),
        });
        Self::Provider {
            status,
            code: parsed.error,
            description: parsed.error_description,
        }
    }
}

#[derive(Debug, Deserialize)]
struct ErrorBody {
    #[serde(default)]
    error: String,
    #[serde(default)]
    error_description: Option<String>,
}

impl std::error::Error for OAuthError {}
