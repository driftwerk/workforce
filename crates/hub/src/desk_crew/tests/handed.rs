//! A person asks their agent in Slack to keep work on track in Desk
//! (DW-770, 25 Nov): the request waits on the agent's hub, their Desk takes
//! it, its boss holds it as their own words relayed by the agent, and the
//! goal's stall is told back in the Slack thread it was asked in.

use parking_lot::Mutex;
use uuid::Uuid;
use workforce_common::channels::{
    ChannelCapabilities, ChannelSurface, OutboundMessage, Reaction, SentMessage, SurfaceKey,
};
use workforce_desk::conversations::SpeakerKind;

use super::goal_checks::{checking, goal_running, mantis};
use super::goals::boss_on;
use super::plan_watch::asked_with;
use super::*;
use crate::desk_knows::store::{Handed as Asked, KnowsStore, MemoryKnows, Owner, Reader};

const WORDS: &str = "ok keep Mantis CI fast on track in desk please";

/// What was posted on Slack: channel, thread and text.
#[derive(Default)]
struct Posted(Mutex<Vec<(String, Option<String>, String)>>);

impl Posted {
    fn saying(&self, words: &str) -> Vec<(String, Option<String>, String)> {
        self.0
            .lock()
            .iter()
            .filter(|(_, _, text)| text.contains(words))
            .cloned()
            .collect()
    }
}

#[async_trait::async_trait]
impl ChannelSurface for Posted {
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
        self.0.lock().push((
            message.channel.clone(),
            message.thread_id.clone(),
            message.text.clone(),
        ));
        Ok(SentMessage {
            channel: message.channel.clone(),
            message_id: "1".to_string(),
            thread_id: message.thread_id.clone(),
            platform: "slack".to_string(),
        })
    }
}

/// The person's Desk on their own computer, linked to agent `agent` here.
fn their_desk(agent: &str) -> Owner {
    Owner {
        person: Uuid::from_u128(7),
        source: crate::desk_knows::sync::LOCAL_SOURCE.into(),
        agent: agent.into(),
    }
}

/// `words` asked of `agent` in a Slack DM thread, kept on its hub.
fn asked(agent: &str, words: &str, session: Option<String>) -> Asked {
    Asked {
        id: Uuid::new_v4(),
        person: their_desk(agent).person,
        agent: agent.into(),
        agent_name: "Desk".into(),
        words: words.into(),
        session,
        session_name: None,
        platform: "slack".into(),
        channel: "D1".into(),
        thread: Some("1700.1".into()),
        asked_at: chrono::Utc::now(),
    }
}

/// Takes what waits for the person's Desk in `knows` and hands it to
/// `crew` from `person`'s link, as their Desk's sync does: twice, as a
/// Desk that died mid-delivery would.
async fn taken(crew: &Arc<CrewHub>, knows: &MemoryKnows, agent: &str, person: &str) {
    let desk = their_desk(agent);
    for h in knows.take_handed(&desk, chrono::Utc::now()).await.unwrap() {
        let h = crate::desk_knows::sync::local_handed(&format!("local/{agent}"), &h);
        crew.take_handed(person, h.clone()).await.unwrap();
        crew.take_handed(person, h).await.unwrap();
    }
}

/// Sends what Desk owes back, as its sync does for an agent of this hub.
async fn says(crew: &CrewHub, knows: &MemoryKnows, agent: &str) {
    let reader = Reader {
        person: their_desk(agent).person,
        agent: agent.into(),
    };
    let store = crew.desk.store();
    for o in store.handed_owed(50).await.unwrap() {
        crate::desk_knows::handed::say(&crew.state, knows, &reader, o.handed, &o.text)
            .await
            .unwrap();
        store.handed_sent(o.id).await.unwrap();
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn a_request_handed_from_slack_is_held_by_the_boss_and_its_stall_told_back_there() {
    with_test_pool(workforce_desk::store::build_pool, |pool| async move {
        let dir = tempfile::tempdir().unwrap();
        let base = std::fs::canonicalize(dir.path()).unwrap();
        let gh = mantis(chrono::Utc::now(), 22, "diverged").await;
        let (boss, crew, d, llm) = boss_on(&base, pool, &checking(5), &gh).await;
        let slack = Arc::new(Posted::default());
        boss.surface_registry
            .write()
            .await
            .register(SurfaceKey::platform("slack"), slack.clone());
        let knows = MemoryKnows::new();
        knows.hand(&asked("desk", WORDS, None)).await.unwrap();

        taken(&crew, &knows, "desk", PERSON).await;
        eventually("the boss's turn on it", async || {
            (asked_with(&llm, &[WORDS, "passed on by Desk", "crew.plan"]).await > 0).then_some(())
        })
        .await;
        let dm = boss.sessions.find(DESK_CHANNEL, PERSON).unwrap().unwrap();
        let theirs = dm
            .messages
            .iter()
            .find(|m| m.content == WORDS)
            .expect("their words, exactly, in the boss's DM");
        let author = eventually("kept as relayed", async || {
            d.store()
                .authors(std::slice::from_ref(&theirs.id))
                .await
                .ok()?
                .remove(&theirs.id)
        })
        .await;
        assert_eq!(author.kind, SpeakerKind::Overseer);
        assert_eq!(author.id, "local/desk");
        // The fixture's person has no name in the identity store.
        assert_eq!(author.name, "Desk, in Slack");

        eventually("its word back", async || {
            d.store().handed_owed(50).await.ok()?.first().map(|_| ())
        })
        .await;
        says(&crew, &knows, "desk").await;
        let answered = slack.saying("has your request and answered");
        assert_eq!(answered.len(), 1, "delivered once, though taken twice");
        assert_eq!(answered[0].0, "D1");
        assert_eq!(answered[0].1.as_deref(), Some("1700.1"));
        assert_eq!(asked_with(&llm, &[WORDS, "passed on by Desk"]).await, 1);

        // The boss keeps it as a goal on the person's words; CI stays slow.
        goal_running(&d, &base, super::goal_checks::fast_ci(false)).await;
        let stalled = eventually("its stall told back in Slack", async || {
            says(&crew, &knows, "desk").await;
            let told = slack.saying("Desk (your Desk boss): No progress on");
            (!told.is_empty()).then_some(told)
        })
        .await;
        assert_eq!(stalled[0].0, "D1");
        assert_eq!(stalled[0].1.as_deref(), Some("1700.1"));
        assert!(stalled[0]
            .2
            .contains("Mantis CI tests only what a change touches"));
    })
    .await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_request_with_no_boss_for_it_or_from_another_persons_link_is_said_back_and_never_delivered(
) {
    with_test_pool(workforce_desk::store::build_pool, |pool| async move {
        let dir = tempfile::tempdir().unwrap();
        let base = std::fs::canonicalize(dir.path()).unwrap();
        let gh = mantis(chrono::Utc::now(), 22, "diverged").await;
        let (boss, crew, _, llm) = boss_on(&base, pool, &checking(5), &gh).await;
        let slack = Arc::new(Posted::default());
        boss.surface_registry
            .write()
            .await
            .register(SurfaceKey::platform("slack"), slack.clone());
        let knows = MemoryKnows::new();
        knows
            .hand(&asked("desk", WORDS, Some(Uuid::new_v4().to_string())))
            .await
            .unwrap();
        taken(&crew, &knows, "desk", PERSON).await;
        knows
            .hand(&asked("desk", "keep the docs green", None))
            .await
            .unwrap();
        taken(&crew, &knows, "desk", "someone-else").await;

        says(&crew, &knows, "desk").await;
        assert_eq!(slack.saying("doesn't have the session").len(), 1);
        assert_eq!(slack.saying("This Desk isn't yours").len(), 1);
        assert_eq!(asked_with(&llm, &["passed on by Desk"]).await, 0);
    })
    .await;
}
