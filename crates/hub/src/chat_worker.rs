//! NATS chat workers — bridge between NATS and `process_message_with_progress`.
//!
//! For each registered agent, a worker subscribes to
//! `workforce.agents.{agent_id}.chat.>` and forwards requests to
//! the corresponding [`AgentService`]. Progress events are published back
//! to `workforce.agents.{agent_id}.progress.{session_id}`.

use std::sync::Arc;

use eyre::Result;
use futures::StreamExt;
use tracing::{info, warn};
use workforce_agent::runner::ProgressEvent;
use workforce_agent::tools::approval::SelfServe;
use workforce_core::types::AgentId;
use workforce_nats::chat::{ChatProgress, NatsChatRequest};
use workforce_nats::envelope::Envelope;

use crate::agent_registry::AgentRegistry;
use crate::live_turns::LiveTurn;

/// Maximum number of bytes carried forward from one stream delta into the
/// next so the credential scanner can detect tokens whose representation
/// straddles a chunk boundary. Sized to comfortably exceed the longest
/// non-PEM secret pattern (Slack/GitHub/Anthropic/etc. token bodies are
/// 40-60 chars; 80 leaves headroom).
const STREAM_DELTA_OVERLAP_BYTES: usize = 80;

/// Sanitize agent-controlled UI text for outbound display:
///   1. render any `[[secret:…]]` markers as their `preview` form so
///      a leaked marker reads as `sk-an...wXyZ` rather than the noisy
///      bracket form;
///   2. defensively redact any cleartext credential pattern that
///      somehow reached the response (e.g. a tool-result that bypassed
///      tokenization) using `first6...last4`.
///
/// Used for `ToolStart` detail strings and complete (non-streamed)
/// `Response` messages where there is no carry-over context.
fn scrub_text(text: &str) -> String {
    let rendered = workforce_common::secret_marker::render_markers_as_preview(text);
    if workforce_common::security_scanner::contains_credentials(&rendered) {
        tracing::warn!(target: "sentinel", "Credential detected in agent output — redacting");
        workforce_common::security_scanner::redact_credentials(&rendered)
    } else {
        rendered
    }
}

/// Redact credentials in a streamed response delta, carrying the tail of
/// the previous delta forward so credentials split across chunk boundaries
/// are still caught. The mutable `delta_tail` is updated in place to hold
/// the last [`STREAM_DELTA_OVERLAP_BYTES`] characters of the *original*
/// delta — using the original (not the redacted) text keeps the boundary
/// stable for the next iteration.
///
/// Known limitation: when a credential is split across two deltas, the
/// prefix in the first delta has already been published unredacted by the
/// time we see the second delta. The overlap buffer only catches
/// credentials whose prefix on its own does not match the regex (e.g. the
/// `xoxb-` pattern requires 20+ chars after the dash, so a stray `xoxb-`
/// in chunk N is harmless until chunk N+1 supplies the body).
fn redact_stream_delta(delta_tail: &mut String, text: &str) -> String {
    // Render markers first so a leaked `[[secret:…]]` chunk surfaces
    // as the partial preview. Marker grammar uses `[[`/`]]` delimiters
    // that no credential pattern matches, so order is independent for
    // correctness — markers first keeps the tail buffer compact.
    let scan_window = format!("{delta_tail}{text}");
    let scan_window = workforce_common::secret_marker::render_markers_as_preview(&scan_window);
    let rendered_text = workforce_common::secret_marker::render_markers_as_preview(text);
    let safe_text = if workforce_common::security_scanner::contains_credentials(&scan_window) {
        tracing::warn!(target: "sentinel", "Credential detected in stream delta — redacting");
        let redacted = workforce_common::security_scanner::redact_credentials(&scan_window);
        let rendered_tail = workforce_common::secret_marker::render_markers_as_preview(delta_tail);
        redacted
            .strip_prefix(rendered_tail.as_str())
            .map(str::to_string)
            .unwrap_or(redacted)
    } else {
        rendered_text
    };

    // Carry the tail of the *original* delta forward; clamp to a char
    // boundary so we never slice through a UTF-8 sequence.
    let mut tail_start = text.len().saturating_sub(STREAM_DELTA_OVERLAP_BYTES);
    while tail_start > 0 && !text.is_char_boundary(tail_start) {
        tail_start -= 1;
    }
    *delta_tail = text[tail_start..].to_string();

    safe_text
}

/// Spawn one NATS chat worker per registered agent.
///
/// Returns immediately; workers run as background tokio tasks.
pub fn spawn_chat_workers(nats: async_nats::Client, registry: Arc<AgentRegistry>) {
    for agent_id in registry.agent_ids() {
        spawn_chat_worker(nats.clone(), Arc::clone(&registry), agent_id);
    }
}

/// Spawn the chat worker for a single agent — used at startup (above) and
/// when an agent is live-spawned from a config overlay change.
pub fn spawn_chat_worker(
    nats: async_nats::Client,
    registry: Arc<AgentRegistry>,
    agent_id: AgentId,
) {
    tokio::spawn(async move {
        if let Err(e) = run_agent_chat_worker(nats, registry, agent_id.clone()).await {
            tracing::error!(agent = %agent_id, "Chat worker exited with error: {}", e);
        }
    });
}

/// Run the chat worker loop for a single agent.
async fn run_agent_chat_worker(
    nats: async_nats::Client,
    registry: Arc<AgentRegistry>,
    agent_id: AgentId,
) -> Result<()> {
    let subject = workforce_nats::subjects::agent_chat_all(&agent_id.to_string());
    let mut sub = nats
        .queue_subscribe(subject.clone(), "workforce-hub".into())
        .await?;
    info!(agent = %agent_id, subject = %subject, "Chat worker listening");

    while let Some(msg) = sub.next().await {
        let envelope: Envelope<NatsChatRequest> = match Envelope::from_bytes(&msg.payload) {
            Ok(env) => env,
            Err(e) => {
                warn!(agent = %agent_id, "Invalid chat request: {}", e);
                continue;
            }
        };

        let request = envelope.payload;
        let service = match registry.get(&agent_id.to_string()) {
            Some(s) => s,
            None => {
                warn!(agent = %agent_id, "No service found for agent");
                continue;
            }
        };

        let nats = nats.clone();
        let agent_str = agent_id.to_string();

        // Process each chat request concurrently
        tokio::spawn(async move {
            process_chat_request(nats, &agent_str, service, request).await;
        });
    }

    Ok(())
}

/// Process a single chat request: run the turn and publish its progress to
/// NATS, always ending with `Done` so the gateway's forwarder stops. The
/// closing events go out before the live turn is released, so a stop's
/// acknowledgement never overtakes them.
async fn process_chat_request(
    nats: async_nats::Client,
    agent_id: &str,
    service: Arc<crate::serve::AgentService>,
    request: NatsChatRequest,
) {
    let progress_subject = workforce_nats::subjects::agent_progress(agent_id, &request.session_id);
    let desk = request.desk.clone();
    let (session_id, tail, live) = run_chat_turn(&nats, &progress_subject, &service, request).await;
    let end = desk.as_ref().map(|desk| {
        ChatProgress::TurnEnd(crate::desk_surface::turn_end(&service, &session_id, desk))
    });
    for progress in closing(tail, end, session_id) {
        publish_progress(&nats, &progress_subject, progress).await;
    }
    drop(live);
}

/// A turn's closing events: its tail, a Desk turn's end, and always `Done`.
fn closing(
    tail: Vec<ChatProgress>,
    end: Option<ChatProgress>,
    session_id: String,
) -> impl Iterator<Item = ChatProgress> {
    tail.into_iter()
        .chain(end)
        .chain(std::iter::once(ChatProgress::Done { session_id }))
}

/// Run one chat turn as a live turn: the session it ran in, the events that
/// close it, and the turn itself, still registered until those are sent.
async fn run_chat_turn(
    nats: &async_nats::Client,
    progress_subject: &str,
    service: &Arc<crate::serve::AgentService>,
    request: NatsChatRequest,
) -> (String, Vec<ChatProgress>, Option<LiveTurn>) {
    let requested = request.session_id.clone();
    let (mut live, inbox) = match service
        .begin_live_turn(
            &requested,
            SelfServe::answerable_if(request.answers_approvals),
            request.desk.as_ref(),
        )
        .await
    {
        Ok(begun) => begun,
        Err(busy) => {
            let refused = ChatProgress::Error {
                message: busy.to_string(),
            };
            return (requested, vec![refused], None);
        }
    };
    publish_progress(nats, progress_subject, ChatProgress::Typing).await;

    let session_attachments = (!request.attachments.is_empty()).then(|| {
        request
            .attachments
            .into_iter()
            .map(|a| workforce_common::session::SessionAttachment {
                filename: a.filename,
                mime_type: a.mime_type,
                data: a.data,
                artifact_id: a.artifact_id,
            })
            .collect()
    });
    let started = service
        .process_message_with_progress(
            &request.channel,
            &request.user,
            None,
            &request.content,
            None,
            Some(&requested),
            session_attachments,
            None,
            None,
            None,
            Some(inbox),
            request.display_content.as_deref(),
            None, // no ticket_id for chat workers
            None, // no step_max_rounds for chat workers
            None, // no step_model for chat workers
            None, // no context_eviction_interval
            None, // no context_eviction_keep_last
            None, // no tool_whitelist
            None, // no persona override
            None, // no workspace_id — chat workers operate outside Slack context
            // No trust classification — chat workers run system-initiated
            // work — save that another agent's hand-off is never the person's.
            crate::desk_surface::hand_off_trust(request.desk.as_ref().map(|desk| &desk.turn), None),
            Some(live.cancel_token()),
            request.voice_channel.as_deref(),
            request.desk.as_ref(),
        )
        .await;
    let progress = match started {
        Ok(progress) => progress,
        Err(_) if live.is_cancelled() => return (requested, Vec::new(), Some(live)),
        Err(e) => {
            let failed = ChatProgress::Error {
                message: format!("Failed to process message: {e}"),
            };
            return (requested, vec![failed], Some(live));
        }
    };
    let (nats, subject) = (nats.clone(), progress_subject.to_string());
    let desk = request.desk.is_some();
    let conducted = crate::live_turns::conduct(&mut live, service, progress, move |rx| {
        forward_progress(rx, nats, subject, desk)
    })
    .await;
    let session_id = conducted.session_id.clone();
    (session_id, conducted.closing(service), Some(live))
}

/// Publish a turn's progress; a Desk turn's (`desk`) also carries what only
/// a Desk gateway knows how to show, so an older gateway never meets it.
async fn forward_progress(
    mut progress_rx: tokio::sync::mpsc::Receiver<ProgressEvent>,
    nats: async_nats::Client,
    subject: String,
    desk: bool,
) {
    let mut delivery = AnswerDelivery::default();
    let mut delta_tail = String::new();
    while let Some(event) = progress_rx.recv().await {
        delivery.note(&event);
        if let Some(progress) = chat_progress(event, &delivery, &mut delta_tail, desk) {
            publish_progress(&nats, &subject, progress).await;
        }
    }
}

fn chat_progress(
    event: ProgressEvent,
    delivery: &AnswerDelivery,
    delta_tail: &mut String,
    desk: bool,
) -> Option<ChatProgress> {
    match event {
        ProgressEvent::ToolStart { name, detail, .. } => Some(ChatProgress::ToolStart {
            name,
            detail: scrub_text(&detail),
        }),
        ProgressEvent::ToolEnd {
            name,
            success,
            duration_ms,
            ..
        } => Some(ChatProgress::ToolEnd {
            name,
            success,
            duration_ms,
            detail: String::new(),
        }),
        ProgressEvent::LlmCall { .. } => Some(ChatProgress::Typing),
        ProgressEvent::StreamStart => {
            delta_tail.clear();
            Some(ChatProgress::StreamStart)
        }
        ProgressEvent::StreamDelta { text } => Some(ChatProgress::StreamDelta {
            text: redact_stream_delta(delta_tail, &text),
        }),
        ProgressEvent::StreamEnd => {
            delta_tail.clear();
            Some(ChatProgress::StreamEnd)
        }
        // This stream is not reconciled against a final answer, so a settled
        // answer has nothing to retire here.
        ProgressEvent::AnswerSettled => None,
        ProgressEvent::StreamDiscard { reason } => reason.retracts().then(|| {
            delta_tail.clear();
            ChatProgress::StreamDiscard { reason }
        }),
        ProgressEvent::Response { ref text } => {
            delivery.delivers_response().then(|| ChatProgress::Message {
                text: scrub_text(text),
            })
        }
        ProgressEvent::CompactStart => Some(ChatProgress::CompactStart),
        ProgressEvent::CompactEnd => Some(ChatProgress::CompactEnd),
        ProgressEvent::ContextUpdate {
            estimated_tokens,
            context_max,
        } => Some(ChatProgress::ContextUpdate {
            estimated_tokens,
            context_max,
        }),
        ProgressEvent::ApprovalRequired {
            request_id,
            agent_id,
            tool_name,
            detail,
            reason,
            options,
            ..
        } => Some(ChatProgress::ApprovalRequired {
            request_id,
            agent_id,
            tool_name,
            detail,
            reason,
            options,
        }),
        ProgressEvent::SubActivity { source, detail } => {
            // Nested activity from a long-running tool (advisor,
            // coding.session); reuse the ToolStart surface so
            // chat clients show it like any other activity.
            Some(ChatProgress::ToolStart {
                name: source,
                detail: scrub_text(&detail),
            })
        }
        ProgressEvent::Done { .. } => None,
        ProgressEvent::InjectedMessage { .. } => None,
        ProgressEvent::RateLimited { wait_secs, attempt } => {
            desk.then_some(ChatProgress::RateLimited { wait_secs, attempt })
        }
        ProgressEvent::SlowModelFailover {
            model,
            elapsed_secs,
        } => desk.then_some(ChatProgress::SlowModelFailover {
            model,
            elapsed_secs,
        }),
        ProgressEvent::Snapshot(snapshot) => desk.then_some(ChatProgress::Snapshot(snapshot)),
        ProgressEvent::File(file) => desk.then_some(ChatProgress::File(file)),
        ProgressEvent::SecretConsumed { .. } => None,
        ProgressEvent::ApprovalResolved { .. } => None,
        ProgressEvent::ConnectRequired { prompt } => Some(ChatProgress::ConnectPrompt { prompt }),
        // Periodic progress check-in — surface as a durable message.
        ProgressEvent::CheckIn { note } => Some(ChatProgress::Message {
            text: scrub_text(&note),
        }),
    }
}

/// Publish a ChatProgress event to a NATS subject.
/// Whether a turn's `Response` still needs delivering: not when its answer
/// streamed, which the `Response` restates, but again once what streamed was
/// taken back off screen, or for a notice a turn ends on after asking to
/// connect a login, which no stream carried.
#[derive(Debug, Default)]
pub(crate) struct AnswerDelivery {
    streamed: bool,
}

impl AnswerDelivery {
    pub(crate) fn note(&mut self, event: &ProgressEvent) {
        match event {
            ProgressEvent::StreamDelta { .. } => self.streamed = true,
            ProgressEvent::ConnectRequired { .. } => self.streamed = false,
            ProgressEvent::StreamDiscard { reason } if reason.retracts() => self.streamed = false,
            _ => {}
        }
    }

    pub(crate) fn delivers_response(&self) -> bool {
        !self.streamed
    }
}

/// What follows a turn's answer, in order: its deterministic code citations
/// (once, when it grounded any) and the line saying what ran it, when one is
/// shown.
pub(crate) fn after_answer(
    code_citations: Vec<workforce_core::code_citation::CodeCitation>,
    route: Option<String>,
) -> impl Iterator<Item = ChatProgress> {
    (!code_citations.is_empty())
        .then_some(ChatProgress::CodeCitations {
            citations: code_citations,
        })
        .into_iter()
        .chain(route.map(|line| ChatProgress::Route { line }))
}

async fn publish_progress(nats: &async_nats::Client, subject: &str, progress: ChatProgress) {
    match serde_json::to_vec(&progress) {
        Ok(bytes) => {
            if let Err(e) = nats.publish(subject.to_string(), bytes.into()).await {
                warn!(subject, "Failed to publish progress: {}", e);
            }
        }
        Err(e) => {
            warn!("Failed to serialize progress event: {}", e);
        }
    }
}

#[cfg(test)]
mod redaction_tests {
    use super::*;

    // A complete Slack bot token: prefix + 50 chars of body, well over the
    // 20-char minimum the regex requires.
    const FULL_TOKEN: &str = concat!(
        "xoxb-",
        "000000000000-00000000000000-EXAMPLEexampleEXAMPLEexa"
    );
    // Plain non-secret prose used as a control.
    const PLAIN: &str = "hello, this is just regular text without any secrets in it.";

    #[test]
    fn scrub_text_passes_clean_strings_through() {
        assert_eq!(scrub_text(PLAIN), PLAIN);
        assert_eq!(scrub_text(""), "");
    }

    #[test]
    fn scrub_text_redacts_known_credentials() {
        let scrubbed = scrub_text(FULL_TOKEN);
        assert!(!scrubbed.contains("hsuXPRNx2gSH2zeqI9DufAHv"));
        assert_ne!(scrubbed, FULL_TOKEN);
    }

    #[test]
    fn redact_delta_passes_clean_text_unchanged() {
        let mut tail = String::new();
        let out = redact_stream_delta(&mut tail, PLAIN);
        assert_eq!(out, PLAIN);
        assert!(tail.ends_with("ts in it."));
    }

    #[test]
    fn redact_delta_redacts_full_credential_in_one_chunk() {
        let mut tail = String::new();
        let chunk = format!("here is the token: {FULL_TOKEN} done.");
        let out = redact_stream_delta(&mut tail, &chunk);
        assert!(!out.contains("hsuXPRNx2gSH2zeqI9DufAHv"));
        assert!(out.contains("here is the token"));
        assert!(out.contains("done."));
    }

    #[test]
    fn redact_delta_catches_credential_split_across_two_chunks() {
        // Split at 10 bytes: `xoxb-20004` — only 5 chars after the dash,
        // well below the 20-char minimum the Slack regex requires, so the
        // first chunk on its own does NOT trigger redaction. This is the
        // case the overlap buffer exists for: the first chunk leaks its
        // tiny prefix (acceptable per the documented limitation), but the
        // second chunk MUST redact the rest of the body.
        let split_at = 10;
        let (first, second) = FULL_TOKEN.split_at(split_at);
        let mut tail = String::new();

        let out1 = redact_stream_delta(&mut tail, first);
        assert_eq!(out1, first, "first-chunk prefix is too short to match");
        assert_eq!(tail, first);

        let out2 = redact_stream_delta(&mut tail, second);
        // The body of the token must not appear verbatim in the second
        // delta — the merged window matched and was redacted.
        assert!(!out2.contains("hsuXPRNx2gSH2zeqI9DufAHv"));
        assert_ne!(out2, second);
    }

    #[test]
    fn redact_delta_overlap_buffer_truncates_to_window_size() {
        let mut tail = String::new();
        let long = "a".repeat(STREAM_DELTA_OVERLAP_BYTES * 3);
        let _ = redact_stream_delta(&mut tail, &long);
        assert_eq!(tail.len(), STREAM_DELTA_OVERLAP_BYTES);
    }

    #[test]
    fn redact_delta_overlap_buffer_handles_short_input() {
        let mut tail = "previous-tail".to_string();
        let _ = redact_stream_delta(&mut tail, "x");
        // The new tail is just the new input, which is shorter than the window.
        assert_eq!(tail, "x");
    }

    #[test]
    fn redact_delta_handles_utf8_boundary_when_truncating() {
        let mut tail = String::new();
        // Build an input where the byte index `len - 80` lands inside a
        // multi-byte sequence: 78 ASCII bytes followed by a 4-byte emoji.
        let input = format!("{}{}", "a".repeat(78), "🎯");
        let _ = redact_stream_delta(&mut tail, &input);
        // The tail must be valid UTF-8 — the assertion is implicit (panic
        // would have fired during slicing if not), but we also verify the
        // emoji was preserved whole.
        assert!(tail.ends_with('🎯'));
    }
}

#[cfg(test)]
mod stream_discard_tests {
    use super::*;
    use workforce_agent::runner::StreamDiscardReason;

    #[test]
    fn a_discard_maps_to_chat_progress_and_clears_the_delta_tail() {
        let delivery = AnswerDelivery::default();
        let mut tail = "an unfinished credential".to_string();
        let progress = chat_progress(
            ProgressEvent::StreamDiscard {
                reason: StreamDiscardReason::Interrupted,
            },
            &delivery,
            &mut tail,
            false,
        );
        assert!(matches!(
            progress,
            Some(ChatProgress::StreamDiscard {
                reason: StreamDiscardReason::Interrupted
            })
        ));
        assert!(tail.is_empty());
    }

    #[test]
    fn a_superseded_discard_publishes_nothing() {
        let delivery = AnswerDelivery::default();
        let mut tail = "kept".to_string();
        let progress = chat_progress(
            ProgressEvent::StreamDiscard {
                reason: StreamDiscardReason::Superseded,
            },
            &delivery,
            &mut tail,
            false,
        );
        assert!(progress.is_none());
        assert_eq!(tail, "kept");
    }
}

#[cfg(test)]
mod after_answer_tests {
    use super::*;

    fn kinds(progress: impl Iterator<Item = ChatProgress>) -> Vec<String> {
        progress
            .map(|progress| {
                serde_json::to_value(progress).unwrap()["type"]
                    .as_str()
                    .unwrap()
                    .to_string()
            })
            .collect()
    }

    #[test]
    fn route_published_after_completion() {
        let citation = workforce_core::code_citation::CodeCitation {
            repo: "workforce".into(),
            owner: String::new(),
            repo_name: String::new(),
            commit: "abc123".into(),
            path: "src/lib.rs".into(),
            start_line: 1,
            end_line: 2,
            kind: "fn".into(),
            symbol: "main".into(),
        };
        let line = "Claude Code · sonnet · Alice's plan".to_string();
        assert_eq!(
            kinds(after_answer(vec![citation], Some(line.clone()))),
            ["code_citations", "route"]
        );
        assert_eq!(kinds(after_answer(Vec::new(), Some(line))), ["route"]);
        assert!(kinds(after_answer(Vec::new(), None)).is_empty());
    }

    #[test]
    fn every_turn_closes_with_done() {
        let refused = vec![ChatProgress::Error {
            message: crate::hub_core::live_turns::TURN_STILL_RUNNING.into(),
        }];
        assert_eq!(
            kinds(closing(refused, None, "s-1".into())),
            ["error", "done"]
        );
        assert_eq!(kinds(closing(Vec::new(), None, "s-1".into())), ["done"]);
        let end = ChatProgress::TurnEnd(workforce_common::channels::desk::TurnEnd {
            turn_ref: Some("ref-1".into()),
            reply_id: None,
        });
        assert_eq!(
            kinds(closing(Vec::new(), Some(end), "s-1".into())),
            ["turn_end", "done"],
            "a Desk turn ends before its request does"
        );
    }

    /// What only a Desk gateway shows goes to Desk turns alone, so a gateway
    /// on the previous build never meets a variant it cannot read.
    #[test]
    fn waits_and_snapshots_are_published_for_desk_turns_only() {
        let delivery = AnswerDelivery::default();
        let waiting = || ProgressEvent::RateLimited {
            wait_secs: 3,
            attempt: 1,
        };
        let switching = || ProgressEvent::SlowModelFailover {
            model: "fast".into(),
            elapsed_secs: 30,
        };
        let snapshot = || {
            ProgressEvent::Snapshot(workforce_common::channels::desk::TurnSnapshot {
                stamp: workforce_common::channels::desk::TurnStamp {
                    turn_ref: None,
                    reply_id: "r".into(),
                },
                since: chrono::Utc::now(),
                status: workforce_common::channels::desk::TurnPhase::Thinking,
                text: String::new(),
            })
        };
        for event in [waiting(), switching(), snapshot()] {
            assert!(chat_progress(event, &delivery, &mut String::new(), false).is_none());
        }
        assert!(matches!(
            chat_progress(waiting(), &delivery, &mut String::new(), true),
            Some(ChatProgress::RateLimited {
                wait_secs: 3,
                attempt: 1
            })
        ));
        assert!(matches!(
            chat_progress(switching(), &delivery, &mut String::new(), true),
            Some(ChatProgress::SlowModelFailover { .. })
        ));
        assert!(matches!(
            chat_progress(snapshot(), &delivery, &mut String::new(), true),
            Some(ChatProgress::Snapshot(_))
        ));
    }
}

#[cfg(test)]
mod answer_delivery_tests {
    use super::*;

    fn delivers(events: Vec<ProgressEvent>) -> bool {
        let mut delivery = AnswerDelivery::default();
        events.iter().for_each(|event| delivery.note(event));
        delivery.delivers_response()
    }

    #[test]
    fn connect_notice_after_streamed_rounds_is_delivered() {
        let delta = || ProgressEvent::StreamDelta {
            text: "partial".into(),
        };
        let prompt = workforce_core::subscription::ConnectPrompt {
            kind: workforce_core::subscription::SubscriptionKind::ChatGpt,
            agent_id: "allan".into(),
            flow_id: None,
            reason: "no ChatGPT login of yours".into(),
            private: true,
            resumes: true,
        };
        assert!(delivers(Vec::new()));
        assert!(
            !delivers(vec![delta()]),
            "a streamed answer is not repeated"
        );
        assert!(delivers(vec![
            delta(),
            ProgressEvent::ConnectRequired { prompt }
        ]));
    }

    #[test]
    fn a_response_after_a_retracted_stream_is_delivered() {
        use workforce_agent::runner::StreamDiscardReason;
        let delta = || ProgressEvent::StreamDelta {
            text: "partial".into(),
        };
        let discard = |reason| ProgressEvent::StreamDiscard { reason };
        assert!(delivers(vec![delta(), discard(StreamDiscardReason::Retry)]));
        assert!(!delivers(vec![
            delta(),
            discard(StreamDiscardReason::Interrupted),
            delta()
        ]));
        assert!(
            !delivers(vec![delta(), discard(StreamDiscardReason::Superseded)]),
            "a superseded draft stays on screen"
        );
    }
}

/// The cancel-then-resend sequence a client runs, end to end through a real
/// NATS server: `WORKFORCE_TEST_NATS_URL=nats://localhost:4222`.
#[cfg(test)]
mod live_control_tests {
    use super::*;
    use std::time::Duration;
    use wiremock::matchers::{body_partial_json, method, path};
    use wiremock::{Mock, ResponseTemplate};
    use workforce_nats::chat::{TurnControl, TurnControlReply};

    fn request(session_id: &str, content: &str) -> NatsChatRequest {
        NatsChatRequest {
            session_id: session_id.into(),
            channel: "ios".into(),
            user: "alice".into(),
            content: content.into(),
            display_content: None,
            attachments: Vec::new(),
            voice_channel: None,
            answers_approvals: false,
            desk: None,
        }
    }

    async fn next_progress(sub: &mut async_nats::Subscriber) -> ChatProgress {
        let msg = tokio::time::timeout(Duration::from_secs(30), sub.next())
            .await
            .expect("progress in time")
            .expect("subscription open");
        serde_json::from_slice(&msg.payload).unwrap()
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_cancelled_turn_settles_silently_and_the_next_message_runs() {
        let Ok(url) = std::env::var("WORKFORCE_TEST_NATS_URL") else {
            return;
        };
        let nats = async_nats::connect(url).await.expect("connect");
        workforce_nats::streams::StreamManager::new(async_nats::jetstream::new(nats.clone()), 1)
            .ensure_all_streams()
            .await
            .expect("streams");
        let tmp = tempfile::tempdir().unwrap();
        let (service, server) = crate::serve::testing::service_on_mock_llm(tmp.path()).await;
        service.set_nats(nats.clone());
        Mock::given(method("POST"))
            .and(path("/v1/chat/completions"))
            .and(body_partial_json(serde_json::json!({"stream": true})))
            .respond_with(ResponseTemplate::new(200).set_delay(Duration::from_secs(600)))
            .with_priority(1)
            .up_to_n_times(1)
            .mount(&server)
            .await;

        let agent = service.agent_id.as_str().to_string();
        let session_id = uuid::Uuid::new_v4().to_string();
        let mut progress = nats
            .subscribe(workforce_nats::subjects::agent_progress(
                &agent,
                &session_id,
            ))
            .await
            .unwrap();
        let first = tokio::spawn({
            let (nats, agent, service) = (nats.clone(), agent.clone(), Arc::clone(&service));
            let request = request(&session_id, "check the latest workforce changes");
            async move { process_chat_request(nats, &agent, service, request).await }
        });
        assert!(matches!(
            next_progress(&mut progress).await,
            ChatProgress::Typing
        ));
        while !server
            .received_requests()
            .await
            .unwrap()
            .iter()
            .any(|r| String::from_utf8_lossy(&r.body).contains("\"stream\":true"))
        {
            tokio::time::sleep(Duration::from_millis(50)).await;
        }

        let cancel = serde_json::to_vec(&TurnControl::Cancel).unwrap();
        let reply = nats
            .request(
                workforce_nats::subjects::turn_control(&agent, &session_id),
                cancel.into(),
            )
            .await
            .expect("the owner answers the cancel");
        let reply: TurnControlReply = serde_json::from_slice(&reply.payload).unwrap();
        assert!(reply.applied, "acknowledged only once settled");
        assert!(matches!(
            service
                .live_turns
                .apply(&session_id, crate::hub_core::live_turns::Control::Cancel)
                .await,
            crate::hub_core::live_turns::Applied::NotLive
        ));
        tokio::time::timeout(Duration::from_secs(30), first)
            .await
            .expect("the stopped turn's worker finishes")
            .unwrap();
        let mut tail = Vec::new();
        loop {
            let event = next_progress(&mut progress).await;
            let done = matches!(event, ChatProgress::Done { .. });
            tail.push(event);
            if done {
                break;
            }
        }
        assert!(
            tail.iter().all(|e| !matches!(
                e,
                ChatProgress::StreamEnd
                    | ChatProgress::StreamDelta { .. }
                    | ChatProgress::Error { .. }
            )),
            "nothing of the stopped turn follows the stop: {tail:?}"
        );

        let second = tokio::spawn({
            let (nats, agent, service) = (nats.clone(), agent.clone(), Arc::clone(&service));
            let request = request(&session_id, "just tell me in the background then");
            async move { process_chat_request(nats, &agent, service, request).await }
        });
        let mut answer = String::new();
        loop {
            match next_progress(&mut progress).await {
                ChatProgress::StreamDelta { text } => answer.push_str(&text),
                ChatProgress::Error { message } => {
                    panic!("the next message was refused: {message}")
                }
                ChatProgress::Done { .. } => break,
                _ => {}
            }
        }
        second.await.unwrap();
        assert_eq!(answer, "done", "the next message ran a fresh turn");
    }
}
