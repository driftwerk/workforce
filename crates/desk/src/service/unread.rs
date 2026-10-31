//! What is unread on this Desk, and the person's read points: counted by
//! the hub that owns the content, announced to every window when a point
//! moves.

use std::collections::HashMap;

use chrono::{DateTime, Utc};
use uuid::Uuid;

use super::{Desk, DeskError};
use crate::crew::Crew;
use crate::model::{DeskEvent, SessionState};
use crate::reads::{
    effective, AllScope, Read, ReadPlace, ReadPoint, ReadPut, SessionUnread, UnreadSummary, Urgency,
};
use crate::rooms::{RoomSummary, ThreadUnread, YOU};

/// What a session's state since `since` leaves unread after `point`: done
/// answered your goal; interrupted, stopped, or closed with a reply is new.
/// A blocked one is a need, which the floor counts.
pub fn session_unread(
    id: Uuid,
    state: SessionState,
    replied: bool,
    since: DateTime<Utc>,
    point: Option<&ReadPoint>,
    floor: DateTime<Utc>,
) -> Option<SessionUnread> {
    if since <= effective(point, floor) {
        return None;
    }
    let urgency = match state {
        SessionState::Done => Urgency::ForYou,
        SessionState::Interrupted | SessionState::Stopped => Urgency::New,
        SessionState::Closed if replied => Urgency::New,
        _ => return None,
    };
    Some(SessionUnread { id, since, urgency })
}

impl Desk {
    /// What is unread on this Desk for `owner`.
    pub async fn unread(&self, crew: &dyn Crew, owner: &str) -> Result<UnreadSummary, DeskError> {
        let store = &self.0.store;
        let floor = store.floor(owner).await?;
        let points: HashMap<ReadPlace, ReadPoint> = store
            .reads(owner)
            .await?
            .into_iter()
            .map(|r| (r.place, r.point))
            .collect();
        let mut rooms: Vec<RoomSummary> = store
            .room_unread(owner, floor)
            .await?
            .into_iter()
            .map(|(room_id, main)| RoomSummary {
                room_id,
                main,
                threads: Vec::new(),
            })
            .collect();
        let threads = store.thread_unread(owner, floor).await?;
        let parents: HashMap<Uuid, _> = store
            .messages_by_id(&threads.iter().map(|(_, t, _)| *t).collect::<Vec<_>>())
            .await?
            .into_iter()
            .map(|m| (m.id, m))
            .collect();
        for (room, thread, unread) in threads {
            let (Some(summary), Some(parent)) = (
                rooms.iter_mut().find(|s| s.room_id == room),
                parents.get(&thread),
            ) else {
                continue;
            };
            summary.threads.push(ThreadUnread {
                parent: parent.clone(),
                unread,
            });
        }
        let sessions = self.sessions_unread(&points, floor);
        let activity = store
            .activity_unread(effective(points.get(&ReadPlace::Activity), floor))
            .await?;
        let brief = self
            .brief_at(crew)
            .await?
            .filter(|at| *at > effective(points.get(&ReadPlace::Brief), floor));
        Ok(UnreadSummary {
            floor,
            rooms,
            sessions,
            activity,
            brief,
            update: points.get(&ReadPlace::Update).cloned(),
        })
    }

    fn sessions_unread(
        &self,
        points: &HashMap<ReadPlace, ReadPoint>,
        floor: DateTime<Utc>,
    ) -> Vec<SessionUnread> {
        let st = self.0.state.lock();
        let mut unread: Vec<SessionUnread> = st
            .records
            .values()
            .filter_map(|r| {
                session_unread(
                    r.id,
                    r.state,
                    r.last_reply.is_some(),
                    st.state_since.get(&r.id).copied().unwrap_or(r.updated_at),
                    points.get(&ReadPlace::Session(r.id)),
                    floor,
                )
            })
            .collect();
        unread.sort_by(|a, b| b.since.cmp(&a.since));
        unread
    }

    /// When the overseer's latest brief was written, while there is one.
    async fn brief_at(&self, crew: &dyn Crew) -> Result<Option<DateTime<Utc>>, DeskError> {
        let Some(overseer) = crew.overseer() else {
            return Ok(None);
        };
        Ok(self
            .0
            .store
            .latest_check_in(&overseer.id)
            .await?
            .filter(|c| {
                c.reply
                    .as_deref()
                    .is_some_and(|r| !crate::overview::first_paragraph(r).is_empty())
            })
            .map(|c| c.at))
    }

    /// Every point `owner` keeps.
    pub async fn reads(&self, owner: &str) -> Result<Vec<Read>, DeskError> {
        Ok(self.0.store.reads(owner).await?)
    }

    /// Writes `puts` and tells every window of the points that moved.
    pub async fn put_reads(&self, owner: &str, puts: &[ReadPut]) -> Result<Vec<Read>, DeskError> {
        if puts.iter().any(|p| p.place == ReadPlace::Floor) {
            return Err(DeskError::Invalid("the floor is Desk's own".into()));
        }
        // A point is never later than now: one from a clock running ahead
        // would count everything until then as read.
        let now = Utc::now();
        let puts: Vec<ReadPut> = puts
            .iter()
            .cloned()
            .map(|mut p| {
                p.point.at = p.point.at.min(now);
                p
            })
            .collect();
        let changed = self.0.store.put_reads(owner, &puts).await?;
        self.announce_reads(&changed);
        Ok(changed)
    }

    /// Moves every place in `scope` with something unread to its newest
    /// item, in one go, and tells every window.
    pub async fn mark_all(
        &self,
        crew: &dyn Crew,
        owner: &str,
        scope: AllScope,
    ) -> Result<Vec<Read>, DeskError> {
        if let AllScope::Room(room) = scope {
            self.room_of(room)?;
        }
        let floor = self.0.store.floor(owner).await?;
        let mut others = Vec::new();
        if scope == AllScope::Everything {
            let points: HashMap<ReadPlace, ReadPoint> = self
                .0
                .store
                .reads(owner)
                .await?
                .into_iter()
                .map(|r| (r.place, r.point))
                .collect();
            others.extend(self.sessions_unread(&points, floor).into_iter().map(|s| {
                advance(
                    ReadPlace::Session(s.id),
                    ReadPoint {
                        at: s.since,
                        message: None,
                    },
                )
            }));
            if let Some(newest) = self.0.store.newest_activity().await? {
                others.push(advance(ReadPlace::Activity, newest));
            }
            if let Some(at) = self.brief_at(crew).await? {
                others.push(advance(ReadPlace::Brief, ReadPoint { at, message: None }));
            }
        }
        let changed = self.0.store.mark_all(owner, scope, floor, &others).await?;
        self.announce_reads(&changed);
        Ok(changed)
    }

    fn announce_reads(&self, changed: &[Read]) {
        for r in changed {
            let _ = self.0.events.send(DeskEvent::Read {
                place: r.place,
                point: r.point.clone(),
            });
        }
    }

    /// Forgets the points of a room that has gone, and of its threads.
    pub(super) async fn forget_room_reads(&self, room: Uuid) -> Result<(), DeskError> {
        let store = &self.0.store;
        store
            .forget_reads(YOU, &ReadPlace::Room(room).to_string())
            .await?;
        store.forget_reads(YOU, &format!("thread:{room}/")).await?;
        Ok(())
    }

    /// Forgets a session's point.
    pub(super) async fn forget_session_read(&self, id: Uuid) -> Result<(), DeskError> {
        Ok(self
            .0
            .store
            .forget_reads(YOU, &ReadPlace::Session(id).to_string())
            .await?)
    }
}

fn advance(place: ReadPlace, point: ReadPoint) -> ReadPut {
    ReadPut {
        place,
        point,
        mark: crate::reads::Mark::Advance,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::crew::Party;
    use crate::rooms::{Member, NewMessage, NewRoom};
    use crate::service::tests::desk;
    use crate::store::build_pool;
    use workforce_postgres::test_support::with_test_pool;

    fn at(s: i64) -> DateTime<Utc> {
        DateTime::parse_from_rfc3339("2026-10-30T10:00:00Z")
            .unwrap()
            .to_utc()
            + chrono::Duration::seconds(s)
    }

    #[test]
    fn a_session_is_unread_by_the_state_it_came_into_after_its_point() {
        let id = Uuid::from_u128(1);
        let point = ReadPoint {
            at: at(10),
            message: None,
        };
        let floor = at(0);
        let urgency = |state, replied, since| {
            session_unread(id, state, replied, since, Some(&point), floor).map(|u| u.urgency)
        };
        assert_eq!(
            urgency(SessionState::Done, false, at(11)),
            Some(Urgency::ForYou)
        );
        assert_eq!(
            urgency(SessionState::Done, false, at(10)),
            None,
            "seen as it got there"
        );
        assert_eq!(
            urgency(SessionState::Interrupted, false, at(11)),
            Some(Urgency::New)
        );
        assert_eq!(
            urgency(SessionState::Stopped, false, at(11)),
            Some(Urgency::New)
        );
        assert_eq!(
            urgency(SessionState::Closed, true, at(11)),
            Some(Urgency::New)
        );
        assert_eq!(urgency(SessionState::Closed, false, at(11)), None);
        assert_eq!(
            urgency(SessionState::Blocked, false, at(11)),
            None,
            "a need, the floor's"
        );
        assert_eq!(urgency(SessionState::Working, false, at(11)), None);
        assert_eq!(
            session_unread(id, SessionState::Done, false, at(5), None, at(6)),
            None,
            "before the floor"
        );
    }

    /// No overseer: what the crew answers when it isn't one.
    struct NoCrew;

    #[async_trait::async_trait]
    impl Crew for NoCrew {
        fn overseer(&self) -> Option<crate::crew::Overseer> {
            None
        }
        fn agents(&self) -> Vec<crate::crew::CrewAgent> {
            Vec::new()
        }
        fn midnight(&self) -> DateTime<Utc> {
            Utc::now()
        }
        async fn accept(&self, _: &crate::crew::Suggestion) -> Result<(), String> {
            Ok(())
        }
    }

    #[tokio::test]
    async fn the_summary_holds_rooms_activity_and_is_cleared_by_marking_all_read() {
        with_test_pool(build_pool, |pool| async move {
            let dir = tempfile::tempdir().unwrap();
            let d = desk(pool, dir.path()).await;
            let room = d
                .create_room(NewRoom {
                    name: "Atlas launch".into(),
                    members: vec![Member {
                        hub: "dw".into(),
                        agent: "allan".into(),
                    }],
                    default_member: None,
                })
                .await
                .unwrap();
            let floor = d.0.store.floor(YOU).await.unwrap();
            let ask = d
                .post(
                    room.id,
                    NewMessage {
                        author: YOU.into(),
                        content: "what's left?".into(),
                        ..NewMessage::default()
                    },
                )
                .await
                .unwrap()
                .message;
            d.post(
                room.id,
                NewMessage {
                    author: "dw/allan".into(),
                    content: "the migration".into(),
                    in_reply_to: Some(ask.id),
                    ..NewMessage::default()
                },
            )
            .await
            .unwrap();
            let allan = Party::agent("dw/allan", "allan");
            d.note(crate::crew::Event::new(
                crate::crew::Kind::Finished,
                allan.clone(),
                Some(allan),
                "finished",
            ));
            d.note(crate::crew::Event::new(
                crate::crew::Kind::Approved,
                Party::you(),
                None,
                "approved",
            ));
            let mut summary = d.unread(&NoCrew, YOU).await.unwrap();
            for _ in 0..100 {
                if summary.activity.count > 0 {
                    break;
                }
                tokio::time::sleep(std::time::Duration::from_millis(20)).await;
                summary = d.unread(&NoCrew, YOU).await.unwrap();
            }
            assert_eq!(summary.floor, floor);
            let main = &summary.rooms[0];
            assert_eq!(
                (main.room_id, main.main.count, main.main.for_you),
                (room.id, 1, 1)
            );
            assert_eq!(
                summary.activity.count, 2,
                "the turn and another hub's answer, not your own"
            );
            assert_eq!(summary.brief, None);

            let ahead = d
                .put_reads(
                    YOU,
                    &[ReadPut {
                        place: ReadPlace::Brief,
                        point: ReadPoint {
                            at: Utc::now() + chrono::Duration::hours(1),
                            message: None,
                        },
                        mark: crate::reads::Mark::Advance,
                    }],
                )
                .await
                .unwrap();
            assert!(
                ahead[0].point.at <= Utc::now(),
                "a point from a clock ahead is held to now"
            );

            let (_, mut events) = d.subscribe();
            let changed = d
                .mark_all(&NoCrew, YOU, AllScope::Everything)
                .await
                .unwrap();
            assert!(changed.iter().any(|r| r.place == ReadPlace::Activity));
            assert!(matches!(events.recv().await, Ok(DeskEvent::Read { .. })));
            let summary = d.unread(&NoCrew, YOU).await.unwrap();
            assert_eq!(
                (summary.rooms[0].main.count, summary.activity.count),
                (0, 0)
            );

            d.delete_room(room.id).await.unwrap();
            assert!(d
                .reads(YOU)
                .await
                .unwrap()
                .iter()
                .all(|r| !matches!(r.place, ReadPlace::Room(_))));
        })
        .await;
    }
}
