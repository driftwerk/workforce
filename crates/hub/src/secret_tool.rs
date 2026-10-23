//! `secret.list` — let the agent discover the named secrets a user set
//! via `cmd: secret <name> <value>` in the current conversation. Returns names only; values
//! never leave the cache. The agent references a listed secret as
//! `[[secret:<name>]]` in a credential field of a secret-sink tool (e.g.
//! `vault.set`), where it is substituted to cleartext at the boundary.

use std::time::Instant;

use async_trait::async_trait;
use eyre::Result;
use workforce_agent::runner::CURRENT_SESSION_KEY;
use workforce_agent::tools::{Tool, ToolResult};
use workforce_common::secret_marker::global_cache;

pub struct SecretListTool;

#[async_trait]
impl Tool for SecretListTool {
    fn name(&self) -> &str {
        "secret.list"
    }

    fn description(&self) -> &str {
        "List the names of secrets the user set via the `cmd: secret <name> <value>` command \
         in this channel (shared across the channel root and its threads). Values are never \
         returned. Reference a secret as `[[secret:<name>]]` in a credential field of a \
         secret-sink tool — `vault.set`, or `http.request`'s `headers`, `query`, `body`, or \
         `body_text` — and never ask the user to paste it again. If you need \
         the user to set a NEW secret, always tell them to use `cmd: secret <name> <value>` — it \
         works on every surface, including Slack threads, where a leading-`/` slash command \
         silently fails, so never tell the user to type a `/`-prefixed command."
    }

    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {},
            "additionalProperties": false
        })
    }

    async fn execute(&self, _input: serde_json::Value) -> Result<ToolResult> {
        let start = Instant::now();
        let names = CURRENT_SESSION_KEY
            .try_with(|k| global_cache().map(|c| c.list_named(k)).unwrap_or_default())
            .unwrap_or_default();
        Ok(ToolResult::success(
            serde_json::json!({ "secrets": names }),
            start.elapsed().as_millis() as u64,
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use workforce_common::secret::Secret;
    use workforce_common::secret_marker::{MarkerId, SecretCache, SecretSource};

    use crate::secret_cache::testing::GlobalCache;

    #[tokio::test]
    async fn lists_named_secrets_for_current_session_without_values() {
        let cache: Arc<dyn SecretCache> =
            Arc::new(crate::secret_cache::InMemorySecretCache::with_defaults());
        cache.capture_named(
            "slack:T1:C1:slash",
            &MarkerId::named("datadog_api_key").unwrap(),
            Secret::new("feedface000000000000000000000001".into()),
            SecretSource::SlashCommand { user: "U1".into() },
        );
        let _installed = GlobalCache::install(cache);

        let result = CURRENT_SESSION_KEY
            .scope("slack:T1:C1:1700000000.5".to_string(), async {
                SecretListTool.execute(serde_json::json!({})).await
            })
            .await
            .unwrap();

        assert_eq!(
            result.output["secrets"],
            serde_json::json!(["datadog_api_key"])
        );
        let rendered = serde_json::to_string(&result.output).unwrap();
        assert!(!rendered.contains("feedface000000000000000000000001"));
    }

    #[tokio::test]
    async fn empty_when_no_session_or_no_secrets() {
        let _installed = GlobalCache::install(Arc::new(
            crate::secret_cache::InMemorySecretCache::with_defaults(),
        ));

        // No task-local session key installed.
        let result = SecretListTool.execute(serde_json::json!({})).await.unwrap();
        assert_eq!(result.output["secrets"], serde_json::json!([]));
    }
}
