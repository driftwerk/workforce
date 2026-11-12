//! A Desk conversation read back: the DM or a room's conversation one person
//! keeps with one agent, from the session store the agent itself reads.
//!
//! The conversation is the session a `/ws` connection on its Desk channel
//! anchors on for its person (`identity_id`, else the paired device) and
//! agent: one active session per person, channel and agent, in the agent's
//! own store, so a reconnect, a Desk restart or a hub restart lands on the
//! same one. A conversation left a day goes on in a successor holding a
//! carry-over; the read walks the whole chain, so it reads as one.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use async_trait::async_trait;
use axum::http::HeaderMap;
use chrono::{DateTime, Utc};
use workforce_agent::rollover::CARRYOVER_PREFIX;
use workforce_common::channels::{is_desk_conversation, DESK_CHANNEL};
use workforce_common::session::{
    MessageRole, Session, SessionAttachment, SessionStore, TranscriptEntry, UnknownCursor,
};
use workforce_desk::attachments::{Attachment, MAX_PER_MESSAGE};
use workforce_desk::{
    Active, Author, Close, Closed, ConversationError, ConversationRead, Conversations, Cursor,
    Match, Matches, Page, PutRead, Said, Speaker,
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

/// The files a person's messages hold on their sessions, read once per
/// session of the conversation's chain. A person's upload is on their message
/// whether or not this hub keeps a desk, and a message saved before its mark
/// carried its files has it nowhere else.
struct HeldFiles<'a> {
    store: &'a dyn SessionStore,
    sessions: HashMap<String, HashMap<String, Vec<SessionAttachment>>>,
    /// The ids of those taken from here: only their clients have named them.
    named: HashSet<String>,
}

impl<'a> HeldFiles<'a> {
    fn of(store: &'a dyn SessionStore) -> Self {
        Self {
            store,
            sessions: HashMap::new(),
            named: HashSet::new(),
        }
    }

    /// The files `entry` holds, as named by its client: [`described`] says
    /// what the store knows of them. One that names no artifact has no
    /// address to be fetched from, and what it holds inline is not for a
    /// conversation's read, so it isn't one.
    ///
    /// [`described`]: crate::desk_attachments::described
    fn on(&mut self, entry: &TranscriptEntry) -> Vec<Attachment> {
        let store = self.store;
        let session = self
            .sessions
            .entry(entry.session_id.clone())
            .or_insert_with(|| {
                store.message_files(&entry.session_id).unwrap_or_else(|e| {
                    tracing::warn!(error = %e, "reading the files of a DM's messages failed");
                    HashMap::new()
                })
            });
        session
            .get(&entry.id)
            .into_iter()
            .flatten()
            .filter_map(crate::desk_attachments::named)
            .take(MAX_PER_MESSAGE)
            .collect()
    }
}

/// What a transcript entry shows in the conversation, if it is one of its
/// messages: said by the person or by `agent`, not tool chatter, a system
/// prompt or a note the hub left for the model; a file sent or delivered
/// without a caption is one, with no text. The text is what the live channel
/// sends: secret markers as previews, credentials redacted; its attachments
/// are the files its mark names and, for a person's, those their message
/// holds that it doesn't.
/// A person's message is theirs until [`with_authors`] says otherwise.
fn said(
    entry: &TranscriptEntry,
    agent: &Speaker,
    held: Option<&mut HeldFiles<'_>>,
) -> Option<Said> {
    let (from, author) = match entry.role {
        MessageRole::User => (Author::User, Speaker::you()),
        MessageRole::Assistant => (Author::Agent, agent.clone()),
        MessageRole::System | MessageRole::Tool => return None,
    };
    let trimmed = entry.content.trim();
    if entry.tool_metadata.is_some() || HUB_NOTES.iter().any(|note| trimmed.starts_with(note)) {
        return None;
    }
    let mut mark = entry.desk.clone();
    let mut attachments: Vec<Attachment> = mark
        .as_mut()
        .map(|mark| std::mem::take(&mut mark.files))
        .unwrap_or_default()
        .iter()
        .map(Into::into)
        .collect();
    // A mark may carry only some of them: one whose lookup failed when it was
    // stamped is still on the message.
    if let (Author::User, Some(held)) = (from, held) {
        let room = MAX_PER_MESSAGE.saturating_sub(attachments.len());
        let others: Vec<Attachment> = held
            .on(entry)
            .into_iter()
            .filter(|file| {
                !attachments
                    .iter()
                    .any(|a| a.artifact_id == file.artifact_id)
            })
            .take(room)
            .collect();
        held.named
            .extend(others.iter().map(|file| file.artifact_id.clone()));
        attachments.extend(others);
    }
    if trimmed.is_empty() && attachments.is_empty() {
        return None;
    }
    Some(Said {
        id: entry.id.clone(),
        from,
        author,
        text: shown(&entry.content),
        at: entry.timestamp,
        desk: None,
        mark,
        attachments,
        client_id: entry.external_id.clone(),
        seq: entry.seq,
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

/// `owner`'s conversation on the Desk `channel` with `agent` in `store`, as
/// [`read`] has it, and the person's files their messages hold: what a person
/// is shown. Looks those up once per session of the chain, which [`read`]
/// spares the reads that run on every listing. Also the ids of those files,
/// which only their clients have named so far (see
/// [`described`](crate::desk_attachments::described)).
pub(crate) fn read_shown(
    store: &dyn SessionStore,
    agent: &Speaker,
    owner: &str,
    channel: &str,
    cursor: Cursor<'_>,
    limit: usize,
) -> Result<(Page, Span, HashSet<String>), ConversationError> {
    let mut held = HeldFiles::of(store);
    let (page, span) = read_with(store, agent, owner, channel, cursor, limit, Some(&mut held))?;
    Ok((page, span, held.named))
}

/// `owner`'s conversation on the Desk `channel` with `agent` in `store`: at
/// most `limit` messages at `cursor`, oldest first, and the time they
/// cover. Reads only: it never creates the session or rolls it over. A
/// person's files are those their marks carry.
pub(crate) fn read(
    store: &dyn SessionStore,
    agent: &Speaker,
    owner: &str,
    channel: &str,
    cursor: Cursor<'_>,
    limit: usize,
) -> Result<(Page, Span), ConversationError> {
    read_with(store, agent, owner, channel, cursor, limit, None)
}

fn read_with(
    store: &dyn SessionStore,
    agent: &Speaker,
    owner: &str,
    channel: &str,
    cursor: Cursor<'_>,
    limit: usize,
    mut held: Option<&mut HeldFiles<'_>>,
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
        Cursor::Around(id) => {
            return around(store, agent, owner, channel, &head, id, limit, held);
        }
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
        gathered.extend(
            newer
                .into_iter()
                .filter_map(|e| said(e, agent, held.as_deref_mut()))
                .map(|mut said| {
                    if said.from == Author::Agent {
                        said.text = crate::spoken_text::kept_text(&head, &said.text).into_owned();
                    }
                    said
                }),
        );
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
    // Each message once, where it was first said: a successor not yet
    // archived reads from its live transcript, which carries the turns its
    // rollover kept verbatim under their own ids, already read from the
    // predecessor's archive.
    let mut seen = HashSet::new();
    gathered.retain(|said| seen.insert(said.id.clone()));
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
    Ok((
        Page {
            messages,
            more,
            more_after: None,
        },
        span,
    ))
}

/// A page with the message `id` in the middle: half of `limit` before it, it,
/// and the rest after; where one side runs out, the other fills the page.
/// Read as a page before it and a page after it are, so it walks the chain's
/// seams as they do. `more` says older messages remain, `more_after` newer.
#[allow(clippy::too_many_arguments)]
fn around(
    store: &dyn SessionStore,
    agent: &Speaker,
    owner: &str,
    channel: &str,
    head: &Session,
    id: &str,
    limit: usize,
    mut held: Option<&mut HeldFiles<'_>>,
) -> Result<(Page, Span), ConversationError> {
    let internal = |e: eyre::Report| ConversationError::Internal(format!("{e:#}"));
    let unknown =
        || ConversationError::Invalid("around names no message of this conversation".into());
    let entry = crate::desk_surface::message_in_chain(store, &head.id, id)
        .map_err(internal)?
        .ok_or_else(unknown)?
        .1;
    let mut middle = said(&entry, agent, held.as_deref_mut()).ok_or_else(unknown)?;
    if middle.from == Author::Agent {
        middle.text = crate::spoken_text::kept_text(head, &middle.text).into_owned();
    }
    let mut side = |cursor, limit| {
        read_with(
            store,
            agent,
            owner,
            channel,
            cursor,
            limit,
            held.as_deref_mut(),
        )
    };
    let (mut older, mut older_span) = side(Cursor::Before(id), limit / 2)?;
    let (newer, newer_span) = side(
        Cursor::After(id),
        limit.saturating_sub(older.messages.len() + 1),
    )?;
    let room = limit.saturating_sub(newer.messages.len() + 1);
    if !newer.more && older.more && older.messages.len() < room {
        (older, older_span) = side(Cursor::Before(id), room)?;
    }
    let span = Span {
        after: older_span.after.or_else(|| older.more.then_some(middle.at)),
        before: newer_span
            .before
            .or_else(|| newer.more.then_some(middle.at)),
    };
    let mut messages = older.messages;
    messages.push(middle);
    messages.extend(newer.messages);
    let mut seen = HashSet::new();
    messages.retain(|said| seen.insert(said.id.clone()));
    Ok((
        Page {
            messages,
            more: older.more,
            more_after: Some(newer.more),
        },
        span,
    ))
}

/// How many characters of a match's text its excerpt holds, and how many of
/// them come before the words it matched.
const EXCERPT_CHARS: usize = 160;
const EXCERPT_LEAD: usize = 40;

/// The sessions of the conversation whose newest is `head`, newest first,
/// as [`SessionStore::read_thread_page`] walks them: back through each
/// predecessor until one is gone.
fn chain_of(store: &dyn SessionStore, head: &Session) -> eyre::Result<Vec<String>> {
    let mut chain = vec![head.id.clone()];
    let mut prev = head.prev_session_id().map(str::to_string);
    while let Some(id) = prev.take() {
        if chain.len() >= 50 || chain.contains(&id) {
            break;
        }
        let Some(session) = store.get(&id)? else {
            break;
        };
        prev = session.prev_session_id().map(str::to_string);
        chain.push(id);
    }
    Ok(chain)
}

/// The messages of `owner`'s DM with `agent` in `store`, its whole chain, that
/// match `query`, newest first: at most `limit`, older than the message
/// `before` when it is named. Only what the conversation shows is a match,
/// and its excerpt is cut from the text it shows. It reads on until the page
/// is full or the chain is, so `more` always has the last match to go on
/// from.
pub(crate) fn search(
    store: &dyn SessionStore,
    agent: &Speaker,
    owner: &str,
    query: &str,
    before: Option<&str>,
    limit: usize,
) -> Result<Matches, ConversationError> {
    let internal = |e: eyre::Report| ConversationError::Internal(format!("{e:#}"));
    let Some(head) = store.find(DESK_CHANNEL, owner).map_err(internal)? else {
        return Ok(Matches::default());
    };
    // A cursor the store's search can't place, a message of another
    // conversation or one it doesn't read, is the caller's mistake.
    let refused = |e: eyre::Report| match e.downcast_ref::<UnknownCursor>() {
        Some(_) => {
            ConversationError::Invalid("before names no message of this conversation".into())
        }
        None => internal(e),
    };
    let chain = chain_of(store, &head).map_err(internal)?;
    let fetch = limit + 1;
    let mut cursor = before.map(str::to_string);
    let mut seen = HashSet::new();
    let mut matches = Vec::new();
    // Each read starts strictly older than the last one ended, so the reads
    // run out with the chain; one that doesn't move on ends them.
    loop {
        let found = store
            .search_archive(&chain, query, fetch, cursor.as_deref())
            .map_err(refused)?;
        let ended = found.len() < fetch;
        for entry in &found {
            if seen.insert(entry.id.clone()) {
                matches.extend(matched(entry, agent, &head, query));
            }
        }
        match found.last() {
            Some(last)
                if !ended
                    && matches.len() <= limit
                    && cursor.as_deref() != Some(last.id.as_str()) =>
            {
                cursor = Some(last.id.clone())
            }
            _ => break,
        }
    }
    let more = matches.len() > limit;
    matches.truncate(limit);
    Ok(Matches { matches, more })
}

/// `entry` as a match of `query`, when the conversation shows it. Where
/// what it shows differs from what was kept (a secret redacted), the words
/// must be in what it shows, so a search can't find what the page hides.
fn matched(entry: &TranscriptEntry, agent: &Speaker, head: &Session, query: &str) -> Option<Match> {
    let said = said(entry, agent, None)?;
    let text = match said.from {
        Author::Agent => crate::spoken_text::kept_text(head, &said.text).into_owned(),
        Author::User => said.text,
    };
    if text != entry.content {
        let shown = text.to_lowercase();
        if !query
            .split_whitespace()
            .all(|word| shown.contains(&word.to_lowercase()))
        {
            return None;
        }
    }
    Some(Match {
        id: said.id,
        from: said.from,
        at: said.at,
        excerpt: excerpt(&text, query),
    })
}

/// A few words of `text` on one line, around the first of `query`'s words it
/// holds (their letters and digits, so `deploy,` finds `deploy`), or from
/// its start; `…` marks what was cut.
fn excerpt(text: &str, query: &str) -> String {
    let line = text.split_whitespace().collect::<Vec<_>>().join(" ");
    let lower = line.to_lowercase();
    let hit = query
        .split(|c: char| !c.is_alphanumeric())
        .filter(|word| !word.is_empty())
        .filter_map(|word| lower.find(&word.to_lowercase()))
        .min()
        .map(|at| lower[..at].chars().count())
        .unwrap_or(0);
    let chars: Vec<char> = line.chars().collect();
    let hit = hit.min(chars.len());
    let mut start = hit.saturating_sub(EXCERPT_LEAD);
    // Not mid-word, where a space is near.
    if start > 0 {
        if let Some(space) = chars[start..hit].iter().position(|c| *c == ' ') {
            start += space + 1;
        }
    }
    let end = (start + EXCERPT_CHARS).min(chars.len());
    let mut out = String::new();
    if start > 0 {
        out.push('…');
    }
    out.extend(&chars[start..end]);
    if end < chars.len() {
        out.push('…');
    }
    out
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
        let (mut page, span, mut held) = read_shown(
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
                &mut held,
            )
            .await;
        }
        crate::desk_attachments::described(
            Arc::clone(&service.sessions),
            service.agent_id.as_str(),
            &owner,
            &mut page,
            &held,
        )
        .await;
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

    /// Only the caller's own DM with `agent` is searched: the person comes
    /// from the request, never from what it asks for.
    async fn search(
        &self,
        headers: &HeaderMap,
        token: Option<&str>,
        agent: &str,
        query: &str,
        before: Option<&str>,
        limit: usize,
    ) -> Result<Matches, ConversationError> {
        let owner = self.person(headers, token).await?;
        let service = self.agent(agent)?;
        let speaker = Speaker::agent(
            agent,
            &service.display_name().unwrap_or_else(|| agent.to_string()),
        );
        search(
            service.sessions.as_ref(),
            &speaker,
            &owner,
            query,
            before,
            limit,
        )
    }

    /// The point is the caller's own, so no one moves another's; every
    /// other Desk of theirs is told it moved.
    async fn put_read(
        &self,
        headers: &HeaderMap,
        token: Option<&str>,
        agent: &str,
        put: PutRead,
    ) -> Result<ConversationRead, ConversationError> {
        let owner = self.person(headers, token).await?;
        let service = self.agent(agent)?;
        if !is_desk_conversation(&put.channel) {
            return Err(ConversationError::Invalid(format!(
                "{:?} is not a Desk conversation",
                put.channel
            )));
        }
        let store = service.sessions.as_ref();
        let unkept = |e: eyre::Report| ConversationError::Unavailable(format!("{e:#}"));
        // Never later than now: a point from a clock running ahead would
        // count everything until then as read.
        let point = workforce_common::channels::desk::ReadPoint {
            at: put.at.min(Utc::now()),
            message: put.message,
        };
        let moved = store
            .put_desk_read(&owner, &put.channel, &point, put.mark)
            .map_err(unkept)?;
        if moved.is_some() {
            if let Some(push) = crate::session_push::installed() {
                push.read_changed(&owner, agent, &put.channel).await;
            }
        }
        store
            .desk_read(&owner, &put.channel)
            .map_err(unkept)?
            .map(|p| ConversationRead::of(&p))
            .ok_or_else(|| ConversationError::Internal("the read point was not kept".into()))
    }
}

/// The Desk conversations `owner` holds with `agent` in `store` whose head
/// changed after `since`, and their DM whenever it has something unread, so
/// a Desk that starts cold gets every count. The DM says how far it is read
/// on a hub that keeps read points; a count that can't be read leaves it
/// listed as before. The person's floor is set at their first listing,
/// whether or not they have a DM yet, so a delivery that starts one later
/// is unread.
fn active_of(
    store: &dyn SessionStore,
    agent: &str,
    owner: &str,
    since: Option<DateTime<Utc>>,
) -> eyre::Result<Vec<Active>> {
    let floor = store.desk_read_floor(owner).unwrap_or_else(|e| {
        tracing::warn!(agent, error = %e, "reading where a person's DM reads begin failed");
        None
    });
    let mut active = Vec::new();
    for s in store
        .list_for_owner_on(owner, DESK_CHANNEL, ACTIVE_SEARCHED)?
        .into_iter()
        .filter(|s| is_desk_conversation(&s.channel) && !s.is_sealed())
    {
        let changed = since.is_none_or(|since| s.updated_at > since);
        let (read, unread) = match (s.channel == DESK_CHANNEL, floor) {
            (true, Some(floor)) => match crate::desk_reads::dm_unread(store, agent, owner, floor) {
                Ok((read, unread)) => (read, Some(unread)),
                Err(e) => {
                    tracing::warn!(agent, error = %e, "counting a DM's unread failed");
                    (None, None)
                }
            },
            _ => (None, None),
        };
        if changed || unread.as_ref().is_some_and(|u| u.count > 0) {
            active.push(Active {
                agent_id: agent.to_string(),
                channel: s.channel,
                updated_at: s.updated_at,
                read,
                unread,
            });
        }
    }
    Ok(active)
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
    /// The DM's listing says what is unread after the person's point, and
    /// once they read it, nothing; a hub whose sessions keep no points says
    /// neither, and a room's conversation never.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_dm_is_listed_with_its_unread_until_its_point_passes_it() {
        use workforce_common::channels::desk::{Mark, ReadPoint};
        use workforce_common::session_postgres::PostgresSessionStore;
        workforce_postgres::test_support::with_test_pool(
            PostgresSessionStore::build_pool,
            |pool| async move {
                let store = PostgresSessionStore::with_pool(pool, "scout")
                    .await
                    .expect("store");
                assert!(
                    active_of(store.as_ref(), "scout", ME, None)
                        .unwrap()
                        .is_empty(),
                    "no DM yet, and the first listing sets where reads begin"
                );
                tokio::time::sleep(std::time::Duration::from_millis(5)).await;
                exchange(store.as_ref(), 0);
                exchange(store.as_ref(), 1);
                let listed = active_of(store.as_ref(), "scout", ME, None).unwrap();
                let dm = listed.iter().find(|a| a.channel == DESK_CHANNEL).unwrap();
                let unread = dm.unread.clone().unwrap();
                assert_eq!(
                    (unread.count, unread.for_you, dm.read.clone()),
                    (2, 2, None)
                );
                let last = unread.last.unwrap();
                store
                    .put_desk_read(
                        ME,
                        DESK_CHANNEL,
                        &ReadPoint {
                            at: last.at,
                            message: last.message_id.clone(),
                        },
                        Mark::Advance,
                    )
                    .unwrap();
                let later = Utc::now();
                let listed = active_of(store.as_ref(), "scout", ME, Some(later)).unwrap();
                assert!(listed.is_empty(), "read, and unchanged since: not listed");
                let listed = active_of(store.as_ref(), "scout", ME, None).unwrap();
                let dm = &listed[0];
                assert_eq!(dm.unread.as_ref().unwrap().count, 0);
                assert_eq!(dm.read.as_ref().unwrap().message_id, last.message_id);
                assert_eq!(
                    serde_json::to_value(dm).unwrap()["unread"]["last"]["message_id"],
                    serde_json::json!(last.message_id)
                );
            },
        )
        .await;
        let memory = InMemorySessionStore::new();
        exchange(&memory, 0);
        let listed = active_of(&memory, "scout", ME, None).unwrap();
        assert_eq!(
            (listed[0].read.clone(), listed[0].unread.clone()),
            (None, None)
        );
    }

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

    /// On postgres every kept row reads with its place in the archive, in
    /// the order they were kept, and what the person sent with the id their
    /// client gave it; a turn's failure reads as the agent's, marked failed
    /// and pointing at the question it failed on.
    #[tokio::test(flavor = "multi_thread")]
    async fn on_postgres_each_row_reads_with_its_client_id_and_archive_order() {
        use workforce_common::channels::desk::DeskMark;
        use workforce_common::session_postgres::PostgresSessionStore;
        workforce_postgres::test_support::with_test_pool(
            PostgresSessionStore::build_pool,
            |pool| async move {
                let store = PostgresSessionStore::with_pool(pool, "scout")
                    .await
                    .expect("store");
                let asked = uuid::Uuid::new_v4();
                let mut session = store.get_or_create(DESK_CHANNEL, ME, "scout").unwrap();
                session.add_user_message("the question", Some(&asked.to_string()));
                session.add_user_message("sent now", Some("c-2"));
                store.save(&session).unwrap();
                for message in &session.messages {
                    store.append_to_archive(&session.id, message).unwrap();
                }
                let mut failure = SessionMessage::tool(
                    "x",
                    &serde_json::json!({}),
                    &serde_json::json!(""),
                    false,
                    0,
                    None,
                );
                failure.role = MessageRole::Assistant;
                failure.tool_data = None;
                failure.content = "Agent error: no first token".into();
                failure.desk = Some(DeskMark {
                    in_reply_to: Some(asked),
                    failed: true,
                    ..DeskMark::default()
                });
                store.append_to_archive(&session.id, &failure).unwrap();

                let read = page(
                    store.as_ref(),
                    &scout(),
                    ME,
                    DESK_CHANNEL,
                    Cursor::Newest,
                    10,
                )
                .unwrap();
                let ids: Vec<Option<&str>> = read
                    .messages
                    .iter()
                    .map(|m| m.client_id.as_deref())
                    .collect();
                let asked_id = asked.to_string();
                assert_eq!(ids, [Some(asked_id.as_str()), Some("c-2"), None]);
                let seqs: Vec<i64> = read
                    .messages
                    .iter()
                    .map(|m| m.seq.expect("every kept row is numbered"))
                    .collect();
                assert!(seqs.windows(2).all(|w| w[0] < w[1]), "{seqs:?}");
                let failed = read.messages[2].mark.clone().unwrap();
                assert!(failed.failed);
                assert_eq!(failed.in_reply_to, Some(asked));
                assert_eq!(read.messages[2].from, Author::Agent);
                let wire = serde_json::to_value(&read.messages[0]).unwrap();
                assert_eq!(wire["client_id"], asked_id.as_str());
                assert!(wire["seq"].is_i64(), "{wire}");
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

    /// A person's message as the hub saved one before its mark carried its
    /// files: `attachments` of the fields a session keeps, and nothing else.
    fn held(store: &dyn SessionStore, text: &str, files: &[(&str, &str)]) {
        let mut session = store.get_or_create(DESK_CHANNEL, ME, "scout").unwrap();
        session.add_user_message_with_attachments(
            text,
            None,
            Some(
                files
                    .iter()
                    .map(|(name, id)| SessionAttachment {
                        data: "aW5saW5lIGJ5dGVz".into(),
                        ..SessionAttachment::from_artifact(*name, "image/png", *id)
                    })
                    .collect(),
            ),
        );
        session.add_assistant_message(&format!("seen {}", files.len()), 5);
        store.save(&session).unwrap();
    }

    fn shape(page: &Page) -> Vec<(&str, Vec<&str>)> {
        page.messages
            .iter()
            .map(|m| {
                (
                    m.text.as_str(),
                    m.attachments
                        .iter()
                        .map(|a| a.artifact_id.as_str())
                        .collect(),
                )
            })
            .collect()
    }

    /// The page a person is shown, newest first.
    fn shown_page(store: &dyn SessionStore, limit: usize) -> Page {
        read_shown(store, &scout(), ME, DESK_CHANNEL, Cursor::Newest, limit)
            .unwrap()
            .0
    }

    /// The read a listing runs on every poll for every agent asks nothing of
    /// a message's own files; only what a person is shown does.
    #[test]
    fn a_listing_s_read_leaves_a_message_s_own_files_alone() {
        let store = InMemorySessionStore::new();
        held(&store, "words", &[("a.png", "art_a")]);
        held(&store, "", &[("b.png", "art_b")]);
        let read = page(&store, &scout(), ME, DESK_CHANNEL, Cursor::Newest, 50).unwrap();
        assert_eq!(
            shape(&read),
            [("words", vec![]), ("seen 1", vec![]), ("seen 1", vec![])]
        );
    }

    /// The reported conversation, read from a session that has only what it
    /// always had: no mark on the person's messages, no desk store.
    fn a_person_s_files_read_back_from_what_their_messages_hold_on(store: &dyn SessionStore) {
        held(
            store,
            "two of these",
            &[("a.png", "art_a"), ("b.png", "art_b")],
        );
        held(store, "", &[("c.png", "art_c")]);
        exchange(store, 0);
        let read = shown_page(store, 50);
        assert_eq!(
            shape(&read),
            [
                ("two of these", vec!["art_a", "art_b"]),
                ("seen 2", vec![]),
                ("", vec!["art_c"]),
                ("seen 1", vec![]),
                ("question 0", vec![]),
                ("answer 0", vec![]),
            ]
        );
        let file = &read.messages[0].attachments[0];
        assert_eq!(
            (file.filename.as_str(), file.media_type.as_str(), file.size),
            ("a.png", "image/png", 0),
            "as its client named it, until the store is asked"
        );
        let body = serde_json::to_string(&read).unwrap();
        assert!(!body.contains("aW5saW5l"), "{body}");
        assert!(!body.contains("\"data\""), "{body}");
    }

    #[test]
    fn a_person_s_files_read_back_in_memory() {
        a_person_s_files_read_back_from_what_their_messages_hold_on(&InMemorySessionStore::new());
    }

    #[test]
    fn a_person_s_files_read_back_from_files() {
        let dir = tempfile::tempdir().unwrap();
        a_person_s_files_read_back_from_what_their_messages_hold_on(
            &JsonlSessionStore::new(dir.path()).unwrap(),
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_person_s_files_read_back_on_postgres_from_the_archive_too() {
        use workforce_common::session_postgres::PostgresSessionStore;
        workforce_postgres::test_support::with_test_pool(
            PostgresSessionStore::build_pool,
            |pool| async move {
                let store = PostgresSessionStore::with_pool(pool, "scout")
                    .await
                    .expect("store");
                a_person_s_files_read_back_from_what_their_messages_hold_on(store.as_ref());
                // Archived as a turn archives them: the archive holds no
                // attachments, so the page comes from it and its files from
                // the session's live messages.
                let session = store.find(DESK_CHANNEL, ME).unwrap().unwrap();
                for message in &session.messages {
                    store.append_to_archive(&session.id, message).unwrap();
                }
                let read = shown_page(store.as_ref(), 50);
                assert_eq!(read.messages.len(), 6);
                assert_eq!(shape(&read)[0].1, ["art_a", "art_b"]);
                assert_eq!(shape(&read)[2], ("", vec!["art_c"]));
            },
        )
        .await;
    }

    /// A file that names no artifact has no address, and its bytes are not
    /// for this read: it isn't shown, and a message that is only that isn't
    /// either. What a message's mark carries is all it says, once.
    #[test]
    fn inline_files_are_left_out_and_a_mark_s_files_are_not_said_twice() {
        use workforce_common::channels::desk::{DeskFile, DeskMark};
        let store = InMemorySessionStore::new();
        let mut session = store.get_or_create(DESK_CHANNEL, ME, "scout").unwrap();
        let inline = SessionAttachment {
            filename: "inline.png".into(),
            mime_type: "image/png".into(),
            data: "aW5saW5lIGJ5dGVz".into(),
            artifact_id: None,
        };
        session.add_user_message_with_attachments("", None, Some(vec![inline.clone()]));
        session.add_user_message_with_attachments("with text", None, Some(vec![inline]));
        session.add_user_message_with_attachments(
            "",
            None,
            Some(vec![SessionAttachment::from_artifact(
                "d.png",
                "image/png",
                "art_d",
            )]),
        );
        session.messages.last_mut().unwrap().desk = Some(DeskMark {
            files: vec![DeskFile {
                artifact_id: "art_d".into(),
                agent_id: "scout".into(),
                filename: "d.png".into(),
                media_type: "image/png".into(),
                size: 9,
                width: Some(2),
                height: Some(3),
                caption: None,
            }],
            ..DeskMark::default()
        });
        store.save(&session).unwrap();
        let read = shown_page(&store, 50);
        assert_eq!(shape(&read), [("with text", vec![]), ("", vec!["art_d"])],);
        assert_eq!(read.messages[1].attachments[0].size, 9);
    }

    /// A mark that carries some of a message's files doesn't hide the others:
    /// one a lookup failed on when it was stamped is still on the message.
    /// Each shows once, the mark's first, and a listing's read asks for no more
    /// than the mark.
    #[test]
    fn a_partly_stamped_mark_leaves_the_message_s_other_files_showing() {
        use workforce_common::channels::desk::{DeskFile, DeskMark};
        let store = InMemorySessionStore::new();
        held(&store, "words", &[("a.png", "art_a"), ("b.png", "art_b")]);
        let mut session = store.get_or_create(DESK_CHANNEL, ME, "scout").unwrap();
        session.messages[0].desk = Some(DeskMark {
            files: vec![DeskFile {
                artifact_id: "art_a".into(),
                agent_id: "scout".into(),
                filename: "a.png".into(),
                media_type: "image/png".into(),
                size: 9,
                width: None,
                height: None,
                caption: None,
            }],
            ..DeskMark::default()
        });
        store.save(&session).unwrap();

        let (read, _, named) =
            read_shown(&store, &scout(), ME, DESK_CHANNEL, Cursor::Newest, 50).unwrap();
        assert_eq!(shape(&read)[0], ("words", vec!["art_a", "art_b"]));
        assert_eq!(read.messages[0].attachments[0].size, 9);
        assert_eq!(named, HashSet::from(["art_b".to_string()]));
        let listed = page(&store, &scout(), ME, DESK_CHANNEL, Cursor::Newest, 50).unwrap();
        assert_eq!(shape(&listed)[0], ("words", vec!["art_a"]));
    }

    /// The files stay with their messages across a rollover: on the sealed
    /// session they were said in, and a carried-over copy doesn't say them
    /// again.
    #[test]
    fn a_person_s_files_survive_a_rollover_once_each() {
        let store = InMemorySessionStore::new();
        held(&store, "before", &[("a.png", "art_a")]);
        held(&store, "", &[("b.png", "art_b")]);
        let mut first = store.find(DESK_CHANNEL, ME).unwrap().unwrap();
        for message in &first.messages {
            store.append_to_archive(&first.id, message).unwrap();
        }
        workforce_agent::rollover::seal_and_mint(&store, &mut first, 0).unwrap();
        held(&store, "after", &[("c.png", "art_c")]);
        let read = shown_page(&store, 50);
        assert_eq!(
            shape(&read),
            [
                ("before", vec!["art_a"]),
                ("seen 1", vec![]),
                ("", vec!["art_b"]),
                ("seen 1", vec![]),
                ("after", vec!["art_c"]),
                ("seen 1", vec![]),
            ]
        );
    }

    /// A message that is only a file takes its place on a page like any
    /// other.
    #[test]
    fn an_image_alone_counts_against_a_page_like_any_message() {
        let store = InMemorySessionStore::new();
        held(&store, "", &[("a.png", "art_a")]);
        held(&store, "", &[("b.png", "art_b")]);
        let read = shown_page(&store, 3);
        assert_eq!(
            shape(&read),
            [("seen 1", vec![]), ("", vec!["art_b"]), ("seen 1", vec![])],
        );
        assert!(read.more);
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
    /// A rollover that carries its last turns verbatim leaves them in the
    /// successor under the same ids; until the successor has an archive its
    /// read falls back to that transcript, and still each message reads once.
    #[test]
    fn turns_a_rollover_carries_verbatim_read_once() {
        let dir = tempfile::tempdir().unwrap();
        let store = JsonlSessionStore::new(dir.path()).unwrap();
        exchange(&store, 0);
        let mut old = store.find(DESK_CHANNEL, ME).unwrap().unwrap();
        for message in &old.messages {
            store.append_to_archive(&old.id, message).unwrap();
        }
        let successor = workforce_agent::rollover::seal_and_mint(&store, &mut old, 3).unwrap();
        assert!(
            successor.messages.iter().any(|m| m.content == "answer 0"),
            "the successor carries the last turns verbatim"
        );
        let newest = page(&store, &scout(), ME, DESK_CHANNEL, Cursor::Newest, 200).unwrap();
        assert_eq!(
            texts(&newest),
            [("question 0", Author::User), ("answer 0", Author::Agent)]
        );
        let ids: HashSet<&str> = newest.messages.iter().map(|m| m.id.as_str()).collect();
        assert_eq!(ids.len(), newest.messages.len(), "every id once");
    }

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

    fn found(matches: &Matches) -> Vec<(&str, Author)> {
        matches
            .matches
            .iter()
            .map(|m| (m.excerpt.as_str(), m.from))
            .collect()
    }

    /// The caller's DM with this agent, five exchanges, and what a search
    /// must never find: another person's DM, the caller's room, a note the
    /// hub left the model, a tool row and a turn's failure, all naming the
    /// same words. With `archive`, every message is archived as a turn
    /// archives it.
    fn searched(store: &dyn SessionStore, archive: bool) {
        use workforce_common::channels::desk::DeskMark;
        for n in 0..5 {
            exchange(store, n);
        }
        let mut dm = store.find(DESK_CHANNEL, ME).unwrap().unwrap();
        dm.add_user_message("[System note: question 9 was compacted]", None);
        dm.messages.push(SessionMessage::tool(
            "notes",
            &serde_json::json!({"q": "question 9"}),
            &serde_json::json!("question 9"),
            true,
            1,
            None,
        ));
        dm.messages.last_mut().unwrap().content = "question 9 in a tool".into();
        dm.add_assistant_message("Agent error: question 9 failed", 0);
        dm.messages.last_mut().unwrap().desk = Some(DeskMark {
            failed: true,
            ..DeskMark::default()
        });
        store.save(&dm).unwrap();
        let mut theirs = store
            .get_or_create(DESK_CHANNEL, "identity-2", "scout")
            .unwrap();
        theirs.add_user_message("question 9 of theirs", None);
        store.save(&theirs).unwrap();
        let mut room = store.get_or_create("desk:release", ME, "scout").unwrap();
        room.add_user_message("question 9 in the room", None);
        store.save(&room).unwrap();
        if archive {
            for id in [&dm.id, &theirs.id, &room.id] {
                let session = store.get(id).unwrap().unwrap();
                for message in &session.messages {
                    store.append_to_archive(id, message).unwrap();
                }
            }
        }
    }

    fn a_search_finds_the_dm_alone_newest_first_a_page_at_a_time(store: &dyn SessionStore) {
        let look = |query: &str, before: Option<&str>, limit| {
            search(store, &scout(), ME, query, before, limit).unwrap()
        };
        assert_eq!(
            look("question 9", None, 50),
            Matches::default(),
            "nobody else's, no room, no note, tool row or failure"
        );
        let first = look("question", None, 2);
        assert_eq!(
            found(&first),
            [("question 4", Author::User), ("question 3", Author::User)]
        );
        assert!(first.more);
        let second = look("question", Some(&first.matches[1].id), 2);
        assert_eq!(
            found(&second),
            [("question 2", Author::User), ("question 1", Author::User)]
        );
        assert!(second.more);
        let last = look("question", Some(&second.matches[1].id), 2);
        assert_eq!(found(&last), [("question 0", Author::User)]);
        assert!(!last.more);
        let answers = look("ANSW", None, 50);
        assert_eq!(answers.matches.len(), 5, "part of a word, any case");
        assert!(answers.matches.iter().all(|m| m.from == Author::Agent));
        assert!(answers.matches.windows(2).all(|w| w[0].at >= w[1].at));
        // Each match's id is a cursor of the conversation's pages.
        let around = page(
            store,
            &scout(),
            ME,
            DESK_CHANNEL,
            Cursor::Around(&first.matches[0].id),
            3,
        )
        .unwrap();
        assert_eq!(around.messages[1].text, "question 4");
        let refused = search(store, &scout(), ME, "question", Some("not-a-message"), 2);
        assert!(matches!(refused, Err(ConversationError::Invalid(_))));
        assert_eq!(
            search(store, &scout(), "identity-3", "question", None, 2).unwrap(),
            Matches::default(),
            "no DM, nothing found"
        );
    }

    #[test]
    fn a_search_finds_the_dm_alone_in_memory() {
        let store = InMemorySessionStore::new();
        searched(&store, false);
        a_search_finds_the_dm_alone_newest_first_a_page_at_a_time(&store);
    }

    #[test]
    fn a_search_finds_the_dm_alone_in_files() {
        for archive in [false, true] {
            let dir = tempfile::tempdir().unwrap();
            let store = JsonlSessionStore::new(dir.path()).unwrap();
            searched(&store, archive);
            a_search_finds_the_dm_alone_newest_first_a_page_at_a_time(&store);
        }
    }

    /// On postgres, from the archive's index or from a live transcript not
    /// yet archived; another agent's DM with the same person is never read.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_search_finds_the_dm_alone_on_postgres() {
        use workforce_common::session_postgres::PostgresSessionStore;
        for archive in [true, false] {
            workforce_postgres::test_support::with_test_pool(
                PostgresSessionStore::build_pool,
                |pool| async move {
                    let store = PostgresSessionStore::with_pool(pool.clone(), "scout")
                        .await
                        .expect("store");
                    let other = PostgresSessionStore::with_shared_pool(pool, "other");
                    let mut theirs = other.get_or_create(DESK_CHANNEL, ME, "other").unwrap();
                    theirs.add_user_message("question 9 for another agent", None);
                    other.save(&theirs).unwrap();
                    for message in &theirs.messages {
                        other.append_to_archive(&theirs.id, message).unwrap();
                    }
                    searched(store.as_ref(), archive);
                    a_search_finds_the_dm_alone_newest_first_a_page_at_a_time(store.as_ref());
                },
            )
            .await;
        }
    }

    /// A search crosses a rolled-over DM's seam, and an id it found pages
    /// around it there.
    #[test]
    fn a_search_crosses_a_rollover() {
        let dir = tempfile::tempdir().unwrap();
        let store = JsonlSessionStore::new(dir.path()).unwrap();
        exchange(&store, 0);
        let old = store.find(DESK_CHANNEL, ME).unwrap().unwrap();
        workforce_agent::rollover::roll_if_stale(
            &store,
            old,
            &workforce_common::session::CompactionConfig::default(),
            chrono::Utc::now() + chrono::Duration::days(3),
        )
        .unwrap();
        exchange(&store, 1);
        let found = search(&store, &scout(), ME, "question", None, 50).unwrap();
        assert_eq!(
            found
                .matches
                .iter()
                .map(|m| m.excerpt.as_str())
                .collect::<Vec<_>>(),
            ["question 1", "question 0"],
            "the carry-over is no match"
        );
        assert!(!found.more);
    }

    #[test]
    fn an_excerpt_is_a_line_around_the_words_cut_where_it_was_cut() {
        assert_eq!(excerpt("short\n\n  text", "text"), "short text");
        let long = format!(
            "{} the café bill {}",
            "word ".repeat(40),
            "tail ".repeat(60)
        );
        assert_eq!(
            excerpt(&long, "café,"),
            excerpt(&long, "café"),
            "its letters"
        );
        let cut = excerpt(&long, "CAFÉ");
        assert!(cut.starts_with('…') && cut.ends_with('…'), "{cut}");
        assert!(cut.contains("the café bill"), "{cut}");
        assert!(cut.chars().count() <= EXCERPT_CHARS + 2, "{cut}");
        assert!(!cut.starts_with("…ord"), "not mid-word: {cut}");
        assert!(excerpt(&long, "absent").starts_with("word word"));
    }

    #[test]
    fn a_match_s_excerpt_reads_as_its_message_does() {
        let store = InMemorySessionStore::new();
        let mut session = store.get_or_create(DESK_CHANNEL, ME, "scout").unwrap();
        session.add_assistant_message("the key is AKIAIOSFODNN7EXAMPLE for the bucket", 5);
        store.save(&session).unwrap();
        let found = search(&store, &scout(), ME, "bucket", None, 10).unwrap();
        assert_eq!(found.matches.len(), 1);
        for probe in [
            "AKIAIOSFODNN7EXAMPLE",
            "akiaiosfodnn7",
            "key AKIAIOSFODNN7EXAMPLE",
        ] {
            assert_eq!(
                search(&store, &scout(), ME, probe, None, 10).unwrap(),
                Matches::default(),
                "a redacted secret is not found: {probe}"
            );
        }
        assert!(
            !found.matches[0].excerpt.contains("AKIAIOSFODNN7EXAMPLE"),
            "{}",
            found.matches[0].excerpt
        );
    }

    fn around(store: &dyn SessionStore, id: &str, limit: usize) -> Page {
        page(store, &scout(), ME, DESK_CHANNEL, Cursor::Around(id), limit).unwrap()
    }

    /// The message in the middle, half the page before it and the rest
    /// after; at either end of the conversation the other side fills it.
    #[test]
    fn a_page_around_a_message_holds_it_in_the_middle() {
        let store = InMemorySessionStore::new();
        for n in 0..10 {
            exchange(&store, n);
        }
        let all = page(&store, &scout(), ME, DESK_CHANNEL, Cursor::Newest, 200).unwrap();
        let id = |text: &str| {
            all.messages
                .iter()
                .find(|m| m.text == text)
                .unwrap()
                .id
                .clone()
        };
        let texts_of =
            |page: &Page| -> Vec<String> { page.messages.iter().map(|m| m.text.clone()).collect() };

        let middle = around(&store, &id("question 5"), 6);
        assert_eq!(
            texts_of(&middle),
            [
                "answer 3",
                "question 4",
                "answer 4",
                "question 5",
                "answer 5",
                "question 6"
            ]
        );
        assert_eq!((middle.more, middle.more_after), (true, Some(true)));

        let start = around(&store, &id("question 0"), 6);
        assert_eq!(
            texts_of(&start),
            [
                "question 0",
                "answer 0",
                "question 1",
                "answer 1",
                "question 2",
                "answer 2"
            ]
        );
        assert_eq!((start.more, start.more_after), (false, Some(true)));

        let end = around(&store, &id("answer 9"), 6);
        assert_eq!(
            texts_of(&end),
            [
                "question 7",
                "answer 7",
                "question 8",
                "answer 8",
                "question 9",
                "answer 9"
            ]
        );
        assert_eq!((end.more, end.more_after), (true, Some(false)));

        let odd = around(&store, &id("question 5"), 5);
        assert_eq!(
            texts_of(&odd),
            [
                "question 4",
                "answer 4",
                "question 5",
                "answer 5",
                "question 6"
            ]
        );
        let odd = around(&store, &id("question 5"), 7);
        assert_eq!(
            texts_of(&odd),
            [
                "answer 3",
                "question 4",
                "answer 4",
                "question 5",
                "answer 5",
                "question 6",
                "answer 6"
            ]
        );
        let one = around(&store, &id("question 5"), 1);
        assert_eq!(texts_of(&one), ["question 5"]);
        assert_eq!((one.more, one.more_after), (true, Some(true)));

        let whole = around(&store, &id("answer 4"), 200);
        assert_eq!(whole.messages.len(), 20);
        assert_eq!((whole.more, whole.more_after), (false, Some(false)));

        let wire = serde_json::to_value(&middle).unwrap();
        assert_eq!(wire["more_after"], serde_json::json!(true));
        assert!(serde_json::to_value(&all)
            .unwrap()
            .get("more_after")
            .is_none());

        let refused = page(
            &store,
            &scout(),
            ME,
            DESK_CHANNEL,
            Cursor::Around("not-a-message"),
            6,
        )
        .unwrap_err();
        assert!(
            matches!(refused, ConversationError::Invalid(_)),
            "{refused}"
        );
    }

    /// Around a message either side of a rolled-over DM's seam, the page
    /// reads across it.
    #[test]
    fn a_page_around_a_message_crosses_a_rollover() {
        let dir = tempfile::tempdir().unwrap();
        let store = JsonlSessionStore::new(dir.path()).unwrap();
        exchange(&store, 0);
        let old = store.find(DESK_CHANNEL, ME).unwrap().unwrap();
        workforce_agent::rollover::roll_if_stale(
            &store,
            old,
            &workforce_common::session::CompactionConfig::default(),
            chrono::Utc::now() + chrono::Duration::days(3),
        )
        .unwrap();
        exchange(&store, 1);
        let all = page(&store, &scout(), ME, DESK_CHANNEL, Cursor::Newest, 200).unwrap();
        assert_eq!(all.messages.len(), 4);
        let before_seam = around(&store, &all.messages[1].id, 4);
        assert_eq!(
            texts(&before_seam),
            [
                ("question 0", Author::User),
                ("answer 0", Author::Agent),
                ("question 1", Author::User),
                ("answer 1", Author::Agent),
            ]
        );
        assert_eq!(
            (before_seam.more, before_seam.more_after),
            (false, Some(false))
        );
        let after_seam = around(&store, &all.messages[2].id, 2);
        assert_eq!(
            texts(&after_seam),
            [("answer 0", Author::Agent), ("question 1", Author::User)]
        );
        assert_eq!((after_seam.more, after_seam.more_after), (true, Some(true)));
    }

    /// Every search of the DM a page at a time, oldest last.
    fn searched_a_page_at_a_time(store: &dyn SessionStore, query: &str) -> Vec<Match> {
        let mut all: Vec<Match> = Vec::new();
        loop {
            let before = all.last().map(|m| m.id.clone());
            let page = search(store, &scout(), ME, query, before.as_deref(), 1).unwrap();
            assert!(
                page.matches.len() == 1 || !page.more,
                "more always has a match to go on from"
            );
            all.extend(page.matches);
            if !page.more {
                return all;
            }
            assert!(all.len() < 50, "the pages end");
        }
    }

    /// Turns a rollover carried into the successor verbatim, under their own
    /// ids, are each found once, where they were first said, and a cursor
    /// on one reads on from there.
    fn carried_turns_are_found_once(store: &dyn SessionStore) {
        for n in 0..3 {
            exchange(store, n);
        }
        let head = store.find(DESK_CHANNEL, ME).unwrap().unwrap();
        let mut old = store.get(&head.id).unwrap().unwrap();
        for message in &old.messages {
            store.append_to_archive(&old.id, message).unwrap();
        }
        let successor = workforce_agent::rollover::seal_and_mint(store, &mut old, 6).unwrap();
        let carried = successor
            .messages
            .iter()
            .find(|m| m.content == "question 2")
            .expect("the successor carries the last turns verbatim");
        exchange(store, 3);
        let all = searched_a_page_at_a_time(store, "question");
        assert_eq!(
            all.iter().map(|m| m.excerpt.as_str()).collect::<Vec<_>>(),
            ["question 3", "question 2", "question 1", "question 0"]
        );
        let older = search(store, &scout(), ME, "question", Some(&carried.id), 10).unwrap();
        assert_eq!(
            older
                .matches
                .iter()
                .map(|m| m.excerpt.as_str())
                .collect::<Vec<_>>(),
            ["question 1", "question 0"]
        );
    }

    #[test]
    fn carried_turns_are_found_once_in_files() {
        let dir = tempfile::tempdir().unwrap();
        carried_turns_are_found_once(&JsonlSessionStore::new(dir.path()).unwrap());
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn carried_turns_are_found_once_on_postgres() {
        use workforce_common::session_postgres::PostgresSessionStore;
        workforce_postgres::test_support::with_test_pool(
            PostgresSessionStore::build_pool,
            |pool| async move {
                let store = PostgresSessionStore::with_pool(pool, "scout")
                    .await
                    .expect("store");
                carried_turns_are_found_once(store.as_ref());
            },
        )
        .await;
    }

    /// A message kept since the session's archive began, but only live, is
    /// no message the search reads: a cursor on it is the caller's mistake.
    fn a_cursor_the_archive_lacks_is_refused(store: &dyn SessionStore) {
        exchange(store, 0);
        let head = store.find(DESK_CHANNEL, ME).unwrap().unwrap();
        let session = store.get(&head.id).unwrap().unwrap();
        for message in &session.messages {
            store.append_to_archive(&session.id, message).unwrap();
        }
        let mut session = store.get(&head.id).unwrap().unwrap();
        let late = session.add_user_message("question late", None);
        store.save(&session).unwrap();
        let refused = search(store, &scout(), ME, "question", Some(&late), 10);
        assert!(
            matches!(refused, Err(ConversationError::Invalid(_))),
            "{refused:?}"
        );
    }

    #[test]
    fn a_cursor_the_archive_lacks_is_refused_in_files() {
        let dir = tempfile::tempdir().unwrap();
        a_cursor_the_archive_lacks_is_refused(&JsonlSessionStore::new(dir.path()).unwrap());
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_cursor_the_archive_lacks_is_refused_on_postgres() {
        use workforce_common::session_postgres::PostgresSessionStore;
        workforce_postgres::test_support::with_test_pool(
            PostgresSessionStore::build_pool,
            |pool| async move {
                let store = PostgresSessionStore::with_pool(pool, "scout")
                    .await
                    .expect("store");
                a_cursor_the_archive_lacks_is_refused(store.as_ref());
            },
        )
        .await;
    }

    /// A page of one, through every match, ends with `more` false and never
    /// says more with nothing to go on from, even past hub notes.
    #[test]
    fn a_search_a_match_at_a_time_reads_to_the_end() {
        let store = InMemorySessionStore::new();
        searched(&store, false);
        let all = searched_a_page_at_a_time(&store, "question");
        assert_eq!(all.len(), 5);
    }
}
