//! RFC 8414 Authorization Server Metadata discovery, with the RFC 9728
//! Protected Resource Metadata cascade the MCP auth spec uses. Lets a
//! self-hosted MCP server (Metabase and future services) advertise its
//! OAuth + dynamic-registration endpoints, so no per-instance URLs are
//! hardcoded.

use serde::Deserialize;

use crate::error::OAuthError;

/// The subset of RFC 8414 Authorization Server Metadata the flow needs.
#[derive(Debug, Clone, Deserialize)]
pub struct AuthServerMetadata {
    pub authorization_endpoint: String,
    pub token_endpoint: String,
    #[serde(default)]
    pub registration_endpoint: Option<String>,
    #[serde(default)]
    pub scopes_supported: Vec<String>,
}

#[derive(Debug, Deserialize)]
struct ProtectedResourceMetadata {
    #[serde(default)]
    authorization_servers: Vec<String>,
}

/// Discover authorization-server metadata for an MCP `resource_url` (the
/// MCP endpoint, e.g. `https://mb.example.com/api/mcp`).
///
/// Cascade per the MCP auth spec:
/// 1. `{origin}/.well-known/oauth-protected-resource` (RFC 9728) — its
///    first `authorization_servers` entry is the issuer.
/// 2. Otherwise the resource origin is the issuer.
/// 3. `{issuer}/.well-known/oauth-authorization-server` (RFC 8414).
pub async fn discover(
    http: &reqwest::Client,
    resource_url: &str,
) -> Result<AuthServerMetadata, OAuthError> {
    let origin = origin_of(resource_url)?;
    let issuer = protected_resource_issuer(http, &origin)
        .await
        .unwrap_or(origin);
    fetch_json(http, &well_known(&issuer, "oauth-authorization-server")?).await
}

/// Best-effort RFC 9728 lookup; any failure (absent doc, empty list)
/// yields `None` so the caller falls back to the resource origin.
async fn protected_resource_issuer(http: &reqwest::Client, origin: &str) -> Option<String> {
    let url = well_known(origin, "oauth-protected-resource").ok()?;
    let meta: ProtectedResourceMetadata = fetch_json(http, &url).await.ok()?;
    meta.authorization_servers.into_iter().next()
}

/// Scheme + host (+ port) of a URL, discarding path/query.
fn origin_of(raw: &str) -> Result<String, OAuthError> {
    let url = url::Url::parse(raw)
        .map_err(|e| OAuthError::InvalidInput(format!("invalid url `{raw}`: {e}")))?;
    Ok(url.origin().ascii_serialization())
}

/// RFC 8414 well-known URL: the `.well-known/<name>` segment is inserted
/// between the issuer's host and any issuer path component.
fn well_known(issuer: &str, name: &str) -> Result<String, OAuthError> {
    let url = url::Url::parse(issuer)
        .map_err(|e| OAuthError::InvalidInput(format!("invalid issuer `{issuer}`: {e}")))?;
    let origin = url.origin().ascii_serialization();
    let path = url.path().trim_end_matches('/');
    Ok(format!("{origin}/.well-known/{name}{path}"))
}

async fn fetch_json<T: serde::de::DeserializeOwned>(
    http: &reqwest::Client,
    url: &str,
) -> Result<T, OAuthError> {
    let resp = http
        .get(url)
        .send()
        .await
        .map_err(|e| OAuthError::Transport(e.to_string()))?;
    let status = resp.status();
    let body = resp
        .text()
        .await
        .map_err(|e| OAuthError::Transport(e.to_string()))?;
    if !status.is_success() {
        return Err(OAuthError::from_response(status.as_u16(), &body));
    }
    serde_json::from_str(&body).map_err(|e| OAuthError::Malformed(e.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    fn as_meta(auth: &str, token: &str, registration: Option<&str>) -> serde_json::Value {
        let mut v = serde_json::json!({
            "authorization_endpoint": auth,
            "token_endpoint": token,
            "scopes_supported": ["read", "write"],
        });
        if let Some(r) = registration {
            v["registration_endpoint"] = serde_json::json!(r);
        }
        v
    }

    #[tokio::test]
    async fn discovers_via_protected_resource_cascade() {
        let server = MockServer::start().await;
        let issuer = server.uri();
        Mock::given(method("GET"))
            .and(path("/.well-known/oauth-protected-resource"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "authorization_servers": [issuer]
            })))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/.well-known/oauth-authorization-server"))
            .respond_with(ResponseTemplate::new(200).set_body_json(as_meta(
                "https://mb.example.com/auth/authorize",
                "https://mb.example.com/auth/token",
                Some("https://mb.example.com/auth/register"),
            )))
            .mount(&server)
            .await;

        let meta = discover(&reqwest::Client::new(), &format!("{issuer}/api/mcp"))
            .await
            .unwrap();
        assert_eq!(
            meta.authorization_endpoint,
            "https://mb.example.com/auth/authorize"
        );
        assert_eq!(
            meta.registration_endpoint.as_deref(),
            Some("https://mb.example.com/auth/register")
        );
        assert_eq!(meta.scopes_supported, vec!["read", "write"]);
    }

    #[tokio::test]
    async fn falls_through_to_origin_issuer_when_no_protected_resource() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/.well-known/oauth-protected-resource"))
            .respond_with(ResponseTemplate::new(404))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/.well-known/oauth-authorization-server"))
            .respond_with(ResponseTemplate::new(200).set_body_json(as_meta(
                "https://mb.example.com/auth/authorize",
                "https://mb.example.com/auth/token",
                None,
            )))
            .mount(&server)
            .await;

        let meta = discover(
            &reqwest::Client::new(),
            &format!("{}/api/mcp", server.uri()),
        )
        .await
        .unwrap();
        assert_eq!(meta.token_endpoint, "https://mb.example.com/auth/token");
        assert!(meta.registration_endpoint.is_none());
    }

    #[tokio::test]
    async fn errors_when_no_metadata_anywhere() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .respond_with(ResponseTemplate::new(404))
            .mount(&server)
            .await;
        let err = discover(
            &reqwest::Client::new(),
            &format!("{}/api/mcp", server.uri()),
        )
        .await
        .unwrap_err();
        assert!(matches!(err, OAuthError::Provider { status: 404, .. }));
    }

    #[test]
    fn well_known_inserts_segment_before_issuer_path() {
        assert_eq!(
            well_known("https://h.example/tenant1", "oauth-authorization-server").unwrap(),
            "https://h.example/.well-known/oauth-authorization-server/tenant1"
        );
        assert_eq!(
            well_known("https://h.example", "oauth-authorization-server").unwrap(),
            "https://h.example/.well-known/oauth-authorization-server"
        );
    }
}
