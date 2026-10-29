//! Agents' turns, as activity: a [`TurnHost`] that runs the turn loop it
//! wraps unchanged and records each turn's start (a line of what was asked)
//! and end (a line of the answer, the question it asks the person, or the
//! failure), from the digest helpers — no model call of its own. Turns of a
//! hub without the desk pass straight through.

use std::sync::Arc;

use async_trait::async_trait;
use chrono::Utc;
use eyre::Result;
use serde_json::json;
use tokio::sync::mpsc;
use workforce_agent::runner::ProgressEvent;
use workforce_common::channels::DESK_CHANNEL;
use workforce_common::session::SessionAttachment;
use workforce_desk::crew::{self, Event, Kind, Party};

use super::{CrewHub, Running, CHECK_IN};
use crate::hosts::TurnHost;
use crate::serve::{AgentService, IsolatedRun, MessageProgress, RunAuthor, TurnInbox, TurnOutcome};

/// The turn loop `T`, with its turns recorded for the crew.
pub struct Observed<T>(pub T);

/// One turn being recorded.
pub(super) struct Track {
    crew: Arc<CrewHub>,
    key: String,
    party: Party,
    task: Option<String>,
    hop: i16,
    /// The overseer's own turn in its DM: not activity, but what its
    /// answer mentions is kept.
    manager_session: Option<String>,
    done: bool,
}

fn line(text: &str) -> Option<String> {
    crate::session_digest::first_sentence(text)
}

impl CrewHub {
    /// Starts recording a turn of `service`'s, unless it is one the crew
    /// leaves out.
    pub(super) fn track(
        &self,
        service: &AgentService,
        channel: &str,
        user: &str,
        message: &str,
        session_id: Option<&str>,
    ) -> Option<Track> {
        // Housekeeping: heartbeat checks and monitors report to their own
        // channels; they are not the crew's work.
        if !workforce_common::subscription::personalizes_prompt(user) {
            return None;
        }
        let agent_id = service.agent_id.as_str().to_string();
        let overseer = self
            .overseer_service()
            .is_some_and(|(s, _)| s.agent_id == service.agent_id);
        let party = Party::agent(&agent_id, &super::name_of(service));
        let hop = session_id
            .and_then(|sid| self.hops.lock().remove(sid))
            .unwrap_or(0);
        if overseer && channel == DESK_CHANNEL {
            if !message.starts_with(CHECK_IN) {
                self.person_spoke();
            }
            let sid = session_id?.to_string();
            self.manager_turns.lock().insert(sid.clone(), crew::now());
            return Some(Track {
                crew: self.me(),
                key: String::new(),
                party,
                task: None,
                hop,
                manager_session: Some(sid),
                done: false,
            });
        }
        if message.starts_with(CHECK_IN) {
            return None;
        }
        let task = line(message);
        let key = uuid::Uuid::new_v4().to_string();
        let now = Utc::now();
        self.running.lock().insert(
            key.clone(),
            Running {
                agent_id,
                task: task.clone(),
                detail: None,
                since: now,
                last_activity: now,
            },
        );
        self.desk.note(
            Event::new(
                Kind::Started,
                party.clone(),
                Some(party.clone()),
                match &task {
                    Some(t) => format!("started “{t}”"),
                    None => "started".to_string(),
                },
            )
            .hop(hop)
            .detail(json!({ "task": task })),
        );
        Some(Track {
            crew: self.me(),
            key,
            party,
            task,
            hop,
            manager_session: None,
            done: false,
        })
    }

    fn tool_started(&self, key: &str, name: &str) {
        if let Some(r) = self.running.lock().get_mut(key) {
            r.detail = Some(format!("● {name}"));
            r.last_activity = Utc::now();
        }
    }
}

impl Track {
    /// The turn ended with `outcome` (the answer, or why it failed).
    async fn finish(mut self, outcome: Result<&str, String>) {
        self.done = true;
        let crew = Arc::clone(&self.crew);
        if let Some(sid) = self.manager_session.take() {
            if let Ok(answer) = outcome {
                crew.keep_extras(&self.party.id, &sid, answer).await;
            }
            crew.manager_turns.lock().remove(&sid);
            return;
        }
        crew.running.lock().remove(&self.key);
        let me = || Some(self.party.clone());
        let event = match outcome {
            Ok(answer) => {
                let answer_line = line(answer);
                let detail = |question: Option<serde_json::Value>| json!({ "task": self.task, "answer": answer_line, "question": question });
                if crew::hit_limit(answer) {
                    Event::new(Kind::Limit, self.party.clone(), me(), "hit a usage limit")
                        .detail(detail(None))
                } else if let Some(q) = crew::question_in(answer) {
                    let options = crew::options_in(answer, &q);
                    Event::new(
                        Kind::Question,
                        self.party.clone(),
                        me(),
                        format!("asked “{q}”"),
                    )
                    .detail(detail(Some(json!({ "text": q, "options": options }))))
                } else {
                    Event::new(
                        Kind::Finished,
                        self.party.clone(),
                        me(),
                        match &answer_line {
                            Some(a) => format!("finished · {a}"),
                            None => "finished".into(),
                        },
                    )
                    .detail(detail(None))
                }
            }
            Err(e) if crew::hit_limit(&e) => {
                Event::new(Kind::Limit, self.party.clone(), me(), "hit a usage limit")
                    .detail(json!({ "task": self.task }))
            }
            Err(e) => Event::new(
                Kind::Failed,
                self.party.clone(),
                me(),
                format!("failed: {}", crew::one_line(&e, 120)),
            )
            .detail(json!({ "task": self.task })),
        };
        crew.desk.note(event.hop(self.hop));
    }
}

impl Drop for Track {
    /// A turn stopped before it ended is no longer running.
    fn drop(&mut self) {
        if self.done {
            return;
        }
        if let Some(sid) = &self.manager_session {
            self.crew.manager_turns.lock().remove(sid);
            return;
        }
        self.crew.running.lock().remove(&self.key);
        self.crew.desk.note(
            Event::new(
                Kind::Stopped,
                self.party.clone(),
                Some(self.party.clone()),
                "was stopped",
            )
            .hop(self.hop),
        );
    }
}

/// Aborts the turn it guards when dropped: an aborted wrapper stops the
/// turn inside it, as aborting the turn itself would.
struct AbortOnDrop(tokio::task::AbortHandle);

impl Drop for AbortOnDrop {
    fn drop(&mut self) {
        self.0.abort();
    }
}

/// `progress` with its turn recorded: its tools mark the agent busy, and
/// its end is recorded once the turn's own task ends.
fn observed(progress: MessageProgress, track: Track) -> MessageProgress {
    let MessageProgress {
        progress_rx: mut inner_rx,
        handle: inner,
        session_id,
        turn_token,
        turn_claim_id,
        cancel_token,
        code_citations,
        served,
    } = progress;
    let (tx, progress_rx) = mpsc::channel(64);
    let crew = Arc::clone(&track.crew);
    let key = track.key.clone();
    tokio::spawn(async move {
        let mut reading = true;
        while let Some(event) = inner_rx.recv().await {
            if let ProgressEvent::ToolStart { name, .. } = &event {
                crew.tool_started(&key, name);
            }
            // Once nobody reads the progress, keep draining it so the turn
            // never blocks on it.
            if reading && tx.send(event).await.is_err() {
                reading = false;
            }
        }
    });
    let handle = tokio::spawn(async move {
        let guard = AbortOnDrop(inner.abort_handle());
        let joined = inner.await;
        drop(guard);
        match joined {
            Ok(result) => {
                let outcome = match &result {
                    Ok(r) => Ok(r.response.as_str()),
                    Err(e) => Err(format!("{e:#}")),
                };
                track.finish(outcome).await;
                result
            }
            Err(e) if e.is_panic() => std::panic::resume_unwind(e.into_panic()),
            Err(_) => {
                drop(track);
                Err(eyre::eyre!("the turn was cancelled"))
            }
        }
    });
    MessageProgress {
        progress_rx,
        handle,
        session_id,
        turn_token,
        turn_claim_id,
        cancel_token,
        code_citations,
        served,
    }
}

#[async_trait]
impl<T: TurnHost> TurnHost for Observed<T> {
    #[allow(clippy::too_many_arguments)]
    async fn process_message(
        &self,
        service: &Arc<AgentService>,
        channel: &str,
        user: &str,
        message: &str,
        external_id: Option<&str>,
        channel_system_prompt: Option<&str>,
        channel_id: Option<&str>,
        thread_id: Option<&str>,
        attachments: Option<Vec<SessionAttachment>>,
        max_rounds_override: Option<u32>,
        model_override: Option<&str>,
        task_id: Option<&str>,
        trust_level: Option<workforce_common::trust::TrustLevel>,
        run_as: Option<&str>,
    ) -> Result<String> {
        let track = super::for_service(service)
            .and_then(|c| c.track(service, channel, user, message, None));
        let result = self
            .0
            .process_message(
                service,
                channel,
                user,
                message,
                external_id,
                channel_system_prompt,
                channel_id,
                thread_id,
                attachments,
                max_rounds_override,
                model_override,
                task_id,
                trust_level,
                run_as,
            )
            .await;
        if let Some(track) = track {
            track
                .finish(result.as_deref().map_err(|e| format!("{e:#}")))
                .await;
        }
        result
    }

    #[allow(clippy::too_many_arguments)]
    async fn process_message_with_progress(
        &self,
        service: &Arc<AgentService>,
        channel: &str,
        user: &str,
        run_as: Option<&str>,
        message: &str,
        external_id: Option<&str>,
        session_id: Option<&str>,
        attachments: Option<Vec<SessionAttachment>>,
        channel_system_prompt: Option<&str>,
        channel_id: Option<&str>,
        thread_id: Option<&str>,
        inbox: Option<TurnInbox>,
        display_content: Option<&str>,
        ticket_id: Option<&str>,
        step_max_rounds: Option<u32>,
        step_model: Option<&str>,
        step_context_eviction_interval: Option<u32>,
        step_context_eviction_keep_last: Option<u32>,
        tool_whitelist: Option<&[String]>,
        persona_override: Option<&str>,
        workspace_id: Option<&str>,
        trust_level: Option<workforce_common::trust::TrustLevel>,
        cancel_token: Option<tokio_util::sync::CancellationToken>,
        voice_channel: Option<&str>,
        desk: Option<&workforce_common::channels::desk::DeskTurnContext>,
    ) -> Result<MessageProgress> {
        let track = super::for_service(service)
            .and_then(|c| c.track(service, channel, user, message, session_id));
        let progress = self
            .0
            .process_message_with_progress(
                service,
                channel,
                user,
                run_as,
                message,
                external_id,
                session_id,
                attachments,
                channel_system_prompt,
                channel_id,
                thread_id,
                inbox,
                display_content,
                ticket_id,
                step_max_rounds,
                step_model,
                step_context_eviction_interval,
                step_context_eviction_keep_last,
                tool_whitelist,
                persona_override,
                workspace_id,
                trust_level,
                cancel_token,
                voice_channel,
                desk,
            )
            .await;
        match (progress, track) {
            (Ok(progress), Some(track)) => Ok(observed(progress, track)),
            (Err(e), Some(track)) => {
                track.finish(Err(format!("{e:#}"))).await;
                Err(e)
            }
            (progress, None) => progress,
        }
    }

    #[allow(clippy::too_many_arguments)]
    async fn process_turn(
        &self,
        service: &Arc<AgentService>,
        channel: &str,
        user: &str,
        message: &str,
        external_id: Option<&str>,
        channel_system_prompt: Option<&str>,
        channel_id: Option<&str>,
        thread_id: Option<&str>,
        attachments: Option<Vec<SessionAttachment>>,
        max_rounds_override: Option<u32>,
        model_override: Option<&str>,
        task_id: Option<&str>,
        trust_level: Option<workforce_common::trust::TrustLevel>,
        run_as: Option<&str>,
        author: RunAuthor,
    ) -> Result<TurnOutcome> {
        let track = super::for_service(service)
            .and_then(|c| c.track(service, channel, user, message, None));
        let result = self
            .0
            .process_turn(
                service,
                channel,
                user,
                message,
                external_id,
                channel_system_prompt,
                channel_id,
                thread_id,
                attachments,
                max_rounds_override,
                model_override,
                task_id,
                trust_level,
                run_as,
                author,
            )
            .await;
        if let Some(track) = track {
            match &result {
                Ok(TurnOutcome::Answered(answer)) => track.finish(Ok(answer)).await,
                Ok(_) => {
                    let mut track = track;
                    track.done = true;
                    track.crew.running.lock().remove(&track.key);
                }
                Err(e) => track.finish(Err(format!("{e:#}"))).await,
            }
        }
        result
    }

    async fn run_isolated(
        &self,
        service: &Arc<AgentService>,
        channel: &str,
        task: &str,
        model_override: Option<&str>,
        system_prompt_override: Option<&str>,
        run: &IsolatedRun,
        profile: crate::hub_core::jobs::AgentProfile,
        curation: Option<(
            crate::hub_core::curation::CurationCtx,
            workforce_common::learning_plan::CurationMode,
        )>,
        task_label: Option<&str>,
    ) -> Result<crate::hub_core::jobs::AgentRunResult> {
        self.0
            .run_isolated(
                service,
                channel,
                task,
                model_override,
                system_prompt_override,
                run,
                profile,
                curation,
                task_label,
            )
            .await
    }

    async fn has_route(&self, service: &AgentService, model: Option<&str>) -> bool {
        self.0.has_route(service, model).await
    }

    async fn admit_turn_or_deny(
        &self,
        service: &AgentService,
        identity: Option<&str>,
        channel_id: Option<&str>,
    ) -> Option<String> {
        self.0
            .admit_turn_or_deny(service, identity, channel_id)
            .await
    }
}
