//! Connecting an agent to Slack. One flow — verify the tokens, refuse a Slack
//! app another workspace already uses, store the tokens in the agent's vault,
//! bind the workspace to the agent, reload Slack on every replica — shared by
//! the `slack.connect` / `slack.disconnect` tools, the agent Slack REST API,
//! and the boot and live-spawn wiring, so an agent created in the UI gets the
//! same Slack wiring as one present at boot.

use std::collections::BTreeMap;
use std::path::Path;
use std::sync::Arc;

use futures::StreamExt;
use serde::{Deserialize, Serialize};
use tokio::sync::Mutex;
use toml_edit::{ArrayOfTables, DocumentMut, Item, Table};
use tracing::{info, warn};
use workforce_common::secret_lock::RefreshLock;
use workforce_common::settings::VaultActor;
use workforce_skills::adapters::slack::transport::SlackTransport;
use workforce_skills::adapters::slack::{BotIdentity, SlackSkillConfig, SlackWorkspaceConfig};

use crate::hosts::RegistryView;
use crate::serve::AgentService;
use crate::slack_manager::{ReloadResult, SlackManager, SlackTokenOverrides};
use crate::vault::{Vault, VaultOp};

/// Where the running [`SlackControl`] is published once Slack starts, for
/// the parts of the hub built before it (the gateway, the overlay watcher).
pub type SlackSlot = Arc<std::sync::OnceLock<Arc<SlackControl>>>;

/// Core NATS subject every replica listens on to reload Slack.
const RELOAD_SUBJECT: &str = "workforce.hub.slack_reload";
const RESUBSCRIBE_AFTER: std::time::Duration = std::time::Duration::from_secs(5);
const TOKEN_KEYS: [&str; 3] = ["bot_token", "app_token", "signing_secret"];
/// The fleet lock every change to the Slack config takes, so two replicas
/// connecting agents at once don't overwrite each other's workspaces.
const CONFIG_LOCK_KEY: &str = "slack-integration/config.toml";

/// The Slack integration as the rest of the hub drives it: the manager, plus
/// the replica fan-out every change goes through.
pub struct SlackControl {
    manager: Arc<Mutex<SlackManager>>,
    registry: Arc<dyn RegistryView>,
    nats: Option<async_nats::Client>,
    /// Serialises connect and disconnect across every replica sharing the
    /// hub vault (a Postgres advisory lock there), taken before the manager.
    config_lock: Arc<dyn RefreshLock>,
    /// This process, so a replica skips the reload notices it sent.
    origin: String,
}

/// The tokens of a Slack app made for one agent.
#[derive(Deserialize)]
pub struct ConnectRequest {
    pub bot_token: String,
    pub app_token: String,
    #[serde(default)]
    pub signing_secret: Option<String>,
    #[serde(default)]
    pub workspace_id: Option<String>,
}

/// An agent's workspace after a successful connect.
#[derive(Debug, Serialize)]
pub struct Connection {
    pub workspace_id: String,
    pub agent_id: String,
    pub bot_name: String,
    pub bot_user_id: String,
    pub team_id: Option<String>,
    /// Whether this replica holds the Socket Mode connection. Only the leader
    /// does; every replica posts.
    pub listening_here: bool,
    /// Other workspaces the reload left unconnected, with why.
    pub unconnected: BTreeMap<String, String>,
}

/// The workspaces a disconnect removed.
#[derive(Debug, Serialize)]
pub struct Disconnection {
    pub agent_id: String,
    pub workspaces: Vec<String>,
}

/// How one agent stands with Slack.
#[derive(Debug, Serialize)]
pub struct SlackStatus {
    pub agent_id: String,
    pub workspaces: Vec<WorkspaceStatus>,
}

#[derive(Debug, Serialize)]
pub struct WorkspaceStatus {
    pub workspace_id: String,
    pub connected: bool,
    pub bot_name: Option<String>,
    pub bot_user_id: Option<String>,
    /// Why the workspace is not connected.
    pub reason: Option<String>,
}

/// Why a connect or disconnect did nothing.
#[derive(Debug, thiserror::Error)]
pub enum SlackRefusal {
    #[error("{0}")]
    Invalid(String),
    #[error("{0}")]
    Conflict(String),
    #[error("Slack refused the tokens: {0}")]
    Rejected(String),
    #[error(transparent)]
    Failed(#[from] eyre::Report),
}

#[derive(Serialize, Deserialize)]
struct ReloadNotice {
    origin: String,
}

impl SlackControl {
    pub fn new(
        manager: Arc<Mutex<SlackManager>>,
        registry: Arc<dyn RegistryView>,
        nats: Option<async_nats::Client>,
        config_lock: Arc<dyn RefreshLock>,
    ) -> Arc<Self> {
        Arc::new(Self {
            manager,
            registry,
            nats,
            config_lock,
            origin: uuid::Uuid::new_v4().to_string(),
        })
    }

    pub fn manager(&self) -> &Arc<Mutex<SlackManager>> {
        &self.manager
    }

    /// Give `service` the Slack tools and the Slack state the current
    /// workspaces imply. Run before a live-spawned service enters the
    /// registry, so it never resolves half-wired.
    pub async fn wire_agent(self: &Arc<Self>, service: &AgentService) {
        self.register_tools(service);
        self.manager.lock().await.attach(service).await;
    }

    /// The Slack tools every agent gets, bound to `service`'s agent id.
    pub fn register_tools(self: &Arc<Self>, service: &AgentService) {
        for tool in crate::slack_tool::onboarding_tools(Arc::clone(self), service.agent_id.as_str())
        {
            service.register_tool(tool);
        }
    }

    /// An agent went live: reload this replica's Slack when the config wants
    /// the agent serving a workspace it isn't — bound before it existed, or
    /// with tokens only its own vault holds. Every replica's overlay watcher
    /// spawns the agent itself, so this stays local.
    pub async fn agent_joined(&self, agent_id: &str) {
        let mut manager = self.manager.lock().await;
        if !manager.is_stale_for(agent_id) {
            return;
        }
        info!(agent = %agent_id, "Slack config binds a workspace to a newly live agent — reloading Slack");
        if let Err(e) = manager.reload(None).await {
            warn!(agent = %agent_id, error = %e, "Slack reload for a newly live agent failed");
        }
    }

    /// An agent was removed: disconnect any workspace bound to it on this
    /// replica, so its app stops answering instead of falling to the default
    /// agent.
    pub async fn agent_left(&self, agent_id: &str) {
        let mut manager = self.manager.lock().await;
        if !manager.binds(agent_id) {
            return;
        }
        info!(agent = %agent_id, "Agent with a Slack workspace removed — reloading Slack");
        if let Err(e) = manager.reload(None).await {
            warn!(agent = %agent_id, error = %e, "Slack reload after an agent was removed failed");
        }
    }

    /// Reload Slack here, persisting `overrides` first, then on every other
    /// replica.
    pub async fn reload(
        &self,
        overrides: Option<SlackTokenOverrides>,
    ) -> eyre::Result<ReloadResult> {
        let reloaded = self.manager.lock().await.reload(overrides).await;
        self.announce_reload().await;
        reloaded
    }

    /// Verify `request`'s tokens, store them in `agent_id`'s vault, bind the
    /// workspace to the agent and reload Slack everywhere. Calling it again
    /// with new tokens rotates them.
    pub async fn connect(
        &self,
        agent_id: &str,
        request: ConnectRequest,
        actor: VaultActor<'_>,
    ) -> Result<Connection, SlackRefusal> {
        let service = self.live_agent(agent_id)?;
        check_token("bot_token", &request.bot_token, "xoxb-")?;
        check_token("app_token", &request.app_token, "xapp-")?;

        let config_lock = self.lock_config().await?;
        let mut manager = self.manager.lock().await;
        manager.sync_vaults();
        let config = current_config(&manager)?;
        let workspace_id = choose_workspace(request.workspace_id.as_deref(), agent_id, &config)?;
        let identity = SlackTransport::with_base_url(
            candidate(&workspace_id, &request),
            manager.api_base_url(),
        )
        .verify_credentials()
        .await
        .map_err(|e| SlackRefusal::Rejected(e.to_string()))?;
        if let Some(other) = conflicting_workspace(
            &workspace_id,
            &identity,
            &request,
            &config,
            &manager.bot_identities(),
        ) {
            let owner = config.agent_bindings.get(other).map_or_else(
                || "every unbound agent".to_string(),
                |a| format!("agent '{a}'"),
            );
            return Err(SlackRefusal::Conflict(format!(
                "these tokens belong to the Slack app already connected as workspace '{other}' \
                 ({owner}) — create a separate Slack app for agent '{agent_id}' \
                 (slack.app_scaffold generates its manifest)"
            )));
        }

        let vault = agent_vault(&service, &manager)?;
        let writes: Vec<(String, &str)> = [
            ("bot_token", Some(request.bot_token.as_str())),
            ("app_token", Some(request.app_token.as_str())),
            ("signing_secret", request.signing_secret.as_deref()),
        ]
        .into_iter()
        .filter_map(|(key, value)| Some((token_path(&workspace_id, key), value?)))
        .collect();
        let ops: Vec<VaultOp<'_>> = writes
            .iter()
            .map(|(path, value)| VaultOp::Set { path, value })
            .collect();
        vault.apply(&ops, actor)?;

        let file = manager.config_file();
        let mut document = read_document(&file)?;
        bind_workspace(&mut document, &workspace_id, agent_id)?;
        write_document(&file, &document)?;
        drop(config_lock);
        let reloaded = manager.reload(None).await;

        let connected = manager.bot_identities().remove(&workspace_id);
        let listening_here = manager.is_running();
        let mut unconnected: BTreeMap<String, String> = manager
            .skipped()
            .iter()
            .map(|(ws, why)| (ws.clone(), why.to_string()))
            .collect();
        let reason = unconnected.remove(&workspace_id);
        drop(manager);
        self.announce_reload().await;
        reloaded?;

        let identity = connected.ok_or_else(|| {
            SlackRefusal::Failed(eyre::eyre!(
                "tokens stored, but workspace '{workspace_id}' did not connect: {}",
                reason.as_deref().unwrap_or("unknown reason")
            ))
        })?;
        info!(agent = %agent_id, workspace = %workspace_id, bot_id = %identity.user_id, "Agent connected to Slack");
        Ok(Connection {
            workspace_id,
            agent_id: agent_id.to_string(),
            bot_name: identity.display_name,
            bot_user_id: identity.user_id,
            team_id: identity.team_id,
            listening_here,
            unconnected,
        })
    }

    /// Remove `agent_id`'s Slack workspaces (or just `workspace_id`): their
    /// tokens from the agent's and the hub vault, their `[[workspaces]]`
    /// entries and bindings, and their connections on every replica.
    pub async fn disconnect(
        &self,
        agent_id: &str,
        workspace_id: Option<&str>,
        actor: VaultActor<'_>,
    ) -> Result<Disconnection, SlackRefusal> {
        let service = self.live_agent(agent_id)?;
        let config_lock = self.lock_config().await?;
        let mut manager = self.manager.lock().await;
        manager.sync_vaults();
        let config = current_config(&manager)?;
        let workspaces: Vec<String> = bound_workspaces(&config, agent_id)
            .into_iter()
            .filter(|ws| workspace_id.is_none_or(|wanted| wanted == ws))
            .collect();
        if workspaces.is_empty() {
            return Err(SlackRefusal::Invalid(match workspace_id {
                Some(ws) => format!("workspace '{ws}' is not bound to agent '{agent_id}'"),
                None => format!("agent '{agent_id}' is not connected to Slack"),
            }));
        }

        let paths: Vec<String> = workspaces
            .iter()
            .flat_map(|ws| TOKEN_KEYS.map(|key| token_path(ws, key)))
            .collect();
        for vault in crate::hub_core::logins::store::distinct_vaults(
            service.vault.iter().chain(manager.global_vault()).cloned(),
        ) {
            let ops: Vec<VaultOp<'_>> = paths
                .iter()
                .filter(|path| vault.get(path).is_some())
                .map(|path| VaultOp::Delete { path })
                .collect();
            if !ops.is_empty() {
                vault.apply(&ops, actor)?;
            }
        }

        let file = manager.config_file();
        let mut document = read_document(&file)?;
        for ws in &workspaces {
            unbind_workspace(&mut document, ws);
        }
        write_document(&file, &document)?;
        drop(config_lock);
        let reloaded = manager.reload(None).await;
        drop(manager);
        self.announce_reload().await;
        reloaded?;

        info!(agent = %agent_id, workspaces = ?workspaces, "Agent disconnected from Slack");
        Ok(Disconnection {
            agent_id: agent_id.to_string(),
            workspaces,
        })
    }

    /// The workspaces bound to `agent_id` and whether each is connected.
    pub async fn status(&self, agent_id: &str) -> Result<SlackStatus, SlackRefusal> {
        let manager = self.manager.lock().await;
        let config = current_config(&manager)?;
        let identities = manager.bot_identities();
        let workspaces = bound_workspaces(&config, agent_id)
            .into_iter()
            .map(|workspace_id| {
                let identity = identities.get(&workspace_id);
                WorkspaceStatus {
                    connected: identity.is_some(),
                    bot_name: identity.map(|i| i.display_name.clone()),
                    bot_user_id: identity.map(|i| i.user_id.clone()),
                    reason: identity
                        .is_none()
                        .then(|| {
                            manager
                                .skipped()
                                .get(&workspace_id)
                                .map(ToString::to_string)
                        })
                        .flatten(),
                    workspace_id,
                }
            })
            .collect();
        Ok(SlackStatus {
            agent_id: agent_id.to_string(),
            workspaces,
        })
    }

    /// Reload Slack whenever another replica announces a change, for as long
    /// as the hub runs. Without NATS there is no other replica.
    pub fn spawn_reload_listener(self: &Arc<Self>) -> Option<tokio::task::JoinHandle<()>> {
        let nats = self.nats.clone()?;
        let control = Arc::clone(self);
        Some(tokio::spawn(async move {
            loop {
                match nats.subscribe(RELOAD_SUBJECT).await {
                    Ok(mut notices) => {
                        while let Some(notice) = notices.next().await {
                            match serde_json::from_slice::<ReloadNotice>(&notice.payload) {
                                Ok(notice) if notice.origin == control.origin => {}
                                Ok(_) => match control.manager.lock().await.reload(None).await {
                                    Ok(result) => info!(
                                        workspaces = result.workspaces,
                                        "Slack reloaded on another replica's change"
                                    ),
                                    Err(e) => {
                                        warn!(error = %e, "Slack reload on another replica's change failed")
                                    }
                                },
                                Err(e) => warn!(error = %e, "an unreadable Slack reload notice"),
                            }
                        }
                        warn!("Slack reload notices ended; subscribing again");
                    }
                    Err(e) => warn!(error = %e, "subscribing to Slack reload notices"),
                }
                tokio::time::sleep(RESUBSCRIBE_AFTER).await;
            }
        }))
    }

    async fn announce_reload(&self) {
        let Some(nats) = &self.nats else {
            return;
        };
        let notice = ReloadNotice {
            origin: self.origin.clone(),
        };
        let payload = serde_json::to_vec(&notice).expect("a string serialises");
        if let Err(e) = nats.publish(RELOAD_SUBJECT, payload.into()).await {
            warn!(error = %e, "announcing a Slack reload to the other replicas failed");
        }
    }

    async fn lock_config(
        &self,
    ) -> Result<workforce_common::secret_lock::RefreshGuard, SlackRefusal> {
        Ok(self.config_lock.lock(CONFIG_LOCK_KEY).await?)
    }

    fn live_agent(&self, agent_id: &str) -> Result<Arc<AgentService>, SlackRefusal> {
        self.registry
            .get(agent_id)
            .ok_or_else(|| SlackRefusal::Invalid(format!("agent '{agent_id}' is not running")))
    }
}

fn check_token(field: &str, token: &str, prefix: &str) -> Result<(), SlackRefusal> {
    if token.starts_with(prefix) && token.len() > prefix.len() {
        Ok(())
    } else {
        Err(SlackRefusal::Invalid(format!(
            "{field} must be a Slack {prefix}… token"
        )))
    }
}

fn token_path(workspace_id: &str, key: &str) -> String {
    format!("slack.{workspace_id}.{key}")
}

fn candidate(workspace_id: &str, request: &ConnectRequest) -> SlackWorkspaceConfig {
    SlackWorkspaceConfig {
        id: workspace_id.to_string(),
        bot_token: request.bot_token.clone(),
        app_token: request.app_token.clone(),
        channels: Vec::new(),
        response_mode: None,
        thread_isolation: None,
        activation_keywords: None,
        paragraph_spacing: None,
        bot_user_id: None,
    }
}

/// The agent's own vault, or the hub's when the agent has none — the two
/// places `SlackManager` reads a bound workspace's tokens from.
fn agent_vault(service: &AgentService, manager: &SlackManager) -> Result<Arc<Vault>, SlackRefusal> {
    service
        .vault
        .clone()
        .or_else(|| manager.global_vault().cloned())
        .ok_or_else(|| {
            SlackRefusal::Failed(eyre::eyre!("no vault is configured to hold the tokens"))
        })
}

/// The Slack config as the manager reads it; none yet is no workspaces.
fn current_config(manager: &SlackManager) -> Result<SlackSkillConfig, SlackRefusal> {
    if manager.config_file().exists() {
        return Ok(manager.load_config()?);
    }
    Ok(serde_json::from_value(serde_json::json!({})).expect("an empty Slack config deserialises"))
}

/// The workspace a connect binds: the one asked for, else the one the agent
/// already has, else a new one named after the agent.
fn choose_workspace(
    requested: Option<&str>,
    agent_id: &str,
    config: &SlackSkillConfig,
) -> Result<String, SlackRefusal> {
    let bound_elsewhere = |ws: &str| {
        config
            .agent_bindings
            .get(ws)
            .filter(|bound| *bound != agent_id)
            .map(|bound| {
                SlackRefusal::Conflict(format!("workspace '{ws}' is bound to agent '{bound}'"))
            })
    };
    if let Some(ws) = requested {
        if ws.is_empty()
            || ws.len() > 64
            || !ws
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
        {
            return Err(SlackRefusal::Invalid(
                "workspace_id must be 1–64 letters, digits, '-' or '_'".to_string(),
            ));
        }
        return bound_elsewhere(ws).map_or_else(|| Ok(ws.to_string()), Err);
    }
    match bound_workspaces(config, agent_id).as_slice() {
        [only] => Ok(only.clone()),
        [] => {
            let ws = agent_id.to_lowercase();
            if let Some(refusal) = bound_elsewhere(&ws) {
                return Err(refusal);
            }
            if config.workspaces.iter().any(|w| w.id == ws) {
                return Err(SlackRefusal::Conflict(format!(
                    "workspace '{ws}' already exists and serves every agent — pass workspace_id \
                     '{ws}' to bind it to agent '{agent_id}', or another id for a new workspace"
                )));
            }
            Ok(ws)
        }
        several => Err(SlackRefusal::Invalid(format!(
            "agent '{agent_id}' has several Slack workspaces ({}) — pass workspace_id",
            several.join(", ")
        ))),
    }
}

/// The workspace other than `target` already served by the same Slack app:
/// the same bot user, or the same bot or app token. `auth.test` can't name
/// the app behind an app-level token, so a reused one is caught by value.
fn conflicting_workspace<'a>(
    target: &str,
    identity: &BotIdentity,
    request: &ConnectRequest,
    config: &'a SlackSkillConfig,
    identities: &'a std::collections::HashMap<String, BotIdentity>,
) -> Option<&'a str> {
    identities
        .iter()
        .find(|(ws, other)| ws.as_str() != target && other.user_id == identity.user_id)
        .map(|(ws, _)| ws.as_str())
        .or_else(|| {
            config
                .workspaces
                .iter()
                .filter(|ws| ws.id != target)
                .find(|ws| {
                    (!ws.bot_token.is_empty() && ws.bot_token == request.bot_token)
                        || (!ws.app_token.is_empty() && ws.app_token == request.app_token)
                })
                .map(|ws| ws.id.as_str())
        })
}

/// The configured workspaces bound to `agent_id`, sorted.
fn bound_workspaces(config: &SlackSkillConfig, agent_id: &str) -> Vec<String> {
    let mut bound: Vec<String> = config
        .workspaces
        .iter()
        .filter(|ws| config.agent_bindings.get(&ws.id).map(String::as_str) == Some(agent_id))
        .map(|ws| ws.id.clone())
        .collect();
    bound.sort();
    bound
}

fn read_document(file: &Path) -> eyre::Result<DocumentMut> {
    match std::fs::read_to_string(file) {
        Ok(text) => Ok(text.parse()?),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(DocumentMut::new()),
        Err(e) => Err(e.into()),
    }
}

/// Replace `file` with `document` in one rename, so a reader never sees half
/// a config.
fn write_document(file: &Path, document: &DocumentMut) -> eyre::Result<()> {
    let dir = file
        .parent()
        .ok_or_else(|| eyre::eyre!("{} has no parent directory", file.display()))?;
    std::fs::create_dir_all(dir)?;
    let staged = dir.join(format!(".config.toml.{}", uuid::Uuid::new_v4()));
    std::fs::write(&staged, document.to_string())?;
    std::fs::rename(&staged, file).inspect_err(|_| {
        let _ = std::fs::remove_file(&staged);
    })?;
    Ok(())
}

/// Add a `[[workspaces]]` entry for `workspace_id` unless one exists, and bind
/// it to `agent_id`, leaving everything else — comments included — as it was.
fn bind_workspace(
    document: &mut DocumentMut,
    workspace_id: &str,
    agent_id: &str,
) -> eyre::Result<()> {
    if document
        .get("workspaces")
        .and_then(Item::as_array)
        .is_some_and(toml_edit::Array::is_empty)
    {
        document.insert("workspaces", Item::ArrayOfTables(ArrayOfTables::new()));
    }
    let workspaces = document
        .entry("workspaces")
        .or_insert_with(|| Item::ArrayOfTables(ArrayOfTables::new()))
        .as_array_of_tables_mut()
        .ok_or_else(|| {
            eyre::eyre!("`workspaces` in the Slack config is not a [[workspaces]] list")
        })?;
    if !workspaces
        .iter()
        .any(|ws| ws.get("id").and_then(Item::as_str) == Some(workspace_id))
    {
        // Empty token keys (read from the vault) keep the entry loadable by a
        // hub that predates optional tokens, as during a rolling deploy.
        let mut entry = Table::new();
        entry.insert("id", toml_edit::value(workspace_id));
        entry.insert("bot_token", toml_edit::value(""));
        entry.insert("app_token", toml_edit::value(""));
        workspaces.push(entry);
    }
    document
        .entry("agent_bindings")
        .or_insert_with(toml_edit::table)
        .as_table_like_mut()
        .ok_or_else(|| eyre::eyre!("`agent_bindings` in the Slack config is not a table"))?
        .insert(workspace_id, toml_edit::value(agent_id));
    Ok(())
}

/// Drop `workspace_id`'s `[[workspaces]]` entry and binding. The last one
/// leaves `workspaces = []`, which a hub that predates an optional
/// `workspaces` still loads.
fn unbind_workspace(document: &mut DocumentMut, workspace_id: &str) {
    if let Some(workspaces) = document
        .get_mut("workspaces")
        .and_then(Item::as_array_of_tables_mut)
    {
        workspaces.retain(|ws| ws.get("id").and_then(Item::as_str) != Some(workspace_id));
        if workspaces.is_empty() {
            document.insert("workspaces", toml_edit::value(toml_edit::Array::new()));
        }
    }
    if let Some(bindings) = document
        .get_mut("agent_bindings")
        .and_then(Item::as_table_like_mut)
    {
        bindings.remove(workspace_id);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent_registry::AgentRegistry;
    use crate::serve::Compose as _;
    use std::collections::HashMap;
    use workforce_common::locking::LockManager;
    use workforce_common::settings::{AgentConfig, WorkforceConfig};

    const ACTOR: VaultActor<'static> = VaultActor::System("test");

    fn config(toml: &str) -> SlackSkillConfig {
        toml::from_str(toml).unwrap()
    }

    fn request(bot: &str, app: &str) -> ConnectRequest {
        ConnectRequest {
            bot_token: bot.to_string(),
            app_token: app.to_string(),
            signing_secret: None,
            workspace_id: None,
        }
    }

    fn identity(user_id: &str) -> BotIdentity {
        BotIdentity {
            user_id: user_id.to_string(),
            display_name: user_id.to_lowercase(),
            bot_id: None,
            team_id: None,
        }
    }

    #[test]
    fn a_connect_reuses_the_workspace_already_bound_to_the_agent() {
        let prod = config(
            "[[workspaces]]\nid = \"allan\"\n[[workspaces]]\nid = \"Minny\"\n\
             [agent_bindings]\nallan = \"allan\"\nMinny = \"Minny\"\n",
        );
        assert_eq!(choose_workspace(None, "Minny", &prod).unwrap(), "Minny");
    }

    #[test]
    fn a_first_connect_names_the_workspace_after_the_agent() {
        let empty = config("workspaces = []\n");
        assert_eq!(choose_workspace(None, "Minny", &empty).unwrap(), "minny");
        assert_eq!(
            choose_workspace(Some("team-a"), "Minny", &empty).unwrap(),
            "team-a"
        );
    }

    #[test]
    fn a_connect_never_takes_another_agent_s_workspace() {
        let taken = config("[[workspaces]]\nid = \"minny\"\n[agent_bindings]\nminny = \"allan\"\n");
        for requested in [None, Some("minny")] {
            assert!(matches!(
                choose_workspace(requested, "Minny", &taken),
                Err(SlackRefusal::Conflict(_))
            ));
        }
        let unbound = config("[[workspaces]]\nid = \"minny\"\n");
        assert!(matches!(
            choose_workspace(None, "Minny", &unbound),
            Err(SlackRefusal::Conflict(_))
        ));
        assert_eq!(
            choose_workspace(Some("minny"), "Minny", &unbound).unwrap(),
            "minny"
        );
        assert!(matches!(
            choose_workspace(Some("bad id"), "Minny", &unbound),
            Err(SlackRefusal::Invalid(_))
        ));
    }

    #[test]
    fn the_same_slack_app_is_refused_for_a_second_workspace() {
        let existing = config(
            "[[workspaces]]\nid = \"allan\"\nbot_token = \"xoxb-allan\"\napp_token = \"xapp-allan\"\n\
             [[workspaces]]\nid = \"minny\"\nbot_token = \"xoxb-minny\"\napp_token = \"xapp-minny\"\n",
        );
        let connected: HashMap<String, BotIdentity> =
            [("allan".to_string(), identity("U_ALLAN"))].into();
        let conflict = |ws: &str, user: &str, bot: &str, app: &str| {
            conflicting_workspace(
                ws,
                &identity(user),
                &request(bot, app),
                &existing,
                &connected,
            )
            .map(str::to_string)
        };
        assert_eq!(
            conflict("minny", "U_ALLAN", "xoxb-new", "xapp-new").as_deref(),
            Some("allan")
        );
        assert_eq!(
            conflict("minny", "U_NEW", "xoxb-new", "xapp-allan").as_deref(),
            Some("allan")
        );
        assert_eq!(
            conflict("minny", "U_MINNY", "xoxb-minny", "xapp-minny"),
            None
        );
        assert_eq!(
            conflict("allan", "U_ALLAN", "xoxb-allan", "xapp-allan"),
            None
        );
    }

    #[test]
    fn binding_a_workspace_keeps_the_rest_of_the_config_and_is_idempotent() {
        let original = "# Slack Integration Configuration\nresponse_mode = \"mention_only\"\n\n\
                        [[workspaces]]\nid = \"allan\"\nbot_token = \"${SLACK_BOT_TOKEN_ALLAN}\"\n\n\
                        [agent_bindings]\nallan = \"allan\"\nminny = \"minny\"\n";
        let mut document: DocumentMut = original.parse().unwrap();
        bind_workspace(&mut document, "minny", "Minny").unwrap();
        let once = document.to_string();
        bind_workspace(&mut document, "minny", "Minny").unwrap();
        assert_eq!(document.to_string(), once);

        assert!(once.starts_with("# Slack Integration Configuration\n"));
        let parsed = config(&once);
        assert_eq!(
            parsed
                .workspaces
                .iter()
                .map(|w| w.id.as_str())
                .collect::<Vec<_>>(),
            vec!["allan", "minny"]
        );
        assert_eq!(parsed.agent_bindings["minny"], "Minny");
        assert_eq!(parsed.agent_bindings["allan"], "allan");

        unbind_workspace(&mut document, "minny");
        let parsed = config(&document.to_string());
        assert_eq!(
            parsed
                .workspaces
                .iter()
                .map(|w| w.id.as_str())
                .collect::<Vec<_>>(),
            vec!["allan"]
        );
        assert!(!parsed.agent_bindings.contains_key("minny"));
    }

    #[test]
    fn binding_into_a_missing_config_creates_one() {
        let mut document = DocumentMut::new();
        bind_workspace(&mut document, "minny", "Minny").unwrap();
        let parsed = config(&document.to_string());
        assert_eq!(parsed.workspaces[0].id, "minny");
        assert_eq!(parsed.agent_bindings["minny"], "Minny");
    }

    /// The Slack config as a hub that predates optional tokens and
    /// workspaces reads it: every field required.
    #[derive(Deserialize)]
    struct StrictConfig {
        workspaces: Vec<StrictWorkspace>,
    }

    #[derive(Deserialize)]
    struct StrictWorkspace {
        #[allow(dead_code)]
        id: String,
        #[allow(dead_code)]
        bot_token: String,
        #[allow(dead_code)]
        app_token: String,
    }

    #[test]
    fn what_connect_and_disconnect_write_loads_on_a_hub_from_before_them() {
        let mut document: DocumentMut = "[agent_bindings]\nallan = \"allan\"\n".parse().unwrap();
        bind_workspace(&mut document, "minny", "Minny").unwrap();
        let bound = document.to_string();
        assert_eq!(
            toml::from_str::<StrictConfig>(&bound)
                .unwrap()
                .workspaces
                .len(),
            1,
            "{bound}"
        );

        unbind_workspace(&mut document, "minny");
        let emptied = document.to_string();
        assert!(
            toml::from_str::<StrictConfig>(&emptied)
                .unwrap()
                .workspaces
                .is_empty(),
            "{emptied}"
        );
        assert_eq!(config(&emptied).agent_bindings["allan"], "allan");

        bind_workspace(&mut document, "bob", "Bob").unwrap();
        let rebound = config(&document.to_string());
        assert_eq!(rebound.workspaces[0].id, "bob");
        assert_eq!(rebound.agent_bindings["bob"], "Bob");
    }

    #[test]
    fn a_reload_notice_names_the_replica_that_sent_it() {
        let notice = ReloadNotice {
            origin: "replica-a".to_string(),
        };
        let wire = serde_json::to_vec(&notice).unwrap();
        let relayed: ReloadNotice = serde_json::from_slice(&wire).unwrap();
        assert_eq!(relayed.origin, "replica-a");
    }

    /// A hub with live agents, a Slack manager on a temp skills dir, and a
    /// mock Slack whose bot tokens map to bot users by name.
    struct Hub {
        dir: tempfile::TempDir,
        slack: wiremock::MockServer,
        config: WorkforceConfig,
        registry: Arc<AgentRegistry>,
        global: Arc<Vault>,
        config_lock: Arc<dyn RefreshLock>,
        control: Arc<SlackControl>,
    }

    impl Hub {
        async fn new(live: &[&str]) -> Self {
            use wiremock::matchers::{method, path};
            use wiremock::{Mock, MockServer, Request, ResponseTemplate};

            let slack = MockServer::start().await;
            Mock::given(method("POST"))
                .and(path("/auth.test"))
                .respond_with(|req: &Request| {
                    let bearer = req
                        .headers
                        .get("authorization")
                        .and_then(|v| v.to_str().ok())
                        .unwrap_or_default();
                    match bearer.strip_prefix("Bearer xoxb-") {
                        Some(name) if !name.starts_with("revoked") => {
                            let bot = name.trim_end_matches(char::is_numeric);
                            ResponseTemplate::new(200).set_body_json(serde_json::json!({
                                "ok": true,
                                "user_id": format!("U_{}", bot.to_uppercase()),
                                "user": bot,
                                "team_id": "T1",
                            }))
                        }
                        _ => ResponseTemplate::new(200).set_body_json(
                            serde_json::json!({ "ok": false, "error": "invalid_auth" }),
                        ),
                    }
                })
                .mount(&slack)
                .await;
            Mock::given(method("POST"))
                .and(path("/apps.connections.open"))
                .respond_with(
                    ResponseTemplate::new(200)
                        .set_body_json(serde_json::json!({ "ok": true, "url": "wss://mock" })),
                )
                .mount(&slack)
                .await;

            let dir = tempfile::tempdir().unwrap();
            let mut config = WorkforceConfig::default();
            config.hub.data_dir = dir.path().to_string_lossy().to_string();
            config.memory.db_path = std::path::Path::new(&config.hub.data_dir)
                .join("memory.db")
                .to_string_lossy()
                .into_owned();
            config.llm.api_key = Some("test-key".into());
            let mut agents = HashMap::new();
            for id in live {
                agents.insert(id.to_string(), agent(&config, id).await);
            }
            let registry = Arc::new(AgentRegistry::from_parts(agents, live[0].to_string()));
            let global =
                Arc::new(Vault::open(&dir.path().join("global.enc"), "test-key-123").unwrap());
            let config_lock: Arc<dyn RefreshLock> =
                Arc::new(workforce_common::secret_lock::LocalRefreshLock::default());
            let control = replica(&dir, &slack, &registry, &global, &config_lock);
            for svc in registry.all_services() {
                control.register_tools(&svc);
            }
            Self {
                dir,
                slack,
                config,
                registry,
                global,
                config_lock,
                control,
            }
        }

        /// Another hub replica: its own Slack manager on the same skills
        /// directory, vaults and fleet lock.
        fn replica(&self) -> Arc<SlackControl> {
            replica(
                &self.dir,
                &self.slack,
                &self.registry,
                &self.global,
                &self.config_lock,
            )
        }

        fn agent(&self, id: &str) -> Arc<AgentService> {
            self.registry.get(id).unwrap()
        }

        async fn slack_workspaces(&self, id: &str) -> Vec<String> {
            self.agent(id)
                .surface_registry
                .read()
                .await
                .workspaces_for("slack")
        }

        async fn config_file(&self) -> SlackSkillConfig {
            let file = self.control.manager().lock().await.config_file();
            config(&std::fs::read_to_string(file).unwrap())
        }
    }

    fn replica(
        dir: &tempfile::TempDir,
        slack: &wiremock::MockServer,
        registry: &Arc<AgentRegistry>,
        global: &Arc<Vault>,
        config_lock: &Arc<dyn RefreshLock>,
    ) -> Arc<SlackControl> {
        let mut manager = SlackManager::new(
            Arc::clone(registry),
            dir.path().join("skills"),
            Some(Arc::clone(global)),
            None,
            Default::default(),
            None,
            tokio_util::sync::CancellationToken::new(),
            crate::access_gate::AccessGate::new(HashMap::new()),
            Default::default(),
            Arc::new(crate::channel_coordinator::ChannelCoordinator::new(
                Vec::new(),
            )),
            None,
        );
        manager.set_api_base_url(slack.uri());
        SlackControl::new(
            Arc::new(Mutex::new(manager)),
            registry.clone(),
            None,
            Arc::clone(config_lock),
        )
    }

    async fn agent(config: &WorkforceConfig, id: &str) -> Arc<AgentService> {
        let entry = AgentConfig {
            id: id.to_string(),
            ..Default::default()
        };
        let mut config = config.clone();
        config.agents = vec![entry.clone()];
        AgentService::build_for_agent(
            config.clone(),
            None,
            Some(&entry),
            None,
            Arc::new(LockManager::new()),
            None,
            None,
            None,
            None,
            crate::subscription::Subscriptions::new(&config),
        )
        .await
        .unwrap()
    }

    fn vault_of(service: &AgentService) -> &Arc<Vault> {
        service.vault.as_ref().expect("agent vault")
    }

    #[tokio::test]
    async fn an_agent_connects_rotates_its_tokens_and_disconnects() {
        let hub = Hub::new(&["allan", "Minny"]).await;

        let connected = hub
            .control
            .connect("Minny", request("xoxb-minny", "xapp-minny"), ACTOR)
            .await
            .unwrap();
        assert_eq!(
            (
                connected.workspace_id.as_str(),
                connected.bot_user_id.as_str()
            ),
            ("minny", "U_MINNY")
        );
        let minny = hub.agent("Minny");
        assert_eq!(
            vault_of(&minny).get("slack.minny.bot_token").as_deref(),
            Some("xoxb-minny")
        );
        assert_eq!(hub.config_file().await.agent_bindings["minny"], "Minny");
        assert_eq!(hub.slack_workspaces("Minny").await, vec!["minny"]);
        assert!(minny.tools.has("slack.list_channels"));
        assert_eq!(
            minny.slack_bot_identities.read().await["minny"].user_id,
            "U_MINNY"
        );
        assert!(hub.slack_workspaces("allan").await.is_empty());
        assert!(!hub.agent("allan").tools.has("slack.list_channels"));

        let refused = hub
            .control
            .connect("allan", request("xoxb-minny", "xapp-other"), ACTOR)
            .await;
        assert!(
            matches!(refused, Err(SlackRefusal::Conflict(_))),
            "{refused:?}"
        );
        let refused = hub
            .control
            .connect("allan", request("xoxb-revoked", "xapp-allan"), ACTOR)
            .await;
        assert!(
            matches!(refused, Err(SlackRefusal::Rejected(_))),
            "{refused:?}"
        );
        assert!(!hub.config_file().await.agent_bindings.contains_key("allan"));

        let rotated = hub
            .control
            .connect("Minny", request("xoxb-minny2", "xapp-minny2"), ACTOR)
            .await
            .unwrap();
        assert_eq!(rotated.workspace_id, "minny");
        assert_eq!(
            vault_of(&minny).get("slack.minny.app_token").as_deref(),
            Some("xapp-minny2")
        );
        assert_eq!(hub.config_file().await.workspaces.len(), 1);

        let gone = hub.control.disconnect("Minny", None, ACTOR).await.unwrap();
        assert_eq!(gone.workspaces, vec!["minny"]);
        assert!(vault_of(&minny).list(Some("slack.minny.")).is_empty());
        assert!(hub.config_file().await.workspaces.is_empty());
        assert!(hub.slack_workspaces("Minny").await.is_empty());
        assert!(!minny.tools.has("slack.list_channels"));
        assert!(minny.slack_bot_identities.read().await.is_empty());
        assert!(matches!(
            hub.control.disconnect("Minny", None, ACTOR).await,
            Err(SlackRefusal::Invalid(_))
        ));
    }

    #[tokio::test]
    async fn two_replicas_connecting_at_once_keep_both_workspaces() {
        let hub = Hub::new(&["allan", "Minny", "Bob", "Sam"]).await;
        let other = hub.replica();

        let (minny, bob) = tokio::join!(
            hub.control
                .connect("Minny", request("xoxb-minny", "xapp-minny"), ACTOR),
            other.connect("Bob", request("xoxb-bob", "xapp-bob"), ACTOR),
        );
        minny.unwrap();
        bob.unwrap();
        let bindings = hub.config_file().await.agent_bindings;
        assert_eq!(
            (bindings["minny"].as_str(), bindings["bob"].as_str()),
            ("Minny", "Bob")
        );

        let shared = |bot: &str, app: &str| ConnectRequest {
            workspace_id: Some("shared".to_string()),
            ..request(bot, app)
        };
        let (allan, sam) = tokio::join!(
            hub.control
                .connect("allan", shared("xoxb-allan", "xapp-allan"), ACTOR),
            other.connect("Sam", shared("xoxb-sam", "xapp-sam"), ACTOR),
        );
        let outcomes = [allan, sam];
        assert_eq!(
            outcomes.iter().filter(|o| o.is_ok()).count(),
            1,
            "{outcomes:?}"
        );
        assert!(outcomes
            .iter()
            .any(|o| matches!(o, Err(SlackRefusal::Conflict(_)))));
        let config = hub.config_file().await;
        assert_eq!(config.workspaces.len(), 3);
        assert!(["allan", "Sam"].contains(&config.agent_bindings["shared"].as_str()));
    }

    #[tokio::test]
    async fn the_old_tool_name_connects_and_explains_a_call_in_its_old_shape() {
        let hub = Hub::new(&["Minny"]).await;
        let tool = hub.agent("Minny").tools.get("slack.app_configure").unwrap();

        let old_shape = tool
            .execute(serde_json::json!({ "bot_token": "xoxb-minny", "workspace_id": "minny" }))
            .await
            .unwrap();
        assert!(!old_shape.success);
        assert_eq!(
            old_shape.error.as_deref(),
            Some("slack.app_configure needs bot_token (xoxb-…) and app_token (xapp-…) as strings")
        );

        let connected = tool
            .execute(serde_json::json!({ "bot_token": "xoxb-minny", "app_token": "xapp-minny" }))
            .await
            .unwrap();
        assert!(connected.success, "{:?}", connected.error);
        assert_eq!(connected.output["bot_user_id"], "U_MINNY");
        assert!(!connected.output.to_string().contains("xoxb-minny"));
    }

    #[tokio::test]
    async fn an_agent_created_after_boot_is_wired_and_keeps_it_across_a_respawn() {
        let hub = Hub::new(&["allan"]).await;
        let file = hub.control.manager().lock().await.config_file();
        std::fs::create_dir_all(file.parent().unwrap()).unwrap();
        std::fs::write(
            &file,
            "[[workspaces]]\nid = \"minny\"\n[agent_bindings]\nminny = \"minny\"\n",
        )
        .unwrap();
        hub.control
            .manager()
            .lock()
            .await
            .register_surfaces_from_disk()
            .await
            .unwrap();
        assert_eq!(
            hub.control.manager().lock().await.skipped()["minny"].to_string(),
            "bound to agent 'minny', which is not running"
        );

        let minny = agent(&hub.config, "Minny").await;
        vault_of(&minny)
            .set("slack.minny.bot_token", "xoxb-minny")
            .unwrap();
        vault_of(&minny)
            .set("slack.minny.app_token", "xapp-minny")
            .unwrap();
        hub.control.wire_agent(&minny).await;
        assert!(minny.tools.has("slack.connect"));
        assert!(minny.tools.has("system.reload_slack"));
        hub.registry.insert("Minny", Arc::clone(&minny));
        hub.control.agent_joined("Minny").await;

        assert_eq!(hub.slack_workspaces("Minny").await, vec!["minny"]);
        assert!(minny.tools.has("slack.list_channels"));
        let status = hub.control.status("Minny").await.unwrap();
        assert!(status.workspaces[0].connected, "{status:?}");
        assert!(hub.slack_workspaces("allan").await.is_empty());

        hub.registry.remove("Minny");
        let respawned = agent(&hub.config, "Minny").await;
        hub.control.wire_agent(&respawned).await;
        hub.registry.insert("Minny", Arc::clone(&respawned));
        assert_eq!(hub.slack_workspaces("Minny").await, vec!["minny"]);
        assert!(respawned.tools.has("slack.list_channels"));
        assert!(respawned.tools.has("slack.connect"));

        hub.registry.remove("Minny");
        hub.control.agent_left("Minny").await;
        let manager = hub.control.manager().lock().await;
        assert!(manager.bot_identities().is_empty());
        assert_eq!(
            manager.skipped()["minny"].to_string(),
            "bound to agent 'minny', which is not running"
        );
    }
}
