//! Goals the boss keeps itself (DW-728): the person's agreement, checked in
//! their own DM, approves a plan with a result and is said back once with
//! its first reading; a part that names what GitHub hasn't got refuses it;
//! the session already on the work is taken on and starts nothing; the
//! goal's ticket is held until it is met; facts reach every turn; the goal
//! and its result change only on the person's answer to the very proposal
//! asked; settled tickets don't finish an unmet goal; and work sent with no
//! goal saved is said at once.

use super::*;
use workforce_desk::board::TicketState;
use workforce_desk::plan::PlanState;

use super::plans::{judged, person_says};

pub(super) const ACTS: &str = "[oversight]\nenabled = true\nlevel = \"act\"\n[oversight.act]\nhand_out_work = true\nnudge = true\n";

const SUGGESTS: &str = "[oversight]\nenabled = true\nlevel = \"suggest\"\n";

/// GitHub as the person's key reads it: acme/web's main has one finished
/// red run of CI an hour ago; acme/nope isn't there.
async fn github() -> wiremock::MockServer {
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};
    let gh = MockServer::start().await;
    let hour_ago = (chrono::Utc::now() - chrono::Duration::hours(1)).to_rfc3339();
    Mock::given(method("GET"))
        .and(path("/repos/acme/web/actions/runs"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "total_count": 1,
            "workflow_runs": [{ "id": 7, "name": "CI", "head_branch": "main", "status": "completed",
                "conclusion": "failure", "run_number": 7, "html_url": "https://github.com/acme/web/actions/runs/7",
                "created_at": hour_ago, "updated_at": hour_ago }]
        })))
        .mount(&gh)
        .await;
    Mock::given(method("GET"))
        .and(path("/repos/acme/flaky/actions/runs"))
        .respond_with(
            ResponseTemplate::new(500).set_body_json(json!({ "message": "Server Error" })),
        )
        .mount(&gh)
        .await;
    Mock::given(method("GET"))
        .and(path("/repos/acme/nope/actions/runs"))
        .respond_with(ResponseTemplate::new(404).set_body_json(json!({ "message": "Not Found" })))
        .mount(&gh)
        .await;
    gh
}

/// The boss at `oversight`, its GitHub `gh` with the person's own key; its
/// DM open, its crew started.
pub(super) async fn boss_on(
    dir: &Path,
    pool: workforce_postgres::PostgresPool,
    oversight: &str,
    gh: &wiremock::MockServer,
) -> (Arc<AgentService>, Arc<CrewHub>, Desk, wiremock::MockServer) {
    boss_until(dir, pool, oversight, gh, CancellationToken::new()).await
}

/// [`boss_on`], its crew running until `stop`.
pub(super) async fn boss_until(
    dir: &Path,
    pool: workforce_postgres::PostgresPool,
    oversight: &str,
    gh: &wiremock::MockServer,
    stop: CancellationToken,
) -> (Arc<AgentService>, Arc<CrewHub>, Desk, wiremock::MockServer) {
    let d = desk_running(
        pool,
        dir,
        Default::default(),
        installed_cli(dir),
        not_installed(dir),
    )
    .await;
    boss_over(dir, d, oversight, gh, stop).await
}

/// [`boss_until`] over the desk `d`.
pub(super) async fn boss_over(
    dir: &Path,
    d: Desk,
    oversight: &str,
    gh: &wiremock::MockServer,
    stop: CancellationToken,
) -> (Arc<AgentService>, Arc<CrewHub>, Desk, wiremock::MockServer) {
    let uri = gh.uri();
    let (overseer, llm) = agent_configured(dir, "desk", "ok", &[], |c| {
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
    write_config(&overseer, oversight);
    let state = hub(std::slice::from_ref(&overseer));
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
    let crew = CrewHub::start(state, d.clone(), FAST, stop);
    eventually("the plan tools", async || {
        overseer.tools.has("crew.plan").then_some(())
    })
    .await;
    (overseer, crew, d, llm)
}

pub(super) async fn run(
    boss: &AgentService,
    tool: &str,
    input: Value,
) -> workforce_agent::tools::ToolResult {
    boss.tools.get(tool).unwrap().execute(input).await.unwrap()
}

/// CI on acme/web's `repo` main green for `runs` runs in a row.
fn green(repo: &str, runs: u8) -> Value {
    json!({ "words": "Done when CI on main is green", "parts": [{
        "kind": "ci", "repo": repo, "branch": "main",
        "want": { "kind": "green", "runs": runs }
    }] })
}

/// What the boss posted in its DM that says `what`.
fn posted(boss: &AgentService, what: &str) -> usize {
    super::decides_at_once::said(boss, &[what])
}

#[tokio::test(flavor = "multi_thread")]
async fn a_goal_the_person_agreed_is_kept_by_desk_until_it_is_met() {
    with_test_pool(workforce_desk::store::build_pool, |pool| async move {
        let dir = tempfile::tempdir().unwrap();
        let base = std::fs::canonicalize(dir.path()).unwrap();
        let gh = github().await;
        let (overseer, crew, d, llm) = boss_on(&base, pool, ACTS, &gh).await;
        let folder = base.to_string_lossy().to_string();

        // The session already on the work, the boss's own.
        let r = run(
            &overseer,
            "crew.start_session",
            json!({ "folder": folder, "goal": "Make CI green", "reason": "Sam asked" }),
        )
        .await;
        assert!(r.success, "{r:?}");
        let s = eventually("its session", async || {
            d.list().into_iter().find(|v| v.goal == "Make CI green")
        })
        .await;

        // "20:14": work sent before the person said anything isn't theirs.
        let r = run(
            &overseer,
            "crew.nudge",
            json!({ "session_id": s.id.to_string(), "message": "Look at the CI logs.", "reason": "Checking" }),
        )
        .await;
        assert!(r.success, "{r:?}");
        assert!(
            !r.output["note"].as_str().unwrap_or_default().contains("no agreed goal saved"),
            "{r:?}"
        );
        // Work sent after the person spoke, with no goal saved: said at once.
        person_says(&overseer, "Yes please, get CI on main green today");
        let r = run(
            &overseer,
            "crew.nudge",
            json!({ "session_id": s.id.to_string(), "message": "Also fix the lint step.", "reason": "Sam asked" }),
        )
        .await;
        assert!(r.success, "{r:?}");
        assert!(
            r.output["note"]
                .as_str()
                .is_some_and(|n| n.contains("has work from you and no agreed goal saved: if the person agreed a goal for it")),
            "{r:?}"
        );

        let plan = |agreed: &str, result: Value| {
            json!({
                "goal": "CI on main is green",
                "agreed": { "person_said": agreed },
                "result": result,
                "tickets": [{ "session": s.id.to_string() }],
            })
        };
        // Words the person never wrote save nothing.
        let r = run(&overseer, "crew.plan", plan("ship it all tonight", green("acme/web", 1))).await;
        assert!(!r.success);
        assert!(r.error.unwrap().contains("didn't find it word for word"));
        // A repository GitHub hasn't got refuses it as it is saved.
        let r = run(&overseer, "crew.plan", plan("get CI on main green", green("acme/nope", 1))).await;
        assert!(!r.success, "{r:?}");
        assert!(d.plans(None, None).await.unwrap().is_empty(), "nothing saved");

        let sessions = d.list().len();
        let r = run(&overseer, "crew.plan", plan("get CI on main green", green("acme/web", 1))).await;
        assert!(r.success, "{r:?}");
        let id: uuid::Uuid = r.output["proposed"]["plan_id"].as_str().unwrap().parse().unwrap();
        let p = d.plan(id).await.unwrap();
        assert_eq!(p.summary.plan.state, PlanState::Running, "agreed, so approved");
        assert_eq!(p.summary.plan.agreed.as_deref(), Some("get CI on main green"));
        assert!(p.summary.plan.read_back_at.is_some());
        let ticket = p.summary.plan.goal_ticket.expect("the session's ticket stands for it");
        let t = d.ticket(ticket).await.unwrap();
        assert_eq!(t.ticket.state, TicketState::Doing);
        assert_eq!(t.ticket.session(), Some(s.id));
        tokio::time::sleep(Duration::from_millis(300)).await;
        assert_eq!(d.list().len(), sessions, "it starts nothing");
        assert_eq!(posted(&overseer, "I've saved this goal: CI on main is green."), 1);
        assert_eq!(
            posted(&overseer, "now 0 of the newest run green in a row"),
            1,
            "with its first reading"
        );

        // Its ticket is Desk's to close, once the goal is met.
        let key = t.key.clone().unwrap();
        let r = run(
            &overseer,
            "crew.move_ticket",
            json!({ "ticket": key, "state": "done", "reason": "The session says it is fixed." }),
        )
        .await;
        assert!(!r.success);
        assert!(
            r.error.unwrap().starts_with(&format!("{key} stands for the goal “CI on main is green”, which isn't met (")),
        );

        // A fact reaches every turn's opening.
        let r = run(
            &overseer,
            "crew.plan_update",
            json!({ "plan_id": id.to_string(), "facts": [{ "said": "The gateway is upgraded." }], "why": "Reported." }),
        )
        .await;
        assert!(r.success, "{r:?}");
        assert_eq!(r.output["facts"][0]["checked"], false);
        let block = crew.goals_block("desk").await.unwrap();
        assert!(block.starts_with(workforce_core::desk::GOALS_MARKER));
        assert!(block.contains("The gateway is upgraded. (unchecked)"), "{block}");

        // How it's measured is the person's: asked first, then changed on
        // their answer to that very proposal.
        let r = run(
            &overseer,
            "crew.plan_update",
            json!({ "plan_id": id.to_string(), "result": green("acme/web", 2), "why": "Steadier." }),
        )
        .await;
        assert!(!r.success);
        assert!(r.error.unwrap().starts_with("The goal and how it's measured are the person's"));
        let r = run(
            &overseer,
            "crew.ask_person",
            json!({ "text": "Shall the goal be two green runs in a row?", "plan_id": id.to_string(),
                    "proposal": { "result": green("acme/web", 2) } }),
        )
        .await;
        assert!(r.success, "{r:?}");
        tokio::time::sleep(Duration::from_millis(20)).await;
        person_says(&overseer, "Yes, make it two green runs in a row");
        let r = run(
            &overseer,
            "crew.plan_update",
            json!({ "plan_id": id.to_string(), "result": green("acme/web", 3),
                    "person_said": "make it two green runs in a row", "why": "Steadier." }),
        )
        .await;
        assert!(!r.success);
        assert!(r.error.unwrap().contains("proposed something else"));
        // Their answer is their whole reply: a part of it is refused, saying so.
        let answer = |said: &str| {
            json!({ "plan_id": id.to_string(), "result": green("acme/web", 2),
                    "person_said": said, "why": "Steadier." })
        };
        let r = run(&overseer, "crew.plan_update", answer("make it two green runs in a row")).await;
        assert!(!r.success);
        assert!(r.error.unwrap().contains("Quote the whole message"));
        let r = run(&overseer, "crew.plan_update", answer("Yes, make it two green runs in a row")).await;
        assert!(r.success, "{r:?}");
        let p = d.plan(id).await.unwrap();
        assert_eq!(p.summary.plan.result, Some(serde_json::from_value(green("acme/web", 2)).unwrap()));
        assert!(!p.summary.plan.awaiting(), "their answer approves it");
        assert_eq!(posted(&overseer, "I've saved this goal: CI on main is green."), 2, "said back again");

        // Only a reading or the person closes it.
        let r = run(
            &overseer,
            "crew.plan_update",
            json!({ "plan_id": id.to_string(), "met": { "person_said": "it's done now" }, "why": "Done." }),
        )
        .await;
        assert!(!r.success);
        assert!(r
            .error
            .unwrap()
            .starts_with("No question of yours proposed that it is met"));

        // Every ticket settled while the goal isn't met: the plan runs on,
        // and its turn is told once.
        d.move_ticket(
            ticket,
            workforce_desk::Move { state: TicketState::Done, before: None, after: None },
            &By::you(),
        )
        .await
        .unwrap();
        let unmet = "Every ticket is settled but the goal isn't met";
        crew.plan_turn_now(id, vec!["MAN-1 is done.".into()]).await;
        crew.plan_turn_now(id, vec!["Still done.".into()]).await;
        assert_eq!(d.plan(id).await.unwrap().summary.plan.state, PlanState::Running);
        let bodies: Vec<String> = llm
            .received_requests()
            .await
            .unwrap()
            .iter()
            .map(|r| String::from_utf8_lossy(&r.body).to_string())
            .collect();
        let turn = |marker: &str| -> Vec<&String> {
            bodies.iter().filter(|b| b.contains(marker)).collect()
        };
        assert!(turn("MAN-1 is done.").iter().any(|b| b.contains(unmet)));
        assert!(!turn("Still done.").is_empty());
        assert!(
            turn("Still done.").iter().all(|b| b.matches(unmet).count() <= 1),
            "told once"
        );
    })
    .await;
}

#[tokio::test(flavor = "multi_thread")]
async fn at_suggest_a_boss_only_proposes_what_the_person_agreed() {
    with_test_pool(workforce_desk::store::build_pool, |pool| async move {
        let dir = tempfile::tempdir().unwrap();
        let base = std::fs::canonicalize(dir.path()).unwrap();
        let gh = github().await;
        let (overseer, _crew, d, _llm) = boss_on(&base, pool, SUGGESTS, &gh).await;
        person_says(&overseer, "Yes, make the suite green");
        let r = run(
            &overseer,
            "crew.plan",
            json!({ "goal": "Make the suite green", "agreed": { "person_said": "make the suite green" },
                    "result": judged(), "tickets": [{ "title": "Fix it", "folder": base.to_string_lossy() }] }),
        )
        .await;
        assert!(!r.success);
        assert!(r.error.unwrap().starts_with("At Suggest you only propose plans"));
        let r = run(
            &overseer,
            "crew.plan",
            json!({ "goal": "Make the suite green",
                    "tickets": [{ "title": "Fix it", "folder": base.to_string_lossy() }] }),
        )
        .await;
        assert!(!r.success, "a result is said");
        assert!(d.plans(None, None).await.unwrap().is_empty());
    })
    .await;
}

/// GitHub where acme/web's CI on main reads red once, as the goal is
/// saved, and green from then on.
async fn github_turning_green() -> wiremock::MockServer {
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};
    let gh = MockServer::start().await;
    let runs = |conclusion: &str| {
        let now = chrono::Utc::now().to_rfc3339();
        json!({ "total_count": 1, "workflow_runs": [{ "id": 8, "name": "CI", "head_branch": "main",
            "status": "completed", "conclusion": conclusion, "run_number": 8,
            "created_at": now, "run_started_at": now, "updated_at": now }] })
    };
    Mock::given(method("GET"))
        .and(path("/repos/acme/web/actions/runs"))
        .respond_with(ResponseTemplate::new(200).set_body_json(runs("failure")))
        .up_to_n_times(1)
        .with_priority(1)
        .mount(&gh)
        .await;
    Mock::given(method("GET"))
        .and(path("/repos/acme/web/actions/runs"))
        .respond_with(ResponseTemplate::new(200).set_body_json(runs("success")))
        .mount(&gh)
        .await;
    gh
}

/// acme/web's CI on main, red in `gh` until now, green from now on.
async fn ci_turns_green(gh: &wiremock::MockServer) {
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, ResponseTemplate};
    let now = chrono::Utc::now().to_rfc3339();
    Mock::given(method("GET"))
        .and(path("/repos/acme/web/actions/runs"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "total_count": 1, "workflow_runs": [{ "id": 9, "name": "CI", "head_branch": "main",
            "status": "completed", "conclusion": "success", "run_number": 9,
            "created_at": now, "run_started_at": now, "updated_at": now }] })))
        .with_priority(1)
        .mount(gh)
        .await;
}

/// A session of the boss's on `folder`, working on `goal`.
async fn session_on(
    boss: &AgentService,
    d: &Desk,
    folder: &str,
    goal: &str,
) -> workforce_desk::model::SessionView {
    let r = run(
        boss,
        "crew.start_session",
        json!({ "folder": folder, "goal": goal, "reason": "Sam asked" }),
    )
    .await;
    assert!(r.success, "{r:?}");
    eventually("its session", async || {
        d.list().into_iter().find(|v| v.goal == goal)
    })
    .await
}

/// The goal plan the person agreed, saved by the boss for session `s`.
async fn agreed_plan(
    boss: &AgentService,
    s: &workforce_desk::model::SessionView,
    result: Value,
) -> uuid::Uuid {
    person_says(boss, "Yes, keep the docs build green");
    let r = run(
        boss,
        "crew.plan",
        json!({ "goal": "Keep the docs build green", "agreed": { "person_said": "keep the docs build green" },
                "result": result, "tickets": [{ "session": s.id.to_string() }] }),
    )
    .await;
    assert!(r.success, "{r:?}");
    r.output["proposed"]["plan_id"]
        .as_str()
        .unwrap()
        .parse()
        .unwrap()
}

/// The newest question of the boss's about plan `id` not yet applied.
async fn asked(d: &Desk, id: uuid::Uuid) -> uuid::Uuid {
    d.store().goal_asks(id).await.unwrap()[0].item
}

/// The person writes `text` in a thread of `boss`'s DM.
fn person_says_in_a_thread(boss: &AgentService, text: &str) {
    let channel = workforce_core::desk::dm_thread_channel(&uuid::Uuid::new_v4().to_string());
    says_in_thread(boss, &channel, text, true);
}

/// The person, else the boss, writes `text` in the DM thread `channel`.
fn says_in_thread(boss: &AgentService, channel: &str, text: &str, person: bool) {
    let mut thread = boss
        .sessions
        .get_or_create(channel, PERSON, "desk")
        .unwrap();
    if person {
        thread.add_user_message(text, None);
    } else {
        thread.add_assistant_message(text, 0);
    }
    boss.sessions.save(&thread).unwrap();
    boss.sessions
        .append_to_archive(&thread.id, thread.messages.last().unwrap())
        .unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn the_persons_short_reply_on_the_buttons_in_the_dm_or_a_thread_applies_what_was_asked_once()
{
    with_test_pool(workforce_desk::store::build_pool, |pool| async move {
        let dir = tempfile::tempdir().unwrap();
        let base = std::fs::canonicalize(dir.path()).unwrap();
        let gh = github().await;
        let (overseer, crew, d, _llm) = boss_on(&base, pool, ACTS, &gh).await;
        let s = session_on(&overseer, &d, &base.to_string_lossy(), "Fix the docs build").await;
        let id = agreed_plan(&overseer, &s, judged()).await;

        // On the item's buttons: "yes" answers what it proposed.
        let to = "Keep the docs build green and fast";
        let r = run(
            &overseer,
            "crew.ask_person",
            json!({ "text": "Shall the goal also be a fast docs build?", "plan_id": id.to_string(),
                    "proposal": { "goal": to } }),
        )
        .await;
        assert!(r.success, "{r:?}");
        let item = asked(&d, id).await;
        tokio::time::sleep(Duration::from_millis(20)).await;
        crew.answer_item(item, "yes").await.unwrap();
        let change = json!({ "plan_id": id.to_string(), "goal": to, "person_said": "yes", "why": "They said yes." });
        let r = run(&overseer, "crew.plan_update", change.clone()).await;
        assert!(r.success, "{r:?}");
        let p = d.plan(id).await.unwrap().summary.plan;
        assert_eq!(p.goal, to);
        assert!(!p.awaiting(), "their answer approves it");
        // Applied once: the same answer again is no answer.
        let again = json!({ "plan_id": id.to_string(), "goal": "Keep the docs build fast", "person_said": "yes", "why": "Again." });
        let r = run(&overseer, "crew.plan_update", again).await;
        assert!(!r.success, "{r:?}");
        assert!(r.error.unwrap().contains("Or ask them"), "the refusal says how");

        // Answered in the DM, the question closed by the boss first.
        let to = "Keep the docs build green, fast and small";
        let r = run(
            &overseer,
            "crew.ask_person",
            json!({ "text": "Shall the goal also be a small docs build?", "plan_id": id.to_string(),
                    "proposal": { "goal": to } }),
        )
        .await;
        assert!(r.success, "{r:?}");
        let item = asked(&d, id).await;
        tokio::time::sleep(Duration::from_millis(20)).await;
        person_says(&overseer, "ok");
        d.store()
            .item_close(item, workforce_desk::open_item::ClosedBy::Boss, "They said ok.")
            .await
            .unwrap();
        let r = run(
            &overseer,
            "crew.plan_update",
            json!({ "plan_id": id.to_string(), "goal": to, "person_said": "ok", "why": "They said ok." }),
        )
        .await;
        assert!(r.success, "{r:?}");
        assert_eq!(d.plan(id).await.unwrap().summary.plan.goal, to);

        // Met, answered in a thread of the DM: Desk closes its ticket, and
        // the closing turn runs after the call, which finishes the plan.
        let r = run(
            &overseer,
            "crew.ask_person",
            json!({ "text": "The docs build is green and fast: is it done?", "plan_id": id.to_string(),
                    "proposal": { "met": "the docs build ran green in 2 minutes" } }),
        )
        .await;
        assert!(r.success, "{r:?}");
        tokio::time::sleep(Duration::from_millis(20)).await;
        person_says_in_a_thread(&overseer, "Yes!");
        let r = run(
            &overseer,
            "crew.plan_update",
            json!({ "plan_id": id.to_string(), "met": { "person_said": "yes" }, "why": "They said so." }),
        )
        .await;
        assert!(r.success, "{r:?}");
        let p = d.plan(id).await.unwrap().summary.plan;
        assert_eq!(p.met.as_ref().map(|m| m.by), Some(workforce_desk::goal::MetBy::Person));
        let t = d.ticket(p.goal_ticket.unwrap()).await.unwrap();
        assert_eq!(t.ticket.state, TicketState::Done);
        eventually("the closing turn to finish it", async || {
            (d.plan(id).await.unwrap().summary.plan.state == PlanState::Done).then_some(())
        })
        .await;
    })
    .await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_measured_goal_is_met_by_its_reading_in_the_plans_turn() {
    with_test_pool(workforce_desk::store::build_pool, |pool| async move {
        let dir = tempfile::tempdir().unwrap();
        let base = std::fs::canonicalize(dir.path()).unwrap();
        let gh = github_turning_green().await;
        let (overseer, crew, d, _llm) = boss_on(&base, pool, ACTS, &gh).await;
        let s = session_on(&overseer, &d, &base.to_string_lossy(), "Make CI green").await;
        let id = agreed_plan(&overseer, &s, green("acme/web", 1)).await;
        let p = d.plan(id).await.unwrap().summary.plan;
        assert_eq!(p.state, PlanState::Running);
        assert!(p.met.is_none(), "red as it was saved");

        crew.plan_turn_now(id, vec!["A turn of its session ended.".into()])
            .await;
        let p = d.plan(id).await.unwrap().summary.plan;
        assert_eq!(
            p.met.as_ref().map(|m| m.by),
            Some(workforce_desk::goal::MetBy::Result)
        );
        assert_eq!(p.state, PlanState::Done, "closed in that turn");
        let t = d.ticket(p.goal_ticket.unwrap()).await.unwrap();
        assert_eq!(t.ticket.state, TicketState::Done);
    })
    .await;
}

#[tokio::test(flavor = "multi_thread")]
async fn an_agreed_goal_covers_work_added_toward_it_and_never_its_own_tickets_words() {
    with_test_pool(workforce_desk::store::build_pool, |pool| async move {
        let dir = tempfile::tempdir().unwrap();
        let base = std::fs::canonicalize(dir.path()).unwrap();
        let folder = base.to_string_lossy().to_string();
        let gh = github().await;
        let (overseer, _crew, d, _llm) = boss_on(&base, pool, ACTS, &gh).await;
        let s = session_on(&overseer, &d, &folder, "Fix the docs build").await;
        let id = agreed_plan(&overseer, &s, judged()).await;
        let goal_ticket = d.plan(id).await.unwrap().summary.plan.goal_ticket.unwrap();
        let key = d.ticket(goal_ticket).await.unwrap().key.unwrap();

        // A ticket added toward it applies at once.
        let r = run(
            &overseer,
            "crew.plan_update",
            json!({ "plan_id": id.to_string(), "why": "The lint step fails too.",
                    "tickets": [{ "key": key, "folder": folder }, { "title": "Fix the lint step", "folder": folder }] }),
        )
        .await;
        assert!(r.success, "{r:?}");
        assert_eq!(r.output["applied"], true, "{r:?}");
        assert!(!d.plan(id).await.unwrap().summary.plan.awaiting());

        // Its own ticket is taken on from its board, so its words are never
        // the plan's to change.
        let r = run(
            &overseer,
            "crew.plan_update",
            json!({ "plan_id": id.to_string(), "why": "Narrower.",
                    "tickets_edit": [{ "key": key, "body": "Only the API docs." }] }),
        )
        .await;
        assert!(!r.success, "{r:?}");
    })
    .await;
}

/// A goal the person changes themselves applies on their own words, with
/// no question of the boss's first; a goal in other words than theirs
/// doesn't.
#[tokio::test(flavor = "multi_thread")]
async fn a_goal_the_person_changes_themselves_applies_on_their_words() {
    with_test_pool(workforce_desk::store::build_pool, |pool| async move {
        let dir = tempfile::tempdir().unwrap();
        let base = std::fs::canonicalize(dir.path()).unwrap();
        let gh = github().await;
        let (overseer, _crew, d, _llm) = boss_on(&base, pool, ACTS, &gh).await;
        let s = session_on(&overseer, &d, &base.to_string_lossy(), "Fix the docs build").await;
        let id = agreed_plan(&overseer, &s, judged()).await;
        tokio::time::sleep(Duration::from_millis(20)).await;
        person_says(
            &overseer,
            "Actually, change the goal to keep the docs build green and fast",
        );
        let said = "change the goal to keep the docs build green and fast";
        let r = run(
            &overseer,
            "crew.plan_update",
            json!({ "plan_id": id.to_string(), "goal": "Make the docs build quick",
                    "person_said": said, "why": "They asked." }),
        )
        .await;
        assert!(!r.success, "{r:?}");
        assert!(r.error.unwrap().contains("set goal to what they wrote"));
        let r = run(
            &overseer,
            "crew.plan_update",
            json!({ "plan_id": id.to_string(), "goal": "Keep the docs build green and fast",
                    "person_said": said, "why": "They asked." }),
        )
        .await;
        assert!(r.success, "{r:?}");
        let p = d.plan(id).await.unwrap().summary.plan;
        assert_eq!(p.goal, "Keep the docs build green and fast");
        assert!(!p.awaiting(), "their own words approve it");
    })
    .await;
}

/// The boss proposes a new goal and result in its DM, as a table; the
/// person's plain "yes" right after it replaces both (DW-757), as their
/// "ok" in a thread does to one proposed there, and the goal checks then
/// measure the new result. A reply that isn't plainly to that proposal,
/// or a goal it didn't say, still refuses.
#[tokio::test(flavor = "multi_thread")]
async fn the_persons_plain_yes_to_the_bosss_proposal_in_the_dm_replaces_the_goal() {
    with_test_pool(workforce_desk::store::build_pool, |pool| async move {
        let dir = tempfile::tempdir().unwrap();
        let base = std::fs::canonicalize(dir.path()).unwrap();
        let gh = github().await;
        let (overseer, crew, d, _llm) = boss_on(&base, pool, ACTS, &gh).await;
        let s = session_on(&overseer, &d, &base.to_string_lossy(), "Fix the docs build").await;
        let id = agreed_plan(&overseer, &s, judged()).await;
        let goal = "Keep acme/web's CI green on main";
        let proposal = "**Proposed done-when goal:**\n\n| Goal | Done when |\n|---|---|\n| Keep acme/web’s CI green on main | **Done when CI on main is green** |\n\nYes, or change it?";
        let change = |goal: &str, said: &str| {
            json!({ "plan_id": id.to_string(), "goal": goal, "result": green("acme/web", 1),
                    "person_said": said, "why": "They agreed." })
        };
        let pause = || tokio::time::sleep(Duration::from_millis(20));

        pause().await;
        boss_says(&overseer, proposal);
        pause().await;
        person_says(&overseer, "yes");
        pause().await;
        person_says(&overseer, "hmm, wait a moment");
        let r = run(&overseer, "crew.plan_update", change(goal, "yes")).await;
        assert!(!r.success, "more came after the yes: {r:?}");
        let why = r.error.unwrap();
        assert!(why.contains("Propose the change to them in your DM"), "{why}");
        assert!(why.contains("never about this check"), "{why}");

        // A "no", a yes to a message that asked more, and a yes on another
        // item's buttons don't agree to it.
        let button = format!("{}Shall I merge #5?”: Yes", workforce_desk::goal::ANSWER_LEAD);
        let asks_more = format!("Shall I merge #5 first?\n\n{proposal}");
        for (asked, reply, why) in [
            (proposal, "no", "Propose the change"),
            (asks_more.as_str(), "yes", "asked more than this change"),
            (proposal, button.as_str(), "Propose the change"),
        ] {
            pause().await;
            boss_says(&overseer, asked);
            pause().await;
            person_says(&overseer, reply);
            let r = run(&overseer, "crew.plan_update", change(goal, "yes")).await;
            assert!(!r.success, "{reply}: {r:?}");
            let err = r.error.unwrap();
            assert!(err.contains(why), "{reply}: {err}");
        }

        pause().await;
        boss_says(&overseer, proposal);
        pause().await;
        person_says(&overseer, "Yes.");
        let r = run(&overseer, "crew.plan_update", change("Ship the docs", "yes")).await;
        assert!(!r.success, "{r:?}");
        let why = r.error.unwrap();
        assert!(why.contains("doesn't say the goal “Ship the docs”"), "{why}");
        assert_eq!(d.plan(id).await.unwrap().summary.plan.goal, "Keep the docs build green");

        let r = run(&overseer, "crew.plan_update", change(goal, "yes")).await;
        assert!(r.success, "{r:?}");
        let p = d.plan(id).await.unwrap().summary.plan;
        assert_eq!(p.goal, goal);
        assert_eq!(p.result.as_ref().map(|r| r.words.as_str()), Some("Done when CI on main is green"));
        assert!(!p.awaiting(), "their yes applies it");
        assert!(p.met.is_none(), "red as it was changed");

        // Proposed in a thread of the DM, answered there.
        let thread = workforce_core::desk::dm_thread_channel(&uuid::Uuid::new_v4().to_string());
        let wider = "Keep acme/web's CI green on main and the docs built";
        let in_thread = format!("How about this goal?\n\n| Goal |\n|---|\n| {wider} |");
        pause().await;
        says_in_thread(&overseer, &thread, &in_thread, false);
        pause().await;
        says_in_thread(&overseer, &thread, "ok", true);
        pause().await;
        says_in_thread(&overseer, &thread, "and the API docs too?", true);
        let r = run(&overseer, "crew.plan_update", change(wider, "ok")).await;
        assert!(!r.success, "more came after the ok: {r:?}");
        pause().await;
        says_in_thread(&overseer, &thread, &in_thread, false);
        pause().await;
        says_in_thread(&overseer, &thread, "ok", true);
        let r = run(&overseer, "crew.plan_update", change(wider, "ok")).await;
        assert!(r.success, "{r:?}");
        let p = d.plan(id).await.unwrap().summary.plan;
        assert_eq!(p.goal, wider);
        assert!(!p.awaiting(), "their ok applies it");
        // The DM's yes, applied already, doesn't change it back.
        let r = run(&overseer, "crew.plan_update", change(goal, "yes")).await;
        assert!(!r.success, "{r:?}");
        assert_eq!(d.plan(id).await.unwrap().summary.plan.goal, wider);

        ci_turns_green(&gh).await;
        crew.plan_turn_now(id, vec!["A turn of its session ended.".into()])
            .await;
        let p = d.plan(id).await.unwrap().summary.plan;
        assert_eq!(
            p.met.as_ref().map(|m| m.by),
            Some(workforce_desk::goal::MetBy::Result),
            "measured by the new result"
        );
    })
    .await;
}

/// The person's replies on every path: an answer to the boss's question,
/// a first goal agreed and work started on their word. A "no" never says
/// yes, whatever its length; one that is neither is said to be unclear; a
/// short yes counts in its many words, and a longer one on its own words.
const REPLIES: [(&str, Option<&str>); 6] = [
    ("no", Some("doesn't agree to it")),
    ("not yet", Some("doesn't agree to it")),
    (
        "hmm",
        Some("couldn't read the person's “hmm” as a clear yes"),
    ),
    ("ok", None),
    ("go for it", None),
    ("Yes, that looks right to me", None),
];

/// Replies that ask for a change to what was put to them: never a yes to
/// it as it stands, answering or agreeing work.
const CHANGES_IT: [(&str, Option<&str>); 2] = [
    (
        "Sure, drop the staging part",
        Some("couldn't read the person's “Sure, drop the staging part” as a clear yes"),
    ),
    (
        "Yes start it, skip the deploy step",
        Some("couldn't read the person's “Yes start it, skip the deploy step” as a clear yes"),
    ),
];

#[tokio::test(flavor = "multi_thread")]
async fn a_no_never_answers_or_agrees_on_any_path_and_a_longer_yes_does() {
    with_test_pool(workforce_desk::store::build_pool, |pool| async move {
        let dir = tempfile::tempdir().unwrap();
        let base = std::fs::canonicalize(dir.path()).unwrap();
        let folder = base.to_string_lossy().to_string();
        let gh = github().await;
        let (overseer, crew, d, _llm) = boss_on(&base, pool, ACTS, &gh).await;
        let pause = || tokio::time::sleep(Duration::from_millis(20));
        let check = |r: &workforce_agent::tools::ToolResult, said: &str, refused: Option<&str>| {
            match refused {
                Some(why) => {
                    assert!(!r.success, "{said}: {r:?}");
                    let err = r.error.clone().unwrap();
                    assert!(err.contains(why), "{said}: {err}");
                }
                None => assert!(r.success, "{said}: {r:?}"),
            }
        };
        let s = session_on(&overseer, &d, &folder, "Fix the docs build").await;
        let id = agreed_plan(&overseer, &s, judged()).await;
        let ask = async |to: &str, options: Value| {
            let r = run(
                &overseer,
                "crew.ask_person",
                json!({ "text": format!("Shall the goal be: {to}?"), "plan_id": id.to_string(),
                        "proposal": { "goal": to }, "options": options, "recommended": "Apply goal" }),
            )
            .await;
            assert!(r.success, "{r:?}");
            asked(&d, id).await
        };
        let apply = async |to: &str, said: &str| {
            run(
                &overseer,
                "crew.plan_update",
                json!({ "plan_id": id.to_string(), "goal": to, "person_said": said, "why": "Their answer." }),
            )
            .await
        };

        // Its buttons are Yes and No, whatever the boss labels them, and a
        // click of Yes applies it. Each click is made with the DM idle, as
        // a person's click lands between the boss's turns.
        let to = "Keep the docs build green, clicked yes";
        let item = ask(to, json!(["Apply goal", "Hold, I'll look"])).await;
        let i = d.store().open_item(item).await.unwrap().unwrap();
        assert_eq!(i.options, ["Yes", "No"]);
        pause().await;
        dm_idle(&overseer).await;
        crew.answer_item(item, "Yes").await.unwrap();
        check(&apply(to, "Yes").await, "Yes", None);
        // A click of No refuses it.
        let to = "Keep the docs build green, clicked no";
        let item = ask(to, json!(["Apply goal"])).await;
        pause().await;
        dm_idle(&overseer).await;
        crew.answer_item(item, "No").await.unwrap();
        check(&apply(to, "No").await, "No", Some("doesn't agree to it"));
        dm_idle(&overseer).await;

        for (n, (said, refused)) in REPLIES.into_iter().chain(CHANGES_IT).enumerate() {
            let to = format!("Keep the docs build green, step {n}");
            let item = ask(&to, json!([])).await;
            pause().await;
            person_says(&overseer, said);
            check(&apply(&to, said).await, said, refused);
            if refused.is_some() {
                let _ = d
                    .store()
                    .item_close(item, workforce_desk::open_item::ClosedBy::Boss, "Not a yes.")
                    .await;
            }
        }
        assert_eq!(
            d.plan(id).await.unwrap().summary.plan.goal,
            "Keep the docs build green, step 5"
        );

        for field in ["agreed", "start_now"] {
            for (n, (said, refused)) in REPLIES.into_iter().chain(CHANGES_IT).enumerate() {
                pause().await;
                person_says(&overseer, said);
                let r = run(
                    &overseer,
                    "crew.plan",
                    json!({ "goal": format!("Make suite {field} {n} green"), field: { "person_said": said },
                            "result": judged(),
                            "tickets": [{ "title": format!("Fix suite {field} {n}"), "folder": folder }] }),
                )
                .await;
                check(&r, said, refused);
            }
        }
    })
    .await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_dropped_goal_ticket_waits_for_the_person() {
    with_test_pool(workforce_desk::store::build_pool, |pool| async move {
        let dir = tempfile::tempdir().unwrap();
        let base = std::fs::canonicalize(dir.path()).unwrap();
        let folder = base.to_string_lossy().to_string();
        let gh = github().await;
        let (overseer, _crew, d, _llm) = boss_on(&base, pool, ACTS, &gh).await;
        let s = session_on(&overseer, &d, &folder, "Fix the docs build").await;
        let id = agreed_plan(&overseer, &s, judged()).await;
        let r = run(
            &overseer,
            "crew.plan_update",
            json!({ "plan_id": id.to_string(), "why": "Another way.",
                    "tickets": [{ "title": "Rewrite the docs build", "folder": folder }] }),
        )
        .await;
        assert!(r.success, "{r:?}");
        assert_eq!(r.output["applied"], false, "{r:?}");
        assert!(d.plan(id).await.unwrap().summary.plan.awaiting());
    })
    .await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_machines_session_gets_its_goal_on_the_plan_that_runs_it() {
    with_test_pool(workforce_desk::store::build_pool, |pool| async move {
        let dir = tempfile::tempdir().unwrap();
        let base = std::fs::canonicalize(dir.path()).unwrap();
        let folder = base.to_string_lossy().to_string();
        let gh = github().await;
        let (overseer, crew, d, _llm) = boss_on(&base, pool, ACTS, &gh).await;
        // A plan of the boss's, approved, with no goal: its ticket runs on
        // the devbox.
        let p = super::plans::proposed(&d, &base, 1).await;
        let id = p.summary.plan.id;
        d.approve_plan(id, 1, &By::you()).await.unwrap();
        let ticket = p.tickets[0].ticket.ticket.id;
        let key = p.tickets[0].ticket.key.clone().unwrap();
        let remote = uuid::Uuid::new_v4();
        let item = format!("devbox/{remote}");
        // Its ticket is taken by a session on the devbox, as a plan's start
        // there leaves it: the plans' loop then knows it as the plan's.
        eventually("its ticket taken there", async || {
            let mut row = d.store().ticket(ticket).await.unwrap()?;
            row.state = TicketState::Doing;
            row.assignee = Some(workforce_desk::crew::Party {
                kind: workforce_desk::crew::PartyKind::Session,
                id: item.clone(),
                name: "flaky-tests".into(),
                engine: Some(workforce_desk::model::Engine::Claude),
                origin: None,
            });
            d.store().put_ticket(&mut row).await.ok()
        })
        .await;
        // And Desk on the devbox said it is working there, as its read of
        // the devbox leaves it: the loop keeps it, its ticket being held.
        crew.machine_seen.lock().insert(
            item.clone(),
            crate::desk_crew::plan::MachineSeen {
                plan: id,
                ticket,
                boss: "desk".into(),
                goal: p.summary.plan.goal.clone(),
                key: key.clone(),
                engine: Some(workforce_desk::model::Engine::Claude),
                state: "working".into(),
                reply: None,
                misses: 0,
            },
        );
        // Kept through the plans' loop's passes.
        tokio::time::sleep(Duration::from_millis(300)).await;
        assert!(crew.machine_seen.lock().contains_key(&item));

        // A new plan for it is pointed at the one that runs it.
        person_says(&overseer, "Yes, get the flaky tests fixed today");
        let r = run(
            &overseer,
            "crew.plan",
            json!({ "goal": "Fix the flaky tests", "agreed": { "person_said": "get the flaky tests fixed today" },
                    "result": judged(), "tickets": [{ "session": item }] }),
        )
        .await;
        assert!(!r.success);
        let why = r.error.unwrap();
        assert!(why.contains(&format!("runs for your plan {id}")), "{why}");
        // That plan takes the goal on the person's words.
        let r = run(
            &overseer,
            "crew.plan_update",
            json!({ "plan_id": id.to_string(), "result": judged(),
                    "agreed": { "person_said": "get the flaky tests fixed today" }, "why": "They agreed it." }),
        )
        .await;
        assert!(r.success, "{r:?}");
        let p = d.plan(id).await.unwrap().summary.plan;
        assert!(p.result.is_some());
        assert_eq!(p.agreed.as_deref(), Some("get the flaky tests fixed today"));
        assert!(!p.awaiting());
        let _ = folder;
    })
    .await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_bound_boss_starts_a_plan_on_the_persons_words_and_is_told_how_when_they_dont_check() {
    with_test_pool(workforce_desk::store::build_pool, |pool| async move {
        let dir = tempfile::tempdir().unwrap();
        let base = std::fs::canonicalize(dir.path()).unwrap();
        let folder = base.to_string_lossy().to_string();
        let gh = github().await;
        let (overseer, _crew, d, _llm) = boss_on(&base, pool, ACTS, &gh).await;
        let plan = |said: &str| {
            json!({ "goal": "Make the suite green", "start_now": { "person_said": said },
                    "result": judged(), "tickets": [{ "title": "Fix it", "folder": folder }] })
        };
        let r = run(&overseer, "crew.plan", plan("go ahead, start it")).await;
        assert!(!r.success);
        let why = r.error.unwrap();
        assert!(why.contains("Quote one message of theirs exactly"), "{why}");
        assert!(d.plans(None, None).await.unwrap().is_empty());
        person_says(&overseer, "Go ahead, start it");
        let r = run(&overseer, "crew.plan", plan("go ahead, start it")).await;
        assert!(r.success, "{r:?}");
        let id: uuid::Uuid = r.output["proposed"]["plan_id"]
            .as_str()
            .unwrap()
            .parse()
            .unwrap();
        assert_eq!(
            d.plan(id).await.unwrap().summary.plan.state,
            PlanState::Running
        );
    })
    .await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_part_whose_reads_keep_failing_is_repaired_where_it_reads_from() {
    with_test_pool(workforce_desk::store::build_pool, |pool| async move {
        let dir = tempfile::tempdir().unwrap();
        let base = std::fs::canonicalize(dir.path()).unwrap();
        let gh = github().await;
        let (overseer, crew, d, _llm) = boss_on(&base, pool, ACTS, &gh).await;
        let s = session_on(&overseer, &d, &base.to_string_lossy(), "Make CI green").await;
        // Read failing as it is saved: kept, not refused.
        let id = agreed_plan(&overseer, &s, green("acme/flaky", 1)).await;
        let repair = json!({ "plan_id": id.to_string(), "why": "The repository moved.",
            "repair": { "part": 1, "measure": { "kind": "ci", "repo": "acme/web", "branch": "main",
                "want": { "kind": "green", "runs": 1 } } } });
        let r = run(&overseer, "crew.plan_update", repair.clone()).await;
        assert!(!r.success, "{r:?}");
        assert!(r
            .error
            .unwrap()
            .contains("only once its newest 3 reads failed"));
        crew.plan_turn_now(id, vec!["One.".into()]).await;
        crew.plan_turn_now(id, vec!["Two.".into()]).await;
        let r = run(&overseer, "crew.plan_update", repair).await;
        assert!(r.success, "{r:?}");
        let p = d.plan(id).await.unwrap().summary.plan;
        assert_eq!(
            p.result,
            Some(serde_json::from_value(green("acme/web", 1)).unwrap())
        );
        assert!(!p.awaiting(), "a repair needs no approval");
    })
    .await;
}

/// A question about a plan's goal that the person settles by approving the
/// plan on its card, as it proposed, closes: it doesn't stay open in their
/// list, nor apply again. One about another goal stays.
#[tokio::test(flavor = "multi_thread")]
async fn a_goal_question_the_person_settles_on_the_plans_card_closes_with_it() {
    use workforce_desk::open_item::OpenState;
    with_test_pool(workforce_desk::store::build_pool, |pool| async move {
        let dir = tempfile::tempdir().unwrap();
        let base = std::fs::canonicalize(dir.path()).unwrap();
        let gh = github().await;
        let (_overseer, _crew, d, _llm) = boss_on(&base, pool, ACTS, &gh).await;
        let p = super::plans::proposed(&d, &base, 1).await;
        let id = p.summary.plan.id;
        let store = d.store();
        let settled = goal_asked(
            store,
            id,
            "Shall the goal be a green suite?",
            &p.summary.plan.goal.to_uppercase(),
        )
        .await;
        let other = goal_asked(
            store,
            id,
            "Shall the goal be the docs shipped?",
            "Ship the docs",
        )
        .await;
        d.approve_plan(id, p.summary.plan.revision, &By::you())
            .await
            .unwrap();
        let open: Vec<_> = store
            .goal_asks(id)
            .await
            .unwrap()
            .into_iter()
            .map(|a| a.item)
            .collect();
        assert_eq!(open, [other]);
        let i = store.open_item(settled).await.unwrap().unwrap();
        assert_eq!(i.state, OpenState::Closed);
        assert_eq!(
            i.closed_why.as_deref(),
            Some("You approved it on the plan's card.")
        );
        assert_eq!(
            store.open_item(other).await.unwrap().unwrap().state,
            OpenState::Open
        );
    })
    .await;
}

/// A short "ok" agrees only to what the boss said last: one written before
/// the boss's newest DM message answered something else.
#[tokio::test(flavor = "multi_thread")]
async fn a_short_agreement_counts_only_after_the_bosss_last_message() {
    with_test_pool(workforce_desk::store::build_pool, |pool| async move {
        let dir = tempfile::tempdir().unwrap();
        let base = std::fs::canonicalize(dir.path()).unwrap();
        let folder = base.to_string_lossy().to_string();
        let gh = github().await;
        let (overseer, _crew, d, _llm) = boss_on(&base, pool, ACTS, &gh).await;
        let plan = json!({ "goal": "Make the suite green", "start_now": { "person_said": "ok" },
                           "result": judged(), "tickets": [{ "title": "Fix it", "folder": folder }] });
        person_says(&overseer, "ok");
        tokio::time::sleep(Duration::from_millis(20)).await;
        boss_says(&overseer, "Shall I make the suite green? It's 3 flaky tests.");
        let r = run(&overseer, "crew.plan", plan.clone()).await;
        assert!(!r.success, "{r:?}");
        let why = r.error.unwrap();
        assert!(why.contains("came before your last message"), "{why}");
        assert!(d.plans(None, None).await.unwrap().is_empty());
        tokio::time::sleep(Duration::from_millis(20)).await;
        person_says(&overseer, "ok");
        // A post of Desk's landing after it isn't the boss asking anything.
        tokio::time::sleep(Duration::from_millis(20)).await;
        desk_posts(&overseer, "**Update.** acme-web finished its tests.").await;
        let r = run(&overseer, "crew.plan", plan).await;
        assert!(r.success, "{r:?}");
    })
    .await;
}

/// `text` asked of the person with buttons by `boss`: its item.
async fn asked_with_buttons(
    boss: &AgentService,
    d: &workforce_desk::Desk,
    text: &str,
) -> uuid::Uuid {
    let r = run(
        boss,
        "crew.ask_person",
        json!({ "text": text, "options": ["Merge", "Hold"] }),
    )
    .await;
    assert!(r.success, "{r:?}");
    d.store()
        .items_not_closed(Some("desk"))
        .await
        .unwrap()
        .into_iter()
        .find(|i| i.text == text)
        .unwrap()
        .id
}

/// The person's `answer` to `asked` as it reaches `boss` in its DM.
fn answer_said(asked: &str, answer: &str) -> String {
    format!("{}{asked}”: {answer}", workforce_desk::goal::ANSWER_LEAD)
}

/// Whether `said`, the person's, is in `boss`'s DM.
fn in_dm(boss: &AgentService, said: &str) -> bool {
    boss.sessions
        .find(DESK_CHANNEL, PERSON)
        .unwrap()
        .is_some_and(|s| {
            s.messages.iter().any(|m| {
                m.role == workforce_common::session::MessageRole::User && m.content.contains(said)
            })
        })
}

/// Whether the person was told an answer to `asked` bounced.
fn bounced(boss: &AgentService, asked: &str) -> bool {
    posted(boss, &format!("Your answer to “{asked}” didn't reach")) > 0
}

/// The boss's DM with the person, once it is there and free.
async fn free_dm(boss: &AgentService) -> String {
    dm_idle(boss).await;
    boss.sessions
        .find(DESK_CHANNEL, PERSON)
        .unwrap()
        .unwrap()
        .id
}

/// The person's answer on the Floor reaches the boss as their own words:
/// it agrees the goal it says, as if typed in the DM.
#[tokio::test(flavor = "multi_thread")]
async fn an_answer_on_the_floor_is_the_persons_and_agrees_the_goal_it_says() {
    with_test_pool(workforce_desk::store::build_pool, |pool| async move {
        let dir = tempfile::tempdir().unwrap();
        let base = std::fs::canonicalize(dir.path()).unwrap();
        let gh = github().await;
        let (overseer, crew, d, _llm) = boss_on(&base, pool, ACTS, &gh).await;
        let s = session_on(&overseer, &d, &base.to_string_lossy(), "Fix the docs build").await;
        let ask = "Shall I keep the docs build green as the goal?";
        let r = run(&overseer, "crew.ask_person", json!({ "text": ask })).await;
        assert!(r.success, "{r:?}");
        let item = d
            .store()
            .items_not_closed(Some("desk"))
            .await
            .unwrap()
            .into_iter()
            .find(|i| i.text == ask)
            .unwrap()
            .id;
        let answer = "Yes, keep the docs build green";
        crew.answer_item(item, answer).await.unwrap();
        eventually("their answer in the DM", async || {
            in_dm(&overseer, &answer_said(ask, answer)).then_some(())
        })
        .await;
        let r = run(
            &overseer,
            "crew.plan",
            json!({ "goal": "Keep the docs build green", "agreed": { "person_said": answer },
                    "result": judged(), "tickets": [{ "session": s.id.to_string() }] }),
        )
        .await;
        assert!(r.success, "{r:?}");
    })
    .await;
}

/// Clicks that land while a turn that can't take them in runs in the
/// boss's DM wait for it: each item stays answered, nothing bounces back,
/// and every answer reaches the boss once the turn ends.
#[tokio::test(flavor = "multi_thread")]
async fn clicks_while_the_boss_is_mid_turn_all_reach_it_once_the_turn_ends() {
    with_test_pool(workforce_desk::store::build_pool, |pool| async move {
        use workforce_desk::open_item::OpenState;
        let dir = tempfile::tempdir().unwrap();
        let base = std::fs::canonicalize(dir.path()).unwrap();
        let gh = github().await;
        let (overseer, crew, d, _llm) = boss_on(&base, pool, ACTS, &gh).await;
        let asks = [
            "Shall I merge the docs fix?",
            "Shall I merge the guide fix?",
        ];
        let mut items = Vec::new();
        for ask in asks {
            items.push(asked_with_buttons(&overseer, &d, ask).await);
        }
        let dm = free_dm(&overseer).await;
        let (running, _) = hold_session(&overseer, &dm).await;
        for item in &items {
            assert_eq!(crew.answer_item(*item, "Merge").await.unwrap(), "Answered.");
        }
        tokio::time::sleep(Duration::from_millis(300)).await;
        for (ask, item) in asks.iter().zip(&items) {
            assert!(
                !in_dm(&overseer, &answer_said(ask, "Merge")),
                "held behind the running turn"
            );
            let i = d.store().open_item(*item).await.unwrap().unwrap();
            assert_eq!(i.state, OpenState::Closed);
        }
        drop(running);
        for ask in asks {
            eventually("the answer reaches the boss", async || {
                in_dm(&overseer, &answer_said(ask, "Merge")).then_some(())
            })
            .await;
        }
        dm_idle(&overseer).await;
        for (ask, item) in asks.iter().zip(&items) {
            let i = d.store().open_item(*item).await.unwrap().unwrap();
            assert_eq!(i.state, OpenState::Closed);
            assert!(!bounced(&overseer, ask));
        }
    })
    .await;
}

/// Clicks that land while a turn that takes follow-ups runs in the boss's
/// DM join it, as the person's typed words do: all of them in that one
/// turn, and a click then "hold off" reach the boss in that order.
#[tokio::test(flavor = "multi_thread")]
async fn clicks_while_the_boss_is_mid_turn_join_that_turn_in_the_order_given() {
    with_test_pool(workforce_desk::store::build_pool, |pool| async move {
        use crate::hub_core::live_turns::Control;
        use workforce_agent::runner::InjectedMessage;
        let dir = tempfile::tempdir().unwrap();
        let base = std::fs::canonicalize(dir.path()).unwrap();
        let gh = github().await;
        let (overseer, crew, d, _llm) = boss_on(&base, pool, ACTS, &gh).await;
        let asks = [
            "Shall I merge the docs fix?",
            "Shall I merge the guide fix?",
        ];
        let mut items = Vec::new();
        for ask in asks {
            items.push(asked_with_buttons(&overseer, &d, ask).await);
        }
        let dm = free_dm(&overseer).await;
        let (_running, mut inbox) = hold_session(&overseer, &dm).await;
        for item in &items {
            assert_eq!(crew.answer_item(*item, "Merge").await.unwrap(), "Answered.");
        }
        let typed = InjectedMessage::UserMessage {
            text: "actually, hold off".into(),
            user: PERSON.into(),
            external_id: None,
        };
        let follow_up = Control::FollowUp {
            message: typed,
            place: Some(workforce_common::channels::desk::DeskPlace::Dm { thread: None }),
            queue: false,
        };
        let said = |m: Option<InjectedMessage>| match m {
            Some(InjectedMessage::UserMessage { text, .. }) => text,
            other => panic!("not the person's words: {other:?}"),
        };
        let mut heard = Vec::new();
        for _ in asks {
            heard.push(said(
                tokio::time::timeout(Duration::from_secs(10), inbox.rx.recv())
                    .await
                    .unwrap(),
            ));
        }
        heard.sort();
        assert!(overseer.live_turns.apply(&dm, follow_up).await.applied());
        heard.push(said(inbox.rx.recv().await));
        assert_eq!(
            heard,
            [
                answer_said(asks[0], "Merge"),
                answer_said(asks[1], "Merge"),
                "actually, hold off".to_string()
            ]
        );
        assert!(asks.iter().all(|ask| !bounced(&overseer, ask)));
    })
    .await;
}

/// Once no turn runs in `boss`'s DM: a click's answer reaches the boss in
/// a turn of its own there, which another turn running would hold up.
async fn dm_idle(boss: &AgentService) {
    eventually("the DM to be idle", async || {
        let dm = boss.sessions.find(DESK_CHANNEL, PERSON).ok().flatten()?;
        (!boss.live_turns.running(&dm.id)).then_some(())
    })
    .await;
}

/// Desk posts `text` in `boss`'s DM with the person, as an update.
async fn desk_posts(boss: &AgentService, text: &str) {
    use workforce_common::channels::desk::{DeliveryKind, DeskPlace, DeskTarget};
    let target = DeskTarget::of_turn(&DeskPlace::Dm { thread: None }, DESK_CHANNEL, None, false);
    let delivery = crate::desk_surface::Delivery::text(DeliveryKind::Update, text);
    crate::desk_surface::deliver(boss, PERSON, &target, delivery)
        .await
        .unwrap();
}

/// The boss writes `text` in its DM with the person.
fn boss_says(boss: &AgentService, text: &str) {
    let mut dm = boss
        .sessions
        .get_or_create(DESK_CHANNEL, PERSON, "desk")
        .unwrap();
    dm.add_assistant_message(text, 0);
    boss.sessions.save(&dm).unwrap();
    boss.sessions
        .append_to_archive(&dm.id, dm.messages.last().unwrap())
        .unwrap();
}

/// A question of the boss's about plan `plan`'s goal, proposing `goal`.
async fn goal_asked(
    store: &workforce_desk::store::DeskStore,
    plan: uuid::Uuid,
    text: &str,
    goal: &str,
) -> uuid::Uuid {
    use workforce_desk::open_item::{NewItem, OpenKind};
    let i = store
        .item_open(&NewItem::ask("desk", OpenKind::Ask, None, text))
        .await
        .unwrap()
        .item()
        .clone();
    let proposal = workforce_desk::goal::Proposal {
        goal: Some(goal.to_string()),
        ..Default::default()
    };
    store
        .item_set_goal(i.id, plan, &proposal, chrono::Utc::now())
        .await
        .unwrap();
    i.id
}
