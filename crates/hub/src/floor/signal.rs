//! Telling a person's Desk hub links that their floor or plans' usage
//! changed. Changes come in bursts (a turn begins, waits and ends within a
//! second), so each person's are gathered for a moment and sent once.

use std::collections::HashSet;
use std::time::Duration;

use tokio::sync::mpsc;
use workforce_common::channels::desk::DeskSignal;

/// How long changes are gathered before they are sent.
const GATHER: Duration = Duration::from_millis(500);

/// Whose change: a person's, or (none) the holder's of this hub.
type Change = (DeskSignal, Option<String>);

/// Where changes are told, gathered, to every replica's links.
#[derive(Clone)]
pub struct Signals(mpsc::UnboundedSender<Change>);

impl Signals {
    /// Starts gathering changes, sending each batch through the hub's
    /// session push.
    pub fn start() -> Self {
        let (tx, rx) = mpsc::unbounded_channel();
        tokio::spawn(gather(rx));
        Self(tx)
    }

    /// `signal` changed for `owner`.
    pub fn send(&self, signal: DeskSignal, owner: Option<&str>) {
        let _ = self.0.send((signal, owner.map(str::to_string)));
    }
}

async fn gather(mut rx: mpsc::UnboundedReceiver<Change>) {
    while let Some(first) = rx.recv().await {
        let mut due = HashSet::from([first]);
        let window = tokio::time::sleep(GATHER);
        tokio::pin!(window);
        loop {
            tokio::select! {
                () = &mut window => break,
                next = rx.recv() => match next {
                    Some(change) => {
                        due.insert(change);
                    }
                    None => break,
                },
            }
        }
        if let Some(push) = crate::session_push::installed() {
            for (signal, owner) in due {
                push.signal(signal, owner.as_deref()).await;
            }
        }
    }
}

/// `signal` changed for `owner`, where this hub keeps a floor.
pub fn changed(signal: DeskSignal, owner: Option<&str>) {
    if let Some(installed) = super::installed() {
        installed.signals.send(signal, owner);
    }
}
