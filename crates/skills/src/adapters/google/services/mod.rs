//! One sub-module per Google service. Each module exposes a `pub fn tools(ctx)
//! -> Vec<Arc<dyn Tool>>` that returns every tool that service provides.
//!
//! The aggregator at the bottom of this file collects them all so
//! `GoogleSkill::tools()` stays one line.

use std::sync::Arc;

use workforce_agent::tools::Tool;

use super::api::GoogleApiContext;

pub mod calendar;
pub mod chat;
pub mod contacts;
pub mod docs;
pub mod drive;
pub mod gmail;
pub mod sheets;
pub mod slides;
pub mod tasks;

/// Build the complete tool set for every service the google skill supports.
pub fn build_all(ctx: Arc<GoogleApiContext>) -> Vec<Arc<dyn Tool>> {
    let mut out: Vec<Arc<dyn Tool>> = Vec::new();
    out.extend(calendar::tools(ctx.clone()));
    out.extend(drive::tools(ctx.clone()));
    out.extend(sheets::tools(ctx.clone()));
    out.extend(docs::tools(ctx.clone()));
    out.extend(gmail::tools(ctx.clone()));
    out.extend(tasks::tools(ctx.clone()));
    out.extend(contacts::tools(ctx.clone()));
    out.extend(chat::tools(ctx.clone()));
    out.extend(slides::tools(ctx));
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::collections::HashSet;

    use workforce_common::settings::SecretStore;

    use crate::adapters::google::api::GoogleApi;
    use crate::adapters::google::oauth::OAuthClientCreds;
    use crate::adapters::google::token_store::TokenStore;

    struct NullStore;

    impl SecretStore for NullStore {
        fn get(&self, _path: &str) -> Option<String> {
            None
        }
        fn set(&self, _path: &str, _value: &str) -> eyre::Result<()> {
            Ok(())
        }
        fn delete(&self, _path: &str) -> eyre::Result<bool> {
            Ok(false)
        }
        fn list(&self, _prefix: Option<&str>) -> Vec<String> {
            Vec::new()
        }
    }

    fn context() -> Arc<GoogleApiContext> {
        let creds = OAuthClientCreds {
            client_id: "test".into(),
            client_secret: "test".into(),
            redirect_uri: None,
        };
        let http = reqwest::Client::new();
        let tokens = TokenStore::new(Arc::new(NullStore), "default".into(), creds, http.clone());
        Arc::new(GoogleApiContext::single(Arc::new(GoogleApi::new(
            http, tokens,
        ))))
    }

    #[test]
    fn only_a_tool_that_sends_to_someone_writes_to_its_surface() {
        use serde_json::json;
        use workforce_agent::tools::SurfaceEffect;
        let tools = build_all(context());
        let effect = |name: &str, input: serde_json::Value| {
            tools
                .iter()
                .find(|t| t.name() == name)
                .and_then(|t| t.surface_effect(&input))
        };
        let writes = |name: &str, input| {
            effect(name, input)
                .and_then(|e| e.destination())
                .map(|d| format!("{} {}", d.platform, d.target))
        };
        assert_eq!(
            writes(
                "google.gmail.send",
                json!({"to": "a@x.com", "bcc": "b@x.com"})
            ),
            Some("email a@x.com, b@x.com".into())
        );
        assert_eq!(
            writes(
                "google.chat.messages_send",
                json!({"space": "spaces/AAA", "text": "hi"})
            ),
            Some("google_chat spaces/aaa".into())
        );
        assert_eq!(
            writes(
                "google.chat.dm_send",
                json!({"user": "users/1", "text": "hi"})
            ),
            Some("google_chat @users/1".into())
        );
        assert!(matches!(
            effect("google.gmail.draft_send", json!({"draft_id": "d1"})),
            Some(SurfaceEffect::Writes { .. })
        ));
        assert_eq!(effect("google.gmail.search", json!({})), None);
        assert_eq!(effect("google.chat.messages_list", json!({})), None);
    }

    /// Regression: the `tool_names()` enumeration on the parent
    /// `adapters::google` module must list every tool that `build_all()`
    /// actually registers, plus `google.setup`. Drift here breaks the
    /// `/cc` MCP allow-list: a missing name means the model can't call
    /// the tool, an extra name is ignored.
    #[test]
    fn parent_tool_names_covers_build_all_plus_setup() {
        let mut registered: HashSet<String> = build_all(context())
            .iter()
            .map(|t| t.name().to_string())
            .collect();
        // `google.setup` is registered separately by skill_loader, not by
        // build_all, but `tool_names()` covers both.
        registered.insert("google.setup".to_string());

        let declared: HashSet<String> = crate::adapters::google::tool_names()
            .iter()
            .map(|s| (*s).to_string())
            .collect();

        let missing: Vec<&String> = registered.difference(&declared).collect();
        let extra: Vec<&String> = declared.difference(&registered).collect();
        assert!(
            missing.is_empty() && extra.is_empty(),
            "google::tool_names() drift — missing: {missing:?}, extra: {extra:?}"
        );
    }

    /// Regression: every tool name must stay unique after the runtime normalizes
    /// dots to underscores for LLM APIs (see `agent-runtime::runner::to_llm_name`).
    /// A collision causes Anthropic to reject the request with
    /// `"tools: Tool names must be unique."` and breaks every provider.
    #[test]
    fn build_all_has_no_duplicate_tool_names_after_normalization() {
        let mut seen: HashSet<String> = HashSet::new();
        let duplicates: Vec<String> = build_all(context())
            .iter()
            .map(|t| t.name().replace('.', "_"))
            .filter(|n| !seen.insert(n.clone()))
            .collect();

        assert!(
            duplicates.is_empty(),
            "duplicate tool names after dot→underscore normalization: {duplicates:?}",
        );
    }
}
