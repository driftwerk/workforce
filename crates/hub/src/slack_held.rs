//! Slack messages routed to an agent that is not serving. One for an agent
//! being (re)built waits for it and then goes back through the dispatch loop;
//! one for an agent that failed to load, is unknown, or is not back in time
//! gets a reply saying it was not answered. Never the default agent's answer
//! in that agent's place, and never silence.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use parking_lot::Mutex;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use tracing::{info, warn};
use workforce_common::channels::{ChannelSurface, OutboundMessage};
use workforce_common::slack_claim::SlackMessageClaimer;
use workforce_skills::adapters::slack::SlackAction;

use crate::agent_registry::AgentRegistry;

/// How long a message waits for its agent to come back from a respawn.
pub(crate) const HOLD_FOR: Duration = Duration::from_secs(60);
/// Messages held per agent; more are answered as not delivered at once.
const MAX_HELD: usize = 100;

pub(crate) const UNANSWERED: &str = "_This agent is unavailable right now, so your message wasn't \
                          answered. Please send it again in a few minutes._";

/// Where to tell a sender their message was not answered.
pub(crate) struct Reply {
    /// `None` keeps the sender untold, as in a shared channel another agent
    /// may answer in.
    pub surface: Option<Arc<dyn ChannelSurface>>,
    pub channel: String,
    pub thread: Option<String>,
    /// The dispatch loop's claim on the message: only the hub task that wins
    /// it replies, so a message another task answered gets no notice.
    pub claim: Option<Claim>,
}

pub(crate) struct Claim {
    pub store: Arc<dyn SlackMessageClaimer>,
    pub agent: String,
    pub workspace: String,
    pub ts: String,
}

type Queue = Vec<(SlackAction, Reply)>;

/// The messages waiting for their agents, one queue per agent, each sent
/// back through `inlet` in arrival order once the agent serves.
pub(crate) struct HeldMessages {
    registry: Arc<AgentRegistry>,
    inlet: mpsc::Sender<SlackAction>,
    drain: CancellationToken,
    within: Duration,
    queues: Arc<Mutex<HashMap<String, Queue>>>,
}

impl HeldMessages {
    pub(crate) fn new(
        registry: Arc<AgentRegistry>,
        inlet: mpsc::Sender<SlackAction>,
        drain: CancellationToken,
        within: Duration,
    ) -> Self {
        Self {
            registry,
            inlet,
            drain,
            within,
            queues: Arc::default(),
        }
    }

    /// `action` is routed to `agent`, which is not live: hold it while the
    /// agent starts, else tell its sender it was not answered.
    pub(crate) fn hold(&self, agent: &str, action: SlackAction, reply: Reply) {
        let state = self.registry.not_serving(agent);
        let starting_or_live =
            state == Some(crate::hosts::NotServing::Starting) || self.registry.get(agent).is_some();
        if !starting_or_live {
            let why = state.map_or_else(|| "not running".to_string(), |s| s.to_string());
            tokio::spawn(tell_unanswered(agent.to_string(), reply, why));
            return;
        }
        let first = {
            let mut queues = self.queues.lock();
            let queue = queues.entry(agent.to_string()).or_default();
            if queue.len() >= MAX_HELD {
                drop(queues);
                tokio::spawn(tell_unanswered(
                    agent.to_string(),
                    reply,
                    "starting, with too many messages already waiting".to_string(),
                ));
                return;
            }
            queue.push((action, reply));
            queue.len() == 1
        };
        info!(
            agent,
            "Slack message held until its agent finishes starting"
        );
        if first {
            tokio::spawn(self.release_when_serving(agent.to_string()));
        }
    }

    fn release_when_serving(&self, agent: String) -> impl std::future::Future<Output = ()> {
        let registry = Arc::clone(&self.registry);
        let inlet = self.inlet.clone();
        let drain = self.drain.clone();
        let within = self.within;
        let queues = Arc::clone(&self.queues);
        async move {
            let served = tokio::select! {
                served = registry.wait_until_serving(&agent, within) => served.is_some(),
                () = drain.cancelled() => false,
            };
            let held = queues.lock().remove(&agent).unwrap_or_default();
            for (action, reply) in held {
                if !served {
                    let why = registry
                        .not_serving(&agent)
                        .map_or_else(|| "not running".to_string(), |s| s.to_string());
                    tell_unanswered(agent.clone(), reply, why).await;
                    continue;
                }
                if inlet.send(action).await.is_err() {
                    tell_unanswered(
                        agent.clone(),
                        reply,
                        "serving again, but Slack was reconnecting".to_string(),
                    )
                    .await;
                }
            }
        }
    }
}

/// Tell the sender `agent` did not answer their message, unless another hub
/// task claimed it. Fail-open, like the dispatch claim: a rare duplicate
/// notice beats a silent drop.
async fn tell_unanswered(agent: String, reply: Reply, why: String) {
    if let Some(claim) = &reply.claim {
        match claim
            .store
            .try_claim(&claim.agent, &claim.workspace, &reply.channel, &claim.ts)
            .await
        {
            Ok(true) => {}
            Ok(false) => return,
            Err(e) => warn!(error = %e, "Slack claim failed — telling the sender fail-open"),
        }
    }
    warn!(
        agent = %agent,
        channel = %reply.channel,
        why = %why,
        "Slack message not answered: its agent is not serving"
    );
    let Some(surface) = reply.surface else {
        return;
    };
    let notice = OutboundMessage {
        channel: reply.channel.clone(),
        text: UNANSWERED.to_string(),
        thread_id: reply.thread,
        username: None,
        icon_url: None,
        blocks: None,
    };
    if let Err(e) = surface.send_message(&notice).await {
        warn!(agent = %agent, channel = %reply.channel, error = %e, "Telling a Slack sender their message wasn't answered failed");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::serve::Compose as _;
    use std::collections::HashSet;
    use workforce_common::channels::{ChannelCapabilities, Reaction, SentMessage};
    use workforce_common::settings::{AgentConfig, WorkforceConfig};

    #[derive(Default)]
    struct Notices(Mutex<Vec<(String, Option<String>)>>);

    #[async_trait::async_trait]
    impl ChannelSurface for Notices {
        fn platform(&self) -> &str {
            "slack"
        }
        fn capabilities(&self) -> ChannelCapabilities {
            ChannelCapabilities::default()
        }
        async fn add_reaction(&self, _: &Reaction) -> eyre::Result<()> {
            Ok(())
        }
        async fn remove_reaction(&self, _: &Reaction) -> eyre::Result<()> {
            Ok(())
        }
        async fn send_message(&self, message: &OutboundMessage) -> eyre::Result<SentMessage> {
            assert_eq!(message.text, UNANSWERED);
            self.0
                .lock()
                .push((message.channel.clone(), message.thread_id.clone()));
            Ok(SentMessage {
                channel: message.channel.clone(),
                message_id: "1".to_string(),
                thread_id: message.thread_id.clone(),
                platform: "slack".to_string(),
            })
        }
    }

    /// A claim store another hub task may already have taken messages in.
    #[derive(Default)]
    struct Claims(Mutex<HashSet<String>>);

    #[async_trait::async_trait]
    impl SlackMessageClaimer for Claims {
        async fn try_claim(&self, _: &str, _: &str, _: &str, ts: &str) -> eyre::Result<bool> {
            Ok(self.0.lock().insert(ts.to_string()))
        }
    }

    fn message(ts: &str) -> SlackAction {
        SlackAction::ProcessMessage {
            workspace_id: "minny".to_string(),
            session_key: format!("slack:minny:C1:{ts}"),
            user: "U1".to_string(),
            text: ts.to_string(),
            raw_text: ts.to_string(),
            body: ts.to_string(),
            shared: Vec::new(),
            channel: "C1".to_string(),
            thread_ts: None,
            message_ts: Some(ts.to_string()),
            is_dm: false,
            is_mentioned: true,
            system_prompt: None,
            channel_display_name: None,
            files: Vec::new(),
        }
    }

    fn ts_of(action: &SlackAction) -> &str {
        match action {
            SlackAction::ProcessMessage { message_ts, .. } => message_ts.as_deref().unwrap(),
            _ => unreachable!(),
        }
    }

    struct Fixture {
        registry: Arc<AgentRegistry>,
        held: HeldMessages,
        resent: mpsc::Receiver<SlackAction>,
        notices: Arc<Notices>,
        claims: Arc<Claims>,
        drain: CancellationToken,
    }

    impl Fixture {
        fn new() -> Self {
            let registry = Arc::new(AgentRegistry::from_parts(HashMap::new(), "allan".into()));
            let (inlet, resent) = mpsc::channel(16);
            let drain = CancellationToken::new();
            Self {
                held: HeldMessages::new(Arc::clone(&registry), inlet, drain.clone(), HOLD_FOR),
                registry,
                resent,
                notices: Arc::default(),
                claims: Arc::default(),
                drain,
            }
        }

        fn hold(&self, ts: &str) {
            let surface: Arc<dyn ChannelSurface> = self.notices.clone();
            self.held.hold(
                "Minny",
                message(ts),
                Reply {
                    surface: Some(surface),
                    channel: "C1".to_string(),
                    thread: Some(ts.to_string()),
                    claim: Some(Claim {
                        store: self.claims.clone(),
                        agent: "Minny".to_string(),
                        workspace: "minny".to_string(),
                        ts: ts.to_string(),
                    }),
                },
            );
        }

        fn notified(&self) -> Vec<Option<String>> {
            self.notices
                .0
                .lock()
                .iter()
                .map(|(_, thread)| thread.clone())
                .collect()
        }

        /// Let every spawned task run until it waits on something.
        async fn settle(&self) {
            for _ in 0..20 {
                tokio::task::yield_now().await;
            }
        }
    }

    fn entry() -> AgentConfig {
        AgentConfig {
            id: "Minny".into(),
            ..Default::default()
        }
    }

    async fn minny() -> Arc<crate::serve::AgentService> {
        let dir = tempfile::tempdir().unwrap();
        let mut config = WorkforceConfig::default();
        config.hub.data_dir = dir.path().to_string_lossy().to_string();
        config.memory.db_path = std::path::Path::new(&config.hub.data_dir)
            .join("memory.db")
            .to_string_lossy()
            .into_owned();
        config.llm.api_key = Some("test-key".into());
        config.agents = vec![entry()];
        std::mem::forget(dir);
        crate::serve::AgentService::from_config(config, None)
            .await
            .unwrap()
    }

    #[tokio::test]
    async fn messages_for_a_respawning_agent_go_back_in_order_once_it_serves() {
        let mut f = Fixture::new();
        f.registry.begin_loading(&entry());
        for ts in ["1.0", "2.0", "3.0"] {
            f.hold(ts);
        }
        f.settle().await;
        assert!(f.resent.try_recv().is_err(), "nothing goes back early");

        f.registry.insert("Minny", minny().await);
        let mut order = Vec::new();
        for _ in 0..3 {
            order.push(ts_of(&f.resent.recv().await.unwrap()).to_string());
        }
        assert_eq!(order, vec!["1.0", "2.0", "3.0"]);
        assert!(f.notified().is_empty());
        assert!(f.claims.0.lock().is_empty(), "the turn takes the claim");
    }

    #[tokio::test]
    async fn a_message_held_just_as_its_agent_came_back_goes_back_through() {
        let mut f = Fixture::new();
        f.registry.insert("Minny", minny().await);
        f.hold("1.0");
        assert_eq!(ts_of(&f.resent.recv().await.unwrap()), "1.0");
        f.settle().await;
        assert!(f.notified().is_empty());
    }

    #[tokio::test]
    async fn the_sender_is_told_at_once_when_the_agent_failed_or_is_unknown() {
        let mut f = Fixture::new();
        f.hold("1.0");
        f.registry
            .load_failed(&entry(), "unknown provider".to_string());
        f.hold("2.0");
        f.settle().await;
        assert_eq!(
            f.notified(),
            vec![Some("1.0".to_string()), Some("2.0".to_string())]
        );
        assert!(f.resent.try_recv().is_err());
    }

    #[tokio::test(start_paused = true)]
    async fn the_sender_is_told_when_the_agent_is_not_back_in_time() {
        let f = Fixture::new();
        f.registry.begin_loading(&entry());
        f.hold("1.0");
        f.settle().await;
        assert!(f.notified().is_empty());

        tokio::time::sleep(HOLD_FOR).await;
        f.settle().await;
        assert_eq!(f.notified(), vec![Some("1.0".to_string())]);
    }

    #[tokio::test]
    async fn a_drain_tells_the_sender() {
        let f = Fixture::new();
        f.registry.begin_loading(&entry());
        f.hold("1.0");
        f.drain.cancel();
        f.settle().await;
        assert_eq!(f.notified(), vec![Some("1.0".to_string())]);
    }

    #[tokio::test]
    async fn a_dispatch_loop_gone_by_the_time_the_agent_serves_tells_the_sender() {
        let mut f = Fixture::new();
        f.registry.begin_loading(&entry());
        f.hold("1.0");
        f.resent.close();
        f.registry.insert("Minny", minny().await);
        f.settle().await;
        assert_eq!(f.notified(), vec![Some("1.0".to_string())]);
    }

    #[tokio::test]
    async fn a_message_another_hub_task_claimed_gets_no_notice() {
        let f = Fixture::new();
        f.claims.0.lock().insert("1.0".to_string());
        f.hold("1.0");
        f.settle().await;
        assert!(f.notified().is_empty());
    }
}
