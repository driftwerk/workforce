//! How far a person has read their Desk DMs, kept beside the conversation
//! on the agent's own hub so every Desk of theirs agrees, and what is
//! unread after it: counted from the transcript, newest first.

use chrono::{DateTime, Utc};
use workforce_common::channels::desk::{DeliveryKind, ReadPoint, DESK_CHANNEL};
use workforce_common::session::SessionStore;
use workforce_desk::{Author, ConversationRead, ConversationUnread, Cursor, Page, Speaker};

/// The most unread messages one conversation counts before it says so.
pub(crate) const UNREAD_CAP: i64 = 100;

/// What is unread on `page`, a conversation's newest messages oldest
/// first, after `point` (or `floor`, when it is later): the agent's
/// messages, a notice only new. `more` says older messages remain beyond
/// the page.
pub(crate) fn unread_of(
    page: &Page,
    point: Option<&ReadPoint>,
    floor: DateTime<Utc>,
) -> ConversationUnread {
    let from = point.map_or(floor, |p| p.at.max(floor));
    let theirs: Vec<_> = page
        .messages
        .iter()
        .filter(|m| m.from == Author::Agent)
        .collect();
    let unread: Vec<_> = theirs
        .iter()
        .filter(|m| m.at > from)
        .rev()
        .take(UNREAD_CAP as usize)
        .collect();
    let notice = |m: &&&workforce_desk::Said| {
        m.mark
            .as_ref()
            .is_some_and(|mark| mark.delivery == Some(DeliveryKind::Notice))
    };
    let reached_start = page.messages.first().is_none_or(|m| m.at <= from);
    ConversationUnread {
        count: unread.len() as i64,
        for_you: unread.iter().filter(|m| !notice(m)).count() as i64,
        first: unread.last().map(|m| m.id.clone()),
        last: theirs.last().map(|m| ConversationRead {
            at: m.at,
            message_id: Some(m.id.clone()),
        }),
        capped: unread.len() as i64 >= UNREAD_CAP || (page.more && !reached_start),
    }
}

/// How many messages a count reads back at most: enough for the cap, the
/// person's own between the agent's.
const COUNT_READ: usize = 200;

/// `owner`'s point on their DM with `agent` in `store`, and what is unread
/// after it; neither when the store keeps no points.
pub(crate) fn dm_unread(
    store: &dyn SessionStore,
    agent: &str,
    owner: &str,
) -> eyre::Result<Option<(Option<ConversationRead>, ConversationUnread)>> {
    let Some(floor) = store.desk_read_floor(owner)? else {
        return Ok(None);
    };
    let point = store.desk_read(owner, DESK_CHANNEL)?;
    let page = crate::desk_conversations::page(
        store,
        &Speaker::agent(agent, agent),
        owner,
        DESK_CHANNEL,
        Cursor::Newest,
        COUNT_READ,
    )
    .map_err(|e| eyre::eyre!("{e}"))?;
    Ok(Some((
        point.as_ref().map(ConversationRead::of),
        unread_of(&page, point.as_ref(), floor),
    )))
}

#[cfg(test)]
mod tests {
    use super::*;
    use workforce_common::channels::desk::DeskMark;
    use workforce_desk::Said;

    fn at(s: i64) -> DateTime<Utc> {
        DateTime::parse_from_rfc3339("2026-10-30T10:00:00Z")
            .unwrap()
            .to_utc()
            + chrono::Duration::seconds(s)
    }

    fn said(id: &str, from: Author, s: i64, delivery: Option<DeliveryKind>) -> Said {
        Said {
            id: id.into(),
            from,
            author: Speaker::agent("allan", "Allan"),
            text: "a line".into(),
            at: at(s),
            desk: None,
            mark: delivery.map(|d| DeskMark {
                delivery: Some(d),
                ..DeskMark::default()
            }),
            attachments: Vec::new(),
        }
    }

    #[test]
    fn a_dms_unread_is_the_agents_messages_after_the_point_a_notice_only_new() {
        let page = Page {
            messages: vec![
                said("a1", Author::Agent, 1, None),
                said("u1", Author::User, 2, None),
                said("a2", Author::Agent, 3, None),
                said("a3", Author::Agent, 4, Some(DeliveryKind::Notice)),
                said("a4", Author::Agent, 5, Some(DeliveryKind::Reminder)),
                said("u2", Author::User, 6, None),
            ],
            more: false,
        };
        let point = ReadPoint {
            at: at(1),
            message: Some("a1".into()),
        };
        let unread = unread_of(&page, Some(&point), at(0));
        assert_eq!(
            (
                unread.count,
                unread.for_you,
                unread.first.as_deref(),
                unread.capped
            ),
            (3, 2, Some("a2"), false)
        );
        assert_eq!(
            unread.last,
            Some(ConversationRead {
                at: at(5),
                message_id: Some("a4".into())
            }),
            "marking it read moves to the agent's newest"
        );
        assert_eq!(
            unread_of(&page, None, at(4)).count,
            1,
            "counted from the floor"
        );
        let read = ReadPoint {
            at: at(9),
            message: None,
        };
        assert_eq!(unread_of(&page, Some(&read), at(0)).count, 0);
    }

    #[test]
    fn a_count_stops_at_its_cap_or_where_the_page_did() {
        let many = Page {
            messages: (0..150)
                .map(|i| said(&format!("a{i}"), Author::Agent, i + 1, None))
                .collect(),
            more: true,
        };
        let unread = unread_of(&many, None, at(0));
        assert_eq!((unread.count, unread.capped), (UNREAD_CAP, true));
        assert_eq!(unread.first.as_deref(), Some("a50"), "the newest hundred");
        let short = Page {
            messages: vec![said("a1", Author::Agent, 5, None)],
            more: true,
        };
        assert!(
            unread_of(&short, None, at(0)).capped,
            "older messages remain unread"
        );
        assert!(!unread_of(&short, None, at(6)).capped);
    }
}
