use std::collections::BTreeMap;

use serde::Deserialize;
use serde_json::Value;

use super::*;
use crate::engine::{Via, View};

fn u(n: u128) -> Uuid {
    Uuid::from_u128(0x0000_0000_0000_4000_8000_0000_0000_0000 | n)
}

fn plan(state: PlanState) -> Plan {
    let now = Utc::now();
    Plan {
        id: u(900),
        board: u(901),
        group: Some("g-acme".into()),
        boss: "sarah".into(),
        goal: "Ship the new checkout".into(),
        state,
        paused: None,
        paused_why: None,
        parallel: None,
        revision: 1,
        approved_revision: match state {
            PlanState::Proposed => None,
            _ => Some(1),
        },
        created_at: now,
        updated_at: now,
    }
}

fn ticket(n: u128, state: TicketState, needs: &[u128]) -> PlanTicket {
    PlanTicket {
        repo: None,
        id: u(n),
        key: Some(format!("ACM-{n}")),
        state,
        rank: n as f64 * 1024.0,
        needs: needs.iter().map(|&x| u(x)).collect(),
        engine: Some(Engine::Claude),
        folder: Some("/work/acme-web".into()),
        view: None,
        machine: None,
        model: None,
        assigned: false,
        session: None,
        starting: false,
        held_back: None,
    }
}

fn planned(n: u128, title: &str, needs: &[u128]) -> Planned {
    Planned {
        repo: None,
        id: u(n),
        title: title.into(),
        body: String::new(),
        engine: Engine::Claude,
        folder: "/work/acme-web".into(),
        view: None,
        needs: needs.iter().map(|&x| u(x)).collect(),
        machine: None,
        model: None,
        adopted: false,
    }
}

fn engines() -> Vec<EngineView> {
    let view = |engine: Engine, installed: bool, sign_in: SignIn| EngineView {
        engine,
        title: engine.title(),
        program: Some(engine.as_str().into()),
        installed: Some(installed),
        version: None,
        sign_in,
        views: engine::views(engine).to_vec(),
        caps: engine::views(engine)
            .iter()
            .map(|&v| (v, engine::caps(engine, v)))
            .collect::<BTreeMap<_, _>>(),
    };
    let signed_in = || SignIn::In {
        via: Via::Plan,
        who: None,
        plan: None,
        ids: vec![],
    };
    vec![
        view(Engine::Claude, true, signed_in()),
        view(Engine::Codex, true, SignIn::Out),
        view(Engine::Gemini, true, signed_in()),
        view(Engine::Copilot, false, SignIn::Unknown),
    ]
}

fn fits(p: &str, _: Option<&str>) -> Result<(), String> {
    p.starts_with("/work/")
        .then_some(())
        .ok_or_else(|| format!("{p} isn't a folder"))
}

fn draft_ticket(title: &str, needs: &[usize]) -> DraftTicket {
    DraftTicket {
        repo: None,
        id: None,
        adopt: None,
        title: title.into(),
        body: String::new(),
        engine: None,
        folder: "/work/acme-web".into(),
        view: None,
        needs: needs.to_vec(),
        machine: None,
        model: None,
    }
}

fn counter() -> impl FnMut() -> Uuid {
    let mut n = 0;
    move || {
        n += 1;
        u(n)
    }
}

// ── Fixtures shared with Desk ───────────────────────────────────────────

#[derive(Deserialize)]
struct Cases<T> {
    cases: Vec<T>,
}

#[derive(Deserialize)]
struct WavesCase {
    name: String,
    tickets: Vec<PlanTicket>,
    waves: Vec<Vec<Uuid>>,
}

#[derive(Deserialize)]
struct ProgressCase {
    name: String,
    tickets: Vec<PlanTicket>,
    progress: Progress,
}

#[test]
fn waves_agree_with_the_shared_fixtures() {
    let fixtures: Cases<WavesCase> =
        serde_json::from_str(include_str!("../fixtures/plan/waves.json")).unwrap();
    assert!(fixtures.cases.len() >= 5);
    for c in fixtures.cases {
        assert_eq!(waves(&c.tickets), c.waves, "{}", c.name);
    }
}

#[test]
fn progress_agrees_with_the_shared_fixtures() {
    let fixtures: Cases<ProgressCase> =
        serde_json::from_str(include_str!("../fixtures/plan/progress.json")).unwrap();
    assert!(fixtures.cases.len() >= 5);
    for c in fixtures.cases {
        assert_eq!(progress(&c.tickets), c.progress, "{}", c.name);
    }
}

#[test]
fn progress_reads_and_writes_the_shape_desk_reads() {
    let p = progress(&[ticket(1, TicketState::Todo, &[])]);
    let v = serde_json::to_value(&p).unwrap();
    for field in [
        "done", "doing", "review", "ready", "blocked", "waiting", "total", "next", "holding",
    ] {
        assert!(v.get(field).is_some(), "{field} in {v}");
    }
    assert_eq!(v["next"], serde_json::json!(["ACM-1"]));
}

// ── Drafts and validation ───────────────────────────────────────────────

#[test]
fn a_draft_names_its_needs_by_place_and_gets_ids_and_the_default_engine() {
    let mut second = draft_ticket("Wire the payment form", &[0]);
    second.engine = Some("Codex".into());
    second.view = Some("terminal".into());
    let draft = Draft {
        goal: "  Ship the new checkout ".into(),
        tickets: vec![draft_ticket("Add the cart API", &[]), second],
    };
    let s = snapshot_of(&draft, None, Engine::Claude, &mut counter()).unwrap();
    assert_eq!(s.goal, "Ship the new checkout");
    assert_eq!(s.tickets[0].id, u(1));
    assert_eq!(s.tickets[0].engine, Engine::Claude, "the boss's default");
    assert_eq!(s.tickets[1].engine, Engine::Codex);
    assert_eq!(s.tickets[1].view, Some(View::Terminal));
    assert_eq!(s.tickets[1].needs, vec![u(1)]);
}

#[test]
fn a_draft_keeps_the_ids_of_the_tickets_it_changes_and_refuses_others() {
    let before = Snapshot {
        parallel: None,
        budget: None,
        goal: "Ship".into(),
        tickets: vec![planned(7, "Add the cart API", &[])],
    };
    let mut kept = draft_ticket("Add the cart API", &[]);
    kept.id = Some(u(7));
    let draft = Draft {
        goal: "Ship".into(),
        tickets: vec![kept.clone(), draft_ticket("New one", &[0])],
    };
    let s = snapshot_of(&draft, Some(&before), Engine::Claude, &mut counter()).unwrap();
    assert_eq!(s.tickets[0].id, u(7));
    assert_eq!(s.tickets[1].needs, vec![u(7)]);

    let mut stranger = kept.clone();
    stranger.id = Some(u(8));
    let bad = Draft {
        goal: "Ship".into(),
        tickets: vec![kept.clone(), kept, stranger],
    };
    let problems = snapshot_of(&bad, Some(&before), Engine::Claude, &mut counter()).unwrap_err();
    assert!(
        problems.contains(&Problem::Twice { ticket: 2 }),
        "{problems:?}"
    );
    assert!(problems.contains(&Problem::UnknownTicket { ticket: 3 }));
    // Without a plan to change, no ticket can be one of it.
    let mut fresh = draft_ticket("x", &[]);
    fresh.id = Some(u(7));
    let none = Draft {
        goal: "Ship".into(),
        tickets: vec![fresh],
    };
    assert_eq!(
        snapshot_of(&none, None, Engine::Claude, &mut counter()).unwrap_err(),
        vec![Problem::UnknownTicket { ticket: 1 }]
    );
}

#[test]
fn a_draft_refuses_a_subagent_an_unknown_engine_and_bad_needs() {
    let mut sub = draft_ticket("Look it up", &[]);
    sub.engine = Some("subagent".into());
    let mut odd = draft_ticket("Odd", &[]);
    odd.engine = Some("emacs".into());
    let mut view = draft_ticket("In a pane", &[]);
    view.view = Some("window".into());
    let draft = Draft {
        goal: "g".into(),
        tickets: vec![
            sub,
            odd,
            view,
            draft_ticket("Self", &[3]),
            draft_ticket("Far", &[9]),
        ],
    };
    let problems = snapshot_of(&draft, None, Engine::Claude, &mut counter()).unwrap_err();
    assert_eq!(
        problems,
        vec![
            Problem::Subagent { ticket: 1 },
            Problem::UnknownEngine {
                ticket: 2,
                engine: "emacs".into()
            },
            Problem::UnknownView {
                ticket: 3,
                view: "window".into()
            },
            Problem::NeedsItself { ticket: 4 },
            Problem::UnknownNeed {
                ticket: 5,
                need: 10
            },
        ]
    );
    assert!(plain(&problems).starts_with("Ticket 1 asks for a sub-agent"));
}

#[test]
fn a_ticket_says_its_machine_and_model_and_keeps_them_unless_changed() {
    let mut there = planned(7, "Add the cart API", &[]);
    there.machine = Some("devbox".into());
    there.model = Some("gpt-5.5".into());
    let before = Snapshot {
        parallel: None,
        budget: None,
        goal: "Ship".into(),
        tickets: vec![there, planned(8, "Wire it", &[])],
    };
    let mut kept = draft_ticket("Add the cart API", &[]);
    kept.id = Some(u(7));
    let mut moved = draft_ticket("Wire it", &[]);
    moved.id = Some(u(8));
    moved.machine = Some(" devbox ".into());
    moved.model = Some("claude-opus-4-8[1m]".into());
    let mut home = kept.clone();
    home.machine = Some("local".into());
    home.model = Some(String::new());
    let draft = |a: DraftTicket, b: DraftTicket| Draft {
        goal: "Ship".into(),
        tickets: vec![a, b],
    };
    let s = snapshot_of(
        &draft(kept, moved.clone()),
        Some(&before),
        Engine::Claude,
        &mut counter(),
    )
    .unwrap();
    assert_eq!(
        s.tickets[0].machine.as_deref(),
        Some("devbox"),
        "kept when not said"
    );
    assert_eq!(s.tickets[0].model.as_deref(), Some("gpt-5.5"));
    assert_eq!(s.tickets[1].machine.as_deref(), Some("devbox"));
    assert_eq!(s.tickets[1].model.as_deref(), Some("claude-opus-4-8[1m]"));
    let s = snapshot_of(
        &draft(home, moved.clone()),
        Some(&before),
        Engine::Claude,
        &mut counter(),
    )
    .unwrap();
    assert_eq!(s.tickets[0].machine, None, "local is this computer");
    assert_eq!(s.tickets[0].model, None, "blank is the engine's own");

    let mut bad = moved.clone();
    bad.machine = Some("../etc".into());
    bad.model = Some("-rf; rm".into());
    let problems = snapshot_of(
        &draft(bad, moved),
        Some(&before),
        Engine::Claude,
        &mut counter(),
    )
    .unwrap_err();
    assert!(
        problems.contains(&Problem::BadMachine {
            ticket: 1,
            machine: "../etc".into()
        }),
        "{problems:?}"
    );
    assert!(
        problems.contains(&Problem::BadModel {
            ticket: 1,
            model: "-rf; rm".into()
        }),
        "{problems:?}"
    );
}

/// A model is its engine's: a ticket moved to another engine without a
/// model of its own takes that engine's default, and a model its engine
/// wouldn't take is refused when the plan is made.
#[test]
fn a_tickets_model_goes_with_its_engine() {
    let mut codex = planned(7, "Add the cart API", &[]);
    codex.engine = Engine::Codex;
    codex.model = Some("gpt-6.1-sol".into());
    let before = Snapshot {
        parallel: None,
        budget: None,
        goal: "Ship".into(),
        tickets: vec![codex],
    };
    let mut same = draft_ticket("Add the cart API", &[]);
    same.id = Some(u(7));
    let draft = |t: DraftTicket| Draft {
        goal: "Ship".into(),
        tickets: vec![t],
    };
    let s = snapshot_of(
        &draft(same.clone()),
        Some(&before),
        Engine::Codex,
        &mut counter(),
    )
    .unwrap();
    assert_eq!(
        s.tickets[0].model.as_deref(),
        Some("gpt-6.1-sol"),
        "kept on Codex"
    );
    let s = snapshot_of(
        &draft(same.clone()),
        Some(&before),
        Engine::Claude,
        &mut counter(),
    )
    .unwrap();
    assert_eq!(s.tickets[0].engine, Engine::Claude);
    assert_eq!(
        s.tickets[0].model, None,
        "Claude Code's own, not Codex's model"
    );
    let mut moved = same.clone();
    moved.engine = Some("devin".into());
    let s = snapshot_of(
        &draft(moved.clone()),
        Some(&before),
        Engine::Codex,
        &mut counter(),
    )
    .unwrap();
    assert_eq!(s.tickets[0].model, None);

    moved.model = Some("sonnet[1m]".into());
    let problems =
        snapshot_of(&draft(moved), Some(&before), Engine::Codex, &mut counter()).unwrap_err();
    assert!(
        problems.contains(&Problem::BadModel {
            ticket: 1,
            model: "sonnet[1m]".into()
        }),
        "{problems:?}"
    );
}

#[test]
fn a_machine_ticket_is_checked_there_not_on_this_computer() {
    // Its folder isn't on this computer's disk, and its engine needn't be
    // installed here: the machine says so when it starts.
    let mut there = planned(1, "On devbox", &[]);
    there.machine = Some("devbox".into());
    there.folder = "/home/dev/vellev".into();
    there.engine = Engine::Codex;
    let mut relative = planned(2, "Relative there", &[]);
    relative.machine = Some("devbox".into());
    relative.folder = "vellev".into();
    let mut pane = planned(3, "No way to start there", &[]);
    pane.machine = Some("devbox".into());
    pane.engine = Engine::Cursor;
    let s = Snapshot {
        parallel: None,
        budget: None,
        goal: "Ship".into(),
        tickets: vec![there, relative, pane],
    };
    let problems = validate(&s, &engines(), &|_, _| Err(String::new())).unwrap_err();
    assert!(
        !problems
            .iter()
            .any(|p| format!("{p:?}").contains("ticket: 1")),
        "{problems:?}"
    );
    assert!(
        problems.contains(&Problem::NoMachineFolder {
            ticket: 2,
            folder: "vellev".into(),
            machine: "devbox".into()
        }),
        "{problems:?}"
    );
    assert!(
        problems
            .iter()
            .any(|p| format!("{p:?}").contains("ticket: 3")),
        "{problems:?}"
    );
}

#[test]
fn a_plan_that_can_run_here_is_valid() {
    let s = Snapshot {
        parallel: None,
        budget: None,
        goal: "Ship the new checkout".into(),
        tickets: vec![
            planned(1, "Add the cart API", &[]),
            planned(2, "Wire it", &[1]),
        ],
    };
    assert_eq!(validate(&s, &engines(), &fits), Ok(()));
}

#[test]
fn each_thing_that_keeps_a_plan_from_running_is_said() {
    let mut codex = planned(2, "Signed out", &[]);
    codex.engine = Engine::Codex;
    let mut gemini = planned(3, "No way to start", &[]);
    gemini.engine = Engine::Gemini;
    let mut copilot = planned(4, "Not here", &[]);
    copilot.engine = Engine::Copilot;
    let mut cursor = planned(5, "Unknown to Desk", &[]);
    cursor.engine = Engine::Cursor;
    let mut pane = planned(6, "Desk view", &[]);
    pane.view = Some(View::Desk);
    let mut nowhere = planned(7, "Nowhere", &[]);
    nowhere.folder = "/tmp/missing".into();
    let mut relative = planned(8, "Relative", &[]);
    relative.folder = "work/acme".into();
    let mut untitled = planned(9, "  ", &[]);
    untitled.body = "x".repeat(BODY_MAX + 1);
    let outside = planned(10, "Outside", &[77]);
    let s = Snapshot {
        parallel: None,
        budget: None,
        goal: String::new(),
        tickets: vec![
            planned(1, "Fine", &[]),
            codex,
            gemini,
            copilot,
            cursor,
            pane,
            nowhere,
            relative,
            untitled,
            outside,
        ],
    };
    let problems = validate(&s, &engines(), &fits).unwrap_err();
    let expect = [
        Problem::NoGoal,
        Problem::SignedOut {
            ticket: 2,
            engine: "Codex",
        },
        Problem::CantStart {
            ticket: 3,
            engine: "Gemini CLI",
        },
        Problem::NotInstalled {
            ticket: 4,
            engine: Engine::Copilot.title(),
        },
        Problem::NotInstalled {
            ticket: 5,
            engine: Engine::Cursor.title(),
        },
        Problem::CantStart {
            ticket: 6,
            engine: "Claude Code",
        },
        Problem::NoFolder {
            ticket: 7,
            folder: "/tmp/missing".into(),
        },
        Problem::NoFolder {
            ticket: 8,
            folder: "work/acme".into(),
        },
        Problem::NoTitle { ticket: 9 },
        Problem::LongBody {
            ticket: 9,
            len: BODY_MAX + 1,
        },
        Problem::NeedsOutside { ticket: 10 },
    ];
    for p in &expect {
        assert!(problems.contains(p), "{p:?} missing from {problems:?}");
    }
    assert_eq!(problems.len(), expect.len(), "{problems:?}");
}

#[test]
fn a_cycle_is_refused_naming_the_tickets_in_it() {
    let s = Snapshot {
        parallel: None,
        budget: None,
        goal: "g".into(),
        tickets: vec![
            planned(1, "a", &[]),
            planned(2, "b", &[3]),
            planned(3, "c", &[4]),
            planned(4, "d", &[2]),
            planned(5, "e", &[2]),
        ],
    };
    let problems = validate(&s, &engines(), &fits).unwrap_err();
    assert_eq!(problems, vec![Problem::Cycle("2, 3, 4, 5".into())]);
    assert!(problems[0].to_string().contains("in a loop"));
}

#[test]
fn a_plan_has_one_to_thirty_tickets_and_a_short_enough_goal() {
    let none = Snapshot {
        parallel: None,
        budget: None,
        goal: "g".into(),
        tickets: vec![],
    };
    assert_eq!(
        validate(&none, &engines(), &fits),
        Err(vec![Problem::NoTickets])
    );
    let many = Snapshot {
        parallel: None,
        budget: None,
        goal: "x".repeat(GOAL_MAX + 1),
        tickets: (1..=31).map(|n| planned(n, "t", &[])).collect(),
    };
    assert_eq!(
        validate(&many, &engines(), &fits),
        Err(vec![Problem::LongGoal(GOAL_MAX + 1), Problem::TooMany(31)])
    );
}

// ── Starts ──────────────────────────────────────────────────────────────

#[test]
fn an_approved_plan_starts_its_ready_tickets_up_to_the_limit_in_rank_order() {
    let p = plan(PlanState::Running);
    let mut tickets: Vec<PlanTicket> = (1..=5).map(|n| ticket(n, TicketState::Todo, &[])).collect();
    tickets[0].rank = 9_999.0;
    let starts = next_starts(&p, &tickets, room(&p, 0, 0, 4), None);
    let order: Vec<Uuid> = starts.iter().map(|s| s.ticket).collect();
    assert_eq!(order, vec![u(2), u(3), u(4), u(5)]);
    // One busy already: three.
    assert_eq!(next_starts(&p, &tickets, room(&p, 1, 0, 4), None).len(), 3);
    // The plan's own limit is lower.
    let mut low = p.clone();
    low.parallel = Some(2);
    assert_eq!(
        next_starts(&low, &tickets, room(&low, 0, 0, 4), None).len(),
        2
    );
    // Never above the boss's.
    low.parallel = Some(9);
    assert_eq!(
        next_starts(&low, &tickets, room(&low, 0, 0, 4), None).len(),
        4
    );
}

#[test]
fn a_ticket_held_back_never_starts_by_itself_and_waits_on_someone() {
    let p = plan(PlanState::Running);
    let mut held = ticket(1, TicketState::Todo, &[]);
    held.held_back = Some("its session ended as it started (exit 1)".into());
    let tickets = vec![held, ticket(2, TicketState::Todo, &[])];
    let order: Vec<Uuid> = next_starts(&p, &tickets, room(&p, 0, 0, 4), None)
        .iter()
        .map(|s| s.ticket)
        .collect();
    assert_eq!(order, vec![u(2)]);
    let progress = super::progress(&tickets);
    assert_eq!((progress.ready, progress.blocked), (1, 1));
    assert_eq!(progress.next, vec!["ACM-2".to_string()]);
}

#[test]
fn a_ticket_sharing_a_folder_with_work_under_way_gets_a_worktree() {
    let p = plan(PlanState::Running);
    let mut doing = ticket(1, TicketState::Doing, &[]);
    doing.assigned = true;
    let mut elsewhere = ticket(3, TicketState::Todo, &[]);
    elsewhere.folder = Some("/work/acme-api".into());
    let tickets = vec![
        doing,
        ticket(2, TicketState::Todo, &[]),
        elsewhere,
        ticket(4, TicketState::Todo, &[]),
    ];
    let starts = next_starts(&p, &tickets, room(&p, 1, 0, 4), None);
    let wt: Vec<(Uuid, bool)> = starts.iter().map(|s| (s.ticket, s.worktree)).collect();
    assert_eq!(wt, vec![(u(2), true), (u(3), false), (u(4), true)]);
    // Alone in its folder, the first doesn't.
    let alone = next_starts(
        &p,
        &[ticket(5, TicketState::Todo, &[])],
        room(&p, 0, 0, 4),
        None,
    );
    assert!(!alone[0].worktree);
}

/// A small, seeded generator, so every run tries the same plans.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }

    fn below(&mut self, n: u64) -> u64 {
        self.next() % n
    }
}

#[test]
fn next_starts_never_exceeds_the_limit_never_starts_an_unready_ticket_and_starts_none_while_held() {
    let states = [
        TicketState::Backlog,
        TicketState::Todo,
        TicketState::Doing,
        TicketState::Review,
        TicketState::Done,
        TicketState::Dropped,
    ];
    let mut rng = Rng(0x5eed_1234_abcd_0042);
    for case in 0..5_000 {
        let n = 1 + rng.below(12) as u128;
        let tickets: Vec<PlanTicket> = (1..=n)
            .map(|i| {
                let needs: Vec<u128> = (1..i).filter(|_| rng.below(4) == 0).collect();
                let mut t = ticket(i, states[rng.below(6) as usize], &needs);
                t.rank = rng.below(1000) as f64;
                t.assigned = rng.below(5) == 0;
                t.starting = rng.below(7) == 0;
                t
            })
            .collect();
        let held = rng.below(6) as usize;
        let busy = rng.below(8) as usize;
        let limit = 1 + rng.below(12) as usize;
        let mut p = plan(PlanState::Running);
        p.parallel = (rng.below(3) == 0).then(|| rng.below(6) as i16);
        let cap = p.limit(limit);
        let free = room(&p, held, busy, limit);
        let starts = next_starts(&p, &tickets, free, None);
        assert!(
            held + starts.len() <= cap.max(held) && busy + starts.len() <= limit.max(busy),
            "case {case}: {} starts with {held} held of {cap} and {busy} busy of {limit}",
            starts.len()
        );
        let by_id: HashMap<Uuid, &PlanTicket> = tickets.iter().map(|t| (t.id, t)).collect();
        let st: HashMap<Uuid, TicketState> = tickets.iter().map(|t| (t.id, t.state)).collect();
        let mut seen = HashSet::new();
        for s in &starts {
            let t = by_id[&s.ticket];
            assert!(seen.insert(s.ticket), "case {case}: started twice");
            assert_eq!(t.state, TicketState::Todo, "case {case}");
            assert!(!t.assigned && !t.starting, "case {case}");
            assert!(
                t.needs
                    .iter()
                    .all(|n| st.get(n) == Some(&TicketState::Done)),
                "case {case}: started before its needs"
            );
        }
        // A ready ticket is left only once the limit is reached, and only
        // for ones ranked after those started.
        let ready: Vec<&PlanTicket> = tickets.iter().filter(|t| startable(t, &st)).collect();
        if starts.len() < ready.len() {
            assert_eq!(starts.len(), free, "case {case}");
        }
        if let Some(last) = starts.last().map(|s| by_id[&s.ticket].rank) {
            for t in &ready {
                if !starts.iter().any(|s| s.ticket == t.id) {
                    assert!(
                        t.rank >= last,
                        "case {case}: skipped a better-ranked ticket"
                    );
                }
            }
        }
        // Held: none at all.
        for reason in [
            Paused::Person,
            Paused::Budget,
            Paused::BossHeld,
            Paused::Restoring,
            Paused::NoBoss,
            Paused::StartFailing,
        ] {
            let mut held = p.clone();
            held.state = PlanState::Paused;
            held.paused = Some(reason);
            assert!(next_starts(&held, &tickets, room(&held, 0, 0, limit), None).is_empty());
        }
        for standing in [Status::Warn, Status::Over] {
            assert!(next_starts(&p, &tickets, room(&p, 0, 0, limit), Some(standing)).is_empty());
        }
        for state in [PlanState::Proposed, PlanState::Done, PlanState::Dropped] {
            let mut other = p.clone();
            other.state = state;
            assert!(next_starts(&other, &tickets, room(&other, 0, 0, limit), None).is_empty());
        }
    }
}

#[test]
fn a_plan_runs_only_once_approved_and_not_paused() {
    let mut p = plan(PlanState::Running);
    assert!(p.runs());
    assert!(!p.awaiting());
    p.approved_revision = None;
    assert!(!p.runs(), "never without the person's approval");
    let proposed = plan(PlanState::Proposed);
    assert!(proposed.awaiting());
    let mut changed = plan(PlanState::Running);
    changed.revision = 3;
    changed.approved_revision = Some(2);
    assert!(changed.runs(), "it runs on the last approved revision");
    assert!(changed.awaiting(), "while the person reads the newer one");
    let mut done = plan(PlanState::Done);
    done.revision = 2;
    assert!(!done.awaiting());
    assert_eq!(plan(PlanState::Running).limit(4), 4);
}

#[test]
fn paused_reasons_read_back_as_written() {
    for p in [
        Paused::Person,
        Paused::Budget,
        Paused::BossHeld,
        Paused::Restoring,
        Paused::NoBoss,
        Paused::StartFailing,
    ] {
        assert_eq!(Paused::parse(p.as_str()).unwrap(), p);
        let v: Value = serde_json::to_value(p).unwrap();
        assert_eq!(v, Value::String(p.as_str().into()));
        assert!(!p.why().is_empty());
    }
    assert!(Paused::parse("tired").is_err());
    for s in [
        PlanState::Proposed,
        PlanState::Running,
        PlanState::Paused,
        PlanState::Done,
        PlanState::Dropped,
    ] {
        assert_eq!(PlanState::parse(s.as_str()).unwrap(), s);
    }
}

#[test]
fn a_plan_is_settled_once_every_ticket_is_done_or_dropped() {
    assert!(!settled(&[]));
    assert!(settled(&[
        ticket(1, TicketState::Done, &[]),
        ticket(2, TicketState::Dropped, &[])
    ]));
    assert!(!settled(&[
        ticket(1, TicketState::Done, &[]),
        ticket(2, TicketState::Review, &[])
    ]));
}

// ── Changes ─────────────────────────────────────────────────────────────

fn base() -> Snapshot {
    let mut form = planned(2, "Wire the payment form", &[1]);
    form.engine = Engine::Codex;
    form.folder = "/work/acme-api".into();
    Snapshot {
        goal: "Ship the new checkout".into(),
        tickets: vec![
            planned(1, "Add the cart API", &[]),
            form,
            planned(3, "Fix the flaky payment test", &[]),
        ],
        parallel: Some(2),
        budget: None,
    }
}

/// An edit of a plan, by what it changes.
type Edit<'a> = (&'a str, &'a dyn Fn(&mut Snapshot));

#[test]
fn plan_change_classifies_each_kind_of_edit() {
    let none = HashSet::new();
    let a = base();
    assert_eq!(plan_change(&a, &a, &none), Ok(None));

    let minor = |f: &dyn Fn(&mut Snapshot)| {
        let mut b = base();
        f(&mut b);
        plan_change(&a, &b, &none)
    };
    let edits: [Edit; 7] = [
        ("order", &|s| s.tickets.swap(0, 2)),
        ("needs", &|s| s.tickets[1].needs.clear()),
        ("title", &|s| {
            s.tickets[0].title = "Add the basket API".into()
        }),
        // Within the folders and engines it already uses.
        ("folder", &|s| s.tickets[2].folder = "/work/acme-api".into()),
        ("engine", &|s| s.tickets[0].engine = Engine::Codex),
        ("view", &|s| s.tickets[2].view = Some(View::Terminal)),
        ("lower limit", &|s| s.parallel = Some(1)),
    ];
    for (what, f) in edits {
        assert_eq!(minor(f), Ok(Some(Change::Minor)), "{what}");
    }
    let scopes: [Edit; 8] = [
        ("goal", &|s| s.goal = "Ship checkout and refunds".into()),
        ("added", &|s| s.tickets.push(planned(4, "Refunds", &[]))),
        ("dropped", &|s| {
            s.tickets.pop();
        }),
        ("body", &|s| s.tickets[0].body = "Use the v2 schema".into()),
        ("new folder", &|s| {
            s.tickets[0].folder = "/work/acme-docs".into()
        }),
        ("new engine", &|s| s.tickets[0].engine = Engine::Gemini),
        ("higher limit", &|s| s.parallel = Some(3)),
        ("no limit", &|s| s.parallel = None),
    ];
    for (what, f) in scopes {
        assert_eq!(minor(f), Ok(Some(Change::Scope)), "{what}");
    }
}

/// A plan's budget is part of what the person approves: setting or
/// lowering it is within that, raising it or taking it off is theirs again.
#[test]
fn a_plans_budget_raised_or_taken_off_is_a_change_of_scope() {
    let none = HashSet::new();
    let capped = |amount: f64| Snapshot {
        budget: Some(PlanBudget::usd(amount)),
        ..base()
    };
    let a = capped(40.0);
    assert_eq!(
        plan_change(&base(), &a, &none),
        Ok(Some(Change::Minor)),
        "set"
    );
    assert_eq!(
        plan_change(&a, &capped(30.0), &none),
        Ok(Some(Change::Minor))
    );
    assert_eq!(
        plan_change(&a, &capped(60.0), &none),
        Ok(Some(Change::Scope))
    );
    assert_eq!(plan_change(&a, &base(), &none), Ok(Some(Change::Scope)));
    let mut later = capped(40.0);
    later.budget.as_mut().unwrap().warn_at = 0.95;
    assert_eq!(plan_change(&a, &later, &none), Ok(Some(Change::Scope)));
    let mut daily = capped(10.0);
    daily.budget.as_mut().unwrap().period = Period::Day;
    assert_eq!(
        plan_change(&a, &daily, &none),
        Ok(Some(Change::Scope)),
        "counted another way"
    );
    let mut told = capped(40.0);
    told.budget.as_mut().unwrap().on_exceed = OnExceed::Warn;
    assert_eq!(plan_change(&a, &told, &none), Ok(Some(Change::Scope)));
    assert_eq!(describe(&base(), &a), "Capped it at a total budget of $40");
    assert_eq!(describe(&a, &base()), "Took its budget off");
    let mut nothing = capped(0.0);
    nothing.tickets.truncate(1);
    let problems = validate(&nothing, &[], &|_, _| Ok(())).unwrap_err();
    assert!(problems.contains(&Problem::Budget(
        "A plan's budget is more than nothing.".into()
    )));
    // Kept in its revision, and read back by an older revision's shape.
    let back: Snapshot = serde_json::from_value(serde_json::to_value(&a).unwrap()).unwrap();
    assert_eq!(back, a);
    let old: Snapshot =
        serde_json::from_value(serde_json::json!({ "goal": "g", "tickets": [] })).unwrap();
    assert_eq!(old.budget, None);
    let said: PlanBudget = serde_json::from_value(serde_json::json!({ "amount": 40 })).unwrap();
    assert_eq!(said, PlanBudget::usd(40.0));
}

#[test]
fn where_a_started_ticket_runs_cant_change() {
    let a = base();
    let started = HashSet::from([u(1)]);
    let mut b = base();
    b.tickets[0].folder = "/work/acme-api".into();
    assert_eq!(
        plan_change(&a, &b, &started),
        Err(Problem::Started {
            title: "Add the cart API".into()
        })
    );
    let mut c = base();
    c.tickets[0].engine = Engine::Codex;
    assert!(plan_change(&a, &c, &started).is_err());
    // Its title may.
    let mut d = base();
    d.tickets[0].title = "Add the basket API".into();
    assert_eq!(plan_change(&a, &d, &started), Ok(Some(Change::Minor)));
}

#[test]
fn a_change_is_described_in_plain_words() {
    let a = base();
    assert_eq!(describe(&a, &a), "No change");
    let mut added = base();
    added.tickets.push(planned(4, "Refunds", &[3]));
    assert_eq!(describe(&a, &added), "Added “Refunds”");
    let mut moved = base();
    moved.tickets.swap(1, 2);
    assert_eq!(
        describe(&a, &moved),
        "Moved “Fix the flaky payment test” after “Add the cart API”"
    );
    let mut first = base();
    first.tickets.rotate_right(1);
    assert_eq!(
        describe(&a, &first),
        "Moved “Fix the flaky payment test” first"
    );
    let mut many = base();
    many.goal = "Ship checkout".into();
    many.tickets.remove(2);
    many.tickets[1].needs.clear();
    many.tickets[0].title = "Add the basket API".into();
    assert_eq!(
        describe(&a, &many),
        "Changed the goal to “Ship checkout”; dropped “Fix the flaky payment test”; renamed “Add the cart API” “Add the basket API”; made “Wire the payment form” need nothing first"
    );
}

#[test]
fn a_folder_left_for_desk_to_find_is_said_so_never_as_a_move_to_nothing() {
    let a = base();
    let mut left = base();
    left.tickets[0].folder = String::new();
    assert_eq!(
        describe(&a, &left),
        "Left the folder of “Add the cart API” for Desk to find"
    );
    assert_eq!(
        describe(&left, &a),
        "Moved “Add the cart API” to /work/acme-web"
    );
    let mut viewed = left.clone();
    viewed.tickets[0].view = Some(View::Terminal);
    assert_eq!(
        describe(&left, &viewed),
        "Set “Add the cart API” to run in the terminal view"
    );
}

#[test]
fn a_snapshot_reads_back_as_written() {
    let s = base();
    let v = serde_json::to_value(&s).unwrap();
    assert_eq!(v["tickets"][1]["needs"][0], Value::String(u(1).to_string()));
    let back: Snapshot = serde_json::from_value(v).unwrap();
    assert_eq!(back, s);
    assert_eq!(base().added(&a_without_last()).len(), 1);
    assert_eq!(a_without_last().dropped(&base()), vec![u(3)]);
}

fn a_without_last() -> Snapshot {
    let mut s = base();
    s.tickets.pop();
    s
}

#[test]
fn a_draft_keeps_the_limit_of_what_it_changes_and_an_old_snapshot_reads_with_none() {
    let before = base();
    let draft = Draft {
        goal: before.goal.clone(),
        tickets: vec![DraftTicket {
            id: Some(u(1)),
            title: "Add the cart API".into(),
            folder: "/work/acme-web".into(),
            ..Default::default()
        }],
    };
    let s = snapshot_of(&draft, Some(&before), Engine::Claude, &mut counter()).unwrap();
    assert_eq!(s.parallel, Some(2));
    let old: Snapshot =
        serde_json::from_value(serde_json::json!({ "goal": "g", "tickets": [] })).unwrap();
    assert_eq!(old.parallel, None);
    assert!(describe(
        &before,
        &Snapshot {
            parallel: Some(1),
            budget: None,
            ..base()
        }
    )
    .contains("uns at most 1 at once"));
}

/// Past its limit (a held session let go on again), the runner starts
/// nothing until enough of them stop working.
#[test]
fn over_the_limit_nothing_starts_until_it_drains() {
    let p = plan(PlanState::Running);
    let mut tickets = Vec::new();
    for i in 0..3 {
        tickets.push(PlanTicket {
            repo: None,
            id: u(i),
            key: None,
            state: TicketState::Todo,
            rank: i as f64,
            needs: vec![],
            engine: Some(Engine::Claude),
            folder: Some("/work/acme-web".into()),
            view: None,
            machine: None,
            model: None,
            assigned: false,
            session: None,
            starting: false,
            held_back: None,
        });
    }
    assert!(next_starts(&p, &tickets, room(&p, 5, 0, 4), None).is_empty());
    assert!(next_starts(&p, &tickets, room(&p, 4, 0, 4), None).is_empty());
    assert_eq!(next_starts(&p, &tickets, room(&p, 3, 0, 4), None).len(), 1);
}

/// The Mantis plan of 21 Nov: four at once, every ticket on devbox. Each
/// session it holds keeps its slot whatever it is doing, or whatever was
/// last seen of it, until it is closed or removed.
fn on_devbox(n: u128, state: TicketState) -> PlanTicket {
    let mut t = ticket(n, state, &[]);
    t.machine = Some("devbox".into());
    t.folder = Some(format!("/home/dev/mantis-{n}"));
    t
}

fn mantis() -> (Plan, Vec<PlanTicket>) {
    let mut p = plan(PlanState::Running);
    p.parallel = Some(4);
    let mut tickets: Vec<PlanTicket> = (1..=18)
        .map(|n| {
            let mut t = on_devbox(n, TicketState::Doing);
            t.assigned = true;
            t.session = Some(format!("devbox/{}", u(100 + n)));
            t
        })
        .collect();
    tickets.extend((19..=22).map(|n| on_devbox(n, TicketState::Todo)));
    (p, tickets)
}

#[test]
fn eighteen_sessions_on_a_machine_hold_eighteen_slots_whatever_was_seen_of_them() {
    let (p, mut tickets) = mantis();
    // Nothing seen of them yet (a restart, devbox not answering), or each
    // seen working, idle or asking, or killed and so interrupted or
    // stopped: none is closed, and their tickets are still in Doing.
    assert_eq!(held(&tickets), 18);
    assert!(next_starts(&p, &tickets, room(&p, 18, 0, 12), None).is_empty());
    let line = full(&progress(&tickets), 4).unwrap();
    assert!(line.starts_with("holds 18 of 4 sessions: ACM-1, ACM-2,"), "{line}");
    // Fifteen closed: one slot of four is free.
    for t in &mut tickets[..15] {
        t.session = None;
    }
    assert_eq!(held(&tickets), 3);
    let starts = next_starts(&p, &tickets, room(&p, 3, 0, 12), None);
    assert_eq!(starts.iter().map(|s| s.ticket).collect::<Vec<_>>(), [u(19)]);
    assert_eq!(full(&progress(&tickets), 4), None);
    // All eighteen closed: it starts four again.
    for t in &mut tickets {
        t.session = None;
    }
    assert_eq!(held(&tickets), 0);
    assert_eq!(next_starts(&p, &tickets, room(&p, 0, 0, 12), None).len(), 4);
}

#[test]
fn a_plan_holds_its_review_sessions_and_unknown_starts_once_each_and_not_its_done_ones() {
    let (_, mut tickets) = mantis();
    tickets.truncate(2);
    let mut review = on_devbox(30, TicketState::Review);
    review.session = Some("devbox/review".into());
    // A start whose outcome isn't known: no session yet.
    let mut unsure = on_devbox(31, TicketState::Doing);
    unsure.starting = true;
    // Started and linked, its link not ended yet: one session.
    let mut linked = on_devbox(32, TicketState::Doing);
    linked.starting = true;
    linked.session = Some("devbox/linked".into());
    // Two tickets on one session.
    let mut twice = on_devbox(33, TicketState::Doing);
    twice.session = tickets[0].session.clone();
    let mut done = on_devbox(34, TicketState::Done);
    done.session = Some("devbox/done".into());
    tickets.extend([review, unsure, linked, twice, done]);
    let names: Vec<String> = holding(&tickets).iter().map(|t| t.name()).collect();
    assert_eq!(names, ["ACM-1", "ACM-2", "ACM-30", "ACM-31", "ACM-32"]);
    // A closed session's ticket holds nothing; a linked start still does.
    tickets[2].session = None;
    tickets[4].session = None;
    assert_eq!(held(&tickets), 4);
}

#[test]
fn room_is_what_the_plan_and_its_boss_each_have_left() {
    let mut p = plan(PlanState::Running);
    p.parallel = Some(4);
    assert_eq!(room(&p, 0, 0, 12), 4);
    assert_eq!(room(&p, 3, 0, 12), 1);
    assert_eq!(room(&p, 18, 0, 12), 0);
    assert_eq!(room(&p, 1, 11, 12), 1);
    assert_eq!(room(&p, 0, 12, 12), 0);
    p.parallel = None;
    assert_eq!(room(&p, 5, 2, 12), 7);
}

#[test]
fn the_same_folder_on_another_computer_needs_no_copy_of_its_own() {
    let mut here = ticket(1, TicketState::Todo, &[]);
    here.folder = Some("/work/acme-web".into());
    let mut there = ticket(2, TicketState::Todo, &[]);
    there.machine = Some("devbox".into());
    let mut there_too = ticket(3, TicketState::Todo, &[]);
    there_too.machine = Some("devbox".into());
    let p = plan(PlanState::Running);
    let starts = next_starts(&p, &[here, there, there_too], room(&p, 0, 0, 4), None);
    let copies: Vec<bool> = starts.iter().map(|s| s.worktree).collect();
    assert_eq!(copies, [false, false, true], "only the second on devbox");
}

fn edit(n: u128) -> TicketEdit {
    TicketEdit {
        id: u(n),
        key: Some(format!("VEL-{n}")),
        ..Default::default()
    }
}

#[test]
fn an_edit_by_key_changes_only_what_it_says() {
    let mut a = planned(1, "Add the cache", &[]);
    a.body = "Cache the index in memory; keep it under 50 MB.".into();
    a.model = Some("opus".into());
    let mut b = planned(2, "Use the cache", &[1]);
    b.body = "Read through the cache.".into();
    let before = Snapshot {
        goal: "Faster reads".into(),
        tickets: vec![a.clone(), b.clone()],
        parallel: Some(2),
        budget: None,
    };
    let after = edited(
        &before,
        &[TicketEdit {
            machine: Some("devbox".into()),
            folder: Some("/home/dev/projects/driftwerk/vellev".into()),
            ..edit(2)
        }],
    )
    .unwrap();
    assert_eq!(after.tickets[0], a, "a ticket not edited is as it was");
    let moved = &after.tickets[1];
    assert_eq!(moved.machine.as_deref(), Some("devbox"));
    assert_eq!(moved.folder, "/home/dev/projects/driftwerk/vellev");
    assert_eq!(moved.title, b.title, "its title is kept");
    assert_eq!(moved.body, b.body, "its brief is kept");
    assert_eq!(moved.needs, b.needs, "its needs are kept");
    assert_eq!(after.parallel, Some(2));
    assert_eq!(after.goal, before.goal);

    // Its model is kept while its engine is, and goes with an engine change.
    let same = edited(
        &before,
        &[TicketEdit {
            folder: Some("/srv/v".into()),
            ..edit(1)
        }],
    )
    .unwrap();
    assert_eq!(same.tickets[0].model.as_deref(), Some("opus"));
    let codex = edited(
        &before,
        &[TicketEdit {
            engine: Some("codex".into()),
            ..edit(1)
        }],
    )
    .unwrap();
    assert_eq!(codex.tickets[0].engine, Engine::Codex);
    assert_eq!(codex.tickets[0].model, None);

    // Back to this computer by its word.
    let mut on = before.clone();
    on.tickets[1].machine = Some("devbox".into());
    let home = edited(
        &on,
        &[TicketEdit {
            machine: Some("local".into()),
            folder: Some("/work/acme-web".into()),
            ..edit(2)
        }],
    )
    .unwrap();
    assert_eq!(home.tickets[1].machine, None);
}

#[test]
fn an_edit_is_refused_by_the_key_it_names() {
    let mut taken = planned(3, "MAN-465", &[]);
    taken.adopted = true;
    let before = Snapshot {
        goal: "Faster reads".into(),
        tickets: vec![planned(1, "Add the cache", &[]), taken],
        ..Default::default()
    };
    let said = |edits: &[TicketEdit]| plain(&edited(&before, edits).unwrap_err());
    assert!(said(&[TicketEdit {
        folder: Some("/x".into()),
        ..edit(9)
    }])
    .contains("VEL-9 isn't one of the plan's tickets"),);
    let twice = said(&[
        TicketEdit {
            folder: Some("/x".into()),
            ..edit(1)
        },
        TicketEdit {
            folder: Some("/y".into()),
            ..edit(1)
        },
    ]);
    assert!(twice.contains("VEL-1 is edited twice"), "{twice}");
    let words = said(&[TicketEdit {
        body: Some("new".into()),
        ..edit(3)
    }]);
    assert!(
        words.contains("VEL-3 is taken on from its board"),
        "{words}"
    );
    let away = said(&[TicketEdit {
        machine: Some("devbox".into()),
        ..edit(1)
    }]);
    assert!(
        away.contains("VEL-1 moves to devbox: say its folder there too"),
        "{away}"
    );
    // The same machine again needs no folder.
    assert!(edited(
        &before,
        &[TicketEdit {
            machine: Some("local".into()),
            ..edit(1)
        }]
    )
    .is_ok());
}

#[test]
fn a_ticket_moved_with_its_repository_leaves_its_old_folder_behind() {
    let mut a = planned(1, "Add the cache", &[]);
    a.folder = "/Users/k/dev/vellev".into();
    a.repo = Some("github.com/driftwerk/vellev".into());
    let b = planned(2, "Use the cache", &[1]);
    let before = Snapshot {
        goal: "Faster reads".into(),
        tickets: vec![a.clone(), b],
        parallel: None,
        budget: None,
    };
    // Its repository says its folder on devbox: the Mac's path goes.
    let after = edited(
        &before,
        &[TicketEdit {
            machine: Some("devbox".into()),
            ..edit(1)
        }],
    )
    .unwrap();
    let moved = &after.tickets[0];
    assert_eq!(moved.machine.as_deref(), Some("devbox"));
    assert_eq!(moved.folder, "", "found as it starts");
    assert_eq!(moved.repo, a.repo, "its repository is kept");
    // Given as owner/name, it is kept as host/owner/name; one that can't
    // be one is refused.
    let named = edited(
        &before,
        &[TicketEdit {
            machine: Some("devbox".into()),
            repo: Some("driftwerk/workforce".into()),
            ..edit(2)
        }],
    )
    .unwrap();
    assert_eq!(
        named.tickets[1].repo.as_deref(),
        Some("github.com/driftwerk/workforce")
    );
    assert_eq!(named.tickets[1].folder, "");
    let bad = edited(
        &before,
        &[TicketEdit {
            repo: Some("not a repo".into()),
            ..edit(2)
        }],
    )
    .unwrap_err();
    assert!(
        bad.iter().any(|p| matches!(p, Problem::BadRepo { .. })),
        "{bad:?}"
    );
    // Without one, the move says its folder there.
    let none = edited(
        &before,
        &[TicketEdit {
            machine: Some("devbox".into()),
            ..edit(2)
        }],
    )
    .unwrap_err();
    assert!(none[0].to_string().contains("or its repo"), "{}", none[0]);
    // Saying the machine it is on already, in other letters, moves nothing.
    let mut on_devbox = before.clone();
    on_devbox.tickets[1].machine = Some("devbox".into());
    let same = edited(
        &on_devbox,
        &[TicketEdit {
            machine: Some("DevBox".into()),
            ..edit(2)
        }],
    )
    .unwrap();
    assert_eq!(same.tickets[1].machine.as_deref(), Some("devbox"));
    assert_eq!(same.tickets[1].folder, "/work/acme-web");
}

#[test]
fn a_ticket_of_a_repository_starts_with_its_checkout_found_there() {
    let mut a = ticket(1, TicketState::Todo, &[]);
    a.folder = Some(String::new());
    a.repo = Some("github.com/driftwerk/vellev".into());
    a.machine = Some("devbox".into());
    let mut b = ticket(2, TicketState::Todo, &[]);
    b.folder = Some("/home/dev/projects/driftwerk/vellev".into());
    b.repo = Some("github.com/driftwerk/vellev".into());
    b.machine = Some("devbox".into());
    let p = plan(PlanState::Running);
    let starts = next_starts(&p, &[a, b], room(&p, 0, 0, 4), None);
    assert_eq!(starts.len(), 2);
    assert_eq!(
        starts[0].repo.as_deref(),
        Some("github.com/driftwerk/vellev")
    );
    assert_eq!(starts[0].folder, "");
    assert_eq!(
        starts.iter().map(|s| s.worktree).collect::<Vec<_>>(),
        [false, true],
        "one checkout of it on devbox: the second gets a copy"
    );
    // A ticket of the folder alone, working in the checkout a ticket of
    // its repository found there: the same folder.
    let mut doing = ticket(3, TicketState::Doing, &[]);
    doing.folder = Some("/home/dev/projects/driftwerk/vellev/".into());
    doing.machine = Some("devbox".into());
    let mut c = ticket(4, TicketState::Todo, &[]);
    c.folder = Some("/home/dev/projects/driftwerk/vellev".into());
    c.repo = Some("github.com/driftwerk/vellev".into());
    c.machine = Some("devbox".into());
    let starts = next_starts(&p, &[doing, c], room(&p, 1, 0, 4), None);
    assert_eq!(starts.len(), 1);
    assert!(starts[0].worktree, "its folder is in use");
}

#[test]
fn a_ticket_names_its_folder_or_its_repository() {
    let mut t = draft_ticket("a", &[]);
    t.folder = String::new();
    let mut d = Draft {
        goal: "Ship it".into(),
        tickets: vec![t],
    };
    let s = snapshot_of(&d, None, Engine::Claude, &mut counter()).unwrap();
    let refused = validate(&s, &engines(), &fits).unwrap_err();
    assert!(
        refused
            .iter()
            .any(|p| matches!(p, Problem::NoFolderOrRepo { .. })),
        "{refused:?}"
    );
    d.tickets[0].repo = Some("driftwerk/vellev".into());
    let s = snapshot_of(&d, None, Engine::Claude, &mut counter()).unwrap();
    assert_eq!(validate(&s, &engines(), &fits), Ok(()));
    assert_eq!(
        s.tickets[0].repo.as_deref(),
        Some("github.com/driftwerk/vellev")
    );
}
