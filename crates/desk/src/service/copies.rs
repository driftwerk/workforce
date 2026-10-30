//! A thread's conclusion posted to its room: a copy of one thread message
//! in the room's timeline, kept once.

use uuid::Uuid;

use super::{Desk, DeskError};
use crate::model::DeskEvent;
use crate::rooms::{Posted, RoomMessage, MAX_HOPS};

impl Desk {
    /// Posts thread message `message` of room `room` to its timeline as a
    /// copy: its author's, its text, files and steps, at the last hop so it
    /// hands on to nobody, without a source, delivery or request, so a
    /// routed approval shows once. It is no turn, so neither the room's
    /// members nor the loop guard hold it. Posting it again answers with
    /// the copy kept.
    pub async fn copy_to_room(&self, room: Uuid, message: Uuid) -> Result<Posted, DeskError> {
        self.room_of(room)?;
        let original = self
            .0
            .store
            .message(message)
            .await?
            .filter(|m| m.room_id == room)
            .ok_or_else(|| DeskError::Invalid("that message isn't in this room".into()))?;
        if original.copy_of.is_some() {
            return Err(DeskError::Invalid("it is a copy already".into()));
        }
        if original.thread_id.is_none() {
            return Err(DeskError::Invalid("it is in the room already".into()));
        }
        let posted = self
            .0
            .store
            .add_message(&RoomMessage {
                id: Uuid::new_v4(),
                thread_id: None,
                hop: MAX_HOPS,
                created_at: chrono::Utc::now(),
                source: None,
                delivery: None,
                request_id: None,
                in_reply_to: None,
                copy_of: Some(original.id),
                replies: None,
                reading: None,
                ..original
            })
            .await?;
        if posted.created {
            let _ = self.0.events.send(DeskEvent::RoomMessage {
                message: Box::new(posted.message.clone()),
            });
        }
        Ok(posted)
    }
}
