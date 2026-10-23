//! RFC 7591 OAuth 2.0 Dynamic Client Registration. A self-hosted MCP
//! server exposing a `registration_endpoint` issues a `client_id` (and,
//! for confidential clients, a `client_secret`) on demand, so no operator
//! app pre-registration is needed.

use serde::{Deserialize, Serialize};

use crate::error::OAuthError;

/// RFC 7591 registration request. Defaults to a public PKCE client
/// (`token_endpoint_auth_method=none`) supporting the authorization-code
/// + refresh-token grants — the MCP client profile.
#[derive(Debug, Clone, Serialize)]
pub struct ClientRegistrationRequest {
    pub client_name: String,
    pub redirect_uris: Vec<String>,
    pub grant_types: Vec<String>,
    pub response_types: Vec<String>,
    pub token_endpoint_auth_method: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub scope: Option<String>,
}

impl ClientRegistrationRequest {
    pub fn public_pkce(
        client_name: impl Into<String>,
        redirect_uri: impl Into<String>,
        scope: Option<String>,
    ) -> Self {
        Self {
            client_name: client_name.into(),
            redirect_uris: vec![redirect_uri.into()],
            grant_types: vec!["authorization_code".into(), "refresh_token".into()],
            response_types: vec!["code".into()],
            token_endpoint_auth_method: "none".into(),
            scope,
        }
    }
}

/// RFC 7591 registration response. `client_secret` is absent for public
/// clients. `registration_access_token`/`registration_client_uri` are
/// retained for future registration management (rotate/delete).
#[derive(Debug, Clone, Deserialize)]
pub struct ClientRegistrationResponse {
    pub client_id: String,
    #[serde(default)]
    pub client_secret: Option<String>,
    #[serde(default)]
    pub registration_access_token: Option<String>,
    #[serde(default)]
    pub registration_client_uri: Option<String>,
}

pub async fn register_client(
    http: &reqwest::Client,
    registration_endpoint: &str,
    request: &ClientRegistrationRequest,
) -> Result<ClientRegistrationResponse, OAuthError> {
    let resp = http
        .post(registration_endpoint)
        .json(request)
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

    async fn server_returning(status: u16, body: serde_json::Value) -> MockServer {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/register"))
            .respond_with(ResponseTemplate::new(status).set_body_json(body))
            .mount(&server)
            .await;
        server
    }

    fn req() -> ClientRegistrationRequest {
        ClientRegistrationRequest::public_pkce(
            "workforce",
            "https://hub.example/api/auth/metabase/callback",
            Some("read".into()),
        )
    }

    #[tokio::test]
    async fn registers_confidential_client_with_secret() {
        let server = server_returning(
            201,
            serde_json::json!({ "client_id": "cid-1", "client_secret": "shh" }),
        )
        .await;
        let out = register_client(
            &reqwest::Client::new(),
            &format!("{}/register", server.uri()),
            &req(),
        )
        .await
        .unwrap();
        assert_eq!(out.client_id, "cid-1");
        assert_eq!(out.client_secret.as_deref(), Some("shh"));
    }

    #[tokio::test]
    async fn registers_public_client_without_secret() {
        let server = server_returning(200, serde_json::json!({ "client_id": "cid-2" })).await;
        let out = register_client(
            &reqwest::Client::new(),
            &format!("{}/register", server.uri()),
            &req(),
        )
        .await
        .unwrap();
        assert_eq!(out.client_id, "cid-2");
        assert!(out.client_secret.is_none());
    }

    #[tokio::test]
    async fn decodes_rfc7591_error_body() {
        let server = server_returning(
            400,
            serde_json::json!({
                "error": "invalid_redirect_uri",
                "error_description": "redirect_uri not allowed"
            }),
        )
        .await;
        let err = register_client(
            &reqwest::Client::new(),
            &format!("{}/register", server.uri()),
            &req(),
        )
        .await
        .unwrap_err();
        match err {
            OAuthError::Provider {
                status,
                code,
                description,
            } => {
                assert_eq!(status, 400);
                assert_eq!(code, "invalid_redirect_uri");
                assert!(description.is_some());
            }
            other => panic!("expected Provider error, got {other:?}"),
        }
    }
}
