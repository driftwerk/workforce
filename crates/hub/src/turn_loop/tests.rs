//! Tests of the turn loop's prompt, recall and routing pieces.

use crate::serve::*;
use crate::turn_loop::prompt::{
    format_named_secret_block, VoiceSurface, DICTATED_TURN_NOTE, FOOTER_TOTAL_MAX_BYTES,
};
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
    let directive = voice_mode_directive("fishaudio", None, "voice-pe");
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
        let directive = voice_mode_directive("fishaudio", None, channel);
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
    let directive = voice_mode_directive("fishaudio", None, "voice-pe");
    assert!(directive.contains("catch you later"), "{directive}");
    assert!(directive.contains("do not wait to"), "{directive}");
}

/// Closing too eagerly is the worse failure — it interrupts — so the
/// guidance has to say which way to err.
#[test]
fn the_agent_is_told_to_err_towards_leaving_it_open() {
    let directive = voice_mode_directive("fishaudio", None, "voice-pe");
    assert!(
        directive.contains("When in doubt, leave it open"),
        "{directive}"
    );
}

/// On a phone the microphone is not left open, so the marker would be
/// meaningless and the agent should not be told to emit it.
#[test]
fn a_screen_surface_is_not_told_about_the_marker() {
    let directive = voice_mode_directive("fishaudio", None, "ios");
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
    let directive = voice_mode_directive("fishaudio", None, "voice-pe");
    assert!(
        directive.contains("Say exact values exactly"),
        "{directive}"
    );
    // Applies on a phone too: the reply is spoken there as well.
    let phone = voice_mode_directive("fishaudio", None, "ios");
    assert!(phone.contains("Say exact values exactly"), "{phone}");
}

/// The satellite has no screen, so a reply that assumes one strands the
/// listener with detail they cannot recover.
#[test]
fn a_satellite_is_told_there_is_no_screen() {
    let directive = voice_mode_directive("fishaudio", None, "voice-pe");
    assert!(directive.contains("no screen"), "{directive}");
    assert!(directive.contains("cannot re-read"), "{directive}");
}

/// On a phone the reply is also written down, so ordinary detail is safe
/// and telling the agent otherwise would make it needlessly vague.
#[test]
fn a_phone_is_told_the_reply_is_also_on_screen() {
    let directive = voice_mode_directive("fishaudio", None, "ios");
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
    assert_eq!(VoiceSurface::of("desk"), VoiceSurface::Desk);
    assert_eq!(VoiceSurface::of("desk:release"), VoiceSurface::Desk);
}

/// Surface and TTS provider are independent: the same device can change
/// synthesizer, and the same synthesizer serves both surfaces.
#[test]
fn the_surface_and_the_provider_are_both_described() {
    let directive = voice_mode_directive("fishaudio", None, "voice-pe");
    assert!(directive.contains("(excited)"), "provider rules missing");
    assert!(
        directive.contains("speaker in a room"),
        "surface rules missing"
    );
}

/// The agent was offered tags S1 doesn't perform ("thoughtful", "concerned",
/// "amused", "serious") and went on to invent its own ("pensive"), which S1
/// reads aloud. It is now given exactly the configured model's set.
#[test]
fn a_fish_voice_is_offered_exactly_what_its_model_performs() {
    let s1 = voice_mode_directive("fish_audio", Some("s1"), "desk");
    assert!(
        s1.contains("(excited)") && s1.contains("(soft tone)"),
        "{s1}"
    );
    for unsupported in ["thoughtful", "concerned", "amused", "serious", "(emphasis)"] {
        assert!(!s1.contains(unsupported), "offers {unsupported}: {s1}");
    }
    assert_eq!(s1, voice_mode_directive("fishaudio", None, "desk"));
    let s2 = voice_mode_directive("fish_audio", Some("s2-pro"), "desk");
    assert!(
        s2.contains("[excited]") && !s2.contains("(excited)"),
        "{s2}"
    );
}

#[test]
fn a_voice_without_directions_is_told_to_use_none() {
    for provider in ["kokoro", "elevenlabs", "system", "gpt-live"] {
        let directive = voice_mode_directive(provider, None, "desk");
        assert!(
            directive.contains("No voice directions"),
            "{provider}: {directive}"
        );
        assert!(!directive.contains("(excited)"), "{provider}: {directive}");
    }
}

/// Spoken to, the agent talks like a person: what only reads well (code,
/// links, paths) is described, never recited, on a screen surface too.
#[test]
fn a_spoken_reply_never_recites_what_only_reads() {
    for channel in ["desk", "ios", "voice-pe"] {
        let directive = voice_mode_directive("kokoro", None, channel);
        assert!(
            directive.contains(
                "Never read out code, symbols, punctuation marks, markdown, URLs or file"
            ),
            "{channel}: {directive}"
        );
        assert!(!directive.contains("normal detail is safe"), "{channel}");
    }
}

/// A phone's reply is kept short and points at the chat for detail; Desk's is
/// on screen and read out whole, so it's written whole.
#[test]
fn desk_is_told_its_whole_reply_is_spoken() {
    for channel in ["desk", "desk:team-room"] {
        let directive = voice_mode_directive("fish_audio", Some("s1"), channel);
        assert!(directive.contains("every word of it"), "{directive}");
        assert!(directive.contains("the code is in the chat"), "{directive}");
        for phone in [
            "offer to put the details in the chat",
            "Keep responses concise",
            "give the gist",
            "or code blocks",
            "Their phone",
        ] {
            assert!(!directive.contains(phone), "{channel} says {phone}");
        }
    }
}

/// The phone and the speaker in a room keep what they were told, word for
/// word: only Desk's reply is read whole.
#[test]
fn the_phone_and_the_satellite_keep_their_rules() {
    const KEPT: &str = "\
         - Never read out code, symbols, punctuation marks, markdown, URLs or file \
           paths. Say what they are in words instead — \"the settings file in your \
           home folder\", \"the link to the pull request\" — and summarise code by \
           what it does, never line by line.\n\
         - When the detail itself matters (a command to run, the code, a list to \
           keep), give the gist aloud and offer to put the details in the chat.\n\
         - No emojis, markdown, bullet lists, headers, tables or code blocks\n\
         - Use proper punctuation for natural pauses\n\
         - Avoid abbreviations and acronyms that read badly aloud\n\
         - Keep responses concise and flowing\n\
         - If you need to convey a list, say it naturally: \
           \"There are three things — first X, then Y, and finally Z\"\n";
    for channel in ["ios", "voice-pe"] {
        let directive = voice_mode_directive("kokoro", None, channel);
        assert!(directive.contains(KEPT), "{channel}: {directive}");
    }
    assert!(voice_mode_directive("kokoro", None, "ios").contains("Their phone or computer"));
    assert!(voice_mode_directive("kokoro", None, "voice-pe").contains("speaker in a room"));
}

#[test]
fn a_dictated_turn_is_read_not_heard() {
    assert!(DICTATED_TURN_NOTE.contains("transcribed"));
    assert!(DICTATED_TURN_NOTE.contains("reply in writing"));
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
        None,
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
        desk: None,
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

/// Slack's block, byte for byte as the turn loop wrote it before Desk had
/// one of its own.
#[test]
fn the_slack_messaging_context_is_unchanged() {
    use super::surface::{surface_context, TurnSurface};
    let in_thread = TurnSurface::of(Some("C123"), Some("1700.1"), None);
    assert_eq!(
        format!(
            "{}\n\nwhat is up?",
            surface_context(&in_thread, "slack:W1:C123:1700.1", "sess-1", "U42", None).unwrap()
        ),
        "[MESSAGING CONTEXT — use these values when calling messaging tools]\nchannel: C123\nsession_channel: slack:W1:C123:1700.1\nthread_id: 1700.1\nsession_id: sess-1\nuser: U42\n\nwhat is up?"
    );
    let top_level = TurnSurface::of(Some("C123"), None, None);
    assert_eq!(
        surface_context(&top_level, "slack:W1:C123:1700.1", "sess-1", "U42", None).unwrap(),
        "[MESSAGING CONTEXT — use these values when calling messaging tools]\nchannel: C123\nsession_channel: slack:W1:C123:1700.1\nsession_id: sess-1\nuser: U42"
    );
    assert!(surface_context(&TurnSurface::of(None, None, None), "ios", "s", "u", None).is_none());
}

/// A live Desk turn's answer says which room message it answers; one re-run
/// with nobody attending answers no ask, so it says none and runs no steps.
#[tokio::test]
async fn a_desk_answer_names_the_message_its_turn_answers_and_a_resumed_one_none() {
    use workforce_common::channels::desk::{
        DeliveryKind, DeskPlace, DeskScope, DeskSpeaker, DeskTarget, DeskTurn, DeskTurnContext,
    };
    let room = uuid::Uuid::new_v4();
    let asked = uuid::Uuid::new_v4();
    let channel = format!("desk:{room}");
    let place = DeskPlace::Room {
        room,
        name: "Atlas launch".into(),
        thread: None,
    };
    let turn = DeskTurn {
        place: place.clone(),
        speaker: DeskSpeaker::Person,
        hop: 0,
        context: Vec::new(),
        message: Some(asked),
        spoken: false,
        voice: None,
        members: Vec::new(),
    };
    let live = DeskTurnContext::new(turn, &channel, None, false, None, "reply-1".into());
    let (id, mark) = crate::turn_loop::records::reply_slot(Some(&live), &channel);
    let mark = mark.unwrap();
    assert_eq!(id.as_deref(), Some("reply-1"));
    assert_eq!((mark.room, mark.in_reply_to), (Some(room), Some(asked)));

    let scope = DeskScope {
        target: DeskTarget::of_turn(&place, &channel, None, false),
        owner: "person-1".into(),
        attended: false,
        relayed: false,
    };
    let (_, resumed) = workforce_agent::runner::with_desk(Some(scope), async {
        crate::turn_loop::records::reply_slot(None, &channel)
    })
    .await;
    let resumed = resumed.unwrap();
    assert_eq!(resumed.delivery, Some(DeliveryKind::Resumed));
    assert_eq!((resumed.in_reply_to, resumed.steps.len()), (None, 0));
}

/// A Desk turn in the person's DM, asked with message `asked`.
fn dm_turn(asked: uuid::Uuid) -> workforce_common::channels::desk::DeskTurnContext {
    use workforce_common::channels::desk::{
        DeskPlace, DeskSpeaker, DeskTurn, DeskTurnContext, DESK_CHANNEL,
    };
    let turn = DeskTurn {
        place: DeskPlace::Dm,
        speaker: DeskSpeaker::Person,
        hop: 0,
        context: Vec::new(),
        message: Some(asked),
        spoken: false,
        voice: None,
        members: Vec::new(),
    };
    DeskTurnContext::new(
        turn,
        DESK_CHANNEL,
        None,
        false,
        Some("ref-1".into()),
        uuid::Uuid::new_v4().to_string(),
    )
}

/// Runs `question` as a Desk turn in `owner`'s DM, with `follow_up` sent
/// now into it under the client's id `steered`, to its end.
async fn steered_turn(
    service: &std::sync::Arc<AgentService>,
    owner: &str,
    question: &str,
    asked: uuid::Uuid,
    follow_up: &str,
    steered: uuid::Uuid,
) -> (String, eyre::Result<workforce_agent::runner::RunResult>) {
    use workforce_common::channels::desk::{DeskPlace, DESK_CHANNEL};
    use workforce_nats::chat::TurnControl;
    let desk = dm_turn(asked);
    let session = service
        .sessions
        .get_or_create(DESK_CHANNEL, owner, service.agent_id.as_str())
        .unwrap();
    let (_live, inbox) = service
        .begin_live_turn(
            &session.id,
            workforce_agent::tools::approval::SelfServe::Answerable,
            Some(&desk),
        )
        .await
        .expect("no turn running yet");
    let applied = service
        .live_turns
        .apply(
            &session.id,
            TurnControl::FollowUp {
                text: follow_up.into(),
                user: owner.into(),
                place: Some(DeskPlace::Dm),
                queue: false,
                id: Some(steered.to_string()),
            }
            .into(),
        )
        .await;
    assert!(applied.applied(), "the follow-up joins the live turn");
    let MessageProgress {
        mut progress_rx,
        handle,
        session_id,
        ..
    } = service
        .process_message_with_progress(
            DESK_CHANNEL,
            owner,
            None,
            question,
            None,
            Some(&session.id),
            None,
            None,
            None,
            None,
            Some(inbox),
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            Some(&desk),
        )
        .await
        .unwrap();
    while progress_rx.recv().await.is_some() {}
    (session_id, handle.await.unwrap())
}

/// The DM as Desk reads it back: each message's words, client id and
/// whether it is a failure.
fn read_back(
    service: &AgentService,
    owner: &str,
) -> Vec<(String, Option<String>, bool, Option<uuid::Uuid>)> {
    let (page, _) = crate::desk_conversations::read(
        service.sessions.as_ref(),
        &workforce_desk::Speaker::agent(service.agent_id.as_str(), "Agent"),
        owner,
        workforce_common::channels::desk::DESK_CHANNEL,
        workforce_desk::Cursor::Newest,
        50,
    )
    .unwrap();
    page.messages
        .into_iter()
        .map(|m| {
            let mark = m.mark.unwrap_or_default();
            (m.text, m.client_id, mark.failed, mark.in_reply_to)
        })
        .collect()
}

/// A turn that fails still keeps what it was asked, and what was sent into
/// it with Send now, each under the client's id: the DM reads them back
/// after a restart, then the failure, pointing at the question. The
/// failure is the conversation's alone: the model never replays it.
#[tokio::test(flavor = "multi_thread")]
async fn a_failed_turn_keeps_its_question_its_follow_up_and_where_it_failed() {
    use wiremock::matchers::method;
    use wiremock::{Mock, MockServer, ResponseTemplate};
    let tmp = tempfile::tempdir().unwrap();
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(400).set_body_string("the model refused the request"))
        .mount(&server)
        .await;
    let mut config = workforce_common::settings::WorkforceConfig::default();
    config.hub.data_dir = tmp.path().to_string_lossy().to_string();
    config.memory.db_path = tmp.path().join("memory.db").to_string_lossy().into_owned();
    config.llm.provider = "ollama".into();
    config.llm.api_key = Some(server.uri());
    config.llm.default_model = "mock".into();
    config.desk.enabled = true;
    let service = AgentService::from_config(config, None).await.unwrap();

    let (asked, steered) = (uuid::Uuid::new_v4(), uuid::Uuid::new_v4());
    let before = Utc::now();
    let (session_id, ran) = steered_turn(
        &service,
        "identity-1",
        "what was that research called again?",
        asked,
        "we talked about it previously",
        steered,
    )
    .await;
    let failed = ran.expect_err("the model refused, so the turn failed");

    assert_eq!(
        read_back(&service, "identity-1"),
        [
            (
                "what was that research called again?".to_string(),
                Some(asked.to_string()),
                false,
                None
            ),
            (
                "we talked about it previously".to_string(),
                Some(steered.to_string()),
                false,
                None
            ),
            (format!("Agent error: {failed}"), None, true, Some(asked)),
        ]
    );
    let session = service.sessions.get(&session_id).unwrap().unwrap();
    let said: Vec<&str> = session
        .messages
        .iter()
        .filter(|m| m.role != workforce_common::session::MessageRole::System)
        .map(|m| m.content.as_str())
        .collect();
    assert_eq!(
        said,
        [
            "what was that research called again?",
            "we talked about it previously"
        ],
        "the transcript the model replays holds no failure"
    );
    let follow_up = &session.messages[session.messages.len() - 1];
    assert!(
        follow_up.timestamp >= before && follow_up.timestamp < Utc::now(),
        "stamped when it arrived"
    );
}

/// A follow-up sent now into a turn that answers is kept under the
/// client's id, stamped when it arrived, between the question and the
/// answer, which names the question it answers.
#[tokio::test(flavor = "multi_thread")]
async fn a_follow_up_is_kept_under_its_client_id_before_the_answer() {
    let tmp = tempfile::tempdir().unwrap();
    let (service, _server) =
        crate::serve::testing::service_on_mock_llm_with(tmp.path(), |config| {
            config.desk.enabled = true;
        })
        .await;
    let (asked, steered) = (uuid::Uuid::new_v4(), uuid::Uuid::new_v4());
    let (_, ran) = steered_turn(
        &service,
        "identity-1",
        "what was that research called again?",
        asked,
        "we talked about it previously",
        steered,
    )
    .await;
    ran.expect("the turn answers");
    assert_eq!(
        read_back(&service, "identity-1"),
        [
            (
                "what was that research called again?".to_string(),
                Some(asked.to_string()),
                false,
                None
            ),
            (
                "we talked about it previously".to_string(),
                Some(steered.to_string()),
                false,
                None
            ),
            ("done".to_string(), None, false, Some(asked)),
        ]
    );
}

/// What a failure keeps in the archive names the tools the turn ran.
#[test]
fn a_failure_row_carries_the_tools_its_turn_ran() {
    let asked = uuid::Uuid::new_v4();
    let ran = [workforce_common::session::SessionMessage::tool(
        "web.search",
        &serde_json::json!({"q": "research"}),
        &serde_json::json!("results"),
        true,
        420,
        None,
    )];
    let row = crate::turn_loop::records::failure_row(
        &dm_turn(asked),
        "Agent error: llm first-token timeout",
        &ran,
    );
    assert_eq!(row.role, workforce_common::session::MessageRole::Assistant);
    let mark = row.desk.unwrap();
    assert!(mark.failed);
    assert_eq!(mark.in_reply_to, Some(asked));
    assert_eq!(
        mark.steps,
        [workforce_common::channels::desk::DeskStep::of(
            "web.search",
            true,
            420
        )]
    );
}

/// A turn stopped mid-call (its task aborted, as a stop does) still leaves
/// its question in the conversation, under the client's id: it went to the
/// archive before the run.
#[tokio::test(flavor = "multi_thread")]
async fn a_stopped_turn_keeps_its_question() {
    use wiremock::matchers::method;
    use wiremock::{Mock, MockServer, ResponseTemplate};
    let tmp = tempfile::tempdir().unwrap();
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(200).set_delay(std::time::Duration::from_secs(5)))
        .mount(&server)
        .await;
    let mut config = workforce_common::settings::WorkforceConfig::default();
    config.hub.data_dir = tmp.path().to_string_lossy().to_string();
    config.memory.db_path = tmp.path().join("memory.db").to_string_lossy().into_owned();
    config.llm.provider = "ollama".into();
    config.llm.api_key = Some(server.uri());
    config.llm.default_model = "mock".into();
    config.desk.enabled = true;
    let service = AgentService::from_config(config, None).await.unwrap();
    let asked = uuid::Uuid::new_v4();
    let desk = dm_turn(asked);
    let MessageProgress { handle, .. } = service
        .process_message_with_progress(
            workforce_common::channels::desk::DESK_CHANNEL,
            "identity-1",
            None,
            "what was that research called again?",
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            Some(&desk),
        )
        .await
        .unwrap();
    for _ in 0..200 {
        if !server
            .received_requests()
            .await
            .unwrap_or_default()
            .is_empty()
        {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(25)).await;
    }
    handle.abort();
    let _ = handle.await;
    assert_eq!(
        read_back(&service, "identity-1"),
        [(
            "what was that research called again?".to_string(),
            Some(asked.to_string()),
            false,
            None
        )]
    );
}

/// A room's failed turn keeps its question but leaves no failure row: what a
/// room's conversation keeps is posted to the room as the agent's reply, and
/// read by the other agents in it.
#[test]
fn a_failed_room_turn_adds_no_room_message() {
    use workforce_common::channels::desk::{DeskPlace, DeskSpeaker, DeskTurn, DeskTurnContext};
    use workforce_common::session::SessionStore;
    let dir = tempfile::tempdir().unwrap();
    let store: std::sync::Arc<dyn SessionStore> =
        std::sync::Arc::new(workforce_common::session::JsonlSessionStore::new(dir.path()).unwrap());
    let room = uuid::Uuid::new_v4();
    let channel = format!("desk:{room}");
    let asked = uuid::Uuid::new_v4();
    let turn = DeskTurn {
        place: DeskPlace::Room {
            room,
            name: "Atlas".into(),
            thread: None,
        },
        speaker: DeskSpeaker::Person,
        hop: 0,
        context: Vec::new(),
        message: Some(asked),
        spoken: false,
        voice: None,
        members: Vec::new(),
    };
    let desk = DeskTurnContext::new(turn, &channel, None, false, None, "reply".into());
    let mut session = store
        .get_or_create(&channel, "identity-1", "scout")
        .unwrap();
    session.add_user_message("what's open?", Some(&asked.to_string()));
    store.save(&session).unwrap();
    store
        .append_to_archive(&session.id, session.messages.last().unwrap())
        .unwrap();
    let upto = session.messages.len();
    crate::turn_loop::records::keep_failed_turn(
        &store,
        &mut session,
        crate::turn_loop::records::FailedTurn {
            archive_base: upto - 1,
            archived_upto: upto,
            heard: Vec::new(),
            trust_level: None,
            desk: Some(&desk),
            asker: None,
            error: "Agent error: boom",
            ran: &[],
        },
    );
    let page = store.read_archive_page(&session.id, None, 10).unwrap();
    let said: Vec<&str> = page.messages.iter().map(|m| m.content.as_str()).collect();
    assert_eq!(said, ["what's open?"]);
}

/// A turn whose transcript an urgent compaction shortens still archives its
/// question, under the client's id.
#[tokio::test(flavor = "multi_thread")]
async fn a_turn_that_compacts_still_archives_its_question() {
    use workforce_common::channels::desk::DESK_CHANNEL;
    let tmp = tempfile::tempdir().unwrap();
    let (service, _server) =
        crate::serve::testing::service_on_mock_llm_with(tmp.path(), |config| {
            config.desk.enabled = true;
        })
        .await;
    let mut tight = service.current_compaction_config();
    tight.max_context_tokens = 60_000;
    tight.keep_recent_messages = 4;
    tight.min_messages_to_compact = 2;
    *service.active_compaction.write() = Some(tight);
    let mut session = service
        .sessions
        .get_or_create(DESK_CHANNEL, "identity-1", service.agent_id.as_str())
        .unwrap();
    let long = "the quarterly figures again, line by line. ".repeat(400);
    for n in 0..40 {
        session.add_user_message(&format!("{n}: {long}"), None);
        session.add_assistant_message(&format!("noted {n}"), 5);
    }
    service.sessions.save(&session).unwrap();
    let before = session.messages.len();

    let asked = uuid::Uuid::new_v4();
    let desk = dm_turn(asked);
    let MessageProgress {
        mut progress_rx,
        handle,
        session_id,
        ..
    } = service
        .process_message_with_progress(
            DESK_CHANNEL,
            "identity-1",
            None,
            "and the totals?",
            None,
            Some(&session.id),
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            Some(&desk),
        )
        .await
        .unwrap();
    while progress_rx.recv().await.is_some() {}
    handle.await.unwrap().unwrap();
    let after = service.sessions.get(&session_id).unwrap().unwrap();
    assert!(after.messages.len() < before, "the turn compacted");
    let read = read_back(&service, "identity-1");
    assert!(
        read.iter().any(|(text, id, _, _)| text == "and the totals?"
            && id.as_deref() == Some(asked.to_string().as_str())),
        "{read:?}"
    );
}

/// A follow-up sent now into a turn that is then stopped is still the
/// person's: kept, as said, once the turn's task is gone.
#[tokio::test(flavor = "multi_thread")]
async fn a_stopped_turn_keeps_what_was_sent_into_it() {
    use wiremock::matchers::method;
    use wiremock::{Mock, MockServer, ResponseTemplate};
    use workforce_common::channels::desk::{DeskPlace, DESK_CHANNEL};
    use workforce_nats::chat::TurnControl;
    let tmp = tempfile::tempdir().unwrap();
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(200).set_delay(std::time::Duration::from_secs(5)))
        .mount(&server)
        .await;
    let mut config = workforce_common::settings::WorkforceConfig::default();
    config.hub.data_dir = tmp.path().to_string_lossy().to_string();
    config.memory.db_path = tmp.path().join("memory.db").to_string_lossy().into_owned();
    config.llm.provider = "ollama".into();
    config.llm.api_key = Some(server.uri());
    config.llm.default_model = "mock".into();
    config.desk.enabled = true;
    let service = AgentService::from_config(config, None).await.unwrap();
    let (asked, steered) = (uuid::Uuid::new_v4(), uuid::Uuid::new_v4());
    let desk = dm_turn(asked);
    let session = service
        .sessions
        .get_or_create(DESK_CHANNEL, "identity-1", service.agent_id.as_str())
        .unwrap();
    let (_live, inbox) = service
        .begin_live_turn(
            &session.id,
            workforce_agent::tools::approval::SelfServe::Answerable,
            Some(&desk),
        )
        .await
        .unwrap();
    let MessageProgress { handle, .. } = service
        .process_message_with_progress(
            DESK_CHANNEL,
            "identity-1",
            None,
            "what was that research called again?",
            None,
            Some(&session.id),
            None,
            None,
            None,
            None,
            Some(inbox),
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            Some(&desk),
        )
        .await
        .unwrap();
    for _ in 0..200 {
        if !server
            .received_requests()
            .await
            .unwrap_or_default()
            .is_empty()
        {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(25)).await;
    }
    assert!(service
        .live_turns
        .apply(
            &session.id,
            TurnControl::FollowUp {
                text: "we talked about it previously".into(),
                user: "identity-1".into(),
                place: Some(DeskPlace::Dm),
                queue: false,
                id: Some(steered.to_string()),
            }
            .into(),
        )
        .await
        .applied());
    handle.abort();
    let _ = handle.await;
    let mut read = Vec::new();
    for _ in 0..100 {
        read = read_back(&service, "identity-1");
        if read.len() == 2 {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(25)).await;
    }
    assert_eq!(
        read.iter()
            .map(|(t, id, _, _)| (t.as_str(), id.clone()))
            .collect::<Vec<_>>(),
        [
            (
                "what was that research called again?",
                Some(asked.to_string())
            ),
            ("we talked about it previously", Some(steered.to_string())),
        ]
    );
}

/// What the run read of the follow-ups heard is kept as answered; what it
/// never read is apart, to ask again.
#[test]
fn follow_ups_split_by_what_the_run_read() {
    use crate::hub_core::live_turns::Heard;
    use workforce_agent::runner::InjectedUserMessage;
    let heard = |text: &str, id: Option<&str>| Heard {
        text: text.into(),
        user: "u1".into(),
        external_id: id.map(str::to_string),
        at: Utc::now(),
    };
    let (read, unread) = crate::turn_loop::records::read_by_run(
        vec![
            heard("same words", Some("a")),
            heard("same words", Some("b")),
            heard("no id", None),
        ],
        &[InjectedUserMessage {
            user: "u1".into(),
            text: "same words".into(),
            external_id: Some("b".into()),
        }],
    );
    assert_eq!(
        read.iter()
            .map(|h| h.external_id.as_deref())
            .collect::<Vec<_>>(),
        [Some("b")]
    );
    assert_eq!(
        unread
            .iter()
            .map(|h| h.external_id.as_deref())
            .collect::<Vec<_>>(),
        [Some("a"), None]
    );
}

fn heard_now(text: &str, id: Option<&str>) -> crate::hub_core::live_turns::Heard {
    crate::hub_core::live_turns::Heard {
        text: text.into(),
        user: "identity-1".into(),
        external_id: id.map(str::to_string),
        at: Utc::now(),
    }
}

/// A follow-up a DM turn's run never read is answered by the hub as the
/// conversation's next turn, with no client there: its message is kept
/// once, under the client's id, and the answer names it.
#[tokio::test(flavor = "multi_thread")]
async fn an_unread_dm_follow_up_runs_as_the_next_turn_with_no_client() {
    use workforce_common::channels::desk::DESK_CHANNEL;
    use workforce_common::session::MessageRole;
    let tmp = tempfile::tempdir().unwrap();
    let (service, _server) =
        crate::serve::testing::service_on_mock_llm_with(tmp.path(), |config| {
            config.desk.enabled = true;
        })
        .await;
    let (asked, steered) = (uuid::Uuid::new_v4(), uuid::Uuid::new_v4());
    let mut session = service
        .sessions
        .get_or_create(DESK_CHANNEL, "identity-1", service.agent_id.as_str())
        .unwrap();
    session.add_user_message("what was that research called?", Some(&asked.to_string()));
    session.add_assistant_message("NCM", 3);
    let unread = crate::turn_loop::next_turn::unread_plan(
        Some(&dm_turn(asked)),
        vec![heard_now("and who wrote it?", Some(&steered.to_string()))],
    );
    crate::turn_loop::records::keep_heard(
        &mut session,
        unread.after.clone(),
        None,
        Some(&dm_turn(asked)),
    );
    service.sessions.save(&session).unwrap();
    for msg in &session.messages {
        service
            .sessions
            .append_to_archive(&session.id, msg)
            .unwrap();
    }
    let (desk, prompt) =
        crate::turn_loop::next_turn::next_of(&dm_turn(asked), &unread.after).expect("a next turn");
    assert_eq!(desk.turn.message, Some(steered));
    crate::turn_loop::next_turn::answer_left(
        std::sync::Arc::clone(&service),
        crate::turn_loop::next_turn::NextTurn {
            session_id: session.id.clone(),
            channel: DESK_CHANNEL.into(),
            user: "identity-1".into(),
            run_as: "identity-1".into(),
            trust_level: None,
            desk,
            prompt,
        },
    )
    .await;
    let after = service.sessions.get(&session.id).unwrap().unwrap();
    let said: Vec<(MessageRole, &str)> = after
        .messages
        .iter()
        .filter(|m| matches!(m.role, MessageRole::User | MessageRole::Assistant))
        .map(|m| (m.role.clone(), m.content.as_str()))
        .collect();
    assert_eq!(
        said,
        [
            (MessageRole::User, "what was that research called?"),
            (MessageRole::Assistant, "NCM"),
            (MessageRole::User, "and who wrote it?"),
            (MessageRole::Assistant, "done"),
        ],
        "the follow-up is answered where it is, once"
    );
    let read = read_back(&service, "identity-1");
    assert_eq!(
        read.last().unwrap(),
        &("done".to_string(), None, false, Some(steered))
    );
    assert_eq!(read[2].1.as_deref(), Some(steered.to_string().as_str()));
}

/// A Desk that names no id for its follow-ups (an older one) still has an
/// unread one answered as the next turn, under an id the hub gives it.
#[test]
fn an_older_desk_s_unread_follow_up_gets_an_id_for_its_next_turn() {
    let asked = uuid::Uuid::new_v4();
    let unread = crate::turn_loop::next_turn::unread_plan(
        Some(&dm_turn(asked)),
        vec![heard_now("and who wrote it?", None)],
    );
    assert!(unread.now.is_empty());
    let id = unread.after[0]
        .external_id
        .clone()
        .expect("an id of the hub's");
    let (desk, prompt) =
        crate::turn_loop::next_turn::next_of(&dm_turn(asked), &unread.after).unwrap();
    assert_eq!(desk.turn.message.map(|m| m.to_string()), Some(id));
    assert_eq!(
        (prompt.as_str(), desk.turn_ref.as_deref()),
        ("and who wrote it?", None)
    );
}

/// In a room an unread follow-up is saved as it always was, not run.
#[test]
fn a_room_s_unread_follow_up_is_saved_as_before() {
    use workforce_common::channels::desk::{DeskPlace, DeskSpeaker, DeskTurn, DeskTurnContext};
    let room = uuid::Uuid::new_v4();
    let turn = DeskTurn {
        place: DeskPlace::Room {
            room,
            name: "Atlas".into(),
            thread: None,
        },
        speaker: DeskSpeaker::Person,
        hop: 0,
        context: Vec::new(),
        message: Some(uuid::Uuid::new_v4()),
        spoken: false,
        voice: None,
        members: Vec::new(),
    };
    let desk = DeskTurnContext::new(turn, &format!("desk:{room}"), None, false, None, "r".into());
    let unread = crate::turn_loop::next_turn::unread_plan(
        Some(&desk),
        vec![heard_now("and the date?", Some("c-1"))],
    );
    assert_eq!(unread.now.len(), 1);
    assert_eq!(unread.now[0].external_id.as_deref(), Some("c-1"));
    assert!(unread.after.is_empty());
    assert!(crate::turn_loop::next_turn::next_of(&desk, &unread.after).is_none());
}

/// The session as a turn that never read follow-up `steered` left it: the
/// question, its answer, then the follow-up kept after it.
fn left_with_follow_up(
    service: &AgentService,
    asked: uuid::Uuid,
    steered: uuid::Uuid,
) -> workforce_common::session::Session {
    let mut session = service
        .sessions
        .get_or_create(
            workforce_common::channels::desk::DESK_CHANNEL,
            "identity-1",
            service.agent_id.as_str(),
        )
        .unwrap();
    session.add_user_message("what was that research called?", Some(&asked.to_string()));
    session.add_assistant_message("NCM", 3);
    let unread = crate::turn_loop::next_turn::unread_plan(
        Some(&dm_turn(asked)),
        vec![heard_now("and who wrote it?", Some(&steered.to_string()))],
    );
    crate::turn_loop::records::keep_heard(&mut session, unread.after, None, Some(&dm_turn(asked)));
    service.sessions.save(&session).unwrap();
    session
}

fn next_for(
    session_id: &str,
    asked: uuid::Uuid,
    steered: uuid::Uuid,
) -> crate::turn_loop::next_turn::NextTurn {
    let (desk, prompt) = crate::turn_loop::next_turn::next_of(
        &dm_turn(asked),
        &[heard_now("and who wrote it?", Some(&steered.to_string()))],
    )
    .unwrap();
    crate::turn_loop::next_turn::NextTurn {
        session_id: session_id.to_string(),
        channel: workforce_common::channels::desk::DESK_CHANNEL.into(),
        user: "identity-1".into(),
        run_as: "identity-1".into(),
        trust_level: None,
        desk,
        prompt,
    }
}

/// The person asks again before the hub's next turn starts, and that turn
/// answers both: the hub's then finds nothing waiting and runs nothing, so
/// the follow-up is neither saved twice nor answered twice.
#[tokio::test(flavor = "multi_thread")]
async fn a_follow_up_answered_meanwhile_is_not_run_again() {
    let tmp = tempfile::tempdir().unwrap();
    let (service, server) = crate::serve::testing::service_on_mock_llm_with(tmp.path(), |config| {
        config.desk.enabled = true;
    })
    .await;
    let (asked, steered) = (uuid::Uuid::new_v4(), uuid::Uuid::new_v4());
    let mut session = left_with_follow_up(&service, asked, steered);
    session.add_user_message(
        "hello? anyone there?",
        Some(&uuid::Uuid::new_v4().to_string()),
    );
    session.add_assistant_message("Yes: Ada wrote it.", 4);
    service.sessions.save(&session).unwrap();
    let before = session.messages.len();
    let asked_before = completions(&server).await;

    crate::turn_loop::next_turn::answer_left(
        std::sync::Arc::clone(&service),
        next_for(&session.id, asked, steered),
    )
    .await;

    let after = service.sessions.get(&session.id).unwrap().unwrap();
    assert_eq!(after.messages.len(), before, "nothing was added");
    let steered = steered.to_string();
    assert_eq!(
        after
            .messages
            .iter()
            .filter(|m| m.external_id.as_deref() == Some(steered.as_str()))
            .count(),
        1
    );
    assert_eq!(
        completions(&server).await,
        asked_before,
        "no model was asked"
    );
}

/// A rollover between the turns leaves the follow-up in the sealed
/// predecessor: the hub's next turn runs nothing in the successor.
#[tokio::test(flavor = "multi_thread")]
async fn a_follow_up_left_behind_a_rollover_is_not_run() {
    let tmp = tempfile::tempdir().unwrap();
    let (service, server) = crate::serve::testing::service_on_mock_llm_with(tmp.path(), |config| {
        config.desk.enabled = true;
    })
    .await;
    let (asked, steered) = (uuid::Uuid::new_v4(), uuid::Uuid::new_v4());
    let mut session = left_with_follow_up(&service, asked, steered);
    let successor =
        workforce_agent::rollover::seal_and_mint(service.sessions.as_ref(), &mut session, 0)
            .unwrap();
    let asked_before = completions(&server).await;

    crate::turn_loop::next_turn::answer_left(
        std::sync::Arc::clone(&service),
        next_for(&session.id, asked, steered),
    )
    .await;

    let next = service.sessions.get(&successor.id).unwrap().unwrap();
    assert!(
        next.messages
            .iter()
            .all(|m| m.role == workforce_common::session::MessageRole::System
                || m.content
                    .starts_with(workforce_agent::rollover::CARRYOVER_PREFIX)),
        "the successor holds nothing of the follow-up: {:?}",
        next.messages.iter().map(|m| &m.content).collect::<Vec<_>>()
    );
    assert_eq!(
        completions(&server).await,
        asked_before,
        "no model was asked"
    );
}

/// How many times the model at `server` was asked for an answer.
async fn completions(server: &wiremock::MockServer) -> usize {
    server
        .received_requests()
        .await
        .unwrap_or_default()
        .iter()
        .filter(|r| r.url.path() == "/v1/chat/completions")
        .count()
}

#[test]
fn the_folders_it_works_in_are_named_with_the_first_as_where_coding_starts() {
    assert_eq!(super::works_in_text(&[], true), None);
    let folders = [
        std::path::PathBuf::from("/Users/sarah/dev/acme-web"),
        std::path::PathBuf::from("/Users/sarah/dev/acme-api"),
    ];
    let line = super::works_in_text(&folders, true).unwrap();
    assert!(
        line.starts_with("- Works in: `/Users/sarah/dev/acme-web`, `/Users/sarah/dev/acme-api`")
    );
    assert!(
        line.contains("`desk.start` sessions in `/Users/sarah/dev/acme-web`"),
        "{line}"
    );
    assert!(
        line.contains("`repo` = `/Users/sarah/dev/acme-web`"),
        "{line}"
    );
    assert!(line.contains("deleting there asks"), "{line}");
    // Below Full access it never sends the agent to run what is refused.
    let lower = super::works_in_text(&folders, false).unwrap();
    assert!(
        !lower.contains("exec.run") && !lower.contains("desk.start"),
        "{lower}"
    );
    assert!(lower.contains("need Full access"), "{lower}");

fn invocation(
    tool: &str,
    input: serde_json::Value,
    output: serde_json::Value,
) -> workforce_agent::runner::ToolInvocation {
    workforce_agent::runner::ToolInvocation {
        tool_name: tool.to_string(),
        input,
        output,
        success: true,
        policy_decision: "allow".to_string(),
        duration_ms: 4100,
        denial_reason: None,
        error: None,
    }
}

/// An advisor call's step keeps what it was asked, what it said and its
/// model, its secrets hidden; other tools' steps keep only how they went.
#[test]
fn an_advisor_step_keeps_its_question_and_answer_with_secrets_hidden() {
    use workforce_common::channels::desk::{ADVICE_ANSWER_CHARS, ADVICE_MESSAGE_CHARS};
    let key = format!("ghp_{}", "a1B2c3D4e5".repeat(4));
    let asked = invocation(
        crate::advisor::AdvisorTool::NAME,
        serde_json::json!({"question": "Is the retry safe to merge?"}),
        serde_json::json!({
            "advice": format!("**No.** The token `{key}` is in the log; rotate it first."),
            "source": "chatgpt",
            "model": "gpt-6.1-sol",
            "duration_ms": 4000,
        }),
    );
    let searched = invocation(
        "web.search",
        serde_json::json!({"query": "x"}),
        serde_json::json!({"results": []}),
    );
    let steps = crate::turn_loop::records::steps_of(&[searched, asked]);
    assert_eq!(steps[0].advice, None);
    let advice = steps[1].advice.clone().unwrap();
    assert_eq!(advice.question, "Is the retry safe to merge?");
    assert!(
        advice.answer.starts_with("**No.** The token"),
        "{}",
        advice.answer
    );
    assert!(!advice.answer.contains(&key) && !advice.answer.contains("a1B2c3D4e5"));
    assert!(advice.answer.contains("REDACTED"), "{}", advice.answer);
    assert_eq!(advice.model.as_deref(), Some("gpt-6.1-sol"));
    assert!(!advice.unavailable && !advice.clipped);

    // No advice: why, as its answer.
    let refused = invocation(
        crate::advisor::AdvisorTool::NAME,
        serde_json::json!({}),
        serde_json::json!({"advisor_unavailable": "The advisor is turned off for this agent; answer without it."}),
    );
    let advice = crate::turn_loop::records::steps_of(&[refused])[0]
        .advice
        .clone()
        .unwrap();
    assert!(advice.unavailable && advice.question.is_empty());
    assert!(advice.answer.starts_with("The advisor is turned off"));

    // A long answer is clipped to a step's most, and a message keeps no
    // more than its most across its steps.
    let long = |n: usize| {
        invocation(
            crate::advisor::AdvisorTool::NAME,
            serde_json::json!({"question": format!("round {n}")}),
            serde_json::json!({"advice": "é".repeat(ADVICE_ANSWER_CHARS * 2), "model": "opus"}),
        )
    };
    let many: Vec<_> = (0..(ADVICE_MESSAGE_CHARS / ADVICE_ANSWER_CHARS) + 2)
        .map(long)
        .collect();
    let steps = crate::turn_loop::records::steps_of(&many);
    let first = steps[0].advice.as_ref().unwrap();
    assert!(first.clipped && first.answer.chars().count() == ADVICE_ANSWER_CHARS);
    let kept: usize = steps
        .iter()
        .filter_map(|s| s.advice.as_ref())
        .filter(|a| a.clipped)
        .map(|a| a.answer.chars().count())
        .sum();
    assert!(kept <= ADVICE_MESSAGE_CHARS, "{kept}");
    let last = steps.last().unwrap().advice.as_ref().unwrap();
    assert!(
        last.answer.contains("the agent has it whole"),
        "{}",
        last.answer
    );
    assert_eq!(last.question, format!("round {}", many.len() - 1));
    assert!(steps.iter().all(|s| s.advice_fits()));
}
