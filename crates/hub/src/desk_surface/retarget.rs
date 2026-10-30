//! Desk deliveries for the rooms of a device that has been unpaired are
//! pointed at the person's DM: only that device's Desk had the rooms.

use workforce_common::channels::desk::DeskTarget;
use workforce_common::delivery::DeliveryTarget;

use crate::serve::AgentService;

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

/// Point every delivery `owner` set up with `service`'s agent for a room on
/// the unpaired `device` at their DM: their jobs, their heartbeat checks
/// (signed again as theirs) and their live workflow runs. Returns what
/// moved, by id.
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
    jobs.into_iter().chain(checks).chain(runs).collect()
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
