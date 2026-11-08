use uuid::Uuid;

use super::*;

fn boss(agent: &str, name: &str) -> BossRef {
    BossRef {
        agent: agent.into(),
        name: name.into(),
    }
}

/// Acme has Sarah, Atlas has Mia, Infra has no boss; Desk is the main boss.
fn ctx(from: Option<&str>, to: Option<&str>) -> MoveContext {
    let of = |g: Option<&str>| match g {
        Some("acme") => Some(boss("sarah", "Sarah")),
        Some("atlas") => Some(boss("mia", "Mia")),
        _ => Some(boss("desk", "Desk")),
    };
    MoveContext {
        from: from.map(str::to_string),
        bossed: ["acme".to_string(), "atlas".to_string()].into(),
        names: [
            ("acme".to_string(), "Acme".to_string()),
            ("atlas".to_string(), "Atlas".to_string()),
            ("infra".to_string(), "Infra".to_string()),
        ]
        .into(),
        from_boss: of(from),
        to_boss: of(to),
        main_boss: Some(boss("desk", "Desk")),
        ..MoveContext::default()
    }
}

fn ticket(id: u128, group: Option<&str>) -> TicketFact {
    TicketFact {
        id: Uuid::from_u128(id),
        key: Some(format!("ACM-{id}")),
        board: Uuid::from_u128(100),
        board_group: group.map(str::to_string),
        synced: false,
        plan: None,
        open: true,
    }
}

const S: Item = Item::Session(Uuid::from_u128(1));

/// A session Sarah runs, with its ticket, a question and spend.
fn busy(from: Option<&str>, to: Option<&str>) -> MoveContext {
    MoveContext {
        charged_by: ctx(from, to).from_boss,
        tickets: vec![ticket(12, from)],
        to_board: Some(Uuid::from_u128(200)),
        open_asks: 1,
        own_budget: Some(Uuid::from_u128(300)),
        ledger: (4, 3.2),
        ..ctx(from, to)
    }
}

#[test]
fn a_session_within_one_scope_changes_only_the_sidebar() {
    for (from, to) in [
        (None, Some("infra")),
        (Some("infra"), None),
        (Some("acme"), Some("acme")),
    ] {
        assert!(
            move_effects(&S, to, &busy(from, to), &Choices::default()).is_empty(),
            "{from:?} → {to:?}"
        );
    }
}

#[test]
fn a_session_across_bosses_leaves_its_ticket_ledger_and_questions_and_its_charge_moves() {
    let t = Uuid::from_u128(12);
    let c = busy(Some("acme"), Some("atlas"));
    assert_eq!(
        move_effects(&S, Some("atlas"), &c, &Choices::default()),
        [
            Effect::StaysUnassigned {
                ticket: t,
                synced: false
            },
            Effect::AsksWithdrawn { count: 1 },
            Effect::ChargeTo { boss: "mia".into() },
            Effect::LedgerStays { rows: 4, usd: 3.2 },
            Effect::BudgetFollows {
                budget: Uuid::from_u128(300)
            },
            Effect::Handover {
                from: "sarah".into(),
                to: "mia".into()
            },
            Effect::Tell,
        ]
    );
    // The ticket goes when the person says so, and leaves its plan.
    let mut planned = c.clone();
    planned.tickets[0].plan = Some(Uuid::from_u128(9));
    let goes = Choices {
        ticket_goes: true,
        release: true,
        ..Choices::default()
    };
    let effects = move_effects(&S, Some("atlas"), &planned, &goes);
    assert_eq!(
        effects[..2],
        [
            Effect::LeavesPlan {
                plan: Uuid::from_u128(9),
                ticket: t
            },
            Effect::TicketGoes {
                ticket: t,
                board: Uuid::from_u128(200)
            },
        ]
    );
    assert!(effects.contains(&Effect::Release), "released when chosen");
    // A synced ticket can't change boards, whatever is chosen.
    let mut synced = c.clone();
    synced.tickets[0].synced = true;
    assert_eq!(
        move_effects(&S, Some("atlas"), &synced, &goes)[0],
        Effect::StaysUnassigned {
            ticket: t,
            synced: true
        }
    );
    // No board there: the ticket stays.
    let mut boardless = c;
    boardless.to_board = None;
    assert!(matches!(
        move_effects(&S, Some("atlas"), &boardless, &goes)[0],
        Effect::StaysUnassigned { .. }
    ));
}

#[test]
fn a_session_into_and_out_of_the_main_bosses_scope() {
    // Out of Acme to Infra, which has no boss: the main boss's.
    let out = move_effects(
        &S,
        Some("infra"),
        &busy(Some("acme"), Some("infra")),
        &Choices::default(),
    );
    assert!(out.contains(&Effect::ChargeTo {
        boss: "desk".into()
    }));
    assert!(out.contains(&Effect::Handover {
        from: "sarah".into(),
        to: "desk".into()
    }));
    // Into Acme from ungrouped.
    let into = move_effects(
        &S,
        Some("acme"),
        &busy(None, Some("acme")),
        &Choices::default(),
    );
    assert!(into.contains(&Effect::ChargeTo {
        boss: "sarah".into()
    }));
    assert!(into.contains(&Effect::Handover {
        from: "desk".into(),
        to: "sarah".into()
    }));
    // A session nobody runs, with nothing on it: told, and Desk writes the
    // note when there is no boss on the old side.
    let mut quiet = ctx(None, Some("acme"));
    quiet.from_boss = None;
    assert_eq!(
        move_effects(&S, Some("acme"), &quiet, &Choices::default()),
        [
            Effect::Handover {
                from: "desk".into(),
                to: "sarah".into()
            },
            Effect::Tell
        ]
    );
}

#[test]
fn a_ticket_moves_to_another_board_unless_synced_or_there_is_none() {
    let item = Item::Ticket(Uuid::from_u128(12));
    let mut c = ctx(Some("acme"), Some("atlas"));
    c.tickets = vec![ticket(12, Some("acme"))];
    c.to_board = Some(Uuid::from_u128(200));
    c.ledger = (2, 1.0);
    c.assigned = true;
    assert_eq!(
        move_effects(&item, Some("atlas"), &c, &Choices::default()),
        [
            Effect::Unassigned {
                ticket: Uuid::from_u128(12)
            },
            Effect::TicketGoes {
                ticket: Uuid::from_u128(12),
                board: Uuid::from_u128(200)
            },
            Effect::LedgerStays { rows: 2, usd: 1.0 },
            Effect::Handover {
                from: "sarah".into(),
                to: "mia".into()
            },
        ]
    );
    // Within one scope: to another board, no note, no ledger line.
    let mut within = ctx(None, Some("infra"));
    within.tickets = vec![ticket(12, None)];
    within.to_board = Some(Uuid::from_u128(201));
    assert_eq!(
        move_effects(&item, Some("infra"), &within, &Choices::default()),
        [Effect::TicketGoes {
            ticket: Uuid::from_u128(12),
            board: Uuid::from_u128(201)
        }]
    );
    let mut synced = c.clone();
    synced.tickets[0].synced = true;
    assert!(matches!(
        move_effects(&item, Some("atlas"), &synced, &Choices::default())[..],
        [Effect::Refused { .. }]
    ));
    let mut boardless = c;
    boardless.to_board = None;
    assert!(matches!(
        move_effects(&item, Some("atlas"), &boardless, &Choices::default())[..],
        [Effect::Refused { .. }]
    ));
}

#[test]
fn a_room_moves_within_one_scope_only() {
    let room = Item::Room(Uuid::from_u128(5));
    assert!(move_effects(
        &room,
        Some("infra"),
        &ctx(None, Some("infra")),
        &Choices::default()
    )
    .is_empty());
    for (from, to) in [
        (None, Some("acme")),
        (Some("acme"), Some("atlas")),
        (Some("atlas"), None),
    ] {
        assert!(
            matches!(
                move_effects(&room, to, &ctx(from, to), &Choices::default())[..],
                [Effect::Refused { .. }]
            ),
            "{from:?} → {to:?}"
        );
    }
}

#[test]
fn a_boss_moved_into_a_bossed_group_asks_and_never_replaces_silently() {
    let sarah = Item::Boss("sarah".into());
    let mut c = ctx(Some("acme"), Some("atlas"));
    c.boss_charges = 2;
    c.boss_plans = vec![Uuid::from_u128(9)];
    assert_eq!(
        move_effects(&sarah, Some("atlas"), &c, &Choices::default()),
        [Effect::AsksToReplace { boss: "mia".into() }]
    );
    assert!(blocked(&move_effects(
        &sarah,
        Some("atlas"),
        &c,
        &Choices::default()
    ))
    .is_some());
    let replace = Choices {
        replace: true,
        ..Choices::default()
    };
    let effects = move_effects(&sarah, Some("atlas"), &c, &replace);
    assert_eq!(
        effects,
        [
            Effect::Replaces { boss: "mia".into() },
            Effect::PassesToMain {
                charges: 2,
                plans: 1
            },
            Effect::Handover {
                from: "sarah".into(),
                to: "desk".into()
            },
            Effect::MemoryStays,
        ]
    );
    assert!(blocked(&effects).is_none());
}

#[test]
fn a_boss_moved_into_a_group_with_none_passes_what_it_leaves_to_the_main_boss() {
    let sarah = Item::Boss("sarah".into());
    let mut c = ctx(Some("acme"), Some("infra"));
    // Infra has no boss of its own.
    c.to_boss = None;
    c.bossed.insert("infra".into());
    c.boss_charges = 1;
    assert_eq!(
        move_effects(&sarah, Some("infra"), &c, &Choices::default()),
        [
            Effect::PassesToMain {
                charges: 1,
                plans: 0
            },
            Effect::MemoryStays
        ]
    );
    // The main boss moving into a group leaves no one over ungrouped work.
    let desk = Item::Boss("desk".into());
    let mut main = ctx(None, Some("infra"));
    main.to_boss = None;
    main.boss_charges = 3;
    main.boss_plans = vec![Uuid::from_u128(1), Uuid::from_u128(2)];
    assert_eq!(
        move_effects(&desk, Some("infra"), &main, &Choices::default()),
        [
            Effect::LeftWithNoBoss {
                charges: 3,
                plans: 2
            },
            Effect::MemoryStays
        ]
    );
    assert!(move_effects(
        &sarah,
        Some("acme"),
        &ctx(Some("acme"), Some("acme")),
        &Choices::default()
    )
    .is_empty());
}

#[test]
fn a_refusal_is_the_only_effect() {
    let mut c = busy(Some("acme"), Some("atlas"));
    c.refused = Some("It ended.".into());
    assert_eq!(
        move_effects(&S, Some("atlas"), &c, &Choices::default()),
        [Effect::Refused {
            why: "It ended.".into()
        }]
    );
}

struct N;

impl Names for N {
    fn boss(&self, agent: &str) -> String {
        match agent {
            "mia" => "Mia".into(),
            "sarah" => "Sarah".into(),
            other => other.into(),
        }
    }
    fn ticket(&self, _: Uuid) -> String {
        "ACME-482".into()
    }
    fn group(&self, group: Option<&str>) -> String {
        match group {
            Some("acme") => "Acme".into(),
            Some("atlas") => "Atlas".into(),
            _ => "no group".into(),
        }
    }
}

#[test]
fn each_effect_reads_in_plain_words() {
    let text = |e: &Effect| effect_text(e, "acme-api", Some("acme"), Some("atlas"), &N);
    let t = Uuid::from_u128(1);
    assert_eq!(
        text(&Effect::StaysUnassigned {
            ticket: t,
            synced: true
        }),
        "ACME-482 lives on its board's Linear or Jira. It stays there, unassigned."
    );
    assert_eq!(
        text(&Effect::AsksToReplace { boss: "mia".into() }),
        "Atlas already has Mia. Replace them with acme-api? Mia stops overseeing."
    );
    assert_eq!(
        text(&Effect::MemoryStays),
        "acme-api keeps what they learned in Acme, but won't recall it while they run Atlas."
    );
    assert_eq!(
        text(&Effect::LedgerStays { rows: 3, usd: 3.2 }),
        "What it spent so far ($3.20) stays in Acme's spend; what it spends from now on is Atlas's."
    );
    assert_eq!(
        text(&Effect::Handover {
            from: "sarah".into(),
            to: "mia".into()
        }),
        "Mia receives only the note below from Sarah."
    );
    // Every effect says something.
    for e in [
        Effect::ChargeTo { boss: "mia".into() },
        Effect::Release,
        Effect::TicketGoes {
            ticket: t,
            board: t,
        },
        Effect::LeavesPlan { plan: t, ticket: t },
        Effect::Unassigned { ticket: t },
        Effect::BudgetFollows { budget: t },
        Effect::AsksWithdrawn { count: 2 },
        Effect::Replaces { boss: "mia".into() },
        Effect::PassesToMain {
            charges: 1,
            plans: 2,
        },
        Effect::LeftWithNoBoss {
            charges: 0,
            plans: 1,
        },
        Effect::Tell,
    ] {
        assert!(!text(&e).is_empty());
    }
}

#[test]
fn the_facts_note_says_the_facts_and_is_never_too_long() {
    let note = facts_note(&NoteFacts {
        item: "acme-api".into(),
        goal: Some("Fix the login".into()),
        ticket: Some("ACM-12 · Fix the login".into()),
        last_reply: Some("The test passes now. Next I'll push.".into()),
        open_asks: vec!["Which port?".into()],
        spend_usd: Some(1.5),
        next: None,
    });
    assert_eq!(
        note,
        "Handing over acme-api.\nGoal: Fix the login\nTicket: ACM-12 · Fix the login\n\
         Last reply: The test passes now.\nOpen questions: Which port?\nSpent so far: $1.50"
    );
    let long = facts_note(&NoteFacts {
        item: "x".repeat(MAX_NOTE * 2),
        ..NoteFacts::default()
    });
    assert_eq!(long.chars().count(), MAX_NOTE);
    assert!(check_note(" ").is_err());
    assert!(check_note(&"é".repeat(MAX_NOTE)).is_ok());
    assert!(check_note(&"é".repeat(MAX_NOTE + 1)).is_err());
}
