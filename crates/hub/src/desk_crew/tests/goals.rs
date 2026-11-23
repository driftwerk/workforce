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

const ACTS: &str = "[oversight]\nenabled = true\nlevel = \"act\"\n[oversight.act]\nhand_out_work = true\nnudge = true\n";

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
        .and(path("/repos/acme/nope/actions/runs"))
        .respond_with(ResponseTemplate::new(404).set_body_json(json!({ "message": "Not Found" })))
        .mount(&gh)
        .await;
    gh
}

/// The boss at `oversight`, its GitHub `gh` with the person's own key; its
/// DM open, its crew started.
async fn boss_on(
    dir: &Path,
    pool: workforce_postgres::PostgresPool,
    oversight: &str,
    gh: &wiremock::MockServer,
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
    let crew = CrewHub::start(state, d.clone(), FAST, CancellationToken::new());
    eventually("the plan tools", async || {
        overseer.tools.has("crew.plan").then_some(())
    })
    .await;
    (overseer, crew, d, llm)
}

async fn run(boss: &AgentService, tool: &str, input: Value) -> workforce_agent::tools::ToolResult {
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
                .is_some_and(|n| n.contains("has work from you and no saved goal: if the person agreed a goal for it")),
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
        assert!(r.error.unwrap().contains("aren't the person's words"));
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
        assert!(r.error.unwrap().contains("answered a different proposal"));
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
        assert!(r.error.unwrap().starts_with("Only a reading or the person closes a goal"));

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
