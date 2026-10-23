//! Microsoft Teams skill lifecycle manager (Bot Framework).
//!
//! Teams is push-based: the Azure Bot Connector calls the hub's public
//! `POST /api/messages` (see [`crate::teams_messages`]) rather than the hub
//! holding a socket as Slack does. This manager loads config, resolves each
//! app's credentials from the vault, builds the per-app outbound surface +
//! inbound auth + dispatch loop, registers the surface with the bound agents,
//! and hands back the [`TeamsRouterState`] the receiver mounts.
//!
//! Phase 0 (DW-558) echoes inbound messages to validate the auth + connector
//! round trip; Phase 1 (DW-559) replaces [`echo_message`] with agent dispatch.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

use eyre::Result;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tracing::{debug, info, warn};

use workforce_common::channels::{self, ChannelSurface, OutboundMessage, SurfaceKey};
use workforce_pairing::{ChannelPairingService, SenderCheckResult};
use workforce_skills::adapters::teams::{
    BotConnectorValidator, BotFrameworkClient, InboundMessage, TeamsAction, TeamsEventHandler,
    TeamsSkillConfig, TeamsSurface,
};

use crate::hosts::RegistryView;
use crate::teams_messages::{TeamsEndpoint, TeamsRouterState};
use crate::vault::Vault;

/// Inbound dispatch queue depth per app. Bot Connector requests are answered
/// `200` as soon as the message is enqueued, so this only bounds the echo
/// backlog under a burst.
const ACTION_CHANNEL_CAPACITY: usize = 256;

/// Owns the Teams skill's background tasks for the process lifetime.
pub struct TeamsManager {
    registry: Arc<dyn RegistryView>,
    skills_dir: PathBuf,
    vault: Option<Arc<Vault>>,
    channel_pairing: Option<Arc<ChannelPairingService>>,
    drain_token: tokio_util::sync::CancellationToken,
    task_handles: Vec<JoinHandle<()>>,
}

impl TeamsManager {
    pub fn new(
        registry: Arc<dyn RegistryView>,
        skills_dir: PathBuf,
        vault: Option<Arc<Vault>>,
        channel_pairing: Option<Arc<ChannelPairingService>>,
        drain_token: tokio_util::sync::CancellationToken,
    ) -> Self {
        Self {
            registry,
            skills_dir,
            vault,
            channel_pairing,
            drain_token,
            task_handles: Vec::new(),
        }
    }

    /// Load config and start every app whose credentials resolve. Returns the
    /// router state for the `POST /api/messages` receiver, or `None` when no app
    /// is configured — the skill stays idle (mirrors Slack's "pending" path).
    pub async fn start(&mut self) -> Result<Option<TeamsRouterState>> {
        let config = self.load_config()?;
        let usable = config.usable_apps();
        if usable.is_empty() {
            info!("TeamsManager: no apps have credentials configured — Teams skill idle");
            return Ok(None);
        }

        // The Bot Framework signing keys are the same for every app, so one
        // validator serves them all.
        let validator = Arc::new(BotConnectorValidator::new());
        let mut apps = HashMap::new();
        for app in usable {
            let client = Arc::new(BotFrameworkClient::new(
                app.app_id.clone(),
                app.app_password.clone(),
            ));
            let surface = Arc::new(TeamsSurface::from_client(Arc::clone(&client)));
            self.register_surface(&config, &app.id, Arc::clone(&surface))
                .await;

            let handler =
                Arc::new(TeamsEventHandler::new().with_bot_id(format!("28:{}", app.app_id)));
            let (action_tx, action_rx) = mpsc::channel(ACTION_CHANNEL_CAPACITY);
            self.task_handles.push(tokio::spawn(echo_dispatch_loop(
                action_rx,
                Arc::clone(&surface),
                self.channel_pairing.clone(),
                self.drain_token.clone(),
            )));

            apps.insert(
                app.app_id.clone(),
                TeamsEndpoint {
                    app_id: app.app_id.clone(),
                    surface,
                    handler,
                    action_tx,
                },
            );
        }

        info!(apps = apps.len(), "TeamsManager: Teams skill started");
        Ok(Some(TeamsRouterState { validator, apps }))
    }

    /// Abort the dispatch loops. The drain token also unblocks them gracefully
    /// during shutdown; this is the hard teardown for a reload.
    pub async fn stop(&mut self) {
        for handle in self.task_handles.drain(..) {
            handle.abort();
        }
    }

    /// Register the surface with the agents bound to this app (empty bindings →
    /// all agents, mirroring Slack) so they can later address the conversation.
    async fn register_surface(
        &self,
        config: &TeamsSkillConfig,
        app_config_id: &str,
        surface: Arc<TeamsSurface>,
    ) {
        let key = SurfaceKey::workspace("teams", app_config_id);
        let bound: Vec<&String> = config
            .agent_bindings
            .iter()
            .filter_map(|(id, agent)| (id == app_config_id).then_some(agent))
            .collect();
        let targets = if bound.is_empty() {
            self.registry.all_services()
        } else {
            bound
                .iter()
                .filter_map(|aid| self.registry.get(aid))
                .collect()
        };
        let surface: Arc<dyn ChannelSurface> = surface;
        for agent_svc in &targets {
            agent_svc
                .register_surface(key.clone(), Arc::clone(&surface))
                .await;
        }
    }

    /// Load `skills/teams-integration/config.toml` and merge each app's
    /// credentials from the vault (`teams.<id>.app_id` / `.app_password`), which
    /// is where secrets live — never the toml.
    fn load_config(&self) -> Result<TeamsSkillConfig> {
        let dir = self.skills_dir.join("teams-integration");
        let resolver: Option<&dyn workforce_common::settings::VaultResolver> = self
            .vault
            .as_ref()
            .map(|v| v.as_ref() as &dyn workforce_common::settings::VaultResolver);
        let Some(loaded) =
            workforce_skills::config_loader::load_skill_config_with_vault(&dir, resolver)?
        else {
            return Ok(TeamsSkillConfig::default());
        };
        let mut config: TeamsSkillConfig =
            workforce_skills::config_loader::deserialize_skill_config(&loaded)?;
        if let Some(vault) = self.vault.as_ref() {
            for app in &mut config.apps {
                if app.app_id.is_empty() {
                    if let Some(v) = vault.get(&format!("teams.{}.app_id", app.id)) {
                        app.app_id = v;
                    }
                }
                if app.app_password.is_empty() {
                    if let Some(v) = vault.get(&format!("teams.{}.app_password", app.id)) {
                        app.app_password = v;
                    }
                }
            }
        }
        Ok(config)
    }
}

/// Consume dispatched messages until the queue closes or the hub drains.
async fn echo_dispatch_loop(
    mut action_rx: mpsc::Receiver<TeamsAction>,
    surface: Arc<TeamsSurface>,
    pairing: Option<Arc<ChannelPairingService>>,
    drain: tokio_util::sync::CancellationToken,
) {
    loop {
        tokio::select! {
            _ = drain.cancelled() => break,
            action = action_rx.recv() => {
                let Some(action) = action else { break };
                match action {
                    TeamsAction::ProcessMessage(msg) => {
                        echo_message(&surface, pairing.as_deref(), &msg).await
                    }
                }
            }
        }
    }
}

/// Phase 0: echo the inbound text back through the surface. Phase 1 (DW-559)
/// replaces this with identity-resolved, pairing-gated agent dispatch.
async fn echo_message(
    surface: &TeamsSurface,
    pairing: Option<&ChannelPairingService>,
    msg: &InboundMessage,
) {
    // Resolve (but do not yet gate on) the sender — the live echo test asserts
    // the sender maps to an internal identity. Phase 1 promotes this to
    // enforcement before dispatch.
    if let Some(pairing) = pairing {
        let channel = channels::channel_id(&msg.channel);
        let key = msg.identity.pairing_key();
        let name = msg.identity.name.clone().unwrap_or_else(|| key.clone());
        match pairing.check_sender("teams", channel, &key, &name) {
            Ok(SenderCheckResult::Allowed) => debug!(sender = %key, "Teams sender paired"),
            Ok(SenderCheckResult::Blocked) => {
                info!(sender = %key, "Teams sender not allowed (Phase 0 still echoes)")
            }
            Ok(SenderCheckResult::PairingRequired { .. }) => {
                info!(sender = %key, "Teams sender unpaired (Phase 0 still echoes)")
            }
            Err(e) => warn!(error = %e, "Teams pairing check failed"),
        }
    }

    let text = msg.text.trim();
    if text.is_empty() {
        return;
    }
    let reply = OutboundMessage {
        channel: msg.channel.clone(),
        text: text.to_string(),
        thread_id: None,
        username: None,
        icon_url: None,
        blocks: None,
    };
    if let Err(e) = surface.send_message(&reply).await {
        warn!(error = %e, "Teams echo reply failed");
    }
}
