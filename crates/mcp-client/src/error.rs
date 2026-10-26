use thiserror::Error;

/// Errors a `McpClient` call can return. Mapped to [`workforce_common::redact::redact_secrets`]
/// at the boundaries so vendor-side cleartext credentials never end up in logs.
#[derive(Debug, Error)]
pub enum McpError {
    /// The server could not be reached: the connection was refused or
    /// timed out, DNS or the TLS handshake failed, or the request timed
    /// out before any response arrived. A caller may treat the server as
    /// down for a while, but a request that timed out may still have run.
    #[error("server unreachable: {0}")]
    Unreachable(String),

    /// HTTP/network failure after the server was reached: a non-2xx
    /// status, a broken or malformed body, or an SSE stream that ended
    /// early.
    #[error("transport error: {0}")]
    Transport(String),

    /// Server returned a JSON-RPC `error` envelope.
    #[error("server error (code {code}): {message}")]
    ServerError {
        code: i32,
        message: String,
        data: Option<serde_json::Value>,
    },

    /// HTTP 401/403 or JSON-RPC code -32001 — the caller's
    /// `AuthProvider` should obtain a fresh credential.
    #[error("authentication failed: {0}")]
    AuthExpired(String),

    /// HTTP 429 — the server refused the request before executing it.
    /// `retry_after_secs` carries the Retry-After header when sent.
    #[error("rate limited: {message}")]
    RateLimited {
        message: String,
        retry_after_secs: Option<u64>,
    },

    /// Tool registration / response violated [`crate::McpLimits`]
    /// (oversize description, malformed name, etc.).
    /// Treated as a server-trust violation, not a transport bug.
    #[error("schema violation: {0}")]
    SchemaViolation(String),

    /// The number of tools to *register* (after any keep-filter) exceeds
    /// [`crate::McpLimits::max_tools_per_server`]. Typed so the caller can
    /// distinguish "too many tools" — which an allowlist can fix — from a
    /// generic schema violation. `kept` is `cap + 1` (the count bails the
    /// instant it crosses the cap), so it is a floor, not the true total.
    #[error("registered tool count {kept} exceeds cap {cap} — filter to a subset")]
    ToolCountExceeded { kept: usize, cap: usize },

    /// The server advertised more *raw* tools than
    /// [`crate::McpLimits::max_raw_tools`] before the keep-filter could
    /// bound the result. A DoS backstop against a server that pages
    /// without end; distinct from the registration cap.
    #[error("server advertised more than {limit} raw tools; refusing to page further")]
    RawToolLimitExceeded { limit: usize },

    /// Caller-cancelled in-flight request. Surfaced after the
    /// `notifications/cancelled` JSON-RPC notification has been
    /// fire-and-forgotten upstream.
    #[error("cancelled")]
    Cancelled,

    /// Result body exceeded [`crate::McpLimits::max_content_bytes`].
    #[error("response body exceeds {limit} bytes")]
    BodyTooLarge { limit: usize },

    /// Caller violated the type contract (e.g. called `tools_call`
    /// before `initialize`).
    #[error("protocol error: {0}")]
    Protocol(String),
}

impl McpError {
    /// Whether the error is retryable in principle (transport-level),
    /// including an unreachable server.
    pub fn is_transport(&self) -> bool {
        matches!(self, Self::Transport(_) | Self::Unreachable(_))
    }

    /// Whether the server could not be reached at all, as opposed to
    /// answering with an HTTP or protocol failure.
    pub fn is_unreachable(&self) -> bool {
        matches!(self, Self::Unreachable(_))
    }

    /// Whether the caller should refresh credentials and retry.
    pub fn is_auth(&self) -> bool {
        matches!(self, Self::AuthExpired(_))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classifiers_partition_auth_and_transport() {
        assert!(McpError::AuthExpired("401".into()).is_auth());
        assert!(!McpError::AuthExpired("401".into()).is_transport());
        assert!(McpError::Transport("boom".into()).is_transport());
        assert!(!McpError::Transport("boom".into()).is_auth());
        assert!(!McpError::Transport("boom".into()).is_unreachable());
    }

    #[test]
    fn unreachable_is_a_transport_error_but_not_the_reverse() {
        let unreachable = McpError::Unreachable("refused".into());
        assert!(unreachable.is_unreachable());
        assert!(unreachable.is_transport());
        assert!(!unreachable.is_auth());
        assert!(!McpError::AuthExpired("401".into()).is_unreachable());
    }
}
