//! A turn in an agent's Desk DM that the person didn't type: the overseer's
//! wake-ups and summaries, and the messages it (or you, through a
//! suggestion) sends the crew. It runs in the DM's own session, so it is in
//! the conversation's history, and goes out live to a DM the person has
//! open, as a typed message's answer does.
//!
//! It answers to that DM as a Desk turn does: what it posts goes to Desk
//! unless it names a platform, and a write anywhere else is held for the
//! person, which (no one being on the turn to answer) refuses it, even to a
//! place the person always allows from their own turns. Its prompt is not
//! the person's own words, so it is read at paired trust.

use std::sync::Arc;

use eyre::{bail, Result};
use workforce_agent::tools::approval::SelfServe;
use workforce_common::channels::desk::{DeskPlace, DeskScope, DeskTarget};
use workforce_common::channels::DESK_CHANNEL;
use workforce_common::session::MessageRole;
use workforce_common::trust::TrustLevel;
use workforce_desk::crew::MentionedApproval;
use workforce_desk::Speaker;

use super::CrewHub;
use crate::broadcast::BroadcastMessage;
use crate::live_turns::Ending;
use crate::serve::AgentService;

pub use crate::broadcast::USER_MESSAGE;

/// What a crew turn the person stopped fails with: told by its type,
/// never its words.
#[derive(Debug)]
pub(super) struct TurnStopped;

impl std::fmt::Display for TurnStopped {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("the turn was stopped")
    }
}

impl std::error::Error for TurnStopped {}

/// Whether `e` is a crew turn the person stopped.
pub(super) fn stopped(e: &eyre::Report) -> bool {
    e.chain().any(|c| c.is::<TurnStopped>())
}

impl CrewHub {
    /// Runs `prompt` as a turn in `service`'s Desk DM with `owner`, at `hop`
    /// hand-offs; `listed` are the requests a wake-up names. A prompt with
    /// an `author` (a nudge or task sent on) is a message of the DM: kept as
    /// theirs and sent live as a `user_message`. The session it ran in and
    /// the answer.
    pub(crate) async fn run_dm_turn(
        &self,
        service: &Arc<AgentService>,
        owner: &str,
        prompt: &str,
        hop: i16,
        listed: Option<Vec<MentionedApproval>>,
        author: Option<Speaker>,
    ) -> Result<(String, String)> {
        self.run_turn(service, owner, DESK_CHANNEL, prompt, hop, listed, author)
            .await
    }

    /// [`Self::run_dm_turn`] for a message a boss (`author`) sent on in
    /// answer to the person's own: `theirs`, their words, are what the
    /// receiving turn answers, so an order of it is kept as theirs.
    pub(crate) async fn run_dm_turn_for_them(
        &self,
        service: &Arc<AgentService>,
        owner: &str,
        prompt: &str,
        hop: i16,
        author: Speaker,
        theirs: String,
    ) -> Result<(String, String)> {
        self.run_turn_with(
            service,
            owner,
            DESK_CHANNEL,
            prompt,
            hop,
            None,
            Some(author),
            Some(theirs),
        )
        .await
    }

    /// [`Self::run_dm_turn`] in `service`'s conversation on `channel`: the
    /// DM, or one of its own off it (a session's line, `desk:line:<id>`),
    /// which keeps that work's context apart from the DM's history. What
    /// such a turn posts goes to the DM, and no one is there to answer
    /// what it asks: a write it would make off Desk is refused.
    pub(crate) async fn run_turn(
        &self,
        service: &Arc<AgentService>,
        owner: &str,
        channel: &str,
        prompt: &str,
        hop: i16,
        listed: Option<Vec<MentionedApproval>>,
        author: Option<Speaker>,
    ) -> Result<(String, String)> {
        self.run_turn_with(service, owner, channel, prompt, hop, listed, author, None)
            .await
    }

    /// [`Self::run_turn`], answering `theirs` (the person's words a boss
    /// sent on) when it is an overseer's message in the DM.
    #[allow(clippy::too_many_arguments)]
    async fn run_turn_with(
        &self,
        service: &Arc<AgentService>,
        owner: &str,
        channel: &str,
        prompt: &str,
        hop: i16,
        listed: Option<Vec<MentionedApproval>>,
        author: Option<Speaker>,
        theirs: Option<String>,
    ) -> Result<(String, String)> {
        let agent_id = service.agent_id.as_str();
        let in_dm = channel == DESK_CHANNEL;
        // The session a DM already open is on: not rolled over here, or an
        // open socket (which only re-points on its own messages) would miss
        // the answer. The person's next connect rolls a stale one.
        let session = service.sessions.get_or_create(channel, owner, agent_id)?;
        let sid = session.id.clone();
        if hop > 0 {
            self.hops.lock().insert(sid.clone(), hop);
        }
        if let Some(listed) = listed {
            self.listed.lock().insert(sid.clone(), listed);
        }
        // What another overseer sent on is never the person's word, unless
        // it was sent in answer to theirs: then their own words, relayed,
        // are what this turn answers.
        let by_overseer = in_dm
            && author
                .as_ref()
                .is_some_and(|a| a.kind != workforce_desk::conversations::SpeakerKind::You);
        let _relayed = by_overseer.then(|| Relayed::mark(&self.relayed_turns, &sid));
        let theirs = theirs.filter(|_| by_overseer);
        // A message of the DM is found again by a mark of its own.
        let relay = author
            .is_some()
            .then(|| format!("desk-relay:{}", workforce_desk::crew::new_id()));
        let tx = self.state.broadcast_tx.clone();
        // Theirs, relayed: in this boss's picture as their word, before
        // its turn on it begins.
        if let Some(words) = &theirs {
            self.picture_person_told(service.agent_id.as_str(), words);
        }
        let (mut live, inbox) = match service
            .begin_live_turn(&sid, SelfServe::answerable_if(false), None)
            .await
        {
            Ok(begun) => begun,
            Err(busy) => {
                self.hops.lock().remove(&sid);
                bail!("{busy}");
            }
        };
        // Started holding the boss's mind: its tools hold it too. Marked
        // once the turn is its own, never while the session is busy with
        // another.
        let _under = self.turn_under_mind(agent_id, &sid);
        // Gone with the turn, however it ends, as the person's own are.
        let _theirs = theirs.map(|words| Theirs::keep(&self.person_turns, &sid, words));
        // The crew's own wake-up, by name, for as long as its turn runs:
        // named only once the turn is the crew's (a session busy with the
        // person's own turn is never named).
        let _wake = crate::desk_browser::CrewWake::begin(&sid);
        let _ = tx.send(BroadcastMessage::new(sid.clone(), "typing", String::new()));
        let scope = DeskScope {
            target: DeskTarget::of_turn(&DeskPlace::Dm { thread: None }, DESK_CHANNEL, None, false),
            owner: owner.to_string(),
            // A quiet check-in is the DM's own turn, off it only so its
            // words aren't posted: it acts as the DM's would.
            attended: in_dm || channel.starts_with(&format!("{DESK_CHANNEL}:checkin:")),
            relayed: true,
        };
        // A turn with no author is the agent's own wake (a check-in, a
        // session's line, a summary): its heavy background model's work. A
        // message with one (the person's words, a task the overseer hands
        // on) is real work for the agent, on its own model.
        // An orchestrating boss's own conversation is real work too: on
        // the person's chosen model, as its DM is (`super::orchestrator`).
        let own_conversation =
            channel == super::watcher::check_in_channel(agent_id) && self.orchestrates(agent_id);
        let model = match &author {
            None if own_conversation => None,
            None => crate::turn_loop::heavy_model(service),
            Some(_) => None,
        };
        let progress = workforce_agent::runner::with_desk(
            Some(scope),
            service.process_message_with_progress(
                channel,
                owner,
                None,
                prompt,
                relay.as_deref(),
                Some(&sid),
                None,
                None,
                None,
                None,
                Some(inbox),
                None,
                None,
                None,
                model.as_deref(),
                None,
                None,
                None,
                None,
                None,
                Some(TrustLevel::Paired),
                Some(live.cancel_token()),
                None,
                None,
            ),
        )
        .await?;
        let session_id = progress.session_id.clone();
        let tx = if session_id == sid {
            tx
        } else {
            self.rolled(&sid, &session_id);
            both(tx, sid.clone())
        };
        if let (Some(author), Some(relay)) = (author, relay.as_deref()) {
            self.relayed(service, &session_id, relay, author, &tx).await;
        }
        let forward = {
            let (tx, sid) = (tx.clone(), session_id.clone());
            let mentions = crate::gateway::mentions_for(service, &sid);
            move |rx| crate::gateway::forward_local_progress(rx, tx, sid, false, mentions, None)
        };
        let conducted = crate::live_turns::conduct(&mut live, service, progress, forward).await;
        let answer = match &conducted.ending {
            Ending::Finished(Ok(result)) => Ok(result.response.clone()),
            Ending::Finished(Err(e)) => Err(eyre::eyre!("{e:#}")),
            Ending::Stopped => Err(eyre::Report::new(TurnStopped)),
            Ending::Failed(e) => Err(eyre::eyre!("{e}")),
        };
        for message in conducted
            .closing(service)
            .into_iter()
            .filter_map(|progress| {
                crate::gateway::chat_progress_to_broadcast(progress, &session_id)
            })
        {
            let _ = tx.send(message);
        }
        drop(live);
        Ok((session_id, answer?))
    }
}

/// The person's words a relayed turn answers, kept as that turn's and
/// dropped when it is done.
struct Theirs<'a>(
    &'a parking_lot::Mutex<std::collections::HashMap<String, String>>,
    String,
    String,
);

impl<'a> Theirs<'a> {
    fn keep(
        turns: &'a parking_lot::Mutex<std::collections::HashMap<String, String>>,
        sid: &str,
        words: String,
    ) -> Self {
        turns.lock().insert(sid.to_string(), words.clone());
        Self(turns, sid.to_string(), words)
    }
}

impl Drop for Theirs<'_> {
    /// Only these words: the person's next turn, begun in the same DM by
    /// now, keeps its own.
    fn drop(&mut self) {
        let mut turns = self.0.lock();
        if turns.get(&self.1) == Some(&self.2) {
            turns.remove(&self.1);
        }
    }
}

/// A DM session marked as running a relayed message, unmarked when the
/// turn is done.
struct Relayed<'a>(
    &'a parking_lot::Mutex<std::collections::HashSet<String>>,
    String,
);

impl<'a> Relayed<'a> {
    fn mark(set: &'a parking_lot::Mutex<std::collections::HashSet<String>>, sid: &str) -> Self {
        set.lock().insert(sid.to_string());
        Self(set, sid.to_string())
    }
}

impl Drop for Relayed<'_> {
    fn drop(&mut self) {
        self.0.lock().remove(&self.1);
    }
}

/// A sender that delivers each message as tagged and again tagged `old`: a
/// DM left open on a session the turn rolled over from still hears it.
fn both(
    tx: tokio::sync::broadcast::Sender<BroadcastMessage>,
    old: String,
) -> tokio::sync::broadcast::Sender<BroadcastMessage> {
    use tokio::sync::broadcast::error::RecvError;
    let (relay, mut rx) = tokio::sync::broadcast::channel::<BroadcastMessage>(1024);
    tokio::spawn(async move {
        loop {
            match rx.recv().await {
                Ok(message) => {
                    let mut copy = message.clone();
                    copy.session_id = old.clone();
                    let _ = tx.send(message);
                    let _ = tx.send(copy);
                }
                Err(RecvError::Lagged(_)) => continue,
                Err(RecvError::Closed) => return,
            }
        }
    });
    relay
}

impl CrewHub {
    /// The prompt just saved in `service`'s DM session `sid`, marked `relay`,
    /// is `author`'s: kept by its message, and sent to an open DM as it
    /// reads in history.
    async fn relayed(
        &self,
        service: &AgentService,
        sid: &str,
        relay: &str,
        author: Speaker,
        tx: &tokio::sync::broadcast::Sender<BroadcastMessage>,
    ) {
        // Saved, marked `relay`, before the turn began.
        let message = service.sessions.get(sid).ok().flatten().and_then(|s| {
            s.messages
                .iter()
                .rev()
                .find(|m| m.role == MessageRole::User && m.external_id.as_deref() == Some(relay))
                .map(|m| (m.id.clone(), m.content.clone()))
        });
        let Some((id, content)) = message else {
            tracing::warn!(session_id = %sid, "a message to the crew was not in its DM");
            return;
        };
        if let Err(e) = self
            .desk
            .store()
            .put_author(&id, service.agent_id.as_str(), &author)
            .await
        {
            tracing::warn!(error = %e, "keeping a message's author failed");
        }
        let frame = BroadcastMessage {
            id: Some(id),
            author: serde_json::to_value(&author).ok(),
            ..BroadcastMessage::silent(sid, USER_MESSAGE, crate::desk_surface::shown(&content))
        };
        let _ = tx.send(frame);
    }

    /// The overseer's turn in `old` runs in its successor `new`: what is
    /// kept about the turn follows it.
    fn rolled(&self, old: &str, new: &str) {
        let mut turns = self.manager_turns.lock();
        if let Some(at) = turns.get(old).copied() {
            turns.insert(new.to_string(), at);
        }
        drop(turns);
        let mut listed = self.listed.lock();
        if let Some(l) = listed.get(old).cloned() {
            listed.insert(new.to_string(), l);
        }
        drop(listed);
        self.mind_turn_rolled(old, new);
        // What it decides on goes with it.
        self.picture_turn_rolled(old, new);
    }
}
