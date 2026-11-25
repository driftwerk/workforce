//! What a Codex session waits on that Desk doesn't answer. Codex's status
//! says it waits (`waitingOnApproval`, `waitingOnUserInput`) apart from
//! the request it waits on, which can come after the status, or be
//! answered before the status says it no longer waits. So a session says
//! it waits in its terminal only once Codex has waited [`CODEX_WAIT`] with
//! nothing Desk answers open, naming the request when Codex asked one Desk
//! never answers; and it stops saying so the moment anything ends the
//! wait ([`clear_wait`]).

use std::time::Duration;

use chrono::{DateTime, Utc};
use serde_json::Value;
use uuid::Uuid;

use super::{codex_front, Desk, Live};
use crate::crew::{Event, Kind, Party};
use crate::model::{PendingApproval, SessionRecord, SessionState, IN_TERMINAL};

/// How long Codex waits with nothing Desk answers open before its session
/// says it waits in its terminal.
pub(super) const CODEX_WAIT: Duration = if cfg!(test) {
    Duration::from_millis(1500)
} else {
    Duration::from_secs(10)
};

/// What a Codex session waits on, as its observer heard it.
#[derive(Debug, Default)]
pub(super) struct CodexWait {
    /// Its last status said it waits on an answer.
    pub(super) waiting: bool,
    /// Since when it has waited with nothing Desk answers open.
    since: Option<DateTime<Utc>>,
    /// What it asked that Desk never answers, oldest first, until it is
    /// answered: each request's id and method.
    elsewhere: Vec<(Value, String)>,
}

impl CodexWait {
    pub(super) fn resolved(&mut self, request: &Value) {
        self.elsewhere.retain(|(r, _)| r != request);
    }
}

/// Its wait is over (answered anywhere, no longer waiting, a turn started
/// or ended): what only said it waits in its terminal goes, and any wait
/// after it counts from now.
pub(super) fn clear_wait(r: &mut SessionRecord, l: &mut Live) {
    l.codex_wait.since = None;
    if l.pending.as_ref().is_some_and(PendingApproval::in_terminal) {
        l.pending = None;
        codex_front(r, l);
    }
}

/// Whether Codex waits while Desk holds nothing it answers.
fn unanswered(l: &Live) -> bool {
    l.codex_wait.waiting
        && l.pending.is_none()
        && l.codex_asks.is_empty()
        && l.codex_question.is_none()
}

/// What its session shows once Codex has waited on what Desk doesn't
/// answer: in its terminal, naming the request when it knows it.
fn in_terminal(l: &Live, since: DateTime<Utc>) -> PendingApproval {
    let detail = match l.codex_wait.elsewhere.last() {
        Some((_, method)) => format!("{IN_TERMINAL} ({method})"),
        None => IN_TERMINAL.to_string(),
    };
    PendingApproval {
        id: Uuid::new_v4(),
        tool: l.current_tool.clone().unwrap_or_else(|| "Codex".into()),
        detail,
        since,
        answerable: false,
        choices: Vec::new(),
        why: None,
        options: None,
        request: None,
        call: None,
        agent: None,
        cli_session: None,
    }
}

impl Desk {
    /// Codex asked `id`'s thread `request` (`method`), which Desk never
    /// answers: kept until it is answered, and one Desk doesn't know is
    /// said.
    pub(super) fn codex_elsewhere(&self, id: Uuid, request: Value, method: String, known: bool) {
        if !known {
            tracing::warn!(%id, %method, "codex asked something Desk doesn't know; its terminal answers it");
            if let Ok(view) = self.view(id) {
                let me = Party::session(&view);
                self.note(Event::new(
                    Kind::Asked,
                    me.clone(),
                    Some(me),
                    format!(
                        "Codex asked {method}, which Desk doesn't know: its terminal answers it"
                    ),
                ));
            }
        }
        self.change(id, |_, l| {
            if let Some(l) =
                l.filter(|l| !l.codex_wait.elsewhere.iter().any(|(r, _)| *r == request))
            {
                l.codex_wait.elsewhere.push((request, method));
            }
        });
    }

    /// Starts counting `id`'s wait, when Codex waits and Desk holds
    /// nothing it answers: once it has waited [`CODEX_WAIT`] so, it shows
    /// as waiting in its terminal.
    pub(super) fn codex_wait_arm(&self, id: Uuid) {
        let since = {
            let mut st = self.0.state.lock();
            let Some(l) = st.live.get_mut(&id) else {
                return;
            };
            if !unanswered(l) || l.codex_wait.since.is_some() {
                return;
            }
            *l.codex_wait.since.insert(Utc::now())
        };
        let d = self.clone();
        tokio::spawn(async move {
            tokio::time::sleep(CODEX_WAIT).await;
            d.change(id, |r, l| {
                if let Some(l) = l.filter(|l| l.codex_wait.since == Some(since) && unanswered(l)) {
                    l.pending = Some(in_terminal(l, since));
                    r.state = SessionState::Blocked;
                    r.asked = None;
                }
            });
        });
    }
}
