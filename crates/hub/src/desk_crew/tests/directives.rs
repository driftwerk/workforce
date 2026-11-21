//! Act fully's directives: a boss newly at Act fully posts one card asking
//! them, never again while it stays there, and again once Act fully is
//! turned on afresh; nothing is posted with everything answered or when
//! the person said to stop asking; every turn is told the answers as orders
//! and what's still its own call; and the person answers them in the chat
//! through crew.directives.

use super::unbound::UNBOUND;
use super::*;
use workforce_desk::preference::MAIN;
use workforce_desk::rooms::YOU;

/// Act fully with a daily budget: held at Act until today's spend is read.
const FULL: &str = "[oversight]\nenabled = true\nlevel = \"full\"\n[oversight.full]\nanywhere = true\nbudget_usd_per_day = 25\n";
const WATCH: &str = "[oversight]\nenabled = true\nlevel = \"watch\"\n";

/// Writes `toml` as the boss's config, its time moved on a second so it is
/// read afresh (two writes in one clock tick would share a time).
fn set_level(overseer: &AgentService, toml: &str) {
    write_config(overseer, toml);
    let path = overseer
        .workspace
        .join(workforce_common::config_files::WORKSPACE_CONFIG);
    let f = std::fs::File::options().write(true).open(&path).unwrap();
    let was = f.metadata().unwrap().modified().unwrap();
    static BUMP: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
    let n = BUMP.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    f.set_modified(was + std::time::Duration::from_secs(n))
        .unwrap();
}

/// The cards once `n` are out, or what's out after a few seconds: the
/// crew's own minute may have taken a look first and still be posting.
async fn cards_by(
    overseer: &AgentService,
    n: usize,
) -> Vec<workforce_common::session::SessionMessage> {
    for _ in 0..50 {
        let c = cards(overseer);
        if c.len() >= n {
            return c;
        }
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
    cards(overseer)
}

fn cards(overseer: &AgentService) -> Vec<workforce_common::session::SessionMessage> {
    overseer
        .sessions
        .find(DESK_CHANNEL, PERSON)
        .unwrap()
        .map(|dm| {
            dm.messages
                .into_iter()
                .filter(|m| m.content.starts_with("Before I run on my own"))
                .collect()
        })
        .unwrap_or_default()
}

#[tokio::test(flavor = "multi_thread")]
async fn a_boss_newly_at_act_fully_asks_the_persons_directives_once() {
    with_test_pool(workforce_desk::store::build_pool, |pool| async move {
        let dir = tempfile::tempdir().unwrap();
        let (overseer, crew, d, _llm) = overseeing(dir.path(), pool, "ok", FULL, &[]).await;
        // The person is the DM's.
        overseer
            .sessions
            .get_or_create(DESK_CHANNEL, PERSON, overseer.agent_id.as_str())
            .unwrap();
        crew.look_at_directives().await;
        let posted = cards_by(&overseer, 1).await;
        assert_eq!(posted.len(), 1, "one card");
        let card = &posted[0];
        assert!(card.content.contains("merging PRs"), "{}", card.content);
        let extras = d
            .store()
            .extras(std::slice::from_ref(&card.id))
            .await
            .unwrap();
        assert_eq!(
            extras[&card.id].directives.as_ref().unwrap().boss_scope,
            MAIN
        );
        let asking = d.store().directive_asking(YOU, MAIN).await.unwrap();
        assert_eq!(asking.card_message.as_deref(), Some(card.id.as_str()));
        // Still at Act fully: never again, even while it is held at Act
        // (its budget spent for the day, or not read after a restart).
        let id = overseer.agent_id.as_str().to_string();
        crew.pin_spent_today(&id, 100.0);
        assert_ne!(
            crew.level_now(&id, &crew.overseers()[0].1).0,
            workforce_common::oversight::Level::Full
        );
        crew.look_at_directives().await;
        crew.pin_spent_today(&id, 0.0);
        // Two looks at once (another instance's, the minute's) post one.
        tokio::join!(crew.look_at_directives(), crew.look_at_directives());
        assert_eq!(cards(&overseer).len(), 1);

        // Off Act fully, then on again: asked afresh while some are open.
        set_level(&overseer, WATCH);
        crew.look_at_directives().await;
        assert!(d
            .store()
            .directive_asking(YOU, MAIN)
            .await
            .unwrap()
            .full_since
            .is_none());
        set_level(&overseer, FULL);
        crew.look_at_directives().await;
        assert_eq!(cards_by(&overseer, 2).await.len(), 2);

        // Stopped: off and on again asks nothing.
        d.store()
            .directive_asking_change(YOU, MAIN, None, Some(true))
            .await
            .unwrap();
        set_level(&overseer, WATCH);
        crew.look_at_directives().await;
        set_level(&overseer, FULL);
        crew.look_at_directives().await;
        assert_eq!(cards(&overseer).len(), 2);
    })
    .await;
}

#[tokio::test(flavor = "multi_thread")]
async fn the_boss_is_told_its_directives_as_orders_and_answers_them_from_the_chat() {
    with_test_pool(workforce_desk::store::build_pool, |pool| async move {
        let dir = tempfile::tempdir().unwrap();
        let (_overseer, crew, d, _llm) = overseeing(dir.path(), pool, "ok", UNBOUND, &[]).await;
        let tool =
            super::super::tools::CrewTool::for_level(&crew, "desk", &Default::default(), false)
                .into_iter()
                .find(|t| workforce_agent::tools::Tool::name(t) == super::super::directives::NAME)
                .expect("crew.directives at every level");
        let run = |input: Value| {
            let tool = &tool;
            async move {
                let r = workforce_agent::tools::Tool::execute(tool, input)
                    .await
                    .unwrap();
                format!("{} {}", r.output, r.error.unwrap_or_default())
            }
        };
        let said =
            run(json!({ "action": "answer", "key": "merge", "answer": "After checks pass" })).await;
        assert!(said.contains("merge only once CI is green"), "{said}");
        let said = run(json!({ "action": "add", "text": "Never touch billing" })).await;
        assert!(said.contains("Never touch billing"), "{said}");
        let listed = run(json!({})).await;
        assert!(listed.contains("answered: checks"), "{listed}");
        assert!(
            listed.contains("Their own: Never touch billing"),
            "{listed}"
        );
        let bad = run(json!({ "action": "answer", "key": "merge", "answer": "maybe" })).await;
        assert!(bad.contains("isn't an answer"), "{bad}");

        // Every turn is told them, and what's still its own call.
        let ctx = crew.preferences_context(MAIN, None, true).await.unwrap();
        assert!(
            ctx.listed.contains("they win over your own judgment"),
            "{}",
            ctx.listed
        );
        assert!(
            ctx.listed
                .contains("Merging PRs: merge only once CI is green"),
            "{}",
            ctx.listed
        );
        assert!(
            ctx.listed.contains("Their own: Never touch billing"),
            "{}",
            ctx.listed
        );
        assert!(
            ctx.listed.contains("my own call: no directive on"),
            "{}",
            ctx.listed
        );
        assert!(
            !ctx.listed.contains("The person's preferences:"),
            "no plain ones: {}",
            ctx.listed
        );

        // A directive cited by a group's boss falls back on the main boss's.
        let (v, _) = d.store().directive_view(YOU, MAIN).await.unwrap();
        assert!(v.questions[0].preference_id.is_some());

        let said = run(json!({ "action": "stop_asking" })).await;
        assert!(said.contains("won't ask"), "{said}");
        assert!(
            d.store()
                .directive_asking(YOU, MAIN)
                .await
                .unwrap()
                .stop_asking
        );
    })
    .await;
}
