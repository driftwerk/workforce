//! Tests of the turn loop's prompt, recall and routing pieces.

use crate::serve::*;
use crate::turn_loop::prompt::{format_named_secret_block, VoiceSurface, FOOTER_TOTAL_MAX_BYTES};
use crate::turn_loop::{
    adapter_contributes_worker_reads, add_redacted_assistant_message, apply_recall_recency_boost,
    classify_context_type, code_context_eligible, dedupe_recall_keep_best, digest_turn_admits,
    footer_segments, format_pinned_memories, format_thread_history, fuse_recall_query,
    observe_replay_prefix, observe_system_prefix, origin_default_max_rounds, plan_turn_extraction,
    post_answer_reviews_eligible, recall_injection_suppressed, resolve_max_rounds,
    run_as_principal, should_inject_session_intro, voice_mode_directive, without_request_unless,
    GroundingOutcome,
};
use chrono::{TimeZone, Utc};
use std::collections::HashMap;
use workforce_agent::llm::types::LlmMessage;
use workforce_common::session::Session;
use workforce_common::settings::LlmConfig;

#[test]
fn named_secret_block_is_none_when_empty() {
    assert!(format_named_secret_block(&[]).is_none());
}

// ── grounding outcomes: only a real finding speaks ──
//
// The contract inverted on 2026-07-31, deliberately. Previously a check
// that started and could not finish HAD to tell the reader, because a
// status line had announced it — silence would have read as verification.
// Nothing announces a check any more (the announcement was removed in the
// same change), so silence is uninformative everywhere, and an advisory
// pass that failed is a log line, not a caveat the reader cannot act on.
// The reader's visible confirmation is the deterministic grounding line
// inside the answer, not this footer.

/// Advisory failures are silent. This is only honest because NO consumer
/// announces a check (the AnswerSettled event deliberately carries no
/// "reviewing" signal) — if an announcement is ever reintroduced, this
/// invariant must flip back with it.
#[test]
fn advisory_check_failures_are_log_only_never_user_visible() {
    for outcome in [
        GroundingOutcome::Incomplete,
        GroundingOutcome::Incomplete,
        GroundingOutcome::PartiallyChecked,
    ] {
        assert!(
            outcome.footer().is_none(),
            "an unannounced advisory failure must not become a user caveat"
        );
    }
}

/// Success is silent: the turn's completion is the confirmation, and a
/// footer on every clean answer would be noise on the common path.
#[test]
fn a_clean_check_stays_quiet() {
    assert!(GroundingOutcome::Clean.footer().is_none());
}

/// A real finding still speaks — silence-on-failure must never swallow an
/// actual contradiction the pass DID reach.
#[test]
fn a_real_finding_is_still_delivered() {
    let block = "⚠ Verify before acting — these claims aren't confirmed:".to_string();
    assert_eq!(
        GroundingOutcome::Flagged(block.clone()).footer(),
        Some(block)
    );
}

/// Never announced, so nothing is owed.
#[test]
fn a_check_that_never_ran_reports_nothing() {
    assert!(GroundingOutcome::NotRun.footer().is_none());
}

/// A turn the reader stopped is not a failed check — they are not waiting.
#[test]
fn a_cancelled_turn_reports_no_failure() {
    assert!(GroundingOutcome::Cancelled.footer().is_none());
}

#[test]
fn a_finding_is_delivered_unchanged() {
    let block = "⚠ Verify before acting\n- [unverified] something".to_string();
    assert_eq!(
        GroundingOutcome::Flagged(block.clone()).footer(),
        Some(block)
    );
}

#[test]
fn digest_block_absent_for_automated_user() {
    // Automated sources own user-keyed, Dm-classified sessions, so the
    // session-shape gate alone would admit them — the turn-shape gate
    // must reject every synthetic source and monitor turns.
    for user in [
        "scheduler",
        "monitor",
        "workflow-engine",
        "heartbeat",
        "games",
    ] {
        let session = Session::new("scheduler:daily", user, "agent");
        assert!(
            !digest_turn_admits(&session, user, None),
            "{user} must not receive a digest"
        );
    }

    // A real user in a private DM is admitted…
    let dm = Session::new("slack:D1", "U1", "agent");
    assert!(digest_turn_admits(&dm, "U1", Some("D1")));
    assert!(
        digest_turn_admits(&dm, "U1", None),
        "channel-less CLI/gateway"
    );
    // …but not when the turn delivers to a shared channel, and never for
    // a shared-owner session.
    assert!(!digest_turn_admits(&dm, "U1", Some("C1")));
    let shared = Session::new("slack:ws:C1:100.1", "shared:100.1", "agent");
    assert!(!digest_turn_admits(&shared, "U1", Some("D1")));
}

// ── Recall recency boost ─────────────────────────────────────────────

fn recall_result(
    score: f32,
    event_age: chrono::Duration,
    now: chrono::DateTime<Utc>,
) -> workforce_memory::types::MemorySearchResult {
    let memory = workforce_memory::types::Memory::new(
        "m",
        workforce_memory::types::MemoryLayer::Episodic,
        workforce_core::types::AgentId("agent".into()),
    )
    .with_event_time(now - event_age);
    workforce_memory::types::MemorySearchResult { memory, score }
}

#[test]
fn boost_is_noop_at_zero() {
    let now = Utc::now();
    let mut results = vec![
        recall_result(0.62, chrono::Duration::minutes(5), now),
        recall_result(0.75, chrono::Duration::days(7), now),
    ];
    apply_recall_recency_boost(&mut results, now, 0.0, 6.0);
    assert_eq!(results[0].score, 0.62);
    assert_eq!(results[1].score, 0.75);

    // A non-positive half-life is likewise a no-op.
    apply_recall_recency_boost(&mut results, now, 0.25, 0.0);
    assert_eq!(results[0].score, 0.62);
    assert_eq!(results[1].score, 0.75);
}

#[test]
fn boost_half_life_math() {
    let now = Utc::now();
    let (boost, half_life) = (0.25, 6.0);

    let mut fresh = vec![recall_result(0.5, chrono::Duration::zero(), now)];
    apply_recall_recency_boost(&mut fresh, now, boost, half_life);
    assert!(
        (fresh[0].score - 0.5 * (1.0 + boost)).abs() < 1e-4,
        "age 0 → ×(1+b)"
    );

    let mut at_half_life = vec![recall_result(0.5, chrono::Duration::hours(6), now)];
    apply_recall_recency_boost(&mut at_half_life, now, boost, half_life);
    assert!(
        (at_half_life[0].score - 0.5 * (1.0 + boost / 2.0)).abs() < 1e-4,
        "age = half-life → ×(1+b/2)"
    );

    let mut ancient = vec![recall_result(0.5, chrono::Duration::days(30), now)];
    apply_recall_recency_boost(&mut ancient, now, boost, half_life);
    assert!(
        (ancient[0].score - 0.5).abs() < 1e-4,
        "age ≫ half-life → ×1.0"
    );
}

#[test]
fn boost_uses_event_time_not_created_at() {
    // The backfill hazard: a memory written just now about last week's
    // events must read as old — no meaningful boost.
    let now = Utc::now();
    let mut results = vec![recall_result(0.6, chrono::Duration::days(7), now)];
    assert!(results[0].memory.created_at > now - chrono::Duration::minutes(1));
    apply_recall_recency_boost(&mut results, now, 0.25, 6.0);
    assert!((results[0].score - 0.6).abs() < 1e-4);
}

#[test]
fn fresh_memory_outranks_older_similar() {
    let now = Utc::now();
    let mut results = vec![
        recall_result(0.75, chrono::Duration::days(7), now),
        recall_result(0.62, chrono::Duration::minutes(5), now),
    ];
    apply_recall_recency_boost(&mut results, now, 0.25, 6.0);
    results.sort_unstable_by(|a, b| {
        b.score
            .partial_cmp(&a.score)
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    assert!(
        (results[0].memory.event_time - now).num_hours().abs() < 1,
        "the 5-minute-old memory sorts first (0.62 → ~0.774 beats 0.75)"
    );
}

#[test]
fn boost_rescues_borderline_above_min_score_gate() {
    let now = Utc::now();
    let mut results = vec![recall_result(0.35, chrono::Duration::minutes(5), now)];
    apply_recall_recency_boost(&mut results, now, 0.25, 6.0);
    assert!(
        results[0].score > 0.4,
        "fresh 0.35 clears the 0.4 injection gate, got {}",
        results[0].score
    );
}

#[test]
fn boost_capped_at_one() {
    let now = Utc::now();
    let mut results = vec![recall_result(0.9, chrono::Duration::zero(), now)];
    apply_recall_recency_boost(&mut results, now, 10.0, 6.0);
    assert_eq!(results[0].score, 1.0);
}

#[test]
fn recall_injection_suppressed_for_steps_and_recaps_only() {
    assert!(recall_injection_suppressed(true, None));
    assert!(recall_injection_suppressed(
        true,
        Some("run the fetch step")
    ));
    assert!(recall_injection_suppressed(
        false,
        Some("what did we discuss yesterday?")
    ));
    assert!(!recall_injection_suppressed(
        false,
        Some("cluster the interop errors")
    ));
    assert!(!recall_injection_suppressed(false, None));
}

#[test]
fn post_answer_reviews_skip_voice_and_workflow_turns() {
    assert!(post_answer_reviews_eligible(&Session::new(
        "slack-dm", "u", "agent"
    )));
    assert!(!post_answer_reviews_eligible(&Session::new(
        "workflow:inst-1:step-a",
        "u",
        "agent"
    )));
    let mut voice = Session::new("slack-dm", "u", "agent");
    voice.metadata = serde_json::json!({ "voice_turn": true });
    assert!(!post_answer_reviews_eligible(&voice));
}

/// The bridge reopens the microphone after every reply, and only the agent
/// knows when the exchange is over. Left to a per-agent instruction this
/// was simply never used — every "playing that for you" ended with the
/// device listening to an empty room.
#[test]
fn a_satellite_is_told_how_to_end_the_conversation() {
    let directive = voice_mode_directive("fishaudio", "voice-pe");
    assert!(directive.contains("<end_conversation>"), "{directive}");
    // Vague guidance is what failed: "a natural end" never matched
    // "playing music" in practice, so the rule names the case.
    assert!(
        directive.contains("Playing Man in the Mirror"),
        "{directive}"
    );
}

/// The same agent answers this correctly in Slack, where it runs a command,
/// and wrongly by voice, where it answers from context and reaches for a
/// time it gave hours earlier. Being told to be exact did not help — it was
/// exact about the wrong number — so it is told to go and look.
#[test]
fn voice_replies_are_told_to_check_the_time_rather_than_recall_it() {
    for channel in ["voice-pe", "ios"] {
        let directive = voice_mode_directive("fishaudio", channel);
        assert!(
            directive.contains("check it with a tool before answering"),
            "{channel}: {directive}"
        );
    }
}

/// The case that failed in use: "catch you later" got "have a good day"
/// back with the microphone still open, so the exchange had to be ended
/// twice.
#[test]
fn the_agent_is_told_to_end_on_a_goodbye() {
    let directive = voice_mode_directive("fishaudio", "voice-pe");
    assert!(directive.contains("catch you later"), "{directive}");
    assert!(directive.contains("do not wait to"), "{directive}");
}

/// Closing too eagerly is the worse failure — it interrupts — so the
/// guidance has to say which way to err.
#[test]
fn the_agent_is_told_to_err_towards_leaving_it_open() {
    let directive = voice_mode_directive("fishaudio", "voice-pe");
    assert!(
        directive.contains("When in doubt, leave it open"),
        "{directive}"
    );
}

/// On a phone the microphone is not left open, so the marker would be
/// meaningless and the agent should not be told to emit it.
#[test]
fn a_screen_surface_is_not_told_about_the_marker() {
    let directive = voice_mode_directive("fishaudio", "ios");
    assert!(!directive.contains("<end_conversation>"), "{directive}");
}

/// Pinned memories carried no date at all, which made a standing rule and a
/// fact about last month read alike. When two pins contradict — and they do,
/// because instructions get revised — the later one is almost always the one
/// meant, and without a date there is nothing to decide that on.
#[test]
fn pinned_memories_carry_the_date_they_were_pinned() {
    use workforce_memory::types::Memory;
    let mut m = Memory::new(
        "always filter completed tasks".to_string(),
        workforce_memory::types::MemoryLayer::Semantic,
        workforce_core::types::AgentId::new("allan"),
    );
    m.created_at = chrono::DateTime::parse_from_rfc3339("2026-08-03T09:00:00Z")
        .unwrap()
        .with_timezone(&chrono::Utc);
    let rendered = format_pinned_memories(
        std::slice::from_ref(&m),
        &workforce_core::types::AgentId::new("allan"),
    );
    assert!(rendered.contains("(pinned 2026-08-03)"), "{rendered}");
    assert!(
        rendered.contains("always filter completed tasks"),
        "{rendered}"
    );
}

/// Spoken replies are paraphrased by nature, and a fast voice model will
/// happily round a timestamp — 8:27 for 8:22 — which reads as an assistant
/// that is simply wrong. With no screen there is nothing to check it
/// against, so exactness has to be asked for explicitly.
#[test]
fn voice_replies_are_told_to_keep_exact_values_exact() {
    let directive = voice_mode_directive("fishaudio", "voice-pe");
    assert!(
        directive.contains("Say exact values exactly"),
        "{directive}"
    );
    // Applies on a phone too: the reply is spoken there as well.
    let phone = voice_mode_directive("fishaudio", "ios");
    assert!(phone.contains("Say exact values exactly"), "{phone}");
}

/// The satellite has no screen, so a reply that assumes one strands the
/// listener with detail they cannot recover.
#[test]
fn a_satellite_is_told_there_is_no_screen() {
    let directive = voice_mode_directive("fishaudio", "voice-pe");
    assert!(directive.contains("no screen"), "{directive}");
    assert!(directive.contains("cannot re-read"), "{directive}");
}

/// On a phone the reply is also written down, so ordinary detail is safe
/// and telling the agent otherwise would make it needlessly vague.
#[test]
fn a_phone_is_told_the_reply_is_also_on_screen() {
    let directive = voice_mode_directive("fishaudio", "ios");
    assert!(directive.contains("on screen"), "{directive}");
    assert!(!directive.contains("no screen"), "{directive}");
}

/// An unrecognised surface is assumed to be a speaker: being a little
/// plainer than necessary costs nothing, while wrongly assuming a screen
/// loses the answer.
#[test]
fn an_unknown_surface_is_assumed_to_have_no_screen() {
    assert_eq!(VoiceSurface::of("kitchen-satellite"), VoiceSurface::Speaker);
    assert_eq!(VoiceSurface::of("ios"), VoiceSurface::Screen);
}

/// Surface and TTS provider are independent: the same device can change
/// synthesizer, and the same synthesizer serves both surfaces.
#[test]
fn the_surface_and_the_provider_are_both_described() {
    let directive = voice_mode_directive("fishaudio", "voice-pe");
    assert!(directive.contains("emotion tags"), "provider rules missing");
    assert!(
        directive.contains("speaker in a room"),
        "surface rules missing"
    );
}

#[test]
fn code_context_is_ineligible_for_spoken_turns_only() {
    // A spoken turn: no code context — symbol-name false positives would
    // dump ~1,300 lines into a latency-sensitive prompt.
    let mut spoken = workforce_common::session::Session::new("ios", "u", "agent");
    spoken.metadata = serde_json::json!({ "voice_mode": true, "voice_turn": true });
    assert!(!code_context_eligible(&spoken));

    // Typing again in the same iOS session — `voice_mode` still latched
    // from earlier speech — gets code context back (DW-571). Gating on the
    // session flag was what silently starved typed code questions.
    let mut typed = workforce_common::session::Session::new("ios", "u", "agent");
    typed.metadata = serde_json::json!({ "voice_mode": true, "voice_turn": false });
    assert!(typed.is_voice(), "session flag is expected to still latch");
    assert!(code_context_eligible(&typed));

    // A session that never saw voice at all.
    let text = workforce_common::session::Session::new("ios", "u", "agent");
    assert!(code_context_eligible(&text));
}

/// The review footers ("⚠ Verify before acting", unverified-claim lists)
/// are written to be read; every voice surface speaks stream text
/// verbatim, so iOS and the voice bridge read them aloud. Spoken turns
/// skip the reviews entirely; typing in the same latched-voice session
/// keeps them.
#[test]
fn review_footers_are_ineligible_for_spoken_turns_only() {
    let mut spoken = workforce_common::session::Session::new("ios", "u", "agent");
    spoken.metadata = serde_json::json!({ "voice_mode": true, "voice_turn": true });
    assert!(!post_answer_reviews_eligible(&spoken));

    let mut typed = workforce_common::session::Session::new("ios", "u", "agent");
    typed.metadata = serde_json::json!({ "voice_mode": true, "voice_turn": false });
    assert!(typed.is_voice(), "session flag is expected to still latch");
    assert!(post_answer_reviews_eligible(&typed));

    let text = workforce_common::session::Session::new("slack", "u", "agent");
    assert!(post_answer_reviews_eligible(&text));
}

#[test]
fn run_as_principal_prefers_nonempty_run_as_over_sender() {
    assert_eq!(run_as_principal(Some("owner"), "scheduler"), "owner");
    assert_eq!(run_as_principal(None, "scheduler"), "scheduler");
    assert_eq!(run_as_principal(Some(""), "scheduler"), "scheduler");
}

/// The five annotation blocks were individually bounded but collectively
/// unbounded — on a turn where several fired they outran the answer.
#[test]
fn footer_blocks_share_one_budget_and_name_what_they_drop() {
    let big = |n: usize| "x".repeat(n);
    let segments = footer_segments(vec![
        ("failed tools", big(600)),
        ("data grounding", big(600)),
        ("source grounding", big(600)),
        ("conclusion review", big(600)),
        ("commitments", big(600)),
    ]);

    // Admitted whole, in priority order, until the budget is spent.
    assert_eq!(segments.len(), 3, "2 admitted + the dropped-summary line");
    let body: usize = segments[..2].iter().map(String::len).sum();
    assert!(body <= FOOTER_TOTAL_MAX_BYTES, "body was {body} bytes");

    // Nothing vanishes silently — the reader is told checks fired.
    let note = segments.last().unwrap();
    assert!(note.contains("3 further checks"));
    assert!(note.contains("source grounding"));
    assert!(note.contains("conclusion review"));
    assert!(note.contains("commitments"));
}

/// A budget that could suppress every warning would be worse than none:
/// one oversized block still gets through rather than being dropped whole.
#[test]
fn footer_always_admits_the_highest_priority_block() {
    let segments = footer_segments(vec![
        ("source grounding", "y".repeat(FOOTER_TOTAL_MAX_BYTES * 3)),
        ("commitments", "z".repeat(50)),
    ]);
    assert_eq!(segments.len(), 2);
    assert!(segments[0].starts_with("yyy"));
    assert!(segments[1].contains("1 further check flagged"));
}

/// A clean turn grows no footer at all.
#[test]
fn footer_is_empty_when_nothing_fired() {
    assert!(footer_segments(vec![]).is_empty());
}

#[test]
fn worker_preloads_only_read_only_service_adapters() {
    // The read-only service adapters a worker/investigate fan-out needs are
    // pre-loaded into the shared registry before the worker snapshot, since
    // a worker can't `system.enable_skill` them itself. Each must report it
    // contributes worker-usable read tools, or `ensure_worker_adapters_loaded_as`
    // would skip it and leave the worker stranded (driftwerk-prod 2026-06-10).
    for adapter in [
        "linear", "jira", "datadog", "metabase", "postmark", "sendgrid",
    ] {
        assert!(
            adapter_contributes_worker_reads(adapter),
            "{adapter} contributes read-only tools a worker uses — must be pre-loaded"
        );
    }
    // Adapters with no worker-usable read tools stay deferred: pre-loading
    // them would bloat the shared registry (and, for browser, spin up heavy
    // sessions) for no worker benefit. github/google/email are write/auth
    // adapters absent from `read_only_adapter_tools`; browser is permitted by
    // the worker patterns but is intentionally not in the service-adapter set.
    for adapter in ["github", "google", "email", "browser", "xero", "notion"] {
        assert!(
            !adapter_contributes_worker_reads(adapter),
            "{adapter} has no worker read-only service tools — must stay deferred"
        );
    }
}

#[test]
fn format_thread_history_renders_markers_and_keeps_file_only_messages() {
    use workforce_common::channels::{ChannelFile, ChannelMessage};
    let file = ChannelFile {
        id: "F1".into(),
        name: "generated.jpg".into(),
        mime_type: "image/jpeg".into(),
        url: None,
        size: None,
    };
    let mk = |text: &str| ChannelMessage {
        channel: "C1".into(),
        message_id: "1".into(),
        user: "U1".into(),
        user_name: Some("Penny".into()),
        channel_name: None,
        text: text.into(),
        timestamp: "1".into(),
        thread_id: None,
        files: vec![file.clone()],
        bot_id: None,
    };
    let out = format_thread_history(&[mk("here you go"), mk("")], &HashMap::new());
    assert!(out.contains("Penny: here you go [image: generated.jpg]"));
    // The file-only message is kept rather than rendering as a blank line.
    assert!(out.contains("Penny: [image: generated.jpg]"));
}

#[test]
fn named_secret_block_lists_names_only() {
    let block = format_named_secret_block(&["datadog_api_key".into(), "stripe_key".into()])
        .expect("block present");
    assert!(block.contains("[[secret:<name>]]"));
    assert!(block.contains("- datadog_api_key"));
    assert!(block.contains("- stripe_key"));
}

#[test]
fn assistant_message_is_masked_at_rest() {
    let mut session = workforce_common::session::Session::new("slack", "u", "agent");
    add_redacted_assistant_message(
        &mut session,
        concat!(
            "your key is ghp_AB",
            "CDEFGHIJKLMNOPQRSTUVWXYZabcdefghij1234 — keep it safe"
        ),
    );
    let stored = &session.messages.last().expect("message stored").content;
    assert!(!stored.contains("ABCDEFGHIJKLMNOPQRSTUVWXYZ"));
    assert!(stored.contains("ghp_"));
}

// ── should_inject_session_intro visibility gating ──────────────────

#[test]
fn intro_skipped_in_shared_channel() {
    assert!(!should_inject_session_intro(
        "channel-key",
        true,
        Some("C123"),
        Some("100.001"),
    ));
}

#[test]
fn intro_fires_in_dm() {
    assert!(should_inject_session_intro(
        "dm-key",
        true,
        Some("D123"),
        Some("100.001"),
    ));
}

#[test]
fn intro_skipped_when_not_new_session() {
    assert!(!should_inject_session_intro(
        "dm-key",
        false,
        Some("D123"),
        Some("100.001"),
    ));
}

#[test]
fn intro_skipped_when_no_thread() {
    assert!(!should_inject_session_intro(
        "dm-key",
        true,
        Some("D123"),
        None
    ));
}

// ── dedupe_recall_keep_best ──────────────────────────────────────

fn fake_recall_result(id: &str, score: f32) -> workforce_memory::types::MemorySearchResult {
    let now = Utc::now();
    let memory = workforce_memory::types::Memory {
        id: workforce_memory::types::MemoryId(id.to_string()),
        content: format!("content-{id}"),
        layer: workforce_memory::types::MemoryLayer::Episodic,
        owner: workforce_core::types::AgentId::new("agent"),
        visibility: workforce_memory::types::Visibility::Private,
        importance: 0.5,
        tags: vec![],
        source: workforce_memory::types::MemorySource::Passive,
        identity_id: None,
        source_session_id: None,
        source_message_ids: vec![],
        created_at: now,
        event_time: now,
        last_accessed: now,
        access_count: 0,
        recurrence_count: 0,
        last_recurrence_at: None,
        deleted: false,
    };
    workforce_memory::types::MemorySearchResult { memory, score }
}

#[test]
fn dedupe_recall_keeps_higher_score() {
    let merged = dedupe_recall_keep_best(vec![
        fake_recall_result("a", 0.5),
        fake_recall_result("a", 0.8),
        fake_recall_result("b", 0.6),
    ]);
    let by_id: std::collections::HashMap<_, _> = merged
        .iter()
        .map(|r| (r.memory.id.0.clone(), r.score))
        .collect();
    assert_eq!(by_id.len(), 2);
    assert!((by_id["a"] - 0.8).abs() < 1e-6);
    assert!((by_id["b"] - 0.6).abs() < 1e-6);
}

#[test]
fn dedupe_recall_empty_input_returns_empty() {
    let merged = dedupe_recall_keep_best(Vec::new());
    assert!(merged.is_empty());
}

// ── fuse_recall_query ───────────────────────────────────────────

fn fake_session_message(
    role: workforce_common::session::MessageRole,
    content: &str,
) -> workforce_common::session::SessionMessage {
    workforce_common::session::SessionMessage {
        id: uuid::Uuid::new_v4().to_string(),
        role,
        content: content.to_string(),
        tool_data: None,
        timestamp: Utc.with_ymd_and_hms(2026, 5, 17, 12, 0, 0).unwrap(),
        tokens: 0,
        external_id: None,
        attachments: None,
        trust_level: None,
        actor: None,
    }
}

#[test]
fn fuse_recall_query_returns_none_for_empty_history() {
    assert!(fuse_recall_query(&[], "hi").is_none());
}

#[test]
fn fuse_recall_query_excludes_tool_and_empty_rows() {
    use workforce_common::session::MessageRole;
    let messages = vec![
        fake_session_message(MessageRole::User, "where am I going next week?"),
        fake_session_message(MessageRole::Tool, "tool dump output that's irrelevant"),
        fake_session_message(MessageRole::Assistant, "Uganda — flight on Friday"),
        fake_session_message(MessageRole::Assistant, "   "),
    ];
    let q = fuse_recall_query(&messages, "you should know")
        .expect("non-empty user/assistant content present");
    assert!(q.contains("Uganda"));
    assert!(!q.contains("tool dump output"));
    assert!(q.trim_end().ends_with("you should know"));
}

#[test]
fn fuse_recall_query_caps_huge_user_message() {
    use workforce_common::session::MessageRole;
    let messages = vec![fake_session_message(MessageRole::Assistant, "context")];
    let huge: String = "x".repeat(5_000);
    let q = fuse_recall_query(&messages, &huge).expect("non-empty tail");
    // Per-message cap is 400 chars; user_message tail must respect it too
    // so the fused query stays bounded.
    let user_segment = q.rsplit_once('\n').map(|(_, tail)| tail).unwrap_or(&q);
    assert!(
        user_segment.chars().count() <= 400,
        "user-message segment overshot PER_MSG_CHARS cap: {} chars",
        user_segment.chars().count()
    );
}

#[test]
fn fuse_recall_query_orders_assistant_then_user_message() {
    use workforce_common::session::MessageRole;
    let messages = vec![
        fake_session_message(MessageRole::User, "u1"),
        fake_session_message(MessageRole::Assistant, "a1"),
        fake_session_message(MessageRole::User, "u2"),
        fake_session_message(MessageRole::Assistant, "a2"),
    ];
    let q = fuse_recall_query(&messages, "current").expect("non-empty");
    let lines: Vec<&str> = q.split('\n').collect();
    // Tail is reverse-traversal then reversed → oldest first; current
    // user message is appended last.
    assert_eq!(*lines.last().unwrap(), "current");
    assert!(lines[..lines.len() - 1].contains(&"a2"));
}

#[test]
fn fuse_recall_query_respects_total_budget() {
    use workforce_common::session::MessageRole;
    // Each message is just under PER_MSG_CHARS (400), so 4 of them ~=
    // 1600 = MAX_FUSED_CHARS. Pushing a fifth must be skipped by the
    // take_while budget gate.
    let big: String = "y".repeat(380);
    let messages = vec![
        fake_session_message(MessageRole::Assistant, &big),
        fake_session_message(MessageRole::Assistant, &big),
        fake_session_message(MessageRole::Assistant, &big),
        fake_session_message(MessageRole::Assistant, &big),
        fake_session_message(MessageRole::Assistant, &big),
    ];
    let q = fuse_recall_query(&messages, "now").expect("non-empty");
    let total = q.chars().count();
    // Budget 1600 (tail) + up to PER_MSG_CHARS (400) user message + a
    // few separator newlines. Pin the upper bound generously but ensure
    // we're not blowing past it.
    assert!(
        total <= 1600 + 400 + 16,
        "fused query overshot budget: {total}"
    );
}

// ── build_task_content + MediaStrategies ─────────────────────────

// ── octet-stream AV mime inference ───────────────────────────────

// ── resolve_max_rounds ───────────────────────────────────────────

#[test]
fn resolve_max_rounds_prefers_explicit_override() {
    assert_eq!(resolve_max_rounds(Some(20), Some(50), 75), 20);
}

#[test]
fn resolve_max_rounds_falls_through_to_active_when_no_override() {
    assert_eq!(resolve_max_rounds(None, Some(50), 75), 50);
}

#[test]
fn resolve_max_rounds_falls_through_to_default_when_neither() {
    assert_eq!(resolve_max_rounds(None, None, 75), 75);
}

#[test]
fn resolve_max_rounds_override_takes_precedence_over_active() {
    // Background-job override must win even when an agent-wide active
    // override is set — otherwise scheduler caps would silently lose
    // to UI-driven `set_max_rounds` overrides.
    assert_eq!(resolve_max_rounds(Some(20), Some(100), 75), 20);
}

#[test]
fn origin_default_selects_interactive_budget_for_humans_only() {
    let llm = LlmConfig::default();
    assert_eq!(llm.interactive_max_rounds, 16, "config default is 16");
    assert_eq!(llm.max_rounds, 50);

    // Automated sources keep the full budget; everyone else — a real
    // platform user id — gets the interactive one.
    assert_eq!(origin_default_max_rounds("scheduler", &llm), 50);
    assert_eq!(origin_default_max_rounds("monitor", &llm), 50);
    assert_eq!(origin_default_max_rounds("workflow-engine", &llm), 50);
    assert_eq!(origin_default_max_rounds("heartbeat", &llm), 50);
    assert_eq!(origin_default_max_rounds("games", &llm), 50);
    assert_eq!(origin_default_max_rounds("U04ABCDEF", &llm), 16);
    assert_eq!(origin_default_max_rounds("kingsley", &llm), 16);
}

#[test]
fn origin_default_never_outranks_overrides() {
    let llm = LlmConfig::default();
    let interactive_default = origin_default_max_rounds("U04ABCDEF", &llm);
    // Explicit per-call override (scheduler cap, playbook step_max_rounds)
    // passes through untouched.
    assert_eq!(
        resolve_max_rounds(Some(30), None, interactive_default),
        30,
        "explicit override beats the origin default"
    );
    // The runtime vault override still outranks the origin default too.
    assert_eq!(resolve_max_rounds(None, Some(40), interactive_default), 40);
    // Only with neither does the origin-selected default apply.
    assert_eq!(resolve_max_rounds(None, None, interactive_default), 16);
}

#[test]
fn a_trace_header_names_the_channel_the_step_and_what_was_asked() {
    let chat = trace_tags(
        "slack:T1:C9:1700000000.1",
        Some("review PR #123"),
        None,
        |_| panic!("a chat belongs to no workflow"),
    );
    assert_eq!(chat.channel.as_deref(), Some("slack"));
    assert_eq!(chat.request.as_deref(), Some("review PR #123"));
    assert_eq!(chat.task_type.as_deref(), Some("review"));
    let unrecorded = without_request_unless(false, chat.clone());
    assert_eq!(
        unrecorded.request, None,
        "not kept where nothing is recorded"
    );
    assert_eq!(unrecorded.task_type, chat.task_type);
    assert_eq!(without_request_unless(true, chat.clone()), chat);
    assert_eq!((chat.workflow_id, chat.step_id), (None, None));

    let step = trace_tags("workflow:i-1:classify", Some("Classify it."), None, |run| {
        assert_eq!(run, "i-1");
        Some("triage".to_string())
    });
    assert_eq!(step.channel.as_deref(), Some("workflow"));
    assert_eq!(step.workflow_id.as_deref(), Some("triage"));
    assert_eq!(step.step_id.as_deref(), Some("classify"));
    assert_eq!(
        step.request, None,
        "a step's prompt is not a person's request"
    );
    assert!(step.task_type.is_some());

    let background = trace_tags("curation", None, Some("subagent"), |_| None);
    assert_eq!(background.channel.as_deref(), Some("curation"));
    assert_eq!(background.task_type.as_deref(), Some("subagent"));
    assert_eq!(background.request, None);
    let unknown = trace_tags("workflow:gone:run", None, None, |_| None);
    assert_eq!((unknown.workflow_id, unknown.task_type), (None, None));
    assert_eq!(unknown.step_id.as_deref(), Some("run"));
}

#[test]
fn observe_replay_prefix_records_and_reports_a_rewrite() {
    let mut session = Session::new("slack:c", "owner", "agent");
    let a = vec![LlmMessage::user("hello"), LlmMessage::assistant("hi")];
    assert!(observe_replay_prefix(&mut session, &a));
    assert!(session.replay_prefix().is_some());

    let mut extended = a.clone();
    extended.push(LlmMessage::user("one more"));
    assert!(observe_replay_prefix(&mut session, &extended));

    let unrelated = vec![LlmMessage::user("something else entirely")];
    assert!(!observe_replay_prefix(&mut session, &unrelated));
}

#[test]
fn observe_system_prefix_records_and_reports_a_change() {
    let mut session = Session::new("slack:c", "owner", "agent");
    assert!(observe_system_prefix(&mut session, "identity\nskills"));
    assert!(session.system_prefix().is_some());
    assert!(observe_system_prefix(&mut session, "identity\nskills"));
    assert!(!observe_system_prefix(
        &mut session,
        "identity\nskills\npins"
    ));
    assert!(observe_system_prefix(
        &mut session,
        "identity\nskills\npins"
    ));
}

#[test]
fn classify_context_type_maps_room_to_type() {
    use workforce_common::session::SessionContextType;
    // Slack DM ('D' channel) and a CLI surface (no channel id) are both 1:1.
    assert_eq!(
        classify_context_type(Some("D123"), None),
        SessionContextType::Dm
    );
    assert_eq!(classify_context_type(None, None), SessionContextType::Dm);
    // Shared channels are Group, or Thread when threaded.
    assert_eq!(
        classify_context_type(Some("C123"), None),
        SessionContextType::Group
    );
    assert_eq!(
        classify_context_type(Some("C123"), Some("t1")),
        SessionContextType::Thread
    );
}

#[test]
fn plan_turn_extraction_gates_relationship_on_dm_and_principal() {
    use workforce_common::settings::MemoryConfig;
    let off = MemoryConfig::default();
    // Disabled by default → never wanted, even in a DM.
    assert!(!plan_turn_extraction(&off, 1000, true, false, true, false, "alice").1);

    let on = MemoryConfig {
        relationship: true,
        relationship_min_turn_tokens: 40,
        ..MemoryConfig::default()
    };
    // Enabled + DM + a principal + enough tokens → captured.
    assert!(plan_turn_extraction(&on, 1000, true, false, true, false, "alice").1);
    // The regression this change fixes: a non-DM turn is never captured.
    // (Before, the Slack/web path never set `context_type`, so `is_dm` was
    // false for every turn and nothing was ever recorded.)
    assert!(!plan_turn_extraction(&on, 1000, false, false, true, false, "alice").1);
    // Empty/blank principal (anonymous — no canonical identity to key on).
    assert!(!plan_turn_extraction(&on, 1000, true, false, true, false, "   ").1);
    // Below the token floor.
    assert!(!plan_turn_extraction(&on, 10, true, false, true, false, "alice").1);
}

#[test]
fn plan_turn_extraction_gates_passive_on_store_and_sandbox() {
    use workforce_common::settings::MemoryConfig;
    let on = MemoryConfig {
        passive: true,
        passive_min_turn_tokens: 40,
        ..MemoryConfig::default()
    };
    // store present, not sandboxed, not shared → wanted.
    assert!(plan_turn_extraction(&on, 1000, true, false, true, false, "alice").0);
    // no memory store → not wanted.
    assert!(!plan_turn_extraction(&on, 1000, true, false, false, false, "alice").0);
    // game sandbox suppresses passive extraction.
    assert!(!plan_turn_extraction(&on, 1000, true, false, true, true, "alice").0);
    // shared session suppresses passive extraction (no widening to global memory).
    assert!(!plan_turn_extraction(&on, 1000, false, true, true, false, "alice").0);
}

// ── should_inject_session_intro ──────────────────────────────────

#[test]
fn inject_intro_fires_for_new_user_session_in_slack_dm_thread() {
    assert!(should_inject_session_intro(
        "slack:T123:D456:1234.5678",
        true,
        Some("D456"),
        Some("1234.5678"),
    ));
}

#[test]
fn inject_intro_skipped_for_workflow_step_session() {
    assert!(!should_inject_session_intro(
        "workflow:bbdf9e1e:2",
        true,
        Some("D456"),
        Some("1234.5678"),
    ));
}

#[test]
fn inject_intro_skipped_for_scheduler_channel() {
    assert!(!should_inject_session_intro(
        "scheduler:daily-summary",
        true,
        Some("C456"),
        Some("1234.5678"),
    ));
}

#[test]
fn inject_intro_skipped_for_subagent_channel() {
    assert!(!should_inject_session_intro(
        "subagent",
        true,
        Some("C456"),
        Some("1234.5678"),
    ));
}

#[test]
fn inject_intro_skipped_when_session_not_new() {
    assert!(!should_inject_session_intro(
        "slack:T123:C456:1234.5678",
        false,
        Some("C456"),
        Some("1234.5678"),
    ));
}

#[test]
fn inject_intro_skipped_when_thread_id_missing() {
    assert!(!should_inject_session_intro(
        "slack:T123:C456:1234.5678",
        true,
        Some("C456"),
        None,
    ));
}

#[test]
fn inject_intro_skipped_when_channel_id_missing() {
    assert!(!should_inject_session_intro(
        "slack:T123:C456:1234.5678",
        true,
        None,
        Some("1234.5678"),
    ));
}
