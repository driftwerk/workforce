//! Monitor runtime — the owned definition store, mirroring the workflow and
//! playbook runtimes. Definitions live in `workflow_defs` under kind `monitor`;
//! each loaded monitor carries its filter or condition compiled once so the
//! dispatch and poll hot paths never recompile.

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::{Arc, OnceLock};

use eyre::WrapErr;
use tokio::sync::RwLock;

use workforce_core::access::Visibility;
use workforce_orchestrator::monitor::definition::SURFACE_ANY;
use workforce_orchestrator::monitor::{
    Action, CompiledCondition, CompiledFilter, MonitorDefinition, MonitorKey, Source,
};
use workforce_orchestrator::workflow_store::{
    EditContext, RunAsWrite, WorkflowStore, KIND_MONITOR,
};

pub use crate::hub_core::monitors::{MonitorStatus, MonitorSummary};
use crate::resource_access::DefListing;

/// Composite addressing key — bare id for system/disk definitions, `{owner}/{id}`
/// for member-owned ones. Mirrors the workflow and playbook runtimes.
fn def_key(owner: Option<&str>, id: &str) -> String {
    match owner {
        Some(o) => format!("{o}/{id}"),
        None => id.to_string(),
    }
}

/// What a loaded monitor tests, compiled once: a channel's message filter or
/// a source's condition.
pub enum CompiledWatch {
    Channel(CompiledFilter),
    Source(CompiledCondition),
}

impl CompiledWatch {
    fn compile(def: &MonitorDefinition) -> eyre::Result<Self> {
        match def.monitor.condition() {
            Some(condition) => CompiledCondition::compile(&condition).map(Self::Source),
            None => CompiledFilter::compile(def.monitor.filter()).map(Self::Channel),
        }
    }

    fn kind(&self) -> &'static str {
        match self {
            Self::Channel(filter) => filter.kind(),
            Self::Source(condition) => condition.kind().name(),
        }
    }
}

/// A loaded monitor with its owner, visibility, last editor, and what it
/// tests compiled once.
pub struct OwnedMonitor {
    pub def: MonitorDefinition,
    pub owner: Option<String>,
    pub visibility: Visibility,
    /// Identity that last wrote this monitor; taint-gates its automated fires
    /// against a share grantee's edit (a monitor only ever runs as its owner).
    pub last_editor: Option<String>,
    pub watch: CompiledWatch,
}

impl OwnedMonitor {
    fn key(&self) -> String {
        def_key(self.owner.as_deref(), &self.def.monitor.id)
    }

    pub fn filter(&self) -> Option<&CompiledFilter> {
        match &self.watch {
            CompiledWatch::Channel(filter) => Some(filter),
            CompiledWatch::Source(_) => None,
        }
    }

    /// The source and its condition, for a monitor that polls one.
    pub fn source(&self) -> Option<(&Source, &CompiledCondition)> {
        match (&self.watch, self.def.monitor.source()) {
            (CompiledWatch::Source(condition), Some(source)) => Some((source, condition)),
            _ => None,
        }
    }

    pub fn monitor_key(&self) -> MonitorKey {
        MonitorKey {
            monitor_id: self.def.monitor.id.clone(),
            owner: self.owner.clone(),
        }
    }

    pub fn enabled(&self) -> bool {
        self.def.monitor.enabled
    }

    /// Enabled and untainted: the monitor may fire.
    pub fn live(&self) -> bool {
        self.enabled() && !self.tainted()
    }

    /// Whether this monitor must not auto-fire because a non-owner last edited it
    /// (a monitor runs as its owner; foreign content must not run as the owner
    /// until the owner re-saves).
    pub fn tainted(&self) -> bool {
        workforce_orchestrator::workflow_store::auto_run_tainted(
            self.owner.as_deref(),
            self.last_editor.as_deref(),
            self.owner.as_deref(),
        )
    }

    fn build(
        def: MonitorDefinition,
        owner: Option<String>,
        visibility: Visibility,
        last_editor: Option<String>,
    ) -> Option<Self> {
        match CompiledWatch::compile(&def) {
            Ok(watch) => Some(Self {
                def,
                owner,
                visibility,
                last_editor,
                watch,
            }),
            Err(e) => {
                tracing::warn!(id = %def.monitor.id, error = %e, "Skipping monitor whose filter or condition does not compile");
                None
            }
        }
    }
}

/// The agent an engine runs as. Monitors are partitioned across engines by
/// their watching agent; unbound monitors belong to the default agent's engine.
#[derive(Clone, Debug)]
pub struct EngineAgent {
    pub id: String,
    pub name: String,
    pub is_default: bool,
}

impl EngineAgent {
    pub fn watches(&self, mon: &OwnedMonitor) -> bool {
        self.watches_def(&mon.def)
    }

    pub fn watches_def(&self, def: &MonitorDefinition) -> bool {
        def.monitor
            .watcher()
            .map_or(self.is_default, |watcher| watcher == self.id)
    }
}

/// The hub's configured agents: lets a save refuse an unknown watcher and a
/// listing name the agent an unbound monitor resolves to.
#[derive(Clone, Debug)]
pub struct KnownAgents {
    pub default_id: String,
    pub ids: Vec<String>,
}

impl KnownAgents {
    fn knows(&self, agent: &str) -> bool {
        self.ids.iter().any(|id| id == agent)
    }
}

/// Holds the loaded monitor definitions, hot-reloadable from disk or DB.
pub struct MonitorRuntime {
    dir: PathBuf,
    defs: RwLock<HashMap<String, Arc<OwnedMonitor>>>,
    store: OnceLock<Arc<WorkflowStore>>,
    agents: OnceLock<KnownAgents>,
}

impl MonitorRuntime {
    pub fn new(dir: PathBuf) -> Self {
        let mut map = HashMap::new();
        match MonitorDefinition::load_dir(&dir) {
            Ok(defs) => {
                for def in defs {
                    if let Some(o) = OwnedMonitor::build(def, None, Visibility::Global, None) {
                        map.insert(o.key(), Arc::new(o));
                    }
                }
            }
            Err(e) => tracing::warn!(error = %e, "Failed to load monitor definitions"),
        }
        Self {
            dir,
            defs: RwLock::new(map),
            store: OnceLock::new(),
            agents: OnceLock::new(),
        }
    }

    pub fn set_store(&self, store: Arc<WorkflowStore>) {
        let _ = self.store.set(store);
    }

    pub fn set_agents(&self, agents: KnownAgents) {
        let _ = self.agents.set(agents);
    }

    /// The agent `def` resolves to, when the hub's agents are known.
    fn watcher_of(&self, def: &MonitorDefinition) -> Option<String> {
        def.monitor
            .watcher()
            .map(str::to_string)
            .or_else(|| self.agents.get().map(|a| a.default_id.clone()))
    }

    /// Whether `agent` is one this hub does not run; unanswerable (no agent
    /// list, or no agent) is not unknown.
    fn is_unknown_agent(&self, agent: Option<&str>) -> bool {
        match (self.agents.get(), agent) {
            (Some(agents), Some(agent)) => !agents.knows(agent),
            _ => false,
        }
    }

    fn check_watcher(&self, def: &MonitorDefinition) -> eyre::Result<()> {
        let watcher = def.monitor.watcher();
        if !self.is_unknown_agent(watcher) {
            return Ok(());
        }
        Err(eyre::eyre!(
            "Monitor '{}' names agent '{}', which this hub does not run (agents: {})",
            def.monitor.id,
            watcher.unwrap_or_default(),
            self.agents
                .get()
                .map(|a| a.ids.join(", "))
                .unwrap_or_default()
        ))
    }

    fn status_of(&self, o: &OwnedMonitor, agent: Option<&str>) -> MonitorStatus {
        if !o.enabled() {
            MonitorStatus::Paused
        } else if o.tainted() {
            MonitorStatus::AwaitingOwner
        } else if self.is_unknown_agent(agent) {
            MonitorStatus::UnknownAgent
        } else {
            MonitorStatus::Active
        }
    }

    fn summary(&self, o: &OwnedMonitor) -> MonitorSummary {
        let agent = self.watcher_of(&o.def);
        let binding = o.def.monitor.binding();
        MonitorSummary {
            id: o.def.monitor.id.clone(),
            name: o.def.monitor.name.clone(),
            description: o.def.monitor.description.clone(),
            kind: if binding.is_some() {
                "channel"
            } else {
                "source"
            },
            surface: binding.map(|b| b.surface.clone()),
            channel: binding.map(|b| b.channel.clone()),
            source: o.def.monitor.source().map(|s| s.tool.clone()),
            filter: o.watch.kind().to_string(),
            action: o.def.monitor.action.kind().to_string(),
            enabled: o.enabled(),
            owner: o.owner.clone(),
            visibility: o.visibility,
            status: self.status_of(o, agent.as_deref()),
            agent,
            block: None,
            poll: None,
        }
    }

    /// A persona may not share a channel: the one definition-level rule a
    /// save checks against the other monitors loaded.
    pub async fn check_definition(&self, def: &MonitorDefinition) -> eyre::Result<()> {
        if let (Action::Persona { .. }, Some(binding)) =
            (&def.monitor.action, def.monitor.binding())
        {
            if let Some(other) = self
                .persona_conflict(&binding.channel, &def.monitor.id)
                .await
            {
                return Err(eyre::eyre!(
                    "channel {} already has a persona monitor '{other}'",
                    binding.channel
                ));
            }
        }
        Ok(())
    }

    pub(crate) fn def_store(&self) -> Option<&Arc<WorkflowStore>> {
        self.store.get()
    }

    fn block_on<F, T>(fut: F) -> T
    where
        F: std::future::Future<Output = T>,
    {
        tokio::task::block_in_place(|| tokio::runtime::Handle::current().block_on(fut))
    }

    pub async fn reload(&self) -> eyre::Result<usize> {
        let mut map = HashMap::new();
        if let Some(store) = self.def_store() {
            for row in store.list(KIND_MONITOR).await? {
                match MonitorDefinition::from_toml(&row.toml_source) {
                    Ok(def) => {
                        if let Some(o) =
                            OwnedMonitor::build(def, row.owner, row.visibility, row.last_editor)
                        {
                            map.insert(o.key(), Arc::new(o));
                        }
                    }
                    Err(e) => {
                        tracing::warn!(id = %row.id, error = %e, "Skipping invalid monitor def in DB")
                    }
                }
            }
        } else {
            for def in MonitorDefinition::load_dir(&self.dir)? {
                if let Some(o) = OwnedMonitor::build(def, None, Visibility::Global, None) {
                    map.insert(o.key(), Arc::new(o));
                }
            }
        }
        let count = map.len();
        *self.defs.write().await = map;
        Ok(count)
    }

    /// Live monitors `agent` watches whose binding matches `(surface, channel)`.
    /// A tainted monitor (last edited by a non-owner) is treated like a disabled
    /// one — it does not fire until its owner re-saves.
    pub async fn matching(
        &self,
        agent: &EngineAgent,
        surface: &str,
        channel: &str,
    ) -> Vec<Arc<OwnedMonitor>> {
        self.defs
            .read()
            .await
            .values()
            .filter(|o| o.live() && agent.watches(o) && o.def.matches_binding(surface, channel))
            .cloned()
            .collect()
    }

    /// All enabled monitors, whoever watches them.
    pub async fn enabled(&self) -> Vec<Arc<OwnedMonitor>> {
        self.defs
            .read()
            .await
            .values()
            .filter(|o| o.enabled())
            .cloned()
            .collect()
    }

    /// Enabled monitors `agent` watches.
    pub async fn enabled_for(&self, agent: &EngineAgent) -> Vec<Arc<OwnedMonitor>> {
        self.defs
            .read()
            .await
            .values()
            .filter(|o| o.enabled() && agent.watches(o))
            .cloned()
            .collect()
    }

    /// Live source monitors `agent` polls.
    pub async fn live_sources_for(&self, agent: &EngineAgent) -> Vec<Arc<OwnedMonitor>> {
        self.defs
            .read()
            .await
            .values()
            .filter(|o| o.live() && agent.watches(o) && o.source().is_some())
            .cloned()
            .collect()
    }

    /// Distinct `(surface, channel)` watched by `agent`'s enabled monitors.
    /// `any` is expanded across the supplied registered platforms.
    pub async fn watched_channels(
        &self,
        agent: &EngineAgent,
        platforms: &[String],
    ) -> Vec<(String, String)> {
        let mut out: HashSet<(String, String)> = HashSet::new();
        for o in self.enabled_for(agent).await {
            let Some(b) = o.def.monitor.binding() else {
                continue;
            };
            if b.surface == SURFACE_ANY {
                for p in platforms {
                    out.insert((p.clone(), b.channel.clone()));
                }
            } else {
                out.insert((b.surface.clone(), b.channel.clone()));
            }
        }
        out.into_iter().collect()
    }

    /// Look up one monitor by `(id, owner)` — used to re-run a recovered dispatch.
    pub async fn get(&self, id: &str, owner: Option<&str>) -> Option<Arc<OwnedMonitor>> {
        self.defs.read().await.get(&def_key(owner, id)).cloned()
    }

    /// The id of another enabled `persona` monitor on `channel`, if any — a
    /// channel can carry only one persona (they share one prompt slot).
    pub async fn persona_conflict(&self, channel: &str, exclude_id: &str) -> Option<String> {
        self.defs
            .read()
            .await
            .values()
            .filter(|o| o.enabled() && o.def.monitor.id != exclude_id)
            .filter(|o| matches!(o.def.monitor.action, Action::Persona { .. }))
            .find(|o| {
                o.def
                    .monitor
                    .binding()
                    .is_some_and(|b| b.channel == channel)
            })
            .map(|o| o.def.monitor.id.clone())
    }

    pub async fn list_definitions(&self) -> Vec<MonitorSummary> {
        let mut out: Vec<_> = self
            .defs
            .read()
            .await
            .values()
            .map(|o| self.summary(o))
            .collect();
        out.sort_by(|a, b| a.id.cmp(&b.id));
        out
    }

    /// The listing entry for one monitor.
    pub async fn summary_of(&self, id: &str, owner: Option<&str>) -> Option<MonitorSummary> {
        self.get(id, owner).await.map(|o| self.summary(&o))
    }

    pub async fn list_definitions_visible(&self, listing: &DefListing) -> Vec<MonitorSummary> {
        self.list_definitions()
            .await
            .into_iter()
            .filter(|s| listing.sees(s.owner.as_deref(), s.visibility, &s.id))
            .collect()
    }

    pub async fn definition_ownership(
        &self,
        bare_id: &str,
        owner: Option<&str>,
    ) -> Option<(Option<String>, Visibility)> {
        self.defs
            .read()
            .await
            .get(&def_key(owner, bare_id))
            .map(|o| (o.owner.clone(), o.visibility))
    }

    pub fn get_definition_source(&self, id: &str, owner: Option<&str>) -> eyre::Result<String> {
        if let Some(store) = self.def_store() {
            return Self::block_on(store.get(id, KIND_MONITOR, owner))?
                .ok_or_else(|| eyre::eyre!("Monitor '{}' not found", id));
        }
        let path = self.dir.join(format!("{id}.toml"));
        std::fs::read_to_string(&path)
            .wrap_err_with(|| format!("Monitor '{}' not found at {}", id, path.display()))
    }

    /// The monitor as saved today, when there is one and it still parses. The
    /// one read behind both a write's membership gate (what is this save
    /// changing?) and its catch-up policy (was it enabled?).
    pub async fn stored_definition(
        &self,
        id: &str,
        owner: Option<&str>,
    ) -> Option<MonitorDefinition> {
        self.get_definition_source(id, owner)
            .ok()
            .and_then(|toml| MonitorDefinition::from_toml(&toml).ok())
    }

    /// Flip a monitor's `enabled` flag through the canonical save path
    /// (revision history, taint stamping, reload), returning the previous
    /// state and the updated definition. Shared by the agent tool's
    /// `set_enabled` and the engine's sweep auto-disable so there is exactly
    /// one rewrite-the-TOML code path.
    pub async fn set_definition_enabled(
        &self,
        id: &str,
        owner: Option<&str>,
        enabled: bool,
        edit: EditContext<'_>,
    ) -> eyre::Result<(bool, MonitorDefinition)> {
        let source = self.get_definition_source(id, owner)?;
        let mut def = MonitorDefinition::from_toml(&source)?;
        let was_enabled = def.monitor.enabled;
        def.monitor.enabled = enabled;
        let toml = toml::to_string(&def)?;
        let (_, visibility) = self
            .definition_ownership(id, owner)
            .await
            .ok_or_else(|| eyre::eyre!("monitor '{id}' vanished"))?;
        self.save_definition_source(id, &toml, owner, visibility, edit)
            .await?;
        Ok((was_enabled, def))
    }

    pub async fn save_definition_source(
        &self,
        id: &str,
        toml_content: &str,
        owner: Option<&str>,
        visibility: Visibility,
        edit: EditContext<'_>,
    ) -> eyre::Result<()> {
        let def = MonitorDefinition::from_toml(toml_content).wrap_err("Invalid monitor TOML")?;
        if def.monitor.id != id {
            return Err(eyre::eyre!(
                "Monitor id mismatch: TOML declares '{}' but saving as '{}'",
                def.monitor.id,
                id
            ));
        }
        self.check_watcher(&def)?;
        if let Some(store) = self.def_store() {
            let parsed = serde_json::to_value(&def).ok();
            store
                .save(
                    id,
                    KIND_MONITOR,
                    toml_content,
                    parsed,
                    owner,
                    visibility,
                    RunAsWrite::Unchanged,
                    edit,
                )
                .await?;
        } else if owner.is_some() {
            return Err(eyre::eyre!(
                "member-owned monitors require a database store; this deployment uses the filesystem"
            ));
        } else {
            if !workforce_common::config_files::is_plain_name(id) {
                return Err(eyre::eyre!("'{id}' is not a monitor id"));
            }
            std::fs::create_dir_all(&self.dir)?;
            std::fs::write(self.dir.join(format!("{id}.toml")), toml_content)?;
        }
        self.reload().await?;
        Ok(())
    }

    pub fn export_all_sources(&self) -> eyre::Result<HashMap<String, String>> {
        if let Some(store) = self.def_store() {
            let rows = Self::block_on(store.list(KIND_MONITOR))?;
            return Ok(rows.into_iter().map(|r| (r.id, r.toml_source)).collect());
        }
        let mut map = HashMap::new();
        if !self.dir.exists() {
            return Ok(map);
        }
        for entry in std::fs::read_dir(&self.dir)? {
            let path = entry?.path();
            if path.extension().is_some_and(|e| e == "toml") {
                let content = std::fs::read_to_string(&path)?;
                if let Ok(def) = MonitorDefinition::from_toml(&content) {
                    map.insert(def.monitor.id, content);
                }
            }
        }
        Ok(map)
    }

    pub async fn delete_definition(
        &self,
        id: &str,
        owner: Option<&str>,
        edit: EditContext<'_>,
    ) -> eyre::Result<()> {
        if let Some(store) = self.def_store() {
            store.delete(id, KIND_MONITOR, owner, edit).await?;
        } else if owner.is_some() {
            return Err(eyre::eyre!(
                "member-owned monitors require a database store; this deployment uses the filesystem"
            ));
        } else {
            let path = self.dir.join(format!("{id}.toml"));
            if !path.exists() {
                return Err(eyre::eyre!(
                    "Monitor '{}' not found at {}",
                    id,
                    path.display()
                ));
            }
            std::fs::remove_file(&path)
                .wrap_err_with(|| format!("Failed to delete {}", path.display()))?;
        }
        self.reload().await?;
        Ok(())
    }

    pub async fn list_revisions(
        &self,
        id: &str,
        owner: Option<&str>,
    ) -> eyre::Result<Vec<workforce_orchestrator::workflow_store::DefRevision>> {
        let store = self
            .def_store()
            .ok_or_else(|| eyre::eyre!("revision history requires the database store"))?;
        store.list_revisions(KIND_MONITOR, id, owner).await
    }

    pub async fn get_revision(
        &self,
        id: &str,
        owner: Option<&str>,
        rev: i32,
    ) -> eyre::Result<Option<workforce_orchestrator::workflow_store::DefRevision>> {
        let store = self
            .def_store()
            .ok_or_else(|| eyre::eyre!("revision history requires the database store"))?;
        store.get_revision(KIND_MONITOR, id, owner, rev).await
    }
}

#[async_trait::async_trait]
impl crate::hosts::MonitorDefinitions for MonitorRuntime {
    async fn list_definitions_visible(&self, listing: &DefListing) -> Vec<MonitorSummary> {
        MonitorRuntime::list_definitions_visible(self, listing).await
    }

    async fn stored_definition(&self, id: &str, owner: Option<&str>) -> Option<MonitorDefinition> {
        MonitorRuntime::stored_definition(self, id, owner).await
    }

    async fn check_definition(&self, def: &MonitorDefinition) -> eyre::Result<()> {
        MonitorRuntime::check_definition(self, def).await
    }

    fn get_definition_source(&self, id: &str, owner: Option<&str>) -> eyre::Result<String> {
        MonitorRuntime::get_definition_source(self, id, owner)
    }

    async fn save_definition_source(
        &self,
        id: &str,
        toml_content: &str,
        owner: Option<&str>,
        visibility: Visibility,
        edit: EditContext<'_>,
    ) -> eyre::Result<()> {
        MonitorRuntime::save_definition_source(self, id, toml_content, owner, visibility, edit)
            .await
    }

    fn export_all_sources(&self) -> eyre::Result<HashMap<String, String>> {
        MonitorRuntime::export_all_sources(self)
    }

    async fn delete_definition(
        &self,
        id: &str,
        owner: Option<&str>,
        edit: EditContext<'_>,
    ) -> eyre::Result<()> {
        MonitorRuntime::delete_definition(self, id, owner, edit).await
    }

    async fn list_revisions(
        &self,
        id: &str,
        owner: Option<&str>,
    ) -> eyre::Result<Vec<workforce_orchestrator::workflow_store::DefRevision>> {
        MonitorRuntime::list_revisions(self, id, owner).await
    }

    async fn get_revision(
        &self,
        id: &str,
        owner: Option<&str>,
        rev: i32,
    ) -> eyre::Result<Option<workforce_orchestrator::workflow_store::DefRevision>> {
        MonitorRuntime::get_revision(self, id, owner, rev).await
    }
}

#[async_trait::async_trait]
impl crate::hosts::DefinitionSurface for MonitorRuntime {
    fn def_store(&self) -> Option<&Arc<WorkflowStore>> {
        MonitorRuntime::def_store(self)
    }

    async fn definition_ownership(
        &self,
        bare_id: &str,
        owner: Option<&str>,
    ) -> Option<(Option<String>, Visibility)> {
        MonitorRuntime::definition_ownership(self, bare_id, owner).await
    }

    async fn owners(&self) -> Vec<(String, Option<String>)> {
        self.list_definitions()
            .await
            .into_iter()
            .map(|d| (d.id, d.owner))
            .collect()
    }

    async fn reload(&self) -> eyre::Result<usize> {
        MonitorRuntime::reload(self).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const MONITOR_TOML: &str = r#"
[monitor]
id = "m1"
name = "M1"
[monitor.binding]
surface = "slack"
channel = "C1"
[monitor.filter]
type = "all"
[monitor.action]
type = "notify"
target = "C2"
template = "x"
"#;

    #[tokio::test]
    async fn set_definition_enabled_flips_through_save_and_reports_previous_state() {
        let dir = std::env::temp_dir().join(format!("monrt-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("m1.toml"), MONITOR_TOML).unwrap();
        let runtime = MonitorRuntime::new(dir.clone());

        let (was_enabled, def) = runtime
            .set_definition_enabled(
                "m1",
                None,
                false,
                EditContext::new("auto_disable", Some("system")),
            )
            .await
            .unwrap();
        assert!(was_enabled, "started enabled (the default)");
        assert!(!def.monitor.enabled);
        assert!(
            !runtime.get("m1", None).await.unwrap().enabled(),
            "flip persisted through save + reload"
        );

        let (was_enabled, def) = runtime
            .set_definition_enabled("m1", None, true, EditContext::new("agent_tool", None))
            .await
            .unwrap();
        assert!(!was_enabled);
        assert!(def.monitor.enabled);
        assert!(runtime.get("m1", None).await.unwrap().enabled());

        assert!(
            runtime
                .set_definition_enabled("nope", None, true, EditContext::new("agent_tool", None))
                .await
                .is_err(),
            "unknown monitor errors"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    const CANDY_TOML: &str = r#"
[monitor]
id = "m2"
name = "M2"
agent = "candy"
[monitor.binding]
surface = "slack"
channel = "C1"
[monitor.filter]
type = "all"
[monitor.action]
type = "notify"
target = "C2"
template = "x"
"#;

    fn agent(id: &str, is_default: bool) -> EngineAgent {
        EngineAgent {
            id: id.into(),
            name: id.into(),
            is_default,
        }
    }

    #[tokio::test]
    async fn monitors_partition_across_engines_by_watching_agent() {
        let dir = std::env::temp_dir().join(format!("monrt-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("m1.toml"), MONITOR_TOML).unwrap();
        std::fs::write(dir.join("m2.toml"), CANDY_TOML).unwrap();
        let runtime = MonitorRuntime::new(dir.clone());
        let ids = |mons: Vec<Arc<OwnedMonitor>>| {
            let mut ids: Vec<String> = mons.iter().map(|m| m.def.monitor.id.clone()).collect();
            ids.sort();
            ids
        };

        let atlas = agent("atlas", true);
        let candy = agent("candy", false);
        let quill = agent("quill", false);
        assert_eq!(ids(runtime.matching(&atlas, "slack", "C1").await), ["m1"]);
        assert_eq!(ids(runtime.matching(&candy, "slack", "C1").await), ["m2"]);
        assert!(runtime.matching(&quill, "slack", "C1").await.is_empty());
        assert_eq!(ids(runtime.enabled_for(&candy).await), ["m2"]);
        assert_eq!(
            runtime.watched_channels(&quill, &["slack".into()]).await,
            Vec::<(String, String)>::new()
        );
        assert_eq!(
            runtime.watched_channels(&candy, &["slack".into()]).await,
            vec![("slack".to_string(), "C1".to_string())]
        );
        assert_eq!(
            ids(runtime.enabled().await),
            ["m1", "m2"],
            "listing is hub-wide"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn summaries_resolve_agent_and_status_and_saves_refuse_unknown_agents() {
        let dir = std::env::temp_dir().join(format!("monrt-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("m1.toml"), MONITOR_TOML).unwrap();
        std::fs::write(dir.join("m2.toml"), CANDY_TOML).unwrap();
        let runtime = MonitorRuntime::new(dir.clone());

        let before = runtime.summary_of("m1", None).await.unwrap();
        assert_eq!(before.agent, None, "no configured agents → unresolved");
        assert_eq!(before.status, MonitorStatus::Active);

        runtime.set_agents(KnownAgents {
            default_id: "atlas".into(),
            ids: vec!["atlas".into(), "candy".into()],
        });
        let m1 = runtime.summary_of("m1", None).await.unwrap();
        assert_eq!(m1.agent.as_deref(), Some("atlas"));
        let m2 = runtime.summary_of("m2", None).await.unwrap();
        assert_eq!(m2.agent.as_deref(), Some("candy"));
        assert_eq!(m2.status, MonitorStatus::Active);

        let err = runtime
            .save_definition_source(
                "m2",
                &CANDY_TOML.replace("agent = \"candy\"", "agent = \"koi\""),
                None,
                Visibility::Global,
                EditContext::new("agent_tool", None),
            )
            .await
            .unwrap_err();
        assert!(err.to_string().contains("'koi'"), "{err}");
        assert_eq!(
            runtime
                .summary_of("m2", None)
                .await
                .unwrap()
                .agent
                .as_deref(),
            Some("candy"),
            "refused save leaves the definition untouched"
        );

        runtime
            .set_definition_enabled("m2", None, false, EditContext::new("agent_tool", None))
            .await
            .unwrap();
        assert_eq!(
            runtime.summary_of("m2", None).await.unwrap().status,
            MonitorStatus::Paused
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}
