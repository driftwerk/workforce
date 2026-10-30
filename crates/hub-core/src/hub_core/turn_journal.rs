//! The session-backed journal a turn keeps as it runs, and how the next turn
//! takes up what an interrupted one left: the calls that returned join the
//! transcript, and the work still running is collected rather than redone.

use std::collections::HashSet;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use chrono::Utc;
use tracing::warn;
use workforce_agent::runner::ToolInvocation;
use workforce_agent::turn_journal::{PendingCall, TurnJournal, OUTCOME_UNKNOWN};
use workforce_common::session::{JournalEntry, JournalStep, Session, SessionMessage, SessionStore};

/// How old journalled work may be and still be collected: inside the day the
/// coding worker keeps a finished job for.
pub const COLLECT_WITHIN: chrono::Duration = chrono::Duration::hours(20);

/// What a call left running answers once its turn has ended without being
/// taken up again, a user stop for one.
const ENDED_UNFINISHED: &str = "This call's turn ended before the call finished, so its \
     result was never collected. Check what it did before repeating it.";

/// One turn's journal on its session. It keeps the rows it wrote, which are
/// the turn's tool rows: the end-of-turn save adds them under the same ids, so
/// a row journalled and then saved is never kept twice.
pub struct SessionJournal {
    sessions: Arc<dyn SessionStore>,
    session_id: String,
    actor: Option<String>,
    rows: Mutex<Vec<SessionMessage>>,
    /// Whether the journal was read whole at the turn's start. One that could
    /// not be read may hold work nobody has collected, so it is never cleared.
    read: bool,
}

impl SessionJournal {
    /// Open `session`'s journal for a turn about to run, taking up what an
    /// interrupted turn left: its finished calls are added to the transcript
    /// and its archive, ahead of the message about to be added, and the work
    /// it left running is returned to collect. `actor` names who a shared
    /// session's rows belong to.
    pub fn open(
        sessions: Arc<dyn SessionStore>,
        session: &mut Session,
        actor: Option<&str>,
    ) -> (Arc<Self>, Vec<PendingCall>) {
        let (read, collect) = match sessions.journal(&session.id) {
            Ok(entries) => (true, absorb(sessions.as_ref(), session, &entries)),
            Err(e) => {
                warn!(session = %session.id, error = %e, "reading the turn journal");
                (false, Vec::new())
            }
        };
        let journal = Arc::new(Self {
            sessions,
            session_id: session.id.clone(),
            actor: actor.map(str::to_string),
            rows: Mutex::new(Vec::new()),
            read,
        });
        (journal, collect)
    }

    /// The journal of work that runs outside an agent turn and settles its own
    /// ending, such as a `/cc` round: nothing is absorbed into a transcript.
    pub fn detached(sessions: Arc<dyn SessionStore>, session_id: &str) -> Arc<Self> {
        Arc::new(Self {
            sessions,
            session_id: session_id.to_string(),
            actor: None,
            rows: Mutex::new(Vec::new()),
            read: true,
        })
    }

    /// This turn's tool rows, in the order its calls returned.
    pub fn rows(&self) -> Vec<SessionMessage> {
        self.rows
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }

    /// Empty the journal once the turn's save holds what it recorded.
    pub fn clear(&self) {
        if !self.read {
            return;
        }
        if let Err(e) = self.sessions.journal_clear(&self.session_id) {
            warn!(session = %self.session_id, error = %e, "clearing the turn journal");
        }
    }

    fn append(&self, call: &str, step: JournalStep) -> eyre::Result<()> {
        self.sessions.journal_append(
            &self.session_id,
            &JournalEntry {
                call: call.to_string(),
                at: Utc::now(),
                step,
            },
        )
    }
}

#[async_trait]
impl TurnJournal for SessionJournal {
    async fn pending(
        &self,
        call: &str,
        tool: &str,
        input: &serde_json::Value,
        plan: serde_json::Value,
    ) -> eyre::Result<()> {
        self.append(
            call,
            JournalStep::Pending {
                tool: tool.to_string(),
                input: input.clone(),
                plan,
                actor: self.actor.clone(),
            },
        )
    }

    async fn checkpoint(&self, call: &str, mark: &str) -> eyre::Result<()> {
        self.append(
            call,
            JournalStep::Checkpoint {
                mark: mark.to_string(),
            },
        )
    }

    async fn finished(&self, call: &str, invocation: &ToolInvocation) {
        let row = SessionMessage {
            id: call.to_string(),
            actor: self.actor.clone(),
            ..SessionMessage::tool(
                &invocation.tool_name,
                &invocation.input,
                &invocation.output,
                invocation.success,
                invocation.duration_ms,
                invocation.error.as_deref(),
            )
        };
        self.rows
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push(row.clone());
        if let Err(e) = self.append(
            call,
            JournalStep::Finished {
                message: Box::new(row),
            },
        ) {
            warn!(session = %self.session_id, call, error = %e, "journalling a finished call");
        }
    }
}

/// A call an interrupted turn left unfinished.
struct Unfinished<'a> {
    call: &'a str,
    at: chrono::DateTime<Utc>,
    tool: &'a str,
    input: &'a serde_json::Value,
    plan: &'a serde_json::Value,
    actor: Option<&'a String>,
    checkpoint: Option<String>,
}

impl Unfinished<'_> {
    /// Whether the call's work is young enough that its worker still keeps
    /// it; older work's outcome nobody can tell.
    fn collectable(&self, now: chrono::DateTime<Utc>) -> bool {
        self.at + COLLECT_WITHIN > now
    }

    fn pending(&self) -> PendingCall {
        PendingCall {
            call: self.call.to_string(),
            tool: self.tool.to_string(),
            input: self.input.clone(),
            plan: self.plan.clone(),
            checkpoint: self.checkpoint.clone(),
        }
    }

    /// The row answering the call with `error`, as its turn would have kept it.
    fn row(&self, error: &str) -> SessionMessage {
        SessionMessage {
            id: self.call.to_string(),
            actor: self.actor.cloned(),
            ..SessionMessage::tool(
                self.tool,
                self.input,
                &serde_json::Value::Null,
                false,
                0,
                Some(error),
            )
        }
    }
}

/// The calls `entries` record as started and not finished, each with the
/// last mark its work reached, leaving out any whose row `held` already has.
fn unfinished<'a>(entries: &'a [JournalEntry], held: &HashSet<&str>) -> Vec<Unfinished<'a>> {
    let finished: HashSet<&str> = entries
        .iter()
        .filter(|entry| matches!(entry.step, JournalStep::Finished { .. }))
        .map(|entry| entry.call.as_str())
        .collect();
    entries
        .iter()
        .filter(|entry| !finished.contains(entry.call.as_str()))
        .filter(|entry| !held.contains(entry.call.as_str()))
        .filter_map(|entry| match &entry.step {
            JournalStep::Pending {
                tool,
                input,
                plan,
                actor,
            } => Some(Unfinished {
                call: &entry.call,
                at: entry.at,
                tool,
                input,
                plan,
                actor: actor.as_ref(),
                checkpoint: entries
                    .iter()
                    .filter(|later| later.call == entry.call)
                    .filter_map(|later| match &later.step {
                        JournalStep::Checkpoint { mark } => Some(mark.clone()),
                        _ => None,
                    })
                    .next_back(),
            }),
            _ => None,
        })
        .collect()
}

fn absorb(
    sessions: &dyn SessionStore,
    session: &mut Session,
    entries: &[JournalEntry],
) -> Vec<PendingCall> {
    let held: HashSet<String> = session.messages.iter().map(|m| m.id.clone()).collect();
    let held_ids: HashSet<&str> = held.iter().map(String::as_str).collect();
    let now = Utc::now();
    let mut collect = Vec::new();
    let mut absorbed: Vec<SessionMessage> = entries
        .iter()
        .filter_map(|entry| match &entry.step {
            JournalStep::Finished { message } => Some((**message).clone()),
            _ => None,
        })
        .collect();
    for call in unfinished(entries, &held_ids) {
        match call.collectable(now) {
            true => collect.push(call.pending()),
            false => absorbed.push(call.row(OUTCOME_UNKNOWN)),
        }
    }
    for row in absorbed.into_iter().filter(|row| !held.contains(&row.id)) {
        if let Err(e) = sessions.append_to_archive(&session.id, &row) {
            warn!(session = %session.id, error = %e, "archiving a journalled call");
        }
        session.push_message(row);
    }
    collect
}

/// What a journal records of work nobody is running any more.
pub struct Left {
    /// The rows of the calls that returned.
    pub returned: Vec<SessionMessage>,
    /// The unfinished calls young enough to collect, each with the last mark
    /// its work reached.
    pub collect: Vec<PendingCall>,
    /// The unfinished calls too old to, whose outcome nobody can tell.
    pub lost: Vec<PendingCall>,
}

pub fn left_in(entries: &[JournalEntry]) -> Left {
    let now = Utc::now();
    let (collect, lost): (Vec<_>, Vec<_>) = unfinished(entries, &HashSet::new())
        .into_iter()
        .partition(|call| call.collectable(now));
    Left {
        returned: entries
            .iter()
            .filter_map(|entry| match &entry.step {
                JournalStep::Finished { message } => Some((**message).clone()),
                _ => None,
            })
            .collect(),
        collect: collect.iter().map(Unfinished::pending).collect(),
        lost: lost.iter().map(Unfinished::pending).collect(),
    }
}

/// The calls journalled on `session_id` that started work recently enough
/// to collect and have not finished, each with the last mark its work
/// reached.
pub fn unfinished_calls(
    sessions: &dyn SessionStore,
    session_id: &str,
) -> eyre::Result<Vec<PendingCall>> {
    Ok(left_in(&sessions.journal(session_id)?).collect)
}

/// Settle the work a turn left unfinished once the turn has ended for good,
/// not interrupted for a successor to take up: each such call is answered as
/// ended, so no later turn collects and so reruns it.
pub fn settle_ended(sessions: &dyn SessionStore, session_id: &str) {
    let entries = match sessions.journal(session_id) {
        Ok(entries) => entries,
        Err(e) => {
            warn!(session = session_id, error = %e, "reading the turn journal");
            return;
        }
    };
    for call in unfinished(&entries, &HashSet::new()) {
        let entry = JournalEntry {
            call: call.call.to_string(),
            at: Utc::now(),
            step: JournalStep::Finished {
                message: Box::new(call.row(ENDED_UNFINISHED)),
            },
        };
        if let Err(e) = sessions.journal_append(session_id, &entry) {
            warn!(session = session_id, error = %e, "settling an unfinished call");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use workforce_common::session::InMemorySessionStore;

    fn invocation(tool: &str) -> ToolInvocation {
        ToolInvocation {
            tool_name: tool.into(),
            input: serde_json::json!({"q": 1}),
            output: serde_json::json!("ok"),
            success: true,
            policy_decision: "allow".into(),
            duration_ms: 3,
            denial_reason: None,
            error: None,
        }
    }

    fn pending(call: &str, at: chrono::DateTime<Utc>) -> JournalEntry {
        JournalEntry {
            call: call.into(),
            at,
            step: JournalStep::Pending {
                tool: "coding.session".into(),
                input: serde_json::json!({"message": "fix it"}),
                plan: serde_json::json!({"round": call}),
                actor: Some("u1".into()),
            },
        }
    }

    fn store_with_session() -> (Arc<dyn SessionStore>, Session) {
        let sessions: Arc<dyn SessionStore> = Arc::new(InMemorySessionStore::new());
        let mut session = sessions.get_or_create("c", "o", "a").unwrap();
        session.add_user_message("the question", None);
        (sessions, session)
    }

    #[tokio::test]
    async fn a_finished_call_is_kept_as_the_row_the_turn_saves() {
        let (sessions, mut session) = store_with_session();
        let (journal, _) = SessionJournal::open(Arc::clone(&sessions), &mut session, Some("u1"));
        journal.finished("call-1", &invocation("shell")).await;

        let rows = journal.rows();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].id, "call-1");
        assert_eq!(rows[0].actor.as_deref(), Some("u1"));
        let entries = sessions.journal(&session.id).unwrap();
        let JournalStep::Finished { message } = &entries[0].step else {
            panic!("expected the finished call");
        };
        assert_eq!(message.id, rows[0].id);
        assert_eq!(message.tool_data, rows[0].tool_data);
    }

    #[test]
    fn work_too_old_for_its_worker_to_keep_is_not_left_to_collect() {
        let (sessions, session) = store_with_session();
        for (call, at) in [
            ("stale", Utc::now() - COLLECT_WITHIN),
            ("young", Utc::now()),
        ] {
            sessions
                .journal_append(&session.id, &pending(call, at))
                .unwrap();
        }
        let calls: Vec<String> = unfinished_calls(sessions.as_ref(), &session.id)
            .unwrap()
            .into_iter()
            .map(|call| call.call)
            .collect();
        assert_eq!(calls, ["young"]);
    }

    #[tokio::test]
    async fn the_next_turn_takes_up_what_an_interrupted_one_journalled() {
        let (sessions, mut session) = store_with_session();
        let (journal, _) = SessionJournal::open(Arc::clone(&sessions), &mut session, None);
        journal.finished("done", &invocation("shell")).await;
        journal
            .pending(
                "running",
                "coding.session",
                &serde_json::json!({}),
                serde_json::json!({"round": "running"}),
            )
            .await
            .unwrap();
        journal.checkpoint("running", "r.0.0").await.unwrap();
        journal.checkpoint("running", "r.0.1").await.unwrap();
        journal
            .pending(
                "returned",
                "coding.session",
                &serde_json::json!({}),
                serde_json::json!({}),
            )
            .await
            .unwrap();
        journal
            .finished("returned", &invocation("coding.session"))
            .await;
        sessions
            .journal_append(&session.id, &pending("stale", Utc::now() - COLLECT_WITHIN))
            .unwrap();

        let (next, collect) = SessionJournal::open(Arc::clone(&sessions), &mut session, None);
        assert_eq!(
            collect,
            vec![PendingCall {
                call: "running".into(),
                tool: "coding.session".into(),
                input: serde_json::json!({}),
                plan: serde_json::json!({"round": "running"}),
                checkpoint: Some("r.0.1".into()),
            }],
            "only work still running and young enough is collected, from its last mark"
        );
        let ids: Vec<&str> = session.messages[1..]
            .iter()
            .map(|m| m.id.as_str())
            .collect();
        assert_eq!(ids, vec!["done", "returned", "stale"]);
        let stale = &session.messages[3];
        assert_eq!(
            stale.tool_data.as_ref().unwrap()["error"],
            serde_json::json!(OUTCOME_UNKNOWN)
        );
        assert_eq!(stale.actor.as_deref(), Some("u1"));

        let (_, again) = SessionJournal::open(Arc::clone(&sessions), &mut session, None);
        assert_eq!(again.len(), 1, "work not yet collected stays to collect");
        assert_eq!(
            session.messages.len(),
            4,
            "rows already held are not added twice"
        );

        next.clear();
        assert!(
            SessionJournal::open(Arc::clone(&sessions), &mut session, None)
                .1
                .is_empty()
        );
    }

    #[tokio::test]
    async fn work_whose_row_the_session_already_holds_is_not_collected_again() {
        let (sessions, mut session) = store_with_session();
        sessions
            .journal_append(&session.id, &pending("saved", Utc::now()))
            .unwrap();
        session.push_message(SessionMessage {
            id: "saved".into(),
            ..SessionMessage::tool(
                "coding.session",
                &serde_json::json!({}),
                &serde_json::json!("ok"),
                true,
                1,
                None,
            )
        });
        let (_, collect) = SessionJournal::open(Arc::clone(&sessions), &mut session, None);
        assert!(collect.is_empty());
    }

    #[tokio::test]
    async fn a_turn_that_ended_for_good_leaves_nothing_to_collect() {
        let (sessions, mut session) = store_with_session();
        sessions
            .journal_append(&session.id, &pending("stopped", Utc::now()))
            .unwrap();
        settle_ended(sessions.as_ref(), &session.id);
        let (_, collect) = SessionJournal::open(Arc::clone(&sessions), &mut session, None);
        assert!(collect.is_empty());
        let row = session.messages.last().unwrap();
        assert_eq!(row.id, "stopped");
        assert_eq!(
            row.tool_data.as_ref().unwrap()["error"],
            serde_json::json!(ENDED_UNFINISHED)
        );
    }

    #[tokio::test]
    async fn a_journal_that_could_not_be_read_is_never_cleared() {
        let (sessions, mut session) = store_with_session();
        sessions
            .journal_append(&session.id, &pending("running", Utc::now()))
            .unwrap();
        let journal = SessionJournal {
            sessions: Arc::clone(&sessions),
            session_id: session.id.clone(),
            actor: None,
            rows: Mutex::new(Vec::new()),
            read: false,
        };
        journal.clear();
        assert_eq!(
            SessionJournal::open(Arc::clone(&sessions), &mut session, None)
                .1
                .len(),
            1
        );
    }
}
