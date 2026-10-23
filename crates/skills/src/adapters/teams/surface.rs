//! [`ChannelSurface`] implementation for Microsoft Teams.
//!
//! Teams replies are addressed by a per-conversation `serviceUrl` that only
//! arrives on inbound activities — but the generic outbound `send` carries just a
//! `teams:<conversationId>` channel. So the surface remembers each conversation's
//! `serviceUrl` (the inbound route calls [`TeamsSurface::record_service_url`]) and
//! resolves it when sending. A send to a conversation the bot has never heard
//! from fails with a clear error rather than guessing.

use std::collections::HashMap;
use std::sync::{Arc, RwLock};

use async_trait::async_trait;
use eyre::{eyre, Result};

use workforce_common::channels::{
    channel_id, ChannelCapabilities, ChannelSurface, OutboundMessage, Reaction, SentMessage,
};

use super::client::BotFrameworkClient;
use super::types::Activity;

/// A Teams channel surface backed by the Bot Framework connector.
pub struct TeamsSurface {
    client: Arc<BotFrameworkClient>,
    service_urls: RwLock<HashMap<String, String>>,
}

impl TeamsSurface {
    pub fn new(app_id: impl Into<String>, app_password: impl Into<String>) -> Self {
        Self::from_client(Arc::new(BotFrameworkClient::new(app_id, app_password)))
    }

    pub fn from_client(client: Arc<BotFrameworkClient>) -> Self {
        Self {
            client,
            service_urls: RwLock::new(HashMap::new()),
        }
    }

    /// Remember a conversation's `serviceUrl`. The inbound route calls this for
    /// every activity so later outbound sends can reach the connector.
    pub fn record_service_url(
        &self,
        conversation_id: impl Into<String>,
        service_url: impl Into<String>,
    ) {
        self.service_urls
            .write()
            .expect("service_urls")
            .insert(conversation_id.into(), service_url.into());
    }

    fn service_url_for(&self, conversation_id: &str) -> Result<String> {
        self.service_urls
            .read()
            .expect("service_urls")
            .get(conversation_id)
            .cloned()
            .ok_or_else(|| {
                eyre!(
                    "no serviceUrl known for conversation {conversation_id}; \
                     the bot must receive an inbound activity from it first"
                )
            })
    }
}

#[async_trait]
impl ChannelSurface for TeamsSurface {
    fn platform(&self) -> &str {
        "teams"
    }

    fn capabilities(&self) -> ChannelCapabilities {
        ChannelCapabilities {
            send_message: true,
            edit_message: true,
            delete_message: true,
            threads: true,
            ..Default::default()
        }
    }

    async fn add_reaction(&self, _reaction: &Reaction) -> Result<()> {
        Err(eyre!("reactions are not supported on teams yet"))
    }

    async fn remove_reaction(&self, _reaction: &Reaction) -> Result<()> {
        Err(eyre!("reactions are not supported on teams yet"))
    }

    async fn send_message(&self, message: &OutboundMessage) -> Result<SentMessage> {
        let conversation_id = channel_id(&message.channel);
        let service_url = self.service_url_for(conversation_id)?;
        let activity = Activity {
            activity_type: "message".to_string(),
            text: Some(message.text.clone()),
            ..Activity::default()
        };
        let id = self
            .client
            .send_activity(&service_url, conversation_id, &activity)
            .await?;
        Ok(SentMessage {
            channel: message.channel.clone(),
            message_id: id,
            thread_id: message.thread_id.clone(),
            platform: "teams".to_string(),
        })
    }

    async fn edit_message(&self, channel: &str, message_id: &str, text: &str) -> Result<()> {
        let conversation_id = channel_id(channel);
        let service_url = self.service_url_for(conversation_id)?;
        let activity = Activity {
            activity_type: "message".to_string(),
            text: Some(text.to_string()),
            ..Activity::default()
        };
        self.client
            .update_activity(&service_url, conversation_id, message_id, &activity)
            .await
    }

    async fn delete_message(&self, channel: &str, message_id: &str) -> Result<()> {
        let conversation_id = channel_id(channel);
        let service_url = self.service_url_for(conversation_id)?;
        self.client
            .delete_activity(&service_url, conversation_id, message_id)
            .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    fn surface_for(server: &MockServer) -> TeamsSurface {
        let client = Arc::new(
            BotFrameworkClient::new("app", "secret")
                .with_token_url(format!("{}/token", server.uri())),
        );
        TeamsSurface::from_client(client)
    }

    async fn mount_token(server: &MockServer) {
        Mock::given(method("POST"))
            .and(path("/token"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "access_token": "tok",
                "token_type": "Bearer",
                "expires_in": 3600
            })))
            .mount(server)
            .await;
    }

    fn outbound(channel: &str, text: &str) -> OutboundMessage {
        OutboundMessage {
            channel: channel.into(),
            text: text.into(),
            thread_id: None,
            username: None,
            icon_url: None,
            blocks: None,
        }
    }

    #[tokio::test]
    async fn send_resolves_cached_service_url_and_returns_sent() {
        let server = MockServer::start().await;
        mount_token(&server).await;
        Mock::given(method("POST"))
            .and(path("/v3/conversations/19:c/activities"))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(serde_json::json!({ "id": "m1" })),
            )
            .expect(1)
            .mount(&server)
            .await;

        let surface = surface_for(&server);
        surface.record_service_url("19:c", server.uri());
        let sent = surface
            .send_message(&outbound("teams:19:c", "hello"))
            .await
            .unwrap();
        assert_eq!(sent.message_id, "m1");
        assert_eq!(sent.platform, "teams");
        assert_eq!(sent.channel, "teams:19:c");
    }

    #[tokio::test]
    async fn send_without_known_service_url_errors() {
        let server = MockServer::start().await;
        let surface = surface_for(&server);
        let err = surface
            .send_message(&outbound("teams:19:unknown", "x"))
            .await;
        assert!(err.is_err());
    }

    #[tokio::test]
    async fn delete_hits_connector() {
        let server = MockServer::start().await;
        mount_token(&server).await;
        Mock::given(method("DELETE"))
            .and(path("/v3/conversations/19:c/activities/m1"))
            .respond_with(ResponseTemplate::new(200))
            .expect(1)
            .mount(&server)
            .await;

        let surface = surface_for(&server);
        surface.record_service_url("19:c", server.uri());
        surface.delete_message("teams:19:c", "m1").await.unwrap();
    }

    #[test]
    fn capabilities_advertise_text_messaging_only() {
        let surface = TeamsSurface::new("app", "secret");
        let caps = surface.capabilities();
        assert!(caps.send_message && caps.edit_message && caps.delete_message && caps.threads);
        assert!(!caps.reactions && !caps.rich_text && !caps.file_upload);
        assert_eq!(surface.platform(), "teams");
    }
}
