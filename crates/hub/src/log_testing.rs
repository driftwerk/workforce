//! What tests read of the log: every line from debug up, captured on the
//! thread that holds a capture's guard.

use std::sync::Arc;

use parking_lot::Mutex;
use tracing::dispatcher::DefaultGuard;
use tracing::Dispatch;

#[derive(Clone, Default)]
pub(crate) struct Lines(Arc<Mutex<Vec<u8>>>);

impl std::io::Write for Lines {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.lock().extend_from_slice(buf);
        Ok(buf.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl Lines {
    /// The log of this thread while the guard is held. The other dispatch
    /// must be kept as long: a second dispatcher makes every callsite ask
    /// each one, so another test's thread can't leave one uninterested.
    pub(crate) fn capture() -> (Self, DefaultGuard, Dispatch) {
        let lines = Self::default();
        let writer = lines.clone();
        let dispatch = Dispatch::new(
            tracing_subscriber::fmt()
                .with_writer(move || writer.clone())
                .with_ansi(false)
                .with_max_level(tracing::Level::DEBUG)
                .finish(),
        );
        let others = Dispatch::new(tracing::subscriber::NoSubscriber::default());
        let guard = tracing::dispatcher::set_default(&dispatch);
        (lines, guard, others)
    }

    pub(crate) fn text(&self) -> String {
        String::from_utf8_lossy(&self.0.lock()).into_owned()
    }

    /// The lines at `level` that say `text`.
    pub(crate) fn at(&self, level: &str, text: &str) -> Vec<String> {
        self.text()
            .lines()
            .filter(|l| l.contains(&format!(" {level} ")) && l.contains(text))
            .map(str::to_string)
            .collect()
    }
}
