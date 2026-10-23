//! Recent-sessions digest: at 1:1-DM session open, a compact block telling the
//! agent what this same user discussed with it in OTHER recent sessions.
//!
//! Privacy contract (mirrors DW-515 / `select_visible_other_sessions`): the
//! target must be a 1:1 DM owned by the user; sources must share that exact
//! owner and agent. Shared rooms are keyed `shared:{thread}` and can neither
//! be a target nor appear in an owner listing, so DM content can never leak
//! into a shared room and shared-room content never needs to (equal-or-wider
//! visibility is vacuously satisfied: source owner == target audience).

use std::collections::HashSet;

use chrono::{DateTime, Utc};
use workforce_agent::rollover::{has_real_dialogue, CARRYOVER_PREFIX};
use workforce_common::secret_marker::redact_known_secrets;
use workforce_common::session::{MessageRole, Session, SessionContextType, SessionStore};
use workforce_common::settings::SessionDigestConfig;
use workforce_common::text::truncate_with_ellipsis;

/// Over-pull from the owner listing; the eligibility predicate filters down.
pub const DIGEST_FETCH_LIMIT: usize = 12;

/// Rollover-ancestor hops walked when deduplicating the target's own
/// carry-over chain out of the digest.
const MAX_PREDECESSOR_HOPS: usize = 5;

const TOPIC_CLIP_CHARS: usize = 100;
const EXCHANGE_CLIP_CHARS: usize = 110;

const DIGEST_HEADER: &str = "[RECENT SESSIONS — this user's other recent conversations with \
     you. Background context only, NOT part of this conversation; reference naturally (\"as we \
     discussed earlier\") when relevant, never dump this list.]";

/// Whether `target` may receive a digest at all (the DM-only target gate).
/// Session-open only: a session that already holds genuine dialogue is never
/// re-digested.
pub fn digest_target_eligible(target: &Session, user: &str) -> bool {
    target.context_type == Some(SessionContextType::Dm)
        && !target.is_shared_conversation() // sentinel owners fail closed
        && target.owner == user // audience == exactly this user
        && !has_real_dialogue(target)
}

/// Source eligibility for one candidate (already owner-listed). Sources must
/// carry the exact single-user audience of the target — same owner, same
/// agent, DM-classified — and be recent, substantive, non-voice,
/// non-internal, not an eval's, and not already represented by the target's
/// own carry-over chain or by a successor candidate's carry-over.
pub fn digest_source_eligible(
    s: &Session,
    target: &Session,
    agent_id: &str,
    cutoff: DateTime<Utc>,
    predecessor_ids: &HashSet<String>,
    successor_prevs: &HashSet<String>,
) -> bool {
    s.id != target.id
        && s.owner == target.owner // same single-user audience
        && s.agent_id == agent_id // same agent (defence for store default impls)
        && !s.is_shared_conversation() // fail closed on sentinel owners
        && s.context_type == Some(SessionContextType::Dm) // legacy None fails closed
        && s.updated_at > cutoff
        && s.message_count() > 0
        && !s.is_voice() // substance reaches prompts via daily notes / resume context
        && !crate::turn_extraction::leaves_no_memory(&s.channel)
        && !predecessor_ids.contains(&s.id) // already in the target's carry-over
        && !successor_prevs.contains(&s.id) // its successor candidate holds the carry-over
}

/// Channel-type label — never raw ids ("where" attribution).
pub fn surface_label(s: &Session) -> &'static str {
    if s.is_voice() {
        return "voice chat";
    }
    match s.channel.as_str() {
        "ios" => "iOS chat",
        "web" => "web chat",
        "terminal" | "cli" => "terminal chat",
        c if workforce_common::channels::is_platform(c, "slack") => "Slack DM",
        _ => "direct chat",
    }
}

/// "5m ago" / "3h ago" — mirrors `build_cross_session_summary`.
pub fn age_str(updated_at: DateTime<Utc>, now: DateTime<Utc>) -> String {
    let age = now - updated_at;
    if age.num_minutes() < 60 {
        format!("{}m ago", age.num_minutes().max(0))
    } else {
        format!("{}h ago", age.num_hours())
    }
}

/// A User message that is genuine dialogue, not an inherited carry-over.
fn is_real_user_dialogue(m: &workforce_common::session::SessionMessage) -> bool {
    m.role == MessageRole::User && !m.content.starts_with(CARRYOVER_PREFIX)
}

fn flatten_clip(text: &str, max: usize) -> String {
    truncate_with_ellipsis(&text.replace('\n', " "), max)
}

/// First sentence of `text` (falling back to its first non-empty line),
/// flattened and clipped.
fn first_sentence(text: &str) -> Option<String> {
    let line = text.lines().map(str::trim).find(|l| !l.is_empty())?;
    let sentence = line
        .char_indices()
        .find(|&(i, c)| {
            matches!(c, '.' | '!' | '?')
                && line[i + c.len_utf8()..]
                    .chars()
                    .next()
                    .is_none_or(char::is_whitespace)
        })
        .map(|(i, c)| &line[..i + c.len_utf8()])
        .unwrap_or(line);
    Some(truncate_with_ellipsis(sentence, TOPIC_CLIP_CHARS))
}

/// Topic line for one source session, first match wins:
/// 1. a human label (`label` set and != the machine `{channel}:{owner}` key)
/// 2. the newest compaction summary's first sentence
/// 3. the newest carry-over's first line after the prefix
/// 4. the earliest real-dialogue User message
///
/// `None` when nothing usable — the entry is then skipped. Secret-redacted
/// like `last_exchange`: a label or LLM-written compaction summary can carry
/// an echoed credential in cleartext.
pub fn topic_line(s: &Session) -> Option<String> {
    raw_topic_line(s).map(|t| redact_known_secrets(&s.channel, &t))
}

fn raw_topic_line(s: &Session) -> Option<String> {
    let machine_label = format!("{}:{}", s.channel, s.owner);
    if let Some(label) = s
        .label
        .as_deref()
        .map(str::trim)
        .filter(|l| !l.is_empty() && *l != machine_label)
    {
        return Some(flatten_clip(label, TOPIC_CLIP_CHARS));
    }
    if let Some(summary) = s
        .messages
        .iter()
        .rev()
        .find(|m| m.is_compaction_summary())
        .and_then(|m| first_sentence(m.content.split_once('\n').map_or("", |(_, body)| body)))
    {
        return Some(summary);
    }
    if let Some(carryover) = s
        .messages
        .iter()
        .rev()
        .find(|m| m.role == MessageRole::User && m.content.starts_with(CARRYOVER_PREFIX))
        .and_then(|m| {
            let body = &m.content[CARRYOVER_PREFIX.len()..];
            let line = body.lines().map(str::trim).find(|l| !l.is_empty())?;
            Some(truncate_with_ellipsis(line, TOPIC_CLIP_CHARS))
        })
    {
        return Some(carryover);
    }
    s.messages
        .iter()
        .find(|m| is_real_user_dialogue(m))
        .map(|m| flatten_clip(&m.content, TOPIC_CLIP_CHARS))
        .filter(|t| !t.is_empty())
}

/// Last exchange: the last real-dialogue User message plus the Assistant
/// reply that followed it (omitted when the turn got no reply), flattened,
/// clipped, and secret-redacted — the same hardening as thread-history
/// injection.
pub fn last_exchange(s: &Session) -> Option<String> {
    let idx = s.messages.iter().rposition(is_real_user_dialogue)?;
    let mut line = format!(
        "user: \"{}\"",
        flatten_clip(&s.messages[idx].content, EXCHANGE_CLIP_CHARS)
    );
    if let Some(reply) = s.messages[idx + 1..]
        .iter()
        .find(|m| m.role == MessageRole::Assistant)
    {
        line.push_str(&format!(
            " / you: \"{}\"",
            flatten_clip(&reply.content, EXCHANGE_CLIP_CHARS)
        ));
    }
    Some(redact_known_secrets(&s.channel, &line))
}

/// One rendered digest entry, newest-first in the final block.
pub struct DigestEntry {
    pub age: String,
    pub surface: &'static str,
    pub topic: String,
    pub exchange: Option<String>,
}

impl DigestEntry {
    fn from_session(s: &Session, now: DateTime<Utc>) -> Option<Self> {
        Some(Self {
            age: age_str(s.updated_at, now),
            surface: surface_label(s),
            topic: topic_line(s)?,
            exchange: last_exchange(s),
        })
    }

    fn render(&self) -> String {
        let mut line = format!("- {} ({}) — {}", self.age, self.surface, self.topic);
        if let Some(exchange) = &self.exchange {
            line.push_str(". Last exchange: ");
            line.push_str(exchange);
        }
        line
    }
}

/// Assemble the digest block, enforcing the total char budget: entries are
/// added newest-first and dropped whole once the header plus entries would
/// exceed `max_chars`. `None` when no entry fits (no empty header).
pub fn render_digest(entries: &[DigestEntry], max_chars: usize) -> Option<String> {
    let mut out = String::from(DIGEST_HEADER);
    let mut rendered = 0usize;
    for entry in entries {
        let line = entry.render();
        if out.len() + 1 + line.len() > max_chars {
            break;
        }
        out.push('\n');
        out.push_str(&line);
        rendered += 1;
    }
    (rendered > 0).then_some(out)
}

/// The target's own rollover-ancestor ids (`prev_session_id` chain, bounded)
/// — their content already reaches the target through its carry-over.
fn predecessor_chain(store: &dyn SessionStore, target: &Session) -> HashSet<String> {
    let mut ids = HashSet::new();
    let mut next = target.prev_session_id().map(str::to_string);
    for _ in 0..MAX_PREDECESSOR_HOPS {
        let Some(id) = next else { break };
        next = store
            .get(&id)
            .ok()
            .flatten()
            .and_then(|s| s.prev_session_id().map(str::to_string));
        ids.insert(id);
    }
    ids
}

/// Build the digest for a 1:1-DM `target` session opening with no dialogue
/// yet. Never fails the turn: store errors log debug and yield `None`.
///
/// Callers must additionally gate on the turn shape (no automated sources,
/// private delivery) — see `AgentService::recent_sessions_digest_block`.
pub fn build_digest(
    store: &dyn SessionStore,
    cfg: &SessionDigestConfig,
    target: &Session,
    user: &str,
    agent_id: &str,
    now: DateTime<Utc>,
) -> Option<String> {
    if !cfg.enabled || cfg.max_sessions == 0 || !digest_target_eligible(target, user) {
        return None;
    }
    let candidates = match store.list_for_owner(&target.owner, DIGEST_FETCH_LIMIT) {
        Ok(candidates) => candidates,
        Err(e) => {
            tracing::debug!(
                error = %e,
                session_id = %target.id,
                "Recent-sessions digest: owner listing failed; opening without digest"
            );
            return None;
        }
    };
    let window_secs = i64::try_from(cfg.window_hours.saturating_mul(3600)).unwrap_or(i64::MAX);
    let cutoff = chrono::Duration::try_seconds(window_secs)
        .and_then(|window| now.checked_sub_signed(window))
        .unwrap_or(DateTime::<Utc>::MIN_UTC);
    let predecessor_ids = predecessor_chain(store, target);
    let successor_prevs: HashSet<String> = candidates
        .iter()
        .filter_map(|c| c.prev_session_id().map(str::to_string))
        .collect();

    // Candidates arrive newest-first; shells carry no messages, so each
    // survivor is hydrated (a deleted-in-between session just drops out).
    let entries: Vec<DigestEntry> = candidates
        .iter()
        .filter(|s| {
            digest_source_eligible(
                s,
                target,
                agent_id,
                cutoff,
                &predecessor_ids,
                &successor_prevs,
            )
        })
        .take(cfg.max_sessions)
        .filter_map(|shell| store.get(&shell.id).ok().flatten())
        .filter_map(|s| DigestEntry::from_session(&s, now))
        .collect();

    let block = render_digest(&entries, cfg.max_chars)?;
    tracing::info!(
        session_id = %target.id,
        channel = %target.channel,
        sessions = entries.len(),
        "Recent-sessions digest injected"
    );
    Some(block)
}

#[cfg(test)]
mod tests {
    use super::*;
    use workforce_common::session::InMemorySessionStore;

    const AGENT: &str = "agent";

    fn dm_session(channel: &str, owner: &str) -> Session {
        let mut s = Session::new(channel, owner, AGENT);
        s.context_type = Some(SessionContextType::Dm);
        s
    }

    /// An eligible digest source with one real exchange, `age` old.
    fn source(channel: &str, owner: &str, first_msg: &str, age: chrono::Duration) -> Session {
        let mut s = dm_session(channel, owner);
        s.add_user_message(first_msg, None);
        s.add_assistant_message(&format!("reply to {first_msg}"), 4);
        s.updated_at = Utc::now() - age;
        s
    }

    fn minutes(n: i64) -> chrono::Duration {
        chrono::Duration::minutes(n)
    }

    fn eligible(s: &Session, target: &Session, cutoff: DateTime<Utc>) -> bool {
        digest_source_eligible(s, target, AGENT, cutoff, &HashSet::new(), &HashSet::new())
    }

    fn default_cutoff() -> DateTime<Utc> {
        Utc::now() - chrono::Duration::hours(12)
    }

    #[test]
    fn eval_sessions_are_never_a_digest_source() {
        let target = dm_session("web", "alice");
        let asked = source("eval:run-1:q1", "alice", "how many orders?", minutes(5));
        assert!(!eligible(&asked, &target, default_cutoff()));
        let chatted = source("rest", "alice", "how many orders?", minutes(5));
        assert!(eligible(&chatted, &target, default_cutoff()));
    }

    // ── Target gate ──────────────────────────────────────────────────────

    #[test]
    fn target_dm_owned_by_user_is_eligible() {
        let target = dm_session("slack:D1", "alice");
        assert!(digest_target_eligible(&target, "alice"));
        // A different user (run_as loading someone else's view) fails closed.
        assert!(!digest_target_eligible(&target, "bob"));
    }

    #[test]
    fn target_shared_owner_rejected() {
        // Even with context_type forced to Dm and the sentinel passed as the
        // "user", the shared: sentinel wins — fail closed.
        let target = dm_session("slack:C7", "shared:1700.1");
        assert!(!digest_target_eligible(&target, "shared:1700.1"));
    }

    #[test]
    fn target_group_thread_or_unclassified_rejected() {
        for context_type in [
            Some(SessionContextType::Group),
            Some(SessionContextType::Thread),
            None,
        ] {
            let mut target = Session::new("slack:C1", "alice", AGENT);
            target.context_type = context_type.clone();
            assert!(
                !digest_target_eligible(&target, "alice"),
                "context {context_type:?} must be rejected"
            );
        }
    }

    #[test]
    fn target_with_real_dialogue_rejected() {
        let mut target = dm_session("slack:D1", "alice");
        target.add_user_message("hello", None);
        assert!(!digest_target_eligible(&target, "alice"));

        // A stale-rollover successor holding only its carry-over is still a
        // session open — eligible.
        let mut successor = dm_session("slack:D1", "alice");
        successor.add_user_message(&format!("{CARRYOVER_PREFIX}\nOld thread summary"), None);
        assert!(digest_target_eligible(&successor, "alice"));
    }

    // ── Source gate ──────────────────────────────────────────────────────

    #[test]
    fn source_other_owner_rejected() {
        let target = dm_session("slack:D1", "alice");
        assert!(!eligible(
            &source("slack:D2", "bob", "hi", minutes(5)),
            &target,
            default_cutoff()
        ));
    }

    #[test]
    fn source_other_agent_rejected() {
        let target = dm_session("slack:D1", "alice");
        let mut s = source("slack:D2", "alice", "hi", minutes(5));
        s.agent_id = "other-agent".into();
        assert!(!eligible(&s, &target, default_cutoff()));
    }

    #[test]
    fn source_current_session_rejected() {
        let target = dm_session("slack:D1", "alice");
        let mut s = source("slack:D1", "alice", "hi", minutes(5));
        s.id = target.id.clone();
        assert!(!eligible(&s, &target, default_cutoff()));
    }

    #[test]
    fn source_shared_group_or_unclassified_rejected() {
        let target = dm_session("slack:D1", "alice");
        let cutoff = default_cutoff();

        let mut shared = source("slack:C7", "shared:1700.1", "hi", minutes(5));
        shared.context_type = Some(SessionContextType::Dm);
        assert!(!eligible(&shared, &target, cutoff), "sentinel owner");

        let mut group = source("slack:C7", "alice", "hi", minutes(5));
        group.context_type = Some(SessionContextType::Group);
        assert!(!eligible(&group, &target, cutoff), "group context");

        let mut legacy = source("slack:D2", "alice", "hi", minutes(5));
        legacy.context_type = None;
        assert!(
            !eligible(&legacy, &target, cutoff),
            "legacy None fails closed"
        );
    }

    #[test]
    fn source_outside_window_rejected() {
        let target = dm_session("slack:D1", "alice");
        let cutoff = default_cutoff();
        assert!(!eligible(
            &source("slack:D2", "alice", "hi", chrono::Duration::hours(13)),
            &target,
            cutoff
        ));
        assert!(eligible(
            &source("slack:D2", "alice", "hi", chrono::Duration::hours(11)),
            &target,
            cutoff
        ));
    }

    #[test]
    fn source_without_messages_rejected() {
        let target = dm_session("slack:D1", "alice");
        let mut empty = dm_session("slack:D2", "alice");
        empty.updated_at = Utc::now() - minutes(5);
        assert!(!eligible(&empty, &target, default_cutoff()));
    }

    #[test]
    fn source_voice_rejected() {
        let target = dm_session("slack:D1", "alice");
        let mut voice = source("ios", "alice", "hi", minutes(5));
        voice.metadata = serde_json::json!({ "voice_mode": true });
        assert!(!eligible(&voice, &target, default_cutoff()));
    }

    #[test]
    fn source_internal_channel_rejected() {
        let target = dm_session("slack:D1", "alice");
        let cutoff = default_cutoff();
        for channel in ["scheduler:daily", "subagent"] {
            assert!(
                !eligible(&source(channel, "alice", "hi", minutes(5)), &target, cutoff),
                "{channel} must be rejected"
            );
        }
    }

    #[test]
    fn source_in_target_prev_chain_rejected() {
        let target = dm_session("slack:D1", "alice");
        let s = source("slack:D1", "alice", "hi", minutes(5));
        let predecessors: HashSet<String> = [s.id.clone()].into();
        assert!(!digest_source_eligible(
            &s,
            &target,
            AGENT,
            default_cutoff(),
            &predecessors,
            &HashSet::new()
        ));
    }

    #[test]
    fn sealed_source_with_candidate_successor_rejected() {
        let target = dm_session("slack:D1", "alice");
        let sealed = source("slack:D2", "alice", "hi", minutes(5));
        let successor_prevs: HashSet<String> = [sealed.id.clone()].into();
        assert!(!digest_source_eligible(
            &sealed,
            &target,
            AGENT,
            default_cutoff(),
            &HashSet::new(),
            &successor_prevs
        ));
    }

    // ── Rendering ────────────────────────────────────────────────────────

    #[test]
    fn topic_line_prefers_human_label_skips_machine_label() {
        let mut s = Session::new("slack", "alice", AGENT);
        s.add_user_message("first real question", None);

        s.label = Some("slack:alice".into()); // machine label {channel}:{owner}
        assert_eq!(topic_line(&s).as_deref(), Some("first real question"));

        s.label = Some("Uganda trip".into());
        assert_eq!(topic_line(&s).as_deref(), Some("Uganda trip"));
    }

    #[test]
    fn topic_line_uses_compaction_then_carryover_then_first_message() {
        let mut s = Session::new("slack:D1", "alice", AGENT);
        s.add_user_message(
            &format!("{CARRYOVER_PREFIX}\nPrior thread: hotel shortlist"),
            None,
        );
        s.add_user_message("which hotel is closest to the airport?", None);
        s.add_system_message(
            "[Compaction Summary — earlier conversation condensed]\n\nCompared Kampala hotels. More detail here.",
        );

        assert_eq!(topic_line(&s).as_deref(), Some("Compared Kampala hotels."));

        s.messages.retain(|m| !m.is_compaction_summary());
        assert_eq!(
            topic_line(&s).as_deref(),
            Some("Prior thread: hotel shortlist")
        );

        s.messages
            .retain(|m| !m.content.starts_with(CARRYOVER_PREFIX));
        assert_eq!(
            topic_line(&s).as_deref(),
            Some("which hotel is closest to the airport?")
        );

        s.messages.clear();
        assert_eq!(topic_line(&s), None);
    }

    #[test]
    fn last_exchange_clips_flattens_newlines_and_skips_missing_assistant() {
        let mut s = Session::new("slack:D1", "alice", AGENT);
        s.add_user_message("what about\nthe Serena?", None);
        s.add_assistant_message(&"long answer ".repeat(30), 90);

        let exchange = last_exchange(&s).expect("exchange");
        assert!(exchange.starts_with("user: \"what about the Serena?\" / you: \"long answer"));
        assert!(!exchange.contains('\n'));
        assert!(exchange.contains('…'), "long reply is clipped");

        // A user turn with no reply yet renders without the you-clause.
        s.add_user_message("and the Sheraton?", None);
        assert_eq!(
            last_exchange(&s).as_deref(),
            Some("user: \"and the Sheraton?\"")
        );

        // Carry-over-only sessions have no real exchange.
        let mut carry = Session::new("slack:D1", "alice", AGENT);
        carry.add_user_message(&format!("{CARRYOVER_PREFIX}\nold"), None);
        assert_eq!(last_exchange(&carry), None);
    }

    #[test]
    fn render_digest_respects_char_budget() {
        let entry = |topic: String| DigestEntry {
            age: "5m ago".into(),
            surface: "Slack DM",
            topic,
            exchange: Some(format!("user: \"{}\"", "x".repeat(110))),
        };
        let entries = vec![
            entry(format!("newest {}", "a".repeat(300))),
            entry(format!("middle {}", "b".repeat(300))),
            entry(format!("oldest {}", "c".repeat(300))),
        ];
        let out = render_digest(&entries, 1200).expect("digest");
        assert!(out.len() <= 1200, "budget respected, got {}", out.len());
        assert!(out.contains("newest"), "newest entry kept");
        assert!(out.contains("middle"), "second entry still fits");
        assert!(!out.contains("oldest"), "over-budget entry dropped whole");

        // A budget too small for header + first entry yields no block at all.
        assert!(render_digest(&entries, DIGEST_HEADER.len() + 10).is_none());
    }

    #[test]
    fn render_digest_empty_entries_yields_none() {
        assert!(render_digest(&[], 1200).is_none());
    }

    #[test]
    fn surface_label_never_contains_channel_ids() {
        let labels = [
            "Slack DM",
            "iOS chat",
            "web chat",
            "terminal chat",
            "voice chat",
            "direct chat",
        ];
        for channel in [
            "slack:D024GH",
            "slack:D9",
            "ios",
            "web",
            "terminal",
            "cli",
            "whatsapp:441234",
        ] {
            let s = Session::new(channel, "alice", AGENT);
            let label = surface_label(&s);
            assert!(labels.contains(&label), "{label} not in fixed set");
            assert!(!label.contains("D024"), "raw id leaked into {label}");
        }
        let mut voice = Session::new("ios", "alice", AGENT);
        voice.metadata = serde_json::json!({ "voice_mode": true });
        assert_eq!(surface_label(&voice), "voice chat");
    }

    // ── build_digest (store-level) ───────────────────────────────────────

    #[test]
    fn digest_block_none_when_disabled() {
        let store = InMemorySessionStore::new();
        store
            .save(&source("slack:D2", "alice", "hi", minutes(5)))
            .unwrap();
        let target = dm_session("slack:D1", "alice");

        let disabled = SessionDigestConfig {
            enabled: false,
            ..Default::default()
        };
        assert!(build_digest(&store, &disabled, &target, "alice", AGENT, Utc::now()).is_none());

        let zero_sessions = SessionDigestConfig {
            max_sessions: 0,
            ..Default::default()
        };
        assert!(
            build_digest(&store, &zero_sessions, &target, "alice", AGENT, Utc::now()).is_none()
        );

        // window_hours = 0 is a soft-disable: cutoff == now filters everything.
        let zero_window = SessionDigestConfig {
            window_hours: 0,
            ..Default::default()
        };
        assert!(build_digest(&store, &zero_window, &target, "alice", AGENT, Utc::now()).is_none());
    }

    #[test]
    fn digest_block_lists_only_same_owner_dm_sessions() {
        let store = InMemorySessionStore::new();
        store
            .save(&source(
                "slack:D2",
                "alice",
                "Uganda trip hotels",
                minutes(5),
            ))
            .unwrap();
        store
            .save(&source("slack:D3", "bob", "bob private topic", minutes(5)))
            .unwrap();
        let mut shared = source("slack:C7", "shared:1700.1", "room topic", minutes(5));
        shared.context_type = Some(SessionContextType::Thread);
        store.save(&shared).unwrap();
        let mut group = source("slack:C8", "alice", "group topic", minutes(5));
        group.context_type = Some(SessionContextType::Group);
        store.save(&group).unwrap();

        let target = dm_session("slack:D1", "alice");
        store.save(&target).unwrap();

        let cfg = SessionDigestConfig::default();
        let block =
            build_digest(&store, &cfg, &target, "alice", AGENT, Utc::now()).expect("digest");
        assert!(block.starts_with("[RECENT SESSIONS"));
        assert!(block.contains("Uganda trip hotels"), "alice DM rendered");
        assert!(block.contains("(Slack DM)"));
        assert!(!block.contains("bob private topic"), "other owner excluded");
        assert!(!block.contains("room topic"), "shared room excluded");
        assert!(!block.contains("group topic"), "group session excluded");
    }

    #[test]
    fn digest_skips_target_prev_chain_and_sealed_predecessors_of_candidates() {
        let store = InMemorySessionStore::new();

        // The target's own rollover ancestor: already in its carry-over.
        let ancestor = source("slack:D1", "alice", "ancestor topic", minutes(30));
        store.save(&ancestor).unwrap();
        let mut target = dm_session("slack:D1", "alice");
        target.set_prev_session_id(&ancestor.id);
        store.save(&target).unwrap();

        // A sealed segment whose live successor is also a candidate: only the
        // successor (holding the carry-over) is rendered.
        let sealed = source("slack:D2", "alice", "sealed topic", minutes(20));
        store.save(&sealed).unwrap();
        let mut successor = source("slack:D2", "alice", "successor topic", minutes(5));
        successor.set_prev_session_id(&sealed.id);
        store.save(&successor).unwrap();

        let cfg = SessionDigestConfig::default();
        let block =
            build_digest(&store, &cfg, &target, "alice", AGENT, Utc::now()).expect("digest");
        assert!(block.contains("successor topic"));
        assert!(
            !block.contains("ancestor topic"),
            "target's own chain deduped"
        );
        assert!(!block.contains("sealed topic"), "sealed segment deduped");
    }

    #[test]
    fn injection_prepends_before_first_turn_only() {
        let store = InMemorySessionStore::new();
        store
            .save(&source(
                "slack:D2",
                "alice",
                "Uganda trip hotels",
                minutes(5),
            ))
            .unwrap();

        let mut target = dm_session("slack:D1", "alice");
        store.save(&target).unwrap();
        let cfg = SessionDigestConfig::default();

        // Turn 1: session open, digest present; the call site prepends it to
        // the LLM-only effective message.
        let raw = "hi again";
        let block =
            build_digest(&store, &cfg, &target, "alice", AGENT, Utc::now()).expect("turn 1");
        let effective = format!("{block}\n\n{raw}");
        assert!(effective.starts_with("[RECENT SESSIONS"));
        assert!(effective.ends_with(raw));

        // The turn stores the RAW message (mirrors add_user_message_with_trust
        // in both entry points), so the envelope never persists.
        target.add_user_message(raw, None);
        store.save(&target).unwrap();
        assert!(target
            .messages
            .iter()
            .all(|m| !m.content.contains("[RECENT SESSIONS")));

        // Turn 2: real dialogue exists → no digest.
        let reloaded = store.get(&target.id).unwrap().expect("target");
        assert!(build_digest(&store, &cfg, &reloaded, "alice", AGENT, Utc::now()).is_none());
    }
}
