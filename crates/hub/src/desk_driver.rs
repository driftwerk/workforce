//! Hub adapter for `workforce-desk`: the session registry in Postgres,
//! system-admin-only access (a desk session runs commands as the hub's
//! user), the vault keys behind cloud voice sessions, and the `/api/desk`
//! routes on the gateway.

use std::sync::{Arc, OnceLock};
use std::time::Duration;

use async_trait::async_trait;
use axum::http::{HeaderMap, StatusCode};
use axum::Router;
use eyre::{Context, Result};
use tokio_util::sync::CancellationToken;
use workforce_common::settings::{DeskRestore, WorkforceConfig};
use workforce_desk::{
    Authorize, Desk, DeskStore, KeySource, Options, Provider, Realtime, RestorePolicy,
};

use crate::credentials::{how_to_add, Caller, Legacy};
use crate::desk_tools::DeskTool;
use crate::gateway::{resolve_caller_context, GatewayState};
use crate::serve::AgentService;
use crate::skill_loader::CredScope;
use crate::vault::Vault;

/// The hub's one desk, once `[desk]` has started it.
static DESK: OnceLock<Desk> = OnceLock::new();

/// Gives `service` the `desk.*` tools when desk sessions are on. Called for
/// every agent at boot and for agents spawned later.
pub fn register_tools(service: &AgentService) {
    let Some(desk) = DESK.get() else { return };
    for tool in DeskTool::all(desk) {
        if !service.tools.has(workforce_agent::tools::Tool::name(&tool)) {
            service.tools.register(Arc::new(tool));
        }
    }
}

struct SystemAdmins(Arc<GatewayState>);

#[async_trait]
impl Authorize for SystemAdmins {
    async fn authorize(
        &self,
        headers: &HeaderMap,
        query_token: Option<&str>,
    ) -> Result<(), StatusCode> {
        let caller = resolve_caller_context(&self.0, headers, query_token).await;
        match caller {
            c if c.is_admin => Ok(()),
            c if c.identity_id.is_some() || c.device_id.is_some() => Err(StatusCode::FORBIDDEN),
            _ => Err(StatusCode::UNAUTHORIZED),
        }
    }
}

/// Cloud voice keys from the vault alone: the provider's speech key at
/// `speech/<provider>/api_key` (the person's own, then shared tiers), then
/// the provider's LLM account. Never the environment.
struct VaultKeys(Option<Arc<Vault>>);

fn speech_key_path(provider: Provider) -> String {
    format!("speech/{}/api_key", provider.name())
}

impl KeySource for VaultKeys {
    fn api_key(&self, provider: Provider) -> Option<String> {
        Caller::new(None, None, self.0.clone())
            .resolve(&speech_key_path(provider), CredScope::Either)
            .or_else(|| {
                Legacy::default()
                    .account(None, self.0.clone(), provider.name())
                    .key()
            })
    }

    fn how_to_add(&self, provider: Provider) -> String {
        how_to_add(&speech_key_path(provider))
    }
}

fn options(config: &WorkforceConfig) -> Options {
    let desk = &config.desk;
    Options {
        claude: desk.claude.clone().into(),
        codex: desk.codex.clone().into(),
        env: std::env::vars().collect(),
        run_dir: std::env::temp_dir().join("wf-desk"),
        scrollback_bytes: desk.scrollback_kb.saturating_mul(1024),
        approval_timeout: Duration::from_secs(desk.approval_timeout_secs.max(10)),
        restore: match desk.restore {
            DeskRestore::Ask => RestorePolicy::Ask,
            DeskRestore::Auto => RestorePolicy::Auto,
            DeskRestore::Off => RestorePolicy::Off,
        },
        adopt_every: (desk.adopt_interval_secs > 0)
            .then(|| Duration::from_secs(desk.adopt_interval_secs)),
    }
}

/// Merges the desk routes into `app` when `[desk]` is enabled. Fails closed
/// without Postgres: the registry is what restore reads.
pub async fn mount(
    app: Router,
    state: &Arc<GatewayState>,
    config: &WorkforceConfig,
    cancel: CancellationToken,
) -> Result<Router> {
    if !config.desk.enabled {
        return Ok(app);
    }
    let pg = config
        .postgres
        .as_ref()
        .filter(|p| p.enabled)
        .ok_or_else(|| eyre::eyre!("[desk] is enabled but [postgres] is not"))?;
    let store = DeskStore::connect(pg)
        .await
        .context("opening the desk registry")?;
    let desk = Desk::start(store, options(config), cancel)
        .await
        .context("starting desk sessions")?;
    if DESK.set(desk.clone()).is_err() {
        eyre::bail!("desk sessions started twice");
    }
    for service in state.registry.all_services() {
        register_tools(&service);
    }
    tracing::info!("desk sessions enabled");
    let voice = Realtime::new(Arc::new(VaultKeys(state.vault.clone())));
    Ok(app.merge(workforce_desk::router(
        desk,
        Arc::new(SystemAdmins(Arc::clone(state))),
        Arc::new(voice),
    )))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn vault(dir: &std::path::Path) -> Arc<Vault> {
        Arc::new(Vault::open(&dir.join("global"), "test-key").unwrap())
    }

    #[test]
    fn voice_keys_come_from_the_speech_path_in_the_vault() {
        let dir = tempfile::tempdir().unwrap();
        let global = vault(dir.path());
        let keys = VaultKeys(Some(Arc::clone(&global)));
        assert_eq!(keys.api_key(Provider::Gemini), None);
        global.set("speech/gemini/api_key", "gm-test").unwrap();
        assert_eq!(keys.api_key(Provider::Gemini).as_deref(), Some("gm-test"));
        assert_eq!(keys.api_key(Provider::OpenAi), None);
        assert!(keys
            .how_to_add(Provider::OpenAi)
            .contains("speech/openai/api_key"));
    }

    #[test]
    fn no_vault_means_no_voice_keys() {
        assert_eq!(VaultKeys(None).api_key(Provider::OpenAi), None);
    }
}
