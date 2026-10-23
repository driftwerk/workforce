//! The ChatGPT backend as this hub reaches it.

use url::Url;
use workforce_agent::llm::responses::{ResponsesClient, CHATGPT_CODEX_BASE};
use workforce_common::settings::SubscriptionConfig;

use super::chatgpt_auth::TokenRefresher;

/// The ChatGPT backend as this hub reaches it: the wire client, the token
/// refresher and the install-wide kill switch.
#[derive(Clone)]
pub struct ChatGptBackend {
    client: ResponsesClient,
    refresher: TokenRefresher,
    enabled: bool,
}

impl ChatGptBackend {
    pub fn new(config: &SubscriptionConfig) -> Self {
        let http = reqwest::Client::new();
        Self {
            client: ResponsesClient::new(
                http.clone(),
                Url::parse(CHATGPT_CODEX_BASE).expect("the ChatGPT backend URL parses"),
            ),
            refresher: TokenRefresher::new(http),
            enabled: config.chatgpt_direct,
        }
    }

    /// The wire client the backend's calls stream through.
    pub fn client(&self) -> &ResponsesClient {
        &self.client
    }

    pub fn refresher(&self) -> &TokenRefresher {
        &self.refresher
    }

    /// Whether the install lets agents call the ChatGPT backend directly.
    pub fn enabled(&self) -> bool {
        self.enabled
    }

    #[cfg(any(test, feature = "test-support"))]
    pub fn at(base: &str, token_url: String, enabled: bool) -> Self {
        let http = reqwest::Client::new();
        Self {
            client: ResponsesClient::new(http.clone(), Url::parse(base).unwrap()),
            refresher: TokenRefresher::at(http, token_url),
            enabled,
        }
    }
}
