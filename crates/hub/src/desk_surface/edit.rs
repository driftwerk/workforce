//! A Desk DM turn that edits the person's latest message (`desk.edit`): the
//! reply to the old version is set aside, and the agent answers the new one,
//! told what that reply already did, since none of it can be undone.

use workforce_common::channels::desk::{one_line, DeskEdit, DeskStep, STEP_TOOL_CHARS};
use workforce_common::session::{SessionStore, TranscriptEntry};

/// What Desk is told when an edit names anything but the person's latest
/// message in the conversation.
pub(crate) const ONLY_THE_LATEST: &str = "only your latest message can be edited";

/// The archive rows read per page while looking back for the person's
/// latest message, and the most pages read: a long reply full of tool rows
/// still leaves its question within reach.
const PAGE: usize = 100;
const MOST_PAGES: usize = 50;

/// The steps the note names one by one; the rest are counted.
const NOTE_STEPS: usize = 20;

/// An edit of anything but the person's latest message.
#[derive(Debug)]
pub(crate) struct EditRefused;

impl std::fmt::Display for EditRefused {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(ONLY_THE_LATEST)
    }
}

impl std::error::Error for EditRefused {}

/// Set aside, in `session_id`, the message `edit` replaces and everything
/// said after it, returning what the agent is told about it. The caller
/// holds the session's turn claim, so no turn adds to it meanwhile.
/// [`EditRefused`] unless `edit` names the person's latest message there,
/// by its id or the client's own id for it.
pub(crate) fn set_aside(
    sessions: &dyn SessionStore,
    session_id: &str,
    edit: &DeskEdit,
) -> eyre::Result<String> {
    let latest = latest_persons(sessions, session_id)?.ok_or(EditRefused)?;
    let named =
        latest.id == edit.replaces || latest.external_id.as_deref() == Some(edit.replaces.as_str());
    if !named {
        return Err(EditRefused.into());
    }
    let set_aside = sessions.set_aside_from(session_id, &latest.id)?;
    let steps: Vec<&DeskStep> = set_aside
        .iter()
        .filter_map(|entry| entry.desk.as_ref())
        .flat_map(|mark| &mark.steps)
        .collect();
    Ok(edit_note(&steps))
}

/// The person's latest message in the session's archive, if it holds one.
fn latest_persons(
    sessions: &dyn SessionStore,
    session_id: &str,
) -> eyre::Result<Option<TranscriptEntry>> {
    let mut before: Option<String> = None;
    for _ in 0..MOST_PAGES {
        let page = sessions.read_archive_page(session_id, before.as_deref(), PAGE)?;
        if let Some(found) = page.messages.iter().rev().find(|entry| persons(entry)) {
            return Ok(Some(found.clone()));
        }
        match page.messages.first() {
            Some(oldest) if page.has_more => before = Some(oldest.id.clone()),
            _ => return Ok(None),
        }
    }
    Ok(None)
}

/// Whether `entry` is something the person said, as their conversation
/// reads it back.
fn persons(entry: &TranscriptEntry) -> bool {
    #[cfg(feature = "postgres")]
    {
        crate::desk_conversations::persons(entry)
    }
    // No conversation reads without Postgres: a user row no tool wrote.
    #[cfg(not(feature = "postgres"))]
    {
        entry.role == workforce_common::session::MessageRole::User && entry.tool_metadata.is_none()
    }
}

/// What the agent is told of an edit: the message it reads is the new
/// version, its earlier reply is gone, and what that reply did stands.
pub(crate) fn edit_note(steps: &[&DeskStep]) -> String {
    let mut note = String::from(
        "[EDITED MESSAGE]\n\
         The person edited their last message. This is the new version; your earlier reply to \
         it has been set aside.",
    );
    if steps.is_empty() {
        return note;
    }
    note.push_str(" That reply already did these, which were not undone:");
    for step in steps.iter().take(NOTE_STEPS) {
        let how = if step.ok { "ok" } else { "failed" };
        note.push_str(&format!(
            "\n- {} ({how})",
            one_line(&step.tool, STEP_TOOL_CHARS)
        ));
    }
    if steps.len() > NOTE_STEPS {
        note.push_str(&format!("\n- and {} more", steps.len() - NOTE_STEPS));
    }
    note
}

#[cfg(test)]
mod tests {
    use super::*;
    use workforce_common::channels::desk::{DeskMark, DeskSpeaker};
    use workforce_common::session::{
        InMemorySessionStore, JsonlSessionStore, MessageRole, SessionMessage,
    };

    fn said(role: MessageRole, text: &str, external_id: Option<&str>) -> SessionMessage {
        SessionMessage {
            id: uuid::Uuid::new_v4().to_string(),
            role,
            content: text.to_string(),
            tool_data: None,
            timestamp: chrono::Utc::now(),
            tokens: 1,
            external_id: external_id.map(str::to_string),
            attachments: None,
            trust_level: None,
            actor: None,
            desk: None,
        }
    }

    /// A DM with two exchanges, the second reply having run two tools.
    /// Returns its session id and the second question's id.
    fn conversation(store: &dyn SessionStore) -> (String, String) {
        let mut session = store.get_or_create("desk", "person-1", "agent-a").unwrap();
        let mut first = said(MessageRole::User, "first", Some("client-1"));
        first.desk = Some(DeskMark {
            speaker: Some(DeskSpeaker::Person),
            ..DeskMark::default()
        });
        let second = said(MessageRole::User, "second", Some("client-2"));
        let second_id = second.id.clone();
        let mut reply = said(MessageRole::Assistant, "sent it", None);
        reply.desk = Some(DeskMark {
            steps: vec![
                DeskStep::of("email.send", true, 10),
                DeskStep::of("calendar.create", false, 5),
            ],
            ..DeskMark::default()
        });
        for message in [
            first,
            said(MessageRole::Assistant, "hello", None),
            second,
            reply,
        ] {
            store.append_to_archive(&session.id, &message).unwrap();
            session.messages.push(message);
        }
        store.save(&session).unwrap();
        (session.id, second_id)
    }

    fn texts(store: &dyn SessionStore, session_id: &str) -> (Vec<String>, Vec<String>) {
        let archived = store
            .read_archive_page(session_id, None, 50)
            .unwrap()
            .messages
            .into_iter()
            .map(|e| e.content)
            .collect();
        let live = store
            .get(session_id)
            .unwrap()
            .unwrap()
            .messages
            .into_iter()
            .map(|m| m.content)
            .collect();
        (archived, live)
    }

    fn edit(replaces: &str) -> DeskEdit {
        DeskEdit {
            replaces: replaces.into(),
        }
    }

    fn an_edit_sets_aside_the_reply_and_tells_what_it_did(store: &dyn SessionStore) {
        let (sid, second) = conversation(store);
        // Only the latest: an earlier message, by either id, is refused and
        // nothing is set aside.
        for earlier in ["client-1", "nobody"] {
            let refused = set_aside(store, &sid, &edit(earlier)).unwrap_err();
            assert!(refused.downcast_ref::<EditRefused>().is_some(), "{refused}");
        }
        assert_eq!(texts(store, &sid).0.len(), 4);

        let note = set_aside(store, &sid, &edit("client-2")).unwrap();
        assert_eq!(
            note,
            "[EDITED MESSAGE]\nThe person edited their last message. This is the new version; \
             your earlier reply to it has been set aside. That reply already did these, which \
             were not undone:\n- email.send (ok)\n- calendar.create (failed)"
        );
        let (archived, live) = texts(store, &sid);
        assert_eq!(archived, ["first", "hello"]);
        assert_eq!(live, ["first", "hello"]);
        // The message was the latest; now it is gone, so naming it again
        // (by its hub id this time) is refused.
        let gone = set_aside(store, &sid, &edit(&second)).unwrap_err();
        assert!(gone.downcast_ref::<EditRefused>().is_some());
    }

    #[test]
    fn an_edit_sets_aside_the_reply_in_memory() {
        an_edit_sets_aside_the_reply_and_tells_what_it_did(&InMemorySessionStore::new());
    }

    #[test]
    fn an_edit_sets_aside_the_reply_in_files() {
        let tmp = tempfile::tempdir().unwrap();
        let store = JsonlSessionStore::new(tmp.path()).unwrap();
        an_edit_sets_aside_the_reply_and_tells_what_it_did(&store);
    }

    #[test]
    fn a_reply_that_ran_nothing_is_said_set_aside_and_no_more() {
        assert_eq!(
            edit_note(&[]),
            "[EDITED MESSAGE]\nThe person edited their last message. This is the new version; \
             your earlier reply to it has been set aside."
        );
        let many: Vec<DeskStep> = (0..25)
            .map(|n| DeskStep::of(&format!("tool.{n}\nnext"), true, 1))
            .collect();
        let note = edit_note(&many.iter().collect::<Vec<_>>());
        assert_eq!(note.lines().filter(|l| l.starts_with("- ")).count(), 21);
        assert!(note.contains("- tool.0 next (ok)"), "{note}");
        assert!(note.ends_with("- and 5 more"), "{note}");
    }
}
