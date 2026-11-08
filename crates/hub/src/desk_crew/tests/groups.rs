//! Groups as boundaries: two clients' groups, Acme and Atlas, each with a
//! boss of its own (Sarah, Mia), a group with none (Infra), and the main
//! overseer (Desk) over everything else. Each boss reads and acts on its own
//! group alone, through every crew tool; the main overseer sees nothing
//! inside a bossed group; what happens wakes the overseer whose scope it is.

use super::*;
use workforce_common::oversight::Scope;
use workforce_desk::board::TicketState;
use workforce_desk::crew::{Event, Party};
use workforce_desk::line::{Asked, LineKind, LineMessage};
use workforce_desk::model::SessionView;
use workforce_desk::{NewBoard, NewTicket};

use super::super::tools::{ACT, READ};

/// What only Acme's rows say, and what only Atlas's say: neither may reach
/// the other's boss, or the main overseer.
const ACME_SECRET: &str = "acmesecret";
const ATLAS_SECRET: &str = "atlassecret";

const ACTS: &str = "level = \"act\"\n[oversight.act]\nhand_out_work = true\nmessage_agents = true\nmessage_terminal_sessions = true\ninterrupt = true\n[oversight.wake]\nquiet_minutes = 0\nwaiting_minutes = 0\n";

/// What one group holds, for the tools to be pointed at.
struct Client {
    session: SessionView,
    agent: &'static str,
    boss: &'static str,
    ticket: String,
    ask: uuid::Uuid,
    /// A plan its boss proposed, on its board.
    plan: uuid::Uuid,
    /// A handover note its boss is asked to write.
    handover: uuid::Uuid,
}

struct Clients {
    desk: Arc<AgentService>,
    sarah: Arc<AgentService>,
    mia: Arc<AgentService>,
    crew: Arc<CrewHub>,
    d: Desk,
    acme: Client,
    atlas: Client,
    infra: SessionView,
    loose: SessionView,
    _llms: Vec<wiremock::MockServer>,
}

async fn client(
    d: &Desk,
    dir: &Path,
    group: &str,
    name: &str,
    secret: &str,
    prefix: &str,
    agent: &'static str,
    boss: &'static str,
) -> Client {
    let session = new_session(d, dir, &format!("{name}-web")).await;
    d.change_layout(|l| l.put(&workforce_desk::session_item(session.id), group, None))
        .await
        .unwrap();
    let session = d.view(session.id).unwrap();
    let board = d
        .create_board(NewBoard {
            name: format!("{name} board"),
            group: Some(group.into()),
            prefix: Some(prefix.into()),
        })
        .await
        .unwrap();
    let ticket = d
        .create_ticket(
            NewTicket {
                board: board.id,
                title: format!("{name} plan {secret}"),
                body: format!("Only {name}'s: {secret}"),
                state: Some(TicketState::Todo),
                needs: vec![],
            },
            &By::you(),
        )
        .await
        .unwrap()
        .key
        .unwrap();
    let mut ask = LineMessage::new(
        Party::session(&session),
        Party::manager(boss, boss),
        LineKind::Ask,
        format!("Which port for {secret}?"),
        1,
    );
    ask.asked = Some(Asked::Open);
    ask.group = Some(group.into());
    d.store().put_line(&ask).await.unwrap();
    d.note(Event::new(
        Kind::Started,
        Party::session(&session),
        Some(Party::session(&session)),
        format!("started “the {secret} work”"),
    ));
    let plan = d
        .propose_plan(
            workforce_desk::NewPlan {
                draft: workforce_desk::plan::Draft {
                    goal: format!("{name} goal {secret}"),
                    tickets: vec![workforce_desk::plan::DraftTicket {
                        title: format!("{name} step {secret}"),
                        folder: dir.to_string_lossy().into_owned(),
                        ..Default::default()
                    }],
                },
                board: Some(board.id),
                parallel: None,
                engine: Engine::Claude,
                group: Some(group.into()),
            },
            &Party::manager(boss, boss),
            &By::manager(boss, boss, "For the test."),
        )
        .await
        .unwrap()
        .summary
        .plan
        .id;
    let handover = asked_handover(d, &session, group, boss).await;
    Client {
        session,
        agent,
        boss,
        ticket,
        ask: ask.id,
        plan,
        handover,
    }
}

/// A move of `session` out of `group` previewed, its note asked of `boss`.
async fn asked_handover(d: &Desk, session: &SessionView, group: &str, boss: &str) -> uuid::Uuid {
    use workforce_desk::moves::{
        Choices, Effect, Handover, HandoverState, Item, MoveRow, MoveState,
    };
    let now = workforce_desk::crew::now();
    let item = Item::Session(session.id);
    let m = MoveRow {
        id: uuid::Uuid::new_v4(),
        item: item.clone(),
        from_group: Some(group.into()),
        to_group: None,
        choices: Choices::default(),
        effects: vec![Effect::Tell],
        state: MoveState::Previewed,
        expires_at: now + workforce_desk::moves::PREVIEW_FOR,
        created_at: now,
        done_at: None,
    };
    let h = Handover {
        id: uuid::Uuid::new_v4(),
        move_id: m.id,
        item: item.key(),
        from_group: Some(group.into()),
        to_group: None,
        from_boss: Some(boss.into()),
        to_boss: Some("desk".into()),
        note: "Handing over.".into(),
        drafted_by: Party::desk(),
        asked_until: Some(now + chrono::Duration::minutes(5)),
        edited: false,
        state: HandoverState::Draft,
        created_at: now,
        delivered_at: None,
    };
    d.store()
        .add_move(&m, std::slice::from_ref(&h))
        .await
        .unwrap();
    h.id
}

async fn clients(dir: &Path, pool: workforce_postgres::PostgresPool) -> Clients {
    let (desk_boss, l1) = agent(dir, "desk", "ok").await;
    let (sarah, l2) = agent(dir, "sarah", "ok").await;
    let (mia, l3) = agent(dir, "mia", "ok").await;
    let (scout, l4) = agent(dir, "scout", "ok").await;
    let (ada, l5) = agent(dir, "ada", "ok").await;
    let d = desk(pool, dir).await;
    d.change_layout(|l| {
        l.create("acme", "Acme", &[], None)?
            .create("atlas", "Atlas", &[], None)?
            .create("infra", "Infra", &[], None)
    })
    .await
    .unwrap();
    let acme = client(
        &d,
        dir,
        "acme",
        "acme",
        ACME_SECRET,
        "ACM",
        "scout",
        "sarah",
    )
    .await;
    let atlas = client(&d, dir, "atlas", "atlas", ATLAS_SECRET, "ATL", "ada", "mia").await;
    let infra = new_session(&d, dir, "infra-web").await;
    let loose = new_session(&d, dir, "loose-web").await;
    let (a, b, i) = (acme.session.id, atlas.session.id, infra.id);
    let _ = (a, b);
    d.change_layout(|l| {
        l.put(&workforce_desk::local_agent_item("scout"), "acme", None)?
            .put(&workforce_desk::local_agent_item("ada"), "atlas", None)?
            .put(&workforce_desk::session_item(i), "infra", None)
    })
    .await
    .unwrap();
    write_config(&desk_boss, &format!("[oversight]\nenabled = true\n{ACTS}"));
    write_config(
        &sarah,
        &format!("[oversight]\nenabled = true\ngroup = \"acme\"\n{ACTS}"),
    );
    write_config(
        &mia,
        &format!("[oversight]\nenabled = true\ngroup = \"atlas\"\n{ACTS}"),
    );
    let all = [&desk_boss, &sarah, &mia, &scout, &ada];
    let state = hub(&all.iter().map(|s| Arc::clone(s)).collect::<Vec<_>>());
    let crew = CrewHub::start(state, d.clone(), FAST, CancellationToken::new());
    for boss in [&desk_boss, &sarah, &mia] {
        let dm = boss
            .sessions
            .get_or_create(DESK_CHANNEL, PERSON, boss.agent_id.as_str())
            .unwrap();
        boss.sessions.save(&dm).unwrap();
        eventually("each boss's tools", async || {
            boss.tools.has("crew.move_ticket").then_some(())
        })
        .await;
    }
    Clients {
        desk: desk_boss,
        sarah,
        mia,
        crew,
        d,
        acme,
        atlas,
        infra,
        loose,
        _llms: vec![l1, l2, l3, l4, l5],
    }
}

/// Every way a crew tool can be pointed at `other`'s scope, and whether it
/// must be refused (an id outside) or only read nothing of it (no id). A
/// tool this doesn't know fails the test: a new one needs its case here.
/// A ticket of another scope's is refused as one that isn't there.
fn pointed_at(name: &str, other: &Client, dir: &Path) -> Vec<(Value, bool)> {
    let s = other.session.id.to_string();
    let r = "For the test.";
    match name {
        "crew.overview" => vec![(json!({}), false)],
        "crew.activity" => vec![
            (json!({}), false),
            (json!({ "subject_id": s }), true),
            (json!({ "subject_id": other.agent }), true),
            (json!({ "subject_id": other.boss }), true),
        ],
        "crew.history" => vec![
            (json!({ "session_id": s }), true),
            (json!({ "agent_id": other.agent }), true),
            (json!({ "agent_id": other.boss }), true),
        ],
        "crew.spend" => ["session", "project", "day", "engine", "account", "group"]
            .iter()
            .map(|by| (json!({ "by": by }), false))
            .collect(),
        "crew.board" => vec![(json!({}), false)],
        "crew.nudge" => vec![
            (
                json!({ "session_id": s, "message": "How is it going?", "reason": r }),
                true,
            ),
            (
                json!({ "agent_id": other.agent, "message": "How is it going?", "reason": r }),
                true,
            ),
            (
                json!({ "agent_id": other.boss, "message": "What does your client want?", "reason": r }),
                true,
            ),
            (
                json!({ "session_id": s, "reply_to": other.ask.to_string(), "message": "5433", "reason": r }),
                true,
            ),
        ],
        "crew.escalate" => vec![(
            json!({ "message_id": other.ask.to_string(), "reason": r }),
            true,
        )],
        "crew.approve" | "crew.deny" => vec![(
            json!({ "session_id": s, "approval_id": uuid::Uuid::new_v4().to_string(), "reason": r }),
            true,
        )],
        "crew.stop" | "crew.interrupt" | "crew.take_on" | "crew.release" => {
            vec![(json!({ "session_id": s, "reason": r }), true)]
        }
        "crew.resume" | "crew.restart" => {
            vec![(
                json!({ "session_id": s, "prompt": "Carry on.", "reason": r }),
                true,
            )]
        }
        "crew.start_session" => vec![(
            json!({ "folder": dir.to_string_lossy(), "goal": "Look around.", "reason": r }),
            false,
        )],
        "crew.assign" => vec![(
            json!({ "agent_id": other.agent, "task": "Check it.", "reason": r }),
            true,
        )],
        "crew.assign_ticket" => vec![
            (
                json!({ "ticket": other.ticket, "to": { "newSession": { "folder": dir.to_string_lossy() } }, "reason": r }),
                true,
            ),
            (
                json!({ "ticket": other.ticket, "to": { "agent": other.agent }, "reason": r }),
                true,
            ),
            (
                json!({ "ticket": other.ticket.to_lowercase(), "to": { "session": s }, "reason": r }),
                true,
            ),
        ],
        "crew.move_ticket" => vec![(
            json!({ "ticket": other.ticket, "state": "done", "reason": r }),
            true,
        )],
        "crew.plan" => vec![(
            json!({
                "goal": "Ship it",
                "board": other.ticket.split('-').next().unwrap_or_default(),
                "tickets": [{ "title": "Look around", "folder": dir.to_string_lossy() }],
            }),
            true,
        )],
        "crew.plan_update" => vec![(
            json!({ "plan_id": other.plan.to_string(), "goal": "Ship it", "why": r }),
            true,
        )],
        "crew.handover" => vec![
            (
                json!({ "handover_id": other.handover.to_string(), "note": "Mine now." }),
                true,
            ),
            (
                json!({ "handover_id": uuid::Uuid::new_v4().to_string(), "note": "Mine now." }),
                true,
            ),
        ],
        other => {
            panic!("{other} is a crew tool with no case here: say how it is kept to its scope")
        }
    }
}

/// Points every crew tool `boss` has at `other`'s scope: each one aimed at
/// an id there is refused in `refusal`'s words, before it does anything,
/// and no answer says `secret`.
async fn kept_out(boss: &AgentService, other: &Client, secret: &str, refusal: &str, dir: &Path) {
    for name in READ.iter().chain(ACT.iter()) {
        let tool = boss
            .tools
            .get(name)
            .unwrap_or_else(|| panic!("{} has no {name}", boss.agent_id));
        for (input, refused) in pointed_at(name, other, dir) {
            let r = tool.execute(input.clone()).await.unwrap();
            let said = format!("{:?} {}", r.error, r.output);
            assert!(
                !said.to_lowercase().contains(secret),
                "{} {name} {input} read another scope's: {said}",
                boss.agent_id
            );
            if refused {
                assert!(!r.success, "{} {name} {input} ran: {said}", boss.agent_id);
                // Whom it is handed to is looked at first, then the ticket.
                let by_ticket = input.get("ticket").is_some()
                    && input.pointer("/to/session").is_none()
                    && input.pointer("/to/agent").is_none();
                let words = match by_ticket {
                    true => format!("There's no {}.", other.ticket),
                    false => refusal.to_string(),
                };
                assert_eq!(
                    r.error.as_deref(),
                    Some(words.as_str()),
                    "{} {name} {input}",
                    boss.agent_id
                );
            }
        }
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn each_boss_reads_and_acts_only_inside_its_own_group_through_every_crew_tool() {
    with_test_pool(workforce_desk::store::build_pool, |pool| async move {
        let dir = tempfile::tempdir().unwrap();
        let c = clients(dir.path(), pool).await;
        let suggestions = c.d.store().suggestions(100).await.unwrap().len();

        kept_out(
            &c.sarah,
            &c.atlas,
            ATLAS_SECRET,
            "That isn't one of Acme's.",
            dir.path(),
        )
        .await;
        kept_out(
            &c.mia,
            &c.acme,
            ACME_SECRET,
            "That isn't one of Atlas's.",
            dir.path(),
        )
        .await;
        let main = "That isn't one of yours.";
        kept_out(&c.desk, &c.acme, ACME_SECRET, main, dir.path()).await;
        kept_out(&c.desk, &c.atlas, ATLAS_SECRET, main, dir.path()).await;

        // Nothing it was refused became a suggestion, a message or a move.
        let after = c.d.store().suggestions(100).await.unwrap();
        let outside = [
            c.acme.session.id.to_string(),
            c.atlas.session.id.to_string(),
        ];
        assert!(
            after
                .iter()
                .skip(suggestions)
                .all(|s| s.subject.as_ref().is_none_or(|p| !outside.contains(&p.id))),
            "{after:?}"
        );
        let thread =
            c.d.line_thread(c.atlas.session.id, None, None)
                .await
                .unwrap();
        assert!(thread
            .iter()
            .all(|m| m.from.id != "sarah" && m.from.id != "desk"));
        assert_eq!(
            c.d.ticket_by_key(&c.atlas.ticket)
                .await
                .unwrap()
                .ticket
                .state,
            TicketState::Todo
        );
        assert!(c.d.charge_of(c.atlas.session.id).is_none());

        // The registry: every overseer's tools are bound to its own scope.
        for (boss, scope) in [
            (&c.desk, Scope::Ungrouped),
            (&c.sarah, Scope::Group("acme".into())),
            (&c.mia, Scope::Group("atlas".into())),
        ] {
            assert_eq!(
                c.crew
                    .overseer_named(boss.agent_id.as_str())
                    .unwrap()
                    .1
                    .scope,
                scope
            );
            for name in READ.iter().chain(ACT.iter()) {
                assert!(boss.tools.has(name), "{} {name}", boss.agent_id);
            }
            for name in crate::desk_tools::NAMES {
                assert!(
                    !boss.tools.has(name),
                    "{} keeps {name}, which reads every scope",
                    boss.agent_id
                );
            }
        }
        let bound = super::super::tools::CrewTool::for_level(
            &c.crew,
            "sarah",
            &Scope::Group("acme".into()),
            false,
        );
        assert_eq!(bound.len(), READ.len() + ACT.len());
        assert!(bound
            .iter()
            .all(|t| *t.scope() == Scope::Group("acme".into())));
    })
    .await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_boss_reads_its_group_and_the_main_overseer_the_rest_but_nothing_inside_a_bossed_group() {
    with_test_pool(workforce_desk::store::build_pool, |pool| async move {
        let dir = tempfile::tempdir().unwrap();
        let c = clients(dir.path(), pool).await;
        let read = |boss: &AgentService, name: &str| {
            let tool = boss.tools.get(name).unwrap();
            async move { tool.execute(json!({})).await.unwrap().output.to_string() }
        };

        let acme = read(&c.sarah, "crew.overview").await;
        assert!(
            acme.contains("acme-web") && acme.contains("Scout"),
            "{acme}"
        );
        for outside in ["atlas-web", "infra-web", "loose-web", "Ada", "Mia"] {
            assert!(!acme.contains(outside), "{outside} in Acme's: {acme}");
        }
        let main = read(&c.desk, "crew.overview").await;
        assert!(
            main.contains("infra-web") && main.contains("loose-web"),
            "a group with no boss is the main overseer's: {main}"
        );
        for inside in ["acme-web", "atlas-web", "Scout", "Ada", "Sarah", "Mia"] {
            assert!(
                !main.contains(inside),
                "{inside} in the main overseer's: {main}"
            );
        }

        let board = read(&c.sarah, "crew.board").await;
        assert!(
            board.contains(&c.acme.ticket) && !board.contains(&c.atlas.ticket),
            "{board}"
        );
        assert!(!read(&c.desk, "crew.board").await.contains("ACM-"));

        let activity = eventually("Acme's activity", async || {
            let a = read(&c.sarah, "crew.activity").await;
            a.contains(ACME_SECRET).then_some(a)
        })
        .await;
        assert!(!activity.contains(ATLAS_SECRET));
        assert!(!read(&c.desk, "crew.activity").await.contains(ACME_SECRET));

        // A row keeps the group it was written in: moved to Atlas, Acme's
        // session's history stays Acme's, and what it does next is Atlas's.
        let id = c.acme.session.id;
        c.d.change_layout(|l| l.put(&workforce_desk::session_item(id), "atlas", None))
            .await
            .unwrap();
        let party = Party::session(&c.d.view(id).unwrap());
        c.d.note(Event::new(
            Kind::Started,
            party.clone(),
            Some(party),
            "started “the move test”",
        ));
        let atlas = eventually("Atlas's activity", async || {
            let a = read(&c.mia, "crew.activity").await;
            a.contains("the move test").then_some(a)
        })
        .await;
        assert!(
            !atlas.contains("the acmesecret work"),
            "the old rows stay Acme's: {atlas}"
        );
        let acme = read(&c.sarah, "crew.activity").await;
        assert!(
            acme.contains(ACME_SECRET) && !acme.contains("the move test"),
            "{acme}"
        );
        let _ = (&c.infra, &c.loose);
    })
    .await;
}

#[tokio::test(flavor = "multi_thread")]
async fn what_happens_wakes_the_overseer_whose_scope_it_is_and_each_spends_its_own_check_ins() {
    with_test_pool(workforce_desk::store::build_pool, |pool| async move {
        let dir = tempfile::tempdir().unwrap();
        let c = clients(dir.path(), pool).await;
        for s in [&c.acme.session, &c.infra] {
            let party = Party::session(s);
            c.d.note(Event::new(
                Kind::Finished,
                party.clone(),
                Some(party),
                "finished · Done.",
            ));
        }
        let woke = |id: &'static str, about: &'static str| {
            let d = c.d.clone();
            async move {
                eventually(&format!("{id}'s check-in"), async || {
                    d.store()
                        .latest_check_in(id)
                        .await
                        .unwrap()
                        .map(|c| c.prompt)
                        .filter(|p| p.contains(about))
                })
                .await
            }
        };
        let sarahs = woke("sarah", "acme-web").await;
        assert!(!sarahs.contains("infra-web"), "{sarahs}");
        let mains = woke("desk", "infra-web").await;
        assert!(!mains.contains("acme-web"), "{mains}");
        assert!(
            c.d.store().latest_check_in("mia").await.unwrap().is_none(),
            "nothing happened in Atlas"
        );

        // Each overseer's check-ins are its own: Desk's never spend Sarah's.
        let (_, mut o) = c.crew.overseer_named("sarah").unwrap();
        o.max_checkins_per_hour = 1;
        let mut desk_o = c.crew.overseer_named("desk").unwrap().1;
        desk_o.max_checkins_per_hour = 1;
        let wakes = |id: &'static str| {
            let d = c.d.clone();
            async move {
                d.store()
                    .wake_ups_since(id, chrono::Utc::now() - chrono::Duration::hours(1))
                    .await
                    .unwrap()
            }
        };
        assert!(wakes("sarah").await >= 1 && wakes("desk").await >= 1);
        assert_eq!(wakes("mia").await, 0);
        for _ in 0..3 {
            c.d.store()
                .put_check_in(&workforce_desk::CheckIn {
                    id: workforce_desk::crew::new_id(),
                    at: workforce_desk::crew::now(),
                    agent_id: "desk".into(),
                    kind: "wake".into(),
                    prompt: "more".into(),
                    reply: None,
                })
                .await
                .unwrap();
        }
        let mias = c.crew.overseer_named("mia").unwrap().1;
        assert_eq!(
            c.crew.budget("mia", &mias).await.unwrap(),
            super::super::watcher::Budget::Open
        );
        assert!(matches!(
            c.crew.budget("sarah", &o).await.unwrap(),
            super::super::watcher::Budget::Spent { .. }
        ));
        assert!(matches!(
            c.crew.budget("desk", &desk_o).await.unwrap(),
            super::super::watcher::Budget::Spent { .. }
        ));
    })
    .await;
}

#[tokio::test(flavor = "multi_thread")]
async fn one_overseer_per_scope_and_a_group_it_oversees_must_be_the_sidebars() {
    with_test_pool(workforce_desk::store::build_pool, |pool| async move {
        let dir = tempfile::tempdir().unwrap();
        let c = clients(dir.path(), pool).await;
        let save = |service: &Arc<AgentService>, body: &str| {
            let service = Arc::clone(service);
            let body = body.to_string();
            async move {
                crate::agent_config_write::save(
                    &service,
                    service.agent_id.as_str(),
                    None,
                    &body,
                    workforce_orchestrator::workflow_store::EditContext::new("web", None),
                    |_| true,
                )
                .await
            }
        };
        let refused = save(&c.mia, "[oversight]\nenabled = true\ngroup = \"nowhere\"\n")
            .await
            .unwrap_err();
        assert_eq!(
            refused.to_string(),
            format!(
                "the agent would not load that: {}",
                workforce_common::oversight::NOT_A_GROUP
            )
        );
        assert_eq!(
            c.crew.overseer_named("mia").unwrap().1.scope,
            Scope::Group("atlas".into()),
            "a refused save changes nothing"
        );

        // Mia takes Acme too: Sarah stops, and nobody else does.
        save(
            &c.mia,
            &format!("[oversight]\nenabled = true\ngroup = \"acme\"\n{ACTS}"),
        )
        .await
        .unwrap();
        let sarahs = std::fs::read_to_string(c.sarah.workspace.join("config.toml")).unwrap();
        assert!(sarahs.contains("enabled = false"), "{sarahs}");
        let desks = std::fs::read_to_string(c.desk.workspace.join("config.toml")).unwrap();
        assert!(
            desks.contains("enabled = true"),
            "the main overseer is another scope's"
        );
        let mine: Vec<(String, Scope)> = c
            .crew
            .overseers()
            .into_iter()
            .map(|(s, o)| (s.agent_id.as_str().to_string(), o.scope))
            .collect();
        assert_eq!(
            mine,
            [
                ("desk".to_string(), Scope::Ungrouped),
                ("mia".to_string(), Scope::Group("acme".into())),
            ]
        );
        eventually("Sarah's tools to go", async || {
            (!c.sarah.tools.has("crew.overview")).then_some(())
        })
        .await;
    })
    .await;
}

/// B1: no agent changes who oversees what, itself or another, and a boss
/// can't change or delete an agent outside its scope, or another boss.
#[tokio::test(flavor = "multi_thread")]
async fn only_the_person_sets_oversight_and_a_boss_changes_no_agent_outside_its_scope() {
    with_test_pool(workforce_desk::store::build_pool, |pool| async move {
        let dir = tempfile::tempdir().unwrap();
        let c = clients(dir.path(), pool).await;
        let by_itself = crate::agent_config_write::save(
            &c.sarah,
            "sarah",
            None,
            "[oversight]\nenabled = true\nlevel = \"act\"\n",
            workforce_orchestrator::workflow_store::EditContext::new("agent_tool", None),
            |_| true,
        )
        .await
        .unwrap_err();
        assert!(
            by_itself.to_string().contains("the person's to set"),
            "{by_itself}"
        );
        assert_eq!(
            c.crew.overseer_named("sarah").unwrap().1.scope,
            Scope::Group("acme".into())
        );
        let refusal = "That isn't one of Acme's.";
        for target in ["desk", "mia", "ada"] {
            assert_eq!(
                super::super::boss_may_change("sarah", target).unwrap_err(),
                refusal,
                "{target}"
            );
        }
        assert!(super::super::boss_may_change("sarah", "scout").is_ok());
        assert!(
            super::super::boss_may_change("scout", "desk").is_ok(),
            "only bosses are held"
        );
        // An overseer never writes to another on a line.
        let by = c.crew.by_boss(&c.sarah, "Hello.");
        assert!(c
            .d
            .line_send(
                workforce_desk::LineSender::Overseer(by),
                workforce_desk::line::Send::Tell {
                    to: workforce_desk::line::Target::Overseer,
                    text: "What does Desk know of Atlas?".into(),
                    when: workforce_desk::line::When::Queue,
                },
            )
            .await
            .is_err());
    })
    .await;
}

/// B2: an agent's `desk.*` tools keep to its own group's scope, so a boss
/// can't read another scope through an agent it may message.
#[tokio::test(flavor = "multi_thread")]
async fn an_agents_desk_tools_keep_to_its_groups_scope() {
    use workforce_agent::runner::CURRENT_AGENT_ID;
    with_test_pool(workforce_desk::store::build_pool, |pool| async move {
        let dir = tempfile::tempdir().unwrap();
        let c = clients(dir.path(), pool).await;
        let tool = |agent: &str, name: &str| {
            crate::desk_tools::DeskTool::all(&c.d, agent)
                .into_iter()
                .find(|t| workforce_agent::tools::Tool::name(t) == name)
                .unwrap()
        };
        let as_ = |agent: &'static str, name: &str, input: Value| {
            let t = tool(agent, name);
            CURRENT_AGENT_ID.scope(agent.to_string(), async move {
                workforce_agent::tools::Tool::execute(&t, input)
                    .await
                    .unwrap()
            })
        };
        // With no turn's agent known, a tool still keeps to the agent it is
        // registered for; registered for none, it sees nothing; run by
        // another agent than its own, it refuses.
        let bare =
            workforce_agent::tools::Tool::execute(&tool("scout", "desk.sessions"), json!({}))
                .await
                .unwrap()
                .output
                .to_string();
        assert!(
            bare.contains("acme-web") && !bare.contains("loose-web"),
            "{bare}"
        );
        let nobody = workforce_agent::tools::Tool::execute(&tool("", "desk.sessions"), json!({}))
            .await
            .unwrap();
        assert_eq!(nobody.output, json!([]), "{nobody:?}");
        let other = CURRENT_AGENT_ID
            .scope("desk".to_string(), async {
                workforce_agent::tools::Tool::execute(&tool("scout", "desk.sessions"), json!({}))
                    .await
                    .unwrap()
            })
            .await;
        assert!(!other.success, "{other:?}");
        let listed = as_("scout", "desk.sessions", json!({}))
            .await
            .output
            .to_string();
        assert!(
            listed.contains("acme-web") && !listed.contains("atlas-web"),
            "{listed}"
        );
        assert!(
            !listed.contains("infra-web") && !listed.contains("loose-web"),
            "{listed}"
        );
        let read = as_(
            "scout",
            "desk.read",
            json!({ "session_id": c.atlas.session.id.to_string() }),
        )
        .await;
        assert!(
            !read.success && !format!("{read:?}").contains("atlas"),
            "{read:?}"
        );
        // An agent outside every bossed group sees nothing inside one.
        let listed = as_("free", "desk.sessions", json!({}))
            .await
            .output
            .to_string();
        assert!(
            listed.contains("loose-web") && !listed.contains("acme-web"),
            "{listed}"
        );
    })
    .await;
}

/// B3: a group stays a boundary while its boss's config doesn't load, and
/// when the person takes the boss off, the main overseer sees it from then
/// on and never its past.
#[tokio::test(flavor = "multi_thread")]
async fn a_boss_that_fails_to_load_keeps_its_group_closed_and_taking_it_off_opens_only_whats_new() {
    with_test_pool(workforce_desk::store::build_pool, |pool| async move {
        let dir = tempfile::tempdir().unwrap();
        let c = clients(dir.path(), pool).await;
        write_config(
            &c.mia,
            "[oversight]\nenabled = true\ngroup = \"atlas\"\nlevel = \"boss\"\n",
        );
        super::super::config_saved(&c.mia);
        eventually(
            "Mia to stop overseeing, and the tools to settle",
            async || {
                (c.crew.overseer_named("mia").is_none()
                    && !c.mia.tools.has("crew.overview")
                    && c.desk.tools.has("crew.overview"))
                .then_some(())
            },
        )
        .await;
        let main = |name: &'static str| {
            let t = c.desk.tools.get(name).unwrap();
            async move { t.execute(json!({})).await.unwrap().output.to_string() }
        };
        let overview = main("crew.overview").await;
        assert!(
            !overview.contains("atlas-web"),
            "still Atlas's boundary: {overview}"
        );
        assert!(!main("crew.activity").await.contains(ATLAS_SECRET));

        // The person takes the boss off Atlas.
        crate::agent_config_write::save(
            &c.mia,
            "mia",
            None,
            "[oversight]\nenabled = false\n",
            workforce_orchestrator::workflow_store::EditContext::new("web", None),
            |_| true,
        )
        .await
        .unwrap();
        let id = c.atlas.session.id;
        eventually("Atlas open to the main overseer", async || {
            main("crew.overview")
                .await
                .contains("atlas-web")
                .then_some(())
        })
        .await;
        let party = Party::session(&c.d.view(id).unwrap());
        c.d.note(Event::new(
            Kind::Started,
            party.clone(),
            Some(party),
            "started “the new work”",
        ));
        let activity = eventually("what is new in Atlas", async || {
            let a = main("crew.activity").await;
            a.contains("the new work").then_some(a)
        })
        .await;
        assert!(!activity.contains(ATLAS_SECRET), "not its past: {activity}");
    })
    .await;
}

/// B4: a question gathered for a boss that is no longer the session's is
/// never read by the boss it is gathered for: it goes to the person.
#[tokio::test(flavor = "multi_thread")]
async fn a_question_to_a_boss_its_session_left_goes_to_the_person() {
    with_test_pool(workforce_desk::store::build_pool, |pool| async move {
        let dir = tempfile::tempdir().unwrap();
        let c = clients(dir.path(), pool).await;
        let id = c.atlas.session.id;
        // Atlas's session asks Sarah, as if it had been Acme's.
        let mut ask = LineMessage::new(
            Party::session(&c.atlas.session),
            Party::manager("sarah", "Sarah"),
            LineKind::Ask,
            format!("Which port for {ATLAS_SECRET}?"),
            1,
        );
        ask.asked = Some(Asked::Open);
        ask.group = Some("acme".into());
        c.d.store().put_line(&ask).await.unwrap();
        c.crew.answer_line(id, vec![ask.clone()], vec![]).await;
        let now = c.d.store().line_message(ask.id).await.unwrap().unwrap();
        assert!(
            matches!(now.asked, Some(Asked::Escalated | Asked::Lapsed)),
            "the person's, never a boss's: {:?}",
            now.asked
        );
        let channel = super::super::line::line_channel(id);
        assert!(c.mia.sessions.find(&channel, PERSON).unwrap().is_none());
        assert!(c.sarah.sessions.find(&channel, PERSON).unwrap().is_none());
    })
    .await;
}

/// While a group has a boss, every overseer recalls only its own memories;
/// and the main overseer lets go of what it ran in a group that gets one.
#[tokio::test(flavor = "multi_thread")]
async fn overseers_recall_apart_and_a_group_getting_a_boss_takes_its_sessions_from_the_main_one() {
    with_test_pool(workforce_desk::store::build_pool, |pool| async move {
        let dir = tempfile::tempdir().unwrap();
        let c = clients(dir.path(), pool).await;
        for boss in [&c.desk, &c.sarah, &c.mia] {
            eventually("recall kept apart", async || {
                (boss.resolved.snapshot().memory.agent_recall_scope
                    == workforce_common::settings::AgentRecallScope::Isolated)
                    .then_some(())
            })
            .await;
        }
        let desk_boss = Party::manager("desk", "Desk");
        for _ in 0..200 {
            if c.d.charges_loaded() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        c.d.take_on(
            c.infra.id,
            &desk_boss,
            workforce_desk::charge::How::Handed,
            &Scope::Ungrouped,
            &By::you(),
        )
        .await
        .unwrap();
        // Infra gets a boss: the main overseer lets its session go.
        crate::agent_config_write::save(
            &c.mia,
            "mia",
            None,
            &format!("[oversight]\nenabled = true\ngroup = \"infra\"\n{ACTS}"),
            workforce_orchestrator::workflow_store::EditContext::new("web", None),
            |_| true,
        )
        .await
        .unwrap();
        eventually("the main overseer to let Infra's session go", async || {
            (!c.d.charge_of(c.infra.id).unwrap().live()).then_some(())
        })
        .await;
    })
    .await;
}

/// What a boss starts is its group's from its first word: nothing of its
/// goal or its start reaches another scope, and placing it is no move.
#[tokio::test(flavor = "multi_thread")]
async fn a_session_a_boss_starts_is_its_groups_from_its_first_row() {
    with_test_pool(workforce_desk::store::build_pool, |pool| async move {
        let dir = tempfile::tempdir().unwrap();
        let c = clients(dir.path(), pool).await;
        let r = "For the test.";
        let folder = dir.path().to_string_lossy().to_string();
        let started = c
            .sarah
            .tools
            .get("crew.start_session")
            .unwrap()
            .execute(json!({ "folder": folder, "goal": format!("Check the {ACME_SECRET} copy"), "name": "acme-fix", "reason": r }))
            .await
            .unwrap();
        assert!(started.output["done"].is_string(), "{started:?}");
        let handed = c
            .sarah
            .tools
            .get("crew.assign_ticket")
            .unwrap()
            .execute(json!({ "ticket": c.acme.ticket, "to": { "newSession": { "folder": folder } }, "reason": r }))
            .await
            .unwrap();
        assert!(handed.success, "{handed:?}");
        let mine: Vec<_> = c
            .d
            .list()
            .into_iter()
            .filter(|v| c.d.charge_of(v.id).is_some_and(|ch| ch.boss == "sarah"))
            .collect();
        assert_eq!(mine.len(), 2, "{mine:?}");
        for v in &mine {
            assert_eq!(v.group.as_ref().map(|g| g.id.as_str()), Some("acme"));
            assert!(c.d.charge_of(v.id).unwrap().live(), "placing it is no move");
        }
        // Let what was noted land, then look from every other scope.
        eventually("Acme's start in its activity", async || {
            let a = c.sarah.tools.get("crew.activity").unwrap().execute(json!({})).await.unwrap();
            a.output.to_string().contains("acme-fix").then_some(())
        })
        .await;
        for other in [&c.desk, &c.mia] {
            for name in ["crew.activity", "crew.overview", "crew.board", "crew.spend"] {
                let said = other.tools.get(name).unwrap().execute(json!({})).await.unwrap();
                let said = format!("{:?} {}", said.error, said.output);
                assert!(
                    !said.contains(ACME_SECRET) && !said.contains("acme-fix"),
                    "{} {name}: {said}",
                    other.agent_id
                );
            }
        }
    })
    .await;
}

/// Saves `body` as `service`'s config, as the person does from Desk.
async fn person_saves(
    service: &Arc<AgentService>,
    body: &str,
) -> Result<crate::agent_config_write::Saved, crate::agent_config_write::Refused> {
    crate::agent_config_write::save(
        service,
        service.agent_id.as_str(),
        None,
        body,
        workforce_orchestrator::workflow_store::EditContext::new("web", None),
        |_| true,
    )
    .await
}

/// What the main overseer's overview says, now.
async fn main_overview(c: &Clients) -> String {
    let t = c.desk.tools.get("crew.overview").unwrap();
    t.execute(json!({})).await.unwrap().output.to_string()
}

/// A boss replaced names its group no more, so when the new boss is taken
/// off, the group opens to the main overseer: no file is left naming it.
#[tokio::test(flavor = "multi_thread")]
async fn a_boss_replaced_and_then_its_successor_taken_off_leaves_the_group_open() {
    with_test_pool(workforce_desk::store::build_pool, |pool| async move {
        let dir = tempfile::tempdir().unwrap();
        let c = clients(dir.path(), pool).await;
        // Mia replaces Sarah over Acme.
        person_saves(
            &c.mia,
            &format!("[oversight]\nenabled = true\ngroup = \"acme\"\n{ACTS}"),
        )
        .await
        .unwrap();
        let sarahs = std::fs::read_to_string(c.sarah.workspace.join("config.toml")).unwrap();
        assert!(sarahs.contains("enabled = false"), "{sarahs}");
        assert_eq!(super::super::group_named_in(&sarahs), None, "{sarahs}");
        assert!(!main_overview(&c).await.contains("acme-web"));

        // The person takes Mia off: Acme is the main overseer's from now on.
        person_saves(&c.mia, &format!("[oversight]\nenabled = false\n{ACTS}"))
            .await
            .unwrap();
        eventually("Acme open to the main overseer", async || {
            main_overview(&c).await.contains("acme-web").then_some(())
        })
        .await;
        assert!(!c.d.bossed().contains("acme"));
        assert!(c.d.sealed().is_empty(), "{:?}", c.d.sealed());
    })
    .await;
}

/// A group getting a boss is closed before its config is written, and
/// opened again, past and all, when the write doesn't happen.
#[tokio::test(flavor = "multi_thread")]
async fn a_group_is_closed_before_its_boss_is_written_and_reopened_if_it_isnt() {
    with_test_pool(workforce_desk::store::build_pool, |pool| async move {
        let dir = tempfile::tempdir().unwrap();
        let c = clients(dir.path(), pool).await;
        assert!(!c.d.bossed().contains("infra"));
        let closed_while_writing = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let seen = Arc::clone(&closed_while_writing);
        let d = c.d.clone();
        // The file changed meanwhile: the write is refused.
        let refused = crate::agent_config_write::save(
            &c.mia,
            "mia",
            None,
            &format!("[oversight]\nenabled = true\ngroup = \"infra\"\n{ACTS}"),
            workforce_orchestrator::workflow_store::EditContext::new("web", None),
            move |_| {
                seen.store(
                    d.bossed().contains("infra"),
                    std::sync::atomic::Ordering::SeqCst,
                );
                false
            },
        )
        .await;
        assert!(refused.is_err());
        assert!(
            closed_while_writing.load(std::sync::atomic::Ordering::SeqCst),
            "closed before the write"
        );
        assert!(!c.d.bossed().contains("infra"), "opened again");
        assert!(
            main_overview(&c).await.contains("infra-web"),
            "past and all"
        );

        // Written, it stays closed.
        person_saves(
            &c.mia,
            &format!("[oversight]\nenabled = true\ngroup = \"infra\"\n{ACTS}"),
        )
        .await
        .unwrap();
        assert!(c.d.bossed().contains("infra"));
    })
    .await;
}

/// Deleting a group takes its boss off first, through the crew: the boss
/// names the group no more, and nothing is left closed.
#[tokio::test(flavor = "multi_thread")]
async fn a_group_deleted_takes_its_boss_off_first() {
    with_test_pool(workforce_desk::store::build_pool, |pool| async move {
        let dir = tempfile::tempdir().unwrap();
        let c = clients(dir.path(), pool).await;
        c.d.release_group("atlas").await.unwrap();
        let mias = std::fs::read_to_string(c.mia.workspace.join("config.toml")).unwrap();
        assert!(mias.contains("enabled = false"), "{mias}");
        assert_eq!(super::super::group_named_in(&mias), None, "{mias}");
        let sarahs = std::fs::read_to_string(c.sarah.workspace.join("config.toml")).unwrap();
        assert!(
            sarahs.contains("group = \"acme\""),
            "another group's boss stays"
        );
        c.d.change_layout(|l| l.delete("atlas")).await.unwrap();
        assert!(!c.d.bossed().contains("atlas"));
        eventually("Mia to stop overseeing", async || {
            c.crew.overseer_named("mia").is_none().then_some(())
        })
        .await;
    })
    .await;
}

/// A group named for a boss whose config is switched off (by hand, so it
/// still names it) is sealed: closed to everyone but the person.
#[tokio::test(flavor = "multi_thread")]
async fn a_group_whose_boss_is_off_but_still_named_is_sealed() {
    with_test_pool(workforce_desk::store::build_pool, |pool| async move {
        let dir = tempfile::tempdir().unwrap();
        let c = clients(dir.path(), pool).await;
        assert!(c.d.sealed().is_empty());
        write_config(
            &c.mia,
            &format!("[oversight]\nenabled = false\ngroup = \"atlas\"\n{ACTS}"),
        );
        super::super::config_saved(&c.mia);
        eventually("Atlas sealed", async || {
            (c.d.sealed() == ["atlas".to_string()]).then_some(())
        })
        .await;
        // Sarah takes Atlas on: the stale name goes with it, so taking her
        // off leaves Atlas open, not sealed.
        person_saves(
            &c.sarah,
            &format!("[oversight]\nenabled = true\ngroup = \"atlas\"\n{ACTS}"),
        )
        .await
        .unwrap();
        let mias = std::fs::read_to_string(c.mia.workspace.join("config.toml")).unwrap();
        assert_eq!(super::super::group_named_in(&mias), None, "{mias}");
        assert!(c.d.sealed().is_empty());
        person_saves(&c.sarah, &format!("[oversight]\nenabled = false\n{ACTS}"))
            .await
            .unwrap();
        eventually("Atlas open to the main overseer", async || {
            main_overview(&c).await.contains("atlas-web").then_some(())
        })
        .await;
        assert!(c.d.sealed().is_empty());
    })
    .await;
}

/// A workspace beside the served agents', of an agent the hub doesn't serve
/// now, its config naming `group` for a boss, switched off.
fn ghost(c: &Clients, group: &str) -> std::path::PathBuf {
    let ws = c.mia.workspace.parent().unwrap().join("ghost");
    std::fs::create_dir_all(&ws).unwrap();
    std::fs::write(
        ws.join("config.toml"),
        format!("[oversight]\nenabled = false\ngroup = \"{group}\"\n"),
    )
    .unwrap();
    ws
}

fn names(ws: &std::path::Path) -> Option<String> {
    super::super::group_named_in(&std::fs::read_to_string(ws.join("config.toml")).unwrap())
}

/// An agent the hub doesn't serve can't keep a group sealed: a new boss of
/// the group takes its name off that agent's config too, so taking the new
/// boss off leaves the group open.
#[tokio::test(flavor = "multi_thread")]
async fn a_new_boss_takes_the_group_off_an_unserved_agents_config_too() {
    with_test_pool(workforce_desk::store::build_pool, |pool| async move {
        let dir = tempfile::tempdir().unwrap();
        let c = clients(dir.path(), pool).await;
        let ws = ghost(&c, "infra");
        super::super::config_saved(&c.mia);
        eventually("Infra sealed by the unserved agent", async || {
            (c.d.sealed() == ["infra".to_string()]).then_some(())
        })
        .await;
        person_saves(
            &c.mia,
            &format!("[oversight]\nenabled = true\ngroup = \"infra\"\n{ACTS}"),
        )
        .await
        .unwrap();
        assert_eq!(names(&ws), None);
        person_saves(&c.mia, &format!("[oversight]\nenabled = false\n{ACTS}"))
            .await
            .unwrap();
        eventually("Infra open to the main overseer", async || {
            main_overview(&c).await.contains("infra-web").then_some(())
        })
        .await;
        assert!(c.d.sealed().is_empty(), "{:?}", c.d.sealed());
    })
    .await;
}

/// Deleting a group takes its name off an unserved agent's config too, and
/// opens it, even with no served boss.
#[tokio::test(flavor = "multi_thread")]
async fn releasing_a_group_takes_it_off_an_unserved_agents_config() {
    with_test_pool(workforce_desk::store::build_pool, |pool| async move {
        let dir = tempfile::tempdir().unwrap();
        let c = clients(dir.path(), pool).await;
        let ws = ghost(&c, "infra");
        super::super::config_saved(&c.mia);
        eventually("Infra sealed", async || {
            c.d.bossed().contains("infra").then_some(())
        })
        .await;
        c.d.release_group("infra").await.unwrap();
        assert_eq!(names(&ws), None);
        assert!(!c.d.bossed().contains("infra"));
        assert!(c.d.sealed().is_empty());
    })
    .await;
}

/// Only agents' folders beside the served ones are walked, and each is known
/// by its folder's name, as the served agents' folders are named after
/// their ids.
#[tokio::test(flavor = "multi_thread")]
async fn unserved_folders_are_agents_only_and_known_by_their_ids() {
    with_test_pool(workforce_desk::store::build_pool, |pool| async move {
        let dir = tempfile::tempdir().unwrap();
        let c = clients(dir.path(), pool).await;
        let ws = ghost(&c, "infra");
        let beside = ws.parent().unwrap();
        std::fs::create_dir_all(beside.join("notes")).unwrap();
        std::fs::write(beside.join("notes").join("config.toml"), "not [ toml").unwrap();
        std::fs::create_dir_all(beside.join("empty")).unwrap();
        let services = c.crew.state.registry.all_services();
        for s in &services {
            if s.workspace.parent() == Some(beside) {
                assert_eq!(
                    s.workspace.file_name().unwrap().to_string_lossy(),
                    s.agent_id.as_str(),
                    "a served agent's folder is named after its id"
                );
            }
        }
        let found = crate::agent_config_write::unserved_beside(&services);
        assert_eq!(
            found,
            [crate::agent_config_write::Unserved {
                workspace: ws.clone(),
                id: Some("ghost".into()),
            }]
        );
    })
    .await;
}

/// A group whose unserved agent's config can't be switched off isn't
/// deleted, and says which agent's settings to fix.
#[tokio::test(flavor = "multi_thread")]
async fn a_release_refused_names_the_agent_whose_settings_to_fix() {
    with_test_pool(workforce_desk::store::build_pool, |pool| async move {
        let dir = tempfile::tempdir().unwrap();
        let c = clients(dir.path(), pool).await;
        let ws = ghost(&c, "infra");
        let path = ws.join("config.toml");
        workforce_common::config_files::guard(&path).unwrap();
        std::fs::write(
            &path,
            "[oversight]\nenabled = false\ngroup = \"infra\"\n# by hand\n",
        )
        .unwrap();
        let why = c.d.release_group("infra").await.unwrap_err();
        assert!(
            why.starts_with("ghost still names this group for its boss: "),
            "{why}"
        );
        assert_eq!(names(&ws).as_deref(), Some("infra"));
    })
    .await;
}
