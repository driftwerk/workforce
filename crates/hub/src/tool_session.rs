//! The session a tool that reads or changes a conversation acts on
//! (`compaction`, `context.*`): only ever the calling person's own.
//!
//! - **Named by id**: it must be the current turn's session, or one the
//!   current identity owns. Any other is "not found", whoever holds it.
//! - **Named by channel**: the identity's own session there, never anyone
//!   else's, and never made: naming a channel creates nothing. A Desk
//!   group's boss conversation's channel is refused outright
//!   ([`crate::boss_links::conversation::refused_channel`]).
//! - **Named by neither**: the current turn's session.
//! - **Kept apart** (a boss conversation, or one a boss left behind): only
//!   from inside itself, so no other conversation's turn reads or compacts
//!   what it holds into its own memory.
//!
//! A call with no identity in scope reaches nothing.

use workforce_common::session::{Session, SessionStore};

/// What a caller is told of a session it may not reach: the same as of one
/// that doesn't exist.
pub const NOT_FOUND: &str = "That conversation isn't one of yours.";

fn current_session_id() -> Option<String> {
    workforce_agent::runner::CURRENT_SESSION_ID
        .try_with(|s| s.clone())
        .ok()
        .filter(|s| !s.is_empty())
}

/// The session `session_id` or `channel` names for the current identity,
/// by the rules above; else why not, in words for the model.
pub fn resolve(
    sessions: &dyn SessionStore,
    session_id: Option<&str>,
    channel: Option<&str>,
) -> Result<Session, String> {
    let current = current_session_id();
    let identity = crate::user_config::current_identity_id();
    let found = match (
        session_id.filter(|s| !s.is_empty()),
        channel.filter(|c| !c.is_empty()),
    ) {
        (Some(id), _) => sessions.get(id).ok().flatten(),
        (None, Some(channel)) => {
            if let Some(why) = crate::boss_links::conversation::refused_channel(channel) {
                return Err(why.to_string());
            }
            let Some(identity) = identity.as_deref() else {
                return Err(NOT_FOUND.into());
            };
            sessions.find(channel, identity).ok().flatten()
        }
        (None, None) => match &current {
            Some(id) => sessions.get(id).ok().flatten(),
            None => None,
        },
    };
    let session = found.ok_or_else(|| NOT_FOUND.to_string())?;
    let own_turn = current.as_deref() == Some(session.id.as_str());
    let owned = identity.as_deref() == Some(session.owner.as_str());
    // Its own turn's session, or another of the person's that isn't kept apart.
    let reachable = own_turn || (owned && !session.kept_apart());
    if !reachable {
        return Err(NOT_FOUND.into());
    }
    Ok(session)
}

#[cfg(test)]
mod tests {
    use super::*;
    use workforce_agent::runner::{CURRENT_SESSION_ID, CURRENT_USER_ID};
    use workforce_common::session::InMemorySessionStore;

    const SAM: &str = "6f1c1c3e-2a51-4a8e-9a57-0d6b1c9f2b11";
    const MIA: &str = "0b7f3d52-8f0a-4c2f-9d44-3c1e6a1f4e22";

    async fn as_sam<T>(current: &str, f: impl std::future::Future<Output = T>) -> T {
        CURRENT_USER_ID
            .scope(SAM.into(), CURRENT_SESSION_ID.scope(current.into(), f))
            .await
    }

    #[tokio::test]
    async fn a_tool_reaches_only_its_persons_own_conversations() {
        let store = InMemorySessionStore::new();
        let sams = store.get_or_create("desk", SAM, "allan").unwrap();
        let mias = store.get_or_create("desk", MIA, "allan").unwrap();
        let mut boss = store.get_or_create("desk:boss:k", SAM, "allan").unwrap();
        boss.metadata = serde_json::json!({ "desk_boss": { "groupKey": "k" } });
        store.save(&boss).unwrap();
        let mut left = store.get_or_create("web", SAM, "allan").unwrap();
        left.metadata = serde_json::json!({ "desk_scope_left": "desk-group:acme" });
        store.save(&left).unwrap();

        // From Sam's DM: his DM, by id, by channel or by default.
        as_sam(&sams.id, async {
            assert_eq!(resolve(&store, Some(&sams.id), None).unwrap().id, sams.id);
            assert_eq!(resolve(&store, None, Some("desk")).unwrap().id, sams.id);
            assert_eq!(resolve(&store, None, None).unwrap().id, sams.id);
            // Never another person's, by id or channel.
            assert!(resolve(&store, Some(&mias.id), None).is_err());
            assert!(resolve(&store, None, Some("slack:T1:C1")).is_err());
            // Never a boss conversation, nor one a boss left, from outside.
            assert!(resolve(&store, Some(&boss.id), None).is_err());
            assert!(resolve(&store, None, Some("desk:boss:k")).is_err());
            assert!(resolve(&store, None, Some("DESK:BOSS:k")).is_err());
            assert!(resolve(&store, Some(&left.id), None).is_err());
        })
        .await;
        // Naming a channel makes nothing.
        assert!(store.find("slack:T1:C1", SAM).unwrap().is_none());
        // From inside the boss conversation, itself.
        as_sam(&boss.id, async {
            assert_eq!(resolve(&store, None, None).unwrap().id, boss.id);
            assert_eq!(resolve(&store, Some(&boss.id), None).unwrap().id, boss.id);
            assert!(resolve(&store, Some(&left.id), None).is_err());
        })
        .await;
        // No identity: nothing but the turn's own.
        let none = CURRENT_SESSION_ID
            .scope(String::new(), async {
                resolve(&store, Some(&sams.id), None)
            })
            .await;
        assert!(none.is_err());
    }
}
