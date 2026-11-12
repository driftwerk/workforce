//! Deleting one of the person's own messages in their Desk DM, and what the
//! agent remembered from it.
//!
//! The message is named by its id in the conversation (what a page reads it
//! as) or by the client's own id for it (`client_id`, which Desk sent it
//! under). Only the person's own messages in their own DM can be deleted:
//! another person's DM is never found, and the agent's messages, the
//! overseer's and the hub's notes are refused. While the agent is answering
//! in the DM nothing is deleted, so a turn can't save its context back over
//! the delete.
//!
//! Deleting takes the message out of the conversation's archive (what Desk
//! reads and the agent searches), leaves a note in its place in the live
//! context so the turns around it stay paired and its words don't reach the
//! model again, and marks the agent's replies to it `answered_deleted`
//! ([`SessionStore::delete_person_message`]). Memories the agent saved from
//! it are forgotten only when asked, and only those that came from it.

use std::sync::Arc;

use workforce_common::session::{MessageRole, SessionStore, TranscriptEntry};
use workforce_desk::{
    Author, ConversationError, Delete, Deleted, SavedMemory, Speaker, SpeakerKind,
};
use workforce_memory::store::MemoryStore;
use workforce_memory::types::{Memory, MemoryFilter, MemoryId};

use crate::desk_conversations::shown_as;
use crate::serve::AgentService;

/// How many transcript entries one read for a client's id asks for.
const ENTRIES_PER_READ: usize = 400;

/// How many reads looking for a client's id may take: a message that far
/// back is past what Desk shows to be deleted.
const MAX_READS: usize = 50;

/// How many sessions of a rolled-over DM are walked.
const MAX_CHAIN: usize = 50;

/// The most of a person's memories with an agent one listing reads, which
/// a vector store holds a query's results to.
const MEMORIES_SCANNED: usize = 10_000;

/// The person's message, found in their DM.
pub(crate) struct Found {
    /// The DM's active session, where its turns run.
    pub head: String,
    /// Every session of the DM, newest first.
    pub chain: Vec<String>,
    pub entry: TranscriptEntry,
}

fn internal(e: eyre::Report) -> ConversationError {
    ConversationError::Internal(format!("{e:#}"))
}

/// The person `owner`'s own message `id` in their DM with `service`'s agent:
/// by its id in the conversation, else by the client's id for it. Not found
/// when it is no message the DM shows; refused when it is not theirs.
pub(crate) async fn persons_message(
    service: &AgentService,
    speaker: &Speaker,
    owner: &str,
    id: &str,
    desk: Option<&workforce_desk::Desk>,
) -> Result<Found, ConversationError> {
    let store = service.sessions.as_ref();
    let not_found = || ConversationError::NotFound("no such message in your conversation".into());
    let Some(head) = store
        .find(workforce_common::channels::DESK_CHANNEL, owner)
        .map_err(internal)?
    else {
        return Err(not_found());
    };
    let entry = find(store, &head.id, id)
        .map_err(internal)?
        .ok_or_else(not_found)?;
    let shown = shown_as(store, &entry, speaker).ok_or_else(not_found)?;
    let theirs = shown.from == Author::User
        && entry.role == MessageRole::User
        && !entry
            .desk
            .as_ref()
            .and_then(|mark| mark.speaker.as_ref())
            .is_some_and(|s| !matches!(s, workforce_common::channels::desk::DeskSpeaker::Person));
    let overseers = match desk {
        Some(desk) => desk
            .store()
            .authors(std::slice::from_ref(&entry.id))
            .await
            .map_err(|e| ConversationError::Unavailable(format!("{e:#}")))?
            .remove(&entry.id)
            .is_some_and(|author| author.kind != SpeakerKind::You),
        None => false,
    };
    if !theirs || overseers {
        return Err(ConversationError::Forbidden(
            "only your own messages can be deleted".into(),
        ));
    }
    Ok(Found {
        chain: chain(store, &head.id).map_err(internal)?,
        head: head.id,
        entry,
    })
}

/// Message `id` in the DM whose active session is `head`: by its id, else
/// the person's message the client sent under that id.
fn find(store: &dyn SessionStore, head: &str, id: &str) -> eyre::Result<Option<TranscriptEntry>> {
    if let Some((_, entry)) = crate::desk_surface::message_in_chain(store, head, id)? {
        return Ok(Some(entry));
    }
    let mut before: Option<(String, String)> = None;
    for _ in 0..MAX_READS {
        let page = store.read_thread_page(
            head,
            before.as_ref().map(|(s, _)| s.as_str()),
            before.as_ref().map(|(_, m)| m.as_str()),
            ENTRIES_PER_READ,
        )?;
        if let Some(entry) = page
            .messages
            .iter()
            .rev()
            .find(|e| e.role == MessageRole::User && e.external_id.as_deref() == Some(id))
        {
            return Ok(Some(entry.clone()));
        }
        match page.messages.first() {
            Some(oldest) if page.has_more => {
                before = Some((oldest.session_id.clone(), oldest.id.clone()));
            }
            _ => break,
        }
    }
    Ok(None)
}

/// The sessions of the DM whose active session is `head`, newest first.
fn chain(store: &dyn SessionStore, head: &str) -> eyre::Result<Vec<String>> {
    let mut chain = vec![head.to_string()];
    while chain.len() < MAX_CHAIN {
        let current = chain.last().expect("never empty");
        match store
            .get(current)?
            .and_then(|s| s.prev_session_id().map(str::to_string))
        {
            // A predecessor pruned or expired ends the chain, as it ends a
            // read of the thread.
            Some(prev) if !chain.contains(&prev) && store.get(&prev)?.is_some() => chain.push(prev),
            _ => break,
        }
    }
    Ok(chain)
}

/// Whether `memory` was saved from message `id` of the DM whose sessions
/// are `chain`: its provenance names a session of the DM and, among the
/// turn's messages, this one.
pub(crate) fn saved_from(memory: &Memory, chain: &[String], id: &str) -> bool {
    !memory.deleted
        && memory
            .source_session_id
            .as_ref()
            .is_some_and(|session| chain.contains(session))
        && memory.source_message_ids.iter().any(|m| m == id)
}

/// The person `owner`'s memories with `service`'s agent that cite `found`
/// in their provenance. None where the agent keeps no memories.
///
/// A memory may cite a message from before or after it was made (a lesson
/// seen again adds the message to one it already had), so every one of the
/// person's memories with the agent is looked through. Past what one read
/// can hold, the listing is refused rather than cut short.
pub(crate) async fn memories_of(
    service: &AgentService,
    owner: &str,
    found: &Found,
) -> Result<Vec<Memory>, ConversationError> {
    let Some(store) = service.memory_store.as_ref() else {
        return Ok(Vec::new());
    };
    let listed = store
        .list(MemoryFilter {
            owner: Some(service.agent_id.clone()),
            identity_id: Some(owner.to_string()),
            limit: Some(MEMORIES_SCANNED),
            ..MemoryFilter::default()
        })
        .await
        .map_err(|e| ConversationError::Unavailable(format!("reading memories failed: {e:#}")))?;
    if listed.len() >= MEMORIES_SCANNED {
        return Err(ConversationError::Unavailable(format!(
            "you have more than {MEMORIES_SCANNED} memories with this agent, too many to look \
             through for one message"
        )));
    }
    Ok(listed
        .into_iter()
        .filter(|m| saved_from(m, &found.chain, &found.entry.id))
        .collect())
}

/// The memories as Desk lists them.
pub(crate) fn listed(memories: &[Memory]) -> Vec<SavedMemory> {
    memories
        .iter()
        .map(|m| SavedMemory {
            id: m.id.0.clone(),
            text: m.content.clone(),
        })
        .collect()
}

/// Forgets those of `asked` that are `owner`'s memories with `agent` citing
/// `found`, each looked up by its id; any other id is left alone. The first
/// that can't be forgotten fails the whole, so the message is kept and the
/// person can try again.
async fn forget_asked(
    memories: Option<&dyn MemoryStore>,
    agent: &workforce_core::types::AgentId,
    owner: &str,
    found: &Found,
    asked: Vec<String>,
) -> Result<Vec<String>, ConversationError> {
    let Some(store) = memories else {
        return Ok(Vec::new());
    };
    let unavailable = |e: eyre::Report| {
        ConversationError::Unavailable(format!(
            "a memory from the message could not be forgotten, so the message was kept: {e:#}"
        ))
    };
    let mut forgot = Vec::new();
    for id in asked {
        if forgot.contains(&id) {
            continue;
        }
        let memory_id = MemoryId(id.clone());
        let cites = store
            .get(&memory_id)
            .await
            .map_err(unavailable)?
            .is_some_and(|m| {
                m.owner == *agent
                    && m.identity_id.as_deref() == Some(owner)
                    && saved_from(&m, &found.chain, &found.entry.id)
            });
        if cites {
            store.forget(&memory_id).await.map_err(unavailable)?;
            forgot.push(id);
        }
    }
    Ok(forgot)
}

/// Deletes `found` from every session of its DM, holding the DM's turn so
/// none runs meanwhile, after forgetting those of `delete.forget` that cite
/// it in `memories`: a memory the person asked to forget is never left with
/// its message gone.
pub(crate) async fn delete(
    service: &AgentService,
    memories: Option<&dyn MemoryStore>,
    speaker: &Speaker,
    owner: &str,
    found: &Found,
    delete: Delete,
) -> Result<Deleted, ConversationError> {
    let busy = || {
        ConversationError::Conflict(format!(
            "{} is answering right now: delete the message once it has finished",
            speaker.name
        ))
    };
    if service.live_turns.running(&found.head) {
        return Err(busy());
    }
    // The fleet's claim keeps a turn on another instance out. On this one
    // the claim is the process's already, so the live turn is the guard,
    // checked again with nothing awaited between it and the deletes. On
    // Postgres a turn that loaded the session before them and saves after
    // misses its revision, and its save keeps the stored messages, the note
    // among them, adding only its own.
    let claim =
        crate::session_turn_claim::SessionTurnClaim::try_acquire(&service.sessions, &found.head)
            .map_err(|e| ConversationError::Unavailable(format!("{e:#}")))?
            .ok_or_else(busy)?;
    let forgot = match forget_asked(memories, &service.agent_id, owner, found, delete.forget).await
    {
        Ok(forgot) => forgot,
        Err(e) => {
            claim.release().await;
            return Err(e);
        }
    };
    let cleared = match service.live_turns.running(&found.head) {
        true => Err(busy()),
        false => {
            let store = service.sessions.as_ref();
            // A rolled-over DM keeps its latest turns word for word in the
            // next session's live context, under the same ids: each session
            // is cleared.
            found
                .chain
                .iter()
                .try_fold(false, |any, session| {
                    Ok(store
                        .delete_person_message(session, &found.entry.id)?
                        .found()
                        || any)
                })
                .map_err(internal)
        }
    };
    // Released before answering, so a message the person sends next isn't
    // refused on its account by another instance.
    claim.release().await;
    if !cleared? {
        return Err(ConversationError::NotFound(
            "no such message in your conversation".into(),
        ));
    }
    Ok(Deleted {
        deleted: true,
        forgot,
    })
}

/// The agent `service` serves as a speaker in a DM.
pub(crate) fn speaker(agent: &str, service: &Arc<AgentService>) -> Speaker {
    Speaker::agent(
        agent,
        &service.display_name().unwrap_or_else(|| agent.to_string()),
    )
}

#[cfg(test)]
#[path = "desk_delete_tests.rs"]
mod tests;
