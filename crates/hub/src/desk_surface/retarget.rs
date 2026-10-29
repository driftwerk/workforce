//! Desk deliveries that can no longer go where they point, a deleted room's
//! or an unpaired device's, are pointed at the person's DM.

use workforce_common::channels::desk::DeskTarget;
use workforce_common::delivery::DeliveryTarget;

use crate::serve::AgentService;

/// The Desk deliveries that can no longer go where they point.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Matches<'a> {
    /// Those to a room conversation, by its channel.
    #[cfg_attr(not(feature = "postgres"), allow(dead_code))]
    Channel(&'a str),
    /// Those to a room on a device, by the device's id.
    Device(&'a str),
}

impl Matches<'_> {
    fn names(&self, target: &DeskTarget) -> bool {
        match self {
            Self::Channel(channel) => target.channel == *channel,
            Self::Device(device) => target.device.as_deref() == Some(*device),
        }
    }

    /// Where `target` goes instead, when it is one of these: the DM.
    pub(crate) fn retargeted(&self, target: &DeliveryTarget) -> Option<DeliveryTarget> {
        target
            .desk_target()
            .filter(|desk| self.names(desk))
            .map(|desk| DeliveryTarget::desk(desk.dm()))
    }
}

/// Point every delivery `owner` set up with `service`'s agent that `matches`
/// names at their DM: their jobs, their heartbeat checks (signed again as
/// theirs) and their live workflow runs. Returns what moved, by id.
pub(crate) async fn retarget(
    service: &AgentService,
    owner: &str,
    matches: Matches<'_>,
) -> Vec<String> {
    let jobs = match &service.scheduler {
        Some(scheduler) => {
            scheduler
                .retarget_desk(owner, &|target| matches.retargeted(target))
                .await
        }
        None => Vec::new(),
    };
    let checks =
        crate::heartbeat::retarget_checks(service, owner, &|target| matches.retargeted(target));
    let runs = service
        .workflow_runtime()
        .map(|runtime| {
            runtime.retarget_desk(owner, &|target| matches.names(target).then(|| target.dm()))
        })
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
    fn a_closed_room_or_unpaired_device_moves_its_deliveries_to_the_dm() {
        let target = DeliveryTarget::desk(room("device-1"));
        let dm = DeliveryTarget::desk(room("device-1").dm());
        assert_eq!(
            Matches::Channel("desk:planning").retargeted(&target),
            Some(dm.clone())
        );
        assert_eq!(
            Matches::Device("device-1").retargeted(&target),
            Some(dm.clone())
        );
        assert_eq!(Matches::Channel("desk:other").retargeted(&target), None);
        assert_eq!(Matches::Device("device-2").retargeted(&target), None);
        assert_eq!(
            Matches::Channel(DESK_CHANNEL).retargeted(&DeliveryTarget::voice("voice-pe".into())),
            None
        );
        assert_eq!(dm.channel, DESK_CHANNEL);
    }
}
