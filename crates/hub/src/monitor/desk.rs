//! A monitor that tells its owner in Desk (`surface = "desk"` on a `notify`
//! or `agent_task`): in their DM with the agent that owns the monitor
//! (`"dm"`, the default), or as a Desk notice there (`"notice"`).
//!
//! - **Only on a hub that runs Desk.** A save on a server hub is refused
//!   ([`super::engine::MonitorEngine::preflight`]); one that reaches a
//!   dispatch anyway (a TOML file on disk) fails it for good, saying why.
//!   On a hub that runs Desk, a dispatch before Desk has started (the
//!   catch-up at boot) waits for it.
//! - **The person's cadence decides when.** What the monitor saw is handed
//!   to the crew ([`crate::desk_crew::CrewHub::monitor_told`]), which tells
//!   it now, or holds it for the quiet hours, the roundup, or the person's
//!   return, as their updates are.
//! - **An `agent_task`'s answer is what is told.** Its turn is told its
//!   answer goes to the person's Desk DM, as a scheduled job's is, rather
//!   than posting it itself: a Desk DM is no place `messaging.send` names.

use std::sync::Arc;

use workforce_orchestrator::monitor::{Action, DeskDelivery, MonitorEvent, MonitorMeta};

use super::engine::ActionOutcome;
use crate::serve::AgentService;

/// Why a Desk delivery waits on a hub that runs Desk.
const DESK_STARTING: &str = "Desk hasn't started on this hub yet";

/// Why a Desk delivery can't be made on this hub.
pub(crate) const NOT_ON_DESK: &str = "this monitor tells you in Desk (`surface = \"desk\"`), and \
     this hub doesn't run Desk: deliver on slack or teams here, or save it on the hub your Desk runs";

/// How an `agent_task` told to Desk ends its prompt.
pub(crate) const TELLS_DESK: &str = "Your answer is what the person reads: it is told to them in \
     their Desk DM with you. Answer with just that, plainly and briefly, and don't post it \
     anywhere yourself.";

/// What one of the person's monitors saw, to tell them in Desk.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Finding {
    /// What makes it one (the monitor, the person, the event): held again
    /// on a dispatch run again, it is still one.
    pub key: String,
    pub monitor_id: String,
    pub monitor_name: String,
    pub place: DeskDelivery,
    pub text: String,
}

/// The crew to tell, with the agent as its hub serves it now; else how the
/// dispatch ends: waiting for Desk to start on a hub that runs it
/// (`desk_on`), refused on one that doesn't.
#[cfg(feature = "postgres")]
fn crew_of(
    service: &AgentService,
    desk_on: bool,
) -> Result<(Arc<crate::desk_crew::CrewHub>, Arc<AgentService>), ActionOutcome> {
    crate::desk_crew::for_agent(service).ok_or_else(|| match desk_on {
        true => ActionOutcome::Transient(DESK_STARTING.to_string()),
        false => ActionOutcome::Terminal(NOT_ON_DESK.to_string()),
    })
}

/// Tell `owner` (else the person whose Desk this is) what `monitor` saw in
/// `ev`, at `place` in their Desk DM with `service`'s agent. `from` is who
/// the event came from, as an `agent_task`'s turn is told. `desk_on`: the
/// hub runs Desk.
pub(crate) async fn deliver(
    service: &Arc<AgentService>,
    desk_on: bool,
    owner: Option<&str>,
    monitor: &MonitorMeta,
    ev: &MonitorEvent,
    place: DeskDelivery,
    from: &str,
) -> ActionOutcome {
    #[cfg(feature = "postgres")]
    {
        let (crew, service) = match crew_of(service, desk_on) {
            Ok(found) => found,
            Err(outcome) => return outcome,
        };
        let service = &service;
        let Some(owner) = owner
            .map(str::to_string)
            .or_else(|| crew.person(Some(service)))
        else {
            return ActionOutcome::Transient(
                "no one has a Desk conversation with this agent yet".to_string(),
            );
        };
        let text = match &monitor.action {
            Action::Notify { template, .. } => ev.render(template),
            Action::AgentTask { prompt, .. } => {
                match turn(service, &crew, &owner, monitor, ev, prompt, from).await {
                    Ok(text) => text,
                    Err(outcome) => return outcome,
                }
            }
            other => {
                return ActionOutcome::Terminal(format!(
                    "a {} action can't tell you in Desk",
                    other.kind()
                ))
            }
        };
        if text.trim().is_empty() {
            return ActionOutcome::Done(None);
        }
        let finding = Finding {
            key: key_of(monitor, &owner, ev),
            monitor_id: monitor.id.clone(),
            monitor_name: monitor.name.clone(),
            place,
            text,
        };
        // Not told nor kept (Postgres is down): the dispatch is tried again,
        // an agent_task's turn with it, rather than its finding lost.
        match crew.monitor_told(service, &owner, finding).await {
            Ok(_) => ActionOutcome::Done(None),
            Err(e) => ActionOutcome::Transient(format!("telling Desk: {e:#}")),
        }
    }
    #[cfg(not(feature = "postgres"))]
    {
        let _ = (service, desk_on, owner, monitor, ev, place, from);
        ActionOutcome::Terminal(NOT_ON_DESK.to_string())
    }
}

/// What makes `ev`'s finding one, for `owner`.
#[cfg(feature = "postgres")]
fn key_of(monitor: &MonitorMeta, owner: &str, ev: &MonitorEvent) -> String {
    format!("{}\n{owner}\n{}", monitor.id, ev.dispatch_id())
}

/// The `agent_task` turn for `ev`, as `owner`: its answer, or how the
/// dispatch ends without one.
#[cfg(feature = "postgres")]
async fn turn(
    service: &Arc<AgentService>,
    crew: &crate::desk_crew::CrewHub,
    owner: &str,
    monitor: &MonitorMeta,
    ev: &MonitorEvent,
    prompt: &str,
    from: &str,
) -> Result<String, ActionOutcome> {
    // As any monitor's turn: a suspended owner's acts on nothing, and
    // retrying would not change that.
    if let Some(person) = service.suspended(owner) {
        return Err(ActionOutcome::Terminal(crate::standing::refusal(&person)));
    }
    let task = super::engine::build_agent_task_prompt(prompt, ev, from, TELLS_DESK);
    let session = format!("monitor:{}:{}", monitor.id, ev.dispatch_id());
    let run = service.process_turn(
        &session,
        "monitor",
        &task,
        None,
        None,
        None,
        None,
        None,
        None,
        None,
        Some(&session),
        None,
        Some(owner),
        crate::serve::RunAuthor::Other,
    );
    let payer = service.background_payer(Some(owner));
    match crate::user_config::billed_to(payer, run).await {
        Ok(crate::serve::TurnOutcome::Answered(answer)) => Ok(answer),
        // Told why it stopped where its answer would have gone; retrying
        // would be refused the same way.
        Ok(crate::serve::TurnOutcome::Stopped(stop)) => {
            if let Some(notice) = stop.notice() {
                let told = Finding {
                    key: format!("{}\nstopped", key_of(monitor, owner, ev)),
                    monitor_id: monitor.id.clone(),
                    monitor_name: monitor.name.clone(),
                    place: DeskDelivery::Notice,
                    text: notice,
                };
                if let Err(e) = crew.monitor_told(service, owner, told).await {
                    tracing::warn!(monitor = %monitor.id, error = %format!("{e:#}"), "telling Desk why a monitor's turn stopped failed");
                }
            }
            Err(ActionOutcome::Terminal(stop.reason()))
        }
        Ok(crate::serve::TurnOutcome::Skipped(skip)) => {
            Err(ActionOutcome::Transient(format!("agent_task turn: {skip}")))
        }
        Err(e) => Err(ActionOutcome::Transient(format!("agent_task turn: {e}"))),
    }
}

/// Tell `owner` a monitor's own news (`reason`: it was disabled, it
/// expired) as a notice in their Desk DM with `service`'s agent, by their
/// cadence. Its news is told once, as on Slack or Teams: when Desk hasn't
/// started yet it is logged, not kept (the monitor's state says the same).
pub(crate) async fn tell_owner(
    service: &Arc<AgentService>,
    desk_on: bool,
    owner: &str,
    reason: &str,
    notice: &str,
) -> ActionOutcome {
    #[cfg(feature = "postgres")]
    {
        let (crew, service) = match crew_of(service, desk_on) {
            Ok(found) => found,
            Err(ActionOutcome::Transient(e) | ActionOutcome::Terminal(e)) => {
                return ActionOutcome::Terminal(format!("{reason}: {e}"))
            }
            Err(done) => return done,
        };
        let service = &service;
        let finding = Finding {
            key: format!("{reason}\n{owner}\n{notice}"),
            monitor_id: reason.to_string(),
            monitor_name: "Your monitors".to_string(),
            place: DeskDelivery::Notice,
            text: notice.to_string(),
        };
        match crew.monitor_told(service, owner, finding).await {
            Ok(_) => ActionOutcome::Done(None),
            Err(e) => ActionOutcome::Transient(format!("{reason}: {e:#}")),
        }
    }
    #[cfg(not(feature = "postgres"))]
    {
        let _ = (service, desk_on, owner, notice);
        ActionOutcome::Terminal(format!("{reason}: {NOT_ON_DESK}"))
    }
}
