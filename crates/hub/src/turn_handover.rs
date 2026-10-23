//! Handing a draining instance's turns to the fleet: once it has released
//! its turn claims it says so, and the leader resumes them straight away
//! rather than on its next timed sweep.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use futures::StreamExt;
use tokio::sync::Notify;
use tracing::warn;

const RELEASED_SUBJECT: &str = "workforce.hub.turns_released";

/// How long an instance told of released turns waits to become the leader
/// that resumes them: long enough for an election after the leader drained.
pub const LEADERSHIP_WAIT: Duration = Duration::from_secs(60);

/// Whether this instance leads, waiting up to `within` for it to. An
/// instance with no leadership flag runs alone and always leads.
pub async fn until_leading(flag: Option<&AtomicBool>, within: Duration) -> bool {
    let Some(flag) = flag else {
        return true;
    };
    let until = tokio::time::Instant::now() + within;
    loop {
        if flag.load(Ordering::Relaxed) {
            return true;
        }
        if tokio::time::Instant::now() >= until {
            return false;
        }
        tokio::time::sleep(Duration::from_secs(1)).await;
    }
}

/// Tell every instance this one has released turns for resuming.
pub async fn announce_released(nats: &async_nats::Client) {
    let published = nats.publish(RELEASED_SUBJECT, Vec::new().into()).await;
    if let Err(e) = published
        .map_err(eyre::Report::from)
        .and(nats.flush().await.map_err(eyre::Report::from))
    {
        warn!(error = %e, "announcing released turns");
    }
}

/// Notified each time an instance announces released turns. Without NATS it
/// is never notified, and resuming waits for the timed sweep.
pub fn released_turns(nats: Option<async_nats::Client>) -> Arc<Notify> {
    let released = Arc::new(Notify::new());
    if let Some(nats) = nats {
        let notify = Arc::clone(&released);
        tokio::spawn(async move {
            let mut announcements = match nats.subscribe(RELEASED_SUBJECT).await {
                Ok(sub) => sub,
                Err(e) => {
                    warn!(error = %e, "subscribing to released turns");
                    return;
                }
            };
            while announcements.next().await.is_some() {
                notify.notify_one();
            }
        });
    }
    released
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test(start_paused = true)]
    async fn a_waiting_instance_leads_once_leadership_lands_and_not_before() {
        let flag = Arc::new(AtomicBool::new(false));
        assert!(!until_leading(Some(&flag), Duration::ZERO).await);
        assert!(until_leading(None, Duration::ZERO).await);

        let elected = Arc::clone(&flag);
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_secs(10)).await;
            elected.store(true, Ordering::Relaxed);
        });
        assert!(until_leading(Some(&flag), LEADERSHIP_WAIT).await);

        let never = AtomicBool::new(false);
        assert!(!until_leading(Some(&never), LEADERSHIP_WAIT).await);
    }

    #[tokio::test]
    async fn without_nats_released_turns_never_wake_anyone() {
        let released = released_turns(None);
        let woke = tokio::time::timeout(Duration::from_millis(50), released.notified()).await;
        assert!(woke.is_err());
    }
}
