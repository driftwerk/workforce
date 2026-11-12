//! A Desk DM turn that edits the person's latest message (`desk.edit`): the
//! reply to the old version is set aside, and the agent answers the new one,
//! told what that reply already did, since none of it can be undone.

use workforce_common::channels::desk::{one_line, DeskEdit, DeskStep, STEP_TOOL_CHARS};
use workforce_common::session::{
    SessionMessage, SessionStore, SetAside, SetAsideStale, TranscriptEntry,
};

use super::persons;

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

/// What came of an edit.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Edited {
    /// The old version and its reply were set aside and the new version
    /// saved in their place; what the agent is told of it.
    Note(String),
    /// The same edit, sent again: its new version is the person's latest
    /// message already, so nothing is set aside twice.
    Already,
}

/// Set aside, in `session_id`, the message `edit` replaces and everything
/// said after it, saving `asked`, the edit's new version, in their place in
/// the same step. The caller holds the session's turn claim, so no turn
/// adds to it meanwhile; anything else saved there since it was read (a
/// delivery) refuses the edit rather than being set aside unread.
/// [`EditRefused`] unless `edit` names the person's latest message there,
/// by its id or the client's own id for it. [`Edited::Already`] when the
/// person's latest message is `asked`'s own client id: the edit was saved
/// before, and the caller answers it where it is.
pub(crate) fn set_aside(
    sessions: &dyn SessionStore,
    session_id: &str,
    edit: &DeskEdit,
    asked: &SessionMessage,
) -> eyre::Result<Edited> {
    let (latest, newest) = latest_persons(sessions, session_id)?.ok_or(EditRefused)?;
    if asked.external_id.is_some() && latest.external_id == asked.external_id {
        return Ok(Edited::Already);
    }
    let named =
        latest.id == edit.replaces || latest.external_id.as_deref() == Some(edit.replaces.as_str());
    if !named {
        return Err(EditRefused.into());
    }
    let aside = SetAside {
        from: &latest.id,
        newest: &newest,
        replacement: Some(asked),
    };
    let rows = match sessions.set_aside_from(session_id, &aside) {
        Err(e) if e.is::<SetAsideStale>() => return Err(EditRefused.into()),
        rows => rows?,
    };
    let steps: Vec<&DeskStep> = rows
        .entries
        .iter()
        .filter_map(|entry| entry.desk.as_ref())
        .flat_map(|mark| &mark.steps)
        .collect();
    Ok(Edited::Note(edit_note(&steps, rows.compacted)))
}

/// The person's latest message in the session's archive, if it holds one,
/// and the id of the newest message there.
fn latest_persons(
    sessions: &dyn SessionStore,
    session_id: &str,
) -> eyre::Result<Option<(TranscriptEntry, String)>> {
    let mut before: Option<String> = None;
    let mut newest: Option<String> = None;
    for _ in 0..MOST_PAGES {
        let page = sessions.read_archive_page(session_id, before.as_deref(), PAGE)?;
        if newest.is_none() {
            newest = page.messages.last().map(|entry| entry.id.clone());
        }
        if let Some(found) = page.messages.iter().rev().find(|entry| persons(entry)) {
            return Ok(newest.map(|newest| (found.clone(), newest)));
        }
        match page.messages.first() {
            Some(oldest) if page.has_more => before = Some(oldest.id.clone()),
            _ => return Ok(None),
        }
    }
    Ok(None)
}

/// What the agent is told of an edit: the message it reads is the new
/// version, its earlier reply is gone, and what that reply did stands.
/// `compacted`: the old version had been folded into a summary of the
/// conversation, which may still speak of it.
pub(crate) fn edit_note(steps: &[&DeskStep], compacted: bool) -> String {
    let mut note = String::from(
        "[EDITED MESSAGE]\n\
         The person edited their last message. This is the new version; your earlier reply to \
         it has been set aside.",
    );
    if compacted {
        note.push_str(
            " The conversation's summary above was written before the edit and may still \
             mention the old version or that reply; go by this one.",
        );
    }
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
        let asked = SessionMessage::user("second, edited", Some("client-3"), None, None);
        // Only the latest: an earlier message, by either id, is refused and
        // nothing is set aside.
        for earlier in ["client-1", "nobody"] {
            let refused = set_aside(store, &sid, &edit(earlier), &asked).unwrap_err();
            assert!(refused.downcast_ref::<EditRefused>().is_some(), "{refused}");
        }
        assert_eq!(texts(store, &sid).0.len(), 4);

        let note = set_aside(store, &sid, &edit("client-2"), &asked).unwrap();
        assert_eq!(
            note,
            Edited::Note(
                "[EDITED MESSAGE]\nThe person edited their last message. This is the new \
                 version; your earlier reply to it has been set aside. That reply already did \
                 these, which were not undone:\n- email.send (ok)\n- calendar.create (failed)"
                    .into()
            )
        );
        let after = texts(store, &sid);
        assert_eq!(after.0, ["first", "hello", "second, edited"]);
        assert_eq!(after.1, ["first", "hello", "second, edited"]);
        // Sent again, by either id it named, it is saved already: nothing
        // more is set aside.
        for replaces in ["client-2", second.as_str()] {
            assert_eq!(
                set_aside(store, &sid, &edit(replaces), &asked).unwrap(),
                Edited::Already
            );
        }
        assert_eq!(texts(store, &sid), after);
        // Another edit of the gone version is refused.
        let other = SessionMessage::user("second, again", Some("client-4"), None, None);
        let gone = set_aside(store, &sid, &edit(&second), &other).unwrap_err();
        assert!(gone.downcast_ref::<EditRefused>().is_some());
    }

    #[test]
    fn the_person_s_messages_leave_out_hub_notes_and_hand_offs() {
        let entry = |text: &str, speaker: Option<DeskSpeaker>| {
            let mut message = said(MessageRole::User, text, None);
            message.desk = speaker.map(|speaker| DeskMark {
                speaker: Some(speaker),
                ..DeskMark::default()
            });
            workforce_common::session::message_to_transcript_entry("s", &message)
        };
        assert!(persons(&entry("hello", None)));
        assert!(persons(&entry("hello", Some(DeskSpeaker::Person))));
        for note in [
            "[System note: context trimmed]",
            "[SYSTEM] overflow",
            "[Desk check-in] what happened",
        ] {
            assert!(!persons(&entry(note, None)), "{note}");
        }
        let handed = DeskSpeaker::Agent {
            name: "helper".into(),
        };
        assert!(!persons(&entry("over to you", Some(handed))));
        assert!(!persons(
            &workforce_common::session::message_to_transcript_entry(
                "s",
                &said(MessageRole::Assistant, "hi", None)
            )
        ));
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
            edit_note(&[], false),
            "[EDITED MESSAGE]\nThe person edited their last message. This is the new version; \
             your earlier reply to it has been set aside."
        );
        assert!(edit_note(&[], true).contains("may still mention the old version"));
        let many: Vec<DeskStep> = (0..25)
            .map(|n| DeskStep::of(&format!("tool.{n}\nnext"), true, 1))
            .collect();
        let note = edit_note(&many.iter().collect::<Vec<_>>(), false);
        assert_eq!(note.lines().filter(|l| l.starts_with("- ")).count(), 21);
        assert!(note.contains("- tool.0 next (ok)"), "{note}");
        assert!(note.ends_with("- and 5 more"), "{note}");
    }
}
