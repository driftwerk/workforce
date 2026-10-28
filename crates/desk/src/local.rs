//! The models a local server has installed: Ollama's `/api/tags`, vLLM's
//! OpenAI-style `/v1/models`, asked with a short timeout so a server that is
//! not running never holds up the picker.

use std::time::Duration;

use serde_json::Value;

/// How long a local server has to answer.
pub const PROBE_TIMEOUT: Duration = Duration::from_millis(1500);

/// A kind of local model server.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Server {
    Ollama,
    Vllm,
}

impl Server {
    pub fn of(provider: &str) -> Option<Self> {
        match provider {
            "ollama" => Some(Self::Ollama),
            "vllm" => Some(Self::Vllm),
            _ => None,
        }
    }

    fn path(self) -> &'static str {
        match self {
            Self::Ollama => "/api/tags",
            Self::Vllm => "/v1/models",
        }
    }

    fn models(self, body: &Value) -> Vec<String> {
        let (list, field) = match self {
            Self::Ollama => ("models", "name"),
            Self::Vllm => ("data", "id"),
        };
        body[list]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|m| m[field].as_str())
            .map(str::to_string)
            .collect()
    }
}

/// `server`'s installed models at `base_url`, or `None` when it does not
/// answer as that server would. `token` is sent as a bearer key.
pub async fn installed(
    server: Server,
    base_url: &str,
    token: Option<&str>,
    timeout: Duration,
) -> Option<Vec<String>> {
    let base = base_url.trim().trim_end_matches('/');
    let base = match server {
        // An OpenAI-style base often already ends in /v1.
        Server::Vllm => base.strip_suffix("/v1").unwrap_or(base),
        Server::Ollama => base,
    };
    let client = reqwest::Client::builder().timeout(timeout).build().ok()?;
    let mut request = client.get(format!("{base}{}", server.path()));
    if let Some(token) = token.filter(|t| !t.is_empty()) {
        request = request.bearer_auth(token);
    }
    let response = request.send().await.ok()?;
    if !response.status().is_success() {
        return None;
    }
    let body: Value = response.json().await.ok()?;
    Some(server.models(&body))
}

#[cfg(test)]
mod tests {
    use axum::http::HeaderMap;
    use axum::routing::get;
    use axum::{Json, Router};
    use serde_json::json;

    use super::*;

    async fn serve(app: Router) -> String {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        format!("http://{addr}")
    }

    #[tokio::test]
    async fn ollama_lists_its_tags() {
        let url = serve(Router::new().route(
            "/api/tags",
            get(|| async {
                Json(json!({ "models": [{ "name": "llama4:8b" }, { "name": "qwen3.8:27b" }] }))
            }),
        ))
        .await;
        assert_eq!(
            installed(Server::Ollama, &format!("{url}/"), None, PROBE_TIMEOUT).await,
            Some(vec!["llama4:8b".to_string(), "qwen3.8:27b".to_string()])
        );
    }

    #[tokio::test]
    async fn vllm_lists_its_models_with_the_key_and_a_v1_base() {
        let url = serve(Router::new().route(
            "/v1/models",
            get(|headers: HeaderMap| async move {
                match headers.get("authorization").and_then(|v| v.to_str().ok()) {
                    Some("Bearer vk") => {
                        Ok(Json(json!({ "data": [{ "id": "qwen/qwen3.8-27b" }] })))
                    }
                    _ => Err(axum::http::StatusCode::UNAUTHORIZED),
                }
            }),
        ))
        .await;
        let base = format!("{url}/v1");
        assert_eq!(
            installed(Server::Vllm, &base, Some("vk"), PROBE_TIMEOUT).await,
            Some(vec!["qwen/qwen3.8-27b".to_string()])
        );
        assert_eq!(
            installed(Server::Vllm, &base, None, PROBE_TIMEOUT).await,
            None
        );
    }

    #[tokio::test]
    async fn nothing_listening_is_no_server() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        drop(listener);
        assert_eq!(
            installed(Server::Ollama, &url, None, PROBE_TIMEOUT).await,
            None
        );
    }
}
