use chrono::{Duration, TimeZone};

use super::*;
use crate::plan::PlanState;

fn at(minute: i64) -> DateTime<Utc> {
    Utc.with_ymd_and_hms(2026, 11, 20, 20, 0, 0).unwrap() + Duration::minutes(minute)
}

fn ci_minutes(at_most: f64, runs: u8) -> Measure {
    Measure::Ci {
        repo: "acme/api".into(),
        workflow: None,
        branch: "main".into(),
        want: Want::Minutes {
            per: Per::Job,
            at_most,
            runs,
        },
    }
}

fn judged() -> Measure {
    Measure::Judged {
        evidence: "only the touched services are tested".into(),
    }
}

fn result(parts: Vec<Measure>) -> GoalResult {
    GoalResult {
        words: "Done when each service's CI job on main takes at most 5 minutes".into(),
        parts,
    }
}

fn reading(minute: i64, part: u16, value: Option<f64>, met: bool) -> Reading {
    Reading {
        at: at(minute),
        part,
        value,
        met,
        words: value.map(minutes).unwrap_or_default(),
        evidence: Vec::new(),
        failed: value.is_none().then(|| "GitHub is slow".to_string()),
    }
}

fn fact(minute: i64, kind: FactKind, part: Option<u16>, checked: bool, text: &str) -> Fact {
    Fact {
        id: Uuid::new_v4(),
        plan: Uuid::nil(),
        kind,
        part,
        at: at(minute),
        text: text.into(),
        evidence: vec![Evidence::Commit {
            repo: "acme/api".into(),
            sha: "abc1234".into(),
        }],
        checked,
        made_by: Party::you(),
        replaces: None,
    }
}

#[test]
fn a_result_reads_from_the_tool_s_words() {
    let r: GoalResult = serde_json::from_value(serde_json::json!({
        "words": "CI in a few minutes",
        "parts": [
            { "kind": "ci", "repo": "acme/api", "branch": "main",
              "want": { "kind": "minutes", "per": "job", "at_most": 5, "runs": 3 } },
            { "kind": "pr_merged", "repo": "acme/api", "pr": 12 },
            { "kind": "checks_green", "repo": "acme/api", "pr": "fix-ci" },
            { "kind": "judged", "evidence": "the services a change touches" }
        ]
    }))
    .unwrap();
    assert_eq!(r.parts[0], ci_minutes(5.0, 3));
    assert_eq!(
        r.parts[1],
        Measure::PrMerged {
            repo: "acme/api".into(),
            pr: PrPick::Number(12)
        }
    );
    assert_eq!(
        r.parts[2],
        Measure::ChecksGreen {
            repo: "acme/api".into(),
            pr: PrPick::Head("fix-ci".into())
        }
    );
    assert_eq!(validate(&r), Ok(()));
    let back: GoalResult = serde_json::from_value(serde_json::to_value(&r).unwrap()).unwrap();
    assert_eq!(back, r);
}

#[test]
fn validate_says_every_problem() {
    assert_eq!(
        validate(&GoalResult {
            words: " ".into(),
            parts: Vec::new()
        }),
        Err(vec![Problem::Parts, Problem::Words])
    );
    assert_eq!(
        validate(&result(vec![judged(); 5])),
        Err(vec![Problem::Parts])
    );
    let bad = GoalResult {
        words: "x".repeat(WORDS_MAX + 1),
        parts: vec![
            Measure::Ci {
                repo: "acme/../api".into(),
                workflow: Some(" ".into()),
                branch: "".into(),
                want: Want::Minutes {
                    per: Per::Run,
                    at_most: f64::NAN,
                    runs: 0,
                },
            },
            Measure::Ci {
                repo: "acme/api".into(),
                workflow: None,
                branch: "main".into(),
                want: Want::Green { runs: 11 },
            },
            Measure::PrMerged {
                repo: "acme".into(),
                pr: PrPick::Number(0),
            },
            Measure::Judged {
                evidence: "".into(),
            },
        ],
    };
    assert_eq!(
        validate(&bad),
        Err(vec![
            Problem::Words,
            Problem::Repo(1),
            Problem::Branch(1),
            Problem::Workflow(1),
            Problem::Bound(1),
            Problem::Runs(1),
            Problem::Runs(2),
            Problem::Repo(3),
            Problem::Pull(3),
            Problem::Evidence(4),
        ])
    );
    assert_eq!(
        validate(&result(vec![ci_minutes(-1.0, 3)])),
        Err(vec![Problem::Bound(1)])
    );
    assert_eq!(
        validate(&result(vec![Measure::ChecksGreen {
            repo: "acme/api".into(),
            pr: PrPick::Head(" ".into())
        }])),
        Err(vec![Problem::Pull(1)])
    );
}

#[test]
fn a_github_repo_is_owner_and_name_alone() {
    assert_eq!(github_repo(" acme/api "), Some(("acme", "api")));
    assert_eq!(github_repo("acme/api.rs"), Some(("acme", "api.rs")));
    for bad in [
        "acme",
        "acme/",
        "/api",
        "acme/..",
        "acme/api?x",
        "acme/api#1",
        "a/b/c",
    ] {
        assert_eq!(github_repo(bad), None, "{bad}");
    }
}

#[test]
fn a_measured_part_keeps_its_best_and_when_it_came_nearer() {
    let r = result(vec![ci_minutes(5.0, 3)]);
    let readings = [
        reading(0, 0, Some(22.0), false),
        reading(10, 0, Some(25.0), false),
        reading(20, 0, None, false),
        reading(30, 0, Some(9.0), false),
        reading(40, 0, None, false),
        reading(50, 0, None, false),
    ];
    let p = progress(&r, &readings, &[]);
    let part = &p.parts[0];
    assert_eq!(part.best.as_ref().unwrap().value, Some(9.0));
    assert_eq!(part.improved_at, Some(at(30)));
    assert_eq!(part.newest.as_ref().unwrap().at, at(50));
    assert_eq!(part.failing, 2);
    assert!(!part.met);
    assert!(!met(&p));
}

#[test]
fn a_part_is_met_as_its_newest_read_says() {
    let r = result(vec![ci_minutes(5.0, 3)]);
    let p = progress(
        &r,
        &[
            reading(0, 0, Some(4.0), true),
            reading(10, 0, Some(6.0), false),
        ],
        &[],
    );
    assert!(!p.parts[0].met);
    let p = progress(
        &r,
        &[
            reading(0, 0, Some(6.0), false),
            reading(10, 0, Some(4.0), true),
            reading(20, 0, None, false),
        ],
        &[],
    );
    assert!(p.parts[0].met, "a failed read changes nothing of it");
    assert!(met(&p));
}

#[test]
fn a_met_reading_that_is_no_nearer_still_counts_as_coming_nearer() {
    let r = result(vec![Measure::Ci {
        repo: "acme/api".into(),
        workflow: None,
        branch: "main".into(),
        want: Want::Green { runs: 1 },
    }]);
    let p = progress(
        &r,
        &[
            reading(0, 0, Some(1.0), true),
            reading(10, 0, Some(1.0), true),
        ],
        &[],
    );
    assert_eq!(p.parts[0].improved_at, Some(at(10)));
    assert_eq!(p.parts[0].best.as_ref().unwrap().at, at(0));
}

#[test]
fn a_judged_part_comes_nearer_only_with_its_own_checked_checkpoints() {
    let r = result(vec![ci_minutes(5.0, 3), judged()]);
    let facts = [
        fact(5, FactKind::Checkpoint, Some(1), true, "split the workflow"),
        fact(15, FactKind::Checkpoint, Some(1), false, "unchecked"),
        fact(25, FactKind::Fact, None, true, "a fact"),
        fact(
            35,
            FactKind::Checkpoint,
            Some(0),
            true,
            "toward the wrong part",
        ),
    ];
    let p = progress(&r, &[], &facts);
    assert_eq!(p.parts[1].improved_at, Some(at(5)));
    assert!(!p.parts[1].met);
    assert_eq!(p.parts[0].improved_at, None, "parts are independent");
    assert!(!met(&p), "only the person meets a judged part");
}

#[test]
fn the_read_back_carries_the_first_reading_in_one_sentence() {
    let r = result(vec![ci_minutes(5.0, 3)]);
    let first = progress(&r, &[reading(0, 0, Some(22.0), false)], &[]);
    assert_eq!(
        read_back("Mantis CI tests only what a change touches.", &r, &first),
        "I've saved this goal: Mantis CI tests only what a change touches. It's done when each \
         service's CI job on main takes at most 5 minutes; now 22 minutes."
    );
}

#[test]
fn the_read_back_says_what_desk_cant_measure() {
    let r = result(vec![ci_minutes(5.0, 3), judged()]);
    let first = progress(&r, &[reading(0, 0, Some(22.0), false)], &[]);
    let said = read_back("Mantis CI", &r, &first);
    assert!(
        said.ends_with(
            "now 22 minutes. I can't measure “only the touched services are tested” \
             automatically, so I'll judge it from the work's evidence."
        ),
        "{said}"
    );
    let only_judged = result(vec![judged()]);
    let said = read_back("Mantis CI", &only_judged, &progress(&only_judged, &[], &[]));
    assert!(
        said.contains("takes at most 5 minutes. I can't measure"),
        "{said}"
    );
}

#[test]
fn the_read_back_names_each_part_of_a_composite() {
    let r = GoalResult {
        words: "the PR is merged with CI under 5 minutes".into(),
        parts: vec![
            ci_minutes(5.0, 1),
            Measure::PrMerged {
                repo: "acme/api".into(),
                pr: PrPick::Number(12),
            },
        ],
    };
    let mut open = reading(0, 1, Some(1.0), false);
    open.words = "open".into();
    let first = progress(&r, &[reading(0, 0, Some(9.0), false), open], &[]);
    let said = read_back("Ship the fix", &r, &first);
    assert_eq!(
        said,
        "I've saved this goal: Ship the fix. It's done when the PR is merged with CI under 5 \
         minutes; now CI on main of acme/api 9 minutes, PR #12 of acme/api open."
    );
    assert!(!said.contains(&Uuid::nil().to_string()));
}

#[test]
fn the_read_back_says_a_failed_first_read_and_a_goal_met_already() {
    let r = result(vec![ci_minutes(5.0, 1)]);
    let failed = progress(&r, &[reading(0, 0, None, false)], &[]);
    assert!(read_back("CI", &r, &failed).ends_with("; now unread (GitHub is slow)."));
    let met_now = progress(&r, &[reading(0, 0, Some(3.0), true)], &[]);
    assert!(read_back("CI", &r, &met_now).ends_with("at most 5 minutes; it's met already."));
}

#[test]
fn normalise_folds_quotes_case_space_and_trailing_punctuation() {
    assert_eq!(
        normalise("  Yes,\tDO   it — it’s “fine” now!!  "),
        "yes, do it — it's \"fine\" now"
    );
    assert_eq!(normalise("ﬁx the ＣＩ…"), "fix the ci");
}

#[test]
fn said_by_finds_the_quote_in_one_message_after_since() {
    let messages = vec![
        (at(0), "Hmm, not sure yet.".to_string()),
        (at(10), "Yes, go ahead and make CI faster!".to_string()),
        (at(20), "Also the deploy".to_string()),
        (at(30), "ok".to_string()),
    ];
    let agreed = |quote: &str, since: i64| said_by(&messages, quote, at(since), Said::Agreeing);
    assert_eq!(agreed("go ahead and make CI faster", -60), Ok(at(10)));
    assert_eq!(
        agreed("“Yes, go ahead”", -60),
        Ok(at(10)),
        "quotes, case and punctuation are folded"
    );
    assert_eq!(
        agreed("faster also the deploy", -60),
        Err(NotSaid::NotFound),
        "a quote across two messages"
    );
    assert_eq!(
        agreed("go ahead and make", 10),
        Err(NotSaid::TooOld(at(10))),
        "only before since"
    );
    assert_eq!(agreed("go ahead and break CI", -60), Err(NotSaid::NotFound));
    assert_eq!(agreed("make CI", -60), Ok(at(10)));
    assert_eq!(
        agreed("ahead", -60),
        Err(NotSaid::TooShort { words: 2, chars: 6 }),
        "one word of a longer message"
    );
    assert_eq!(
        agreed("OK.", -60),
        Ok(at(30)),
        "the whole of a short message"
    );
    assert_eq!(
        said_by(&messages, "make CI faster", at(-60), Said::Changing),
        Err(NotSaid::TooShort {
            words: 4,
            chars: 15
        }),
        "three words don't change a goal"
    );
    assert_eq!(
        said_by(&messages, "ok", at(-60), Said::Changing),
        Err(NotSaid::TooShort {
            words: 4,
            chars: 15
        }),
        "nor a whole short message, unprompted"
    );
    assert_eq!(
        said_by(
            &messages,
            "go ahead and make CI faster",
            at(-60),
            Said::Changing
        ),
        Ok(at(10))
    );
}

#[test]
fn a_reply_answers_a_question_whole_however_short_and_never_in_part() {
    let messages = vec![
        (at(0), "yes".to_string()),
        (at(10), "Yes, do it".to_string()),
        (at(20), "No — yes, I see why, but not now".to_string()),
    ];
    let answered = |quote: &str, since: i64| said_by(&messages, quote, at(since), Said::Answering);
    assert_eq!(answered("yes", -60), Ok(at(0)));
    assert_eq!(answered("Yes, do it", -60), Ok(at(10)));
    assert_eq!(
        answered("yes, do", -60),
        Err(NotSaid::NotWhole),
        "two words of a reply aren't it"
    );
    assert_eq!(
        answered("I see why", -60),
        Err(NotSaid::NotWhole),
        "a part of a no is never a yes"
    );
    assert_eq!(
        answered("yes", 0),
        Err(NotSaid::NotWhole),
        "a lone yes inside a longer reply after the question isn't one"
    );
    assert_eq!(answered("yes", 5), Err(NotSaid::NotWhole));
    assert_eq!(
        said_by(&messages[..1], "yes", at(5), Said::Answering),
        Err(NotSaid::TooOld(at(0))),
        "before the question"
    );
    assert_eq!(answered("go", -60), Err(NotSaid::NotFound));
}

#[test]
fn an_answer_from_an_items_buttons_is_only_what_follows_its_question() {
    let posted = format!("{ANSWER_LEAD}Change the goal to “ship it”?”: no");
    assert_eq!(answered_text(&posted), "no");
    assert_eq!(answered_text("yes please"), "yes please");
    assert!(says_goal(
        "Change the goal to: ship the API by Friday!",
        "Ship the API by Friday"
    ));
    assert!(!says_goal(
        "Change the goal to shipping",
        "Ship the API by Friday"
    ));
}

#[test]
fn the_same_proposal_is_the_same_goal_result_and_claim() {
    let r = result(vec![ci_minutes(5.0, 3)]);
    let asked = Proposal {
        goal: Some("Make CI fast".into()),
        result: Some(r.clone()),
        met: None,
    };
    assert!(same_proposal(
        &asked,
        &Proposal {
            goal: Some("  make   CI fast. ".into()),
            result: Some(r.clone()),
            met: None
        }
    ));
    assert!(!same_proposal(
        &asked,
        &Proposal {
            goal: Some("Make CI fast".into()),
            result: Some(result(vec![ci_minutes(6.0, 3)])),
            met: None
        }
    ));
    assert!(!same_proposal(
        &asked,
        &Proposal {
            goal: Some("Make CI fast".into()),
            result: None,
            met: None
        }
    ));
    assert!(!same_proposal(
        &Proposal {
            met: Some("run 12 was 4 minutes".into()),
            ..Proposal::default()
        },
        &Proposal {
            goal: Some("Make CI fast".into()),
            ..Proposal::default()
        }
    ));
    assert!(same_proposal(
        &Proposal {
            met: Some("run 12 was 4 minutes".into()),
            ..Proposal::default()
        },
        &Proposal {
            met: Some("the person says so".into()),
            ..Proposal::default()
        }
    ));
}

#[test]
fn a_session_sent_work_after_the_person_s_turn_with_no_goal_is_unsaved() {
    let (a, b, c, d) = (
        Uuid::new_v4(),
        Uuid::new_v4(),
        Uuid::new_v4(),
        Uuid::new_v4(),
    );
    let late = Uuid::new_v4();
    let sends = [
        (a, at(15)),
        (b, at(15)),
        (c, at(5)),
        (d, at(30)),
        (late, at(10 + 31)),
    ];
    let on_goals = HashSet::from([b]);
    assert_eq!(
        unsaved_goals(&[a, b, c, late], &sends, &[at(10)], &on_goals),
        vec![a],
        "b is on a goal, c was sent work before the person spoke, d isn't the boss's to run, \
         and late was sent long after the person's turn, not as work they asked for"
    );
    assert!(unsaved_goals(&[a], &sends, &[], &on_goals).is_empty());
}

fn goal_plan(goal: &str, r: GoalResult) -> Plan {
    Plan {
        id: Uuid::new_v4(),
        board: Uuid::new_v4(),
        group: None,
        boss: "boss".into(),
        goal: goal.into(),
        state: PlanState::Running,
        paused: None,
        paused_why: None,
        parallel: None,
        revision: 1,
        approved_revision: Some(1),
        created_at: at(0),
        updated_at: at(0),
        result: Some(r),
        agreed: Some("yes do it".into()),
        agreed_at: Some(at(0)),
        read_back_at: None,
        goal_ticket: None,
        met: None,
    }
}

#[test]
fn the_goals_block_opens_with_its_marker_and_shows_the_newest_facts() {
    let r = result(vec![ci_minutes(5.0, 3)]);
    let p = goal_plan("Make CI fast", r.clone());
    let progress = progress(&r, &[reading(0, 0, Some(22.0), false)], &[]);
    let facts: Vec<Fact> = (0..10)
        .rev()
        .map(|i| fact(i, FactKind::Fact, None, i % 2 == 0, &format!("fact {i}")))
        .collect();
    let mut draft = goal_plan("Ship the docs", r.clone());
    draft.approved_revision = None;
    draft.state = PlanState::Proposed;
    let id = p.id;
    let block = goals_block(&[(p, progress.clone()), (draft.clone(), progress)], &facts).unwrap();
    assert!(block.starts_with(GOALS_MARKER));
    let drafts = block.find("Proposed, not yet agreed").unwrap();
    assert!(block.find("“Make CI fast”").unwrap() < drafts);
    assert!(block.find("“Ship the docs”").unwrap() > drafts);
    assert!(block.contains(&format!(
        "- “Make CI fast” (plan_id {id}) · Done when each service's CI job on main takes at most 5 minutes · \
         now: CI on main of acme/api: 22 minutes, for at most 5 minutes a job, 3 runs in a row · unmet"
    )));
    assert!(block.contains("fact 9 (unchecked)"));
    assert!(block.contains("fact 8\n"));
    assert!(block.contains("fact 2"));
    assert!(!block.contains("fact 1"), "only the 8 newest");
    assert!(!block.contains("\n\n"), "one passage");
    assert_eq!(goals_block(&[], &facts), None);
}

#[test]
fn a_long_fact_is_cut_in_the_goals_block() {
    let r = result(vec![judged()]);
    let p = goal_plan("Make CI fast", r.clone());
    let long = fact(1, FactKind::Checkpoint, Some(0), true, &"word ".repeat(200));
    let block = goals_block(&[(p, progress(&r, &[], &[]))], &[long]).unwrap();
    let line = block.lines().last().unwrap();
    assert!(line.chars().count() < FACT_MAX + 60, "{line}");
    assert!(line.contains("checkpoint (part 1)"));
}

#[test]
fn only_a_message_that_stands_alone_is_more_than_an_answer_to_the_last() {
    assert!(!stands_alone("ok"));
    assert!(!stands_alone("yes go ahead"));
    assert!(stands_alone("Go ahead, start it"));
}

#[test]
fn a_plan_approved_holds_what_a_question_proposed_when_it_holds_each_part() {
    let measured = result(vec![judged()]);
    let asked = |goal: Option<&str>, result: Option<GoalResult>, met: Option<&str>| Proposal {
        goal: goal.map(str::to_string),
        result,
        met: met.map(str::to_string),
    };
    let covers = |p: &Proposal| approved_covers(p, "Make  the suite green", Some(&measured));
    assert!(covers(&asked(Some("make the suite green"), None, None)));
    assert!(covers(&asked(None, Some(measured.clone()), None)));
    assert!(covers(&asked(
        Some("Make the suite green"),
        Some(measured.clone()),
        None
    )));
    assert!(!covers(&asked(Some("Ship the docs"), None, None)));
    assert!(!covers(&asked(None, Some(result(vec![])), None)));
    assert!(
        !covers(&asked(None, None, Some("it ran green"))),
        "met is never approved on a card"
    );
    assert!(!covers(&Proposal::default()));
}
