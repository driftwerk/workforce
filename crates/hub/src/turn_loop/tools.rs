//! The tools one turn is offered, decided once for every turn path, and
//! what its system prompt may speak about.

use std::sync::Arc;

use tracing::{info, warn};
use workforce_common::session::Session;

use crate::serve::AgentService;

/// Tools a workflow step keeps even when it declares an exclusive whitelist:
/// **retrieval only** — `data.query`, `data.inspect`, `tool.retrieve`.
///
/// Retrieval is infrastructure a step cannot know to ask for: a large tool
/// result is spilled to an artifact only when the registry can read it back,
/// so stripping these turns the step into a surface that cannot spill and
/// every big result lands inline — on precisely the steps configured because
/// the model struggles with a wide context. Everything else is the
/// whitelist's contract: a step that needs `messaging.send` or `memory`
/// declares it. (Scaffolding used to bleed through wholesale — ~70 schemas
/// per step and observed misfires like a step burning a round on
/// `system.routing_status`.)
pub(crate) fn survives_exclusive_filter(name: &str) -> bool {
    name == workforce_agent::tools::data::DATA_QUERY_TOOL_NAME
        || name == workforce_agent::tools::data::DATA_INSPECT_TOOL_NAME
        || name == workforce_agent::spill::RETRIEVE_TOOL_NAME
}

/// Whether `name` is the tool `entry` names, or one under its namespace.
pub(crate) fn names_tool(entry: &str, name: &str) -> bool {
    name.strip_prefix(entry)
        .is_some_and(|rest| rest.is_empty() || rest.starts_with('.'))
}

/// A snapshot of `source` holding only the tools `whitelist` names (by name
/// or namespace) and retrieval ([`survives_exclusive_filter`]). An empty
/// whitelist leaves retrieval alone. The copy is frozen: tools registered in
/// `source` later never reach it.
pub(crate) fn exclusive_registry(
    source: &workforce_agent::tools::registry::ToolRegistry,
    whitelist: &[String],
) -> workforce_agent::tools::registry::ToolRegistry {
    let filtered = source.clone_all();
    filtered
        .names()
        .into_iter()
        .filter(|name| {
            !whitelist.iter().any(|entry| names_tool(entry, name))
                && !survives_exclusive_filter(name)
        })
        .for_each(|name| {
            filtered.unregister(&name);
        });
    filtered
}

/// The tools one turn is offered, and whether they are an exclusive subset
/// chosen for the turn (a whitelist or a game sandbox) rather than the
/// agent's whole surface.
#[derive(Clone)]
pub struct TurnTools {
    pub registry: Arc<workforce_agent::tools::registry::ToolRegistry>,
    pub exclusive: bool,
}

/// What the system prompt may speak about: every section for the agent's
/// whole surface, and only guidance for tools present on an exclusive one.
pub(crate) enum PromptSurface<'a> {
    Full,
    Exclusive(&'a workforce_agent::tools::registry::ToolRegistry),
}

impl PromptSurface<'_> {
    pub(crate) fn of(tools: &TurnTools) -> PromptSurface<'_> {
        if tools.exclusive {
            PromptSurface::Exclusive(&tools.registry)
        } else {
            PromptSurface::Full
        }
    }

    /// Whether the surface holds a tool that is, or is under, one of `any_of`.
    pub(crate) fn admits(&self, any_of: &[&str]) -> bool {
        match self {
            Self::Full => true,
            Self::Exclusive(registry) => registry
                .names()
                .iter()
                .any(|name| any_of.iter().any(|entry| names_tool(entry, name))),
        }
    }

    /// Whether the surface offers any of a loaded skill's tools.
    pub(crate) fn offers_skill(
        &self,
        skill: &crate::hub_core::skill_state::LoadedSkillState,
    ) -> bool {
        match self {
            Self::Full => true,
            Self::Exclusive(registry) => skill.tool_names.iter().any(|t| registry.has(t)),
        }
    }
}

/// Record on `session` the whitelist an automated turn declared when the
/// session was minted for that run alone, so a turn resumed on it by id is
/// offered the same surface. A session other work shares never keeps one
/// run's narrow surface.
pub(crate) fn stamp_tool_whitelist(session: &mut Session) {
    if let Some(whitelist) =
        workforce_agent::runner::tool_whitelist().filter(|whitelist| whitelist.owns_session)
    {
        session.set_tool_whitelist(&whitelist.tools);
    }
}

/// The tools `session`'s turn is offered, decided in one place for every
/// turn path: none in a game sandbox; the agent's registry with the
/// session's local agent standing in for `exec.run`; without outbound
/// delivery tools when the caller delivers; and narrowed to a whitelist
/// — `whitelist`, else the turn's declared [`tool_whitelist`], else the
/// one stamped on the session — as a frozen [`exclusive_registry`].
///
/// [`tool_whitelist`]: workforce_agent::runner::tool_whitelist
pub(crate) async fn turn_tools(
    service: &AgentService,
    session: &Session,
    whitelist: Option<&[String]>,
) -> TurnTools {
    if workforce_agent::runner::game_sandboxed() {
        return TurnTools {
            registry: Arc::new(workforce_agent::tools::registry::ToolRegistry::new()),
            exclusive: true,
        };
    }
    let whitelist = whitelist
        .map(<[String]>::to_vec)
        .or_else(|| workforce_agent::runner::tool_whitelist().map(|w| w.tools.to_vec()))
        .or_else(|| session.tool_whitelist());
    if let Some(whitelist) = &whitelist {
        crate::skill_loader::ensure_adapters_loaded(service, |skill| {
            crate::skill_loader::static_skill_tool_names(skill)
                .iter()
                .any(|tool| whitelist.iter().any(|entry| names_tool(entry, tool)))
        });
    }
    let base = session_registry(service, session).await;
    let base = if workforce_agent::runner::delivery_tools_suppressed() {
        Arc::new(workforce_agent::runner::without_delivery_tools(&base))
    } else {
        base
    };
    let base = without_disabled_tools(service, base);
    let Some(whitelist) = whitelist else {
        return TurnTools {
            registry: base,
            exclusive: false,
        };
    };
    let registry = exclusive_registry(&base, &whitelist);
    let offered = registry.names();
    let unmatched: Vec<&String> = whitelist
        .iter()
        .filter(|entry| !offered.iter().any(|name| names_tool(entry, name)))
        .collect();
    if !unmatched.is_empty() {
        warn!(
            session_id = %session.id,
            ?unmatched,
            "Tool whitelist entries match no tool this turn is offered"
        );
    }
    info!(
        tools_before = base.len(),
        tools_after = registry.len(),
        "Exclusive tool surface applied"
    );
    TurnTools {
        registry: Arc::new(registry),
        exclusive: true,
    }
}

/// `base` without the hot opt-in tools this agent's config has off this
/// turn; `base` itself when there are none to hide.
fn without_disabled_tools(
    service: &AgentService,
    base: Arc<workforce_agent::tools::registry::ToolRegistry>,
) -> Arc<workforce_agent::tools::registry::ToolRegistry> {
    let refactor = crate::code::refactor::TOOL_NAME;
    if !base.has(refactor) || service.refactor().is_some() {
        return base;
    }
    let without = base.clone_all();
    without.unregister(refactor);
    Arc::new(without)
}

/// Whether `session` is its person's own conversation with the agent: one
/// of their screens, or a Slack DM. Nowhere else may reach their machine —
/// in a channel, a thread, a desk room or a shared conversation, what others
/// write would act on it, and an automated run's output can leave them.
fn is_own_conversation(session: &Session) -> bool {
    use workforce_common::channels::{is_platform, CONVERSATIONAL_CHANNELS};
    !session.is_shared_conversation()
        && (CONVERSATIONAL_CHANNELS.contains(&session.channel.as_str())
            || (is_platform(&session.channel, "slack")
                && session.context_type == Some(workforce_common::session::SessionContextType::Dm)))
}

/// The agent's registry for `session`'s turn: with `exec.run` routed to the
/// local agent the session selected (DW-324), and the tools of the nodes the
/// turn's person has connected, when the node acts for them and the session
/// is [their own conversation](is_own_conversation).
async fn session_registry(
    service: &AgentService,
    session: &Session,
) -> Arc<workforce_agent::tools::registry::ToolRegistry> {
    let (Some(agents), Some(person)) = (
        service.local_agents.as_ref(),
        crate::user_config::current_identity_id(),
    ) else {
        return Arc::clone(&service.tools);
    };
    if !is_own_conversation(session) {
        return Arc::clone(&service.tools);
    }
    let online = agents.agents_of(&person).await;
    let selected = session
        .metadata
        .get("selected_local_agent")
        .and_then(|v| v.as_str())
        .filter(|node| online.iter().any(|agent| agent.key == *node));
    let node_tools: Vec<_> = online.iter().flat_map(|agent| agent.offered()).collect();
    if selected.is_none() && node_tools.is_empty() {
        return Arc::clone(&service.tools);
    }
    let registry = service.tools.clone_all();
    if let Some((node, original_exec)) = selected.zip(service.tools.get("exec.run")) {
        registry.register(Arc::new(crate::local_agent_tool::LocalAgentExecTool::new(
            Arc::clone(agents),
            node.to_string(),
            original_exec,
        )));
    }
    for offered in node_tools {
        registry.register(Arc::new(crate::local_agent_tool::NodeTool::new(
            Arc::clone(agents),
            offered,
        )));
    }
    Arc::new(registry)
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use workforce_agent::tools::registry::ToolRegistry;
    use workforce_agent::tools::{Tool, ToolResult};

    use crate::serve::testing::plain_service;

    struct Named(String);

    #[async_trait::async_trait]
    impl Tool for Named {
        fn name(&self) -> &str {
            &self.0
        }
        fn description(&self) -> &str {
            "stub"
        }
        fn input_schema(&self) -> serde_json::Value {
            serde_json::json!({"type": "object"})
        }
        async fn execute(&self, _input: serde_json::Value) -> eyre::Result<ToolResult> {
            Ok(ToolResult::success(serde_json::json!({}), 0))
        }
    }

    pub(crate) fn named(name: &str) -> Arc<dyn Tool> {
        Arc::new(Named(name.to_string()))
    }

    const RETRIEVAL: [&str; 3] = [
        workforce_agent::tools::data::DATA_INSPECT_TOOL_NAME,
        workforce_agent::tools::data::DATA_QUERY_TOOL_NAME,
        workforce_agent::spill::RETRIEVE_TOOL_NAME,
    ];

    fn registry(names: &[&str]) -> ToolRegistry {
        ToolRegistry::from_tools(names.iter().chain(RETRIEVAL.iter()).map(|name| named(name)))
    }

    fn sorted(mut names: Vec<String>) -> Vec<String> {
        names.sort();
        names
    }

    /// Retrieval survives any whitelist: without it the turn cannot spill —
    /// `RetrievalSurface::from_registry` reports no capability, so every
    /// oversized tool result lands inline on exactly the turns narrowed for
    /// models that cannot cope with one.
    #[test]
    fn exclusive_registry_keeps_whitelisted_namespaces_and_retrieval() {
        let source = registry(&[
            "github.pr_list",
            "github.issue_get",
            "githubx.search",
            "messaging.send",
            "messaging.read",
            "memory",
            "memory_extra",
            "system.version",
        ]);
        let kept = exclusive_registry(
            &source,
            &["github".into(), "messaging.send".into(), "memory".into()],
        );
        let mut expected: Vec<String> = [
            "github.issue_get",
            "github.pr_list",
            "memory",
            "messaging.send",
        ]
        .iter()
        .chain(RETRIEVAL.iter())
        .map(|name| name.to_string())
        .collect();
        expected.sort();
        assert_eq!(sorted(kept.names()), expected);
        assert_eq!(source.len(), 11, "the source registry is untouched");
    }

    #[test]
    fn empty_whitelist_keeps_only_retrieval_survivors() {
        let kept = exclusive_registry(&registry(&["memory", "messaging.send"]), &[]);
        assert_eq!(
            sorted(kept.names()),
            sorted(RETRIEVAL.iter().map(|name| name.to_string()).collect())
        );
    }

    #[test]
    fn exclusive_registry_is_a_snapshot_immune_to_later_registration() {
        let source = registry(&["github.pr_list"]);
        let kept = exclusive_registry(&source, &["github".into()]);
        source.register(named("github.issue_get"));
        assert!(source.has("github.issue_get"));
        assert!(!kept.has("github.issue_get"));
        assert!(kept.has("github.pr_list"));
    }

    #[test]
    fn scaffolding_and_subject_matter_are_both_filterable() {
        for name in [
            "system.version",
            "system.routing_status",
            "messaging.send",
            "memory",
            "notes",
            "workflow",
            "jobs",
            "linear.issue_list",
            "aws.s3.get",
            "code.search",
        ] {
            assert!(
                !survives_exclusive_filter(name),
                "{name} must be filterable — a step that needs it whitelists it"
            );
        }
    }

    fn automated_session(service: &AgentService) -> Session {
        Session::new(
            "heartbeat:inbox:1",
            workforce_common::subscription::HEARTBEAT,
            service.agent_id.as_str(),
        )
    }

    fn names_of(tools: &TurnTools) -> Vec<String> {
        tools.registry.names()
    }

    fn only_whitelisted_or_retrieval(names: &[String], whitelist: &[&str]) -> bool {
        names.iter().all(|name| {
            whitelist.iter().any(|entry| names_tool(entry, name)) || survives_exclusive_filter(name)
        })
    }

    /// A node's tools reach its own person's conversations, and nobody
    /// else's, nor a conversation others read; a session naming another
    /// person's node runs `exec.run` on the hub as before.
    #[tokio::test]
    async fn a_persons_node_tools_reach_only_their_own_conversations() {
        use crate::local_agent_registry::NodeHello;
        use workforce_agent::runner::CURRENT_USER_ID;
        use workforce_gateway_client::protocol::NodeToolSpec;

        let tmp = tempfile::tempdir().unwrap();
        let (service, _llm) = crate::serve::testing::service_on_mock_llm_with(tmp.path(), |c| {
            c.local_agents.enabled = true
        })
        .await;
        let agents = service.local_agents.clone().expect("local agents are on");
        let (tx, _rx) = tokio::sync::mpsc::channel(1);
        agents
            .register(
                NodeHello::agent(
                    "studio".into(),
                    "Studio".into(),
                    vec![],
                    vec![NodeToolSpec {
                        name: "desk_sessions".into(),
                        description: "List desk sessions".into(),
                        input_schema: serde_json::json!({"type": "object"}),
                        read_only: false,
                    }],
                ),
                "alice".into(),
                tx,
                vec![],
            )
            .await
            .unwrap();
        let agent = service.agent_id.as_str();
        let offered = |person: &str, channel: &str, owner: &str| {
            let mut session = Session::new(channel, owner, agent);
            session.metadata = serde_json::json!({"selected_local_agent": "studio"});
            let service = &service;
            CURRENT_USER_ID.scope(person.to_string(), async move {
                turn_tools(service, &session, None).await.registry
            })
        };

        let alices = offered("alice", "desk", "alice").await;
        let tool = alices.get("node.studio.desk_sessions").expect("offered");
        assert!(tool.description().contains("Studio"));
        assert!(tool.description().contains("List desk sessions"));
        assert_eq!(
            alices.get("exec.run").unwrap().description(),
            "Manage local agent processes",
            "her selected node runs her commands"
        );

        let bobs = offered("bob", "desk", "bob").await;
        assert!(!bobs.has("node.studio.desk_sessions"));
        assert_ne!(
            bobs.get("exec.run").unwrap().description(),
            "Manage local agent processes",
            "naming her node routes nothing to it"
        );
        for (channel, owner) in [
            ("desk:release", "alice"),
            ("slack:C1", "shared:1700000000.0001"),
            ("slack:C1:1700000000.0001", "alice"),
            ("scheduler:job-1", "alice"),
        ] {
            let elsewhere = offered("alice", channel, owner).await;
            assert!(!elsewhere.has("node.studio.desk_sessions"), "{channel}");
        }
    }

    #[tokio::test]
    async fn turn_tools_honours_task_local_whitelist() {
        let tmp = tempfile::tempdir().unwrap();
        let service = plain_service(tmp.path()).await;
        let session = automated_session(&service);

        let full = turn_tools(&service, &session, None).await;
        assert!(!full.exclusive);
        let mut offered = service.tools.names();
        offered.retain(|name| name != crate::code::refactor::TOOL_NAME);
        assert_eq!(sorted(full.registry.names()), sorted(offered));

        let narrowed = workforce_agent::runner::with_tool_whitelist(
            Some(workforce_agent::runner::ToolWhitelist::for_turn(&[
                "memory".to_string(),
                "heartbeat".to_string(),
            ])),
            turn_tools(&service, &session, None),
        )
        .await;
        assert!(narrowed.exclusive);
        let names = names_of(&narrowed);
        assert!(names.iter().any(|n| n == "memory"));
        assert!(names.iter().any(|n| n == "heartbeat.list"));
        assert!(
            only_whitelisted_or_retrieval(&names, &["memory", "heartbeat"]),
            "{names:?}"
        );
    }

    #[tokio::test]
    async fn structural_editing_is_offered_only_to_an_agent_that_opted_in() {
        let tmp = tempfile::tempdir().unwrap();
        let service = plain_service(tmp.path()).await;
        let session = automated_session(&service);
        let refactor = crate::code::refactor::TOOL_NAME;
        assert!(service.tools.has(refactor), "registered for the hot opt-in");
        assert!(!turn_tools(&service, &session, None)
            .await
            .registry
            .has(refactor));

        let agent: workforce_common::settings::AgentWorkspaceConfig =
            toml::from_str("[tools.refactor]\nenabled = true\n").unwrap();
        service
            .resolved
            .store(crate::hub_core::resolved_config::ResolvedConfig::resolve(
                None,
                Some(&agent),
                &service.config,
                "anthropic",
                None,
            ));
        let full = turn_tools(&service, &session, None).await;
        assert!(full.registry.has(refactor));
        assert!(
            Arc::ptr_eq(&full.registry, &service.tools),
            "nothing to hide, nothing cloned"
        );
    }

    #[tokio::test]
    async fn turn_tools_falls_back_to_stamped_session_whitelist() {
        let tmp = tempfile::tempdir().unwrap();
        let service = plain_service(tmp.path()).await;
        let mut session = automated_session(&service);
        workforce_agent::runner::with_tool_whitelist(
            Some(workforce_agent::runner::ToolWhitelist::for_own_session(&[
                "notes".to_string(),
            ])),
            async { stamp_tool_whitelist(&mut session) },
        )
        .await;
        service.sessions.save(&session).unwrap();
        let resumed = service.sessions.get(&session.id).unwrap().unwrap();

        let tools = turn_tools(&service, &resumed, None).await;

        assert!(tools.exclusive, "a turn resumed by id keeps its surface");
        let names = names_of(&tools);
        assert!(names.iter().any(|n| n == "notes"));
        assert!(
            only_whitelisted_or_retrieval(&names, &["notes"]),
            "{names:?}"
        );
    }

    #[tokio::test]
    async fn whitelist_stamp_never_narrows_a_shared_session() {
        let tmp = tempfile::tempdir().unwrap();
        let service = plain_service(tmp.path()).await;
        let mut conversation = Session::new("briefing", "scheduler", service.agent_id.as_str());
        workforce_agent::runner::with_tool_whitelist(
            Some(workforce_agent::runner::ToolWhitelist::for_turn(&[
                "notes".to_string()
            ])),
            async { stamp_tool_whitelist(&mut conversation) },
        )
        .await;

        assert_eq!(conversation.tool_whitelist(), None);
        assert!(!turn_tools(&service, &conversation, None).await.exclusive);
    }

    #[tokio::test]
    async fn whitelist_composes_with_delivery_suppression() {
        let tmp = tempfile::tempdir().unwrap();
        let service = plain_service(tmp.path()).await;
        let session = automated_session(&service);

        let tools = workforce_agent::runner::SUPPRESS_DELIVERY_TOOLS
            .scope(
                true,
                turn_tools(&service, &session, Some(&["messaging".to_string()])),
            )
            .await;

        let names = names_of(&tools);
        assert!(names.iter().any(|n| n == "messaging.read"), "{names:?}");
        assert!(
            !names.iter().any(|n| n == "messaging.send"),
            "a whitelist never restores a delivery tool the caller took away"
        );
        assert!(only_whitelisted_or_retrieval(&names, &["messaging"]));
    }

    #[tokio::test]
    async fn whitelisted_configured_adapter_is_loaded_before_narrowing() {
        let tmp = tempfile::tempdir().unwrap();
        let service = plain_service(tmp.path()).await;
        service
            .vault
            .as_ref()
            .expect("the agent vault")
            .set("skills/linear/api_key", "lk_test")
            .unwrap();
        assert!(!service
            .tools
            .names()
            .iter()
            .any(|n| n.starts_with("linear.")));
        let session = automated_session(&service);

        let tools = turn_tools(&service, &session, Some(&["linear".to_string()])).await;

        let names = names_of(&tools);
        assert!(
            names.iter().any(|n| n.starts_with("linear.")),
            "the configured adapter the whitelist names is offered: {names:?}"
        );
        assert!(only_whitelisted_or_retrieval(&names, &["linear"]));
    }
}
