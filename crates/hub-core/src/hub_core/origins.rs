//! Where an automated run says it came from, and how a task passes that on.

use workforce_common::subscription::is_automated_source;

/// The automated source whose turn is in scope (a heartbeat, a monitor, a
/// workflow step, another job's run), if any: the one a sub-agent it spawned
/// carries ([`carried_origin`]), else the one its session names
/// ([`session_origin`]). A job created there was written by that automation,
/// whoever it runs as.
pub fn automated_origin() -> Option<String> {
    carried_origin().or_else(session_origin)
}

/// The automated origin the work in scope carries
/// ([`workforce_agent::runner::CURRENT_AUTOMATED_ORIGIN`]).
pub fn carried_origin() -> Option<String> {
    workforce_agent::runner::CURRENT_AUTOMATED_ORIGIN
        .try_with(String::clone)
        .ok()
        .filter(|origin| !origin.is_empty())
}

/// The automated source the session in scope names, by its key.
pub fn session_origin() -> Option<String> {
    workforce_agent::runner::CURRENT_SESSION_KEY
        .try_with(|key| {
            let source = key.split(':').next().unwrap_or_default();
            (is_automated_source(source)
                || workforce_lessons::turn_extraction::is_internal_channel(key))
            .then(|| source.to_string())
        })
        .ok()
        .flatten()
}

/// Run `fut` as work descending from the automated `origin`, when it has one
/// ([`automated_origin`]).
pub async fn descending_from<F: std::future::Future>(origin: Option<String>, fut: F) -> F::Output {
    match origin {
        Some(origin) => {
            workforce_agent::runner::CURRENT_AUTOMATED_ORIGIN
                .scope(origin, fut)
                .await
        }
        None => fut.await,
    }
}
