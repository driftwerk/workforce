//! What an overseer's scope holds, as its tools check it: a session, an
//! agent, a line message or a ticket is its when Desk's rule says its
//! scope sees it ([`workforce_desk::scope`]). Everything else is refused in
//! the same words, before it is looked into or the gate weighs it, so a
//! refusal never says what another scope holds. Another overseer is in no
//! scope but its own: an overseer never messages or reads another.

use std::sync::Arc;

use uuid::Uuid;
use workforce_desk::scope::Seen;

use super::actions::{Action, Handout, Target};
use super::CrewHub;
use crate::serve::AgentService;

impl CrewHub {
    /// The group the agent `id` is in for the scopes: an overseer's is its
    /// scope's, wherever it sits; another agent's is its sidebar group's.
    fn agent_group(&self, id: &str) -> Option<String> {
        match self.desk.scope_of_boss(id) {
            Some(scope) => scope.group().map(str::to_string),
            None => self
                .desk
                .group_of_member(&workforce_desk::local_agent_item(id)),
        }
    }

    /// Whether `seen` holds the agent `id`: one of this hub's in its scope,
    /// and no other overseer.
    pub(crate) fn agent_in(&self, id: &str, seen: &Seen) -> bool {
        let other_boss = self.desk.scope_of_boss(id).is_some_and(|s| s != seen.scope);
        !other_boss
            && self.state.registry.get(id).is_some()
            && seen.sees(self.agent_group(id).as_deref())
    }

    /// The agent `id`, when `seen` holds it; refused in the same words
    /// whether it is another scope's or no agent at all.
    pub(crate) fn agent_seen(&self, id: &str, seen: &Seen) -> Result<Arc<AgentService>, String> {
        match self.state.registry.get(id) {
            Some(service) if self.agent_in(id, seen) => Ok(service),
            _ => Err(self.not_ours(&seen.scope)),
        }
    }

    /// Whether `seen` holds the session `id`: refused in the same words
    /// whether it is another scope's or isn't there at all.
    pub(crate) fn session_seen(&self, id: Uuid, seen: &Seen) -> Result<(), String> {
        match self.desk.view(id) {
            Ok(v) if seen.sees(v.group.as_ref().map(|g| g.id.as_str())) => Ok(()),
            _ => Err(self.not_ours(&seen.scope)),
        }
    }

    /// Whether `seen` holds what `id` names: a session (by its id) or an
    /// agent.
    pub(crate) fn subject_seen(&self, id: &str, seen: &Seen) -> Result<(), String> {
        match id.parse::<Uuid>() {
            Ok(session) if self.desk.view(session).is_ok() => self.session_seen(session, seen),
            _ => self.agent_seen(id, seen).map(drop),
        }
    }

    /// Whether `seen` holds the line message `id`: its session's group's.
    async fn message_seen(&self, id: Uuid, seen: &Seen) -> Result<(), String> {
        match self.desk.store().line_message(id).await {
            Ok(Some(m)) if !seen.sees_row(m.group.as_deref(), m.at) => {
                Err(self.not_ours(&seen.scope))
            }
            Ok(Some(m)) => match m.from_session().or_else(|| m.to_session()) {
                Some(s) => self.session_seen(s, seen),
                None => Ok(()),
            },
            _ => Err(self.not_ours(&seen.scope)),
        }
    }

    /// Refuses `action` when anything it names is outside `seen`: every
    /// session, agent and line message in it. Tickets are refused as they
    /// are read ([`CrewHub::resolve_ticket`]).
    pub(crate) async fn in_scope(&self, action: &Action, seen: &Seen) -> Result<(), String> {
        match action {
            Action::Nudge { to, reply_to, .. } => {
                match to {
                    Target::Session(s) => self.session_seen(*s, seen)?,
                    Target::Agent(a) => self.agent_seen(a, seen).map(drop)?,
                }
                if let Some(ask) = reply_to {
                    self.message_seen(*ask, seen).await?;
                }
                Ok(())
            }
            Action::Escalate { message, .. } => self.message_seen(*message, seen).await,
            Action::Approve { session, .. }
            | Action::Deny { session, .. }
            | Action::Stop { session }
            | Action::Interrupt { session }
            | Action::Resume { session, .. }
            | Action::Restart { session, .. }
            | Action::TakeOn { session }
            | Action::Release { session }
            | Action::Handoff { session, .. }
            | Action::SetMode { session, .. } => self.session_seen(*session, seen),
            Action::Assign { agent, .. } => self.agent_seen(agent, seen).map(drop),
            Action::AssignTicket { to, .. } => match to {
                Handout::Session(s) => self.session_seen(*s, seen),
                Handout::Agent(a) => self.agent_seen(a, seen).map(drop),
                // Started in the overseer's own group.
                Handout::NewSession { .. } => Ok(()),
            },
            // Started in the overseer's own group; its ticket is read below.
            Action::StartSession { .. } | Action::MoveTicket { .. } => Ok(()),
        }
    }
}
