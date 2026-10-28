//! A Desk DM read back: the conversation one person keeps with one agent on
//! the `desk` channel, from the session store the agent itself reads.
//!
//! The conversation is the session a `/ws` connection on `desk` anchors on
//! for its person (`identity_id`, else the paired device) and agent: one
//! active session per person and agent, in the agent's own store, so a
//! reconnect, a Desk restart or a hub restart lands on the same one. A
//! conversation left a day goes on in a successor holding a carry-over;
//! the read walks the whole chain, so it reads as one.

use std::sync::Arc;

use async_trait::async_trait;
use axum::http::HeaderMap;
use workforce_agent::rollover::CARRYOVER_PREFIX;
use workforce_common::channels::DESK_CHANNEL;
use workforce_common::secret_marker::render_markers_as_preview;
use workforce_common::security_scanner::{contains_credentials, redact_credentials};
use workforce_common::session::{MessageRole, SessionStore, TranscriptEntry};
use workforce_desk::{Author, ConversationError, Conversations, Page, Said};

use crate::gateway::{resolve_caller_context, AuthFailure, GatewayState};

/// How many transcript entries one read of the store asks for: dialogue is
/// interleaved with tool rows, so a page of messages takes several.
const ENTRIES_PER_READ: usize = 400;

/// How many reads one page may take before it settles for what it has.
const MAX_READS: usize = 25;

/// User-role rows the hub writes for the model, never typed by anyone: a
/// rolled-over thread's carry-over, compaction's orientation note, and the
/// overflow notes the runner leaves when it drops context.
const HUB_NOTES: &[&str] = &[CARRYOVER_PREFIX, "[System note:", "[SYSTEM]"];

/// What a transcript entry shows in the conversation, if it is one of its
/// messages: said by the person or the agent, not tool chatter, a system
/// prompt or a note the hub left for the model. The text is what the live
/// channel sends: secret markers as previews, credentials redacted.
fn said(entry: &TranscriptEntry) -> Option<Said> {
    let from = match entry.role {
        MessageRole::User => Author::User,
        MessageRole::Assistant => Author::Agent,
        MessageRole::System | MessageRole::Tool => return None,
    };
    let text = entry.content.trim();
    if entry.tool_metadata.is_some()
        || text.is_empty()
        || HUB_NOTES.iter().any(|note| text.starts_with(note))
    {
        return None;
    }
    let rendered = render_markers_as_preview(&entry.content);
    let text = match contains_credentials(&rendered) {
        true => redact_credentials(&rendered),
        false => rendered,
    };
    Some(Said {
        id: entry.id.clone(),
        from,
        text,
        at: entry.timestamp,
    })
}

/// The chain member that holds the message `id`, walking back from `head`:
/// in its archive, or in its live messages for a session the archive does
/// not hold yet (whose transcript reads them instead).
fn holder(store: &dyn SessionStore, head: &str, id: &str) -> eyre::Result<Option<String>> {
    let mut session = Some(head.to_string());
    for _ in 0..50 {
        let Some(current) = session else { break };
        let live = store.get(&current)?;
        if store.read_archive_message(&current, id)?.is_some()
            || live
                .as_ref()
                .is_some_and(|s| s.messages.iter().any(|m| m.id == id))
        {
            return Ok(Some(current));
        }
        session = live.and_then(|s| s.prev_session_id().map(str::to_string));
    }
    Ok(None)
}

/// `owner`'s conversation on the `desk` channel in `store`: at most `limit`
/// messages ending just before `before`, oldest first. Reads only: it never
/// creates the session or rolls it over.
pub(crate) fn page(
    store: &dyn SessionStore,
    owner: &str,
    before: Option<&str>,
    limit: usize,
) -> Result<Page, ConversationError> {
    let internal = |e: eyre::Report| ConversationError::Internal(format!("{e:#}"));
    let Some(head) = store.find(DESK_CHANNEL, owner).map_err(internal)? else {
        return Ok(Page::default());
    };
    let mut cursor = match before {
        None => None,
        Some(id) => match holder(store, &head.id, id).map_err(internal)? {
            Some(session) => Some((session, id.to_string())),
            None => {
                return Err(ConversationError::Invalid(
                    "before names no message of this conversation".into(),
                ))
            }
        },
    };
    // Newest first while gathering; one past the limit says more remain.
    let mut gathered: Vec<Said> = Vec::new();
    let mut exhausted = false;
    for _ in 0..MAX_READS {
        let read = store
            .read_thread_page(
                &head.id,
                cursor.as_ref().map(|(s, _)| s.as_str()),
                cursor.as_ref().map(|(_, id)| id.as_str()),
                ENTRIES_PER_READ,
            )
            .map_err(internal)?;
        gathered.extend(read.messages.iter().rev().filter_map(said));
        let oldest = read.messages.first();
        exhausted = !read.has_more || oldest.is_none();
        if gathered.len() > limit || exhausted {
            break;
        }
        cursor = oldest.map(|e| (e.session_id.clone(), e.id.clone()));
    }
    let more = gathered.len() > limit || !exhausted;
    gathered.truncate(limit);
    gathered.reverse();
    Ok(Page {
        messages: gathered,
        more,
    })
}

/// `GET /api/desk/conversations/{agent}` on `state`'s agents.
pub fn router(state: Arc<GatewayState>) -> axum::Router {
    workforce_desk::conversations::router(Arc::new(HubConversations(state)))
}

/// Reads a Desk DM for the person a request carries: a paired device (or a
/// signed-in person) reads their own. The gateway's admin token names no
/// person, so it reads none.
pub(crate) struct HubConversations(pub(crate) Arc<GatewayState>);

#[async_trait]
impl Conversations for HubConversations {
    async fn page(
        &self,
        headers: &HeaderMap,
        token: Option<&str>,
        agent: &str,
        before: Option<&str>,
        limit: usize,
    ) -> Result<Page, ConversationError> {
        let state = &self.0;
        let caller = resolve_caller_context(state, headers, token).await;
        match caller.auth_failure {
            Some(AuthFailure::InvalidCredentials) => return Err(ConversationError::Unauthorized),
            Some(AuthFailure::StoreError) => {
                return Err(ConversationError::Unavailable(
                    "the hub could not check that token".into(),
                ))
            }
            None => {}
        }
        let owner = match (caller.identity_id.as_deref(), caller.device_id.as_deref()) {
            (Some(person), _) | (None, Some(person)) => person.to_string(),
            (None, None) if caller.is_admin => {
                return Err(ConversationError::Invalid(
                    "the hub's admin token is no one's: read a conversation with the token \
                     its chat uses"
                        .into(),
                ))
            }
            (None, None) => return Err(ConversationError::Unauthorized),
        };
        if !caller.has_scope(crate::jwt::SCOPE_SESSIONS) {
            return Err(ConversationError::Forbidden(
                "this token may not read conversations".into(),
            ));
        }
        if let Some(id) = caller.identity_id.as_deref() {
            if crate::standing::suspended(state.identity_store.as_ref(), id).is_some() {
                return Err(ConversationError::Forbidden(
                    "this person is suspended".into(),
                ));
            }
        }
        let service = state
            .registry
            .get(agent)
            .ok_or_else(|| ConversationError::NotFound(format!("no agent named {agent:?}")))?;
        page(service.sessions.as_ref(), &owner, before, limit)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use workforce_common::session::{InMemorySessionStore, JsonlSessionStore, SessionMessage};

    const ME: &str = "identity-1";

    fn texts(page: &Page) -> Vec<(&str, Author)> {
        page.messages
            .iter()
            .map(|m| (m.text.as_str(), m.from))
            .collect()
    }

    /// One exchange as a turn leaves it: the question, its tool rows, the
    /// answer.
    fn exchange(store: &dyn SessionStore, n: usize) {
        let mut session = store.get_or_create(DESK_CHANNEL, ME, "scout").unwrap();
        session.add_user_message(&format!("question {n}"), None);
        session.messages.push(SessionMessage::tool(
            "notes",
            &serde_json::json!({}),
            &serde_json::json!("ok"),
            true,
            1,
            None,
        ));
        session.add_assistant_message(&format!("answer {n}"), 5);
        store.save(&session).unwrap();
    }

    #[test]
    fn the_conversation_is_what_was_said_without_tool_rows_or_hub_notes() {
        let store = InMemorySessionStore::new();
        let mut session = store.get_or_create(DESK_CHANNEL, ME, "scout").unwrap();
        session.add_system_message("## Your Identity");
        session.add_user_message("what model are you on?", None);
        session.messages.push(SessionMessage::tool(
            "settings",
            &serde_json::json!({}),
            &serde_json::json!("{}"),
            true,
            62,
            None,
        ));
        session.add_assistant_message("", 0);
        session.add_user_message(
            "[System note: context was compacted. 4 messages were summarised.]",
            None,
        );
        session.add_user_message(&format!("{CARRYOVER_PREFIX}\nearlier"), None);
        session.add_assistant_message(
            "I'm on **gpt-6-luna**.\n\n```chart\n{\"type\":\"bar\"}\n```",
            9,
        );
        store.save(&session).unwrap();
        let page = page(&store, ME, None, 200).unwrap();
        assert_eq!(
            texts(&page),
            [
                ("what model are you on?", Author::User),
                (
                    "I'm on **gpt-6-luna**.\n\n```chart\n{\"type\":\"bar\"}\n```",
                    Author::Agent
                ),
            ]
        );
        assert!(!page.more);
        assert_eq!(page.messages[0].id, session.messages[1].id);
        assert_eq!(page.messages[0].at, session.messages[1].timestamp);
    }

    #[test]
    fn nobody_elses_conversation_and_no_conversation_read_as_empty_and_create_nothing() {
        let store = InMemorySessionStore::new();
        exchange(&store, 0);
        assert_eq!(
            page(&store, "identity-2", None, 200).unwrap(),
            Page::default()
        );
        assert!(store.find(DESK_CHANNEL, "identity-2").unwrap().is_none());
        let mut room = store.get_or_create("desk:release", ME, "scout").unwrap();
        room.add_user_message("in the room", None);
        store.save(&room).unwrap();
        let mine = page(&store, ME, None, 200).unwrap();
        assert_eq!(mine.messages.len(), 2, "a room is not the DM");
    }

    #[test]
    fn pages_walk_back_through_before_until_nothing_more_remains() {
        let store = InMemorySessionStore::new();
        for n in 0..5 {
            exchange(&store, n);
        }
        let newest = page(&store, ME, None, 4).unwrap();
        assert_eq!(
            texts(&newest),
            [
                ("question 3", Author::User),
                ("answer 3", Author::Agent),
                ("question 4", Author::User),
                ("answer 4", Author::Agent),
            ]
        );
        assert!(newest.more);
        let older = page(&store, ME, Some(&newest.messages[0].id), 4).unwrap();
        assert_eq!(older.messages[0].text, "question 1");
        assert_eq!(older.messages[3].text, "answer 2");
        assert!(older.more);
        let oldest = page(&store, ME, Some(&older.messages[0].id), 4).unwrap();
        assert_eq!(
            texts(&oldest),
            [("question 0", Author::User), ("answer 0", Author::Agent)]
        );
        assert!(!oldest.more);
        let refused = page(&store, ME, Some("not-a-message"), 4).unwrap_err();
        assert!(
            matches!(refused, ConversationError::Invalid(_)),
            "{refused}"
        );
    }

    #[test]
    fn a_secret_reads_as_it_reads_live() {
        let store = InMemorySessionStore::new();
        let mut session = store.get_or_create(DESK_CHANNEL, ME, "scout").unwrap();
        session.add_assistant_message(
            "the key is AKIAIOSFODNN7EXAMPLE and the secret wJalrXUtnFEMI/K7MDENG/bPxRfiCYEXAMPLEKEY",
            5,
        );
        store.save(&session).unwrap();
        let text = &page(&store, ME, None, 10).unwrap().messages[0].text;
        assert!(!text.contains("AKIAIOSFODNN7EXAMPLE"), "{text}");
    }

    /// The hub's own store: pages walk back the same whether the messages
    /// are in the archive yet or only in the session.
    #[tokio::test(flavor = "multi_thread")]
    async fn on_postgres_pages_walk_back_from_the_archive_or_the_live_session() {
        use workforce_common::session_postgres::PostgresSessionStore;
        workforce_postgres::test_support::with_test_pool(
            PostgresSessionStore::build_pool,
            |pool| async move {
                let store = PostgresSessionStore::with_pool(pool, "scout")
                    .await
                    .expect("store");
                for n in 0..3 {
                    exchange(store.as_ref(), n);
                }
                let walk = |store: &dyn SessionStore| {
                    let newest = page(store, ME, None, 4).unwrap();
                    assert_eq!(newest.messages[0].text, "question 1");
                    assert!(newest.more);
                    let oldest = page(store, ME, Some(&newest.messages[0].id), 4).unwrap();
                    assert_eq!(
                        texts(&oldest),
                        [("question 0", Author::User), ("answer 0", Author::Agent)]
                    );
                    assert!(!oldest.more);
                };
                walk(store.as_ref());
                let session = store.find(DESK_CHANNEL, ME).unwrap().unwrap();
                for message in &session.messages {
                    store.append_to_archive(&session.id, message).unwrap();
                }
                walk(store.as_ref());
            },
        )
        .await;
    }

    /// A dormant DM goes on in a successor; the read crosses the seam.
    #[test]
    fn a_rolled_over_conversation_reads_as_one() {
        let dir = tempfile::tempdir().unwrap();
        let store = JsonlSessionStore::new(dir.path()).unwrap();
        exchange(&store, 0);
        let old = store.find(DESK_CHANNEL, ME).unwrap().unwrap();
        let now = chrono::Utc::now() + chrono::Duration::days(3);
        let successor = workforce_agent::rollover::roll_if_stale(
            &store,
            old.clone(),
            &workforce_common::session::CompactionConfig::default(),
            now,
        )
        .unwrap();
        assert_ne!(successor.id, old.id, "the DM rolled over");
        exchange(&store, 1);
        let page = page(&store, ME, None, 200).unwrap();
        assert_eq!(
            texts(&page),
            [
                ("question 0", Author::User),
                ("answer 0", Author::Agent),
                ("question 1", Author::User),
                ("answer 1", Author::Agent),
            ]
        );
        let older = super::page(&store, ME, Some(&page.messages[2].id), 200).unwrap();
        assert_eq!(older.messages.len(), 2, "the cursor crosses the seam");
    }
}
