//! Experiential-expertise curation — the Phase-2 counterpart to [`super::deep`].
//!
//! Where deep-expertise models a domain from *real source* (code/docs the curator
//! reads), this deepens the SAME tree from the agent's own *operational history*:
//! it pre-loads excerpts of the agent's recent session transcripts
//! ([`scan_experiential_sessions`]), formats them into an `## Operational
//! Experience` block appended to the deep prompt
//! ([`format_experiential_section`]), and the curation turn distils durable
//! lessons into nodes, each cited to a `session_id:turn_index`.
//!
//! Because an experiential node cites a transcript turn — not a researchable
//! primary source — it must NEVER go through the code quality gate (whose
//! verifier extracts only code/system claims and would vacuously mark it
//! `Verified`). Instead [`ground_experiential`] confirms each cited turn EXISTS
//! in the stored transcript (the curator cannot fabricate a citation) and the
//! node stays [`NodeStatus::Modelled`] for a human to approve the interpretation.
//!
//! The pure pieces ([`scan_experiential_sessions`] over a [`SessionStore`], the
//! cursor logic, [`format_experiential_section`], [`ground_experiential`]) are
//! unit-tested in isolation; the orchestration lives in [`super::runner`].

use std::path::{Path, PathBuf};

use chrono::{DateTime, Utc};
use eyre::Result;
use tracing::debug;

use workforce_common::learning_plan;
use workforce_common::session::{MessageRole, SessionStore, TranscriptEntry};
use workforce_self_learning::{DeepNode, SourceKind};

/// Cursor file under `{workspace}/learning/`, distinct from the
/// consolidation cursor (`{workspace}/memory/consolidation-cursor`) so the two
/// scanners advance independently.
const CURSOR_FILENAME: &str = "experiential-cursor";

/// Safety bound on sessions pulled per scan — a cap, not a budget. With
/// `list_since` doing the primary agent+cutoff filtering it is reached only on a
/// genuinely high-volume agent. Mirrors the consolidation scan's bound.
const SESSION_SCAN_CAP: usize = 2000;
/// Bound the pre-loaded prompt: keep the most recent turns, truncating each side
/// so a long lookback window or a verbose turn can't blow the curation context.
const EXPERIENTIAL_MAX_TURNS: usize = 40;
const EXPERIENTIAL_TURN_CHARS: usize = 600;

/// One paired user→assistant exchange distilled into the prompt. `turn_index` is
/// the position of the pair within the session's transcript and is the stable
/// label a recorded lesson cites as `session_id:turn_index`; [`ground_experiential`]
/// re-derives it the same way, so the citation round-trips.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExperientialTurn {
    pub session_id: String,
    pub turn_index: usize,
    pub user: String,
    pub assistant: String,
    pub at: DateTime<Utc>,
}

/// Pair a transcript's user→assistant exchanges, the single definition shared by
/// the scan (which formats the pairs into the prompt) and grounding (which
/// re-finds a cited pair). Each `User` entry opens a turn closed by the next
/// `Assistant` entry; `turn_index` counts paired turns in order. Tool and system
/// entries are skipped. A trailing user with no assistant reply is not a turn.
fn pair_turns(transcript: &[TranscriptEntry]) -> Vec<ExperientialTurn> {
    let mut turns = Vec::new();
    let mut pending: Option<&TranscriptEntry> = None;
    for entry in transcript {
        match entry.role {
            MessageRole::User => pending = Some(entry),
            MessageRole::Assistant => {
                if let Some(user) = pending.take() {
                    turns.push(ExperientialTurn {
                        session_id: entry.session_id.clone(),
                        turn_index: turns.len(),
                        user: user.content.clone(),
                        assistant: entry.content.clone(),
                        at: entry.timestamp,
                    });
                }
            }
            MessageRole::System | MessageRole::Tool => {}
        }
    }
    turns
}

/// Scan the agent's recent session transcripts and pair their user→assistant
/// turns for the experiential curation prompt.
///
/// The cutoff is pinned the way the consolidation scan pins its own: read the
/// cursor and take `max(now - lookback, cursor)` so a fresh install starts at the
/// lookback window and subsequent runs never re-walk before the cursor. On a
/// successful scan the cursor advances to the NEWEST scanned session's
/// `updated_at` (the boundary session re-appears next run because `list_since`'s
/// cutoff is inclusive — harmless, as record_node upserts by tree position). An
/// empty scan advances the cursor to `now`.
pub fn scan_experiential_sessions(
    sessions: &dyn SessionStore,
    workspace: &Path,
    agent_id: &str,
    lookback_secs: u64,
    now: DateTime<Utc>,
) -> Result<Vec<ExperientialTurn>> {
    let window_start = now - chrono::Duration::seconds(lookback_secs as i64);
    let cutoff = read_cursor(workspace).map_or(window_start, |c| c.max(window_start));

    // Shared (multi-human) sessions are skipped: their turns belong to no single
    // user, so feeding them into the agent's learned experience would widen one
    // room's content — a private channel included — to everyone the agent talks
    // to. Their multi-speaker transcript would also mis-pair user/assistant turns.
    // An eval's turns are skipped too: a set's questions are nobody's experience.
    let recent: Vec<_> = sessions
        .list_since(agent_id, cutoff, SESSION_SCAN_CAP)?
        .into_iter()
        .filter(|s| {
            !s.is_shared_conversation() && !crate::turn_extraction::is_eval_channel(&s.channel)
        })
        .collect();
    if recent.is_empty() {
        debug!("Experiential scan: no new sessions since last scan");
        write_cursor(workspace, now)?;
        return Ok(Vec::new());
    }

    // What is learned here reaches every user's prompt, so it may be drawn
    // only from conversations every user could have read: one person's own.
    // An agent several people talk to privately would turn one user's DM into
    // another's context, so its transcripts are not read at all.
    let mut owners: Vec<&str> = recent.iter().map(|s| s.owner.as_str()).collect();
    owners.sort_unstable();
    owners.dedup();
    if owners.len() > 1 {
        tracing::warn!(
            agent = agent_id,
            people = owners.len(),
            "Experiential scan skipped: private sessions from more than one person; their \
             transcripts are never learned from together"
        );
        write_cursor(workspace, now)?;
        return Ok(Vec::new());
    }

    // `list_since` returns newest-first; the newest `updated_at` is the cursor's
    // next anchor.
    let newest = recent.iter().map(|s| s.updated_at).max().unwrap_or(now);

    let mut turns = Vec::new();
    for session in &recent {
        match sessions.read_transcript(&session.id) {
            Ok(transcript) => turns.extend(pair_turns(&transcript)),
            Err(e) => {
                debug!(session = %session.id, error = %e, "Experiential scan: transcript read failed")
            }
        }
    }

    turns.truncate(EXPERIENTIAL_MAX_TURNS);
    write_cursor(workspace, newest)?;
    Ok(turns)
}

/// Render the `## Operational Experience` block appended to the deep prompt. Pure
/// over the paired turns. Each turn is labelled `session_id:turn_index` — exactly
/// the `reference` a recorded lesson must cite — so the curator has the citation
/// in front of it. Empty input ⇒ empty string (the runner appends nothing).
pub fn format_experiential_section(turns: &[ExperientialTurn]) -> String {
    if turns.is_empty() {
        return String::new();
    }
    let mut out = String::from(
        "## Operational Experience\nExcerpts from your recent sessions. Cite a lesson to its \
         turn as `session_id:turn_index`.\n\n",
    );
    for turn in turns {
        out.push_str(&format!(
            "### {}:{}\nUser: {}\nYou: {}\n\n",
            turn.session_id,
            turn.turn_index,
            workforce_common::text::truncate_with_ellipsis(
                turn.user.trim(),
                EXPERIENTIAL_TURN_CHARS
            ),
            workforce_common::text::truncate_with_ellipsis(
                turn.assistant.trim(),
                EXPERIENTIAL_TURN_CHARS
            ),
        ));
    }
    out
}

/// The verdict of grounding an experiential node against its transcripts: the
/// citations that could not be confirmed (unparseable, or a `session_id:turn_index`
/// whose turn is absent from the stored transcript). An empty vec ⇒ every cited
/// turn was found; the caller leaves the node `Modelled` either way (presence
/// proves the citation is real, not that the lesson is correctly drawn).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ExperientialGrounding {
    pub bad_citations: Vec<String>,
}

impl ExperientialGrounding {
    pub fn is_grounded(&self) -> bool {
        self.bad_citations.is_empty()
    }
}

/// Confirm each `Experience` source of a node cites a turn that EXISTS in its
/// stored transcript — the curator cannot fabricate a citation. Pure over the
/// node and a [`SessionStore`] (no tree store): it returns the verdict and the
/// caller persists the review note + leaves the node `Modelled`, so "never
/// auto-Verified" is structurally guaranteed. Non-`Experience` sources are
/// ignored (an experiential node should carry only `Experience` sources, but a
/// mixed node grounds on the experiential ones alone).
pub fn ground_experiential(node: &DeepNode, sessions: &dyn SessionStore) -> ExperientialGrounding {
    let mut bad = Vec::new();
    for source in &node.sources {
        if source.kind != SourceKind::Experience {
            continue;
        }
        let reference = source.reference.trim();
        let Some((session_id, index)) = parse_citation(reference) else {
            bad.push(reference.to_string());
            continue;
        };
        let present = sessions
            .read_transcript(session_id)
            .map(|transcript| index < pair_turns(&transcript).len())
            .unwrap_or(false);
        if !present {
            bad.push(reference.to_string());
        }
    }
    ExperientialGrounding { bad_citations: bad }
}

/// Parse a `session_id:turn_index` citation. `rsplit_once(':')` so a session id
/// containing colons keeps everything before the final `:` as the id. A blank id
/// or a non-numeric index is unparseable.
fn parse_citation(reference: &str) -> Option<(&str, usize)> {
    let (session_id, index) = reference.rsplit_once(':')?;
    if session_id.is_empty() {
        return None;
    }
    index.parse::<usize>().ok().map(|i| (session_id, i))
}

fn cursor_path(workspace: &Path) -> PathBuf {
    learning_plan::learning_plans_dir(workspace).join(CURSOR_FILENAME)
}

fn read_cursor(workspace: &Path) -> Option<DateTime<Utc>> {
    let content = std::fs::read_to_string(cursor_path(workspace)).ok()?;
    content.trim().parse::<DateTime<Utc>>().ok()
}

fn write_cursor(workspace: &Path, when: DateTime<Utc>) -> Result<()> {
    let path = cursor_path(workspace);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(&path, when.to_rfc3339())?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use uuid::Uuid;
    use workforce_common::session::{InMemorySessionStore, Session};
    use workforce_self_learning::{ItemSource, NodeStatus, SurfaceItem};

    fn store() -> InMemorySessionStore {
        InMemorySessionStore::new()
    }

    /// Save a session for `agent` aged `secs_ago`, with `pairs` user→assistant
    /// exchanges, and return its id.
    fn seed_session(
        store: &InMemorySessionStore,
        agent: &str,
        channel: &str,
        secs_ago: i64,
        pairs: &[(&str, &str)],
    ) -> String {
        seed_session_of(store, "owner", agent, channel, secs_ago, pairs)
    }

    fn seed_session_of(
        store: &InMemorySessionStore,
        owner: &str,
        agent: &str,
        channel: &str,
        secs_ago: i64,
        pairs: &[(&str, &str)],
    ) -> String {
        let mut session = Session::new(channel, owner, agent);
        for (user, assistant) in pairs {
            session.add_user_message(user, None);
            session.add_assistant_message(assistant, 100);
        }
        let when = Utc::now() - chrono::Duration::seconds(secs_ago);
        session.created_at = when;
        session.updated_at = when;
        store.save(&session).unwrap();
        session.id
    }

    fn node_with_sources(sources: Vec<ItemSource>) -> DeepNode {
        let now = Utc::now();
        DeepNode {
            id: Uuid::new_v4(),
            agent_id: "a".into(),
            plan_id: "p".into(),
            parent_id: None,
            depth: 0,
            title: "Lesson".into(),
            path_hash: "h".into(),
            body: "In practice X.".into(),
            sources,
            coverage: Vec::<SurfaceItem>::new(),
            memory_id: None,
            status: NodeStatus::Modelled,
            confidence: 0.5,
            created_at: now,
            created_by: "id".into(),
            verified_at: now,
            expires_at: None,
            reviewed_at: None,
            reviewed_by: None,
            review_note: None,
            superseded_by: None,
        }
    }

    fn experience_source(reference: &str) -> ItemSource {
        ItemSource {
            kind: SourceKind::Experience,
            skill: None,
            reference: reference.into(),
            excerpt: None,
        }
    }

    #[test]
    fn pair_turns_pairs_user_assistant_and_skips_unmatched() {
        let store = store();
        let id = seed_session(
            &store,
            "a",
            "c",
            10,
            &[
                ("how do I deploy?", "run the deploy script"),
                ("and rollback?", "use --rollback"),
            ],
        );
        let transcript = SessionStore::read_transcript(&store, &id).unwrap();
        let turns = pair_turns(&transcript);
        assert_eq!(turns.len(), 2);
        assert_eq!(turns[0].turn_index, 0);
        assert_eq!(turns[0].user, "how do I deploy?");
        assert_eq!(turns[0].assistant, "run the deploy script");
        assert_eq!(turns[1].turn_index, 1);
        assert_eq!(turns[1].session_id, id);
    }

    #[test]
    fn scan_returns_paired_turns_within_lookback() {
        let dir = tempfile::tempdir().unwrap();
        let store = store();
        seed_session(&store, "a", "c1", 60, &[("q1", "a1")]);
        seed_session(&store, "a", "c2", 120, &[("q2", "a2"), ("q3", "a3")]);
        // Another agent's session must be excluded.
        seed_session(&store, "other", "c3", 30, &[("qx", "ax")]);

        let turns = scan_experiential_sessions(&store, dir.path(), "a", 3600, Utc::now()).unwrap();
        assert_eq!(turns.len(), 3);
        assert!(turns.iter().all(|t| t.user != "qx"));
    }

    #[test]
    fn an_evals_turns_are_never_learned_from() {
        let dir = tempfile::tempdir().unwrap();
        let store = store();
        seed_session(&store, "a", "eval:t1", 60, &[("a set's question", "42")]);
        seed_session(&store, "a", "web", 120, &[("q1", "a1")]);
        let turns = scan_experiential_sessions(&store, dir.path(), "a", 3600, Utc::now()).unwrap();
        assert_eq!(turns.len(), 1);
        assert_eq!(turns[0].user, "q1");
    }

    #[test]
    fn two_peoples_private_sessions_are_never_learned_from_together() {
        let dir = tempfile::tempdir().unwrap();
        let store = store();
        seed_session_of(
            &store,
            "alice",
            "a",
            "c1",
            60,
            &[("alice's secret", "noted")],
        );
        seed_session_of(
            &store,
            "bob",
            "a",
            "c2",
            120,
            &[("bob's question", "answer")],
        );
        let turns = scan_experiential_sessions(&store, dir.path(), "a", 3600, Utc::now()).unwrap();
        assert!(
            turns.is_empty(),
            "one user's DM must never become another's context"
        );

        let dir = tempfile::tempdir().unwrap();
        let alone = super::tests::store();
        seed_session_of(&alone, "alice", "a", "c1", 60, &[("q1", "a1")]);
        seed_session_of(&alone, "alice", "a", "c2", 90, &[("q2", "a2")]);
        let turns = scan_experiential_sessions(&alone, dir.path(), "a", 3600, Utc::now()).unwrap();
        assert_eq!(
            turns.len(),
            2,
            "one person's own sessions are still learned from"
        );
    }

    #[test]
    fn scan_excludes_sessions_older_than_lookback() {
        let dir = tempfile::tempdir().unwrap();
        let store = store();
        // 2h old, lookback 1h ⇒ excluded.
        seed_session(&store, "a", "old", 7200, &[("q", "a")]);
        seed_session(&store, "a", "fresh", 600, &[("recent", "answer")]);

        let turns = scan_experiential_sessions(&store, dir.path(), "a", 3600, Utc::now()).unwrap();
        assert_eq!(turns.len(), 1);
        assert_eq!(turns[0].user, "recent");
    }

    #[test]
    fn scan_advances_cursor_then_skips_already_scanned() {
        let dir = tempfile::tempdir().unwrap();
        let store = store();
        let now = Utc::now();
        // A session updated 1h ago is scanned, the cursor advances to its
        // updated_at, and a second scan a moment later (with the SAME session,
        // now strictly before the cursor) returns nothing.
        seed_session(&store, "a", "c", 3600, &[("q", "a")]);
        let first = scan_experiential_sessions(&store, dir.path(), "a", 86400, now).unwrap();
        assert_eq!(first.len(), 1);

        // The cursor now sits at the scanned session's updated_at; a later scan
        // with a wide lookback re-walks only sessions AT/after the cursor. The
        // boundary session re-appears (inclusive cutoff) but nothing older does.
        let later = now + chrono::Duration::seconds(1);
        let second = scan_experiential_sessions(&store, dir.path(), "a", 86400, later).unwrap();
        // Only the boundary session (inclusive cutoff) re-appears; no new turns
        // beyond it. Re-walking it is harmless (upsert-by-path).
        assert_eq!(second.len(), 1);

        // A genuinely older session (before the cursor) is NOT re-scanned.
        seed_session(&store, "a", "ancient", 100_000, &[("old", "stale")]);
        let third = scan_experiential_sessions(&store, dir.path(), "a", 86400, later).unwrap();
        assert!(third.iter().all(|t| t.user != "old"));
    }

    #[test]
    fn scan_empty_advances_cursor_to_now() {
        let dir = tempfile::tempdir().unwrap();
        let store = store();
        let turns = scan_experiential_sessions(&store, dir.path(), "a", 3600, Utc::now()).unwrap();
        assert!(turns.is_empty());
        assert!(read_cursor(dir.path()).is_some());
    }

    #[test]
    fn format_section_labels_turns_with_citation() {
        let turns = vec![ExperientialTurn {
            session_id: "sess-1".into(),
            turn_index: 2,
            user: "why did X fail?".into(),
            assistant: "because Y".into(),
            at: Utc::now(),
        }];
        let out = format_experiential_section(&turns);
        assert!(out.contains("## Operational Experience"));
        assert!(out.contains("### sess-1:2"));
        assert!(out.contains("why did X fail?"));
        assert!(out.contains("because Y"));
    }

    #[test]
    fn format_section_empty_is_empty_string() {
        assert!(format_experiential_section(&[]).is_empty());
    }

    #[test]
    fn ground_present_citation_is_grounded() {
        let store = store();
        let id = seed_session(&store, "a", "c", 10, &[("q0", "a0"), ("q1", "a1")]);
        let node = node_with_sources(vec![
            experience_source(&format!("{id}:0")),
            experience_source(&format!("{id}:1")),
        ]);
        let verdict = ground_experiential(&node, &store);
        assert!(verdict.is_grounded());
        assert!(verdict.bad_citations.is_empty());
        // Grounding never promotes status; the node is recorded `Modelled` and
        // stays so (the caller does not touch status on a clean verdict).
        assert_eq!(node.status, NodeStatus::Modelled);
    }

    #[test]
    fn ground_fabricated_turn_is_not_grounded() {
        let store = store();
        let id = seed_session(&store, "a", "c", 10, &[("q0", "a0")]);
        // Turn 5 does not exist (only turn 0 does).
        let node = node_with_sources(vec![experience_source(&format!("{id}:5"))]);
        let verdict = ground_experiential(&node, &store);
        assert!(!verdict.is_grounded());
        assert_eq!(verdict.bad_citations, vec![format!("{id}:5")]);
    }

    #[test]
    fn ground_unparseable_and_unknown_session_citations_are_bad() {
        let store = store();
        let node = node_with_sources(vec![
            experience_source("no-colon-here"),
            experience_source("missing-session:0"),
            experience_source(":3"),
        ]);
        let verdict = ground_experiential(&node, &store);
        assert!(!verdict.is_grounded());
        assert_eq!(verdict.bad_citations.len(), 3);
    }

    #[test]
    fn ground_ignores_non_experience_sources() {
        let store = store();
        let node = node_with_sources(vec![ItemSource {
            kind: SourceKind::Codebase,
            skill: None,
            reference: "src/lib.rs:1".into(),
            excerpt: None,
        }]);
        // No Experience sources ⇒ nothing to fail, vacuously grounded.
        let verdict = ground_experiential(&node, &store);
        assert!(verdict.is_grounded());
    }
}
