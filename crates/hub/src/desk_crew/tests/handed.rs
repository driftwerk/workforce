//! A person asks their agent in Slack to keep work on track in Desk
//! (DW-770, 25 Nov): the request waits on the agent's hub, their Desk's
//! sync takes it, its boss holds it as their own words relayed by the agent,
//! and the goal's stall is told back in the Slack thread it was asked in.

use uuid::Uuid;
use workforce_common::channels::SurfaceKey;
use workforce_common::settings::DeskKnowsSyncConfig;
use workforce_desk::conversations::SpeakerKind;

use super::goal_checks::{checking, mantis};
use super::goals::boss_on;
use super::plan_watch::asked_with;
use super::*;
use crate::desk_knows::store::{Handed as Asked, KnowsStore, MemoryKnows, Owner, Reader};
use crate::desk_knows::sync::KnowsSync;
use crate::serve::testing::SentToSlack;

const WORDS: &str = "ok keep Mantis CI fast on track in desk please";
const GOAL: &str = "keep Mantis CI fast";

/// The person, as their own computer's Desk and its links name them.
fn owner() -> String {
    Uuid::from_u128(7).to_string()
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
        conversation: None,
        asked_at: chrono::Utc::now(),
    }
}

/// The crew's Desk sync, its person `person`, over `knows` for this hub's
/// own agents.
fn sync_of(crew: &CrewHub, d: &Desk, knows: &Arc<MemoryKnows>, person: &str) -> KnowsSync {
    let person = person.to_string();
    KnowsSync::new(
        Arc::clone(&crew.state),
        d.clone(),
        DeskKnowsSyncConfig::default(),
        Arc::new(move || Some(person.clone())),
    )
    .over(Arc::clone(knows) as Arc<dyn KnowsStore>)
}

/// The person's link to this hub's agent `desk`, as Desk keeps it, with
/// whether it may hand their boss requests: `None` as it is until they say.
async fn link(d: &Desk, may_hand: Option<bool>) {
    let rule = workforce_desk::knows::Rule::new(&[], false, Default::default()).unwrap();
    d.store()
        .knows_rule_put("local/desk", &rule, may_hand, chrono::Utc::now())
        .await
        .unwrap();
}

/// `boss`'s Desk DM with the person, theirs, the newest: the crew reads
/// them as the person whose Desk this is, with GitHub as their key reads it.
fn their_dm(boss: &AgentService, person: &str) {
    boss.vault
        .as_ref()
        .expect("the agent's vault")
        .set(
            &format!("users/{person}/skills/github/token"),
            "their-token",
        )
        .unwrap();
    let dm = boss
        .sessions
        .get_or_create(DESK_CHANNEL, person, boss.agent_id.as_str())
        .unwrap();
    boss.sessions.save(&dm).unwrap();
}

/// The person's `words` in their Slack DM thread with `agent`, handed by
/// its tool from that turn to `knows`: what the tool answered, and the
/// agent's session there.
async fn handed_in_slack(
    agent: &Arc<AgentService>,
    knows: &Arc<MemoryKnows>,
    words: &str,
    session: &str,
) -> (Value, String) {
    use workforce_agent::tools::Tool as _;
    let scheduler = Arc::new(crate::scheduler::Scheduler::new(Arc::clone(agent)));
    let key = "slack:T1:D1:1700.1";
    scheduler
        .set_messaging_context(key, "D1".into(), Some("1700.1".into()))
        .await;
    let desk = their_desk(agent.agent_id.as_str());
    let mut dm = workforce_common::session::Session::new(key, &desk.person.to_string(), "desk");
    dm.context_type = Some(workforce_common::session::SessionContextType::Dm);
    agent.sessions.save(&dm).unwrap();
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
            workforce_agent::runner::CURRENT_SESSION_ID.scope(
                dm.id.clone(),
                tool.execute(json!({ "person_said": words, "session": session })),
            ),
        )
        .await
        .unwrap();
    assert!(r.success, "{r:?}");
    (r.output, dm.id)
}

/// What `sent` holds that says `words`.
fn saying(slack: &SentToSlack, words: &str) -> Vec<(String, Option<String>, String)> {
    slack
        .sent()
        .into_iter()
        .filter(|(_, _, text)| text.contains(words))
        .collect()
}

#[tokio::test(flavor = "multi_thread")]
async fn a_request_handed_from_slack_is_kept_as_a_goal_on_the_persons_words_and_its_stall_told_back_there(
) {
    with_test_pool(workforce_desk::store::build_pool, |pool| async move {
        let dir = tempfile::tempdir().unwrap();
        let base = std::fs::canonicalize(dir.path()).unwrap();
        let gh = mantis(chrono::Utc::now(), 22, "diverged").await;
        let (boss, crew, d, llm) = boss_on(&base, pool, &checking(5), &gh).await;
        let slack = SentToSlack::on(&boss).await;
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
        their_dm(&boss, &owner());
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
            .take_handed(&their_desk("desk"), chrono::Utc::now(), &[])
            .await
            .unwrap();
        let (queued, conversation) = handed_in_slack(&boss, &knows, WORDS, &s.id.to_string()).await;
        assert!(
            queued["message"].as_str().unwrap().starts_with("Handed to your Desk."),
            "{queued}"
        );
        link(&d, Some(true)).await;
        let sync = sync_of(&crew, &d, &knows, &owner());
        sync.take("local/desk").await.unwrap();

        let theirs = eventually("their words, exactly, in the boss's DM", async || {
            let dm = boss.sessions.find(DESK_CHANNEL, &owner()).ok()??;
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
        assert_eq!(author.name, "Desk, in Slack");

        let answered = eventually("its word back", async || {
            sync.says().await;
            let told = saying(&slack, "isn't kept as a goal yet");
            (!told.is_empty()).then_some(told)
        })
        .await;
        assert_eq!(answered.len(), 1);
        assert_eq!(answered[0].0, "D1");
        assert_eq!(answered[0].1.as_deref(), Some("1700.1"));
        assert_eq!(asked_with(&llm, &[WORDS, "Desk in Slack to pass this on"]).await, 1);
        let told_agent = boss.sessions.get(&conversation).unwrap().unwrap();
        assert!(
            told_agent
                .messages
                .iter()
                .any(|m| m.content == answered[0].2),
            "the agent knows what its person was told"
        );

        // Relayed, their words are never read as typed in the DM.
        assert!(crew.agreed_at(&boss, GOAL, GOAL).is_err());
        let other = super::goals::run(
            &boss,
            "crew.plan",
            json!({
                "goal": "Ship the docs site",
                "agreed": { "person_said": GOAL },
                "result": { "words": "Done when CI on main is green", "parts": [{
                    "kind": "ci", "repo": "acme/mantis", "branch": "main",
                    "want": { "kind": "green", "runs": 1 } }] },
            }),
        )
        .await;
        assert!(!other.success, "they agree their own goal alone: {other:?}");

        // They agree the goal they say, once.
        let plan = |goal: &str| {
            json!({
                "goal": goal,
                "agreed": { "person_said": GOAL },
                "result": { "words": "Done when CI on main is green", "parts": [{
                    "kind": "ci", "repo": "acme/mantis", "branch": "main",
                    "want": { "kind": "green", "runs": 1 } }] },
                "tickets": [{ "session": s.id.to_string() }],
            })
        };
        let (a, b) = tokio::join!(
            super::goals::run(&boss, "crew.plan", plan(GOAL)),
            super::goals::run(&boss, "crew.plan", plan(GOAL))
        );
        assert!(a.success ^ b.success, "one plan of two at once: {a:?} {b:?}");
        let r = if a.success { a } else { b };
        let id: uuid::Uuid = r.output["proposed"]["plan_id"].as_str().unwrap().parse().unwrap();
        let p = d.plan(id).await.unwrap().summary.plan;
        assert_eq!(p.agreed.as_deref(), Some(GOAL));
        assert_eq!(p.state, workforce_desk::plan::PlanState::Running);
        let again = super::goals::run(&boss, "crew.plan", plan(GOAL)).await;
        assert!(!again.success, "a plan carries it already: {again:?}");

        // CI stays slow.
        let stalled = eventually("its stall told back in Slack", async || {
            sync.says().await;
            let told = saying(&slack, "Desk (your Desk boss): No progress on");
            (!told.is_empty()).then_some(told)
        })
        .await;
        assert_eq!(stalled[0].0, "D1");
        assert_eq!(stalled[0].1.as_deref(), Some("1700.1"));
        assert!(stalled[0].2.contains(GOAL));

        // Its plan ended, the request agrees no other.
        d.drop_plan(id, workforce_desk::OnDrop::Leave, &By::you())
            .await
            .unwrap();
        let mut untaken = plan(GOAL);
        untaken["tickets"] = json!([]);
        let after = super::goals::run(&boss, "crew.plan", untaken).await;
        assert!(!after.success, "{after:?}");
        assert!(format!("{after:?}").contains("already agreed a plan"), "{after:?}");
    })
    .await;
}

/// Hands `h` to `crew` from `person`'s link, through the sync over `knows`,
/// and sends what it owes back.
async fn handed_through(
    crew: &Arc<CrewHub>,
    d: &Desk,
    knows: &Arc<MemoryKnows>,
    person: &str,
    h: &Asked,
) {
    knows.hand(h).await.unwrap();
    let sync = sync_of(crew, d, knows, person);
    sync.take("local/desk").await.unwrap();
    sync.says().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_request_desk_cannot_deliver_is_said_back_saying_why_and_never_delivered() {
    with_test_pool(workforce_desk::store::build_pool, |pool| async move {
        let dir = tempfile::tempdir().unwrap();
        let base = std::fs::canonicalize(dir.path()).unwrap();
        let gh = mantis(chrono::Utc::now(), 22, "diverged").await;
        let (boss, crew, d, llm) = boss_on(&base, pool, &checking(5), &gh).await;
        let slack = SentToSlack::on(&boss).await;
        boss.surface_registry.write().await.register(
            SurfaceKey::workspace("slack", "T1"),
            Arc::clone(&slack) as _,
        );
        their_dm(&boss, &owner());
        link(&d, Some(true)).await;
        let knows = MemoryKnows::new();
        let asking = |words: &str, session: Option<String>| asked("desk", words, session);

        let gone = asking(
            "keep the old work on track please",
            Some(Uuid::new_v4().to_string()),
        );
        handed_through(&crew, &d, &knows, &owner(), &gone).await;
        assert_eq!(
            saying(&slack, "doesn't have the session you named").len(),
            1
        );

        let unreadable = asking(
            "keep that thing on track please",
            Some("not a session".into()),
        );
        handed_through(&crew, &d, &knows, &owner(), &unreadable).await;
        assert_eq!(saying(&slack, "couldn't tell which session").len(), 1);

        let long = asking(
            &format!("keep this on track {}", "please ".repeat(200)),
            None,
        );
        handed_through(&crew, &d, &knows, &owner(), &long).await;
        assert_eq!(saying(&slack, "too long for your Desk").len(), 1);

        let theirs = asking("keep the docs green on track please", None);
        knows.hand(&theirs).await.unwrap();
        crew.take_handed(
            &Uuid::from_u128(8).to_string(),
            crate::desk_knows::sync::local_handed("local/desk", "Desk", &theirs),
        )
        .await
        .unwrap();
        sync_of(&crew, &d, &knows, &owner()).says().await;
        assert_eq!(saying(&slack, "This Desk isn't yours").len(), 1);

        assert_eq!(
            asked_with(
                &llm,
                &[
                    "keep the old work on track",
                    "Desk in Slack to pass this on"
                ]
            )
            .await
                + asked_with(
                    &llm,
                    &["keep that thing on track", "Desk in Slack to pass this on"]
                )
                .await
                + asked_with(
                    &llm,
                    &["keep the docs green", "Desk in Slack to pass this on"]
                )
                .await,
            0
        );
    })
    .await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_request_with_no_boss_over_it_is_told_what_turns_one_on() {
    with_test_pool(workforce_desk::store::build_pool, |pool| async move {
        let dir = tempfile::tempdir().unwrap();
        let base = std::fs::canonicalize(dir.path()).unwrap();
        let gh = mantis(chrono::Utc::now(), 22, "diverged").await;
        let (boss, crew, d, llm) = boss_on(&base, pool, &checking(5), &gh).await;
        let slack = SentToSlack::on(&boss).await;
        boss.surface_registry.write().await.register(
            SurfaceKey::workspace("slack", "T1"),
            Arc::clone(&slack) as _,
        );
        write_config(
            &boss,
            "[oversight]\nenabled = true\ngroup = \"acme\"\nlevel = \"act\"\n",
        );
        eventually("the boss over acme alone", async || {
            crew.overseer_for(&workforce_common::oversight::Scope::Ungrouped)
                .is_none()
                .then_some(())
        })
        .await;
        their_dm(&boss, &owner());
        link(&d, Some(true)).await;
        let knows = MemoryKnows::new();
        let loose = asked("desk", "keep the docs green on track please", None);
        handed_through(&crew, &d, &knows, &owner(), &loose).await;
        assert_eq!(saying(&slack, "You have no boss in Desk yet").len(), 1);

        let session = new_session(&d, &base, "atlas-web").await;
        d.change_layout(|l| {
            l.create(
                "atlas",
                "Atlas",
                &[workforce_desk::session_item(session.id)],
                None,
            )
        })
        .await
        .unwrap();
        let grouped = asked(
            "desk",
            "keep atlas on track please",
            Some(session.id.to_string()),
        );
        handed_through(&crew, &d, &knows, &owner(), &grouped).await;
        assert_eq!(saying(&slack, "No boss oversees").len(), 1);
        assert_eq!(
            asked_with(&llm, &["Desk in Slack to pass this on"]).await,
            0
        );
    })
    .await;
}

/// The open asks of the boss `desk` whether the agent `desk` may hand the
/// person's boss requests.
async fn hand_asks(d: &Desk) -> Vec<workforce_desk::open_item::OpenItem> {
    d.store()
        .items_not_closed(Some("desk"))
        .await
        .unwrap()
        .into_iter()
        .filter(|i| i.dedupe_key == "handed-link:local/desk")
        .collect()
}

#[tokio::test(flavor = "multi_thread")]
async fn a_request_from_a_link_not_let_hand_is_held_and_asked_once_then_passed_on_or_dropped() {
    with_test_pool(workforce_desk::store::build_pool, |pool| async move {
        const ON: &str = "Desk in Slack to pass this on";
        let dir = tempfile::tempdir().unwrap();
        let base = std::fs::canonicalize(dir.path()).unwrap();
        let gh = mantis(chrono::Utc::now(), 22, "diverged").await;
        let (boss, crew, d, llm) = boss_on(&base, pool.clone(), &checking(5), &gh).await;
        let slack = SentToSlack::on(&boss).await;
        boss.surface_registry.write().await.register(
            SurfaceKey::workspace("slack", "T1"),
            Arc::clone(&slack) as _,
        );
        their_dm(&boss, &owner());
        let knows = MemoryKnows::new();

        // A link made before: off, and never said.
        link(&d, None).await;
        let first = asked("desk", WORDS, None);
        handed_through(&crew, &d, &knows, &owner(), &first).await;
        let second = asked("desk", "keep the docs green on track please", None);
        handed_through(&crew, &d, &knows, &owner(), &second).await;
        assert_eq!(saying(&slack, "Your Desk asks you first").len(), 2);
        let asks = hand_asks(&d).await;
        assert_eq!(asks.len(), 1, "asked once");
        assert!(
            asks[0]
                .text
                .contains("asked to pass on your request from Slack"),
            "{}",
            asks[0].text
        );
        assert_eq!(asks[0].options, ["Yes", "No"]);
        crew.take_handed(
            &owner(),
            crate::desk_knows::sync::local_handed("local/desk", "Desk", &first),
        )
        .await
        .unwrap();
        sync_of(&crew, &d, &knows, &owner())
            .take("local/desk")
            .await
            .unwrap();
        assert_eq!(asked_with(&llm, &[ON]).await, 0, "held, never delivered");

        // Yes: on, and both passed on.
        let said = crew.answer_item(asks[0].id, "Yes").await.unwrap();
        assert!(said.starts_with("Turned on"), "{said}");
        eventually("both passed on", async || {
            (asked_with(&llm, &[WORDS, ON]).await == 1
                && asked_with(&llm, &["keep the docs green", ON]).await == 1)
                .then_some(())
        })
        .await;
        let rule = d.store().knows_rule("local/desk").await.unwrap().unwrap();
        assert_eq!(rule.may_hand, Some(true));

        // Never said again, then No: dropped, and told so.
        pool.get()
            .await
            .unwrap()
            .execute(
                "UPDATE knows_rules SET may_hand = NULL WHERE agent = 'local/desk'",
                &[],
            )
            .await
            .unwrap();
        let third = asked("desk", "keep the guide fresh on track please", None);
        handed_through(&crew, &d, &knows, &owner(), &third).await;
        let asks = hand_asks(&d).await;
        assert_eq!(asks.len(), 1);
        let said = crew.answer_item(asks[0].id, "No").await.unwrap();
        assert!(said.starts_with("Kept off"), "{said}");
        sync_of(&crew, &d, &knows, &owner()).says().await;
        let off = "Your Desk doesn't let me hand your boss requests";
        assert_eq!(saying(&slack, off).len(), 1);

        // Off: the next is told so at once, never asked.
        let fourth = asked("desk", "keep the site up on track please", None);
        handed_through(&crew, &d, &knows, &owner(), &fourth).await;
        assert_eq!(saying(&slack, off).len(), 2);
        assert!(hand_asks(&d).await.is_empty());
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
        assert_eq!(asked_with(&llm, &["keep the guide fresh", ON]).await, 0);
        assert_eq!(asked_with(&llm, &["keep the site up", ON]).await, 0);
    })
    .await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_request_taken_again_after_desk_stopped_is_delivered_once_or_said_to_wait_in_the_dm() {
    with_test_pool(workforce_desk::store::build_pool, |pool| async move {
        let dir = tempfile::tempdir().unwrap();
        let base = std::fs::canonicalize(dir.path()).unwrap();
        let gh = mantis(chrono::Utc::now(), 22, "diverged").await;
        let (boss, crew, d, llm) = boss_on(&base, pool, &checking(5), &gh).await;
        let slack = SentToSlack::on(&boss).await;
        their_dm(&boss, &owner());
        link(&d, Some(true)).await;
        let knows = MemoryKnows::new();
        let store = d.store();
        let kept = |h: &Asked| crate::desk_knows::sync::local_handed("local/desk", "Desk", h);

        // Kept, then Desk stopped before delivering it.
        let never = asked("desk", WORDS, None);
        knows.hand(&never).await.unwrap();
        store.handed_keep(&kept(&never)).await.unwrap();
        // Delivered, then Desk stopped before its boss answered.
        let lost = asked("desk", "keep the docs green on track please", None);
        knows.hand(&lost).await.unwrap();
        store.handed_keep(&kept(&lost)).await.unwrap();
        store
            .handed_delivered(&kept(&lost), "desk", chrono::Utc::now())
            .await
            .unwrap();

        // The hub offers both again; the first is taken twice at once.
        for h in [&never, &never, &lost] {
            crew.take_handed(&owner(), kept(h)).await.unwrap();
        }
        eventually("the undelivered one delivered", async || {
            (asked_with(&llm, &[WORDS, "Desk in Slack to pass this on"]).await > 0).then_some(())
        })
        .await;
        let sync = sync_of(&crew, &d, &knows, &owner());
        eventually("both said back", async || {
            sync.says().await;
            (!saying(&slack, "isn't kept as a goal yet").is_empty()).then_some(())
        })
        .await;
        assert_eq!(
            asked_with(&llm, &[WORDS, "Desk in Slack to pass this on"]).await,
            1,
            "delivered once"
        );
        assert_eq!(
            saying(
                &slack,
                "has your request in your Desk DM, but Desk stopped before it answered"
            )
            .len(),
            1
        );
        assert_eq!(
            asked_with(
                &llm,
                &["keep the docs green", "Desk in Slack to pass this on"]
            )
            .await,
            0
        );
    })
    .await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_word_the_hub_cannot_post_is_kept_to_try_again_and_one_it_never_knew_is_done() {
    with_test_pool(workforce_desk::store::build_pool, |pool| async move {
        let dir = tempfile::tempdir().unwrap();
        let base = std::fs::canonicalize(dir.path()).unwrap();
        let gh = mantis(chrono::Utc::now(), 22, "diverged").await;
        let (boss, crew, d, _llm) = boss_on(&base, pool, &checking(5), &gh).await;
        their_dm(&boss, &owner());
        let knows = MemoryKnows::new();
        let store = d.store();
        // No surface for Slack: the hub can't post Desk's word.
        let unknown = crate::desk_knows::sync::local_handed(
            "local/desk",
            "Desk",
            &asked("desk", "keep the docs green on track please", None),
        );
        store.handed_keep(&unknown).await.unwrap();
        store
            .handed_say(&unknown, "Your Desk boss has it.")
            .await
            .unwrap();
        let h = asked("desk", WORDS, None);
        knows.hand(&h).await.unwrap();
        let kept = crate::desk_knows::sync::local_handed("local/desk", "Desk", &h);
        store.handed_keep(&kept).await.unwrap();
        store
            .handed_say(&kept, "Your Desk boss has it.")
            .await
            .unwrap();

        let sync = sync_of(&crew, &d, &knows, &owner());
        sync.says().await;
        let owed = store.handed_owed(10).await.unwrap();
        assert_eq!(owed.len(), 1, "the one never known is done with");
        assert_eq!(owed[0].handed, h.id);
        sync.says().await;
        assert_eq!(
            store.handed_owed(10).await.unwrap().len(),
            1,
            "waiting out its back-off, untried"
        );
    })
    .await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_machines_desk_takes_no_request() {
    with_test_pool(workforce_desk::store::build_pool, |pool| async move {
        let dir = tempfile::tempdir().unwrap();
        let base = std::fs::canonicalize(dir.path()).unwrap();
        let gh = mantis(chrono::Utc::now(), 22, "diverged").await;
        let machine_dir = tempfile::tempdir().unwrap();
        let machine = super::machine_desk(pool.clone(), machine_dir.path()).await;
        let (boss, crew, _d, llm) = boss_on(&base, pool, &checking(5), &gh).await;
        their_dm(&boss, &owner());
        link(&machine, Some(true)).await;
        let knows = MemoryKnows::new();
        knows.hand(&asked("desk", WORDS, None)).await.unwrap();
        sync_of(&crew, &machine, &knows, &owner()).pass().await;
        assert_eq!(
            knows
                .checked_in(&Reader {
                    person: their_desk("desk").person,
                    agent: "desk".into(),
                })
                .await
                .unwrap(),
            None
        );
        assert_eq!(
            knows
                .take_handed(&their_desk("desk"), chrono::Utc::now(), &[])
                .await
                .unwrap()
                .len(),
            1,
            "still waiting for the person's own computer's Desk"
        );
        assert_eq!(
            asked_with(&llm, &["Desk in Slack to pass this on"]).await,
            0
        );
    })
    .await;
}

/// A hub answering Desk's word on each request with the status `statuses`
/// gives its id, counting what it is sent.
async fn hub_answering(statuses: Vec<(Uuid, u16)>) -> (String, Arc<parking_lot::Mutex<Vec<Uuid>>>) {
    let seen = Arc::new(parking_lot::Mutex::new(Vec::new()));
    let log = Arc::clone(&seen);
    let app = axum::Router::new().route(
        "/api/desk-knows/handed/{id}/say",
        axum::routing::post(move |axum::extract::Path(id): axum::extract::Path<Uuid>| {
            let (log, statuses) = (Arc::clone(&log), statuses.clone());
            async move {
                log.lock().push(id);
                let status = statuses
                    .iter()
                    .find(|(h, _)| *h == id)
                    .map_or(500, |(_, s)| *s);
                StatusCode::from_u16(status).unwrap()
            }
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    (url, seen)
}

#[tokio::test(flavor = "multi_thread")]
async fn a_word_another_hub_can_never_take_is_done_and_one_it_refuses_now_is_kept() {
    with_test_pool(workforce_desk::store::build_pool, |pool| async move {
        let dir = tempfile::tempdir().unwrap();
        let base = std::fs::canonicalize(dir.path()).unwrap();
        let gh = mantis(chrono::Utc::now(), 22, "diverged").await;
        let (_boss, _crew, d, _llm) = boss_on(&base, pool, &checking(5), &gh).await;
        let gone = Uuid::new_v4();
        let refused = Uuid::new_v4();
        let (url, seen) = hub_answering(vec![(gone, 404), (refused, 401)]).await;
        let hub = workforce_desk::hub_id_for_url(&url).unwrap();
        let agent = format!("{hub}/allan");
        let vault = crate::vault::Vault::open(
            &dir.path().join("links.json"),
            &crate::vault::Vault::generate_master_key(),
        )
        .unwrap();
        vault
            .set(
                &crate::desk_knows::sync::link_key(&hub, "allan"),
                &json!({ "url": url, "token": format!("wfk_{}", "a".repeat(40)), "person": owner() })
                    .to_string(),
            )
            .unwrap();
        let state = Arc::new(crate::gateway::GatewayState {
            vault: Some(Arc::new(vault)),
            ..Arc::try_unwrap(crate::remote_mcp::testing::fixture(false).state)
                .ok()
                .expect("the fixture's only handle")
        });
        let store = d.store();
        for id in [gone, refused] {
            let h = workforce_desk::handed::Handed {
                id,
                agent: agent.clone(),
                agent_name: "allan".into(),
                place: "Slack".into(),
                words: WORDS.into(),
                session: None,
                asked_at: chrono::Utc::now(),
                boss: None,
                delivered_at: None,
            };
            store.handed_keep(&h).await.unwrap();
            store.handed_say(&h, "Your Desk boss has it.").await.unwrap();
        }
        let person = owner();
        let sync = KnowsSync::new(
            state,
            d.clone(),
            DeskKnowsSyncConfig::default(),
            Arc::new(move || Some(person.clone())),
        );
        sync.says().await;
        assert_eq!(seen.lock().as_slice(), [gone, refused]);
        let owed = store.handed_owed(10).await.unwrap();
        assert_eq!(owed.len(), 1, "404 is done with; 401 is kept");
        assert_eq!(owed[0].handed, refused);
    })
    .await;
}
