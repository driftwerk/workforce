//! Multi-agent service registry.
//!
//! Holds one [`AgentService`] per configured agent, keyed by [`AgentId`].
//! The gateway and chat workers look up services from here.

use crate::serve::Compose as _;
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

use eyre::Result;
use tracing::{error, info, warn};
use workforce_common::locking::LockManager;
use workforce_common::settings::{AgentConfig, WorkforceConfig};
use workforce_core::types::AgentId;

use crate::hosts::{NotServing, Unloaded};
use crate::serve::AgentService;

/// Registry of running agent services.
///
/// The agent set is mutable at runtime (live spawn/teardown from config
/// overlay changes):
/// every holder shares one `Arc<AgentRegistry>` and the map inside it
/// changes under a lock. Accessors return owned `Arc<AgentService>` clones;
/// sessions holding a torn-down service keep it alive until they finish.
pub struct AgentRegistry {
    agents: parking_lot::RwLock<HashMap<String, Arc<AgentService>>>,
    /// Configured agents that are not serving: being built, or failed to.
    unloaded: parking_lot::RwLock<HashMap<String, Unloaded>>,
    /// Woken whenever an agent starts serving or fails to load.
    settled: tokio::sync::Notify,
    default_id: String,
    /// The process-wide access and scope gates, once the hub has built them;
    /// every service, present or spawned later, is attached to them.
    gates: std::sync::OnceLock<Gates>,
    routed: std::sync::OnceLock<tokio::sync::mpsc::Sender<crate::hosts::RoutedApproval>>,
}

#[derive(Clone)]
pub struct Gates {
    pub access: Arc<crate::access_gate::AccessGate>,
    pub scope: Arc<crate::scope_gate::ScopeGate>,
}

const LOAD_FAILURE_CHARS: usize = 500;

/// The live agent id `wanted` names: itself when an agent has exactly that id,
/// else the one agent whose id matches it ignoring case. Agent ids are
/// case-sensitive, but a binding written as `minny` for agent `Minny` means
/// that agent, not none.
pub fn canonical_agent_id<'a>(wanted: &str, agents: &'a [String]) -> Option<&'a str> {
    if let Some(exact) = agents.iter().find(|id| *id == wanted) {
        return Some(exact);
    }
    let mut folded = agents.iter().filter(|id| id.eq_ignore_ascii_case(wanted));
    match (folded.next(), folded.next()) {
        (Some(only), None) => Some(only),
        _ => None,
    }
}

/// A non-default agent's build, or — once an error naming the agent and why
/// it failed is logged — that reason, fit to show: one agent's broken config
/// leaves the hub serving the others, where the default agent's stops the hub.
pub fn loaded<T>(agent_id: &str, built: Result<T>) -> std::result::Result<T, String> {
    built.map_err(|e| {
        let reason = format!("{e:#}");
        error!(agent_id, error = %reason, "Agent not loaded; the hub runs without it until its config is fixed");
        workforce_common::redact::redact_secrets(&reason)
            .chars()
            .take(LOAD_FAILURE_CHARS)
            .collect()
    })
}

fn attach_gates(service: &AgentService, gates: &Gates) {
    service.set_access_gate(Arc::clone(&gates.access));
    service.set_scope_gate(Arc::clone(&gates.scope));
}

impl AgentRegistry {
    /// Build from config — creates one `AgentService` per `[[agents]]` entry.
    ///
    /// Knowledge services are grouped by effective knowledge root: agents that
    /// index the same directory share a single `KnowledgeService` instance,
    /// while agents with different roots get isolated instances.
    ///
    /// If no agents are configured, creates a single default agent using the
    /// legacy workspace path (`{data_dir}/workspace`).
    pub async fn from_config(config: &WorkforceConfig) -> Result<Self> {
        let mut agents = HashMap::new();

        // Create shared lock manager for all agents
        let lock_manager = Arc::new(LockManager::new());
        let subscriptions = crate::subscription::Subscriptions::boot(config, None);

        if config.agents.is_empty() {
            // Legacy: no [[agents]] section — create a single default agent.
            // build_for_agent with shared_knowledge=None creates its own.
            info!("No [[agents]] configured — using single default agent");
            let service = AgentService::build_for_agent(
                config.clone(),
                None,
                None,
                None,
                Arc::clone(&lock_manager),
                None,
                None,
                None,
                None,
                subscriptions,
            )
            .await?;
            agents.insert("default".to_string(), service);
            return Ok(Self::from_parts(agents, "default".to_string()));
        }

        // ── Build knowledge services grouped by effective root ──
        //
        // Agents with the same knowledge root share one KnowledgeService.
        // Agents with knowledge.enabled=false get None.
        let global_vault = crate::vault::open_global(config)
            .await
            .inspect_err(|e| {
                warn!(error = %e, "Global vault unavailable; the knowledge summariser reads its key from the install config")
            })
            .ok()
            .flatten();
        let mut knowledge_by_root: HashMap<PathBuf, Arc<crate::knowledge::KnowledgeService>> =
            HashMap::new();
        let mut agent_knowledge: Vec<Option<Arc<crate::knowledge::KnowledgeService>>> =
            Vec::with_capacity(config.agents.len());

        for agent_config in &config.agents {
            let effective = agent_config.effective_knowledge(&config.knowledge);
            if !effective.enabled {
                info!(
                    agent = %agent_config.id,
                    "Knowledge disabled for this agent"
                );
                agent_knowledge.push(None);
                continue;
            }

            // Resolve the effective root path for grouping
            let root = effective
                .root
                .as_ref()
                .map(PathBuf::from)
                .unwrap_or_else(|| PathBuf::from(&config.hub.data_dir));

            // Canonicalize for consistent grouping (handles symlinks, trailing slashes)
            let canonical_root = std::fs::canonicalize(&root).unwrap_or_else(|_| root.clone());

            info!(
                agent = %agent_config.id,
                root = %root.display(),
                canonical = %canonical_root.display(),
                "Resolved knowledge root for agent"
            );

            if let Some(existing) = knowledge_by_root.get(&canonical_root) {
                info!(
                    agent = %agent_config.id,
                    root = %root.display(),
                    "Sharing knowledge service with existing group"
                );
                agent_knowledge.push(Some(Arc::clone(existing)));
            } else {
                match crate::knowledge::create_service(
                    &effective,
                    crate::knowledge::VectorBackend::of(config),
                    &config.hub.data_dir,
                    &config.codegraph,
                    config.postgres.as_ref(),
                    crate::knowledge::HubSummariserBuilder::new(effective.summariser.clone())
                        .with_global_vault(global_vault.clone()),
                    crate::knowledge::VectorUpkeep::Kept,
                ) {
                    Some(ks) => {
                        info!(
                            agent = %agent_config.id,
                            root = %root.display(),
                            "Created knowledge service for root"
                        );
                        knowledge_by_root.insert(canonical_root, Arc::clone(&ks));
                        agent_knowledge.push(Some(ks));
                    }
                    None => {
                        agent_knowledge.push(None);
                    }
                }
            }
        }

        // ── Build agent services ──

        let default_id = config
            .default_agent()
            .map(|a| a.id.clone())
            .unwrap_or_else(|| "default".to_string());

        // The first agent built owns the hub's single workflow runtime; the rest
        // share it so schedule triggers are registered exactly once.
        let mut shared_workflow_runtime: Option<Arc<dyn crate::hosts::WorkflowHost>> = None;
        for (i, agent_config) in config.agents.iter().enumerate() {
            info!(agent = %agent_config.id, "Building agent service");
            let service = AgentService::build_for_agent(
                config.clone(),
                None,
                Some(agent_config),
                None,
                Arc::clone(&lock_manager),
                agent_knowledge[i].clone(),
                None,
                shared_workflow_runtime.clone(),
                None,
                subscriptions.clone(),
            )
            .await?;
            shared_workflow_runtime =
                shared_workflow_runtime.or_else(|| service.workflow_runtime().cloned());
            agents.insert(agent_config.id.clone(), service);
        }

        // ── Start background tasks once per unique knowledge service ──

        for ks in knowledge_by_root.values() {
            crate::knowledge::start_background(ks, config);
        }

        info!(
            count = agents.len(),
            knowledge_groups = knowledge_by_root.len(),
            default = %default_id,
            "Agent registry initialised"
        );

        Ok(Self::from_parts(agents, default_id))
    }

    /// Look up an agent service by ID.
    pub fn get(&self, id: &str) -> Option<Arc<AgentService>> {
        self.agents.read().get(id).cloned()
    }

    /// Whether `identity` may use `agent` as a chat with it would decide:
    /// it is registered and its access policy admits them.
    pub fn admits(&self, agent: &str, identity: &str) -> bool {
        self.get(agent).is_some_and(|service| {
            service.access_gate.read().as_ref().is_some_and(|gate| {
                matches!(
                    gate.check(crate::access_gate::AccessRequest {
                        agent_id: agent,
                        identity_id: Some(identity),
                        surface_ids: &[],
                        is_external: false,
                    }),
                    workforce_common::settings::AccessDecision::Allow
                )
            })
        })
    }

    /// Get the default agent service.
    pub fn default_agent(&self) -> Option<Arc<AgentService>> {
        self.agents.read().get(&self.default_id).cloned()
    }

    /// Get the default agent ID.
    pub fn default_id(&self) -> &str {
        &self.default_id
    }

    /// List all registered agent IDs.
    pub fn agent_ids(&self) -> Vec<AgentId> {
        self.agents.read().keys().map(AgentId::new).collect()
    }

    /// Number of registered agents.
    pub fn len(&self) -> usize {
        self.agents.read().len()
    }

    /// Whether the registry is empty.
    pub fn is_empty(&self) -> bool {
        self.agents.read().is_empty()
    }

    /// Snapshot of all agent services.
    pub fn all_services(&self) -> Vec<Arc<AgentService>> {
        self.agents.read().values().cloned().collect()
    }

    /// Every vault the hub holds, once each: `global` and every agent's.
    /// Where a copy of a login may sit.
    pub fn login_vaults(
        &self,
        global: Option<&Arc<crate::vault::Vault>>,
    ) -> Vec<Arc<crate::vault::Vault>> {
        crate::subscription::store::distinct_vaults(
            global.cloned().into_iter().chain(
                self.all_services()
                    .into_iter()
                    .filter_map(|service| service.vault.clone()),
            ),
        )
    }

    /// Insert (or replace) a live agent service. Used by the overlay watcher
    /// for runtime spawn; the caller is responsible for wiring the service
    /// (advisor / coding.session / MCP) after insertion.
    pub fn insert(&self, id: &str, service: Arc<AgentService>) -> Option<Arc<AgentService>> {
        info!(agent_id = %id, "Agent service inserted into live registry");
        let replaced = self.agents.write().insert(id.to_string(), service);
        self.unloaded.write().remove(id);
        self.settled.notify_waiters();
        replaced
    }

    /// `config`'s agent is being built: until it is inserted or fails, it
    /// is starting rather than missing.
    pub fn begin_loading(&self, config: &AgentConfig) {
        self.unloaded.write().insert(
            config.id.clone(),
            Unloaded {
                config: config.clone(),
                state: NotServing::Starting,
            },
        );
    }

    /// `config`'s agent failed to load, for `reason`.
    pub fn load_failed(&self, config: &AgentConfig, reason: String) {
        self.unloaded.write().insert(
            config.id.clone(),
            Unloaded {
                config: config.clone(),
                state: NotServing::Failed(reason),
            },
        );
        self.settled.notify_waiters();
    }

    /// Forget the agents no longer configured among those not serving.
    pub fn retain_unloaded(&self, configured: impl Fn(&str) -> bool) {
        self.unloaded.write().retain(|id, _| configured(id));
        self.settled.notify_waiters();
    }

    /// The configured agents that are not serving, sorted by id.
    pub fn unloaded(&self) -> Vec<Unloaded> {
        let mut unloaded: Vec<Unloaded> = self.unloaded.read().values().cloned().collect();
        unloaded.sort_by(|a, b| a.config.id.cmp(&b.config.id));
        unloaded
    }

    /// Why `id` is not serving, when it is configured but not live.
    pub fn not_serving(&self, id: &str) -> Option<NotServing> {
        self.unloaded.read().get(id).map(|u| u.state.clone())
    }

    /// `id`'s service once it serves, waiting up to `within` while it is
    /// starting. `None` when it failed, is unknown, or is still starting.
    pub async fn wait_until_serving(
        &self,
        id: &str,
        within: std::time::Duration,
    ) -> Option<Arc<AgentService>> {
        let deadline = tokio::time::Instant::now() + within;
        loop {
            let settled = self.settled.notified();
            tokio::pin!(settled);
            settled.as_mut().enable();
            if let Some(service) = self.get(id) {
                return Some(service);
            }
            if self.not_serving(id) != Some(NotServing::Starting) {
                return None;
            }
            tokio::time::timeout_at(deadline, settled).await.ok()?;
        }
    }

    /// Remove a live agent service. The default agent is never removed.
    /// In-flight sessions holding the service's `Arc` keep it alive until
    /// they finish; new lookups stop resolving it immediately, and its
    /// scheduler retires so a replacement never shares a live loop on the
    /// same job store.
    pub fn remove(&self, id: &str) -> Option<Arc<AgentService>> {
        if id == self.default_id {
            warn!(agent_id = %id, "Refusing to remove the default agent from the registry");
            return None;
        }
        let removed = self.agents.write().remove(id);
        if let Some(service) = &removed {
            service.retire();
        }
        if removed.is_some() {
            info!(agent_id = %id, "Agent service removed from live registry");
        }
        removed
    }

    /// List all agents with their capabilities.
    ///
    /// Returns `(agent_id, capabilities)` pairs. Capabilities are derived
    /// from the agent's tool registry.
    pub fn list_all(&self) -> Vec<(String, Vec<String>)> {
        self.agents
            .read()
            .iter()
            .map(|(id, svc)| {
                let caps: Vec<String> = svc.tools.names();
                (id.clone(), caps)
            })
            .collect()
    }

    /// Count total active runners across all agents.
    pub async fn active_runner_count(&self) -> usize {
        let mut total = 0;
        for svc in self.all_services() {
            total += svc.active_runners.read().await.len();
        }
        total
    }

    /// Resolve an agent_id: use the provided ID if it exists, otherwise fall back to default.
    pub fn resolve(&self, agent_id: Option<&str>) -> Option<Arc<AgentService>> {
        match agent_id {
            Some(id) => self.get(id).or_else(|| self.default_agent()),
            None => self.default_agent(),
        }
    }

    /// Where every routed approval raised on this registry's agents goes to
    /// have its approvers DMed: one queue shared by remote MCP and CLI rounds,
    /// started on first use and ending with the registry.
    pub fn routed_delivery(
        self: &Arc<Self>,
    ) -> tokio::sync::mpsc::Sender<crate::hosts::RoutedApproval> {
        self.routed
            .get_or_init(|| {
                crate::approvals::slack::spawn_routed_delivery(Arc::<Self>::downgrade(self))
            })
            .clone()
    }

    /// Build a registry from a pre-built map of agents.
    pub fn from_parts(agents: HashMap<String, Arc<AgentService>>, default_id: String) -> Self {
        Self {
            agents: parking_lot::RwLock::new(agents),
            unloaded: parking_lot::RwLock::default(),
            settled: tokio::sync::Notify::new(),
            default_id,
            gates: std::sync::OnceLock::new(),
            routed: std::sync::OnceLock::new(),
        }
    }

    /// Install the process-wide gates: every current service is attached to
    /// them now and each later-spawned one on its way in, so a workspace or
    /// overlay agent is filtered the same as one from the config file.
    pub fn install_gates(&self, gates: Gates) {
        let gates = self.gates.get_or_init(|| gates);
        for service in self.all_services() {
            attach_gates(&service, gates);
        }
    }

    /// Attach the registry's weak ref to every contained service and
    /// register the advisor tool on agents whose `effective_advisor` is
    /// enabled. Idempotent — safe to call multiple times. Call this
    /// after wrapping the registry in `Arc::new(...)`.
    pub fn wire_advisor_tools(self: &Arc<Self>) {
        for service in self.all_services() {
            self.wire_advisor_tools_for(&service);
        }
    }

    /// Single-service sibling of [`Self::wire_advisor_tools`] — used when a
    /// service is spawned into a live registry. Also wires the agent-job
    /// executor, the tool-usage refresh and the installed gates, the other
    /// per-service capabilities that need the service `Arc` after construction.
    pub fn wire_advisor_tools_for(self: &Arc<Self>, service: &Arc<AgentService>) {
        service.set_agent_registry(Arc::downgrade(self));
        crate::serve::register_advisor_tool(service);
        crate::serve::wire_agent_jobs(service);
        service.start_tool_usage_refresh();
        if let Some(gates) = self.gates.get() {
            attach_gates(service, gates);
        }
    }

    /// Register the `coding.session` tool on every contained service.
    /// Idempotent. Separate from [`Self::wire_advisor_tools`] because it needs
    /// the project store and base data dir, which aren't `AgentService` fields.
    /// Call after the project store is built (it's constructed later than the
    /// registry in the hub `Run` path).
    ///
    /// Also registers `claude_code.setup_token` unconditionally on every
    /// agent with a vault, with a reload callback that re-runs
    /// `register_coding_session_tool` after a successful token write — so a
    /// user can install their Max token via the agent in chat and have
    /// `coding.session` appear the same turn (no hub restart).
    pub fn wire_coding_session_tools(
        self: &Arc<Self>,
        project_store: Arc<dyn workforce_projects::ProjectStore>,
        roots: crate::projects::layout::WorkRoots,
        mcp_enabled: bool,
        coding: workforce_common::settings::CodingSessionConfig,
        exec: workforce_common::settings::ExecConfig,
        subscriptions: &crate::subscription::Subscriptions,
    ) {
        for service in self.all_services() {
            Self::wire_coding_session_tools_for(
                &service,
                Arc::clone(&project_store),
                roots.clone(),
                mcp_enabled,
                coding.clone(),
                exec.clone(),
                subscriptions,
            );
        }
    }

    /// Single-service sibling of [`Self::wire_coding_session_tools`] — used
    /// when a service is spawned into a live registry.
    pub fn wire_coding_session_tools_for(
        service: &Arc<AgentService>,
        project_store: Arc<dyn workforce_projects::ProjectStore>,
        roots: crate::projects::layout::WorkRoots,
        mcp_enabled: bool,
        coding: workforce_common::settings::CodingSessionConfig,
        exec: workforce_common::settings::ExecConfig,
        subscriptions: &crate::subscription::Subscriptions,
    ) {
        crate::agent_turn::wire(
            service,
            roots.clone(),
            &coding,
            &exec,
            subscriptions.codex_worker().cloned(),
        );
        // Closure that (re-)attempts coding.session registration with the
        // current vault state. The `register_coding_session_tool` `has(...)`
        // check makes it a no-op once the tool is already wired.
        let reload: crate::claude_code_setup_token_tool::ReloadCodingSession = {
            let service = Arc::clone(service);
            let project_store = Arc::clone(&project_store);
            Arc::new(move || {
                crate::serve::register_coding_session_tool(
                    &service,
                    Arc::clone(&project_store),
                    roots.clone(),
                    mcp_enabled,
                    coding.clone(),
                    exec.clone(),
                );
                crate::serve::register_claude_code_tool(&service);
            })
        };
        // Initial attempt — succeeds if any token is already configured.
        reload();
        // Setup tool is always registered (independent of token state) so
        // an agent has a path to *install* the token from a no-token start.
        crate::serve::register_claude_code_setup_token_tool(service, Arc::clone(&reload));
        crate::serve::register_codex_setup_auth_tool(service, Arc::clone(&reload));
        crate::serve::register_chatgpt_login_tool(service);
        service.set_coding_session_reload(reload);
    }

    /// Create a registry wrapping a single pre-built service.
    ///
    /// Used when the service was already constructed with custom wiring
    /// (e.g., scheduler delivery, reactions) and just needs to be wrapped.
    pub fn single(service: Arc<AgentService>) -> Self {
        let id = service.agent_id.to_string();
        let mut agents = HashMap::new();
        agents.insert(id.clone(), service);
        Self::from_parts(agents, id)
    }
}

impl crate::hosts::RegistryView for AgentRegistry {
    fn default_agent(&self) -> Option<Arc<AgentService>> {
        AgentRegistry::default_agent(self)
    }

    fn get(&self, id: &str) -> Option<Arc<AgentService>> {
        AgentRegistry::get(self, id)
    }

    fn agent_ids(&self) -> Vec<AgentId> {
        AgentRegistry::agent_ids(self)
    }

    fn admits(&self, agent: &str, identity: &str) -> bool {
        AgentRegistry::admits(self, agent, identity)
    }

    fn all_services(&self) -> Vec<Arc<AgentService>> {
        AgentRegistry::all_services(self)
    }

    fn unloaded(&self) -> Vec<Unloaded> {
        AgentRegistry::unloaded(self)
    }

    fn login_vaults(
        &self,
        global: Option<&Arc<crate::vault::Vault>>,
    ) -> Vec<Arc<crate::vault::Vault>> {
        AgentRegistry::login_vaults(self, global)
    }

    fn routed_delivery(self: Arc<Self>) -> tokio::sync::mpsc::Sender<crate::hosts::RoutedApproval> {
        AgentRegistry::routed_delivery(&self)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn scoped_service(agent_id: &str, prompt: &str) -> Arc<AgentService> {
        let tmp = tempfile::tempdir().unwrap();
        let mut config = WorkforceConfig::default();
        config.hub.data_dir = tmp.path().to_string_lossy().to_string();
        // Not the relative default `data/memory.db`, which every test leaving it
        // shares — under cargo-nextest that is "database is locked".
        config.memory.db_path = tmp.path().join("memory.db").to_string_lossy().into_owned();
        config.llm.api_key = Some("test-key".into());
        config.agents = vec![workforce_common::settings::AgentConfig {
            id: agent_id.into(),
            ..Default::default()
        }];
        std::mem::forget(tmp);
        let service = AgentService::from_config(config, None).await.unwrap();
        std::fs::create_dir_all(&service.workspace).unwrap();
        std::fs::write(
            service.workspace.join("config.toml"),
            format!("[scope]\ncustom_prompt = \"{prompt}\"\n"),
        )
        .unwrap();
        service
    }

    fn entry(id: &str) -> AgentConfig {
        AgentConfig {
            id: id.into(),
            ..Default::default()
        }
    }

    #[tokio::test]
    async fn a_starting_agent_is_waited_for_until_it_serves() {
        let registry = Arc::new(AgentRegistry::from_parts(HashMap::new(), "default".into()));
        registry.begin_loading(&entry("late"));
        assert_eq!(registry.not_serving("late"), Some(NotServing::Starting));

        let waiting = {
            let registry = Arc::clone(&registry);
            tokio::spawn(async move {
                registry
                    .wait_until_serving("late", std::time::Duration::from_secs(60))
                    .await
            })
        };
        tokio::task::yield_now().await;
        registry.insert("late", scoped_service("late", "sales").await);

        let served = waiting.await.unwrap().expect("the agent serves");
        assert_eq!(served.agent_id.as_str(), "late");
        assert_eq!(registry.not_serving("late"), None);
        assert!(registry.unloaded().is_empty());
    }

    #[tokio::test(start_paused = true)]
    async fn waiting_ends_for_an_agent_that_fails_is_unknown_or_stays_starting() {
        let registry = Arc::new(AgentRegistry::from_parts(HashMap::new(), "default".into()));
        let within = std::time::Duration::from_secs(60);
        assert!(registry
            .wait_until_serving("nobody", within)
            .await
            .is_none());

        registry.begin_loading(&entry("broken"));
        let waiting = {
            let registry = Arc::clone(&registry);
            tokio::spawn(async move { registry.wait_until_serving("broken", within).await })
        };
        tokio::task::yield_now().await;
        registry.load_failed(&entry("broken"), "unknown provider".into());
        assert!(waiting.await.unwrap().is_none());
        assert_eq!(
            registry.not_serving("broken").unwrap().to_string(),
            "failed to load: unknown provider"
        );

        registry.begin_loading(&entry("slow"));
        let started = tokio::time::Instant::now();
        assert!(registry.wait_until_serving("slow", within).await.is_none());
        assert_eq!(started.elapsed(), within);

        registry.retain_unloaded(|id| id == "slow");
        assert_eq!(
            registry
                .unloaded()
                .iter()
                .map(|u| u.config.id.as_str())
                .collect::<Vec<_>>(),
            vec!["slow"]
        );
    }

    #[test]
    fn a_load_failure_reason_is_redacted_and_bounded() {
        let key = concat!("sk-ant", "-api03-abcdefghijklmnopqrstuvwxyz1234567890");
        let reason = loaded::<()>(
            "scout",
            Err(eyre::eyre!("provider refused {key}: {}", "x".repeat(2000))),
        )
        .unwrap_err();
        assert!(!reason.contains(key), "{reason}");
        assert_eq!(reason.chars().count(), LOAD_FAILURE_CHARS);
    }

    /// Gates used to be attached only to the services present at boot, so an
    /// agent spawned later ran unfiltered. Installing them on the registry
    /// covers both the present and the late.
    #[tokio::test]
    async fn installed_gates_reach_present_and_later_spawned_services() {
        let present = scoped_service("present", "bugs").await;
        let registry = Arc::new(AgentRegistry::from_parts(
            HashMap::from([("present".to_string(), Arc::clone(&present))]),
            "present".into(),
        ));
        let scope = crate::scope_gate::ScopeGate::new(HashMap::new());
        registry.install_gates(Gates {
            access: crate::access_gate::AccessGate::new(HashMap::new()),
            scope: Arc::clone(&scope),
        });
        assert_eq!(
            scope.lookup("present").unwrap().custom_prompt.as_deref(),
            Some("bugs")
        );

        let late = scoped_service("late", "sales").await;
        registry.insert("late", Arc::clone(&late));
        registry.wire_advisor_tools_for(&late);
        assert_eq!(
            scope.lookup("late").unwrap().custom_prompt.as_deref(),
            Some("sales")
        );
    }
}
