//! The plan watchdog (`plan_watch`), on the shapes the vellev plan stood
//! still in on 19 Nov:
//!
//! - VEL-6: its PR merged, its session done, its ticket still in Doing:
//!   the boss is told once, and once it moves the ticket to Done the close
//!   sweep closes and removes the session (the five sessions left `done`
//!   and open that afternoon).
//! - VEL-8: the same in Review, waiting on a nightly: told once, the
//!   re-check the boss's promise.
//! - VEL-5: waiting on CI: told when it lands, once a head and outcome.
//! - VEL-1: in Doing, its session gone: back to To do and started again,
//!   told to check what is already done first.
//! - A session the person stopped is theirs to resume: asked once; one the
//!   person runs by hand is left alone; a stall is said once; and the
//!   board says "in Doing, no session", never that it is being done.
//! - The overnight stall of 21 Nov: a ticket whose session waits on a
//!   devbox build slot is stuck however busy it looks, said to the person
//!   once and to the plan's turn once.

use super::act_fully::repo;
use super::plans::{hub_on, hub_with, started, ACTS};
use super::stalls::{at_work, building_cli, builds_stop, looked_until_woken, woken_by, Says};
use super::*;
use workforce_desk::account::{Account, Done, Ref, RefKind};
use workforce_desk::board::TicketState;
use workforce_desk::crew::Party;
use workforce_desk::model::{SessionState, SessionView};
use workforce_desk::open_item::OpenKind;
use workforce_desk::plan::{Draft, DraftTicket};
use workforce_desk::{AssignTo, Move, NewPlan};

use super::super::github_seen::Pull;

/// Act, the grace a tenth of a second and a stall half a second.
fn watching() -> String {
    format!("{ACTS}[oversight.work]\norphan_grace_minutes = 1\nstall_minutes = 5\n")
}

const REPO: &str = "acme/vellev";

/// A running plan of `n` tickets in `folder`, approved, its sessions
/// started by the runner.
async fn running(d: &Desk, folder: &Path, n: usize) -> (uuid::Uuid, Vec<SessionView>) {
    let p = d
        .propose_plan(
            NewPlan {
                draft: Draft {
                    goal: "Ship vellev M8".into(),
                    tickets: (1..=n)
                        .map(|i| DraftTicket {
                            title: format!("Milestone {i}"),
                            folder: folder.to_string_lossy().into_owned(),
                            ..Default::default()
                        })
                        .collect(),
                },
                board: None,
                parallel: None,
                engine: Engine::Claude,
                group: None,
                budget: None,
            },
            &Party::manager("desk", "Desk"),
            &By::manager("desk", "Desk", "Sam asked for M8."),
        )
        .await
        .unwrap();
    let id = p.summary.plan.id;
    d.approve_plan(id, 1, &By::you()).await.unwrap();
    // Each ticket given its session, and those sessions: a start retried
    // under load may have made a spare one, which no ticket has.
    let sessions = eventually("its tickets given their sessions", async || {
        let held: Vec<uuid::Uuid> = d
            .plan(id)
            .await
            .ok()?
            .tickets
            .iter()
            .filter_map(|t| t.ticket.ticket.session())
            .collect();
        let s: Vec<SessionView> = started(d)
            .into_iter()
            .filter(|v| held.contains(&v.id))
            .collect();
        (s.len() == n).then_some(s)
    })
    .await;
    (id, sessions)
}

/// Ends `s`'s turn with `said`, as its engine would.
async fn turn_ends(d: &Desk, base: &Path, s: &SessionView, said: &str) -> SessionView {
    let url = hook_url(base, s.id).await;
    hook(
        &url,
        json!({"hook_event_name": "SessionStart", "session_id": format!("cc-{}", s.id)}),
    )
    .await;
    hook(
        &url,
        json!({"hook_event_name": "Stop", "last_assistant_message": said}),
    )
    .await;
    eventually("its turn to end", async || {
        d.view(s.id).ok().filter(|v| v.state == SessionState::Done)
    })
    .await
}

/// `s`'s account names pull request `n`.
async fn names_pr(d: &Desk, s: uuid::Uuid, n: u64) {
    let mut a = Account::new(s, "", chrono::Utc::now());
    a.done.push(Done {
        text: format!("Opened #{n}"),
        refs: vec![Ref {
            kind: RefKind::Pr,
            id: format!("#{n}"),
            url: None,
        }],
        at: chrono::Utc::now(),
        restated: false,
    });
    d.store().session_account_put(&a).await.unwrap();
}

/// The ticket of plan `id` keyed `key`.
async fn ticket(d: &Desk, id: uuid::Uuid, key: &str) -> workforce_desk::PlanTicketView {
    d.plan(id)
        .await
        .unwrap()
        .tickets
        .into_iter()
        .find(|t| t.ticket.key.as_deref() == Some(key))
        .unwrap()
}

/// How many of the boss's turns were asked with every one of `words`.
async fn asked_with(llm: &wiremock::MockServer, words: &[&str]) -> usize {
    llm.received_requests()
        .await
        .unwrap_or_default()
        .iter()
        .filter(|r| {
            let body = String::from_utf8_lossy(&r.body);
            words.iter().all(|w| body.contains(w))
        })
        .count()
}

fn boss_by() -> By {
    By::manager("desk", "Desk", "Its work is done.")
}

fn to(state: TicketState) -> Move {
    Move {
        state,
        before: None,
        after: None,
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn a_merged_pr_with_its_ticket_still_in_doing_tells_the_boss_once_and_done_closes_its_session(
) {
    with_test_pool(workforce_desk::store::build_pool, |pool| async move {
        let dir = tempfile::tempdir().unwrap();
        let base = std::fs::canonicalize(dir.path()).unwrap();
        let folder = base.join("vellev");
        repo(&folder, "git@github.com:acme/vellev.git");
        let (_overseer, state, d, llm) = hub_with(&base, pool, &watching()).await;
        let crew = CrewHub::start(state, d.clone(), FAST, CancellationToken::new());
        *crew.close_settle.lock() = chrono::Duration::zero();
        let (id, sessions) = running(&d, &folder, 1).await;
        let s = &sessions[0];
        // VEL-6 at 14:47: merged, its last words a wait nothing ends.
        crew.pull_seen(
            REPO,
            175,
            Pull {
                merged: true,
                merged_at: Some("2026-11-19T14:45:32Z".into()),
                head: Some("9bfed64c258dbf4275e156041c9d35378b5a5c57".into()),
                ..Default::default()
            },
        );
        names_pr(&d, s.id, 175).await;
        turn_ends(
            &d,
            &base,
            s,
            "Background waits are on both runs, and I'll report when they finish.",
        )
        .await;
        let told = ["DES-1: #175 merged", "still in Doing", "crew.move_ticket"];
        eventually("the boss told", async || {
            (asked_with(&llm, &told).await >= 1).then_some(())
        })
        .await;
        tokio::time::sleep(Duration::from_millis(800)).await;
        assert_eq!(asked_with(&llm, &told).await, 1, "once");
        assert_eq!(
            d.view(s.id).unwrap().state,
            SessionState::Done,
            "its ticket open, it isn't closed"
        );

        // The boss moves it to Done: the sweep closes and removes it.
        let t = ticket(&d, id, "DES-1").await;
        d.move_ticket(t.ticket.ticket.id, to(TicketState::Done), &boss_by())
            .await
            .unwrap();
        eventually("it closed and removed", async || {
            d.store().get(s.id).await.unwrap().is_none().then_some(())
        })
        .await;
        assert!(d.view(s.id).is_err());
    })
    .await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_ticket_in_review_waiting_on_a_nightly_tells_the_boss_once_to_keep_the_check_itself() {
    with_test_pool(workforce_desk::store::build_pool, |pool| async move {
        let dir = tempfile::tempdir().unwrap();
        let base = std::fs::canonicalize(dir.path()).unwrap();
        let folder = base.join("vellev");
        repo(&folder, "git@github.com:acme/vellev.git");
        let (_overseer, state, d, llm) = hub_with(&base, pool, &watching()).await;
        let crew = CrewHub::start(state, d.clone(), FAST, CancellationToken::new());
        *crew.close_settle.lock() = chrono::Duration::zero();
        let (id, sessions) = running(&d, &folder, 1).await;
        let s = &sessions[0];
        // VEL-8: both merged, the ticket in Review, the nightly tomorrow.
        for n in [184, 189] {
            crew.pull_seen(
                REPO,
                n,
                Pull {
                    merged: true,
                    ..Default::default()
                },
            );
            names_pr(&d, s.id, n).await;
        }
        let t = ticket(&d, id, "DES-1").await;
        d.move_ticket(t.ticket.ticket.id, to(TicketState::Review), &boss_by())
            .await
            .unwrap();
        turn_ends(
            &d,
            &base,
            s,
            "Both PRs are merged; tomorrow's nightly security audit will show it green.",
        )
        .await;
        let told = ["still in Review", "crew.promise"];
        eventually("the boss told", async || {
            (asked_with(&llm, &told).await >= 1).then_some(())
        })
        .await;
        tokio::time::sleep(Duration::from_millis(800)).await;
        assert_eq!(asked_with(&llm, &told).await, 1, "once");
    })
    .await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_session_waiting_on_ci_is_told_when_it_lands_once_a_head_and_outcome() {
    with_test_pool(workforce_desk::store::build_pool, |pool| async move {
        let dir = tempfile::tempdir().unwrap();
        let base = std::fs::canonicalize(dir.path()).unwrap();
        let folder = base.join("vellev");
        repo(&folder, "git@github.com:acme/vellev.git");
        let (_overseer, state, d, _llm) = hub_with(&base, pool, &watching()).await;
        let crew = CrewHub::start(state, d.clone(), FAST, CancellationToken::new());
        *crew.close_settle.lock() = chrono::Duration::zero();
        let (_id, sessions) = running(&d, &folder, 1).await;
        let s = &sessions[0];
        let open = |checks: &str, failed: u64| Pull {
            open: true,
            head: Some("d05e8a4c0ffee".into()),
            checks: Some(checks.into()),
            failed,
            ..Default::default()
        };
        // VEL-5: its CI still running, nothing is said.
        crew.pull_seen(REPO, 191, open("pending", 0));
        names_pr(&d, s.id, 191).await;
        turn_ends(
            &d,
            &base,
            s,
            "I'll send Desk #191 and d05e8a4 once CI is green on that head.",
        )
        .await;
        let told = |words: &'static str| {
            let d = d.clone();
            let id = s.id;
            async move {
                d.line_thread(id, None, Some(30))
                    .await
                    .unwrap()
                    .iter()
                    .filter(|m| m.text.contains(words))
                    .count()
            }
        };
        tokio::time::sleep(Duration::from_millis(600)).await;
        assert_eq!(told("Desk checked GitHub").await, 0, "pending says nothing");

        crew.pull_seen(REPO, 191, open("success", 0));
        eventually("it told green", async || {
            (told("CI on #191's head d05e8a4 is green").await == 1).then_some(())
        })
        .await;
        tokio::time::sleep(Duration::from_millis(600)).await;
        assert_eq!(told("is green").await, 1, "once");

        // A new head goes red: said too, with how many failed.
        crew.pull_seen(
            REPO,
            191,
            Pull {
                head: Some("e1f2a3b4c5d6".into()),
                ..open("failure", 2)
            },
        );
        eventually("it told red", async || {
            (told("CI on #191's head e1f2a3b is red (2 failed checks)").await == 1).then_some(())
        })
        .await;
    })
    .await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_ticket_in_doing_whose_session_is_gone_starts_again_told_to_check_what_is_done() {
    with_test_pool(workforce_desk::store::build_pool, |pool| async move {
        let dir = tempfile::tempdir().unwrap();
        let base = std::fs::canonicalize(dir.path()).unwrap();
        let (_overseer, state, d, _llm) = hub_with(&base, pool, &watching()).await;
        let crew = CrewHub::start(state, d.clone(), FAST, CancellationToken::new());
        let (id, _) = running(&d, &base, 1).await;
        // The session its ticket has (a start retried under load may have
        // made another first).
        let first = eventually("its ticket given its session", async || {
            ticket(&d, id, "DES-1").await.ticket.ticket.session()
        })
        .await;
        let before: Vec<uuid::Uuid> = started(&d).iter().map(|v| v.id).collect();
        // VEL-1: its session went away with the ticket still in Doing.
        d.end(first, Some(boss_by())).await.unwrap();
        d.remove_closed(first).await.unwrap();
        // Ending it let the ticket go: in Doing, no one on it. Putting a
        // ticket that ran back in the queue is the person's call at Act:
        // suggested to them, once.
        let open = || async {
            d.store()
                .suggestions(50)
                .await
                .unwrap()
                .into_iter()
                .filter(|s| s.state == workforce_desk::crew::SuggestionState::Open)
                .filter(|s| s.text.contains("DES-1"))
                .collect::<Vec<_>>()
        };
        tokio::time::sleep(Duration::from_millis(300)).await;
        eventually("putting it back suggested", async || {
            // Looked at now, not when the items' minute comes round.
            crew.look_at_items().await;
            (!open().await.is_empty()).then_some(())
        })
        .await;
        tokio::time::sleep(Duration::from_millis(600)).await;
        assert_eq!(open().await.len(), 1, "once");
        let t = ticket(&d, id, "DES-1").await;
        assert_eq!(
            t.ticket.ticket.state,
            TicketState::Doing,
            "not moved under them"
        );
        assert!(
            started(&d).iter().all(|v| before.contains(&v.id)),
            "nothing started by itself"
        );

        // The person puts it back: the plan starts it again.
        d.move_ticket(t.ticket.ticket.id, to(TicketState::Todo), &By::you())
            .await
            .unwrap();
        let again = eventually("it started again", async || {
            started(&d).into_iter().find(|v| !before.contains(&v.id))
        })
        .await;
        assert!(
            again.last_prompt.as_deref().is_some_and(
                |p| p.contains("started before") && p.contains("Check what is already done")
            ),
            "{:?}",
            again.last_prompt
        );
        eventually("it given its new session", async || {
            let t = ticket(&d, id, "DES-1").await;
            (t.ticket.ticket.state == TicketState::Doing
                && t.ticket.ticket.session() == Some(again.id))
            .then_some(())
        })
        .await;
    })
    .await;
}

/// A session removed is gone from Desk before its ticket's let go of it is
/// written: one look sees the ticket in Doing still on a session that is
/// gone, the next sees it with no one on it. Putting it back is suggested
/// once across both.
#[tokio::test(flavor = "multi_thread")]
async fn a_ticket_whose_session_is_gone_is_put_back_once_however_each_look_sees_it() {
    with_test_pool(workforce_desk::store::build_pool, |pool| async move {
        let dir = tempfile::tempdir().unwrap();
        let base = std::fs::canonicalize(dir.path()).unwrap();
        let (_overseer, state, d, _llm) = hub_with(&base, pool, &watching()).await;
        let crew = CrewHub::start(state, d.clone(), FAST, CancellationToken::new());
        let (id, _) = running(&d, &base, 1).await;
        let first = eventually("its ticket given its session", async || {
            ticket(&d, id, "DES-1").await.ticket.ticket.session()
        })
        .await;
        let held = ticket(&d, id, "DES-1").await.ticket.ticket.assignee;
        d.end(first, Some(boss_by())).await.unwrap();
        d.remove_closed(first).await.unwrap();
        let open = || async {
            d.store()
                .suggestions(50)
                .await
                .unwrap()
                .into_iter()
                .filter(|s| s.state == workforce_desk::crew::SuggestionState::Open)
                .filter(|s| s.text.contains("DES-1"))
                .collect::<Vec<_>>()
        };
        // The moment before the let go is written: the session gone, the
        // ticket still on it.
        let mut t = ticket(&d, id, "DES-1").await.ticket.ticket;
        let doing_since = t.state_at;
        t.assignee = held;
        d.store().put_ticket(&mut t).await.unwrap();
        tokio::time::sleep(Duration::from_millis(300)).await;
        eventually("putting it back suggested", async || {
            crew.look_at_plans().await;
            (!open().await.is_empty()).then_some(())
        })
        .await;
        // Then the let go written: the ticket in Doing with no one on it,
        // since the same moment.
        let mut t = ticket(&d, id, "DES-1").await.ticket.ticket;
        t.assignee = None;
        d.store().put_ticket(&mut t).await.unwrap();
        assert_eq!(t.state, TicketState::Doing);
        assert_eq!(t.state_at, doing_since);
        for _ in 0..3 {
            crew.look_at_plans().await;
        }
        tokio::time::sleep(Duration::from_millis(600)).await;
        assert_eq!(open().await.len(), 1, "once");
    })
    .await;
}

/// A ticket put back, then given another session while still in Doing (its
/// time in Doing the same): when that session goes too, it is put back
/// again.
#[tokio::test(flavor = "multi_thread")]
async fn a_ticket_given_another_session_in_doing_is_put_back_again_when_that_one_goes() {
    with_test_pool(workforce_desk::store::build_pool, |pool| async move {
        let dir = tempfile::tempdir().unwrap();
        let base = std::fs::canonicalize(dir.path()).unwrap();
        let (_overseer, state, d, _llm) = hub_with(&base, pool, &watching()).await;
        let crew = CrewHub::start(state, d.clone(), FAST, CancellationToken::new());
        let (id, _) = running(&d, &base, 1).await;
        let first = eventually("its ticket given its session", async || {
            ticket(&d, id, "DES-1").await.ticket.ticket.session()
        })
        .await;
        let held = ticket(&d, id, "DES-1")
            .await
            .ticket
            .ticket
            .assignee
            .unwrap();
        let doing_since = ticket(&d, id, "DES-1").await.ticket.ticket.state_at;
        let open = || async {
            d.store()
                .suggestions(50)
                .await
                .unwrap()
                .into_iter()
                .filter(|s| s.state == workforce_desk::crew::SuggestionState::Open)
                .filter(|s| s.action["tool"] == "crew.move_ticket" && s.text.contains("DES-1"))
                .collect::<Vec<_>>()
        };
        let gone_and_put_back = async |s: uuid::Uuid, by: Option<By>| {
            d.end(s, by).await.unwrap();
            d.remove_closed(s).await.unwrap();
            eventually("putting it back suggested", async || {
                crew.look_at_plans().await;
                open().await.into_iter().next()
            })
            .await
        };
        // Its session goes: putting it back is suggested, and the person
        // says no.
        let once = gone_and_put_back(first, Some(boss_by())).await;
        assert!(d.store().suggestion_dismiss_open(&once.id).await.unwrap());
        // Given another session within Doing, seen by a look.
        let again = new_session(&d, &base, "again").await;
        let mut t = ticket(&d, id, "DES-1").await.ticket.ticket;
        t.assignee = Some(workforce_desk::crew::Party {
            id: again.id.to_string(),
            ..held
        });
        d.store().put_ticket(&mut t).await.unwrap();
        assert_eq!(t.state, TicketState::Doing);
        assert_eq!(t.state_at, doing_since);
        crew.look_at_plans().await;
        // That one (the person's own) goes too: it is put back again.
        let twice = gone_and_put_back(again.id, Some(By::you())).await;
        assert_ne!(twice.id, once.id);
    })
    .await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_stopped_session_is_the_persons_a_hand_run_ticket_is_left_alone_and_a_stall_is_said_once()
{
    with_test_pool(workforce_desk::store::build_pool, |pool| async move {
        let dir = tempfile::tempdir().unwrap();
        let base = std::fs::canonicalize(dir.path()).unwrap();
        let (overseer, state, d, _llm) = hub_with(&base, pool, &watching()).await;
        let _crew = CrewHub::start(state, d.clone(), FAST, CancellationToken::new());
        let (id, sessions) = running(&d, &base, 2).await;
        let one = sessions.iter().find(|v| v.name == "DES-1").unwrap().id;
        let two = sessions.iter().find(|v| v.name == "DES-2").unwrap().id;
        // DES-2 the person runs in a session of their own.
        let mine = new_session(&d, &base, "mine").await;
        let t2 = ticket(&d, id, "DES-2").await;
        d.assign_ticket(
            t2.ticket.ticket.id,
            Some(AssignTo::Session(mine.id)),
            &By::you(),
        )
        .await
        .unwrap();
        d.stop(two, Some(By::you())).ok();
        d.stop(mine.id, Some(By::you())).ok();
        d.stop(one, Some(By::you())).unwrap();

        let stopped = || async {
            d.store()
                .items_not_closed(Some("desk"))
                .await
                .unwrap()
                .into_iter()
                .filter(|i| i.dedupe_key.starts_with("plan-stopped:"))
                .collect::<Vec<_>>()
        };
        let asked = eventually("the person asked", async || {
            let s = stopped().await;
            (!s.is_empty()).then_some(s)
        })
        .await;
        assert_eq!(asked.len(), 1, "{asked:?}");
        assert_eq!(asked[0].kind, OpenKind::Todo);
        assert_eq!(asked[0].session, Some(one), "never the person's own");
        assert!(
            asked[0].text.contains("DES-1 is in Doing"),
            "{}",
            asked[0].text
        );
        assert_eq!(
            d.view(one).unwrap().state,
            SessionState::Stopped,
            "never resumed under them"
        );

        // The board says it plainly.
        let board = overseer
            .tools
            .get("crew.board")
            .unwrap()
            .execute(json!({}))
            .await
            .unwrap();
        let table = board.output["table"]
            .as_str()
            .unwrap_or_default()
            .to_string();
        assert!(table.contains("in Doing, no session"), "{table}");

        // Nothing moves: the person is told once what is stuck.
        let stalls = || async {
            overseer
                .sessions
                .find(DESK_CHANNEL, PERSON)
                .unwrap()
                .map(|s| s.messages)
                .unwrap_or_default()
                .iter()
                .filter(|m| m.content.contains("hasn't moved in"))
                .map(|m| m.content.clone())
                .collect::<Vec<_>>()
        };
        let said = eventually("the stall said", async || {
            let s = stalls().await;
            (!s.is_empty()).then_some(s)
        })
        .await;
        assert!(said[0].contains("DES-1 is in Doing"), "{said:?}");
        tokio::time::sleep(Duration::from_millis(800)).await;
        assert_eq!(stalls().await.len(), 1, "once");
        assert_eq!(stopped().await.len(), 1, "asked once");
    })
    .await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_ticket_whose_session_waits_on_a_build_slot_is_stuck_said_once_and_its_turn_told_once() {
    with_test_pool(workforce_desk::store::build_pool, |pool| async move {
        let dir = tempfile::tempdir().unwrap();
        let base = std::fs::canonicalize(dir.path()).unwrap();
        let d = sighted_desk(pool, &base, building_cli(&base, Says::Queued)).await;
        let (overseer, state, d, llm) = hub_on(&base, &watching(), d).await;
        let _crew = CrewHub::start(state, d.clone(), FAST, CancellationToken::new());
        let (_, sessions) = running(&d, &base, 1).await;
        at_work(&base, sessions[0].id).await;
        let told = "DES-1's session has waited ";
        looked_until_woken(&d, &llm, told).await;
        let stalls = || {
            overseer
                .sessions
                .find(DESK_CHANNEL, PERSON)
                .unwrap()
                .map(|s| s.messages)
                .unwrap_or_default()
                .iter()
                .filter(|m| m.content.contains("hasn't moved in"))
                .map(|m| m.content.clone())
                .collect::<Vec<_>>()
        };
        let said = eventually("the plan's stall said", async || {
            d.sight_pass().await;
            let s = stalls();
            (!s.is_empty()).then_some(s)
        })
        .await;
        assert!(said[0].contains(told), "{said:?}");
        assert!(said[0].contains("for a build slot"), "{said:?}");
        for _ in 0..8 {
            d.sight_pass().await;
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        assert_eq!(stalls().len(), 1, "said once");
        assert_eq!(woken_by(&llm, told).await, 1, "its turn told once");
        builds_stop(&base);
    })
    .await;
}
