//! Minimal MCP serving shell — `McpServingAgent`, behind the `mcp-serve`
//! operator subcommand.
//!
//! `AgentService::build_for_agent` does the full agent startup (LLM provider
//! pool + failover, telemetry SQLite, scheduler + persisted job load, job
//! runner, workflow runtime, FS watchers, knowledge service startup indexing,
//! Weaviate connect with retries, etc.) and takes 20+ seconds, longer than an
//! MCP client waits for `initialize`.
//!
//! To serve an agent's tools we need only:
//!   * the agent's tool registry (code tools, knowledge tools, vault tools,
//!     plus skill tools loaded on demand from the agent's vault)
//!   * the agent's vault (for credential resolution)
//!   * a `WorkforceConfig` for `skill_loader::load_skill_from_vault`
//!
//! Everything else (LLM, scheduler, telemetry, workflows) is dead weight in a
//! JSON-RPC helper.

use std::path::PathBuf;
use std::sync::Arc;

use eyre::Result;
use tracing::{error, info, warn};

use workforce_agent::runner::ToolSecrets;
use workforce_agent::tools::registry::ToolRegistry;
use workforce_common::identity::AgentIdentity;
use workforce_common::settings::{AgentConfig, WorkforceConfig};
use workforce_core::types::AgentId;
use workforce_memory::in_memory::InMemoryStore;
use workforce_memory::store::MemoryStore;

use crate::memory_tool::MemoryTool;
use crate::notes_tool::NotesTool;
use crate::skill_state::SkillStateTracker;
use crate::vault::Vault;

/// Memory backend the helper mirrors from the agent's production wiring.
/// Pure decision so the registration policy (`memory` only over a real
/// store) is unit-testable without constructing live stores.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum MemoryBackend {
    /// Weaviate's or vellev's.
    Persistent,
    Ephemeral,
}

/// `Err` for a `[memory] backend` no memory store can be built for
/// ("shadow" until it is built, or an unknown value), which would
/// otherwise fall to the in-memory store when Weaviate is off.
fn memory_backend(config: &WorkforceConfig) -> eyre::Result<MemoryBackend> {
    config.check_memory_backend()?;
    Ok(if config.weaviate.enabled || config.memory.is_vellev() {
        MemoryBackend::Persistent
    } else {
        MemoryBackend::Ephemeral
    })
}

/// Minimal serving shell exposing an agent's tool registry over MCP.
///
/// Constructed by `workforce mcp-serve`; lives as long as that command.
/// Drops are cheap; nothing here owns long-running tasks.
pub struct McpServingAgent {
    pub agent_id: AgentId,
    pub tools: Arc<ToolRegistry>,
    pub vault: Option<Arc<Vault>>,
    /// Hub global vault, layered under the per-agent vault so per-user creds
    /// (`users/{uid}/...`, tier 2) resolve. See [`crate::vault::open_global`].
    pub global_vault: Option<Arc<Vault>>,
    pub config: WorkforceConfig,
    pub skill_state: Arc<SkillStateTracker>,
}

impl McpServingAgent {
    /// Build the minimal serving agent.
    ///
    /// Mirrors the relevant slice of `AgentService::build_inner` for tool
    /// registration, but skips every async/long-running side effect that
    /// the full agent constructor performs.
    pub async fn build(config: WorkforceConfig, agent_config: &AgentConfig) -> Result<Self> {
        let agent_id = AgentId::new(&agent_config.id);

        let workspace = crate::projects::layout::agent_workspace(
            std::path::Path::new(&config.hub.data_dir),
            agent_id.as_str(),
            agent_config.workspace.as_deref(),
        );
        std::fs::create_dir_all(&workspace)?;
        AgentIdentity::bootstrap_defaults(&workspace)?;
        let workspace = workspace.canonicalize()?;

        info!(
            agent = %agent_id,
            workspace = %workspace.display(),
            "MCP serving agent: workspace resolved"
        );

        let vault = crate::vault::open_agent(
            &config,
            &agent_config.id,
            agent_config.vault.as_ref(),
            &workspace,
        )
        .await?;
        // Layer the hub's global vault under the per-agent vault so a user's
        // per-user creds (`users/{uid}/...`, tier 2) resolve here exactly as in
        // the full hub agent; without it coding.session sees no per-user skills.
        let global_vault = crate::vault::open_global(&config).await?;
        info!(agent = %agent_id, global_vault = global_vault.is_some(), "MCP serving agent: vaults opened");

        let tool_secrets = ToolSecrets {
            brave_api_key: Some(crate::credentials::brave_source(
                vault.clone(),
                global_vault.clone(),
            )),
            ..Default::default()
        };

        let worktrees_dir = crate::code::worktree::worktrees_dir(
            std::path::Path::new(&config.hub.data_dir),
            config.hub.worktree_root_path(),
        );
        let allowed_paths: Vec<PathBuf> = [workspace.clone(), worktrees_dir]
            .into_iter()
            .chain(crate::projects::layout::WorkRoots::from_hub(&config.hub).hot)
            .collect();
        let tools = Arc::new(workforce_agent::runner::default_tools(
            allowed_paths.clone(),
            tool_secrets,
        ));

        register_code_tools(&tools, &allowed_paths);

        let workspace_config = workforce_common::settings::AgentWorkspaceConfig::load(&workspace)
            .ok()
            .flatten();

        // Mirror the agent's production memory wiring (serve.rs memory
        // slice). `memory` is registered only over the real store: recall
        // over an empty ephemeral store answers "no memories" confidently,
        // which reads as agent amnesia from inside a Claude Code session —
        // absent and honestly reported (MCP `instructions`) beats present
        // and lying. Stricter than the agent loop's fall-back-to-empty on
        // Weaviate outage: here a down store surfaces as a tool error.
        let backend = memory_backend(&config)?;
        let memory_store: Arc<dyn MemoryStore> = match backend {
            MemoryBackend::Persistent => {
                crate::embeddings::switched_memory_store(
                    &config,
                    crate::embeddings::base_store(&config)?,
                )
                .await?
            }
            MemoryBackend::Ephemeral => Arc::new(InMemoryStore::new()),
        };
        let sessions = crate::session_store_factory::build_session_store(
            &config,
            &workspace,
            agent_id.as_str(),
        )
        .await?;
        let effective_memory = agent_config.effective_memory(&config.memory);
        if backend == MemoryBackend::Persistent {
            let decay_rate = workspace_config
                .as_ref()
                .and_then(|ws| ws.temporal.as_ref())
                .map_or(0.005, |t| t.decay_rate);
            let mut memory_tool = MemoryTool::new(
                Arc::clone(&memory_store),
                agent_id.clone(),
                workspace.clone(),
            )
            .with_sessions(Arc::clone(&sessions))
            .with_decay_rate(decay_rate)
            .with_min_certainty(effective_memory.recall_min_certainty);
            if agent_config.owner_type == workforce_common::settings::OwnerType::Team {
                if let Some(ref tid) = agent_config.owner_id {
                    memory_tool = memory_tool.with_team_id(tid);
                }
            }
            tools.register(Arc::new(memory_tool));
        }
        tools.register(Arc::new(
            NotesTool::with_store(
                workspace.clone(),
                Arc::clone(&memory_store),
                agent_id.clone(),
            )
            .with_sessions(Arc::clone(&sessions))
            .with_recall_scope(effective_memory.agent_recall_scope),
        ));

        if let Some(ks) = crate::knowledge::create_service(
            &config.knowledge,
            crate::knowledge::VectorBackend::of(&config),
            &workspace.to_string_lossy(),
            &config.codegraph,
            config.postgres.as_ref(),
            crate::knowledge::HubSummariserBuilder::new(config.knowledge.summariser.clone())
                .with_global_vault(global_vault.clone())
                .with_plans(global_vault.clone().map(|vault| {
                    crate::subscription::PlanLogins::new(
                        crate::subscription::Subscriptions::new(&config),
                        crate::subscription::CredentialScope::system(vault, &config.subscription),
                    )
                })),
            // The hub keeps the code vectors; this process serves a session.
            crate::knowledge::VectorUpkeep::Left,
        ) {
            crate::knowledge::register_tools(&tools, &ks, &workspace);
            // A helper: resolve once (tier only affects model
            // names, not recall scope), no hot-reload needed here.
            let resolved = crate::resolved_config::ConfigCell::new(
                crate::resolved_config::ResolvedConfig::resolve(
                    Some(agent_config),
                    workspace_config.as_ref(),
                    &config,
                    "anthropic",
                    None,
                ),
            );
            tools.register(Arc::new(crate::context_fuse_tool::ContextFuseTool::new(
                Arc::clone(&ks),
                Arc::clone(&memory_store),
                workspace.clone(),
                agent_id.clone(),
                resolved,
            )));
            info!("Knowledge tools + context.fuse registered");
        }

        match crate::store::build_document_store(&config).await {
            Ok(store) => {
                crate::store_tool::register(
                    &tools,
                    Arc::clone(&store),
                    crate::authz::always_admin(),
                );
                crate::pipeline_tool::register(&tools, store, crate::authz::always_admin());
            }
            Err(e) => warn!(error = %e, "Document store unavailable in MCP serving"),
        }
        if let Some(ref v) = vault {
            let concrete = Arc::new(crate::user_config::LayeredUserVault::new(
                Arc::clone(v),
                global_vault.clone(),
            ));
            // `mcp-serve` runs without the hub's agent registry: a ChatGPT
            // sign-in is checked against this agent's vault and the global
            // one only.
            let logins = crate::subscription::store::LoginWrites::new(
                Arc::clone(&concrete),
                crate::subscription::store::known_vaults(
                    Some(Arc::clone(v)),
                    global_vault.clone(),
                    Arc::default(),
                ),
                crate::vault_tool::always_admin(),
            );
            let layered: Arc<dyn workforce_common::settings::SecretStore> = concrete;
            tools.register(Arc::new(crate::vault_tool::VaultGetTool::new(Arc::clone(
                &layered,
            ))));
            tools.register(Arc::new(
                crate::vault_tool::VaultSetTool::new(Arc::clone(&layered))
                    .with_admin_policy(crate::vault_tool::always_admin())
                    .with_logins(logins.clone()),
            ));
            tools.register(Arc::new(crate::vault_tool::VaultListTool::new(Arc::clone(
                &layered,
            ))));
            tools.register(Arc::new(
                crate::vault_tool::VaultDeleteTool::new(Arc::clone(&layered))
                    .with_logins(logins.clone()),
            ));
            tools.register(Arc::new(crate::vault_tool::VaultInspectTool::new(
                Arc::clone(&layered),
            )));
            tools.register(Arc::new(crate::vault_tool::VaultMyCredentialsTool::new(
                Arc::clone(&layered),
                crate::vault_tool::no_owner_resolver(),
            )));
            tools.register(Arc::new(
                crate::vault_tool::VaultMoveTool::new(
                    Arc::clone(&layered),
                    crate::vault_tool::always_admin(),
                )
                .with_logins(logins),
            ));
            for tool in
                crate::github_local::github_local_tools(crate::github_local::GithubLocalContext {
                    vault: Arc::clone(v),
                    global_vault: global_vault.clone(),
                    workspace: workspace.clone(),
                })
            {
                tools.register(tool);
            }
        }

        Ok(Self {
            agent_id,
            tools,
            vault,
            global_vault,
            config,
            skill_state: Arc::new(SkillStateTracker::new()),
        })
    }

    /// Preload the named skills from the agent's vault into the tool registry,
    /// returning `(skill, reason)` for each one that failed. Silent partial
    /// loads were the cause of "Tool not found" errors that looked like
    /// deferral bugs but were really missing/expired vault credentials.
    ///
    /// The reason travels with the name because this process logs to its own
    /// stderr (stdout is the JSON-RPC channel) and nothing collects it — a
    /// caller that only knows *which* skills died can say no more than "check
    /// the hub logs", where the reason provably is not.
    ///
    /// CONTRACT: the reason is surfaced verbatim to the model (and thence the
    /// user) via the MCP `instructions` field. Loader errors must therefore
    /// name vault *paths* and placeholders, never credential values.
    pub fn preload_skills(&self, skills: &[String]) -> Vec<(String, String)> {
        let Some(ref vault) = self.vault else {
            if !skills.is_empty() {
                warn!(
                    skills = ?skills,
                    "Preload skills requested but agent has no vault; skipping"
                );
                return skills
                    .iter()
                    .map(|s| (s.clone(), "agent has no vault configured".to_string()))
                    .collect();
            }
            return Vec::new();
        };
        let mut failed = Vec::new();
        for skill in skills {
            // Pass the global vault so per-user creds (tier 2) resolve, not just
            // the agent vault — a global-only user otherwise gets no tools.
            match crate::skill_loader::load_and_track(
                skill,
                Arc::clone(vault),
                self.global_vault.clone(),
                &self.config,
                &self.tools,
                &self.skill_state,
                "mcp-serve",
            ) {
                Ok(result) => info!(
                    skill = %skill,
                    tools = result.tool_names.len(),
                    "Preloaded skill"
                ),
                Err(e) => {
                    error!(skill = %skill, error = %e, "Failed to preload skill — its tools will return METHOD_NOT_FOUND");
                    failed.push((skill.clone(), e.to_string()));
                }
            }
        }
        failed
    }
}

fn register_code_tools(tools: &ToolRegistry, allowed_paths: &[PathBuf]) {
    use crate::code::{
        CodeBuildTool, CodeCargoTool, CodeDiffTool, CodeEditTool, CodeFindReplaceTool,
        CodeFindTool, CodeGitTool, CodeLintTool, CodeReadTool, CodeSearchTool, CodeSymbolsTool,
        CodeTestTool, CodeTreeTool,
    };
    let p = allowed_paths.to_vec();
    tools.register(Arc::new(CodeReadTool::new(p.clone())));
    tools.register(Arc::new(CodeEditTool::new(p.clone())));
    tools.register(Arc::new(CodeSearchTool::new(p.clone())));
    tools.register(Arc::new(CodeTreeTool::new(p.clone())));
    tools.register(Arc::new(CodeSymbolsTool::new(p.clone())));
    tools.register(Arc::new(CodeDiffTool::new(p.clone())));
    tools.register(Arc::new(CodeTestTool::new(p.clone())));
    tools.register(Arc::new(CodeLintTool::new(p.clone())));
    tools.register(Arc::new(CodeBuildTool::new(p.clone())));
    tools.register(Arc::new(CodeGitTool::new(p.clone())));
    tools.register(Arc::new(CodeFindReplaceTool::new(p.clone())));
    tools.register(Arc::new(CodeFindTool::new(p.clone())));
    tools.register(Arc::new(CodeCargoTool::new(p)));
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config_in(dir: &std::path::Path, weaviate_enabled: bool) -> WorkforceConfig {
        let mut config = WorkforceConfig::default();
        config.hub.data_dir = dir.to_string_lossy().into_owned();
        config.memory.db_path = std::path::Path::new(&config.hub.data_dir)
            .join("memory.db")
            .to_string_lossy()
            .into_owned();
        config.weaviate.enabled = weaviate_enabled;
        config
    }

    fn agent_config() -> AgentConfig {
        AgentConfig {
            id: "test-agent".to_string(),
            ..Default::default()
        }
    }

    #[test]
    fn memory_backend_keys_off_weaviate_enabled_or_vellev() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(
            memory_backend(&config_in(dir.path(), true)).unwrap(),
            MemoryBackend::Persistent
        );
        assert_eq!(
            memory_backend(&config_in(dir.path(), false)).unwrap(),
            MemoryBackend::Ephemeral
        );
        let mut vellev = config_in(dir.path(), false);
        vellev.memory.backend = "vellev".into();
        assert_eq!(memory_backend(&vellev).unwrap(), MemoryBackend::Persistent);
    }

    /// A memory backend no store can be built for is refused by name, with
    /// Weaviate on or off: never served from the in-memory store instead.
    #[test]
    fn mcp_serve_refuses_a_memory_backend_it_cannot_use() {
        let dir = tempfile::tempdir().unwrap();
        for weaviate in [false, true] {
            for (backend, says) in [
                ("shadow", "[memory] backend = \"shadow\""),
                ("mongodb", "[memory] backend must be one of"),
            ] {
                let mut config = config_in(dir.path(), weaviate);
                config.memory.backend = backend.into();
                let refused = memory_backend(&config)
                    .err()
                    .unwrap_or_else(|| panic!("{backend} with weaviate {weaviate} was served"))
                    .to_string();
                assert!(refused.contains(says), "{backend}: {refused}");
            }
        }
    }

    /// A process serving one session leaves the code vectors to the hub:
    /// its knowledge service fills nothing in the background, where the
    /// hub's does.
    #[tokio::test]
    async fn the_code_vectors_are_kept_by_the_hub_and_not_by_a_session() {
        let dir = tempfile::tempdir().unwrap();
        let mut config = config_in(dir.path(), false);
        config.knowledge.enabled = true;
        config.knowledge.vector_backend = "vellev".into();
        config.knowledge.root = Some(dir.path().to_string_lossy().to_string());
        config.vellev.url = "http://127.0.0.1:9".into();
        config.vellev.api_key = Some("key".into());
        config.vellev.tenant = "workforce".into();
        let made_under = |root: &std::path::Path| {
            let root = root.to_path_buf();
            crate::knowledge::services()
                .into_iter()
                .find(move |service| service.workspace() == root)
        };

        let _session = McpServingAgent::build(config.clone(), &agent_config())
            .await
            .expect("a session's tools");
        let sessions = made_under(dir.path()).expect("the session's knowledge service");
        assert!(sessions.vector_store().is_some(), "it searches the vectors");
        assert!(
            !sessions.keeps_vectors(),
            "and leaves keeping them to the hub"
        );

        let hub_dir = tempfile::tempdir().unwrap();
        config.knowledge.root = Some(hub_dir.path().to_string_lossy().to_string());
        let hubs = crate::knowledge::create_service(
            &config.knowledge,
            crate::knowledge::VectorBackend::of(&config),
            &config.hub.data_dir,
            &config.codegraph,
            None,
            crate::knowledge::HubSummariserBuilder::new(config.knowledge.summariser.clone()),
            crate::knowledge::VectorUpkeep::Kept,
        )
        .expect("the hub's knowledge service");
        assert!(hubs.keeps_vectors());
    }

    /// `memory` must only be served when backed by the real store — an
    /// ephemeral store recalls nothing and reads as agent amnesia from
    /// inside a Claude Code session. `notes` is file-backed and serves
    /// regardless.
    #[tokio::test]
    async fn memory_tool_registered_only_with_real_store() {
        let dir = tempfile::tempdir().unwrap();
        let without = McpServingAgent::build(config_in(dir.path(), false), &agent_config())
            .await
            .expect("build without weaviate");
        assert!(without.tools.get("memory").is_none());
        assert!(without.tools.get("notes").is_some());

        let dir = tempfile::tempdir().unwrap();
        let with = McpServingAgent::build(config_in(dir.path(), true), &agent_config())
            .await
            .expect("build with weaviate");
        assert!(with.tools.get("memory").is_some());
        assert!(with.tools.get("notes").is_some());
    }
}
