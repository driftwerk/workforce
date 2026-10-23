//! Vector-backend health primitives shared between the
//! `CodeVectorStore` (which produces insert-error signals) and the
//! hub-side probe (which consumes them and composes the typed state).
//!
//! The trait `InsertErrorSink` is intentionally synchronous so the
//! vector store can call it from any context — implementations must be
//! non-blocking. The hub's `BackendHealthProbe` uses a `std::sync::Mutex`
//! over a small ring buffer; lock contention is sub-microsecond.

use std::sync::{Arc, Mutex};

use chrono::{DateTime, Utc};

/// One captured insert-batch failure. Held in the probe's bounded
/// ring buffer; older entries are evicted past the window the probe
/// is configured with.
#[derive(Debug, Clone)]
pub struct InsertError {
    pub at: DateTime<Utc>,
    pub message: String,
}

/// Sink for non-fatal insert-batch errors emitted by
/// `CodeVectorStore`. Implementations MUST NOT block — they're called
/// from inside the async indexer path. The canonical hub
/// implementation records into an in-memory ring buffer and is
/// trivially non-blocking.
pub trait InsertErrorSink: Send + Sync {
    fn record(&self, message: String);
}

/// Where a store sends its insert errors: the sink last wired, if any.
#[derive(Default)]
pub(crate) struct InsertErrorSlot(Mutex<Option<Arc<dyn InsertErrorSink>>>);

impl InsertErrorSlot {
    /// Replace the sink.
    pub(crate) fn set(&self, sink: Arc<dyn InsertErrorSink>) {
        if let Ok(mut slot) = self.0.lock() {
            *slot = Some(sink);
        }
    }

    /// Hand `message` to the sink, if one is wired.
    pub(crate) fn notify(&self, message: &str) {
        let sink = self.0.lock().ok().and_then(|slot| slot.clone());
        if let Some(sink) = sink {
            sink.record(message.to_string());
        }
    }
}
