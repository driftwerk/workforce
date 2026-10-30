//! A Desk conversation read back: the DM or a room's conversation one person
//! keeps with one agent, from the session store the agent itself reads.
//!
//! The conversation is the session a `/ws` connection on its Desk channel
//! anchors on for its person (`identity_id`, else the paired device) and
//! agent: one active session per person, channel and agent, in the agent's
//! own store, so a reconnect, a Desk restart or a hub restart lands on the
//! same one. A conversation left a day goes on in a successor holding a
//! carry-over; the read walks the whole chain, so it reads as one.

use std::sync::Arc;

use async_trait::async_trait;
use axum::http::HeaderMap;
use chrono::{DateTime, Utc};
use workforce_agent::rollover::CARRYOVER_PREFIX;
use workforce_common::channels::{is_desk_conversation, DESK_CHANNEL};
use workforce_common::session::{MessageRole, SessionStore, TranscriptEntry};
use workforce_desk::{
    Active, Author, Close, Closed, ConversationError, Conversations, Cursor, Page, Said, Speaker,
};

use crate::desk_surface::shown;
use crate::gateway::{resolve_caller_context, AuthFailure, GatewayState};

/// How many transcript entries one read of the store asks for: dialogue is
/// interleaved with tool rows, so a page of messages takes several.
const ENTRIES_PER_READ: usize = 400;

/// How many reads a page back from the newest, or from a message, may take
/// before it settles for what it has. A page forward from a message reads
/// back until it reaches that message, however far.
const MAX_READS: usize = 25;

/// User-role rows the hub writes for the model, never typed by anyone: a
/// rolled-over thread's carry-over, compaction's orientation note, and the
/// overflow notes the runner leaves when it drops context.
const HUB_NOTES: &[&str] = &[
    CARRYOVER_PREFIX,
    "[System note:",
    "[SYSTEM]",
    crate::desk_crew::CHECK_IN,
];

/// What a transcript entry shows in the conversation, if it is one of its
/// messages: said by the person or by `agent`, not tool chatter, a system
/// prompt or a note the hub left for the model; a file delivered without a
/// caption is one, with no text. The text is what the live channel sends:
/// secret markers as previews, credentials redacted; the files its mark
/// names are its attachments. A person's message is theirs
/// until [`with_authors`] says otherwise.
fn said(entry: &TranscriptEntry, agent: &Speaker) -> Option<Said> {
    let (from, author) = match entry.role {
        MessageRole::User => (Author::User, Speaker::you()),
        MessageRole::Assistant => (Author::Agent, agent.clone()),
        MessageRole::System | MessageRole::Tool => return None,
    };
    let text = entry.content.trim();
    let carries_files = entry
        .desk
        .as_ref()
        .is_some_and(|mark| !mark.files.is_empty());
    if entry.tool_metadata.is_some()
        || (text.is_empty() && !carries_files)
        || HUB_NOTES.iter().any(|note| text.starts_with(note))
    {
        return None;
    }
    let text = shown(&entry.content);
    let mut mark = entry.desk.clone();
    let files = mark
        .as_mut()
        .map(|mark| std::mem::take(&mut mark.files))
        .unwrap_or_default();
    Some(Said {
        id: entry.id.clone(),
        from,
        author,
        text,
        at: entry.timestamp,
        desk: None,
        mark,
        attachments: files.iter().map(Into::into).collect(),
    })
}

/// The time a page of a conversation covers, which the files that are
/// messages of their own are placed in: from its first message when older
/// ones remain before it, else from the start; up to the message after its
/// last, else to now.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct Span {
    pub after: Option<DateTime<Utc>>,
    pub before: Option<DateTime<Utc>>,
}

/// `owner`'s conversation on the Desk `channel` with `agent` in `store`: at
/// most `limit` messages at `cursor`, oldest first, and the time they
/// cover. Reads only: it never creates the session or rolls it over.
pub(crate) fn read(
    store: &dyn SessionStore,
    agent: &Speaker,
    owner: &str,
    channel: &str,
    cursor: Cursor<'_>,
    limit: usize,
) -> Result<(Page, Span), ConversationError> {
    let internal = |e: eyre::Report| ConversationError::Internal(format!("{e:#}"));
    if !is_desk_conversation(channel) {
        return Err(ConversationError::Invalid(format!(
            "{channel:?} is not a Desk conversation"
        )));
    }
    let Some(head) = store.find(channel, owner).map_err(internal)? else {
        return Ok((Page::default(), Span::default()));
    };
    let unknown = |side: &str| {
        ConversationError::Invalid(format!("{side} names no message of this conversation"))
    };
    let (mut read_from, after, mut cursor_at) = match cursor {
        Cursor::Newest => (None, None, None),
        Cursor::Before(id) => {
            match crate::desk_surface::message_in_chain(store, &head.id, id).map_err(internal)? {
                Some((session, entry)) => {
                    (Some((session, id.to_string())), None, Some(entry.timestamp))
                }
                None => return Err(unknown("before")),
            }
        }
        Cursor::After(id) => (None, Some(id), None),
    };
    // Newest first while gathering: past the limit, or reaching the message
    // a forward page starts after, ends it. A forward page keeps only the
    // oldest of what it passes on the way back, which is all it returns.
    let mut gathered: Vec<Said> = Vec::new();
    let mut exhausted = false;
    let mut reached = false;
    let reads = match after {
        Some(_) => usize::MAX,
        None => MAX_READS,
    };
    for _ in 0..reads {
        let read = store
            .read_thread_page(
                &head.id,
                read_from.as_ref().map(|(s, _)| s.as_str()),
                read_from.as_ref().map(|(_, id)| id.as_str()),
                ENTRIES_PER_READ,
            )
            .map_err(internal)?;
        let newer: Vec<&TranscriptEntry> = read
            .messages
            .iter()
            .rev()
            .take_while(|e| Some(e.id.as_str()) != after)
            .collect();
        reached = after.is_some() && newer.len() < read.messages.len();
        if reached {
            cursor_at = read
                .messages
                .iter()
                .rfind(|e| Some(e.id.as_str()) == after)
                .map(|e| e.timestamp);
        }
        gathered.extend(newer.into_iter().filter_map(|e| said(e, agent)));
        if after.is_some() && gathered.len() > limit + 1 {
            gathered.drain(..gathered.len() - (limit + 1));
        }
        let oldest = read.messages.first();
        exhausted = !read.has_more || oldest.is_none();
        if reached || exhausted || (after.is_none() && gathered.len() > limit) {
            break;
        }
        read_from = oldest.map(|e| (e.session_id.clone(), e.id.clone()));
    }
    if after.is_some() && !reached {
        return Err(unknown("after"));
    }
    gathered.reverse();
    let more = match after {
        Some(_) => gathered.len() > limit,
        None => gathered.len() > limit || !exhausted,
    };
    let messages: Vec<Said> = match after {
        Some(_) => gathered.into_iter().take(limit).collect(),
        None => gathered.split_off(gathered.len().saturating_sub(limit)),
    };
    let bound = |m: Option<&Said>| m.filter(|_| more).map(|m| m.at);
    let span = match after {
        Some(_) => Span {
            after: cursor_at,
            before: bound(messages.last()),
        },
        None => Span {
            after: bound(messages.first()),
            before: cursor_at,
        },
    };
    Ok((Page { messages, more }, span))
}

/// [`read`]'s page alone.
pub(crate) fn page(
    store: &dyn SessionStore,
    agent: &Speaker,
    owner: &str,
    channel: &str,
    cursor: Cursor<'_>,
    limit: usize,
) -> Result<Page, ConversationError> {
    read(store, agent, owner, channel, cursor, limit).map(|(page, _)| page)
}

/// What the agent's messages on `page` mention to act on in place, as the
/// desk kept it.
pub(crate) async fn with_extras(desk: &workforce_desk::Desk, page: &mut Page) {
    let ids: Vec<String> = page
        .messages
        .iter()
        .filter(|m| m.from == Author::Agent)
        .map(|m| m.id.clone())
        .collect();
    match desk.store().extras(&ids).await {
        Ok(mut extras) => page
            .messages
            .iter_mut()
            .for_each(|m| m.desk = extras.remove(&m.id)),
        Err(e) => tracing::warn!(error = %e, "reading the extras of a DM failed"),
    }
}

/// Who said the person's messages on `page` that they didn't type, as the
/// desk kept it: the overseer's nudges and tasks, and suggestions said yes
/// to.
pub(crate) async fn with_authors(desk: &workforce_desk::Desk, page: &mut Page) {
    let ids: Vec<String> = page
        .messages
        .iter()
        .filter(|m| m.from == Author::User)
        .map(|m| m.id.clone())
        .collect();
    match desk.store().authors(&ids).await {
        Ok(mut authors) => page.messages.iter_mut().for_each(|m| {
            if let Some(author) = authors.remove(&m.id) {
                m.author = author;
            }
        }),
        Err(e) => tracing::warn!(error = %e, "reading the authors of a DM failed"),
    }
}

/// The Desk conversation routes on `state`'s agents.
pub fn router(state: Arc<GatewayState>) -> axum::Router {
    workforce_desk::conversations::router(Arc::new(HubConversations(state)))
}

/// How many of a person's sessions with one agent are looked through for
/// the Desk conversations that changed.
const ACTIVE_SEARCHED: usize = 1_000;

/// Reads a person's Desk conversations for the person a request carries: a
/// paired device (or a signed-in person) reads their own. The gateway's
/// admin token names no person, so it reads none.
pub(crate) struct HubConversations(pub(crate) Arc<GatewayState>);

impl HubConversations {
    /// The person a request is from, when they may read their
    /// conversations.
    async fn person(
        &self,
        headers: &HeaderMap,
        token: Option<&str>,
    ) -> Result<String, ConversationError> {
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
        Ok(owner)
    }

    fn agent(&self, agent: &str) -> Result<Arc<crate::serve::AgentService>, ConversationError> {
        self.0
            .registry
            .get(agent)
            .ok_or_else(|| ConversationError::NotFound(format!("no agent named {agent:?}")))
    }
}

#[async_trait]
impl Conversations for HubConversations {
    async fn page(
        &self,
        headers: &HeaderMap,
        token: Option<&str>,
        agent: &str,
        channel: &str,
        cursor: Cursor<'_>,
        limit: usize,
    ) -> Result<Page, ConversationError> {
        let owner = self.person(headers, token).await?;
        let service = self.agent(agent)?;
        let speaker = Speaker::agent(
            agent,
            &service.display_name().unwrap_or_else(|| agent.to_string()),
        );
        let (mut page, span) = read(
            service.sessions.as_ref(),
            &speaker,
            &owner,
            channel,
            cursor,
            limit,
        )?;
        if let Some(crew) = crate::desk_crew::for_state(&self.0) {
            with_extras(crew.desk(), &mut page).await;
            with_authors(crew.desk(), &mut page).await;
            crate::desk_attachments::with_attachments(
                crew.desk(),
                &speaker,
                channel,
                &owner,
                &mut page,
                span,
            )
            .await;
        }
        Ok(page)
    }

    async fn active(
        &self,
        headers: &HeaderMap,
        token: Option<&str>,
        since: Option<DateTime<Utc>>,
    ) -> Result<Vec<Active>, ConversationError> {
        let owner = self.person(headers, token).await?;
        let registry = &self.0.registry;
        let mut active = registry
            .agent_ids()
            .iter()
            .filter_map(|id| registry.get(id.as_str()).map(|service| (id, service)))
            .map(|(id, service)| active_of(service.sessions.as_ref(), id.as_str(), &owner, since))
            .collect::<eyre::Result<Vec<_>>>()
            .map_err(|e| ConversationError::Internal(format!("{e:#}")))?
            .into_iter()
            .flatten()
            .collect::<Vec<_>>();
        active.sort_by_key(|a| a.updated_at);
        Ok(active)
    }

    async fn close(
        &self,
        headers: &HeaderMap,
        token: Option<&str>,
        agent: &str,
        close: Close,
    ) -> Result<Closed, ConversationError> {
        let owner = self.person(headers, token).await?;
        let service = self.agent(agent)?;
        if !crate::desk_surface::is_room(&close.channel) {
            return Err(ConversationError::Invalid(format!(
                "{:?} is not a Desk room's conversation",
                close.channel
            )));
        }
        crate::desk_surface::close(&service, &owner, &close.channel, &close.undelivered)
            .await
            .map(|copied| Closed { copied })
            .map_err(|e| ConversationError::Internal(format!("{e:#}")))
    }
}

/// The Desk conversations `owner` holds with `agent` in `store` whose head
/// changed after `since`.
fn active_of(
    store: &dyn SessionStore,
    agent: &str,
    owner: &str,
    since: Option<DateTime<Utc>>,
) -> eyre::Result<Vec<Active>> {
    Ok(store
        .list_for_owner_on(owner, DESK_CHANNEL, ACTIVE_SEARCHED)?
        .into_iter()
        .take_while(|s| since.is_none_or(|since| s.updated_at > since))
        .filter(|s| is_desk_conversation(&s.channel) && !s.is_sealed())
        .map(|s| Active {
            agent_id: agent.to_string(),
            channel: s.channel,
            updated_at: s.updated_at,
        })
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    use workforce_common::session::{InMemorySessionStore, JsonlSessionStore, SessionMessage};

    const ME: &str = "identity-1";

    fn scout() -> Speaker {
        Speaker::agent("scout", "Scout")
    }

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
        let page = page(&store, &scout(), ME, DESK_CHANNEL, Cursor::Newest, 200).unwrap();
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
        assert_eq!(page.messages[0].author, Speaker::you());
        assert_eq!(page.messages[1].author, scout());
        assert_eq!(page.messages[0].id, session.messages[1].id);
        assert_eq!(page.messages[0].at, session.messages[1].timestamp);
    }

    #[test]
    fn nobody_elses_conversation_and_no_conversation_read_as_empty_and_create_nothing() {
        let store = InMemorySessionStore::new();
        exchange(&store, 0);
        assert_eq!(
            page(
                &store,
                &scout(),
                "identity-2",
                DESK_CHANNEL,
                Cursor::Newest,
                200
            )
            .unwrap(),
            Page::default()
        );
        assert!(store.find(DESK_CHANNEL, "identity-2").unwrap().is_none());
        let mut room = store.get_or_create("desk:release", ME, "scout").unwrap();
        room.add_user_message("in the room", None);
        store.save(&room).unwrap();
        let mine = page(&store, &scout(), ME, DESK_CHANNEL, Cursor::Newest, 200).unwrap();
        assert_eq!(mine.messages.len(), 2, "a room is not the DM");
    }

    #[test]
    fn pages_walk_back_through_before_until_nothing_more_remains() {
        let store = InMemorySessionStore::new();
        for n in 0..5 {
            exchange(&store, n);
        }
        let newest = page(&store, &scout(), ME, DESK_CHANNEL, Cursor::Newest, 4).unwrap();
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
        let older = page(
            &store,
            &scout(),
            ME,
            DESK_CHANNEL,
            Cursor::Before(&newest.messages[0].id),
            4,
        )
        .unwrap();
        assert_eq!(older.messages[0].text, "question 1");
        assert_eq!(older.messages[3].text, "answer 2");
        assert!(older.more);
        let oldest = page(
            &store,
            &scout(),
            ME,
            DESK_CHANNEL,
            Cursor::Before(&older.messages[0].id),
            4,
        )
        .unwrap();
        assert_eq!(
            texts(&oldest),
            [("question 0", Author::User), ("answer 0", Author::Agent)]
        );
        assert!(!oldest.more);
        let refused = page(
            &store,
            &scout(),
            ME,
            DESK_CHANNEL,
            Cursor::Before("not-a-message"),
            4,
        )
        .unwrap_err();
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
        let text = &page(&store, &scout(), ME, DESK_CHANNEL, Cursor::Newest, 10)
            .unwrap()
            .messages[0]
            .text;
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
                    let newest =
                        page(store, &scout(), ME, DESK_CHANNEL, Cursor::Newest, 4).unwrap();
                    assert_eq!(newest.messages[0].text, "question 1");
                    assert!(newest.more);
                    let oldest = page(
                        store,
                        &scout(),
                        ME,
                        DESK_CHANNEL,
                        Cursor::Before(&newest.messages[0].id),
                        4,
                    )
                    .unwrap();
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

    /// A room's conversation pages forward after a message Desk already
    /// placed, across a rollover, and each message carries its place.
    /// Each page covers the time between its neighbours, so the files that
    /// are messages of their own are each read on exactly one page.
    #[test]
    fn a_page_spans_the_time_between_its_neighbours() {
        let store = InMemorySessionStore::new();
        for n in 0..5 {
            exchange(&store, n);
        }
        let read_at = |cursor| read(&store, &scout(), ME, DESK_CHANNEL, cursor, 4).unwrap();
        let (newest, span) = read_at(Cursor::Newest);
        assert_eq!(
            span,
            Span {
                after: Some(newest.messages[0].at),
                before: None
            }
        );
        let (older, span) = read_at(Cursor::Before(&newest.messages[0].id));
        assert_eq!(
            span,
            Span {
                after: Some(older.messages[0].at),
                before: Some(newest.messages[0].at)
            }
        );
        let (oldest, span) = read_at(Cursor::Before(&older.messages[0].id));
        assert_eq!(
            span.after, None,
            "the oldest page reaches back to the start"
        );
        let (forward, span) = read_at(Cursor::After(&oldest.messages[0].id));
        assert!(forward.more);
        assert_eq!(
            span,
            Span {
                after: Some(oldest.messages[0].at),
                before: Some(forward.messages[3].at)
            }
        );
    }

    /// A message's files ride on it as its attachments, not in its mark.
    #[test]
    fn a_message_s_files_are_its_attachments() {
        use workforce_common::channels::desk::{DeliveryKind, DeskFile, DeskMark};
        let store = InMemorySessionStore::new();
        let mut session = store.get_or_create(DESK_CHANNEL, ME, "scout").unwrap();
        session.add_assistant_message("your report", 3);
        let report = DeskFile {
            artifact_id: "art_1".into(),
            agent_id: "scout".into(),
            filename: "report.pdf".into(),
            media_type: "application/pdf".into(),
            size: 9,
            width: None,
            height: None,
            caption: None,
        };
        session.messages.last_mut().unwrap().desk = Some(DeskMark {
            delivery: Some(DeliveryKind::File),
            files: vec![report.clone()],
            ..DeskMark::default()
        });
        store.save(&session).unwrap();
        session.add_assistant_message("", 0);
        session.messages.last_mut().unwrap().desk = Some(DeskMark {
            delivery: Some(DeliveryKind::File),
            files: vec![report.clone()],
            ..DeskMark::default()
        });
        session.add_assistant_message("", 0);
        store.save(&session).unwrap();
        let read = page(&store, &scout(), ME, DESK_CHANNEL, Cursor::Newest, 10).unwrap();
        assert_eq!(
            read.messages.len(),
            2,
            "an uncaptioned file is a message, an empty one isn't"
        );
        assert_eq!(
            (
                read.messages[1].text.as_str(),
                read.messages[1].attachments.len()
            ),
            ("", 1)
        );
        let said = &read.messages[0];
        assert_eq!(said.attachments, vec![(&report).into()]);
        assert_eq!(
            said.mark,
            Some(DeskMark {
                delivery: Some(DeliveryKind::File),
                ..DeskMark::default()
            })
        );
    }

    /// A Desk closed while a conversation ran on reads forward from where it
    /// was, however far back that is: nothing between is skipped.
    #[test]
    fn a_forward_page_from_far_back_starts_right_after_its_cursor() {
        let store = InMemorySessionStore::new();
        let exchanges = MAX_READS * ENTRIES_PER_READ / 3 + 50;
        let mut session = store.get_or_create(DESK_CHANNEL, ME, "scout").unwrap();
        for n in 0..exchanges {
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
        }
        store.save(&session).unwrap();
        let first = session.messages[2].id.clone();
        let next = page(&store, &scout(), ME, DESK_CHANNEL, Cursor::After(&first), 2).unwrap();
        assert_eq!(
            texts(&next),
            [("question 1", Author::User), ("answer 1", Author::Agent)]
        );
        assert!(next.more);
    }

    #[test]
    fn a_room_reads_forward_after_a_message_across_the_seam() {
        use workforce_common::channels::desk::{DeliveryKind, DeskMark};
        let room = "desk:planning";
        let dir = tempfile::tempdir().unwrap();
        let store = JsonlSessionStore::new(dir.path()).unwrap();
        let said_in_room = |store: &JsonlSessionStore, text: &str, mark: Option<DeskMark>| {
            let mut session = store.get_or_create(room, ME, "scout").unwrap();
            session.add_assistant_message(text, 1);
            session.messages.last_mut().unwrap().desk = mark;
            store.save(&session).unwrap();
        };
        said_in_room(&store, "first", None);
        let old = store.find(room, ME).unwrap().unwrap();
        let successor = workforce_agent::rollover::roll_if_stale(
            &store,
            old.clone(),
            &workforce_common::session::CompactionConfig::default(),
            chrono::Utc::now() + chrono::Duration::days(3),
        )
        .unwrap();
        assert_ne!(successor.id, old.id, "the room's conversation rolled over");
        let reminder = DeskMark {
            delivery: Some(DeliveryKind::Reminder),
            ..DeskMark::default()
        };
        said_in_room(&store, "second", Some(reminder.clone()));
        said_in_room(&store, "third", None);

        let all = page(&store, &scout(), ME, room, Cursor::Newest, 200).unwrap();
        let texts: Vec<&str> = all.messages.iter().map(|m| m.text.as_str()).collect();
        assert_eq!(texts, ["first", "second", "third"]);
        assert_eq!(all.messages[1].mark, Some(reminder));

        let after_first = page(
            &store,
            &scout(),
            ME,
            room,
            Cursor::After(&all.messages[0].id),
            1,
        )
        .unwrap();
        assert_eq!(after_first.messages[0].text, "second");
        assert!(after_first.more, "the third is newer still");
        let caught_up = page(
            &store,
            &scout(),
            ME,
            room,
            Cursor::After(&all.messages[2].id),
            10,
        )
        .unwrap();
        assert_eq!(caught_up, Page::default());
        assert!(matches!(
            page(&store, &scout(), ME, room, Cursor::After("nope"), 10),
            Err(ConversationError::Invalid(_))
        ));
        assert!(matches!(
            page(&store, &scout(), ME, "ios", Cursor::Newest, 10),
            Err(ConversationError::Invalid(_))
        ));
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
        let newest = page(&store, &scout(), ME, DESK_CHANNEL, Cursor::Newest, 200).unwrap();
        assert_eq!(
            texts(&newest),
            [
                ("question 0", Author::User),
                ("answer 0", Author::Agent),
                ("question 1", Author::User),
                ("answer 1", Author::Agent),
            ]
        );
        let older = page(
            &store,
            &scout(),
            ME,
            DESK_CHANNEL,
            Cursor::Before(&newest.messages[2].id),
            200,
        )
        .unwrap();
        assert_eq!(older.messages.len(), 2, "the cursor crosses the seam");
    }
}
