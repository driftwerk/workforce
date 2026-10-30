//! The turns running on this instance that the person in the conversation can
//! still steer: stop, follow up on, or answer an approval for.
//!
//! A turn registers before its setup, so a stop that lands during setup is
//! seen, and stays registered until it has settled — record ended, transcript
//! annotated, fleet-wide claim released. A stop is acknowledged only then, so
//! the message a client sends right after stopping never meets the dying turn.
//! With a NATS bus the registry also listens on each live turn's control
//! subject, so a client connected to another instance reaches it.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use futures::StreamExt;
use parking_lot::Mutex;
use tokio::sync::mpsc;
use tokio::task::{JoinError, JoinHandle};
use tokio_util::sync::CancellationToken;
use tracing::warn;
use workforce_agent::runner::{InjectedMessage, ProgressEvent, RunResult};
use workforce_agent::tools::approval::SelfServe;
use workforce_core::desk::{DeskPlace, DeskTurnContext, TurnPhase, TurnSnapshot, TurnStamp};
use workforce_nats::chat::{TurnControl, TurnControlReply, REFUSED_BUSY};

use crate::serve::TurnInbox;

/// What every refusal of a second live turn on a session tells the user.
pub const TURN_STILL_RUNNING: &str =
    "a turn for this conversation is still running — cancel it or wait for it to finish";

/// How long a stop waits for the turn to settle before reporting it unsettled.
pub const SETTLE_WAIT: Duration = Duration::from_secs(8);

/// How long a finished turn's last progress events may take to go out.
pub const FORWARD_DRAIN: Duration = Duration::from_secs(5);

/// How long registering for control may wait on the bus before the turn runs
/// reachable from this instance only.
const LISTEN_WAIT: Duration = Duration::from_secs(2);

/// Follow-ups and approval answers that may queue for a busy turn.
const INBOX_BUFFER: usize = 32;

/// Progress events relayed ahead of a slow forwarder.
pub const RELAY_BUFFER: usize = 64;

static NEXT_GENERATION: AtomicU64 = AtomicU64::new(1);

/// A second turn for a session that already has one live on this instance.
#[derive(Debug)]
pub struct TurnBusy;

impl std::fmt::Display for TurnBusy {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(TURN_STILL_RUNNING)
    }
}

impl std::error::Error for TurnBusy {}

/// A turn the person stopped before it answered.
#[derive(Debug)]
pub struct TurnStopped;

impl std::fmt::Display for TurnStopped {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("the turn was stopped")
    }
}

impl std::error::Error for TurnStopped {}

#[derive(Debug)]
pub enum Control {
    Cancel,
    /// Put a snapshot of the live Desk turn into its progress stream.
    Snapshot,
    Inject(InjectedMessage),
    /// A person's follow-up, from the Desk place it names, if any.
    FollowUp {
        message: InjectedMessage,
        place: Option<DeskPlace>,
    },
}

impl From<TurnControl> for Control {
    fn from(control: TurnControl) -> Self {
        match control {
            TurnControl::Cancel => Self::Cancel,
            TurnControl::Snapshot => Self::Snapshot,
            TurnControl::FollowUp { text, user, place } => Self::FollowUp {
                message: InjectedMessage::UserMessage {
                    text,
                    user,
                    external_id: None,
                },
                place,
            },
            TurnControl::Approval {
                request_id,
                approved,
                user,
                choice,
            } => Self::Inject(InjectedMessage::ApprovalResponse {
                request_id,
                approved,
                user,
                choice,
            }),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Applied {
    Settled,
    Injected,
    NotLive,
    /// The stop was delivered but the turn had not settled within
    /// [`SETTLE_WAIT`].
    Unsettled,
    /// A follow-up from another Desk place than the live turn's: it waits
    /// for the turn to end rather than steering it.
    Busy,
}

impl Applied {
    pub fn applied(self) -> bool {
        matches!(self, Self::Settled | Self::Injected)
    }
}

/// How a driven turn ended.
#[derive(Debug)]
pub enum Ending {
    Finished(eyre::Result<RunResult>),
    /// Stopped by the person before its answer settled.
    Stopped,
    /// The turn's task panicked, or was aborted by something other than a stop.
    Failed(String),
}

#[derive(Clone)]
struct Entry {
    generation: u64,
    cancel: CancellationToken,
    settled: CancellationToken,
    answered: Arc<AtomicBool>,
    inject: mpsc::Sender<InjectedMessage>,
    desk: Option<Arc<DeskLive>>,
}

/// A live turn asked from Desk: its place, and the mirror its relay keeps of
/// what it has said, which a Desk connecting mid-turn is sent.
struct DeskLive {
    place: DeskPlace,
    mirror: Mutex<TurnMirror>,
    asked: tokio::sync::Notify,
}

/// What a live Desk turn has streamed of its answer and what it is doing,
/// folded from its progress events in order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TurnMirror {
    stamp: TurnStamp,
    since: chrono::DateTime<chrono::Utc>,
    status: TurnPhase,
    text: String,
    /// Where the stream segment now being written starts in `text`.
    segment: usize,
}

impl TurnMirror {
    pub fn new(stamp: TurnStamp) -> Self {
        Self {
            stamp,
            since: chrono::Utc::now(),
            status: TurnPhase::Thinking,
            text: String::new(),
            segment: 0,
        }
    }

    /// Fold `event` in: the deltas build the text, a retracted stream
    /// takes back its own segment, and each kind of work names the phase.
    pub fn observe(&mut self, event: &ProgressEvent) {
        match event {
            ProgressEvent::LlmCall { .. } | ProgressEvent::CompactEnd => {
                self.status = TurnPhase::Thinking
            }
            ProgressEvent::ToolStart { .. } | ProgressEvent::SubActivity { .. } => {
                self.status = TurnPhase::Working
            }
            ProgressEvent::StreamStart => {
                self.segment = self.text.len();
                self.status = TurnPhase::Writing;
            }
            ProgressEvent::StreamDelta { text } => {
                self.text.push_str(text);
                self.status = TurnPhase::Writing;
            }
            ProgressEvent::StreamDiscard { reason } if reason.retracts() => {
                self.text.truncate(self.segment);
            }
            ProgressEvent::CompactStart => self.status = TurnPhase::Tidying,
            ProgressEvent::RateLimited { .. } | ProgressEvent::SlowModelFailover { .. } => {
                self.status = TurnPhase::Waiting
            }
            _ => {}
        }
    }

    pub fn snapshot(&self) -> TurnSnapshot {
        TurnSnapshot {
            stamp: self.stamp.clone(),
            since: self.since,
            status: self.status,
            text: self.text.clone(),
        }
    }
}

impl Entry {
    async fn stop(&self) -> Applied {
        self.cancel.cancel();
        match tokio::time::timeout(SETTLE_WAIT, self.settled.cancelled()).await {
            Ok(()) => Applied::Settled,
            Err(_) => Applied::Unsettled,
        }
    }

    /// Hand `message` to the turn. A turn whose answer is already out is
    /// wound up instead, so the message starts the next turn once this one
    /// has saved.
    async fn inject(&self, message: InjectedMessage) -> Applied {
        if self.cancel.is_cancelled() {
            return Applied::NotLive;
        }
        if self.answered.load(Ordering::Acquire) {
            self.stop().await;
            return Applied::NotLive;
        }
        match self.inject.try_send(message) {
            Ok(()) => Applied::Injected,
            Err(_) => Applied::NotLive,
        }
    }
}

pub struct LiveTurns {
    agent_id: String,
    turns: Mutex<HashMap<String, Entry>>,
}

impl LiveTurns {
    pub fn new(agent_id: impl Into<String>) -> Arc<Self> {
        Arc::new(Self {
            agent_id: agent_id.into(),
            turns: Mutex::new(HashMap::new()),
        })
    }

    /// Register a turn for `session_id`, the Desk turn `desk` if it came from
    /// Desk, or refuse while one is live, with the inbox the turn must run
    /// on. With `nats`, the turn also answers control requests from other
    /// instances.
    pub async fn begin(
        self: &Arc<Self>,
        session_id: &str,
        self_serve: SelfServe,
        desk: Option<&DeskTurnContext>,
        nats: Option<async_nats::Client>,
    ) -> Result<(LiveTurn, TurnInbox), TurnBusy> {
        let (inject, rx) = mpsc::channel(INBOX_BUFFER);
        let entry = Entry {
            generation: NEXT_GENERATION.fetch_add(1, Ordering::Relaxed),
            cancel: CancellationToken::new(),
            settled: CancellationToken::new(),
            answered: Arc::new(AtomicBool::new(false)),
            inject,
            desk: desk.map(|desk| {
                Arc::new(DeskLive {
                    place: desk.turn.place.clone(),
                    mirror: Mutex::new(TurnMirror::new(TurnStamp::from(desk))),
                    asked: tokio::sync::Notify::new(),
                })
            }),
        };
        {
            let mut turns = self.turns.lock();
            if turns.contains_key(session_id) {
                return Err(TurnBusy);
            }
            turns.insert(session_id.to_string(), entry.clone());
        }
        let mut turn = LiveTurn {
            turns: Arc::clone(self),
            ids: vec![session_id.to_string()],
            entry,
            nats,
            pumps: Vec::new(),
        };
        turn.listen(session_id).await;
        Ok((turn, TurnInbox { rx, self_serve }))
    }

    /// Steer the live turn for `session_id`. A stop resolves once the turn has
    /// settled. A follow-up from another Desk place than the turn's is
    /// refused as [`Applied::Busy`], so two threads' asks never merge into
    /// one turn.
    pub async fn apply(&self, session_id: &str, control: Control) -> Applied {
        let Some(entry) = self.turns.lock().get(session_id).cloned() else {
            return Applied::NotLive;
        };
        match control {
            Control::Cancel => entry.stop().await,
            Control::Snapshot => match &entry.desk {
                Some(desk) => {
                    desk.asked.notify_one();
                    Applied::Injected
                }
                None => Applied::NotLive,
            },
            Control::FollowUp {
                place: Some(place), ..
            } if entry.desk.as_ref().is_some_and(|live| live.place != place) => Applied::Busy,
            Control::FollowUp { message, .. } | Control::Inject(message) => {
                entry.inject(message).await
            }
        }
    }

    fn unregister(&self, ids: &[String], generation: u64) {
        let mut turns = self.turns.lock();
        ids.iter().for_each(|id| {
            if turns.get(id).is_some_and(|e| e.generation == generation) {
                turns.remove(id);
            }
        });
    }
}

/// What the turn's progress tells its forwarder: whether the answer has
/// settled, and when a stop means nothing more of it should reach the client.
#[derive(Clone)]
pub struct TurnWatch {
    cancel: CancellationToken,
    answered: Arc<AtomicBool>,
    desk: Option<Arc<DeskLive>>,
}

impl TurnWatch {
    fn note(&self, event: &ProgressEvent) {
        if matches!(event, ProgressEvent::AnswerSettled) {
            self.answered.store(true, Ordering::Release);
        }
        if let Some(desk) = &self.desk {
            desk.mirror.lock().observe(event);
        }
    }

    /// The Desk turn as it stands, if it is one.
    fn snapshot(&self) -> Option<ProgressEvent> {
        self.desk
            .as_ref()
            .map(|desk| ProgressEvent::Snapshot(desk.mirror.lock().snapshot()))
    }

    /// Resolves when a snapshot of the Desk turn is asked for; never for
    /// any other turn.
    async fn asked(&self) {
        match &self.desk {
            Some(desk) => desk.asked.notified().await,
            None => std::future::pending().await,
        }
    }

    /// Resolves once the turn is stopped before its answer settled; never
    /// after, since an answer on screen still finishes and saves.
    async fn stopped(&self) {
        self.cancel.cancelled().await;
        if self.answered.load(Ordering::Acquire) {
            std::future::pending::<()>().await;
        }
    }
}

/// One registered turn. Dropping it unregisters the turn and marks it
/// settled, so every exit path — a panic included — releases its waiters.
pub struct LiveTurn {
    turns: Arc<LiveTurns>,
    ids: Vec<String>,
    entry: Entry,
    nats: Option<async_nats::Client>,
    pumps: Vec<JoinHandle<()>>,
}

impl LiveTurn {
    pub fn cancel_token(&self) -> CancellationToken {
        self.entry.cancel.clone()
    }

    pub fn is_cancelled(&self) -> bool {
        self.entry.cancel.is_cancelled()
    }

    pub fn watch(&self) -> TurnWatch {
        TurnWatch {
            cancel: self.entry.cancel.clone(),
            answered: Arc::clone(&self.entry.answered),
            desk: self.entry.desk.clone(),
        }
    }

    /// Also answer to `session_id` — the successor a rollover moved the turn
    /// to — unless another live turn already holds it.
    pub async fn alias(&mut self, session_id: &str) {
        if self.ids.iter().any(|id| id == session_id) {
            return;
        }
        let claimed = {
            let mut turns = self.turns.turns.lock();
            match turns.get(session_id) {
                Some(_) => false,
                None => {
                    turns.insert(session_id.to_string(), self.entry.clone());
                    true
                }
            }
        };
        if claimed {
            self.ids.push(session_id.to_string());
            self.listen(session_id).await;
        }
    }

    /// Await the turn's task, aborting it on a stop that lands before its
    /// answer settled. Abort drops the run with its tool calls, so a long
    /// tool cannot hold the stop.
    pub async fn drive(&self, mut handle: JoinHandle<eyre::Result<RunResult>>) -> Ending {
        let answered = || self.entry.answered.load(Ordering::Acquire);
        let (joined, unanswered) = tokio::select! {
            biased;
            joined = &mut handle => (joined, !answered()),
            () = self.entry.cancel.cancelled() => {
                let unanswered = !answered();
                if unanswered {
                    handle.abort();
                }
                (handle.await, unanswered)
            }
        };
        ending(joined, unanswered && self.is_cancelled())
    }

    async fn listen(&mut self, session_id: &str) {
        let Some(nats) = self.nats.clone() else {
            return;
        };
        let subject = workforce_nats::subjects::turn_control(&self.turns.agent_id, session_id);
        let subscribed = tokio::time::timeout(LISTEN_WAIT, async {
            let sub = nats.subscribe(subject).await?;
            nats.flush().await?;
            Ok::<_, eyre::Report>(sub)
        })
        .await;
        match subscribed {
            Ok(Ok(sub)) => self.pumps.push(tokio::spawn(pump(
                Arc::clone(&self.turns),
                nats,
                sub,
                session_id.to_string(),
            ))),
            Ok(Err(e)) => warn!(session_id, error = %e, "Listening for turn control failed"),
            Err(_) => warn!(session_id, "Listening for turn control timed out"),
        }
    }
}

impl Drop for LiveTurn {
    fn drop(&mut self) {
        self.turns.unregister(&self.ids, self.entry.generation);
        self.entry.settled.cancel();
        self.pumps.iter().for_each(JoinHandle::abort);
    }
}

fn ending(joined: Result<eyre::Result<RunResult>, JoinError>, stopped: bool) -> Ending {
    match joined {
        Ok(Ok(result)) => Ending::Finished(Ok(result)),
        Ok(Err(_)) if stopped => Ending::Stopped,
        Ok(Err(e)) => Ending::Finished(Err(e)),
        Err(e) if e.is_cancelled() && stopped => Ending::Stopped,
        Err(e) => Ending::Failed(e.to_string()),
    }
}

/// Pass the turn's progress on, noting when its answer settles, until a stop
/// before that answer silences it. A Desk turn's stream opens with a
/// snapshot, which names it to every consumer, and carries one more between
/// two events whenever one is asked for.
pub async fn relay(
    mut progress_rx: mpsc::Receiver<ProgressEvent>,
    relay_tx: mpsc::Sender<ProgressEvent>,
    watch: TurnWatch,
) {
    if let Some(snapshot) = watch.snapshot() {
        if relay_tx.send(snapshot).await.is_err() {
            return;
        }
    }
    loop {
        let event = tokio::select! {
            biased;
            () = watch.stopped() => return,
            () = watch.asked() => match watch.snapshot() {
                Some(snapshot) => snapshot,
                None => continue,
            },
            event = progress_rx.recv() => match event {
                Some(event) => {
                    watch.note(&event);
                    event
                }
                None => return,
            },
        };
        if relay_tx.send(event).await.is_err() {
            return;
        }
    }
}

/// Answer control requests for one live turn. Each request is applied on its
/// own task, so a stop waiting to settle never holds up a follow-up, and the
/// reply outlives this pump if the turn ends mid-request.
async fn pump(
    turns: Arc<LiveTurns>,
    nats: async_nats::Client,
    mut sub: async_nats::Subscriber,
    session_id: String,
) {
    while let Some(msg) = sub.next().await {
        let Some(reply_to) = msg.reply else {
            continue;
        };
        let control = match serde_json::from_slice::<TurnControl>(&msg.payload) {
            Ok(control) => control,
            Err(e) => {
                warn!(session_id = %session_id, error = %e, "Undecodable turn control");
                continue;
            }
        };
        let (turns, nats, session_id) = (Arc::clone(&turns), nats.clone(), session_id.clone());
        tokio::spawn(async move {
            let applied = turns.apply(&session_id, control.into()).await;
            let reply = TurnControlReply {
                applied: applied.applied(),
                refused: (applied == Applied::Busy).then(|| REFUSED_BUSY.to_string()),
            };
            match serde_json::to_vec(&reply) {
                Ok(payload) => {
                    if let Err(e) = nats.publish(reply_to, payload.into()).await {
                        warn!(session_id = %session_id, error = %e, "Replying to turn control failed");
                    }
                }
                Err(e) => warn!(error = %e, "Encoding turn control reply failed"),
            }
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn follow_up(text: &str) -> Control {
        Control::Inject(InjectedMessage::UserMessage {
            text: text.into(),
            user: "u1".into(),
            external_id: None,
        })
    }

    fn pending() -> JoinHandle<eyre::Result<RunResult>> {
        tokio::spawn(std::future::pending())
    }

    async fn begin(turns: &Arc<LiveTurns>, session_id: &str) -> (LiveTurn, TurnInbox) {
        turns
            .begin(session_id, SelfServe::Unanswerable, None, None)
            .await
            .expect("no live turn yet")
    }

    #[tokio::test]
    async fn a_second_turn_on_a_live_session_is_refused() {
        let turns = LiveTurns::new("allan");
        let (mut first, _inbox) = begin(&turns, "s1").await;
        assert!(turns
            .begin("s1", SelfServe::Answerable, None, None)
            .await
            .is_err());
        first.alias("s2").await;
        assert!(
            turns
                .begin("s2", SelfServe::Answerable, None, None)
                .await
                .is_err(),
            "the rollover alias is live too"
        );
        drop(first);
        assert!(turns
            .begin("s1", SelfServe::Answerable, None, None)
            .await
            .is_ok());
    }

    #[tokio::test]
    async fn a_superseded_turn_leaves_its_successor_registered() {
        let turns = LiveTurns::new("allan");
        let (first, _) = begin(&turns, "s1").await;
        let generation = first.entry.generation;
        drop(first);
        let (_second, _inbox) = begin(&turns, "s1").await;
        turns.unregister(&["s1".to_string()], generation);
        assert_eq!(
            turns.apply("s1", follow_up("still here")).await,
            Applied::Injected,
            "a stale unregister does not evict the live turn"
        );
    }

    #[tokio::test]
    async fn injection_reaches_the_inbox_until_the_turn_is_stopped() {
        let turns = LiveTurns::new("allan");
        let (turn, mut inbox) = begin(&turns, "s1").await;
        assert_eq!(inbox.self_serve, SelfServe::Unanswerable);
        assert_eq!(
            turns.apply("s1", follow_up("and main")).await,
            Applied::Injected
        );
        assert!(
            matches!(inbox.rx.recv().await, Some(InjectedMessage::UserMessage { text, .. }) if text == "and main")
        );

        turn.entry.cancel.cancel();
        assert_eq!(
            turns.apply("s1", follow_up("too late")).await,
            Applied::NotLive,
            "a dying turn absorbs nothing"
        );
        assert_eq!(turns.apply("nope", Control::Cancel).await, Applied::NotLive);
    }

    #[tokio::test]
    async fn a_message_after_the_answer_winds_the_turn_up_instead() {
        let turns = LiveTurns::new("allan");
        let (turn, _inbox) = begin(&turns, "s1").await;
        turn.watch().note(&ProgressEvent::AnswerSettled);
        let offered = tokio::spawn({
            let turns = Arc::clone(&turns);
            async move { turns.apply("s1", follow_up("next question")).await }
        });
        let reviews = turn.cancel_token();
        let ending = turn
            .drive(tokio::spawn(async move {
                reviews.cancelled().await;
                Ok(crate::serve::reply_only("done".into()))
            }))
            .await;
        assert!(turn.is_cancelled(), "the reviews still running are stopped");
        assert!(
            matches!(ending, Ending::Finished(Ok(_))),
            "the answer is kept"
        );
        drop(turn);
        assert_eq!(offered.await.unwrap(), Applied::NotLive);
    }

    #[tokio::test]
    async fn a_stop_resolves_only_once_the_turn_has_settled() {
        let turns = LiveTurns::new("allan");
        let (turn, _inbox) = begin(&turns, "s1").await;
        let stopping = tokio::spawn({
            let turns = Arc::clone(&turns);
            async move { turns.apply("s1", Control::Cancel).await }
        });
        let ending = turn.drive(pending()).await;
        assert!(matches!(ending, Ending::Stopped));
        assert!(!stopping.is_finished(), "not acknowledged before settling");
        drop(turn);
        assert_eq!(stopping.await.unwrap(), Applied::Settled);
        assert_eq!(turns.apply("s1", Control::Cancel).await, Applied::NotLive);
    }

    #[tokio::test(start_paused = true)]
    async fn a_stop_that_cannot_settle_reports_unsettled() {
        let turns = LiveTurns::new("allan");
        let (_turn, _inbox) = begin(&turns, "s1").await;
        assert_eq!(turns.apply("s1", Control::Cancel).await, Applied::Unsettled);
    }

    #[tokio::test]
    async fn a_stop_after_the_answer_settled_lets_the_turn_finish() {
        let turns = LiveTurns::new("allan");
        let (turn, _inbox) = begin(&turns, "s1").await;
        turn.watch().note(&ProgressEvent::AnswerSettled);
        let (finish, finished) = tokio::sync::oneshot::channel::<()>();
        let handle = tokio::spawn(async move {
            let _ = finished.await;
            Ok(crate::serve::reply_only("done".into()))
        });
        turn.entry.cancel.cancel();
        let driving = tokio::spawn(async move { (turn.drive(handle).await, turn) });
        tokio::task::yield_now().await;
        finish.send(()).unwrap();
        let (ending, _turn) = driving.await.unwrap();
        assert!(
            matches!(ending, Ending::Finished(Ok(_))),
            "not aborted once answered"
        );
    }

    #[tokio::test]
    async fn a_cooperative_stop_is_still_a_stop() {
        let turns = LiveTurns::new("allan");
        let (turn, _inbox) = begin(&turns, "s1").await;
        turn.entry.cancel.cancel();
        let ending = turn
            .drive(tokio::spawn(async {
                Err(eyre::eyre!("Cancelled by user"))
            }))
            .await;
        assert!(matches!(ending, Ending::Stopped));
    }

    #[tokio::test]
    async fn a_panicking_turn_fails_and_an_error_finishes() {
        let turns = LiveTurns::new("allan");
        let (turn, _inbox) = begin(&turns, "s1").await;
        let ending = turn.drive(tokio::spawn(async { panic!("boom") })).await;
        assert!(matches!(ending, Ending::Failed(_)));
        let ending = turn
            .drive(tokio::spawn(async { Err(eyre::eyre!("provider down")) }))
            .await;
        assert!(matches!(ending, Ending::Finished(Err(_))));
    }

    #[tokio::test]
    async fn the_relay_goes_quiet_on_a_stop_before_the_answer() {
        let turns = LiveTurns::new("allan");
        let (turn, _inbox) = begin(&turns, "s1").await;
        let (progress_tx, progress_rx) = mpsc::channel(8);
        let (relay_tx, mut relay_rx) = mpsc::channel(8);
        let relaying = tokio::spawn(relay(progress_rx, relay_tx, turn.watch()));
        progress_tx.send(ProgressEvent::StreamStart).await.unwrap();
        assert!(matches!(
            relay_rx.recv().await,
            Some(ProgressEvent::StreamStart)
        ));
        turn.entry.cancel.cancel();
        relaying.await.unwrap();
        let _ = progress_tx.send(ProgressEvent::StreamEnd).await;
        assert!(relay_rx.recv().await.is_none(), "nothing after the stop");
    }

    #[test]
    fn a_stopped_turn_closes_silently() {
        assert!(matches!(
            ending(Ok(Err(eyre::eyre!("aborted"))), true),
            Ending::Stopped
        ));
    }

    #[test]
    fn turn_control_maps_to_injected_messages() {
        assert!(matches!(
            Control::from(TurnControl::Cancel),
            Control::Cancel
        ));
        assert!(matches!(
            Control::from(TurnControl::Approval {
                request_id: "r1".into(),
                approved: false,
                user: "u1".into(),
                choice: None,
            }),
            Control::Inject(InjectedMessage::ApprovalResponse {
                approved: false,
                ..
            })
        ));
        assert!(matches!(
            Control::from(TurnControl::FollowUp {
                text: "hi".into(),
                user: "u1".into(),
                place: Some(DeskPlace::Dm),
            }),
            Control::FollowUp {
                message: InjectedMessage::UserMessage {
                    external_id: None,
                    ..
                },
                place: Some(DeskPlace::Dm),
            }
        ));
    }

    fn follow_up_from(place: Option<DeskPlace>) -> Control {
        Control::FollowUp {
            message: InjectedMessage::UserMessage {
                text: "and this".into(),
                user: "u1".into(),
                external_id: None,
            },
            place,
        }
    }

    fn room_thread(thread: u128) -> DeskPlace {
        DeskPlace::Room {
            room: uuid::Uuid::from_u128(1),
            name: "Planning".into(),
            thread: Some(uuid::Uuid::from_u128(thread)),
        }
    }

    fn asked_in(place: DeskPlace) -> DeskTurnContext {
        let turn = workforce_core::desk::DeskTurn {
            place,
            speaker: workforce_core::desk::DeskSpeaker::Person,
            hop: 0,
            context: Vec::new(),
            message: None,
            spoken: false,
        };
        DeskTurnContext::new(
            turn,
            "desk:room",
            None,
            false,
            Some("ref-1".into()),
            "reply-1".into(),
        )
    }

    #[test]
    fn the_mirror_follows_deltas_retractions_and_what_the_turn_is_doing() {
        use workforce_core::types::StreamDiscardReason;
        let mut mirror = TurnMirror::new(TurnStamp {
            turn_ref: Some("ref-1".into()),
            reply_id: "reply-1".into(),
        });
        let phase = |mirror: &TurnMirror| mirror.snapshot().status;
        assert_eq!(phase(&mirror), TurnPhase::Thinking);
        for event in [
            ProgressEvent::StreamStart,
            ProgressEvent::StreamDelta {
                text: "Checking ".into(),
            },
            ProgressEvent::StreamEnd,
            ProgressEvent::ToolStart {
                name: "github.search".into(),
                round: 1,
                detail: String::new(),
            },
        ] {
            mirror.observe(&event);
        }
        assert_eq!(phase(&mirror), TurnPhase::Working);
        for event in [
            ProgressEvent::StreamStart,
            ProgressEvent::StreamDelta {
                text: "a draft".into(),
            },
            ProgressEvent::StreamDiscard {
                reason: StreamDiscardReason::Interrupted,
            },
            ProgressEvent::RateLimited {
                wait_secs: 3,
                attempt: 1,
            },
        ] {
            mirror.observe(&event);
        }
        assert_eq!(phase(&mirror), TurnPhase::Waiting);
        mirror.observe(&ProgressEvent::StreamStart);
        mirror.observe(&ProgressEvent::StreamDelta {
            text: "the answer".into(),
        });
        let snapshot = mirror.snapshot();
        assert_eq!(snapshot.text, "Checking the answer");
        assert_eq!(snapshot.status, TurnPhase::Writing);
        assert_eq!(snapshot.stamp.reply_id, "reply-1");
    }

    /// A Desk turn's relay opens with a snapshot, and puts one in between
    /// two events when asked; another turn's relay never does.
    #[tokio::test]
    async fn a_desk_turn_s_relay_carries_its_snapshots_in_order() {
        let turns = LiveTurns::new("allan");
        let (turn, _inbox) = turns
            .begin(
                "s1",
                SelfServe::Answerable,
                Some(&asked_in(DeskPlace::Dm)),
                None,
            )
            .await
            .unwrap();
        let (progress_tx, progress_rx) = mpsc::channel(8);
        let (relay_tx, mut relayed) = mpsc::channel(8);
        let relaying = tokio::spawn(relay(progress_rx, relay_tx, turn.watch()));
        progress_tx
            .send(ProgressEvent::StreamDelta { text: "Hel".into() })
            .await
            .unwrap();
        let opening = relayed.recv().await.unwrap();
        assert!(
            matches!(&opening, ProgressEvent::Snapshot(s) if s.text.is_empty()),
            "{opening:?}"
        );
        assert!(matches!(
            relayed.recv().await.unwrap(),
            ProgressEvent::StreamDelta { .. }
        ));
        assert_eq!(
            turns.apply("s1", Control::Snapshot).await,
            Applied::Injected
        );
        let asked = relayed.recv().await.unwrap();
        assert!(
            matches!(&asked, ProgressEvent::Snapshot(s) if s.text == "Hel"),
            "{asked:?}"
        );
        drop(progress_tx);
        relaying.await.unwrap();

        let (plain, _inbox) = begin(&turns, "s2").await;
        assert_eq!(turns.apply("s2", Control::Snapshot).await, Applied::NotLive);
        let (progress_tx, progress_rx) = mpsc::channel(8);
        let (relay_tx, mut relayed) = mpsc::channel(8);
        let relaying = tokio::spawn(relay(progress_rx, relay_tx, plain.watch()));
        progress_tx.send(ProgressEvent::StreamStart).await.unwrap();
        drop(progress_tx);
        assert!(matches!(
            relayed.recv().await.unwrap(),
            ProgressEvent::StreamStart
        ));
        relaying.await.unwrap();
    }

    #[tokio::test]
    async fn a_follow_up_from_another_place_is_refused_as_busy() {
        let turns = LiveTurns::new("allan");
        let (_turn, mut inbox) = turns
            .begin(
                "s1",
                SelfServe::Answerable,
                Some(&asked_in(room_thread(2))),
                None,
            )
            .await
            .unwrap();
        assert_eq!(
            turns
                .apply("s1", follow_up_from(Some(room_thread(3))))
                .await,
            Applied::Busy
        );
        assert!(!Applied::Busy.applied());
        assert!(inbox.rx.try_recv().is_err(), "nothing reached the turn");
        assert_eq!(
            turns
                .apply("s1", follow_up_from(Some(room_thread(2))))
                .await,
            Applied::Injected
        );
        assert_eq!(
            turns.apply("s1", follow_up_from(None)).await,
            Applied::Injected,
            "a surface that names no place steers as before"
        );
        assert!(inbox.rx.try_recv().is_ok() && inbox.rx.try_recv().is_ok());
    }

    #[tokio::test]
    async fn a_turn_from_no_place_takes_any_follow_up() {
        let turns = LiveTurns::new("allan");
        let (_turn, _inbox) = begin(&turns, "s1").await;
        assert_eq!(
            turns.apply("s1", follow_up_from(Some(DeskPlace::Dm))).await,
            Applied::Injected
        );
    }

    /// Against a real NATS server with the hub's streams in place:
    /// `WORKFORCE_TEST_NATS_URL=nats://localhost:4222`.
    #[tokio::test]
    async fn control_reaches_only_a_live_turn_over_nats() {
        let Ok(url) = std::env::var("WORKFORCE_TEST_NATS_URL") else {
            return;
        };
        let nats = async_nats::connect(url).await.expect("connect");
        workforce_nats::streams::StreamManager::new(async_nats::jetstream::new(nats.clone()), 1)
            .ensure_all_streams()
            .await
            .expect("streams");
        let turns = LiveTurns::new(format!("allan-{}", uuid::Uuid::new_v4().simple()));
        let request = |control: &TurnControl| {
            let subject = workforce_nats::subjects::turn_control(&turns.agent_id, "s1");
            let payload = serde_json::to_vec(control).unwrap();
            let nats = nats.clone();
            async move { nats.request(subject, payload.into()).await }
        };

        let idle = request(&TurnControl::Cancel).await;
        assert!(
            matches!(&idle, Err(e) if e.kind() == async_nats::RequestErrorKind::NoResponders),
            "nobody holds the turn, and no stream answers in its place: {idle:?}"
        );

        let (turn, _inbox) = turns
            .begin("s1", SelfServe::Unanswerable, None, Some(nats.clone()))
            .await
            .unwrap();
        let stopping = tokio::spawn(request(&TurnControl::Cancel));
        let ending = turn.drive(pending()).await;
        assert!(matches!(ending, Ending::Stopped));
        drop(turn);
        let reply = stopping.await.unwrap().expect("the owner replies");
        let reply: TurnControlReply = serde_json::from_slice(&reply.payload).unwrap();
        assert!(reply.applied);

        let (_turn, _inbox) = turns
            .begin(
                "s1",
                SelfServe::Answerable,
                Some(&asked_in(room_thread(2))),
                Some(nats.clone()),
            )
            .await
            .unwrap();
        let elsewhere = TurnControl::FollowUp {
            text: "and this".into(),
            user: "u1".into(),
            place: Some(room_thread(3)),
        };
        let reply = request(&elsewhere).await.expect("the owner replies");
        let reply: TurnControlReply = serde_json::from_slice(&reply.payload).unwrap();
        assert_eq!(
            reply,
            TurnControlReply {
                applied: false,
                refused: Some(REFUSED_BUSY.into())
            }
        );
    }
}
