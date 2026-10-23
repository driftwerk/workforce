use async_trait::async_trait;

use crate::error::McpError;

/// Pluggable credential source. Each MCP server gets its own
/// implementation: GitHub-remote uses a static PAT lookup, Linear
/// uses an `oauth-client::TokenStore` via a thin adapter, etc.
///
/// Implementations should:
/// - Refresh expired credentials transparently before returning.
/// - Map "credential genuinely cannot be obtained" to
///   [`McpError::AuthExpired`] so the caller can prompt the user.
/// - Map transport / vault errors to [`McpError::Transport`].
#[async_trait]
pub trait AuthProvider: Send + Sync {
    /// Return the full `Authorization` header value, e.g.
    /// `"Bearer ghp_xxx"` or `"Bearer eyJ..."`. The MCP client
    /// sends it verbatim; provider impls own the prefix.
    async fn authorization_header(&self) -> Result<String, McpError>;
}

/// Static-bearer auth — the simplest case (GitHub PAT,
/// long-lived API key). Constructed once with the token; never
/// refreshes.
pub struct StaticBearer {
    header: String,
}

impl StaticBearer {
    pub fn new(token: impl Into<String>) -> Self {
        Self {
            header: format!("Bearer {}", token.into()),
        }
    }
}

#[async_trait]
impl AuthProvider for StaticBearer {
    async fn authorization_header(&self) -> Result<String, McpError> {
        Ok(self.header.clone())
    }
}

/// No credential: the transport sends no `Authorization` header at all
/// (an empty value means "none", not an empty header). For a server that
/// admits anonymous discovery, or a probe meant to learn whether it does.
pub struct NoAuth;

#[async_trait]
impl AuthProvider for NoAuth {
    async fn authorization_header(&self) -> Result<String, McpError> {
        Ok(String::new())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn static_bearer_prefixes_with_bearer() {
        let auth = StaticBearer::new("ghp_xxx");
        assert_eq!(auth.authorization_header().await.unwrap(), "Bearer ghp_xxx");
    }

    #[tokio::test]
    async fn no_auth_yields_the_empty_value_the_transport_omits() {
        assert_eq!(NoAuth.authorization_header().await.unwrap(), "");
    }
}
