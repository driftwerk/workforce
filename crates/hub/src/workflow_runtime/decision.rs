//! Decision steps on the hub: who the deciders are, the cards that ask
//! them, the one fenced settlement every surface goes through, and carrying a
//! settled decision out. The pure state
//! transitions are [`workforce_orchestrator::workflow::decision`]'s.

use std::collections::HashSet;
use std::sync::Arc;

use chrono::Utc;
use eyre::eyre;
use tracing::{info, warn};
use uuid::Uuid;
use workforce_common::channels::{ChannelSurface, OutboundMessage};
use workforce_common::persistence::WorkflowInstanceRow;
use workforce_identity::types::{Identity, IdentityStatus};
use workforce_orchestrator::workflow::decision::DecisionSpec;
use workforce_orchestrator::workflow::decision::{
    self as run_decision, Card, CardTarget, Closed, DecidedBy, Decider, Opening, Person, Refusal,
    RunDecision,
};
use workforce_orchestrator::workflow::definition::{StepDefinition, WorkflowDefinition};
use workforce_orchestrator::workflow::engine::WorkflowExecutor;
use workforce_orchestrator::workflow::instance::{
    Actor, Intervention, InterventionKind, WorkflowInstance,
};

use super::{
    anchored, clear_child_started, delivery_context, instance_of, instance_principal, principal_of,
    send_status_message, ChildLaunch, WorkflowRuntime, REJECTED,
};
use crate::def_write_auth::DefWriter;
pub use crate::hub_core::workflows::{
    DecisionAsk, DecisionMade, DecisionRefused, ResumeAsk, ResumeRefused, Resumed,
};
use crate::recipients::{self, Recipient, RecipientRef};
use crate::run_steering::{self, Standing, SteerAction, SteerRefused, Steerer};
use crate::slack_blocks::{self, CardState, DecisionCardView};

/// How long a decision may stand unposted before the sweep posts its cards:
/// past any loop still on its way to posting them.
const UNDELIVERED_AFTER: chrono::Duration = chrono::Duration::seconds(120);

/// One settlement, by a person or by expiry.
pub(crate) struct Settle {
    pub gate: Option<Uuid>,
    pub choice: String,
    pub by: SettledBy,
    pub feedback: Option<String>,
}

pub(crate) enum SettledBy {
    Person {
        steerer: Steerer,
        person: Option<Person>,
        display: String,
    },
    Expiry,
}

/// What delivering a decision's cards found when it went to record them.
enum Recorded {
    Fresh,
    /// Another delivery recorded its cards first, or the decision is gone.
    Duplicate,
    /// The decision closed while its cards were on their way.
    Closed,
}

/// Where a run's loop goes after following a decision.
pub(super) enum Followed {
    Continue,
    Stop,
}

/// The labels of `decision`'s choices, as a person reads them.
fn choices_of(decision: &RunDecision) -> String {
    decision
        .choices
        .iter()
        .map(|c| c.label.as_str())
        .collect::<Vec<_>>()
        .join(" · ")
}

/// Who `decision` waits on, by name, as one alternative.
fn deciders_of(decision: &RunDecision, starter: Option<&str>) -> String {
    slack_blocks::either(
        &decision
            .waiting_on(starter)
            .iter()
            .map(|d| d.display.clone())
            .collect::<Vec<_>>(),
    )
}

/// The refusal a steer gets while `decision` is open.
pub(super) fn awaiting(decision: &RunDecision, starter: Option<&str>) -> SteerRefused {
    SteerRefused::AwaitingDecision {
        choices: choices_of(decision),
        deciders: deciders_of(decision, starter),
    }
}

/// The starter of `instance`, as the steering rule reads it.
fn starter_of(instance: &WorkflowInstance) -> Option<String> {
    run_steering::starter(&instance_principal(instance)).map(str::to_string)
}

/// A decider as the identity store knows them.
fn decider_of(identity: &Identity, slack_user_id: Option<String>) -> Decider {
    Decider {
        identity_id: Some(identity.id.to_string()),
        email: identity.email.clone(),
        slack_user_id: identity.linked_ids.get("slack").cloned().or(slack_user_id),
        display: identity.display_name.clone(),
    }
}

/// Whether two deciders are the same person by any attribute both know.
fn same_decider(a: &Decider, b: &Decider) -> bool {
    let both = |x: &Option<String>, y: &Option<String>| x.is_some() && x == y;
    both(&a.identity_id, &b.identity_id)
        || both(&a.slack_user_id, &b.slack_user_id)
        || matches!((&a.email, &b.email), (Some(x), Some(y)) if x.eq_ignore_ascii_case(y))
}

/// The view a card shows of `decision`, as `starter` leaves it waiting.
fn card_view<'a>(decision: &'a RunDecision, starter: Option<&str>) -> DecisionCardView<'a> {
    let state = match &decision.closed {
        None => CardState::Open,
        Some(Closed::Decided {
            outcome, by, at, ..
        }) => {
            let label = decision
                .choice(outcome)
                .map_or(outcome.as_str(), run_decision::Choice::done);
            match by {
                DecidedBy::Person {
                    display,
                    slack_user_id,
                    ..
                } => CardState::Decided {
                    label,
                    by_mention: slack_user_id
                        .as_deref()
                        .map_or_else(|| display.clone(), |id| format!("<@{id}>")),
                    at: *at,
                },
                DecidedBy::Expiry => CardState::Expired { label, at: *at },
            }
        }
        Some(Closed::Stopped { by, .. }) => CardState::Stopped { by: by.as_deref() },
    };
    DecisionCardView {
        title: &decision.title,
        context: decision.context.as_deref(),
        body: decision.body.as_deref(),
        choices: &decision.choices,
        waiting_on: decision
            .waiting_on(starter)
            .iter()
            .map(|d| d.mention())
            .collect(),
        expires: decision.expires_at.zip(
            decision
                .on_expiry
                .as_deref()
                .and_then(|outcome| decision.choice(outcome))
                .map(|c| c.label.as_str()),
        ),
        state,
    }
}

/// The sentence a card that cannot be posted fails its step with.
fn undeliverable(error: &str, channel: Option<&str>) -> String {
    let at = channel.map_or_else(|| "the channel".to_string(), |c| format!("<#{c}>"));
    if error.contains("not_in_channel") || error.contains("channel_not_found") {
        format!("The Workforce app is not in {at}. Invite it to the channel, then retry the step.")
    } else if error.contains("is_archived") {
        format!("{at} is archived.")
    } else {
        error.to_string()
    }
}

/// Record the cards a delivery posted on the decision `gate`, anchoring a run
/// with no thread (or no channel) to the first.
fn record_cards(instance: &mut WorkflowInstance, gate: Uuid, cards: &[Card]) -> Recorded {
    let Some(decision) = RunDecision::read(&instance.variables).filter(|d| d.gate == gate) else {
        return Recorded::Duplicate;
    };
    if decision.posted {
        return Recorded::Duplicate;
    }
    let unanchored = match &decision.target {
        CardTarget::Thread { thread: None, .. } => true,
        CardTarget::Channel { .. } => instance.variables.get("_channel_id").is_none(),
        _ => false,
    };
    if let Some(first) = cards.first().filter(|_| unanchored) {
        anchored(instance, Some(&first.channel), &first.ts);
    }
    let closed = decision.closed.is_some();
    RunDecision {
        cards: cards.to_vec(),
        posted: true,
        ..decision
    }
    .write(&mut instance.variables);
    match closed {
        true => Recorded::Closed,
        false => Recorded::Fresh,
    }
}

/// Fail the step whose decision `gate` could not be posted, when it still
/// stands open and unposted: its record is failed so a retry runs it afresh.
fn fail_opening(instance: &mut WorkflowInstance, gate: Uuid, reason: &str) -> bool {
    let failing = RunDecision::read(&instance.variables)
        .filter(|d| d.gate == gate && d.closed.is_none() && !d.posted);
    if let Some(decision) = &failing {
        instance.fail_last_record(&decision.step_id);
        RunDecision::remove(&mut instance.variables);
        WorkflowExecutor::fail(instance, reason);
    }
    failing.is_some()
}

impl WorkflowRuntime {
    /// The person `identity_id` names, while they are not suspended.
    pub(crate) fn person(&self, identity_id: Option<&str>) -> Option<Person> {
        let store = self.service.identity_store.read().clone()?;
        let identity = identity_id
            .and_then(|id| Uuid::parse_str(id).ok())
            .and_then(|id| store.get_by_id(id).ok().flatten())
            .filter(|identity| identity.status != IdentityStatus::Suspended)?;
        Some(Person {
            id: identity.id.to_string(),
            email: identity.email.clone(),
            slack_user_id: identity.linked_ids.get("slack").cloned(),
        })
    }

    /// How `identity_id` reads to a person.
    fn display_of(&self, identity_id: &str) -> String {
        recipients::display_name(
            self.service.identity_store.read().clone().as_deref(),
            identity_id,
        )
    }

    /// Who steered, as a closed card names them.
    pub(super) fn actor_display(&self, actor: &Actor) -> Option<String> {
        match actor {
            Actor::Person { id } => Some(self.display_of(id)),
            Actor::Agent { id } => Some(id.clone()),
            Actor::System => None,
        }
    }

    /// Every decider `tokens` name, with each attribute known now. A team is
    /// its active members; a suspended person is left out; an email or a
    /// Slack mention nobody here holds is still a decider when Slack knows
    /// them. Any token nothing answers to fails the whole resolution.
    pub(crate) async fn resolve_deciders(
        &self,
        tokens: &[String],
        surface: Option<&Arc<dyn ChannelSurface>>,
    ) -> Result<Vec<Decider>, String> {
        let identities = self
            .service
            .identity_store
            .read()
            .clone()
            .ok_or_else(|| recipients::NO_IDENTITY_STORE.to_string())?;
        let teams = self.service.team_store.read().clone();
        let slack_id = |email: Option<String>| async move {
            match (surface, email) {
                (Some(surface), Some(email)) => surface.lookup_user_id_by_email(&email).await.ok(),
                _ => None,
            }
        };
        let display_in_slack = |user: String| async move {
            match surface {
                Some(surface) => surface
                    .member_info(&user)
                    .await
                    .ok()
                    .map(|info| info.display_name)
                    .filter(|name| !name.is_empty()),
                None => None,
            }
        };
        let mut resolved: Vec<Decider> = Vec::new();
        for token in tokens {
            let found: Vec<Decider> = match recipients::parse(token) {
                RecipientRef::Team(_) => {
                    let Recipient::Team { id, .. } =
                        recipients::resolve(identities.as_ref(), teams.as_deref(), token)?
                    else {
                        return Err(recipients::person_not_team("decider"));
                    };
                    let members = Uuid::parse_str(&id)
                        .ok()
                        .and_then(|id| teams.as_ref()?.get_team(id).ok().flatten())
                        .map(|team| team.members)
                        .unwrap_or_default();
                    let mut deciders = Vec::new();
                    for member in members {
                        if let Some(identity) = identities
                            .get_by_id(member)
                            .ok()
                            .flatten()
                            .filter(|i| i.status == IdentityStatus::Active)
                        {
                            let slack = slack_id(identity.email.clone()).await;
                            deciders.push(decider_of(&identity, slack));
                        }
                    }
                    deciders
                }
                RecipientRef::IdentityId(_) => {
                    let Recipient::User { id, .. } =
                        recipients::resolve(identities.as_ref(), teams.as_deref(), token)?
                    else {
                        return Err(recipients::person_not_team("decider"));
                    };
                    match Uuid::parse_str(&id)
                        .ok()
                        .and_then(|id| identities.get_by_id(id).ok().flatten())
                        .filter(|i| i.status != IdentityStatus::Suspended)
                    {
                        Some(identity) => {
                            let slack = slack_id(identity.email.clone()).await;
                            vec![decider_of(&identity, slack)]
                        }
                        None => Vec::new(),
                    }
                }
                RecipientRef::Email(email) => match identities.get_by_email(email).ok().flatten() {
                    Some(identity) if identity.status == IdentityStatus::Suspended => Vec::new(),
                    Some(identity) => {
                        let slack = slack_id(identity.email.clone()).await;
                        vec![decider_of(&identity, slack)]
                    }
                    None => {
                        let Some(slack) = slack_id(Some(email.to_string())).await else {
                            return Err(format!(
                                "nobody has the email `{email}` in Workforce or in Slack"
                            ));
                        };
                        let display = display_in_slack(slack.clone())
                            .await
                            .unwrap_or_else(|| email.to_string());
                        vec![Decider {
                            identity_id: None,
                            email: Some(email.to_string()),
                            slack_user_id: Some(slack),
                            display,
                        }]
                    }
                },
                RecipientRef::SlackUser(user) => {
                    match identities.get_by_linked_id("slack", user).ok().flatten() {
                        Some(identity) if identity.status == IdentityStatus::Suspended => {
                            Vec::new()
                        }
                        Some(identity) => vec![decider_of(&identity, Some(user.to_string()))],
                        None => {
                            let display = display_in_slack(user.to_string())
                                .await
                                .unwrap_or_else(|| format!("<@{user}>"));
                            vec![Decider {
                                identity_id: None,
                                email: None,
                                slack_user_id: Some(user.to_string()),
                                display,
                            }]
                        }
                    }
                }
            };
            resolved.extend(found);
        }
        Ok(resolved.into_iter().fold(Vec::new(), |mut kept, decider| {
            if !kept.iter().any(|k| same_decider(k, &decider)) {
                kept.push(decider);
            }
            kept
        }))
    }

    /// The decision the step `step` that just paused opens, its deciders
    /// resolved and its text made safe for a channel.
    pub(super) async fn open_decision(
        &self,
        def: &WorkflowDefinition,
        step: &StepDefinition,
        spec: &DecisionSpec,
        instance: &WorkflowInstance,
        body: Option<String>,
        step_agent_id: &str,
    ) -> Result<RunDecision, String> {
        let (channel, thread) = delivery_context(instance);
        let surface = self
            .get_surface_for_agent(Some(step_agent_id), instance.agent_id.as_deref())
            .await;
        let tokens = spec.decider_tokens(def)?;
        let deciders = self.resolve_deciders(&tokens, surface.as_ref()).await?;
        let starter = starter_of(instance);
        RunDecision::open(
            spec,
            step,
            instance,
            Opening {
                deciders,
                target: run_decision::target(
                    spec.channel.as_deref(),
                    channel.as_deref(),
                    thread.as_deref(),
                ),
                body,
                agent: Some(step_agent_id.to_string()),
                starter: starter.as_deref(),
                now: Utc::now(),
            },
        )
        .map(|decision| decision.cleaned(slack_blocks::card_text))
    }

    /// Post the cards of the decision `gate` on `instance_id`, once: record
    /// them, delete them when another delivery got there first, close them at
    /// once when the decision closed meanwhile, and fail the step when none
    /// could be posted.
    pub(crate) async fn deliver_decision(self: &Arc<Self>, instance_id: &str, gate: Uuid) {
        let Ok(instance) = self.load_instance(instance_id) else {
            return;
        };
        let Some(decision) =
            RunDecision::read(&instance.variables).filter(|d| d.gate == gate && !d.posted)
        else {
            return;
        };
        let surface = self
            .get_surface_for_agent(decision.agent.as_deref(), instance.agent_id.as_deref())
            .await;
        let starter = starter_of(&instance);
        let posted = match &surface {
            Some(surface) => {
                Self::post_cards(surface, &decision, instance_id, starter.as_deref()).await
            }
            None => Err("no Slack surface can post the decision's card".to_string()),
        };
        let cards = match posted {
            Ok(cards) if !cards.is_empty() => cards,
            Ok(_) => {
                let reason = match decision.target {
                    CardTarget::DirectMessages => "None of the deciders can be reached on Slack.",
                    _ => "The decision's card could not be posted.",
                };
                return self.fail_delivery(&instance, gate, reason).await;
            }
            Err(reason) => return self.fail_delivery(&instance, gate, &reason).await,
        };
        let recorded = self.update_instance(instance_id, |i| {
            Ok::<_, eyre::Report>(record_cards(i, gate, &cards))
        });
        match recorded {
            Ok(Recorded::Fresh) => {
                self.post_pointer(&instance, &decision, starter.as_deref())
                    .await
            }
            Ok(Recorded::Closed) => self.close_cards(instance_id).await,
            Ok(Recorded::Duplicate) => {
                if let Some(surface) = &surface {
                    for card in &cards {
                        if let Err(e) = surface.delete_message(&card.channel, &card.ts).await {
                            warn!(instance_id, error = %e, "Could not delete a duplicate decision card");
                        }
                    }
                }
            }
            Err(e) => warn!(instance_id, error = %e, "Could not record the decision's cards"),
        }
    }

    /// Post a card for each place `decision` asks, and its body's overflow
    /// in each card's thread. The cards that went out, or why none did.
    async fn post_cards(
        surface: &Arc<dyn ChannelSurface>,
        decision: &RunDecision,
        instance_id: &str,
        starter: Option<&str>,
    ) -> Result<Vec<Card>, String> {
        let (text, blocks) =
            slack_blocks::decision_card(&card_view(decision, starter), instance_id, decision.gate);
        let overflow = decision
            .body
            .as_deref()
            .map(|body| slack_blocks::decision_card_overflow(body).1)
            .unwrap_or_default();
        let post = |channel: String, thread: Option<String>, decider: Option<String>| {
            let (text, blocks, overflow) = (text.clone(), blocks.clone(), overflow.clone());
            async move {
                let message =
                    |text: &str, thread: Option<String>, blocks: Vec<serde_json::Value>| {
                        OutboundMessage {
                            channel: channel.clone(),
                            text: text.to_string(),
                            thread_id: thread,
                            username: None,
                            icon_url: None,
                            blocks: Some(blocks),
                        }
                    };
                let ts =
                    send_status_message(surface.as_ref(), &message(&text, thread.clone(), blocks))
                        .await
                        .map_err(|e| e.to_string())?;
                let replies = thread.unwrap_or_else(|| ts.clone());
                for group in overflow {
                    if let Err(e) = send_status_message(
                        surface.as_ref(),
                        &message(&text, Some(replies.clone()), group),
                    )
                    .await
                    {
                        warn!(instance_id, error = %e, "A decision body's overflow was not posted");
                        break;
                    }
                }
                Ok::<_, String>(Card {
                    channel: channel.clone(),
                    ts,
                    decider,
                })
            }
        };
        match &decision.target {
            CardTarget::Thread { channel, thread } => post(channel.clone(), thread.clone(), None)
                .await
                .map(|card| vec![card])
                .map_err(|e| undeliverable(&e, Some(channel))),
            CardTarget::Channel { channel } => post(channel.clone(), None, None)
                .await
                .map(|card| vec![card])
                .map_err(|e| undeliverable(&e, Some(channel))),
            CardTarget::DirectMessages => {
                let mut cards = Vec::new();
                for decider in decision.waiting_on(starter) {
                    let Some(user) = &decider.slack_user_id else {
                        continue;
                    };
                    let card = match surface.open_conversation(user).await {
                        Ok(channel) => post(channel, None, Some(decider.display.clone())).await,
                        Err(e) => Err(e.to_string()),
                    };
                    match card {
                        Ok(card) => cards.push(card),
                        Err(e) => {
                            warn!(instance_id, decider = %decider.display, error = %e, "Could not send a decider their decision card")
                        }
                    }
                }
                Ok(cards)
            }
        }
    }

    /// Fail the step whose decision `gate` could not be posted, and say so.
    async fn fail_delivery(
        self: &Arc<Self>,
        instance: &WorkflowInstance,
        gate: Uuid,
        reason: &str,
    ) {
        warn!(instance_id = %instance.id, %reason, "A decision's card could not be posted");
        let failed = self.update_instance(&instance.id, |i| {
            Ok::<_, eyre::Report>(fail_opening(i, gate, reason).then(|| i.clone()))
        });
        let failed = match failed {
            Ok(Some(failed)) => failed,
            Ok(None) => return,
            Err(e) => {
                warn!(instance_id = %instance.id, error = %e, "Could not fail a step whose decision was not posted");
                return;
            }
        };
        let step_name = match failed.current_step_id.as_deref() {
            Some(step) => self
                .definitions
                .read()
                .await
                .get(&failed.workflow_id)
                .and_then(|o| o.def.get_step(step).map(|s| s.name.clone()))
                .unwrap_or_else(|| step.to_string()),
            None => "unknown".to_string(),
        };
        let (channel, thread) = delivery_context(&failed);
        self.announce_failure(
            &failed,
            reason,
            &Self::stopped_card(&step_name, reason),
            channel.as_deref(),
            thread.as_deref(),
            failed.agent_id.as_deref(),
            failed.agent_id.as_deref(),
        )
        .await;
        self.promote_queued_after(&failed.id).await;
    }

    /// Tell the run's thread where a card it cannot hold went.
    async fn post_pointer(
        &self,
        instance: &WorkflowInstance,
        decision: &RunDecision,
        starter: Option<&str>,
    ) {
        let (Some(channel), thread) = delivery_context(instance) else {
            return;
        };
        let mentions = slack_blocks::either(
            &decision
                .waiting_on(starter)
                .iter()
                .map(|d| d.mention())
                .collect::<Vec<_>>(),
        );
        let place = match &decision.target {
            CardTarget::Channel { channel } => format!("The card is in <#{channel}>."),
            CardTarget::DirectMessages => {
                "The card has been sent to each of them directly.".to_string()
            }
            CardTarget::Thread { .. } => return,
        };
        self.send_status(
            &format!("⏸ *{}* is waiting on {mentions}. {place}", decision.title),
            Some(&channel),
            thread.as_deref(),
            decision.agent.as_deref(),
            instance.agent_id.as_deref(),
        )
        .await;
    }

    /// Rewrite every card of the run's closed decision to its closed state,
    /// say how it closed in the run's thread when the cards were elsewhere,
    /// and note the cards closed. The one way cards close.
    pub(super) async fn close_cards(&self, instance_id: &str) {
        let Some((instance, decision)) = self
            .load_instance(instance_id)
            .ok()
            .and_then(|i| run_decision::cards_to_close(&i).map(|d| (i, d)))
        else {
            return;
        };
        let surface = self
            .get_surface_for_agent(decision.agent.as_deref(), instance.agent_id.as_deref())
            .await;
        let starter = starter_of(&instance);
        let (text, blocks) = slack_blocks::decision_card(
            &card_view(&decision, starter.as_deref()),
            instance_id,
            decision.gate,
        );
        if let Some(surface) = &surface {
            for card in &decision.cards {
                let message = OutboundMessage {
                    channel: card.channel.clone(),
                    text: text.clone(),
                    thread_id: None,
                    username: None,
                    icon_url: None,
                    blocks: Some(blocks.clone()),
                };
                if let Err(e) = surface
                    .update_message(&card.channel, &card.ts, &message)
                    .await
                {
                    warn!(instance_id, error = %e, "Could not close a decision card");
                }
            }
        }
        if !matches!(decision.target, CardTarget::Thread { .. }) {
            if let Some(result) = Self::result_line(&decision) {
                let (channel, thread) = delivery_context(&instance);
                self.send_status(
                    &result,
                    channel.as_deref(),
                    thread.as_deref(),
                    decision.agent.as_deref(),
                    instance.agent_id.as_deref(),
                )
                .await;
            }
        }
        let gate = decision.gate;
        if let Err(e) = self.update_instance(instance_id, |i| {
            if let Some(decision) = RunDecision::read(&i.variables).filter(|d| d.gate == gate) {
                RunDecision {
                    cards_closed: true,
                    ..decision
                }
                .write(&mut i.variables);
            }
            Ok::<_, eyre::Report>(())
        }) {
            warn!(instance_id, error = %e, "Could not note a decision's cards closed");
        }
    }

    /// How a closed decision ended, for a run thread its cards are not in.
    fn result_line(decision: &RunDecision) -> Option<String> {
        Some(match decision.closed.as_ref()? {
            Closed::Decided { outcome, by, .. } => {
                let done = decision
                    .choice(outcome)
                    .map_or(outcome.as_str(), run_decision::Choice::done);
                match by {
                    DecidedBy::Person { display, .. } => {
                        format!("✅ *{done}*, decided by {display}")
                    }
                    DecidedBy::Expiry => format!("⌛ Expired: *{done}*"),
                }
            }
            Closed::Stopped { by: Some(by), .. } => {
                format!("⏹ Stopped by {by}; nothing was decided")
            }
            Closed::Stopped { by: None, .. } => "⏹ Stopped; nothing was decided".to_string(),
        })
    }

    /// Close the run's open decision because the run was stopped, and its
    /// cards with it: also the cards of a decision settled just before the
    /// stop, whose loop now never runs to close them.
    pub(super) async fn stop_decision(&self, instance_id: &str, by: Option<String>) {
        if let Err(e) = self.update_instance(instance_id, |i| {
            Ok::<_, eyre::Report>(run_decision::stop(i, by.clone(), Utc::now()))
        }) {
            warn!(instance_id, error = %e, "Could not close a stopped run's decision");
        }
        self.close_cards(instance_id).await;
    }

    /// Settle what is due on every waiting decision: an expired one is
    /// pressed with its `on_expiry` choice, and one whose cards never went
    /// out is posted.
    pub(super) async fn decision_sweep(self: &Arc<Self>) {
        let rows = match self.list_instances(Some("paused"), None) {
            Ok(rows) => rows,
            Err(e) => {
                warn!(error = %e, "Decision sweep: failed to query paused instances");
                return;
            }
        };
        let now = Utc::now();
        let due = rows
            .into_iter()
            .filter_map(|row| instance_of(row).ok())
            .filter_map(|instance| run_decision::open_in(&instance).map(|d| (instance.id, d)));
        for (id, decision) in due {
            if let Some(choice) = decision.expired(now) {
                let settled = self
                    .settle(
                        &id,
                        Settle {
                            gate: Some(decision.gate),
                            choice: choice.outcome.clone(),
                            by: SettledBy::Expiry,
                            feedback: None,
                        },
                    )
                    .await;
                match settled {
                    Ok(made) => {
                        info!(instance_id = %id, outcome = %made.outcome, "Decision expired")
                    }
                    Err(e) => {
                        warn!(instance_id = %id, error = %e, "Decision sweep: an expiry settled nothing")
                    }
                }
            } else if !decision.posted && decision.opened_at + UNDELIVERED_AFTER <= now {
                info!(instance_id = %id, "Decision sweep: posting a decision's undelivered cards");
                self.deliver_decision(&id, decision.gate).await;
            }
        }
    }

    /// The single settlement every surface goes through. Inside one fenced
    /// change it checks the decision is open at the gate asked about, the
    /// choice is one it offers, the person may make it and its step still
    /// exists; then it records the choice and runs the run, whose loop
    /// carries the decision out.
    pub(crate) async fn settle(
        self: &Arc<Self>,
        instance_id: &str,
        ask: Settle,
    ) -> Result<DecisionMade, DecisionRefused> {
        let row = self
            .get_instance_status(instance_id)?
            .ok_or(DecisionRefused::NotWaiting)?;
        let variables: serde_json::Value = serde_json::from_str(&row.variables).unwrap_or_default();
        let done_for = principal_of(&variables);
        let starter = run_steering::starter(&done_for).map(str::to_string);
        let def_steps: Option<HashSet<String>> = self
            .definitions
            .read()
            .await
            .get(&row.workflow_id)
            .map(|owned| owned.def.steps.iter().map(|s| s.id.clone()).collect());
        let now = Utc::now();
        let settled = self.update_instance(instance_id, |i| {
            let Some(decision) = run_decision::open_in(i) else {
                let held = RunDecision::read(&i.variables);
                return Err(match held {
                    _ if i.is_terminal() => DecisionRefused::Ended {
                        status: i.status.to_string(),
                    },
                    Some(held)
                        if held.closed.is_some()
                            && ask.gate.is_none_or(|gate| gate == held.gate) =>
                    {
                        DecisionRefused::Decided {
                            summary: held.summary().unwrap_or_default(),
                        }
                    }
                    Some(held) if ask.gate.is_some_and(|gate| gate != held.gate) => {
                        DecisionRefused::Outdated
                    }
                    _ => DecisionRefused::NotWaiting,
                });
            };
            if ask.gate.is_some_and(|gate| gate != decision.gate) {
                return Err(DecisionRefused::Outdated);
            }
            let choice =
                decision
                    .choice(&ask.choice)
                    .cloned()
                    .ok_or_else(|| DecisionRefused::Choose {
                        choices: choices_of(&decision),
                    })?;
            let by = match &ask.by {
                SettledBy::Expiry => DecidedBy::Expiry,
                SettledBy::Person {
                    person, display, ..
                } => {
                    let deciders = || deciders_of(&decision, starter.as_deref());
                    match decision.refusal(person.as_ref(), starter.as_deref()) {
                        Some(Refusal::NotDecider) => {
                            return Err(SteerRefused::NotDecider {
                                deciders: deciders(),
                            }
                            .into())
                        }
                        Some(Refusal::OwnDecision) => {
                            return Err(SteerRefused::OwnDecision {
                                deciders: deciders(),
                            }
                            .into())
                        }
                        None => {}
                    }
                    let person = person.as_ref().expect("a decider is a person");
                    DecidedBy::Person {
                        id: person.id.clone(),
                        display: display.clone(),
                        slack_user_id: person.slack_user_id.clone(),
                    }
                }
            };
            match (&def_steps, &choice.next, &ask.by) {
                (None, _, SettledBy::Person { .. }) => return Err(DecisionRefused::WorkflowGone),
                (Some(steps), run_decision::Next::Step(step), _) if !steps.contains(step) => {
                    return Err(DecisionRefused::StepGone { step: step.clone() })
                }
                _ => {}
            }
            run_decision::settle(i, &choice, by, ask.feedback.as_deref(), now)
                .map_err(|_| DecisionRefused::NotWaiting)?;
            let decision = RunDecision::read(&i.variables).unwrap_or(decision);
            Ok((i.clone(), decision, choice))
        });
        let (steerer, standing) = match &ask.by {
            SettledBy::Person { steerer, .. } => (steerer.clone(), Standing::Decider),
            SettledBy::Expiry => (
                Steerer {
                    writer: DefWriter {
                        identity_id: None,
                        is_admin: false,
                    },
                    actor: Actor::System,
                },
                Standing::Decider,
            ),
        };
        let verdict = match &settled {
            Ok(_) => Some(Ok(standing)),
            Err(DecisionRefused::Steer(refused)) => Some(Err(refused.clone())),
            Err(_) => None,
        };
        if let Some(verdict) = &verdict {
            run_steering::recorded(
                instance_id,
                &done_for,
                &steerer,
                SteerAction::Decide,
                verdict,
            );
        }
        tracing::info!(
            target: "audit.workflow_run_decision",
            run = %instance_id,
            gate = ?ask.gate,
            outcome = %ask.choice,
            decided_by = ?steerer.actor,
            refused = settled.as_ref().err().map(tracing::field::display),
            "Workflow run decision"
        );
        let (instance, decision, choice) = settled?;
        let intervention = Intervention {
            outcome: Some(choice.outcome.clone()),
            applied: Some(true),
            ..instance.intervention(InterventionKind::Resume, steerer.actor.clone())
        };
        crate::decision_records::moved(&instance, intervention);
        let (channel_id, thread_id) = delivery_context(&instance);
        self.spawn_execute_loop(
            instance.id.clone(),
            channel_id,
            thread_id,
            instance.agent_id.clone(),
            instance_principal(&instance),
        );
        Ok(DecisionMade {
            outcome: choice.outcome.clone(),
            label: choice.label.clone(),
            done: choice.done().to_string(),
            summary: decision.summary().unwrap_or_default(),
        })
    }

    /// A person's press, on any surface. An agent working a workflow step
    /// acts for the person its run acts as, but never decides for them.
    pub(crate) async fn decide(
        self: &Arc<Self>,
        instance_id: &str,
        ask: DecisionAsk,
        steerer: &Steerer,
    ) -> Result<DecisionMade, DecisionRefused> {
        let identity = steerer.writer.identity_id.as_deref();
        let person = self.person(identity).filter(|_| !in_a_workflow_step());
        let display = identity.map_or_else(|| "someone".to_string(), |id| self.display_of(id));
        self.settle(
            instance_id,
            Settle {
                gate: ask.gate,
                choice: ask.choice,
                feedback: ask.feedback,
                by: SettledBy::Person {
                    steerer: steerer.clone(),
                    person,
                    display,
                },
            },
        )
        .await
    }

    /// Carry out the decision the run settled: start the child its choice
    /// names (once, attaching on a restart), then move to its step or end the
    /// run. Whether the loop goes on.
    pub(super) async fn follow_decision(
        self: &Arc<Self>,
        def: &WorkflowDefinition,
        instance: &mut WorkflowInstance,
        decision: RunDecision,
        announce: bool,
    ) -> eyre::Result<Followed> {
        let (channel_id, thread_id) = delivery_context(instance);
        let Some(Closed::Decided { outcome, child, .. }) = &decision.closed else {
            return Ok(Followed::Continue);
        };
        let choice = decision
            .choice(outcome)
            .ok_or_else(|| eyre!("the decision chose '{outcome}', which it never offered"))?;
        if self.stopped_during_step(&instance.id, &decision.step_id)? {
            return Ok(Followed::Stop);
        }
        let agent = decision
            .agent
            .clone()
            .unwrap_or_else(|| self.instance_agent(instance).to_string());
        let failed = match (&choice.start, child) {
            (Some(start), None) => {
                let started = self
                    .start_child_run(
                        instance,
                        &decision.step_id,
                        &start.workflow,
                        || Ok(serde_json::Value::Object(start.payload.clone())),
                        ChildLaunch {
                            agent_id: &agent,
                            channel_id: channel_id.as_deref(),
                            thread_id: thread_id.as_deref(),
                        },
                    )
                    .await;
                match started {
                    Ok(_) if self.stopped_during_step(&instance.id, &decision.step_id)? => {
                        self.cascade_abort_children(&instance.id).await;
                        return Ok(Followed::Stop);
                    }
                    Ok(child) => {
                        run_decision::record_child(instance, &child);
                        clear_child_started(instance);
                        self.save_instance(instance)?;
                        None
                    }
                    Err(e) => Some(format!(
                        "Decided {}, but workflow '{}' could not start: {e}",
                        choice.label, start.workflow
                    )),
                }
            }
            _ => None,
        };
        let followed = match failed {
            Some(reason) => Err(reason),
            None => run_decision::follow(instance, def, Utc::now()),
        };
        match followed {
            Err(reason) => {
                let step_name = def
                    .get_step(&decision.step_id)
                    .map_or(decision.step_id.as_str(), |s| s.name.as_str());
                let fallback = instance.agent_id.clone();
                self.halt(
                    instance,
                    &reason,
                    &Self::stopped_card(step_name, &reason),
                    channel_id.as_deref(),
                    thread_id.as_deref(),
                    Some(&agent),
                    fallback.as_deref(),
                )
                .await?;
                Ok(Followed::Stop)
            }
            Ok(run_decision::Followed::Step(step)) => {
                self.save_instance(instance)?;
                info!(instance_id = %instance.id, next_step = %step, "Following the decision to its step");
                Ok(Followed::Continue)
            }
            Ok(run_decision::Followed::Completed) => {
                self.save_instance(instance)?;
                if announce {
                    self.send_status(
                        "Workflow complete",
                        channel_id.as_deref(),
                        thread_id.as_deref(),
                        None,
                        instance.agent_id.as_deref(),
                    )
                    .await;
                }
                info!(instance_id = %instance.id, "Workflow completed by its decision");
                Ok(Followed::Stop)
            }
            Ok(run_decision::Followed::Cancelled(reason)) => {
                self.save_instance(instance)?;
                self.wind_down_cancelled(
                    &instance.id,
                    channel_id.as_deref(),
                    thread_id.as_deref(),
                    &reason,
                )
                .await?;
                info!(instance_id = %instance.id, %reason, "Workflow cancelled by its decision");
                Ok(Followed::Stop)
            }
        }
    }

    /// The open decision of `row` as a viewer sees it, and whether they may
    /// make it; `None` while the run waits on none.
    pub(crate) fn decision_view(
        &self,
        row: &WorkflowInstanceRow,
        viewer: Option<&str>,
    ) -> Option<serde_json::Value> {
        let instance = instance_of(row.clone()).ok()?;
        let decision = run_decision::open_in(&instance)?;
        let starter = starter_of(&instance);
        let may_decide = decision
            .refusal(self.person(viewer).as_ref(), starter.as_deref())
            .is_none();
        Some(serde_json::json!({
            "gate": decision.gate,
            "step_id": decision.step_id,
            "title": decision.title,
            "context": decision.context,
            "choices": decision.choices.iter().map(|c| serde_json::json!({
                "label": c.label,
                "outcome": c.outcome,
                "style": c.style.map(|s| s.as_str()),
                "confirm": c.confirm,
            })).collect::<Vec<_>>(),
            "waiting_on": decision
                .waiting_on(starter.as_deref())
                .iter()
                .map(|d| d.display.as_str())
                .collect::<Vec<_>>(),
            "expires_at": decision.expires_at,
            "on_expiry": decision
                .on_expiry
                .as_deref()
                .and_then(|outcome| decision.choice(outcome))
                .map(|c| c.label.as_str()),
            "may_decide": may_decide,
        }))
    }

    /// The press a reply typed in a run's thread makes: the run waiting on a
    /// decision there, its gate, and the outcome the text names.
    pub(crate) fn decision_reply(
        &self,
        channel: &str,
        thread_ts: &str,
        text: &str,
    ) -> Option<(String, Uuid, String)> {
        let row = self
            .find_instance_by_thread(channel, thread_ts, &["paused"])
            .ok()
            .flatten()?;
        let instance = instance_of(row).ok()?;
        let decision = run_decision::open_in(&instance)?;
        let outcome = decision.choice(text)?.outcome.clone();
        Some((instance.id, decision.gate, outcome))
    }

    /// What a thread is told about the decision its run waits on.
    pub(crate) fn decision_reminder(&self, row: &WorkflowInstanceRow) -> Option<String> {
        let instance = instance_of(row.clone()).ok()?;
        let decision = run_decision::open_in(&instance)?;
        let choices = decision
            .choices
            .iter()
            .map(|c| format!("*{}*", c.label))
            .collect::<Vec<_>>()
            .join(" · ");
        let starter = starter_of(&instance);
        Some(format!(
            "This run is waiting on a decision: {choices}, from {}. Reply with a choice or use \
             the buttons.",
            deciders_of(&decision, starter.as_deref())
        ))
    }

    /// Put a clicked card right after a press that settled nothing: rebuilt
    /// from the decision while the run holds it, otherwise the card Slack
    /// echoed without its buttons.
    pub(crate) async fn repair_card(
        &self,
        instance_id: &str,
        gate: Uuid,
        channel: &str,
        ts: &str,
        echoed: Option<serde_json::Value>,
        response_url: &str,
    ) {
        let instance = self.load_instance(instance_id).ok();
        let decision = instance
            .as_ref()
            .and_then(|i| RunDecision::read(&i.variables))
            .filter(|d| d.gate == gate);
        let (text, blocks, agent) = match (&decision, &instance) {
            (Some(decision), Some(instance)) => {
                let starter = starter_of(instance);
                let (text, blocks) = slack_blocks::decision_card(
                    &card_view(decision, starter.as_deref()),
                    instance_id,
                    gate,
                );
                (text, blocks, decision.agent.clone())
            }
            _ => {
                let mut blocks: Vec<serde_json::Value> = echoed
                    .and_then(|v| v.as_array().cloned())
                    .unwrap_or_default()
                    .into_iter()
                    .filter(|block| block["type"] != "actions")
                    .collect();
                blocks.push(serde_json::json!({
                    "type": "context",
                    "elements": [{ "type": "mrkdwn", "text": "This decision is closed." }],
                }));
                ("This decision is closed.".to_string(), blocks, None)
            }
        };
        let surface = self
            .get_surface_for_agent(
                agent.as_deref(),
                instance.as_ref().and_then(|i| i.agent_id.as_deref()),
            )
            .await;
        let message = OutboundMessage {
            channel: channel.to_string(),
            text: text.clone(),
            thread_id: None,
            username: None,
            icon_url: None,
            blocks: Some(blocks.clone()),
        };
        let updated = match &surface {
            Some(surface) => surface.update_message(channel, ts, &message).await.is_ok(),
            None => false,
        };
        if !updated && !response_url.is_empty() {
            if let Err(e) = reqwest::Client::new()
                .post(response_url)
                .json(&serde_json::json!({
                    "text": text,
                    "blocks": blocks,
                    "replace_original": true,
                }))
                .send()
                .await
            {
                warn!(instance_id, error = %e, "Could not repair a decision card");
            }
        }
    }

    /// Resume the run `id` from any surface: a press when it waits on a
    /// decision (or the ask names one), otherwise a steer-checked resume,
    /// with any feedback written first.
    pub(crate) async fn resume_as(
        self: &Arc<Self>,
        id: &str,
        ask: ResumeAsk,
        steerer: &Steerer,
    ) -> Result<Resumed, ResumeRefused> {
        let open = self
            .load_instance(id)
            .ok()
            .and_then(|instance| run_decision::open_in(&instance));
        if open.is_some() || ask.gate.is_some() {
            let choice = match (ask.outcome, &open) {
                (Some(outcome), _) => outcome,
                (None, Some(decision)) => {
                    return Err(ResumeRefused::Decision(DecisionRefused::Choose {
                        choices: choices_of(decision),
                    }))
                }
                (None, None) => String::new(),
            };
            return self
                .decide(
                    id,
                    DecisionAsk {
                        gate: ask.gate,
                        choice,
                        feedback: ask.feedback,
                    },
                    steerer,
                )
                .await
                .map(Resumed::Decided)
                .map_err(ResumeRefused::Decision);
        }
        let action = match ask.outcome.as_deref() {
            Some(REJECTED) => SteerAction::Reject,
            _ => SteerAction::Resume,
        };
        let by = self
            .authorize_steer(id, steerer, action)
            .await
            .map_err(ResumeRefused::Steer)?;
        if let Some(feedback) = &ask.feedback {
            self.set_instance_variable(id, "_human_feedback", feedback)
                .map_err(|e| ResumeRefused::Failed(format!("{e:#}")))?;
        }
        self.resume_workflow(
            id,
            ask.outcome,
            ask.channel_id,
            ask.thread_id,
            ask.originating_agent_id,
            &by,
        )
        .await
        .map(|applied| Resumed::Moved { applied })
        .map_err(|e| ResumeRefused::Failed(format!("{e:#}")))
    }

    /// Whether `reader` is a decider of the decision `row` waits on.
    pub(super) fn decides(row: &WorkflowInstanceRow, reader: Option<&Person>) -> bool {
        reader.is_some_and(|person| {
            open_decision_of(row)
                .is_some_and(|decision| decision.deciders.iter().any(|d| d.is(person)))
        })
    }
}

/// The decision the stored run `row` waits on, if any.
fn open_decision_of(row: &WorkflowInstanceRow) -> Option<RunDecision> {
    (row.status == "paused")
        .then(|| instance_of(row.clone()).ok())
        .flatten()
        .and_then(|instance| run_decision::open_in(&instance))
}

/// Whether the stored run `row` waits on a decision, which only its
/// deciders' presses move on.
pub(crate) fn awaits_decision(row: &WorkflowInstanceRow) -> bool {
    open_decision_of(row).is_some()
}

/// Whether this call is made by an agent working a workflow step, whose
/// session key is `workflow:<instance>:<step>`.
fn in_a_workflow_step() -> bool {
    workforce_agent::runner::CURRENT_SESSION_KEY
        .try_with(|key| key.starts_with("workflow:"))
        .unwrap_or(false)
}

/// The steer refusal a run at a decision gives an action other than a
/// press, if any: everything but a stop is refused, and a stop is the
/// starter's or an administrator's alone.
pub(super) fn decision_rule(
    instance: &WorkflowInstance,
    action: SteerAction,
    standing: Standing,
) -> Result<Standing, SteerRefused> {
    let Some(decision) = run_decision::open_in(instance) else {
        return Ok(standing);
    };
    match (action, standing) {
        (
            SteerAction::Resume
            | SteerAction::Reject
            | SteerAction::Goto
            | SteerAction::Skip
            | SteerAction::Retry
            | SteerAction::Pause,
            _,
        ) => Err(awaiting(&decision, starter_of(instance).as_deref())),
        (SteerAction::Abort, Standing::Other) => Err(SteerRefused::StopAtDecision),
        (SteerAction::Abort | SteerAction::Decide, _) => Ok(standing),
    }
}

/// Why a hand move of a run that is deciding is refused, if it is: an open
/// decision is for its deciders, and a settled one is being carried out.
pub(super) fn deciding(instance: &WorkflowInstance) -> eyre::Result<()> {
    if let Some(decision) = run_decision::open_in(instance) {
        return Err(eyre!(
            "{}",
            awaiting(&decision, starter_of(instance).as_deref())
        ));
    }
    if run_decision::awaiting_follow(instance).is_some() {
        return Err(eyre!("the run is carrying out a decision"));
    }
    Ok(())
}
