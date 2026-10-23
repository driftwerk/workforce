//! `messaging.whoami` — identify the active user from the canonical
//! [`workforce_identity::store::IdentityStore`].
//!
//! Returns identity UUID, display name, email, and every platform-linked
//! external ID for the user the current turn belongs to. The agent uses
//! the returned `external_id` for each platform to drive follow-up calls
//! like `messaging.member_info(platform=…, user_id=…)`.

use std::sync::Arc;
use std::time::Instant;

use async_trait::async_trait;
use eyre::Result;
use parking_lot::RwLock;
use serde_json::{json, Value};
use uuid::Uuid;
use workforce_agent::tools::{Tool, ToolResult};
use workforce_identity::store::IdentityStore;

pub struct WhoAmITool {
    identity_store: Arc<RwLock<Option<Arc<dyn IdentityStore>>>>,
}

impl WhoAmITool {
    pub fn new(identity_store: Arc<RwLock<Option<Arc<dyn IdentityStore>>>>) -> Self {
        Self { identity_store }
    }
}

#[async_trait]
impl Tool for WhoAmITool {
    fn name(&self) -> &str {
        "messaging.whoami"
    }

    fn description(&self) -> &str {
        "Identify the user this conversation belongs to: returns identity_id, \
         display_name, email, and platform-linked external IDs (e.g. Slack \
         user id 'U03…'). Use the returned `external_id` for follow-up tools \
         like `messaging.member_info(platform=…, user_id=…)` or to personalise \
         responses without asking the user for their name."
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {},
            "additionalProperties": false,
        })
    }

    async fn execute(&self, _input: Value) -> Result<ToolResult> {
        let start = Instant::now();
        let elapsed = || start.elapsed().as_millis() as u64;

        let Some(user_id) = crate::user_config::current_identity_id() else {
            return Ok(ToolResult::failure(
                "No active user in this context — `messaging.whoami` is only meaningful \
                 inside a user-initiated turn (e.g. a Slack message). Background jobs \
                 and system-initiated workflows have no active user.",
                elapsed(),
            ));
        };

        let store = match self.identity_store.read().clone() {
            Some(store) => store,
            None => {
                return Ok(ToolResult::failure(
                    "Identity store is not configured on this hub — \
                     `messaging.whoami` cannot resolve users without it.",
                    elapsed(),
                ));
            }
        };

        let Ok(uuid) = Uuid::parse_str(&user_id) else {
            return Ok(ToolResult::failure(
                format!(
                    "Active user id '{user_id}' is not a UUID — identity store cannot \
                     resolve it. This usually means the request bypassed identity \
                     resolution (anonymous or system-scoped)."
                ),
                elapsed(),
            ));
        };

        let identity = match store.get_by_id(uuid) {
            Ok(Some(identity)) => identity,
            Ok(None) => {
                return Ok(ToolResult::failure(
                    format!(
                        "Active user {uuid} not found in the identity store — \
                         provisioning may be incomplete."
                    ),
                    elapsed(),
                ));
            }
            Err(e) => {
                return Ok(ToolResult::failure(
                    format!("Identity store lookup failed: {e}"),
                    elapsed(),
                ));
            }
        };

        let platforms: Vec<Value> = identity
            .linked_ids
            .iter()
            .map(|(platform, external_id)| {
                json!({ "platform": platform, "external_id": external_id })
            })
            .collect();

        Ok(ToolResult::success(
            json!({
                "identity_id": identity.id.to_string(),
                "display_name": identity.display_name,
                "email": identity.email,
                "platforms": platforms,
            }),
            elapsed(),
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use uuid::Uuid;
    use workforce_identity::store::{IdentityFilter, IdentityStore};
    use workforce_identity::types::{Identity, IdentityRole, IdentityStatus};

    #[derive(Default)]
    struct InMemoryStore {
        identities: parking_lot::Mutex<Vec<Identity>>,
    }

    impl IdentityStore for InMemoryStore {
        fn create(&self, identity: &Identity) -> Result<()> {
            self.identities.lock().push(identity.clone());
            Ok(())
        }
        fn get_by_id(&self, id: Uuid) -> Result<Option<Identity>> {
            Ok(self.identities.lock().iter().find(|i| i.id == id).cloned())
        }
        fn get_by_email(&self, email: &str) -> Result<Option<Identity>> {
            Ok(self
                .identities
                .lock()
                .iter()
                .find(|i| i.email.as_deref() == Some(email))
                .cloned())
        }
        fn get_by_linked_id(&self, platform: &str, platform_id: &str) -> Result<Option<Identity>> {
            Ok(self
                .identities
                .lock()
                .iter()
                .find(|i| i.linked_ids.get(platform).map(|s| s.as_str()) == Some(platform_id))
                .cloned())
        }
        fn update(&self, identity: &Identity) -> Result<()> {
            let mut all = self.identities.lock();
            if let Some(slot) = all.iter_mut().find(|i| i.id == identity.id) {
                *slot = identity.clone();
            }
            Ok(())
        }
        fn delete(&self, id: Uuid) -> Result<()> {
            self.identities.lock().retain(|i| i.id != id);
            Ok(())
        }
        fn list(&self, _filter: &IdentityFilter) -> Result<Vec<Identity>> {
            Ok(self.identities.lock().clone())
        }
        fn count_filtered(&self, _filter: &IdentityFilter) -> Result<usize> {
            Ok(self.identities.lock().len())
        }
    }

    fn make_tool(store: Arc<dyn IdentityStore>) -> WhoAmITool {
        WhoAmITool::new(Arc::new(RwLock::new(Some(store))))
    }

    #[tokio::test]
    async fn whoami_fails_without_current_user() {
        let store = Arc::new(InMemoryStore::default()) as Arc<dyn IdentityStore>;
        let result = make_tool(store).execute(json!({})).await.unwrap();
        assert!(!result.success);
        assert!(result.error.unwrap().contains("No active user"));
    }

    #[tokio::test]
    async fn whoami_fails_without_store() {
        let tool = WhoAmITool::new(Arc::new(RwLock::new(None)));
        let result = workforce_agent::runner::CURRENT_USER_ID
            .scope(Uuid::new_v4().to_string(), async {
                tool.execute(json!({})).await.unwrap()
            })
            .await;
        assert!(!result.success);
        assert!(result.error.unwrap().contains("Identity store"));
    }

    #[tokio::test]
    async fn whoami_fails_when_user_id_is_not_uuid() {
        let store = Arc::new(InMemoryStore::default()) as Arc<dyn IdentityStore>;
        let tool = make_tool(store);
        let result = workforce_agent::runner::CURRENT_USER_ID
            .scope("not-a-uuid".to_string(), async {
                tool.execute(json!({})).await.unwrap()
            })
            .await;
        assert!(!result.success);
        assert!(result.error.unwrap().contains("not a UUID"));
    }

    #[tokio::test]
    async fn whoami_fails_when_identity_missing() {
        let store = Arc::new(InMemoryStore::default()) as Arc<dyn IdentityStore>;
        let tool = make_tool(store);
        let missing = Uuid::new_v4().to_string();
        let result = workforce_agent::runner::CURRENT_USER_ID
            .scope(missing.clone(), async {
                tool.execute(json!({})).await.unwrap()
            })
            .await;
        assert!(!result.success);
        assert!(result.error.unwrap().contains("not found"));
    }

    #[tokio::test]
    async fn whoami_returns_identity_with_platforms() {
        let store = Arc::new(InMemoryStore::default());
        let mut identity = Identity::new("Kingsley Hendrickse", IdentityRole::Member)
            .with_email("kingsley@example.com")
            .with_status(IdentityStatus::Active);
        identity.link_id("slack", "U03ABCXYZ");
        identity.link_id("ios_device", "device-uuid-123");
        let uid = identity.id;
        store.create(&identity).unwrap();

        let tool = make_tool(store as Arc<dyn IdentityStore>);
        let result = workforce_agent::runner::CURRENT_USER_ID
            .scope(uid.to_string(), async {
                tool.execute(json!({})).await.unwrap()
            })
            .await;

        assert!(result.success, "expected success, got {:?}", result.error);
        let output = result.output;
        assert_eq!(output["identity_id"], json!(uid.to_string()));
        assert_eq!(output["display_name"], json!("Kingsley Hendrickse"));
        assert_eq!(output["email"], json!("kingsley@example.com"));
        let platforms = output["platforms"].as_array().unwrap();
        assert_eq!(platforms.len(), 2);
        let has = |platform: &str, external_id: &str| {
            platforms
                .iter()
                .any(|p| p["platform"] == json!(platform) && p["external_id"] == json!(external_id))
        };
        assert!(has("slack", "U03ABCXYZ"));
        assert!(has("ios_device", "device-uuid-123"));
    }
}
