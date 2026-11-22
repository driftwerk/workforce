//! Which connectors' tools reach an agent, on a hub with `[desk]`: what the
//! agent's own `[skills]` turns on (`enabled`), and of those, the ones that
//! only read (`read_only`, and those that start read only until `writable`
//! names them). An agent whose `[skills]` names no `enabled` keeps every
//! connector, as before the key existed; every other hub never builds this.
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
    use workforce_common::connector_binding::CONNECTORS;

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

    /// Every read of every connector Desk lists, by name: what a read-only
    /// connection still lets through.
    const READS: &[&str] = &[
        "github.list_repos",
        "github.get_repo",
        "github.list_issues",
        "github.get_issue",
        "github.list_pulls",
        "github.get_pull",
        "github.get_file",
        "github.pr_diff",
        "github.dir_list",
        "github.compare",
        "github.search_code",
        "github.search_commits",
        "github.branch_list",
        "github.ci_status",
        "github.workflow_runs",
        "github.workflow_run_logs",
        "github.workflow_run_failed_logs",
        "github.viewer",
        "github.projects_list",
        "github.project_get",
        "github.project_items",
        "github.issues_changed",
        "github.repo_labels",
        "github.workspaces",
        "google.calendar.calendars",
        "google.calendar.events",
        "google.calendar.event_get",
        "google.calendar.search",
        "google.calendar.freebusy",
        "google.calendar.colors",
        "google.drive.ls",
        "google.drive.search",
        "google.drive.get",
        "google.drive.download",
        "google.drive.permissions",
        "google.drive.comments_list",
        "google.drive.drives",
        "google.sheets.get",
        "google.sheets.metadata",
        "google.docs.cat",
        "google.docs.info",
        "google.gmail.search",
        "google.gmail.get",
        "google.gmail.thread_get",
        "google.gmail.attachment",
        "google.gmail.history",
        "google.gmail.labels_list",
        "google.gmail.label_get",
        "google.gmail.draft_get",
        "google.gmail.drafts_list",
        "google.gmail.filters_list",
        "google.gmail.forwarding_list",
        "google.gmail.sendas_list",
        "google.gmail.vacation_get",
        "google.tasks.lists",
        "google.tasks.list",
        "google.tasks.get",
        "google.contacts.list",
        "google.contacts.get",
        "google.contacts.search",
        "google.contacts.other_list",
        "google.contacts.directory_search",
        "google.people.me",
        "google.people.get",
        "google.people.search",
        "google.chat.spaces_list",
        "google.chat.spaces_find",
        "google.chat.messages_list",
        "google.chat.threads_list",
        "google.chat.reactions_list",
        "google.slides.info",
        "google.slides.list_slides",
        "google.slides.read_slide",
        "google.slides.export",
        "google.workspaces",
        "email.accounts",
        "email.inbox",
        "email.folders",
        "email.read",
        "email.get_attachment",
        "email.search",
        "email.contacts",
        "jira.search",
        "jira.get_issue",
        "jira.list_comments",
        "jira.list_transitions",
        "jira.get_attachment",
        "jira.attachment_read",
        "jira.list_projects",
        "jira.workspaces",
        "jira.get_project",
        "jira.search_users",
        "jira.myself",
        "jira.list_fields",
        "jira.dev_status",
        "jira.list_remote_links",
        "jira.list_boards",
        "jira.board_order",
        "jira.list_sprints",
        "linear.issue_list",
        "linear.issue_get",
        "linear.issue_search",
        "linear.issue_children",
        "linear.issue_history",
        "linear.comment_list",
        "linear.states_list",
        "linear.teams_list",
        "linear.workspaces",
        "linear.project_list",
        "linear.project_get",
        "linear.labels_list",
        "linear.members_list",
        "linear.viewer",
        "linear.view_ordering",
        "linear.upload_read",
        "linear.document_get",
        "linear.document_list",
        "linear.document_search",
        "slack.list_channels",
        "slack.list_users",
        "slack.lookup_user_by_email",
        "slack.list_channel_members",
        "slack.list_user_conversations",
        "slack.search_messages",
        "slack.get_message_replies",
        "slack.user_info",
        "slack.list_files",
        "datadog.metrics_query",
        "datadog.metrics_scalar",
        "datadog.metrics_list",
        "datadog.metrics_metadata",
        "datadog.logs_search",
        "datadog.logs_export",
        "datadog.logs_aggregate",
        "datadog.monitors_list",
        "datadog.monitors_get",
        "datadog.monitors_search",
        "datadog.dashboards_list",
        "datadog.dashboards_get",
        "datadog.events_list",
        "datadog.incidents_list",
        "datadog.incidents_get",
        "datadog.slos_list",
        "datadog.slos_get",
        "datadog.hosts_list",
        "datadog.downtimes_list",
        "datadog.synthetics_list",
        "metabase.card_list",
        "metabase.card_get",
        "metabase.card_query",
        "metabase.dashboard_list",
        "metabase.dashboard_get",
        "metabase.dashboard_query",
        "metabase.collection_list",
        "metabase.collection_get",
        "metabase.collection_items",
        "metabase.database_list",
        "metabase.database_tables",
        "metabase.table_fields",
        "metabase.search",
        "freeagent.company",
        "freeagent.contacts_list",
        "freeagent.contact_get",
        "freeagent.invoices_list",
        "freeagent.invoice_get",
        "freeagent.bills_list",
        "freeagent.bill_get",
        "freeagent.estimates_list",
        "freeagent.expenses_list",
        "freeagent.bank_accounts_list",
        "freeagent.bank_transactions_list",
        "freeagent.projects_list",
        "freeagent.project_get",
        "freeagent.tasks_list",
        "freeagent.timeslips_list",
        "freeagent.categories_list",
        "freeagent.users_list",
        "freeagent.profit_and_loss_summary",
        "freeagent.workspaces",
        "bank.workspaces",
        "bank.accounts",
        "bank.balances",
        "bank.transactions",
    ];

    fn registered(connector: &str) -> Vec<&'static str> {
        match connector {
            "slack" => workforce_skills::adapters::slack::NEW_SLACK_TOOL_NAMES.to_vec(),
            c => crate::skill_loader::static_skill_tool_names(c),
        }
    }

    #[test]
    fn every_connector_tool_is_read_or_written_as_the_table_says() {
        let every: Vec<&str> = CONNECTORS.iter().flat_map(|c| registered(c)).collect();
        for c in CONNECTORS {
            assert!(!registered(c).is_empty(), "{c} registers no tools");
        }
        for read in READS {
            assert!(every.contains(read), "{read} isn't a registered tool");
        }
        let wrong: Vec<String> = every
            .iter()
            .filter(|t| is_read(t) != READS.contains(t))
            .map(|t| format!("{t}: is_read says {}", is_read(t)))
            .collect();
        assert!(wrong.is_empty(), "{wrong:#?}");
    }

    #[test]
    fn an_agent_with_no_enabled_keeps_every_connector() {
        let snapshot = hidden(&ConnectorBinding::default(), names(TOOLS));
        assert!(snapshot.is_empty());
    }

    #[test]
    fn freeagent_loses_its_writes_until_writable_names_a_connection() {
        let tools = names(&["freeagent.invoices_list", "freeagent.invoice_create"]);
        let snapshot = hidden(&ConnectorBinding::default(), tools.clone());
        assert!(!snapshot.is_blocked("freeagent.invoices_list"));
        assert!(snapshot
            .block_reason("freeagent.invoice_create")
            .unwrap()
            .contains("only reads freeagent"));
        let harbor = ConnectorBinding {
            writable: names(&["freeagent:harbor"]),
            ..Default::default()
        };
        assert!(hidden(&harbor, tools).is_empty());
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

    /// A widening the gate decided runs only on the file it was decided
    /// on: changed in between, it is refused and nothing is written.
    #[tokio::test]
    async fn a_widening_runs_only_on_the_config_it_was_asked_on() {
        use workforce_agent::tools::Tool;
        let service = crate::environments::test_service().await;
        let config = service
            .workspace
            .join(workforce_common::config_files::WORKSPACE_CONFIG);
        std::fs::write(&config, "[skills]\nenabled = [\"linear\"]\n").unwrap();
        let tool = crate::config_tool::ConfigTool::new(Arc::clone(&service), None);
        let widen = serde_json::json!({
            "action": "set",
            "key": "skills.enabled",
            "value": ["linear", "github"],
        });
        assert!(matches!(
            tool.effect(&widen),
            workforce_core::computer::Effect::Connector { write: true, .. }
        ));
        // Something else changes the file after the gate decided.
        let between = "[skills]\nenabled = [\"linear\"]\nread_only = [\"linear\"]\n";
        std::fs::write(&config, between).unwrap();
        let refused = tool.execute(widen.clone()).await.unwrap();
        assert!(!refused.success);
        assert!(refused
            .error
            .as_deref()
            .unwrap_or_default()
            .contains("changed"));
        assert_eq!(std::fs::read_to_string(&config).unwrap(), between);
        // Asked again on the file as it is, it runs.
        tool.effect(&widen);
        let ran = tool.execute(widen).await.unwrap();
        assert!(ran.success, "{:?}", ran.error);
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
