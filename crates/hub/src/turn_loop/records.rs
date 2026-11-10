//! What a turn records: its session turn record, usage, telemetry and
//! the prefixes it observes.

use std::sync::Arc;
use workforce_agent::llm::prefix::{ReplayPrefix, SystemPrefix};
use workforce_agent::llm::types::LlmMessage;
use workforce_agent::runner::RunResult;
use workforce_common::cost::CostTracker;
use workforce_common::persistence::{
    RoundTelemetryRecord, TaskTelemetryRecord, ToolTelemetryRecord,
};
use workforce_common::session::{estimate_tokens, Session, TurnPrincipal};
use workforce_common::settings::LlmConfig;
use workforce_common::subscription::is_automated_source;
use workforce_common::telemetry::TelemetryStore;

/// Open the session's turn record, naming the user message the turn answers
/// and the principal it runs as, and return the token that ends it. Failure
/// is logged, not propagated — see [`end_turn_record`] — and yields an empty
/// token, which matches no record, so a turn that could not open one can
/// never clear another's.
pub(crate) fn begin_turn_record(
    sessions: &Arc<dyn workforce_common::session::SessionStore>,
    session_id: &str,
    message_id: Option<&str>,
    principal: &TurnPrincipal,
) -> String {
    match sessions.begin_turn(session_id, message_id, principal) {
        Ok(token) => token,
        Err(e) => {
            tracing::warn!(error = %e, session_id, "Failed to open session turn record");
            String::new()
        }
    }
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn record_task_telemetry_from_result(
    store: &dyn TelemetryStore,
    cost_tracker: &CostTracker,
    agent_id: &str,
    provider: &str,
    result: &RunResult,
    started_at: chrono::DateTime<chrono::Utc>,
    task_type: Option<&str>,
    ticket_id: Option<&str>,
    description: Option<&str>,
    task_id: Option<&str>,
) {
    let completed_at = chrono::Utc::now();
    let cost_usd = cost_tracker.calculate_cost(
        &result.model,
        result.total_usage.input_tokens as u64,
        result.total_usage.output_tokens as u64,
        result.total_usage.cache_creation_input_tokens as u64,
        result.total_usage.cache_read_input_tokens as u64,
    );

    let round_records: Vec<RoundTelemetryRecord> = result
        .round_details
        .iter()
        .map(|rd| RoundTelemetryRecord {
            round_number: rd.round_number,
            input_tokens: rd.usage.input_tokens as u64,
            output_tokens: rd.usage.output_tokens as u64,
            latency_ms: rd.latency_ms,
            cache_creation_tokens: rd.usage.cache_creation_input_tokens as u64,
            cache_read_tokens: rd.usage.cache_read_input_tokens as u64,
            prompt_size_bytes: rd.prompt_size_bytes as u64,
            response_size_bytes: rd.response_size_bytes as u64,
            tool_count: rd.tool_count,
            stop_reason: rd.stop_reason.clone(),
        })
        .collect();

    let tool_records: Vec<ToolTelemetryRecord> = result
        .tool_invocations
        .iter()
        .map(|inv| ToolTelemetryRecord {
            round_number: 0,
            tool_name: inv.tool_name.clone(),
            success: inv.success,
            duration_ms: inv.duration_ms,
            policy_decision: inv.policy_decision.clone(),
            input_size_bytes: serde_json::to_string(&inv.input)
                .map(|s| s.len() as u64)
                .unwrap_or(0),
            output_size_bytes: serde_json::to_string(&inv.output)
                .map(|s| s.len() as u64)
                .unwrap_or(0),
            denial_reason: inv.denial_reason.clone(),
        })
        .collect();

    if let Err(e) = store.record_task_telemetry(
        &TaskTelemetryRecord {
            agent_id: agent_id.to_string(),
            model: result.model.clone(),
            provider: provider.to_string(),
            input_tokens: result.total_usage.input_tokens as u64,
            output_tokens: result.total_usage.output_tokens as u64,
            cache_creation_input_tokens: result.total_usage.cache_creation_input_tokens as u64,
            cache_read_input_tokens: result.total_usage.cache_read_input_tokens as u64,
            cost_usd,
            rounds: result.rounds,
            duration_ms: result.duration_ms,
            tool_invocation_count: result.tool_invocations.len() as u32,
            success: true,
            error_type: None,
            retry_count: 0,
            task_type: task_type.map(String::from),
            task_id: task_id.map(String::from),
            started_at: started_at.to_rfc3339(),
            completed_at: completed_at.to_rfc3339(),
            // No forecast available for direct serve calls (DW-249)
            forecast_cost_usd: None,
            forecast_rounds: None,
            forecast_confidence: None,
            team_id: None,
            session_id: None,
            model_switches: if result.model_switches.is_empty() {
                None
            } else {
                serde_json::to_string(&result.model_switches).ok()
            },
            ticket_id: ticket_id.map(String::from),
            identity_id: crate::user_config::current_billing_identity_id(),
            description: description.map(String::from),
        },
        &round_records,
        &tool_records,
    ) {
        tracing::warn!("Failed to record telemetry: {}", e);
    }
}

/// Pick the runner's `max_rounds` for one call.
///
/// Priority: explicit per-call override > runtime active override > config default.
/// Scheduled jobs pass `override_` to cap background work that would otherwise
/// inherit the agent's general (large) round budget.
pub(crate) fn resolve_max_rounds(override_: Option<u32>, active: Option<u32>, default: u32) -> u32 {
    override_.or(active).unwrap_or(default)
}

/// The origin-selected round-budget default fed to [`resolve_max_rounds`]:
/// automated turns ([`is_automated_source`]) keep the full `max_rounds`
/// budget, while a human waiting in a conversation gets the tighter
/// `interactive_max_rounds` (the wind-down invites them to say "continue").
/// Explicit per-call and vault overrides still win.
pub(crate) fn origin_default_max_rounds(user: &str, llm: &LlmConfig) -> u32 {
    if is_automated_source(user) {
        llm.max_rounds
    } else {
        llm.interactive_max_rounds
    }
}

/// Apply a builder's `with_cancel` when the caller has a token, leaving the
/// value untouched when it does not. Keeps the three review construction sites
/// free of the same `match` written three times.
pub(crate) fn with_cancel_opt<T>(
    value: T,
    cancel: Option<&tokio_util::sync::CancellationToken>,
    with: impl FnOnce(T, tokio_util::sync::CancellationToken) -> T,
) -> T {
    match cancel {
        Some(token) => with(value, token.clone()),
        None => value,
    }
}

/// Record this turn's replayed history and say when it did not extend the
/// previous turn's: that is the one moment the message cache re-bills.
/// Returns whether the previous prefix was extended (`true` with none stored).
pub(crate) fn observe_replay_prefix(session: &mut Session, prior: &[LlmMessage]) -> bool {
    let extended = session
        .replay_prefix()
        .and_then(|v| serde_json::from_value::<ReplayPrefix>(v.clone()).ok())
        .is_none_or(|previous| previous.is_extended_by(prior));
    if !extended {
        tracing::info!(
            session_id = %session.id,
            messages = prior.len(),
            "Replayed history is not an extension of the previous turn's"
        );
    }
    if let Ok(value) = serde_json::to_value(ReplayPrefix::of(prior)) {
        session.set_replay_prefix(value);
    }
    extended
}

/// Record this turn's stable system prompt and say when it is not the
/// previous turn's: with the per-turn tail in the task message, that is the
/// only way the system prefix can move between turns. Returns whether it was
/// unchanged (`true` with none stored).
pub(crate) fn observe_system_prefix(session: &mut Session, stable: &str) -> bool {
    let current = SystemPrefix::of(stable);
    let changed = session
        .system_prefix()
        .and_then(|v| serde_json::from_value::<SystemPrefix>(v.clone()).ok())
        .filter(|previous| *previous != current);
    if let Some(previous) = changed {
        tracing::info!(
            session_id = %session.id,
            len_before = previous.len,
            len_after = current.len,
            "Stable system prompt changed since the previous turn"
        );
    }
    if let Ok(value) = serde_json::to_value(current) {
        session.set_system_prefix(value);
    }
    changed.is_none()
}

/// Store the assistant's response with secrets masked at rest. Unlike
/// ingress (which tokenizes), assistant output is masked — a model echo
/// of a credential must not become a cache-backed marker the agent could
/// later resolve back to cleartext at a sink.
pub(crate) fn add_redacted_assistant_message(
    session: &mut Session,
    response: &str,
    desk: Option<&workforce_common::channels::desk::DeskTurnContext>,
) {
    let masked = workforce_common::redact::redact_secrets(response);
    let tokens = estimate_tokens(&masked);
    let (id, mark) = reply_slot(desk, &session.channel);
    session.add_assistant_message_with_id(
        id.unwrap_or_else(|| uuid::Uuid::new_v4().to_string()),
        &masked,
        tokens,
    );
    if let Some(reply) = session.messages.last_mut() {
        reply.desk = mark;
    }
}

/// What a live Desk turn's `invocations` leave on the answer it just saved
/// in `session`: the files they delivered, so a Desk that missed them live
/// reads them with it, and the tools they ran, as steps.
pub(crate) fn carry_turn(
    session: &mut Session,
    invocations: &[workforce_agent::runner::ToolInvocation],
) {
    let files = crate::desk_attachments::delivered_in(session, invocations);
    let steps = steps_of(invocations);
    if files.is_empty() && steps.is_empty() {
        return;
    }
    let Some(reply) = session.messages.last_mut() else {
        return;
    };
    if !files.is_empty() {
        reply.attachments = Some(files.iter().map(Into::into).collect());
    }
    let mark = reply.desk.get_or_insert_with(Default::default);
    mark.files = files;
    mark.steps = steps;
}

/// The steps a turn's tool runs make, at most
/// [`workforce_common::channels::desk::MAX_STEPS`] of them, the first kept;
/// an advisor call's with what it was asked and said.
pub(crate) fn steps_of(
    invocations: &[workforce_agent::runner::ToolInvocation],
) -> Vec<workforce_common::channels::desk::DeskStep> {
    use workforce_common::channels::desk::{DeskStep, ADVICE_MESSAGE_CHARS, MAX_STEPS};
    let mut room = ADVICE_MESSAGE_CHARS;
    invocations
        .iter()
        .take(MAX_STEPS)
        .map(|i| {
            let step = DeskStep::of(&i.tool_name, i.success, i.duration_ms);
            match advice_of(i, room) {
                Some(advice) => {
                    room = room.saturating_sub(advice.answer.chars().count());
                    step.with_advice(advice)
                }
                None => step,
            }
        })
        .collect()
}

/// What an advisor call asked and was told, secrets hidden and then
/// clipped (so no cut leaves part of one a pattern can't find), its answer
/// to at most `room` characters; `None` for any other tool's call.
fn advice_of(
    i: &workforce_agent::runner::ToolInvocation,
    room: usize,
) -> Option<workforce_common::channels::desk::DeskAdvice> {
    if i.tool_name != crate::advisor::AdvisorTool::NAME {
        return None;
    }
    let text = |v: &serde_json::Value, key: &str| {
        v.get(key)
            .and_then(serde_json::Value::as_str)
            .map(str::to_string)
    };
    let (answer, unavailable) = match (
        text(&i.output, "advice"),
        text(&i.output, "advisor_unavailable"),
    ) {
        (Some(advice), _) => (advice, false),
        (None, Some(why)) => (why, true),
        (None, None) => (i.error.clone()?, true),
    };
    let redact = crate::redact::redact_secrets;
    // Past the message's room, the question alone.
    let (answer, room) = match room {
        0 => (
            "This answer is past what one message keeps; the agent has it whole.".to_string(),
            usize::MAX,
        ),
        _ => (redact(&answer), room),
    };
    Some(workforce_common::channels::desk::DeskAdvice::new(
        &redact(&text(&i.input, "question").unwrap_or_default()),
        &answer,
        text(&i.output, "model").map(|m| redact(&m)).as_deref(),
        unavailable,
        room,
    ))
}

/// The id and Desk mark a turn's answer in a session on `channel` is saved
/// with: for a live Desk turn, the id its client was promised and its
/// place; for a turn re-run in a Desk conversation nobody is attending (an
/// approval's resume, a recovered turn), its place, as resumed.
pub(crate) fn reply_slot(
    desk: Option<&workforce_common::channels::desk::DeskTurnContext>,
    channel: &str,
) -> (
    Option<String>,
    Option<workforce_common::channels::desk::DeskMark>,
) {
    match desk {
        Some(desk) => (
            Some(desk.reply_id.clone()),
            Some(workforce_common::channels::desk::DeskMark {
                in_reply_to: desk.turn.message,
                ..desk.turn.place.mark()
            }),
        ),
        None => (
            None,
            workforce_agent::runner::current_desk()
                .filter(|scope| !scope.attended && scope.target.channel == channel)
                .map(|scope| workforce_common::channels::desk::DeskMark {
                    delivery: Some(workforce_common::channels::desk::DeliveryKind::Resumed),
                    ..scope.target.place.mark()
                }),
        ),
    }
}

/// Save a served turn's answer in `session` and keep what its route leaves
/// behind: a CLI route's transcript slot, else the observed usage that
/// calibrates the session's estimate — a CLI's input tokens measure its own
/// transcript, not this conversation. The provider task telemetry files the
/// turn under, when its route names one.
pub(crate) fn settle_served(
    session: &mut Session,
    result: &RunResult,
    route: crate::agent_turn::RouteTag,
    reserved: workforce_common::session::ReservedContext,
    desk: Option<&workforce_common::channels::desk::DeskTurnContext>,
) -> Option<&'static str> {
    // A spoken reply can be nothing but a direction: said, but with nothing to
    // read, and so nothing to keep. Tools it ran still need their message.
    let nothing_to_read = session.is_voice_turn()
        && result.response.trim().is_empty()
        && result.tool_invocations.is_empty();
    if !nothing_to_read {
        add_redacted_assistant_message(session, &result.response, desk);
    }
    if desk.is_some() {
        carry_turn(session, &result.tool_invocations);
    }
    if !route.is_cli() {
        record_observed_usage(session, result, reserved);
    }
    let label = route.label();
    route.settle(session);
    label
}

/// Record the LAST round's provider-reported prompt size on the session so
/// the next turn's compaction check sees the REAL context, and log when
/// reality runs well ahead of the estimate — the drift is ephemeral per-turn
/// content (code-context injection, thread history) the stored-message
/// accounting cannot see, and it is exactly what makes "why didn't
/// compaction fire" undiagnosable without this line.
///
/// Deliberately NOT `total_usage.input_tokens`: that is the SUM of every
/// round's prompt across a multi-round tool turn, which overstates the
/// context several-fold and would grind healthy sessions through emergency
/// compaction (the compaction-storm failure class).
pub(crate) fn record_observed_usage(
    session: &mut Session,
    result: &RunResult,
    reserved: workforce_common::session::ReservedContext,
) {
    let observed = result
        .round_details
        .last()
        .map(|r| {
            r.usage
                .input_tokens
                .saturating_add(r.usage.cache_read_input_tokens)
                .saturating_add(r.usage.cache_creation_input_tokens)
        })
        .unwrap_or(0);
    if observed == 0 {
        return;
    }
    session.record_estimate_calibration(observed, reserved);
    let estimate = session.estimated_total_with_reserved(reserved);
    if observed > estimate.saturating_add(estimate / 5) {
        tracing::info!(
            session_id = %session.id,
            observed,
            estimate,
            calibration = session.estimate_calibration(),
            "Prompt estimate drift: real input tokens ran ahead of stored-message accounting"
        );
    }
    session.record_observed_input_tokens(observed);
}

/// Adds the person's follow-ups a live turn took in (`heard`) to `session`,
/// as they arrived: each under the client's id for it, stamped when it came
/// rather than when the turn saved, and in the turn's Desk place, said by
/// the person.
pub(crate) fn keep_heard(
    session: &mut Session,
    heard: Vec<crate::hub_core::live_turns::Heard>,
    trust_level: Option<workforce_common::trust::TrustLevel>,
    desk: Option<&workforce_common::channels::desk::DeskTurnContext>,
) {
    for follow_up in heard {
        session.add_user_message_with_trust(
            &follow_up.text,
            follow_up.external_id.as_deref(),
            None,
            trust_level,
        );
        if let Some(said) = session.messages.last_mut() {
            said.timestamp = follow_up.at;
            said.desk = desk.map(|desk| workforce_common::channels::desk::DeskMark {
                speaker: Some(workforce_common::channels::desk::DeskSpeaker::Person),
                ..desk.turn.place.mark()
            });
        }
    }
}

/// The follow-ups `heard` that the run read (`absorbed`), and those it never
/// did, each in the order it arrived. One the client named is known by its
/// id; one it didn't, by who said what.
pub(crate) fn read_by_run(
    heard: Vec<crate::hub_core::live_turns::Heard>,
    absorbed: &[workforce_agent::runner::InjectedUserMessage],
) -> (
    Vec<crate::hub_core::live_turns::Heard>,
    Vec<crate::hub_core::live_turns::Heard>,
) {
    let mut left: Vec<&workforce_agent::runner::InjectedUserMessage> = absorbed.iter().collect();
    heard.into_iter().partition(|h| {
        let found = left.iter().position(|a| match &h.external_id {
            Some(id) => a.external_id.as_deref() == Some(id.as_str()),
            None => a.external_id.is_none() && a.text == h.text && a.user == h.user,
        });
        found.map(|at| left.remove(at)).is_some()
    })
}

/// Once `over` (the turn's task has ended, however), keeps the follow-ups
/// still in `heard`: those of a turn stopped or cut short, which no end path
/// took. They are added to the session's transcript and its archive as they
/// arrived.
pub(crate) async fn keep_heard_left(
    over: tokio_util::sync::CancellationToken,
    heard: crate::hub_core::live_turns::HeardLog,
    sessions: Arc<dyn workforce_common::session::SessionStore>,
    session_id: String,
    trust_level: Option<workforce_common::trust::TrustLevel>,
    desk: Option<workforce_common::channels::desk::DeskTurnContext>,
) {
    over.cancelled().await;
    let left = heard.take();
    if left.is_empty() {
        heard.kept();
        return;
    }
    let kept = tokio::task::spawn_blocking(move || -> eyre::Result<()> {
        let Some(mut session) = sessions.get(&session_id)? else {
            return Ok(());
        };
        let from = session.messages.len();
        keep_heard(&mut session, left, trust_level, desk.as_ref());
        sessions.save_refreshing(&mut session)?;
        for msg in session.messages.get(from..).unwrap_or_default() {
            sessions.append_to_archive(&session.id, msg)?;
        }
        Ok(())
    })
    .await;
    match kept {
        Ok(Ok(())) => {}
        Ok(Err(e)) => tracing::warn!(error = %e, "Failed to keep a stopped turn's follow-ups"),
        Err(e) => tracing::warn!(error = %e, "Keeping a stopped turn's follow-ups panicked"),
    }
    heard.kept();
}

/// What a turn that failed leaves to keep: where its messages start in the
/// session, the follow-ups it took in, and what it failed with after
/// running which tools.
pub(crate) struct FailedTurn<'a> {
    pub archive_base: usize,
    /// Where the messages not archived yet start: the question went before
    /// the run.
    pub archived_upto: usize,
    pub heard: Vec<crate::hub_core::live_turns::Heard>,
    pub trust_level: Option<workforce_common::trust::TrustLevel>,
    pub desk: Option<&'a workforce_common::channels::desk::DeskTurnContext>,
    /// Who asked, in a shared session.
    pub asker: Option<&'a str>,
    /// What the client was told the turn failed with.
    pub error: &'a str,
    /// The tool rows the turn's journal holds.
    pub ran: &'a [workforce_common::session::SessionMessage],
}

/// Keeps what a failed turn was asked: its prompt is saved and archived
/// already, and the follow-ups it took in go to the transcript and the
/// archive, so the conversation still reads them after a restart. A DM
/// turn's failure is archived too, as the agent's row marked failed,
/// pointing at the message the turn was asked with and carrying the tools it
/// ran. That row is for the person's DM alone: it never enters the
/// transcript the model replays, and a room's turn leaves none, since what a
/// room's conversation keeps is posted to the room as the agent's reply.
pub(crate) fn keep_failed_turn(
    sessions: &Arc<dyn workforce_common::session::SessionStore>,
    session: &mut Session,
    turn: FailedTurn<'_>,
) {
    if !turn.heard.is_empty() {
        keep_heard(session, turn.heard, turn.trust_level, turn.desk);
        if let Some(asker) = turn.asker {
            session.attribute_messages_from(turn.archive_base, asker);
        }
        if let Err(e) = sessions.save_refreshing(session) {
            tracing::warn!(error = %e, "Failed to save a failed turn's follow-ups");
        }
    }
    for msg in session
        .messages
        .get(turn.archived_upto..)
        .unwrap_or_default()
        .iter()
        .filter(|m| m.role == workforce_common::session::MessageRole::User)
    {
        if let Err(e) = sessions.append_to_archive(&session.id, msg) {
            tracing::warn!(error = %e, "Failed to archive a failed turn's message");
        }
    }
    let Some(desk) = turn.desk.filter(|desk| {
        matches!(
            desk.turn.place,
            workforce_common::channels::desk::DeskPlace::Dm
        )
    }) else {
        return;
    };
    let failure = failure_row(desk, turn.error, turn.ran);
    if let Err(e) = sessions.append_to_archive(&session.id, &failure) {
        tracing::warn!(error = %e, "Failed to archive a turn's failure");
    }
}

/// The archive row a Desk turn's failure is: the agent's, saying what the
/// client was told, marked failed and pointing at the message the turn was
/// asked with, with the tools the turn ran as its steps.
pub(crate) fn failure_row(
    desk: &workforce_common::channels::desk::DeskTurnContext,
    error: &str,
    ran: &[workforce_common::session::SessionMessage],
) -> workforce_common::session::SessionMessage {
    let steps = ran
        .iter()
        .filter_map(|row| {
            workforce_common::session::message_to_transcript_entry("", row).tool_metadata
        })
        .take(workforce_common::channels::desk::MAX_STEPS)
        .map(|tool| {
            workforce_common::channels::desk::DeskStep::of(
                &tool.name,
                tool.success,
                tool.duration_ms,
            )
        })
        .collect();
    workforce_common::session::SessionMessage {
        id: uuid::Uuid::new_v4().to_string(),
        role: workforce_common::session::MessageRole::Assistant,
        content: workforce_common::redact::redact_secrets(error),
        tool_data: None,
        timestamp: chrono::Utc::now(),
        tokens: 0,
        external_id: None,
        attachments: None,
        trust_level: None,
        actor: None,
        desk: Some(workforce_common::channels::desk::DeskMark {
            in_reply_to: desk.turn.message,
            steps,
            failed: true,
            ..desk.turn.place.mark()
        }),
    }
}
