//! What a monitor's `agent_task` turn may do ([`MonitorTurn`]).

use std::sync::Arc;

use workforce_agent::runner::{with_content_guard, with_tool_whitelist, ToolWhitelist};
use workforce_agent::security::monitor::{
    AnomalySignal, ContentGuard, ContentScope, Knowledge, Place,
};
use workforce_orchestrator::monitor::MonitorMeta;

use crate::serve::AgentService;

/// The tool a turn posts its own result with.
const DELIVERY_TOOL: &str = "messaging.send";

/// Reads a monitor's turn is not given unless it names them: a fetch can
/// carry what the turn read out in its URL, and the agent's own files are
/// not what the event is about. `data.*` stays: every whitelisted turn keeps
/// it to read back what a call spilled
/// ([`survives_exclusive_filter`](crate::turn_loop::tools::survives_exclusive_filter)).
const UNOFFERED_READS: &[&str] = &["web.", "filesystem."];

/// How a monitor's turn delivers its result.
pub(crate) enum Delivery<'a> {
    /// It posts it itself on `surface`, to the destination resolved for
    /// `deliver_to`.
    Posts {
        surface: &'a str,
        dest: &'a str,
        deliver_to: &'a str,
    },
    /// Its answer is told for it.
    Told,
}

/// The tools the hub knows only read: the curation set, every adapter's
/// reads, what other people wrote in a connected system, mail's, Google's
/// outside reads and the messaging reads.
pub(crate) fn reads() -> Vec<String> {
    crate::turn_loop::worker::curation_tool_patterns()
        .into_iter()
        .chain(workforce_skills::adapters::outside_reads())
        .chain(
            workforce_skills::adapters::email::READ_TOOLS
                .iter()
                .chain(workforce_skills::adapters::google::tool_helpers::OUTSIDE_READS)
                .chain(workforce_skills::adapters::messaging::READS)
                .map(|tool| tool.to_string()),
        )
        .collect()
}

/// What every guard in the hub knows of its tools: what only reads, what
/// returns other people's words, and the authorisation's command-running
/// class. Installed at boot, and by a test that needs it, always the same.
pub(crate) fn knowledge() -> Knowledge {
    Knowledge {
        reads: reads().into_iter().collect(),
        outside_reads: workforce_skills::adapters::outside_reads()
            .into_iter()
            .collect(),
        runs_commands: Some(|tool, input| {
            crate::authz::tool_capability(tool, input) == Some(crate::authz::Capability::ShellExec)
        }),
    }
}

/// The tools a monitor's turn is given: the reads it is offered, its own
/// delivery when it posts, and `named`. A `program_run` the agent offers
/// runs only these, under the same guard.
pub(crate) fn turn_tools(delivery: &Delivery<'_>, named: &[String]) -> Vec<String> {
    let posts = matches!(delivery, Delivery::Posts { .. }).then_some(DELIVERY_TOOL);
    reads()
        .into_iter()
        .filter(|tool| !UNOFFERED_READS.iter().any(|read| tool.starts_with(read)))
        .chain(posts.map(str::to_string))
        .chain(named.iter().cloned())
        .collect()
}

/// One monitor's `agent_task` turn, scoped to what its job needs. The event
/// that starts it is outside content, so it runs on a frozen set of read
/// tools, its own delivery and what the monitor names, under a guard armed
/// from the start that refuses rather than asks; its owner is told what was
/// refused.
pub(crate) struct MonitorTurn {
    guard: Arc<ContentGuard>,
    tools: Vec<String>,
}

impl MonitorTurn {
    pub(crate) fn new(monitor: &MonitorMeta, delivery: &Delivery<'_>, named: &[String]) -> Self {
        let expected = match delivery {
            Delivery::Posts {
                surface,
                dest,
                deliver_to,
            } => vec![Place::on(surface, dest), Place::on(surface, deliver_to)],
            Delivery::Told => Vec::new(),
        };
        Self {
            guard: Arc::new(ContentGuard::new(ContentScope {
                expected,
                named: named.to_vec(),
                armed: Some(format!("what monitor '{}' was given", monitor.name)),
                refuses: true,
                ..ContentScope::default()
            })),
            tools: turn_tools(delivery, named),
        }
    }

    /// The turn in `session`, as `owner`, told `task`, posting to `dest`.
    pub(crate) async fn run(
        &self,
        service: &Arc<AgentService>,
        session: &str,
        task: &str,
        dest: Option<&str>,
        owner: Option<&str>,
    ) -> eyre::Result<crate::serve::TurnOutcome> {
        with_tool_whitelist(
            Some(ToolWhitelist::for_own_session(&self.tools)),
            with_content_guard(
                Some(Arc::clone(&self.guard)),
                service.process_turn(
                    session,
                    "monitor",
                    task,
                    None,
                    None,
                    dest,
                    None,
                    None,
                    None,
                    None,
                    Some(session),
                    None,
                    owner,
                    crate::serve::RunAuthor::Other,
                ),
            ),
        )
        .await
    }

    /// What the owner is told when the turn's guard refused anything.
    pub(crate) fn notice(&self, monitor: &MonitorMeta) -> Option<String> {
        stopped_notice(monitor, &self.guard.stops())
    }
}

/// Keep the guard a run that owns `session` set on it, so a turn resumed
/// there later (after a restart) is guarded as this one is.
pub(crate) fn keep(session: &mut workforce_common::session::Session) {
    if let Some(scope) = workforce_agent::runner::caller_content_guard()
        .and_then(|guard| serde_json::to_value(guard.kept_scope()).ok())
    {
        session.set_content_scope(scope);
    }
}

/// The guard kept on `session` for a turn its caller set none for: the
/// unfinished turn's as it stopped ([`keep_turn`], [`turn_keeper`]), else
/// the one a monitor's run set. A kept scope that can't be read is kept as
/// strictly as any: armed, refusing.
pub(crate) fn kept_guard(
    session: &workforce_common::session::Session,
) -> Option<Arc<ContentGuard>> {
    if workforce_agent::runner::caller_content_guard().is_some() {
        return None;
    }
    let kept = session
        .turn_content_scope()
        .or_else(|| session.content_scope())?;
    let scope = serde_json::from_value(kept.clone()).unwrap_or_else(|e| {
        tracing::warn!(session = %session.id, error = %e, "A kept content scope could not be read; the turn refuses");
        ContentScope {
            armed: Some("a turn kept before it was resumed".to_string()),
            refuses: true,
            ..ContentScope::default()
        }
    });
    Some(Arc::new(ContentGuard::new(scope)))
}

/// Keep on `session` the guard of the turn that just stopped while a call
/// of it waits on someone's answer, so the turn resumed when they answer is
/// guarded as it was; a turn that finished keeps nothing.
pub(crate) fn keep_turn(
    session: &mut workforce_common::session::Session,
    result: &workforce_agent::runner::RunResult,
) {
    let waiting = result
        .tool_invocations
        .iter()
        .any(|call| call.policy_decision == "require_approval");
    session.set_turn_content_scope(
        result
            .guard
            .as_ref()
            .filter(|_| waiting)
            .and_then(|scope| serde_json::to_value(scope).ok()),
    );
}

/// What keeps the guard of a turn on `session_id` as it arms and refuses,
/// so a turn a restart cut off resumes guarded as it was. A turn that
/// finishes replaces it ([`keep_turn`]).
pub(crate) fn turn_keeper(
    sessions: Arc<dyn workforce_common::session::SessionStore>,
    session_id: String,
) -> workforce_agent::security::monitor::Keeper {
    // Kept off the runtime's threads, in the order the guard changed: a
    // write that lands after a later one is skipped.
    let order = Arc::new(std::sync::atomic::AtomicU64::new(0));
    let written = Arc::new(std::sync::Mutex::new(0u64));
    Arc::new(move |scope: &ContentScope| {
        let Ok(kept) = serde_json::to_value(scope) else {
            return;
        };
        let at = order.fetch_add(1, std::sync::atomic::Ordering::SeqCst) + 1;
        let (sessions, session_id, written) = (
            Arc::clone(&sessions),
            session_id.clone(),
            Arc::clone(&written),
        );
        let write = move || {
            let mut last = written.lock().unwrap_or_else(|e| e.into_inner());
            if *last > at {
                return;
            }
            let mut patch = serde_json::Map::new();
            patch.insert(
                workforce_common::session::TURN_CONTENT_SCOPE.to_string(),
                kept,
            );
            match sessions.merge_metadata(&session_id, patch, &[]) {
                Ok(()) => *last = at,
                Err(e) => {
                    tracing::warn!(session = %session_id, error = %e, "The turn's guard could not be kept")
                }
            }
        };
        match tokio::runtime::Handle::try_current() {
            Ok(runtime) => drop(runtime.spawn_blocking(write)),
            Err(_) => write(),
        }
    })
}

fn stopped_notice(monitor: &MonitorMeta, stops: &[AnomalySignal]) -> Option<String> {
    let (first, _) = stops.split_first()?;
    let refused = stops
        .iter()
        .map(|signal| format!("- {}", signal.description()))
        .collect::<Vec<_>>()
        .join("\n");
    Some(format!(
        "Your monitor '{name}' (`{id}`) was processing outside content, and its turn tried \
         something that content may have asked for, so it was refused:\n{refused}\n\
         Nothing was done. If that was meant, name the tool in the monitor's `tools` \
         (for example `{tool}`).",
        name = monitor.name,
        id = monitor.id,
        tool = first.tool(),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    const OUT_OF_SCOPE: &[&str] = &[
        "vault.get",
        "vault.set",
        "exec.run",
        "email.send",
        "email.forward",
        "email.delete",
        "email.move",
        "email.archive",
        "email.rules",
        "monitor",
        "scheduler",
        "workflow",
        "system.enable_skill",
        "web.fetch",
        "web.search",
        "filesystem.write",
        "memory",
        "notes",
        "browser.click",
    ];

    fn meta() -> MonitorMeta {
        serde_json::from_value(serde_json::json!({
            "id": "inbox",
            "name": "Inbox watch",
            "action": { "type": "notify", "target": "#ops", "template": "x" },
        }))
        .unwrap()
    }

    fn posts() -> Delivery<'static> {
        Delivery::Posts {
            surface: "slack",
            dest: "C1",
            deliver_to: "#ops",
        }
    }

    #[test]
    fn the_default_set_reads_and_delivers_and_does_nothing_else() {
        let tools = turn_tools(&posts(), &[]);
        for tool in OUT_OF_SCOPE {
            assert!(!tools.iter().any(|t| t == tool), "{tool} is in scope");
        }
        for tool in [
            "email.read",
            "email.search",
            "google.gmail.get",
            "messaging.read",
            "messaging.send",
            "knowledge.ask",
        ] {
            assert!(tools.iter().any(|t| t == tool), "{tool} is missing");
        }
        for tool in ["filesystem.read", "web.fetch"] {
            assert!(
                !tools.iter().any(|t| t == tool),
                "{tool} is offered unnamed"
            );
        }
    }

    #[test]
    fn every_tool_a_turn_is_offered_passes_the_guard() {
        workforce_agent::security::monitor::know(knowledge());
        let scope = ContentScope::default();
        let input = serde_json::json!({});
        for tool in turn_tools(&Delivery::Told, &[]) {
            assert_eq!(
                workforce_agent::security::monitor::assess(
                    &workforce_agent::security::monitor::Call::new(&tool, &input, None),
                    &scope,
                    &Default::default(),
                ),
                None,
                "{tool}"
            );
        }
    }

    #[test]
    fn the_hub_knows_its_command_runners_and_connector_reads() {
        workforce_agent::security::monitor::know(knowledge());
        let guard = ContentGuard::new(ContentScope {
            armed: Some("a webhook".into()),
            ..ContentScope::default()
        });
        let input = serde_json::json!({});
        for runs in [
            "code.build",
            "code.cargo",
            "wasm_skill.build",
            "desk.prompt",
            "crew.nudge",
        ] {
            assert!(
                matches!(
                    guard.check(runs, &input, None).map(|held| held.signal),
                    Some(AnomalySignal::CommandExecution { .. })
                ),
                "{runs}"
            );
        }
        let refactor = serde_json::json!({ "action": "rename_symbol" });
        assert!(guard.check("code.refactor", &refactor, None).is_some());
        for read in ["linear.issue_get", "github.list_issues", "notion.page_get"] {
            assert!(
                workforce_agent::security::monitor::reads_outside(read),
                "{read}"
            );
            assert_eq!(guard.check(read, &input, None), None, "{read}");
        }
        assert!(!workforce_agent::security::monitor::reads_outside(
            "linear.issue_create"
        ));
    }

    #[tokio::test]
    async fn a_kept_scope_guards_the_resumed_turn_as_the_first_was() {
        use workforce_common::session::Session;
        let turn = MonitorTurn::new(&meta(), &posts(), &["linear".into()]);
        let held = turn
            .guard
            .check("vault.get", &serde_json::json!({}), None)
            .unwrap();
        turn.guard.refused(&held.signal);
        let mut session = Session::new("monitor:inbox:1", "u", "a");
        assert!(
            kept_guard(&session).is_none(),
            "nothing kept, nothing guarded"
        );
        with_content_guard(Some(Arc::clone(&turn.guard)), async { keep(&mut session) }).await;
        assert!(session.content_scope().is_some());
        let resumed = kept_guard(&session).unwrap();
        assert_eq!(resumed.armed(), turn.guard.armed());
        assert_eq!(resumed.stops(), vec![held.signal]);
        let refused = resumed
            .check("email.send", &serde_json::json!({}), None)
            .unwrap();
        assert!(refused.refused);
        assert!(resumed
            .check("linear.issue_create", &serde_json::json!({}), None)
            .is_none());
        let under_caller = with_content_guard(Some(Arc::clone(&turn.guard)), async {
            kept_guard(&session)
        })
        .await;
        assert!(under_caller.is_none(), "a caller's own guard wins");
    }

    #[test]
    fn a_kept_scope_that_cannot_be_read_refuses() {
        let mut session = workforce_common::session::Session::new("monitor:inbox:1", "u", "a");
        session.set_content_scope(serde_json::json!({ "expected": "not a list" }));
        let guard = kept_guard(&session).unwrap();
        assert!(guard.armed().is_some());
        assert!(guard
            .check("messaging.send", &serde_json::json!({}), None)
            .is_some_and(|held| held.refused));
    }

    #[test]
    fn a_told_turn_cannot_post_and_named_tools_are_added() {
        let told = turn_tools(&Delivery::Told, &["email.move".into()]);
        assert!(!told.iter().any(|t| t == DELIVERY_TOOL));
        assert!(told.iter().any(|t| t == "email.move"));
    }

    #[test]
    fn the_guard_starts_armed_refusing_and_expects_its_delivery() {
        let turn = MonitorTurn::new(&meta(), &posts(), &[]);
        let to = |channel: &str| workforce_agent::tools::SurfaceEffect::Writes {
            platform: "slack".into(),
            destination: channel.into(),
        };
        assert!(turn
            .guard
            .check("messaging.send", &serde_json::json!({}), Some(&to("C1")))
            .is_none());
        assert!(turn
            .guard
            .check("messaging.send", &serde_json::json!({}), Some(&to("#ops")))
            .is_none());
        let held = turn
            .guard
            .check("messaging.send", &serde_json::json!({}), Some(&to("C666")))
            .unwrap();
        assert!(held.refused);
        assert!(turn.notice(&meta()).is_none(), "nothing refused yet");
        turn.guard.refused(&held.signal);
        let on_teams = workforce_agent::tools::SurfaceEffect::Writes {
            platform: "teams".into(),
            destination: "C1".into(),
        };
        assert!(
            turn.guard
                .check("messaging.send", &serde_json::json!({}), Some(&on_teams))
                .is_some(),
            "its channel on another platform is somewhere new"
        );
        let notice = turn.notice(&meta()).unwrap();
        assert!(notice.contains("Inbox watch"), "{notice}");
        assert!(notice.contains("`messaging.send`"), "{notice}");
        assert!(MonitorTurn::new(&meta(), &posts(), &[])
            .notice(&meta())
            .is_none());
    }
}
