//! The person's logins (engine accounts) as a boss reads them
//! (`crew.logins`): each one's name, provider and id, how far into its plan
//! it was last read, whether it is at its limit or turned away, and the
//! sessions on it the boss sees. And Desk's own moves of sessions between
//! them, said in one line in the main boss's DM, so the person hears it
//! where they talk to Desk.

use std::sync::Arc;

use chrono::Utc;
use serde_json::{json, Value};
use tokio::sync::broadcast::error::RecvError;
use tokio_util::sync::CancellationToken;
use workforce_common::oversight::Scope;
use workforce_desk::crew::Kind;
use workforce_desk::model::DeskEvent;
use workforce_desk::scope::Seen;

use super::watcher::reported;
use super::CrewHub;

impl CrewHub {
    /// `crew.logins`: every login of the person's, with the sessions on it
    /// that `seen` sees.
    pub(super) fn logins_seen(&self, seen: &Seen) -> Value {
        let now = Utc::now();
        let tz = self.desk.timezone();
        let at =
            |t: chrono::DateTime<Utc>| t.with_timezone(&tz).format("%a %d %b %H:%M").to_string();
        let logins: Vec<Value> = self
            .desk
            .accounts()
            .into_iter()
            .map(|a| {
                let standing = a.standing.clone().unwrap_or_default();
                let full = standing.full_by(now);
                let state = match (&a.refused, full) {
                    (Some(r), _) if r.recheck_at > now => format!(
                        "turned away by the provider: {} (tried again after {})",
                        r.reason,
                        at(r.recheck_at)
                    ),
                    (_, Some(f)) => match f.until {
                        Some(u) => format!("at its {}, resets {}", f.window_name(), at(u)),
                        None => format!("at its {}, no reset time yet", f.window_name()),
                    },
                    _ if !a.signed_in => {
                        "its sign-in can't be read: the person signs it in again in Settings".into()
                    }
                    _ => "has room".into(),
                };
                let windows: Vec<Value> = standing
                    .windows
                    .iter()
                    .filter(|w| w.resets_at.is_none_or(|r| r > now))
                    .map(|w| {
                        json!({
                            "window": match w.minutes {
                                300 => "5-hour".to_string(),
                                10_080 => "weekly".to_string(),
                                m => format!("{m} minutes"),
                            },
                            "used_percent": w.used_percent.round(),
                            "resets": w.resets_at.map(at),
                        })
                    })
                    .collect();
                let sessions: Vec<Value> = a
                    .sessions
                    .iter()
                    .filter_map(|id| self.desk.view(*id).ok())
                    .filter(|v| seen.sees(v.group.as_ref().map(|g| g.id.as_str())))
                    .map(|v| json!({ "id": v.id, "name": v.label() }))
                    .collect();
                json!({
                    "name": a.name,
                    "provider": a.engine.title(),
                    "id": a.id,
                    "default": a.is_default,
                    "state": state,
                    "plan": windows,
                    "plan_read": standing.read_at.map(at),
                    "sessions": sessions,
                })
            })
            .collect();
        json!({
            "logins": reported(&json!(logins).to_string(), "the person's logins"),
            "moves": if self.desk.auto_switch() {
                "Desk moves sessions off a login at its limit or turned away to another of the same provider's with room, by itself, and says so"
            } else {
                "The person turned off Desk's own moves between logins: move a session with crew.switch_login"
            },
        })
    }
}

/// Desk's own moves between logins, and that none has room, each said in
/// the main boss's DM as Desk said it in Activity: once.
pub(super) fn logins_loop(
    crew: Arc<CrewHub>,
    cancel: CancellationToken,
) -> impl std::future::Future<Output = ()> {
    let mut events = crew.desk.events();
    async move {
        loop {
            let event = tokio::select! {
                () = cancel.cancelled() => return,
                received = events.recv() => match received {
                    Ok(DeskEvent::Activity { event })
                        if matches!(event.kind, Kind::LoginMoved | Kind::LoginsFull) => event,
                    Ok(_) | Err(RecvError::Lagged(_)) => continue,
                    Err(RecvError::Closed) => return,
                },
            };
            let Some((service, _)) = crew.overseer_for(&Scope::Ungrouped) else {
                continue;
            };
            crew.tell_dm(service.agent_id.as_str(), &event.text).await;
        }
    }
}
