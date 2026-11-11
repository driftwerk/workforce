//! Which connectors' tools reach an agent, on a hub with `[desk]`: what the
//! agent's own `[skills]` turns on (`enabled`), and of those, the ones that
//! only read (`read_only`). An agent whose `[skills]` names no `enabled`
//! keeps every connector, as before the key existed; every other hub never
//! builds this.
//!
//! A connector is one Desk lists ([`CONNECTORS`](workforce_common::connector_binding::CONNECTORS)),
//! matched by its adapter's `tool_prefix`; every other built-in adapter
//! (`browser`, `xero`, …) is not chosen per agent and always reaches it.
//! Its tools are reads or writes by [`is_read`](workforce_common::connector_binding::is_read): a tool this can't place is
//! a write, so a read-only connector hides it.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::SystemTime;

use parking_lot::Mutex;
use workforce_agent::tools::access::{ToolAccess, ToolAccessSnapshot};
use workforce_agent::tools::registry::ToolRegistry;
use workforce_common::connector_binding::{connector_of, is_read, ConnectorBinding};
use workforce_common::settings::AgentWorkspaceConfig;

/// What `binding` hides of `names`, and why, as the agent is told.
pub fn hidden(
    binding: &ConnectorBinding,
    names: impl IntoIterator<Item = String>,
) -> ToolAccessSnapshot {
    ToolAccessSnapshot::new(
        names
            .into_iter()
            .filter_map(|name| {
                let connector = connector_of(&name)?;
                let reason = if !binding.is_on(connector) {
                    format!(
                        "{connector} is off for this agent. The person turns it on in the agent's Connectors."
                    )
                } else if binding.reads_only(connector) && !is_read(&name) {
                    format!(
                        "This agent only reads {connector}. The person allows changes in the agent's Connectors."
                    )
                } else {
                    return None;
                };
                Some((name, reason))
            })
            .collect(),
    )
}

/// The policy for one agent: its `[skills]`, read again when its
/// `config.toml` changes.
pub struct ConnectorToolAccess {
    workspace: PathBuf,
    tools: Arc<ToolRegistry>,
    read: Mutex<Option<(Option<SystemTime>, Result<ConnectorBinding, String>)>>,
}

impl ConnectorToolAccess {
    pub fn policy(workspace: PathBuf, tools: Arc<ToolRegistry>) -> Arc<dyn ToolAccess> {
        Arc::new(Self {
            workspace,
            tools,
            read: Mutex::new(None),
        })
    }

    fn binding(&self) -> Result<ConnectorBinding, String> {
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
            Ok(Some(config)) => config.connectors(),
            Ok(None) => Ok(ConnectorBinding::default()),
            Err(e) => Err(e.to_string()),
        };
        *read = Some((at, binding.clone()));
        binding
    }
}

impl ToolAccess for ConnectorToolAccess {
    fn snapshot(&self) -> ToolAccessSnapshot {
        match self.binding() {
            Ok(binding) if binding.enabled.is_none() && binding.read_only.is_empty() => {
                ToolAccessSnapshot::default()
            }
            Ok(binding) => hidden(&binding, self.tools.names()),
            // Unreadable: no connector reaches it until it reads again.
            Err(_) => hidden(
                &ConnectorBinding {
                    enabled: Some(Vec::new()),
                    ..ConnectorBinding::default()
                },
                self.tools.names(),
            ),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn names(list: &[&str]) -> Vec<String> {
        list.iter().map(|s| s.to_string()).collect()
    }

    const TOOLS: &[&str] = &[
        "linear.issue_get",
        "linear.issue_create",
        "github.pr_list",
        "github.pr_merge",
        "github.api",
        "jira.search",
        "browser.navigate",
        "xero.invoices_list",
        "memory.recall",
        "filesystem.write",
        "web.search",
    ];

    #[test]
    fn an_agent_with_no_enabled_keeps_every_connector() {
        let snapshot = hidden(&ConnectorBinding::default(), names(TOOLS));
        assert!(snapshot.is_empty());
    }

    #[test]
    fn an_empty_enabled_hides_every_connector_and_nothing_else() {
        let none = ConnectorBinding {
            enabled: Some(vec![]),
            ..Default::default()
        };
        let snapshot = hidden(&none, names(TOOLS));
        let mut blocked: Vec<&str> = snapshot.blocked_names().collect();
        blocked.sort();
        assert_eq!(
            blocked,
            [
                "github.api",
                "github.pr_list",
                "github.pr_merge",
                "jira.search",
                "linear.issue_create",
                "linear.issue_get"
            ]
        );
        assert!(snapshot
            .block_reason("linear.issue_get")
            .unwrap()
            .contains("linear is off for this agent"));
    }

    #[test]
    fn only_the_enabled_reach_it_and_a_read_only_one_loses_its_writes() {
        let binding = ConnectorBinding {
            enabled: Some(names(&["linear", "github"])),
            read_only: names(&["github"]),
            own_credential: vec![],
            ..Default::default()
        };
        let snapshot = hidden(&binding, names(TOOLS));
        assert!(!snapshot.is_blocked("linear.issue_get"));
        assert!(!snapshot.is_blocked("linear.issue_create"));
        assert!(!snapshot.is_blocked("github.pr_list"));
        assert!(snapshot.is_blocked("github.pr_merge"));
        assert!(snapshot.is_blocked("github.api"));
        assert!(snapshot.is_blocked("jira.search"));
        assert!(snapshot
            .block_reason("github.pr_merge")
            .unwrap()
            .contains("only reads github"));
    }

    struct NamedTool(&'static str);

    #[async_trait::async_trait]
    impl workforce_agent::tools::Tool for NamedTool {
        fn name(&self) -> &str {
            self.0
        }
        fn description(&self) -> &str {
            ""
        }
        fn input_schema(&self) -> serde_json::Value {
            serde_json::json!({ "type": "object" })
        }
        async fn execute(
            &self,
            _input: serde_json::Value,
        ) -> eyre::Result<workforce_agent::tools::ToolResult> {
            Ok(workforce_agent::tools::ToolResult::success(
                serde_json::Value::Null,
                0,
            ))
        }
    }

    /// A server hub (no `[desk]`) never reads `[skills]`:
    /// an agent there whose config turns every connector off keeps them all.
    #[tokio::test]
    async fn a_hub_without_desk_offers_every_connector_whatever_skills_says() {
        let service = crate::environments::test_service().await;
        assert!(!service.config.desk.enabled);
        std::fs::write(
            service
                .workspace
                .join(workforce_common::config_files::WORKSPACE_CONFIG),
            "[skills]\nenabled = []\n",
        )
        .unwrap();
        service
            .tools
            .register(Arc::new(NamedTool("linear.issue_get")));
        let access = crate::wasm_skill_access::compose_tool_access(
            &service,
            workforce_agent::tools::access::noop(),
        );
        assert!(!access.snapshot().is_blocked("linear.issue_get"));
    }

    /// Asked on the person's turn, the agent turns its own connectors on
    /// and off through its config tool: nothing refuses `[skills]`, and the
    /// policy follows what it wrote.
    #[tokio::test]
    async fn the_agent_sets_its_own_connectors_through_its_config_tool() {
        use workforce_agent::tools::Tool;
        let service = crate::environments::test_service().await;
        for name in ["linear.issue_get", "github.pr_get"] {
            service.tools.register(Arc::new(NamedTool(name)));
        }
        let tool = crate::config_tool::ConfigTool::new(Arc::clone(&service), None);
        let set = tool
            .execute(serde_json::json!({
                "action": "set",
                "key": "skills.enabled",
                "value": ["linear"],
            }))
            .await
            .unwrap();
        assert!(set.success, "{:?}", set.error);
        let policy =
            ConnectorToolAccess::policy(service.workspace.clone(), Arc::clone(&service.tools));
        let snapshot = policy.snapshot();
        assert!(!snapshot.is_blocked("linear.issue_get"));
        assert!(snapshot.is_blocked("github.pr_get"));
        // Its own connection is a binding, not a secret the tool refuses.
        let own = tool
            .execute(serde_json::json!({
                "action": "set",
                "key": "skills.own_credential",
                "value": ["linear"],
            }))
            .await
            .unwrap();
        assert!(own.success, "{:?}", own.error);

        let unset = tool
            .execute(serde_json::json!({ "action": "unset", "key": "skills.enabled" }))
            .await
            .unwrap();
        assert!(unset.success, "{:?}", unset.error);
        let policy =
            ConnectorToolAccess::policy(service.workspace.clone(), Arc::clone(&service.tools));
        assert!(policy.snapshot().is_empty());
    }

    #[test]
    fn on_a_desk_hub_the_agents_registry_is_what_is_hidden() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path()
                .join(workforce_common::config_files::WORKSPACE_CONFIG),
            "[skills]\nenabled = [\"github\"]\nread_only = [\"github\"]\n",
        )
        .unwrap();
        let tools = Arc::new(ToolRegistry::new());
        for name in [
            "linear.issue_get",
            "github.pr_get",
            "github.pr_merge",
            "memory.recall",
        ] {
            tools.register(Arc::new(NamedTool(name)));
        }
        let snapshot = ConnectorToolAccess::policy(dir.path().to_path_buf(), tools).snapshot();
        let mut blocked: Vec<&str> = snapshot.blocked_names().collect();
        blocked.sort();
        assert_eq!(blocked, ["github.pr_merge", "linear.issue_get"]);
    }

    #[test]
    fn an_unreadable_skills_reaches_no_connector() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path()
                .join(workforce_common::config_files::WORKSPACE_CONFIG),
            "[skills]\nenabled = \"linear\"\n",
        )
        .unwrap();
        let tools = Arc::new(ToolRegistry::new());
        tools.register(Arc::new(NamedTool("linear.issue_get")));
        tools.register(Arc::new(NamedTool("memory.recall")));
        let snapshot = ConnectorToolAccess::policy(dir.path().to_path_buf(), tools).snapshot();
        assert!(snapshot.is_blocked("linear.issue_get"));
        assert!(!snapshot.is_blocked("memory.recall"));
    }

    #[test]
    fn the_policy_reads_the_agents_own_config_and_follows_it_as_it_changes() {
        let dir = tempfile::tempdir().unwrap();
        let tools = Arc::new(ToolRegistry::new());
        let policy = ConnectorToolAccess {
            workspace: dir.path().to_path_buf(),
            tools: Arc::clone(&tools),
            read: Mutex::new(None),
        };
        let write = |body: &str| {
            std::fs::write(
                dir.path()
                    .join(workforce_common::config_files::WORKSPACE_CONFIG),
                body,
            )
            .unwrap();
            // A later write is seen even within the clock's resolution.
            *policy.read.lock() = None;
        };
        assert_eq!(policy.binding().unwrap(), ConnectorBinding::default());
        write("[skills]\nenabled = [\"linear\"]\n");
        assert_eq!(policy.binding().unwrap().enabled, Some(names(&["linear"])));
        write("[skills]\nenabled = \"linear\"\n");
        assert!(policy.binding().is_err());
    }
}
