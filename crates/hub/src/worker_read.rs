//! `read_source.*` — lets a read-only Worker sub-agent acquire a data source's
//! read-only tools on demand, on top of its frozen base set.
//!
//! A Worker ([`crate::job_runner::AgentProfile::Worker`]) runs on a cheap model
//! with a detached, read-only snapshot of the tool registry
//! ([`crate::serve::AgentService::worker_tool_registry`]). It has no
//! `system.enable_skill` (that would let it load write tools), so when it needs
//! to read from another system mid-task it would otherwise be stranded.
//!
//! These two tools close that gap WITHOUT weakening the read-only invariant:
//!
//! * Read-only is enforced at the *enable boundary* — only tool names on an
//!   explicit, per-source read allowlist (NEVER a `prefix.*` glob; the surfaces
//!   mix reads and writes under one prefix) are ever materialised into the
//!   worker snapshot.
//! * The snapshot is the hard boundary: a tool that is not in it is uncallable.
//! * For already-registered sources (AWS, Slack) the read tools are *copied*
//!   from the global registry. For vault skills not yet loaded (GitHub, Notion)
//!   the skill is loaded from the vault directly into the worker snapshot and
//!   every non-read tool is immediately dropped — nothing reaches the global
//!   registry or the parent.
//!
//! Because workers are `PRECURATED_TOOLSET` (the catalogue is bypassed — every
//! snapshot tool is sent inline), acquired read tools appear inline on the very
//! next round: a single `read_source.enable` call, no load step.

use std::collections::HashSet;
use std::sync::Arc;
use std::time::Instant;

use async_trait::async_trait;
use eyre::Result;
use serde::Deserialize;
use serde_json::{json, Value};

use workforce_agent::tools::registry::ToolRegistry;
use workforce_agent::tools::{Tool, ToolResult};
use workforce_common::settings::WorkforceConfig;
use workforce_policy::rules::{PolicyRule, PolicyScope, RuleAction};

use crate::vault::Vault;

/// How a source's read tools are materialised into the worker snapshot.
enum LoadStrategy {
    /// The tools are already registered in the global registry whenever the
    /// source is configured/active (AWS once `[aws_*]` is ready, Slack once
    /// `SlackManager` registers its agent tools). Copy them into the snapshot.
    CopyFromGlobal,
    /// A vault-backed builtin skill that may not be loaded yet. Load it from the
    /// vault directly into the worker snapshot, then drop every non-read tool.
    VaultSkill {
        /// `system.enable_skill` skill name to load from the vault.
        skill: &'static str,
    },
}

/// A source a Worker can acquire read-only tools from.
struct ReadSource {
    /// Menu name the worker passes to `read_source.enable`.
    name: &'static str,
    /// One-line description for `read_source.list`.
    description: &'static str,
    /// Explicit read-only tool names offered to workers. NEVER a prefix glob.
    read_tools: Vec<&'static str>,
    strategy: LoadStrategy,
}

/// The catalogue of sources a Worker may acquire read tools from. Each
/// `read_tools` list is a source's curated, audited read surface (see the
/// per-adapter `READ_TOOLS` constants / `crate::aws::read_tool_names`).
fn catalog() -> Vec<ReadSource> {
    vec![
        ReadSource {
            name: "aws",
            description: "AWS read-only: list/read S3 objects (allowlisted buckets) and \
                          describe ECS/ECR/RDS/ELB + deployment status",
            read_tools: crate::aws::read_tool_names(),
            strategy: LoadStrategy::CopyFromGlobal,
        },
        ReadSource {
            name: "slack",
            description: "Slack read-only: search messages, read thread replies, list \
                          channels/users/members, look up users, list files",
            read_tools: workforce_skills::adapters::slack::AGENT_READ_TOOLS.to_vec(),
            strategy: LoadStrategy::CopyFromGlobal,
        },
        ReadSource {
            name: "github",
            description: "GitHub read-only: list/read repos, issues, PRs and files; \
                          search code/commits; CI and workflow logs",
            read_tools: workforce_skills::adapters::github::READ_TOOLS.to_vec(),
            strategy: LoadStrategy::VaultSkill { skill: "github" },
        },
        ReadSource {
            name: "notion",
            description: "Notion read-only: search, read pages/databases/blocks, \
                          query databases, list users and comments",
            read_tools: workforce_skills::adapters::notion::READ_TOOLS.to_vec(),
            strategy: LoadStrategy::VaultSkill { skill: "notion" },
        },
        ReadSource {
            name: "google",
            description: "Google Drive/Docs/Sheets read-only: list/search/read Drive files, \
                          read Docs, read Sheets values/metadata (no Gmail or Contacts)",
            read_tools: workforce_skills::adapters::google::WORKER_READ_TOOLS.to_vec(),
            strategy: LoadStrategy::VaultSkill { skill: "google" },
        },
    ]
}

/// Copy a source's read tools that are present in `global` into `snapshot`,
/// returning the names copied. Only the source's explicit read names are ever
/// considered, so no write tool can be copied.
fn copy_reads_from_global(
    snapshot: &ToolRegistry,
    global: &ToolRegistry,
    read_tools: &[&str],
) -> Vec<String> {
    let mut acquired = Vec::new();
    for name in read_tools {
        if let Some(tool) = global.get(name) {
            snapshot.register(tool);
            acquired.push((*name).to_string());
        }
    }
    acquired
}

/// After a vault skill has been loaded into `snapshot`, drop every tool that is
/// not on the read allowlist and return the read tools kept. This is the
/// enable-boundary that keeps a loaded skill's write tools out of the worker.
fn prune_to_reads(snapshot: &ToolRegistry, loaded: &[String], read: &HashSet<&str>) -> Vec<String> {
    let mut kept = Vec::new();
    for name in loaded {
        if read.contains(name.as_str()) {
            kept.push(name.clone());
        } else {
            snapshot.unregister(name);
        }
    }
    kept
}

/// Constitutional `Allow` rules for the worker read-source tools. Mirrors the
/// pattern used by `crate::aws::default_policies` so the tools are permitted
/// regardless of an install's base policy (the engine default-denies unmatched
/// tools). They are only ever registered into a worker snapshot, so a full
/// agent never sees them.
pub fn default_policies() -> Vec<PolicyRule> {
    ["read_source.list", "read_source.enable"]
        .iter()
        .map(|tool| PolicyRule {
            name: format!("{tool}-allow-default"),
            scope: PolicyScope::Global,
            tool: (*tool).to_string(),
            action: RuleAction::Allow,
            conditions: Vec::new(),
            priority: 10,
            constitutional: true,
        })
        .collect()
}

/// `read_source.list` — shows the worker which read sources it can acquire,
/// computed live against the current registry/vault so it never goes stale.
pub struct ReadSourceListTool {
    snapshot: Arc<ToolRegistry>,
    global: Arc<ToolRegistry>,
    vault: Option<Arc<Vault>>,
    global_vault: Option<Arc<Vault>>,
}

impl ReadSourceListTool {
    pub fn new(
        snapshot: Arc<ToolRegistry>,
        global: Arc<ToolRegistry>,
        vault: Option<Arc<Vault>>,
        global_vault: Option<Arc<Vault>>,
    ) -> Self {
        Self {
            snapshot,
            global,
            vault,
            global_vault,
        }
    }
}

#[async_trait]
impl Tool for ReadSourceListTool {
    fn name(&self) -> &str {
        "read_source.list"
    }

    fn description(&self) -> &str {
        "List the read-only data sources you can pull in on demand (e.g. AWS, Slack, GitHub, \
         Notion). Shows whether each is available in this workspace and whether you have already \
         loaded it. Use `read_source.enable` to load one."
    }

    fn input_schema(&self) -> Value {
        json!({ "type": "object", "properties": {} })
    }

    async fn execute(&self, _input: Value) -> Result<ToolResult> {
        let start = Instant::now();
        let configured = crate::skill_loader::agent_configured_builtin_skills(
            self.vault.as_ref(),
            self.global_vault.as_ref(),
        );
        let sources: Vec<Value> = catalog()
            .iter()
            .map(|s| {
                let available = match &s.strategy {
                    LoadStrategy::CopyFromGlobal => {
                        s.read_tools.iter().any(|n| self.global.get(n).is_some())
                    }
                    LoadStrategy::VaultSkill { skill } => {
                        configured.iter().any(|c| c == skill)
                            || s.read_tools.iter().any(|n| self.global.get(n).is_some())
                    }
                };
                let loaded = s.read_tools.iter().any(|n| self.snapshot.get(n).is_some());
                json!({
                    "name": s.name,
                    "description": s.description,
                    "available": available,
                    "loaded": loaded,
                    "read_tools": s.read_tools.len(),
                })
            })
            .collect();
        Ok(ToolResult::success(
            json!({ "sources": sources }),
            start.elapsed().as_millis() as u64,
        ))
    }
}

#[derive(Deserialize)]
struct EnableInput {
    source: String,
}

/// `read_source.enable` — materialises a source's read-only tools into the
/// worker snapshot.
pub struct ReadSourceEnableTool {
    snapshot: Arc<ToolRegistry>,
    global: Arc<ToolRegistry>,
    vault: Option<Arc<Vault>>,
    global_vault: Option<Arc<Vault>>,
    config: Arc<WorkforceConfig>,
}

impl ReadSourceEnableTool {
    pub fn new(
        snapshot: Arc<ToolRegistry>,
        global: Arc<ToolRegistry>,
        vault: Option<Arc<Vault>>,
        global_vault: Option<Arc<Vault>>,
        config: Arc<WorkforceConfig>,
    ) -> Self {
        Self {
            snapshot,
            global,
            vault,
            global_vault,
            config,
        }
    }

    /// Materialise `source`'s read tools into the snapshot, returning the names
    /// now available, or an `Err` message for the model.
    fn enable(&self, source: &ReadSource) -> std::result::Result<Vec<String>, String> {
        // Already-registered read tools are copied for every strategy — this
        // covers AWS/Slack and a vault skill a full agent already loaded.
        let copied = copy_reads_from_global(&self.snapshot, &self.global, &source.read_tools);
        if !copied.is_empty() {
            return Ok(copied);
        }
        match &source.strategy {
            LoadStrategy::CopyFromGlobal => Err(format!(
                "'{}' is not available in this workspace — its tools are not active here.",
                source.name
            )),
            LoadStrategy::VaultSkill { skill } => {
                let vault = self
                    .vault
                    .clone()
                    .ok_or_else(|| "no vault is configured for this agent".to_string())?;
                let loaded = crate::skill_loader::load_skill_from_vault(
                    skill,
                    vault,
                    self.global_vault.clone(),
                    &self.config,
                    &self.snapshot,
                )
                .map_err(|e| {
                    format!(
                        "could not load '{}' (is it configured in the vault?): {e}",
                        source.name
                    )
                })?;
                let read: HashSet<&str> = source.read_tools.iter().copied().collect();
                let kept = prune_to_reads(&self.snapshot, &loaded.tool_names, &read);
                if kept.is_empty() {
                    Err(format!(
                        "'{}' loaded but exposed no read tools.",
                        source.name
                    ))
                } else {
                    Ok(kept)
                }
            }
        }
    }
}

#[async_trait]
impl Tool for ReadSourceEnableTool {
    fn name(&self) -> &str {
        "read_source.enable"
    }

    fn description(&self) -> &str {
        "Load a data source's read-only tools so you can read from it. Pass \
         {\"source\": \"<name>\"} (see `read_source.list`). The tools become callable on your next \
         step. Read-only only — you cannot post, send, or modify anything."
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "source": {
                    "type": "string",
                    "description": "Source name from `read_source.list` (e.g. 'aws', 'slack', 'github', 'notion')."
                }
            },
            "required": ["source"]
        })
    }

    async fn execute(&self, input: Value) -> Result<ToolResult> {
        let start = Instant::now();
        let elapsed = || start.elapsed().as_millis() as u64;
        let params: EnableInput = match serde_json::from_value(input) {
            Ok(p) => p,
            Err(e) => {
                return Ok(ToolResult::failure(
                    format!("Invalid input: {e}"),
                    elapsed(),
                ))
            }
        };
        let cat = catalog();
        let Some(source) = cat.iter().find(|s| s.name == params.source) else {
            let names: Vec<&str> = cat.iter().map(|s| s.name).collect();
            return Ok(ToolResult::failure(
                format!(
                    "Unknown source '{}'. Available: {}.",
                    params.source,
                    names.join(", ")
                ),
                elapsed(),
            ));
        };
        match self.enable(source) {
            Ok(acquired) => Ok(ToolResult::success(
                json!({
                    "source": source.name,
                    "tools_loaded": acquired.len(),
                    "tool_names": acquired,
                }),
                elapsed(),
            )),
            Err(e) => Ok(ToolResult::failure(e, elapsed())),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct DummyTool(&'static str);

    #[async_trait]
    impl Tool for DummyTool {
        fn name(&self) -> &str {
            self.0
        }
        fn description(&self) -> &str {
            "dummy"
        }
        fn input_schema(&self) -> Value {
            json!({ "type": "object", "properties": {} })
        }
        async fn execute(&self, _input: Value) -> Result<ToolResult> {
            Ok(ToolResult::success(json!({}), 0))
        }
    }

    fn enable_tool(snapshot: Arc<ToolRegistry>, global: Arc<ToolRegistry>) -> ReadSourceEnableTool {
        ReadSourceEnableTool::new(
            snapshot,
            global,
            None,
            None,
            Arc::new(WorkforceConfig::default()),
        )
    }

    #[test]
    fn catalog_aws_read_set_is_the_full_aws_surface() {
        let cat = catalog();
        let aws = cat.iter().find(|s| s.name == "aws").unwrap();
        assert_eq!(aws.read_tools, crate::aws::read_tool_names());
        assert!(aws.read_tools.contains(&"aws.s3.get"));
    }

    #[test]
    fn copy_from_global_takes_only_listed_reads() {
        let global = ToolRegistry::new();
        global.register(Arc::new(DummyTool("aws.s3.list")));
        global.register(Arc::new(DummyTool("aws.s3.get")));
        // A write-shaped tool under the same prefix must never be copied.
        global.register(Arc::new(DummyTool("aws.s3.delete")));
        global.register(Arc::new(DummyTool("filesystem.read")));
        let snapshot = Arc::new(ToolRegistry::new());

        let tool = enable_tool(Arc::clone(&snapshot), Arc::new(global));
        let cat = catalog();
        let aws = cat.iter().find(|s| s.name == "aws").unwrap();
        let acquired = tool.enable(aws).unwrap();

        assert!(acquired.contains(&"aws.s3.list".to_string()));
        assert!(acquired.contains(&"aws.s3.get".to_string()));
        assert!(snapshot.get("aws.s3.list").is_some());
        assert!(snapshot.get("aws.s3.get").is_some());
        assert!(snapshot.get("aws.s3.delete").is_none());
        assert!(snapshot.get("filesystem.read").is_none());
    }

    #[test]
    fn enable_is_idempotent() {
        let global = ToolRegistry::new();
        global.register(Arc::new(DummyTool("aws.s3.list")));
        let snapshot = Arc::new(ToolRegistry::new());
        let tool = enable_tool(Arc::clone(&snapshot), Arc::new(global));
        let cat = catalog();
        let aws = cat.iter().find(|s| s.name == "aws").unwrap();

        let first = tool.enable(aws).unwrap();
        let second = tool.enable(aws).unwrap();
        assert_eq!(first, second);
    }

    #[test]
    fn copy_from_global_unavailable_is_error() {
        let global = Arc::new(ToolRegistry::new());
        let snapshot = Arc::new(ToolRegistry::new());
        let tool = enable_tool(snapshot, global);
        let cat = catalog();
        let aws = cat.iter().find(|s| s.name == "aws").unwrap();
        assert!(tool.enable(aws).is_err());
    }

    #[tokio::test]
    async fn enable_unknown_source_fails_with_menu() {
        let tool = enable_tool(Arc::new(ToolRegistry::new()), Arc::new(ToolRegistry::new()));
        let res = tool
            .execute(json!({ "source": "definitely-not-a-source" }))
            .await
            .unwrap();
        assert!(!res.success);
        assert!(res.error.unwrap().contains("aws"));
    }

    #[test]
    fn prune_to_reads_drops_non_reads() {
        let snapshot = ToolRegistry::new();
        snapshot.register(Arc::new(DummyTool("notion.search")));
        snapshot.register(Arc::new(DummyTool("notion.page_create")));
        let loaded = vec![
            "notion.search".to_string(),
            "notion.page_create".to_string(),
        ];
        let read: HashSet<&str> = ["notion.search"].into_iter().collect();

        let kept = prune_to_reads(&snapshot, &loaded, &read);
        assert_eq!(kept, vec!["notion.search".to_string()]);
        assert!(snapshot.get("notion.search").is_some());
        assert!(snapshot.get("notion.page_create").is_none());
    }

    #[test]
    fn vault_skill_read_sets_exclude_known_writes() {
        let cat = catalog();
        let github = cat.iter().find(|s| s.name == "github").unwrap();
        for write in workforce_skills::adapters::github::WRITE_TOOLS {
            assert!(
                !github.read_tools.contains(write),
                "{write} must not be in the github worker read set"
            );
        }
        let notion = cat.iter().find(|s| s.name == "notion").unwrap();
        for write in workforce_skills::adapters::notion::WRITE_TOOLS {
            assert!(
                !notion.read_tools.contains(write),
                "{write} must not be in the notion worker read set"
            );
        }
    }
}
