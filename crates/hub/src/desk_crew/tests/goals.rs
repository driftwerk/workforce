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
        let r = run(
            &overseer,
            "crew.plan_update",
            json!({ "plan_id": id.to_string(), "result": green("acme/web", 2),
                    "person_said": "make it two green runs in a row", "why": "Steadier." }),
        )
        .await;
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
    let mut thread = boss
        .sessions
        .get_or_create(&channel, PERSON, "desk")
        .unwrap();
    thread.add_user_message(text, None);
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
        crew.machine_seen.lock().insert(
            item.clone(),
            crate::desk_crew::plan::MachineSeen {
                plan: id,
                ticket,
                boss: "desk".into(),
                goal: p.summary.plan.goal.clone(),
                key: key.clone(),
                engine: None,
                state: "working".into(),
                reply: None,
                misses: 0,
            },
        );

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
