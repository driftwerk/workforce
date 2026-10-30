//! Desk deliveries for the rooms of a device that has been unpaired are
//! pointed at the person's DM: only that device's Desk had the rooms.

use workforce_common::channels::desk::{DeskTarget, DESK_CHANNEL};
use workforce_common::delivery::DeliveryTarget;
use workforce_common::session::SessionStore;

use crate::serve::AgentService;

/// How many of a person's Desk conversations are searched for the rooms of
/// an unpaired device.
const SESSIONS_SEARCHED: usize = 500;

/// Where a delivery to `target` goes once `device` is unpaired: the DM,
/// when `target` is a room on that device.
fn moved_off(device: &str, target: &DeskTarget) -> Option<DeskTarget> {
    (target.device.as_deref() == Some(device)).then(|| target.dm())
}

/// Where `target` goes once `device` is unpaired, when it is a Desk room's
/// on that device.
pub(crate) fn retargeted(device: &str, target: &DeliveryTarget) -> Option<DeliveryTarget> {
    target
        .desk_target()
        .and_then(|desk| moved_off(device, desk))
        .map(DeliveryTarget::desk)
}

/// Point the room conversations `owner` held on `device` at their DM, so a
/// run resumed in one (an approval's, a recovered turn's) answers there.
/// Returns them, by id.
fn retarget_sessions(sessions: &dyn SessionStore, owner: &str, device: &str) -> Vec<String> {
    let listed = sessions
        .list_for_owner_on(owner, DESK_CHANNEL, SESSIONS_SEARCHED)
        .unwrap_or_else(|e| {
            tracing::warn!(error = %e, "Desk conversations could not be read to point them elsewhere");
            Vec::new()
        });
    listed
        .into_iter()
        .filter(|shell| shell.desk_origin().is_some_and(|o| moved_off(device, &o).is_some()))
        .filter_map(|shell| {
            let mut session = sessions.get(&shell.id).ok().flatten()?;
            let dm = moved_off(device, &session.desk_origin()?)?;
            session.set_desk_origin(&dm);
            match sessions.save(&session) {
                Ok(()) => Some(session.id),
                Err(e) => {
                    tracing::warn!(session_id = %session.id, error = %e, "a Desk conversation could not be pointed elsewhere");
                    None
                }
            }
        })
        .collect()
}

/// Point every delivery `owner` set up with `service`'s agent for a room on
/// the unpaired `device` at their DM: their jobs, their heartbeat checks
/// (signed again as theirs), their live workflow runs and their room
/// conversations. Returns what moved, by id.
pub(crate) async fn retarget(service: &AgentService, owner: &str, device: &str) -> Vec<String> {
    let jobs = match &service.scheduler {
        Some(scheduler) => {
            scheduler
                .retarget_desk(owner, &|target| retargeted(device, target))
                .await
        }
        None => Vec::new(),
    };
    let checks =
        crate::heartbeat::retarget_checks(service, owner, &|target| retargeted(device, target));
    let runs = service
        .workflow_runtime()
        .map(|runtime| runtime.retarget_desk(owner, &|target| moved_off(device, target)))
        .unwrap_or_default();
    let sessions = retarget_sessions(service.sessions.as_ref(), owner, device);
    jobs.into_iter()
        .chain(checks)
        .chain(runs)
        .chain(sessions)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use uuid::Uuid;
    use workforce_common::channels::desk::{DeskPlace, DESK_CHANNEL};

    fn room(device: &str) -> DeskTarget {
        DeskTarget {
            place: DeskPlace::Room {
                room: Uuid::from_u128(1),
                name: "Planning".into(),
                thread: None,
            },
            channel: "desk:planning".into(),
            device: Some(device.into()),
            spoken: true,
        }
    }

    #[test]
    fn an_unpaired_device_moves_its_rooms_deliveries_to_the_dm() {
        let target = DeliveryTarget::desk(room("device-1"));
        let dm = DeliveryTarget::desk(room("device-1").dm());
        assert_eq!(retargeted("device-1", &target), Some(dm.clone()));
        assert_eq!(retargeted("device-2", &target), None);
        assert_eq!(
            retargeted("device-1", &DeliveryTarget::voice("voice-pe".into())),
            None
        );
        assert_eq!(dm.channel, DESK_CHANNEL);
    }
}
