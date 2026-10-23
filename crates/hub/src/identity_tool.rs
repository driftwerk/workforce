//! `identity.whoami` — report the current turn's identity, role, and the
//! per-capability decision view from the live role→capability matrix.
//!
//! This is the auditor-facing authorization introspection tool: ask the agent "what can I do" and
//! it answers from the same matrix every privileged tool is gated against.
//! Self-scoped — it only ever reports the caller's own role; capability
//! booleans only, never secrets.

use std::sync::Arc;
use std::time::Instant;

use async_trait::async_trait;
use eyre::Result;
use parking_lot::RwLock;
use serde_json::{json, Value};
use uuid::Uuid;
use workforce_agent::tools::{Tool, ToolResult};
use workforce_identity::store::IdentityStore;

use crate::authz::capability_summary;

pub struct WhoAmIIdentityTool {
    identity_store: Arc<RwLock<Option<Arc<dyn IdentityStore>>>>,
}

impl WhoAmIIdentityTool {
    pub fn new(identity_store: Arc<RwLock<Option<Arc<dyn IdentityStore>>>>) -> Self {
        Self { identity_store }
    }
}

#[async_trait]
impl Tool for WhoAmIIdentityTool {
    fn name(&self) -> &str {
        "identity.whoami"
    }

    fn description(&self) -> &str {
        "Report who you are talking to and what they are allowed to do: returns \
         identity_id, display_name, role (member / system_admin), and a \
         per-capability decision view (allow / ask / deny) from the live \
         authorization matrix. Use it to answer 'what can I do' or to check \
         whether the current user may perform an admin action before you try."
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
                "No active user in this context — `identity.whoami` is only meaningful \
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
                     `identity.whoami` cannot resolve roles without it.",
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

        let capabilities = capability_summary(
            identity.effective_role_key(),
            identity.status,
            &crate::authz::capability_matrix(),
        );
        Ok(ToolResult::success(
            json!({
                "identity_id": identity.id.to_string(),
                "display_name": identity.display_name,
                "role": identity.effective_role_key(),
                "capabilities": capabilities.capabilities,
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
    use workforce_identity::types::{Identity, IdentityRole};

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

    fn make_tool(store: Arc<dyn IdentityStore>) -> WhoAmIIdentityTool {
        WhoAmIIdentityTool::new(Arc::new(RwLock::new(Some(store))))
    }

    fn decision_for(output: &Value, key: &str) -> String {
        output["capabilities"]
            .as_array()
            .unwrap()
            .iter()
            .find(|c| c["capability"] == json!(key))
            .unwrap_or_else(|| panic!("capability {key} missing"))["decision"]
            .as_str()
            .unwrap()
            .to_string()
    }

    #[tokio::test]
    async fn whoami_fails_without_current_user() {
        let store = Arc::new(InMemoryStore::default()) as Arc<dyn IdentityStore>;
        let result = make_tool(store).execute(json!({})).await.unwrap();
        assert!(!result.success);
        assert!(result.error.unwrap().contains("No active user"));
    }

    #[tokio::test]
    async fn whoami_reports_member_capabilities() {
        let store = Arc::new(InMemoryStore::default());
        let identity = Identity::new("Member User", IdentityRole::Member);
        let uid = identity.id;
        store.create(&identity).unwrap();
        let tool = make_tool(store as Arc<dyn IdentityStore>);
        let result = workforce_agent::runner::CURRENT_USER_ID
            .scope(uid.to_string(), async {
                tool.execute(json!({})).await.unwrap()
            })
            .await;
        assert!(result.success, "expected success, got {:?}", result.error);
        let out = result.output;
        assert_eq!(out["identity_id"], json!(uid.to_string()));
        assert_eq!(out["role"], json!("member"));
        assert_eq!(decision_for(&out, "own_data_rw"), "allow");
        assert_eq!(decision_for(&out, "agents_manage"), "deny");
        assert_eq!(decision_for(&out, "vault_shared_delete"), "deny");
    }

    #[tokio::test]
    async fn whoami_reports_system_admin_capabilities() {
        let store = Arc::new(InMemoryStore::default());
        let identity = Identity::new("Admin User", IdentityRole::SystemAdmin);
        let uid = identity.id;
        store.create(&identity).unwrap();
        let tool = make_tool(store as Arc<dyn IdentityStore>);
        let result = workforce_agent::runner::CURRENT_USER_ID
            .scope(uid.to_string(), async {
                tool.execute(json!({})).await.unwrap()
            })
            .await;
        assert!(result.success, "expected success, got {:?}", result.error);
        let out = result.output;
        assert_eq!(out["role"], json!("system_admin"));
        assert_eq!(decision_for(&out, "agents_manage"), "allow");
        assert_eq!(decision_for(&out, "vault_shared_delete"), "allow");
        // Cross-user read stays deny even for an admin (system-only).
        assert_eq!(decision_for(&out, "crossuser_read"), "deny");
    }

    #[tokio::test]
    async fn whoami_reports_suspended_admin_as_non_admin() {
        use workforce_identity::types::IdentityStatus;
        let store = Arc::new(InMemoryStore::default());
        let identity = Identity::new("Suspended Admin", IdentityRole::SystemAdmin)
            .with_status(IdentityStatus::Suspended);
        let uid = identity.id;
        store.create(&identity).unwrap();
        let tool = make_tool(store as Arc<dyn IdentityStore>);
        let result = workforce_agent::runner::CURRENT_USER_ID
            .scope(uid.to_string(), async {
                tool.execute(json!({})).await.unwrap()
            })
            .await;
        assert!(result.success, "expected success, got {:?}", result.error);
        let out = result.output;
        // Role string is still factual, but admin capabilities are all denied.
        assert_eq!(out["role"], json!("system_admin"));
        assert_eq!(decision_for(&out, "agents_manage"), "deny");
        assert_eq!(decision_for(&out, "vault_shared_delete"), "deny");
        assert_eq!(decision_for(&out, "vault_shared_write"), "deny");
    }
}
