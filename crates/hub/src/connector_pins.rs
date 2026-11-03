//! Which vault tiers an agent's connectors read their keys from, on a hub
//! with `[desk]`: the agent's `[skills]` binding, read again as its
//! `config.toml` changes.
//!
//! - A connector the binding turns off resolves no key, on every path a
//!   loaded adapter reads its store from: a turn, a workflow `tool =` step,
//!   a subagent.
//! - One in `own_credential` ("its own connection") reads `AgentPerUser`,
//!   then `AgentSpecific`.
//! - Any other ("my connection") reads `GlobalPerUser`, then `Global`, so a
//!   key the person saved earlier at the bare tier keeps working.
//! - An agent with no `enabled` keeps every tier for every connector, as
//!   before.
//!
//! A connector's OAuth app (`skills/<id>/oauth_client/…`) is the hub's, not
//! a connection's, and is never pinned. Every hub without `[desk]` fills no
//! slot, so nothing here runs there.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::SystemTime;

use parking_lot::Mutex;
use workforce_common::connector_binding::{ConnectorBinding, CONNECTORS};
use workforce_common::credential_pin::{PinSource, TierPin};
use workforce_common::settings::{AgentWorkspaceConfig, SecretStore, VaultSource};

use crate::serve::AgentService;
use crate::vault::Vault;

/// The connector whose connection `path` (a logical vault path) is part of.
pub fn connector_of_path(path: &str) -> Option<&'static str> {
    if path == "slack" || path.starts_with("slack.") {
        return Some("slack");
    }
    let rest = path.strip_prefix("skills/")?;
    let (id, tail) = rest.split_once('/').unwrap_or((rest, ""));
    if tail.starts_with("oauth_client/") || tail == "oauth_client" {
        return None;
    }
    CONNECTORS.iter().copied().find(|c| *c == id)
}

/// The pin `binding` gives `path`.
pub fn pin_of(binding: &ConnectorBinding, path: &str) -> TierPin {
    let Some(connector) = connector_of_path(path) else {
        return TierPin::All;
    };
    if binding.enabled.is_none() {
        return TierPin::All;
    }
    match (binding.is_on(connector), binding.own(connector)) {
        (false, _) => TierPin::Nothing,
        (true, true) => TierPin::Own,
        (true, false) => TierPin::Mine,
    }
}

/// One agent's binding, read from its `config.toml` when it changes. One
/// that can't be read pins every connector off until it reads again.
pub struct AgentPins {
    workspace: PathBuf,
    read: Mutex<Option<(Option<SystemTime>, ConnectorBinding)>>,
}

impl AgentPins {
    pub fn new(workspace: PathBuf) -> Self {
        Self {
            workspace,
            read: Mutex::new(None),
        }
    }

    pub fn binding(&self) -> ConnectorBinding {
        let path = self
            .workspace
            .join(workforce_common::config_files::WORKSPACE_CONFIG);
        let at = std::fs::metadata(&path).and_then(|m| m.modified()).ok();
        let mut read = self.read.lock();
        if let Some((seen, binding)) = read.as_ref() {
            if *seen == at {
                return binding.clone();
            }
        }
        let binding = match AgentWorkspaceConfig::load(&self.workspace) {
            Ok(Some(config)) => config.connectors().unwrap_or_else(|_| ConnectorBinding {
                enabled: Some(Vec::new()),
                ..ConnectorBinding::default()
            }),
            Ok(None) => ConnectorBinding::default(),
            Err(_) => ConnectorBinding {
                enabled: Some(Vec::new()),
                ..ConnectorBinding::default()
            },
        };
        *read = Some((at, binding.clone()));
        binding
    }

    #[cfg(test)]
    pub(crate) fn forget(&self) {
        *self.read.lock() = None;
    }
}

impl PinSource for AgentPins {
    fn pin(&self, path: &str) -> TierPin {
        pin_of(&self.binding(), path)
    }
}

/// Where a connection's key is kept, to say which tier a call's key came
/// from.
fn primary(connector: &str) -> Option<&'static str> {
    Some(match connector {
        "github" => "skills/github/token",
        "google" => "skills/google/workspaces/default/refresh_token",
        "email" => "skills/email/accounts/default/password",
        "jira" => "skills/jira/api_token",
        "linear" => "skills/linear/api_key",
        "slack" => "slack.bot_token",
        "datadog" => "skills/datadog/api_key",
        "metabase" => "skills/metabase/api_key",
        _ => return None,
    })
}

struct Pinned {
    agent: String,
    pins: Arc<AgentPins>,
    vault: Option<Arc<Vault>>,
    global: Option<Arc<Vault>>,
}

static PINNED: std::sync::RwLock<Vec<Pinned>> = std::sync::RwLock::new(Vec::new());

/// Pins `service`'s connectors by its `[skills]`: fills its registry's slot,
/// which every adapter store it loaded or will load reads.
pub fn pin(service: &AgentService) {
    let slot = service.tools.credential_pins();
    if slot.is_filled() {
        return;
    }
    let pins = Arc::new(AgentPins::new(service.workspace.clone()));
    slot.fill(Arc::clone(&pins) as Arc<dyn PinSource>);
    let mut pinned = PINNED.write().unwrap_or_else(|e| e.into_inner());
    pinned.retain(|p| p.agent != service.agent_id.as_str());
    pinned.push(Pinned {
        agent: service.agent_id.as_str().to_string(),
        pins,
        vault: service.vault.clone(),
        global: service.global_vault.clone(),
    });
}

/// `agent`'s binding, if its connectors are pinned.
pub fn binding_of(agent: &str) -> Option<ConnectorBinding> {
    let pinned = PINNED.read().unwrap_or_else(|e| e.into_inner());
    pinned
        .iter()
        .find(|p| p.agent == agent)
        .map(|p| p.pins.binding())
}

/// The tier `agent`'s key for `connector` resolves from now, for the person
/// in scope, as its pin allows.
pub fn source_of(agent: &str, connector: &str) -> Option<VaultSource> {
    let path = primary(connector)?;
    let pinned = PINNED.read().unwrap_or_else(|e| e.into_inner());
    let p = pinned.iter().find(|p| p.agent == agent)?;
    let store =
        crate::user_config::LayeredUserVault::new(Arc::clone(p.vault.as_ref()?), p.global.clone())
            .with_pins({
                let slot = Arc::new(workforce_common::credential_pin::PinSlot::default());
                slot.fill(Arc::clone(&p.pins) as Arc<dyn PinSource>);
                slot
            });
    store.get_with_source(path).map(|(_, source)| source)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn binding(enabled: Option<&[&str]>, own: &[&str]) -> ConnectorBinding {
        ConnectorBinding {
            enabled: enabled.map(|e| e.iter().map(|s| s.to_string()).collect()),
            read_only: vec![],
            own_credential: own.iter().map(|s| s.to_string()).collect(),
        }
    }

    #[test]
    fn a_path_belongs_to_its_connector_and_an_oauth_app_to_none() {
        assert_eq!(connector_of_path("skills/linear/api_key"), Some("linear"));
        assert_eq!(
            connector_of_path("skills/github/workspaces/acme/token"),
            Some("github")
        );
        assert_eq!(connector_of_path("slack.bot_token"), Some("slack"));
        assert_eq!(connector_of_path("slack.acme.bot_token"), Some("slack"));
        assert_eq!(
            connector_of_path("skills/google/oauth_client/client_id"),
            None
        );
        assert_eq!(connector_of_path("skills/xero/client_id"), None);
        assert_eq!(connector_of_path("embeddings/key"), None);
        assert_eq!(connector_of_path("skills/linearx/api_key"), None);
    }

    #[test]
    fn the_binding_pins_each_connector() {
        let path = "skills/linear/api_key";
        assert_eq!(pin_of(&binding(None, &["linear"]), path), TierPin::All);
        assert_eq!(pin_of(&binding(Some(&[]), &[]), path), TierPin::Nothing);
        assert_eq!(
            pin_of(&binding(Some(&["linear"]), &[]), path),
            TierPin::Mine
        );
        assert_eq!(
            pin_of(&binding(Some(&["linear"]), &["linear"]), path),
            TierPin::Own
        );
        // Anything that isn't a connection is never pinned.
        assert_eq!(
            pin_of(&binding(Some(&[]), &[]), "skills/xero/client_id"),
            TierPin::All
        );
    }

    /// A really loaded adapter reads its key as the agent's binding pins it,
    /// once the hub pins the agent, on every path that runs the tool.
    #[tokio::test]
    async fn a_loaded_adapter_resolves_no_key_its_binding_rules_out() {
        use workforce_agent::runner::CURRENT_USER_ID;
        use workforce_agent::tools::registry::ToolRegistry;

        let dir = tempfile::tempdir().unwrap();
        let open = |name: &str| Arc::new(Vault::open(&dir.path().join(name), "test-key").unwrap());
        let (agent, global) = (open("agent"), open("global"));
        let workspace = dir.path().join("ws");
        std::fs::create_dir_all(&workspace).unwrap();
        let config_file = workspace.join(workforce_common::config_files::WORKSPACE_CONFIG);
        // Sarah's key for all her agents, and a shared one at the agent.
        global
            .set("users/sarah/skills/linear/api_key", "lin_api_sarah")
            .unwrap();
        let registry = ToolRegistry::new();
        CURRENT_USER_ID
            .sync_scope("sarah".to_string(), || {
                crate::skill_loader::load_skill_from_vault(
                    "linear",
                    Arc::clone(&agent),
                    Some(Arc::clone(&global)),
                    &workforce_common::settings::WorkforceConfig::default(),
                    &registry,
                )
            })
            .unwrap();
        // Pinned after loading: the stores the adapter already holds follow.
        let pins = Arc::new(AgentPins::new(workspace.clone()));
        registry
            .credential_pins()
            .fill(Arc::clone(&pins) as Arc<dyn PinSource>);
        let tool = registry.get("linear.issue_get").expect("linear loaded");
        // A subagent's or a workflow step's registry holds the same tool.
        let copied = registry.clone_all().get("linear.issue_get").unwrap();

        let unresolved = |body: &'static str| {
            std::fs::write(&config_file, body).unwrap();
            pins.forget();
            let (tool, copied) = (Arc::clone(&tool), Arc::clone(&copied));
            CURRENT_USER_ID.scope("sarah".to_string(), async move {
                for t in [tool, copied] {
                    let said = match t
                        .execute(serde_json::json!({ "identifier": "ENG-1" }))
                        .await
                    {
                        Ok(r) => format!("{:?} {:?}", r.error, r.output),
                        Err(e) => format!("{e:#}"),
                    };
                    assert!(said.contains("not configured"), "{body}: {said}");
                }
            })
        };
        // Off: nothing resolves.
        unresolved("[skills]\nenabled = []\n").await;
        // Its own connection: Sarah's key for all her agents isn't its own.
        unresolved("[skills]\nenabled = [\"linear\"]\nown_credential = [\"linear\"]\n").await;
        // My connection: a key only this agent has isn't Sarah's.
        global.delete("users/sarah/skills/linear/api_key").unwrap();
        agent
            .set("users/sarah/skills/linear/api_key", "lin_api_agent")
            .unwrap();
        unresolved("[skills]\nenabled = [\"linear\"]\n").await;
    }

    #[test]
    fn an_earlier_global_key_still_resolves_for_my_connection_and_each_tier_is_pinned() {
        use workforce_agent::runner::CURRENT_USER_ID;
        use workforce_common::credential_pin::PinSlot;

        let dir = tempfile::tempdir().unwrap();
        let open = |name: &str| Arc::new(Vault::open(&dir.path().join(name), "test-key").unwrap());
        let (agent, global) = (open("agent"), open("global"));
        let path = "skills/linear/api_key";
        let workspace = dir.path().join("ws");
        std::fs::create_dir_all(&workspace).unwrap();
        let file = workspace.join(workforce_common::config_files::WORKSPACE_CONFIG);
        let pins = Arc::new(AgentPins::new(workspace));
        let slot = Arc::new(PinSlot::default());
        slot.fill(Arc::clone(&pins) as Arc<dyn PinSource>);
        let store = crate::user_config::LayeredUserVault::new(
            Arc::clone(&agent),
            Some(Arc::clone(&global)),
        )
        .with_pins(slot);
        let source = |body: &str| {
            std::fs::write(&file, body).unwrap();
            pins.forget();
            CURRENT_USER_ID.sync_scope("sarah".to_string(), || {
                store.get_with_source(path).map(|(_, s)| s)
            })
        };
        let mine = "[skills]\nenabled = [\"linear\"]\n";
        let own = "[skills]\nenabled = [\"linear\"]\nown_credential = [\"linear\"]\n";

        // A key Sarah saved before Connections, at the bare global tier.
        global.set(path, "old").unwrap();
        assert_eq!(source(mine), Some(VaultSource::Global));
        assert_eq!(source(own), None);
        // Each of the four tiers, against each pin.
        agent.set(path, "agent-shared").unwrap();
        assert_eq!(source(own), Some(VaultSource::AgentSpecific));
        agent
            .set("users/sarah/skills/linear/api_key", "own")
            .unwrap();
        assert_eq!(source(own), Some(VaultSource::AgentPerUser));
        global
            .set("users/sarah/skills/linear/api_key", "mine")
            .unwrap();
        assert_eq!(source(mine), Some(VaultSource::GlobalPerUser));
        assert_eq!(source(own), Some(VaultSource::AgentPerUser));
        assert_eq!(source("[skills]\nenabled = []\n"), None);
        // No binding: every tier, most specific first, as before.
        assert_eq!(source(""), Some(VaultSource::AgentPerUser));
        // The person's own override is pinned too.
        std::fs::write(&file, mine).unwrap();
        pins.forget();
        let over =
            CURRENT_USER_ID.sync_scope("sarah".to_string(), || store.get_user_override(path));
        assert_eq!(over.as_deref(), Some("mine"));
    }
}
