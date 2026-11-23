//! At Act fully the boss runs as an orchestrator (`super::super::orchestrator`):
//! a plan of chained tickets runs to its end with no word from the person,
//! on stand-in sessions and a stand-in GitHub. Each session's turn end and
//! each watch's end is a notice in the boss's one running conversation,
//! where it acts at once: a CI wait it started itself resolves, tickets
//! move to done, the next starts, and finished sessions close.

use super::*;
use workforce_desk::board::TicketState;
use workforce_desk::crew::Party;
use workforce_desk::plan::{Draft, DraftTicket, PlanState};
use workforce_desk::NewPlan;

use super::super::orchestrator::Notice;
use super::super::watcher::check_in_channel;

const FULL: &str =
    "[oversight]\nenabled = true\nlevel = \"full\"\n[oversight.work]\ntake_on = \"all\"\n";

/// What only a notices turn's prompt says.
const NOTICES: &str = "Notices for you, at Act fully";

/// What the boss says at the end of each of its turns: its own notes.
const OWN_WORDS: &str = "BOSS-OWN-NOTES on what I did";

/// The boss at Act fully on a hub whose Claude Code is installed, its GitHub
/// `gh` with the person's own key; its DM open, its crew started.
async fn hub_on(
    dir: &Path,
    pool: workforce_postgres::PostgresPool,
    gh: &wiremock::MockServer,
) -> (Arc<AgentService>, Arc<CrewHub>, Desk, wiremock::MockServer) {
    let uri = gh.uri();
    let (overseer, llm) = agent_configured(dir, "desk", OWN_WORDS, &[], |c| {
        c.github.base_url = uri;
    })
    .await;
    overseer
        .vault
        .as_ref()
        .expect("the agent's vault")
        .set(
            &format!("users/{PERSON}/skills/github/token"),
            "their-token",
        )
        .unwrap();
    write_config(&overseer, FULL);
    let state = hub(std::slice::from_ref(&overseer));
    let d = desk_running(
        pool,
        dir,
        Default::default(),
        installed_cli(dir),
        not_installed(dir),
    )
    .await;
    let dm = overseer
        .sessions
        .get_or_create(DESK_CHANNEL, PERSON, "desk")
        .unwrap();
    overseer.sessions.save(&dm).unwrap();
    eventually("Desk to read its engines", async || {
        d.engines()
            .iter()
            .any(|v| v.engine == Engine::Claude && v.installed == Some(true))
            .then_some(())
    })
    .await;
    d.boss_bounds_with(workforce_desk::charge::Bounds {
        home: dir.to_path_buf(),
        ..Default::default()
    });
    let crew = CrewHub::start(state, d.clone(), FAST, CancellationToken::new());
    *crew.close_settle.lock() = chrono::Duration::zero();
    eventually("Desk to know it is unbound", async || {
        d.boss_unbound("desk").then_some(())
    })
    .await;
    eventually("its watch tool", async || {
        overseer.tools.has("crew.watch").then_some(())
    })
    .await;
    (overseer, crew, d, llm)
}

/// The boss's notices turn that has `marker` makes `calls`, one a round in
/// order (each round's request carries the turn, so each next call answers
/// the round after the last).
async fn acts_on(llm: &wiremock::MockServer, marker: &str, calls: &[(&str, Value)]) {
    use wiremock::matchers::{body_partial_json, body_string_contains, method, path};
    use wiremock::{Mock, ResponseTemplate};
    let chunk = |body: Value| format!("data: {body}\n\n");
    for (i, (tool, args)) in calls.iter().enumerate() {
        let sse = [
            chunk(json!({"id": "c0", "object": "chat.completion.chunk", "model": "mock",
                "choices": [{"index": 0, "delta": {"role": "assistant", "tool_calls": [
                    {"index": 0, "id": format!("call_{i}_{}", uuid::Uuid::new_v4().simple()), "type": "function",
                        "function": {"name": tool, "arguments": args.to_string()}}
                ]}, "finish_reason": null}]})),
            chunk(json!({"id": "c0", "object": "chat.completion.chunk", "model": "mock",
                "choices": [{"index": 0, "delta": {}, "finish_reason": "tool_calls"}],
                "usage": {"prompt_tokens": 10, "completion_tokens": 5, "total_tokens": 15}})),
            "data: [DONE]\n\n".to_string(),
        ]
        .concat();
        // Mounted in order at one priority: the first not used up answers.
        Mock::given(method("POST"))
            .and(path("/v1/chat/completions"))
            .and(body_string_contains(NOTICES))
            .and(body_string_contains(marker))
            .and(body_partial_json(json!({"stream": true})))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("content-type", "text/event-stream")
                    .set_body_string(sse),
            )
            .up_to_n_times(1)
            .with_priority(1)
            .mount(llm)
            .await;
    }
}

/// GitHub as the person's key reads it: pull request 5 in acme/web, its
/// checks running for the first `pending` reads and green after.
async fn github(pending: u64) -> wiremock::MockServer {
    use wiremock::matchers::{header, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};
    let gh = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/repos/acme/web/pulls/5"))
        .and(header("authorization", "Bearer their-token"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "number": 5, "title": "Login page", "state": "open", "merged": false,
            "mergeable": true, "mergeable_state": "clean",
            "head": {"ref": "login-page", "sha": "5ha"}, "base": {"ref": "main"},
            "html_url": "https://github.com/acme/web/pull/5"
        })))
        .mount(&gh)
        .await;
    if pending > 0 {
        Mock::given(method("GET"))
            .and(path("/repos/acme/web/commits/5ha/check-runs"))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(json!({"check_runs": [
                    {"name": "Test", "status": "in_progress", "conclusion": null},
                ]})),
            )
            .up_to_n_times(pending)
            .with_priority(1)
            .mount(&gh)
            .await;
    }
    Mock::given(method("GET"))
        .and(path("/repos/acme/web/commits/5ha/check-runs"))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(json!({"check_runs": [
                {"name": "Test", "status": "completed", "conclusion": "success"},
            ]})),
        )
        .with_priority(2)
        .mount(&gh)
        .await;
    Mock::given(method("GET"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!([])))
        .with_priority(5)
        .mount(&gh)
        .await;
    gh
}

/// The plan's tickets' sessions, by ticket title.
fn session_of(d: &Desk, title: &str) -> Option<workforce_desk::model::SessionView> {
    d.list().into_iter().find(|v| v.goal.contains(title))
}

/// `url`'s session takes a turn and ends it saying `said`.
async fn turn_ends(url: &str, said: &str) {
    hook(
        url,
        json!({"hook_event_name": "SessionStart", "session_id": format!("cc-{}", uuid::Uuid::new_v4())}),
    )
    .await;
    hook(
        url,
        json!({"hook_event_name": "Stop", "last_assistant_message": said}),
    )
    .await;
}

/// The prompts of the boss's notices turns, in order.
async fn notices_turns(llm: &wiremock::MockServer) -> Vec<String> {
    llm.received_requests()
        .await
        .unwrap_or_default()
        .iter()
        .map(|r| String::from_utf8_lossy(&r.body).to_string())
        .filter(|b| b.contains(NOTICES))
        .collect()
}

#[tokio::test(flavor = "multi_thread")]
async fn a_plan_of_chained_tickets_runs_to_its_end_with_no_word_from_the_person() {
    with_test_pool(workforce_desk::store::build_pool, |pool| async move {
        let dir = tempfile::tempdir().unwrap();
        let base = std::fs::canonicalize(dir.path()).unwrap();
        let gh = github(2).await;
        let (overseer, crew, d, llm) = hub_on(&base, pool, &gh).await;

        // The person's plan: the logout page needs the login page done.
        let folder = base.to_string_lossy().into_owned();
        let plan = d
            .propose_plan(
                NewPlan {
                    draft: Draft {
                        goal: "Ship sign-in".into(),
                        tickets: vec![
                            DraftTicket {
                                title: "Build the login page".into(),
                                folder: folder.clone(),
                                ..Default::default()
                            },
                            DraftTicket {
                                title: "Build the logout page".into(),
                                folder,
                                needs: vec![0],
                                ..Default::default()
                            },
                        ],
                    },
                    board: None,
                    parallel: None,
                    engine: Engine::Claude,
                    group: None,
                    budget: None,
                    result: None,
                    goal_ticket: None,
                },
                &Party::manager("desk", "Desk"),
                &By::manager("desk", "Desk", "Sam asked for sign-in."),
            )
            .await
            .unwrap();
        let key = |i: usize| plan.tickets[i].ticket.key.clone().unwrap();
        let (login_key, logout_key) = (key(0), key(1));
        d.approve_plan(plan.summary.plan.id, 1, &By::you())
            .await
            .unwrap();

        // Its first ticket starts; the second waits on it.
        let login = eventually("the login ticket's session", async || {
            session_of(&d, "Build the login page")
        })
        .await;
        tokio::time::sleep(Duration::from_millis(300)).await;
        assert!(
            session_of(&d, "Build the logout page").is_none(),
            "it waits"
        );

        // The session opens a pull request: the boss watches its CI itself.
        acts_on(
            &llm,
            "opened PR #5 on acme/web",
            &[(
                "crew_watch",
                json!({"repo": "acme/web", "number": 5, "session_id": login.id.to_string(),
                    "plan_id": plan.summary.plan.id.to_string(),
                    "note": "When green, move the login ticket to done and close its session."}),
            )],
        )
        .await;
        // Its checks go green: the ticket moves to done and the session closes.
        acts_on(
            &llm,
            "its checks are done: success",
            &[
                (
                    "crew_move_ticket",
                    json!({"ticket": login_key, "state": "done", "reason": "PR 5 is green."}),
                ),
                (
                    "crew_close_session",
                    json!({"session_id": login.id.to_string(), "reason": "Its work is done."}),
                ),
            ],
        )
        .await;
        let url = hook_url(&base, login.id).await;
        turn_ends(
            &url,
            "I opened PR #5 on acme/web for the login page; CI is running.",
        )
        .await;

        let watch = eventually("the boss's watch on its CI", async || {
            d.store()
                .watches_open("desk", PERSON)
                .await
                .ok()
                .and_then(|w| w.into_iter().next())
        })
        .await;
        assert_eq!(watch.number, Some(5));
        assert_eq!(watch.session, Some(login.id));

        // The CI wait resolves on its own: no one told the boss.
        eventually("the login ticket done", async || {
            let p = d.plan(plan.summary.plan.id).await.ok()?;
            p.tickets
                .iter()
                .any(|t| {
                    t.ticket.key.as_deref() == Some(login_key.as_str())
                        && t.ticket.ticket.state == TicketState::Done
                })
                .then_some(())
        })
        .await;
        eventually("the finished login session closed", async || {
            d.view(login.id)
                .map_or(true, |v| {
                    v.state == workforce_desk::model::SessionState::Closed
                })
                .then_some(())
        })
        .await;
        assert!(
            d.store()
                .watches_open("desk", PERSON)
                .await
                .unwrap()
                .is_empty(),
            "the watch ended"
        );

        // What it needed is done: the logout ticket starts by itself.
        let logout = eventually("the logout ticket's session", async || {
            session_of(&d, "Build the logout page")
        })
        .await;
        acts_on(
            &llm,
            "logout page is built and merged",
            &[
                (
                    "crew_move_ticket",
                    json!({"ticket": logout_key, "state": "done", "reason": "PR 6 merged."}),
                ),
                (
                    "crew_close_session",
                    json!({"session_id": logout.id.to_string(), "reason": "Its work is done."}),
                ),
            ],
        )
        .await;
        let url = hook_url(&base, logout.id).await;
        turn_ends(&url, "The logout page is built and merged as PR #6.").await;
        eventually("the plan done", async || {
            let p = d.plan(plan.summary.plan.id).await.ok()?;
            (p.summary.plan.state == PlanState::Done
                || p.summary.progress.done == p.summary.progress.total)
                .then_some(())
        })
        .await;
        eventually("the finished logout session closed", async || {
            d.view(logout.id)
                .map_or(true, |v| {
                    v.state == workforce_desk::model::SessionState::Closed
                })
                .then_some(())
        })
        .await;

        // One running conversation, apart from the DM: every notice is in it,
        // nothing of it posted to the person, and nothing asked of them.
        let conversation = overseer
            .sessions
            .find(&check_in_channel("desk"), PERSON)
            .unwrap()
            .expect("its oversight conversation");
        let all: String = conversation
            .messages
            .iter()
            .map(|m| m.content.as_str())
            .collect::<Vec<_>>()
            .join("\n");
        for said in [
            "opened PR #5 on acme/web",
            "its checks are done: success",
            "logout page is built and merged",
        ] {
            assert!(all.contains(said), "{said:?} in its conversation");
        }
        assert!(
            all.contains(OWN_WORDS),
            "its own words are in its conversation"
        );
        assert_eq!(
            super::decides_at_once::said(&overseer, &[OWN_WORDS]),
            0,
            "and never posted to the person's DM"
        );
        assert_eq!(
            super::decides_at_once::said(&overseer, &[NOTICES]),
            0,
            "nor its notices"
        );
        // This hub's own boss at Act fully runs on notices (who else never
        // does is `orchestrator`'s unit test).
        assert!(crew.orchestrates("desk"));
        let asked: Vec<_> = d
            .store()
            .items_not_closed(Some("desk"))
            .await
            .unwrap()
            .into_iter()
            .filter(|i| {
                matches!(
                    i.kind,
                    workforce_desk::open_item::OpenKind::Ask
                        | workforce_desk::open_item::OpenKind::Confirm
                        | workforce_desk::open_item::OpenKind::Todo
                )
            })
            .collect();
        assert!(asked.is_empty(), "nothing asked of the person: {asked:?}");
        assert!(d.plan_needs().is_empty(), "no plan waits on the person");
        // Its second and third turns resumed the same conversation: what
        // came before is in what they were given.
        let turns = notices_turns(&llm).await;
        let last = turns
            .iter()
            .rev()
            .find(|t| t.contains("logout page is built and merged"))
            .unwrap();
        assert!(
            last.contains("opened PR #5 on acme/web"),
            "the conversation carried on"
        );
        drop(crew);
    })
    .await;
}

/// A notice that comes while the boss's turn runs waits for it and goes in
/// the next, never dropped; and every turn has the person's own DM words.
#[tokio::test(flavor = "multi_thread")]
async fn notices_wait_their_turn_and_the_persons_dm_words_come_with_them() {
    with_test_pool(workforce_desk::store::build_pool, |pool| async move {
        let dir = tempfile::tempdir().unwrap();
        let base = std::fs::canonicalize(dir.path()).unwrap();
        let gh = github(0).await;
        let (overseer, crew, _d, llm) = hub_on(&base, pool, &gh).await;
        // The person answered in the DM (after the boss's own first words
        // there, its ask for their directives): the next turn has it.
        eventually("the boss's first words in its DM", async || {
            (super::decides_at_once::said(&overseer, &["Before I run on my own"]) > 0).then_some(())
        })
        .await;
        let mut dm = overseer
            .sessions
            .get_or_create(DESK_CHANNEL, PERSON, "desk")
            .unwrap();
        dm.add_user_message("Use the blue button, and merge when green.", None);
        overseer.sessions.save(&dm).unwrap();
        overseer
            .sessions
            .append_to_archive(&dm.id, dm.messages.last().unwrap())
            .unwrap();

        super::one_picture::slow_once(
            &llm,
            "FIRST-NOTICE",
            Duration::from_millis(1200),
            super::one_picture::Answer::Tool("crew_watches", json!({})),
        )
        .await;
        crew.notify("desk", PERSON, Notice::new("FIRST-NOTICE"));
        eventually("the first turn", async || {
            notices_turns(&llm)
                .await
                .iter()
                .any(|t| t.contains("FIRST-NOTICE"))
                .then_some(())
        })
        .await;
        crew.notify("desk", PERSON, Notice::new("SECOND-NOTICE"));
        crew.notify("desk", PERSON, Notice::new("THIRD-NOTICE"));
        let next = eventually("the next turn, with both", async || {
            notices_turns(&llm).await.into_iter().find(|t| {
                t.contains("SECOND-NOTICE")
                    && !t
                        .rsplit(NOTICES)
                        .next()
                        .unwrap_or("")
                        .contains("FIRST-NOTICE")
            })
        })
        .await;
        assert!(next.contains("THIRD-NOTICE"), "folded into one turn");
        let first = notices_turns(&llm)
            .await
            .into_iter()
            .find(|t| t.contains("FIRST-NOTICE"))
            .unwrap();
        assert!(
            first.contains("Use the blue button, and merge when green."),
            "the person's DM words come with it"
        );
        // Never told twice, nor out of order: no turn has the first with the
        // ones that came while it ran.
        assert!(!notices_turns(&llm).await.iter().any(|t| {
            let given = t.rsplit(NOTICES).next().unwrap_or("");
            given.contains("FIRST-NOTICE") && given.contains("SECOND-NOTICE")
        }));
    })
    .await;
}
