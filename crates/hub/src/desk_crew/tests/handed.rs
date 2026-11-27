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

use super::goal_checks::{checking, mantis};
use super::goals::boss_on;
use super::plan_watch::asked_with;
use super::*;
use crate::desk_knows::store::{Handed as Asked, KnowsStore, MemoryKnows, Owner, Reader};

const WORDS: &str = "ok keep Mantis CI fast on track in desk please";
const GOAL: &str = "keep Mantis CI fast";

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
        workspace: Some("T1".into()),
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

/// The person's `words` in their Slack DM thread with `agent`, handed by
/// its tool from that turn to `knows`: what the tool answered.
async fn handed_in_slack(
    agent: &Arc<AgentService>,
    knows: &Arc<MemoryKnows>,
    words: &str,
    session: &str,
) -> Value {
    use workforce_agent::tools::Tool as _;
    let scheduler = Arc::new(crate::scheduler::Scheduler::new(Arc::clone(agent)));
    let key = "slack:T1:D1:1700.1";
    scheduler
        .set_messaging_context(key, "D1".into(), Some("1700.1".into()))
        .await;
    let desk = their_desk(agent.agent_id.as_str());
    let mut dm = workforce_common::session::Session::new(key, &desk.person.to_string(), "desk");
    dm.context_type = Some(workforce_common::session::SessionContextType::Dm);
    // The turn's own message: not yet in the session store.
    dm.messages
        .push(workforce_common::session::SessionMessage::user(
            words, None, None, None,
        ));
    let work = crate::desk_knows::tools::Work {
        store: Arc::clone(knows) as Arc<dyn KnowsStore>,
        reader: Reader {
            person: desk.person,
            agent: desk.agent,
        },
    };
    let tool = crate::desk_knows::handed::tool_for(work, "Desk".into(), &dm, Some(scheduler));
    let r = workforce_agent::runner::CURRENT_SESSION_KEY
        .scope(
            key.into(),
            tool.execute(json!({ "person_said": words, "session": session })),
        )
        .await
        .unwrap();
    assert!(r.success, "{r:?}");
    r.output
}

#[tokio::test(flavor = "multi_thread")]
async fn a_request_handed_from_slack_is_kept_as_a_goal_on_the_persons_words_and_its_stall_told_back_there(
) {
    with_test_pool(workforce_desk::store::build_pool, |pool| async move {
        let dir = tempfile::tempdir().unwrap();
        let base = std::fs::canonicalize(dir.path()).unwrap();
        let gh = mantis(chrono::Utc::now(), 22, "diverged").await;
        let (boss, crew, d, llm) = boss_on(&base, pool, &checking(5), &gh).await;
        let slack = Arc::new(Posted::default());
        boss.surface_registry
            .write()
            .await
            .register(SurfaceKey::workspace("slack", "T1"), slack.clone());
        let r = super::goals::run(
            &boss,
            "crew.start_session",
            json!({ "folder": base.to_string_lossy(), "goal": "Make Mantis CI fast", "reason": "Sam asked" }),
        )
        .await;
        assert!(r.success, "{r:?}");
        let s = eventually("its session", async || {
            d.list().into_iter().find(|v| v.goal == "Make Mantis CI fast")
        })
        .await;
        // What the person's Desk shares with the agent: the session's account.
        let knows = MemoryKnows::new();
        knows
            .push(
                &their_desk("desk"),
                &[crate::desk_knows::store::Item {
                    kind: "account".into(),
                    reference: s.id.to_string(),
                    session: Some(s.id.to_string()),
                    group_name: None,
                    at: chrono::Utc::now(),
                    body: json!({ "name": "Make Mantis CI fast", "text": "Make Mantis CI fast" }),
                }],
                chrono::Utc::now(),
            )
            .await
            .unwrap();
        knows
            .take_handed(&their_desk("desk"), chrono::Utc::now())
            .await
            .unwrap();
        let queued = handed_in_slack(&boss, &knows, WORDS, &s.id.to_string()).await;
        assert!(
            queued["message"].as_str().unwrap().starts_with("Handed to your Desk."),
            "{queued}"
        );
        taken(&crew, &knows, "desk", PERSON).await;

        let theirs = eventually("their words, exactly, in the boss's DM", async || {
            let dm = boss.sessions.find(DESK_CHANNEL, PERSON).ok()??;
            dm.messages.into_iter().find(|m| m.content == WORDS)
        })
        .await;
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

        let answered = eventually("its word back", async || {
            says(&crew, &knows, "desk").await;
            let told = slack.saying("has your request and answered");
            (!told.is_empty()).then_some(told)
        })
        .await;
        assert_eq!(answered.len(), 1, "delivered once, though taken twice");
        assert_eq!(answered[0].0, "D1");
        assert_eq!(answered[0].1.as_deref(), Some("1700.1"));
        assert_eq!(asked_with(&llm, &[WORDS, "passed on by Desk"]).await, 1);

        // The boss keeps it as a goal on the person's words, relayed: they
        // read as theirs.
        let r = super::goals::run(
            &boss,
            "crew.plan",
            json!({
                "goal": GOAL,
                "agreed": { "person_said": "keep Mantis CI fast" },
                "result": { "words": "Done when CI on main is green", "parts": [{
                    "kind": "ci", "repo": "acme/mantis", "branch": "main",
                    "want": { "kind": "green", "runs": 1 } }] },
                "tickets": [{ "session": s.id.to_string() }],
            }),
        )
        .await;
        assert!(r.success, "{r:?}");
        let id: uuid::Uuid = r.output["proposed"]["plan_id"].as_str().unwrap().parse().unwrap();
        let p = d.plan(id).await.unwrap().summary.plan;
        assert_eq!(p.agreed.as_deref(), Some("keep Mantis CI fast"));
        assert_eq!(p.state, workforce_desk::plan::PlanState::Running);

        // CI stays slow.
        let stalled = eventually("its stall told back in Slack", async || {
            says(&crew, &knows, "desk").await;
            let told = slack.saying("Desk (your Desk boss): No progress on");
            (!told.is_empty()).then_some(told)
        })
        .await;
        assert_eq!(stalled[0].0, "D1");
        assert_eq!(stalled[0].1.as_deref(), Some("1700.1"));
        assert!(stalled[0].2.contains(GOAL));
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

#[tokio::test(flavor = "multi_thread")]
async fn a_request_taken_again_after_desk_stopped_is_delivered_or_said_to_wait_in_the_dm() {
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
        let store = d.store();

        // Kept, then Desk stopped before delivering it.
        let never = asked("desk", WORDS, None);
        knows.hand(&never).await.unwrap();
        let taken_first = knows
            .take_handed(&their_desk("desk"), chrono::Utc::now())
            .await
            .unwrap();
        store
            .handed_keep(&crate::desk_knows::sync::local_handed(
                "local/desk",
                &taken_first[0],
            ))
            .await
            .unwrap();
        // Delivered, then Desk stopped before its boss answered.
        let lost = asked("desk", "keep the docs green", None);
        let lost_kept = crate::desk_knows::sync::local_handed("local/desk", &lost);
        store.handed_keep(&lost_kept).await.unwrap();
        store.handed_delivered(lost.id, "desk").await.unwrap();

        // The hub offers both again after a while.
        for h in [&taken_first[0], &lost] {
            crew.take_handed(
                PERSON,
                crate::desk_knows::sync::local_handed("local/desk", h),
            )
            .await
            .unwrap();
        }
        eventually("the undelivered one delivered", async || {
            (asked_with(&llm, &[WORDS, "passed on by Desk"]).await > 0).then_some(())
        })
        .await;
        knows.hand(&lost).await.unwrap();
        eventually("both said back", async || {
            says(&crew, &knows, "desk").await;
            (!slack.saying("has your request and answered").is_empty()).then_some(())
        })
        .await;
        assert_eq!(
            slack
                .saying("has your request in your Desk DM, but Desk stopped before it answered")
                .len(),
            1
        );
        assert_eq!(
            asked_with(&llm, &["keep the docs green", "passed on by Desk"]).await,
            0
        );
    })
    .await;
}
