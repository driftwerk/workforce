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
        id: u(n),
        key: Some(format!("ACM-{n}")),
        state,
        rank: n as f64 * 1024.0,
        needs: needs.iter().map(|&x| u(x)).collect(),
        engine: Some(Engine::Claude),
        folder: Some("/work/acme-web".into()),
        view: None,
        assigned: false,
        starting: false,
    }
}

fn planned(n: u128, title: &str, needs: &[u128]) -> Planned {
    Planned {
        id: u(n),
        title: title.into(),
        body: String::new(),
        engine: Engine::Claude,
        folder: "/work/acme-web".into(),
        view: None,
        needs: needs.iter().map(|&x| u(x)).collect(),
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

fn is_dir(p: &str) -> bool {
    p.starts_with("/work/")
}

fn draft_ticket(title: &str, needs: &[usize]) -> DraftTicket {
    DraftTicket {
        id: None,
        title: title.into(),
        body: String::new(),
        engine: None,
        folder: "/work/acme-web".into(),
        view: None,
        needs: needs.to_vec(),
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
        "done", "doing", "review", "ready", "blocked", "waiting", "total", "next",
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
fn a_plan_that_can_run_here_is_valid() {
    let s = Snapshot {
        parallel: None,
        goal: "Ship the new checkout".into(),
        tickets: vec![
            planned(1, "Add the cart API", &[]),
            planned(2, "Wire it", &[1]),
        ],
    };
    assert_eq!(validate(&s, &engines(), &is_dir), Ok(()));
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
    let problems = validate(&s, &engines(), &is_dir).unwrap_err();
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
        goal: "g".into(),
        tickets: vec![
            planned(1, "a", &[]),
            planned(2, "b", &[3]),
            planned(3, "c", &[4]),
            planned(4, "d", &[2]),
            planned(5, "e", &[2]),
        ],
    };
    let problems = validate(&s, &engines(), &is_dir).unwrap_err();
    assert_eq!(problems, vec![Problem::Cycle("2, 3, 4, 5".into())]);
    assert!(problems[0].to_string().contains("in a loop"));
}

#[test]
fn a_plan_has_one_to_thirty_tickets_and_a_short_enough_goal() {
    let none = Snapshot {
        parallel: None,
        goal: "g".into(),
        tickets: vec![],
    };
    assert_eq!(
        validate(&none, &engines(), &is_dir),
        Err(vec![Problem::NoTickets])
    );
    let many = Snapshot {
        parallel: None,
        goal: "x".repeat(GOAL_MAX + 1),
        tickets: (1..=31).map(|n| planned(n, "t", &[])).collect(),
    };
    assert_eq!(
        validate(&many, &engines(), &is_dir),
        Err(vec![Problem::LongGoal(GOAL_MAX + 1), Problem::TooMany(31)])
    );
}

// ── Starts ──────────────────────────────────────────────────────────────

#[test]
fn an_approved_plan_starts_its_ready_tickets_up_to_the_limit_in_rank_order() {
    let p = plan(PlanState::Running);
    let mut tickets: Vec<PlanTicket> = (1..=5).map(|n| ticket(n, TicketState::Todo, &[])).collect();
    tickets[0].rank = 9_999.0;
    let starts = next_starts(&p, &tickets, 0, 4, None);
    let order: Vec<Uuid> = starts.iter().map(|s| s.ticket).collect();
    assert_eq!(order, vec![u(2), u(3), u(4), u(5)]);
    // One busy already: three.
    assert_eq!(next_starts(&p, &tickets, 1, 4, None).len(), 3);
    // The plan's own limit is lower.
    let mut low = p.clone();
    low.parallel = Some(2);
    assert_eq!(next_starts(&low, &tickets, 0, 4, None).len(), 2);
    // Never above the boss's.
    low.parallel = Some(9);
    assert_eq!(next_starts(&low, &tickets, 0, 4, None).len(), 4);
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
    let starts = next_starts(&p, &tickets, 1, 4, None);
    let wt: Vec<(Uuid, bool)> = starts.iter().map(|s| (s.ticket, s.worktree)).collect();
    assert_eq!(wt, vec![(u(2), true), (u(3), false), (u(4), true)]);
    // Alone in its folder, the first doesn't.
    let alone = next_starts(&p, &[ticket(5, TicketState::Todo, &[])], 0, 4, None);
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
        let busy = rng.below(6) as usize;
        let limit = 1 + rng.below(12) as usize;
        let mut p = plan(PlanState::Running);
        p.parallel = (rng.below(3) == 0).then(|| rng.below(6) as i16);
        let cap = p.limit(limit);
        let starts = next_starts(&p, &tickets, busy, limit, None);
        assert!(
            starts.len() <= cap.saturating_sub(busy),
            "case {case}: {} starts with {busy} busy of {cap}",
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
            assert_eq!(busy + starts.len(), cap.max(busy), "case {case}");
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
        ] {
            let mut held = p.clone();
            held.state = PlanState::Paused;
            held.paused = Some(reason);
            assert!(next_starts(&held, &tickets, 0, limit, None).is_empty());
        }
        for standing in [Status::Warn, Status::Over] {
            assert!(next_starts(&p, &tickets, 0, limit, Some(standing)).is_empty());
        }
        for state in [PlanState::Proposed, PlanState::Done, PlanState::Dropped] {
            let mut other = p.clone();
            other.state = state;
            assert!(next_starts(&other, &tickets, 0, limit, None).is_empty());
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
            ..base()
        }
    )
    .contains("uns at most 1 at once"));
}
