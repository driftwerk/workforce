//! What an overseer's DM message mentions to act on in place: the session
//! requests it names (or its wake-up listed) that still wait, and the
//! suggestions it made in that turn. Sent on the live `stream_end` /
//! `message` frames and kept by message for the DM's history.

use std::sync::Arc;

use chrono::Duration;
use workforce_common::channels::desk::{DeskPlace, DeskTarget};
use workforce_common::channels::DESK_CHANNEL;
use workforce_desk::crew::{self, Extras, MentionedApproval, MentionedSuggestion};

use super::CrewHub;
use crate::desk_surface::{Delivered, Delivery};
use crate::gateway::Mentions;
use crate::serve::AgentService;

impl CrewHub {
    /// Delivers `delivery` to `owner`'s DM with `service`, its `extras`
    /// kept first, by the id it is saved under: the person's Desk reads the
    /// message the moment it is announced, and reads its card with it. A
    /// failed keep is logged and the message goes plain, as before.
    pub(crate) async fn deliver_with_extras(
        &self,
        service: &AgentService,
        owner: &str,
        delivery: Delivery,
        extras: &Extras,
    ) -> eyre::Result<Delivered> {
        let id = uuid::Uuid::new_v4().to_string();
        self.keep_extras_of(&id, service.agent_id.as_str(), extras)
            .await;
        let target =
            DeskTarget::of_turn(&DeskPlace::Dm { thread: None }, DESK_CHANNEL, None, false);
        crate::desk_surface::deliver_as(service, owner, &target, delivery, id).await
    }

    /// Keeps `extras` by `message`, logging a failure.
    pub(super) async fn keep_extras_of(&self, message: &str, agent_id: &str, extras: &Extras) {
        if let Err(e) = self
            .desk
            .store()
            .put_extras(message, agent_id, extras)
            .await
        {
            tracing::warn!(error = %e, "keeping a message's extras failed");
        }
    }

    /// The extras of an answer `text` the overseer `agent_id` is giving in
    /// its DM session `sid`.
    pub(crate) async fn extras_for(&self, agent_id: &str, sid: &str, text: &str) -> Extras {
        let since = self
            .manager_turns
            .lock()
            .get(sid)
            .copied()
            .unwrap_or_else(|| crew::now() - Duration::minutes(10));
        let suggestions = match self.desk.store().suggestions_since(agent_id, since).await {
            Ok(list) => list
                .into_iter()
                .map(|s| MentionedSuggestion {
                    id: s.id,
                    text: s.text,
                    state: s.state,
                })
                .collect(),
            Err(e) => {
                tracing::warn!(error = %e, "reading the overseer's suggestions failed");
                Vec::new()
            }
        };
        let listed = self.listed.lock().get(sid).cloned().unwrap_or_default();
        let mut approvals: Vec<MentionedApproval> = Vec::new();
        // Only its own scope's requests are offered in its DM.
        let seen = self
            .desk
            .scope_of_boss(agent_id)
            .map(|scope| self.desk.seen_by(&scope));
        for v in self.desk.list() {
            let Some(p) = v.pending.as_ref().filter(|_| v.live) else {
                continue;
            };
            if !seen
                .as_ref()
                .is_some_and(|s| s.sees(v.group.as_ref().map(|g| g.id.as_str())))
            {
                continue;
            }
            let id = p.id.to_string();
            let named = text.contains(&v.name)
                || v.display_name.as_deref().is_some_and(|n| text.contains(n))
                || text.contains(&v.id.to_string());
            if named || listed.iter().any(|l| l.approval_id == id) {
                approvals.push(MentionedApproval {
                    session_id: v.id.to_string(),
                    approval_id: id,
                    summary: crew::approval_summary(&p.tool, &p.detail),
                });
            }
        }
        Extras {
            approvals,
            suggestions,
            ..Default::default()
        }
    }

    /// Keeps the extras of the overseer's answer just saved in `sid`, by
    /// the message that holds it.
    pub(super) async fn keep_extras(&self, agent_id: &str, sid: &str, answer: &str) {
        let extras = self.extras_for(agent_id, sid, answer).await;
        self.listed.lock().remove(sid);
        if extras.is_empty() {
            return;
        }
        let Some(service) = self.state.registry.get(agent_id) else {
            return;
        };
        // A turn that rolled the DM over answered in its successor.
        let session = service.sessions.get(sid).ok().flatten().and_then(|s| {
            if s.is_sealed() {
                service.sessions.find(&s.channel, &s.owner).ok().flatten()
            } else {
                Some(s)
            }
        });
        let message = session.and_then(|s| {
            s.messages
                .iter()
                .rev()
                .find(|m| m.role == workforce_common::session::MessageRole::Assistant)
                .map(|m| m.id.clone())
        });
        let Some(message) = message else { return };
        if let Err(e) = self
            .desk
            .store()
            .put_extras(&message, agent_id, &extras)
            .await
        {
            tracing::warn!(error = %e, "keeping a message's extras failed");
        }
    }
}

/// What the live frames of a turn in `sid` carry, when `service` is the
/// overseer and `sid` its Desk DM.
pub(crate) fn mentions_for(service: &AgentService, sid: &str) -> Option<Mentions> {
    let crew = super::for_service(service)?;
    crew.overseer_named(service.agent_id.as_str())?;
    let session = service.sessions.get(sid).ok().flatten()?;
    if session.channel != DESK_CHANNEL {
        return None;
    }
    let agent_id = service.agent_id.as_str().to_string();
    let sid = sid.to_string();
    Some(Arc::new(move |text: String| {
        let (crew, agent_id, sid) = (Arc::clone(&crew), agent_id.clone(), sid.clone());
        Box::pin(async move {
            let extras = crew.extras_for(&agent_id, &sid, &text).await;
            (!extras.is_empty())
                .then(|| serde_json::to_value(extras).ok())
                .flatten()
        })
    }))
}
