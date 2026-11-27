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
fn a_spell_backs_off_at_one_two_four_and_eight_windows_then_every_eight() {
    let window = Duration::minutes(30);
    let spell = |minutes: i64| spell_of(Duration::minutes(minutes), window);
    assert_eq!(spell(29), None);
    assert_eq!(spell(30), Some(0));
    assert_eq!(spell(59), Some(0));
    assert_eq!(spell(60), Some(1));
    assert_eq!(spell(120), Some(2));
    assert_eq!(spell(239), Some(2));
    assert_eq!(spell(240), Some(3));
    assert_eq!(spell(479), Some(3));
    assert_eq!(spell(480), Some(4));
    assert_eq!(spell(720), Some(5));
    assert_eq!(spell_of(Duration::minutes(90), Duration::zero()), None);
}

#[test]
fn each_part_stalls_on_its_own_since_it_last_came_nearer() {
    let r = result(vec![ci_minutes(5.0, 1), judged()]);
    let window = Duration::minutes(30);
    // CI stuck at 22 minutes all night; the judged part shown at 60.
    let readings = [
        reading(0, 0, Some(22.0), false),
        reading(50, 0, Some(22.0), false),
    ];
    let facts = [fact(
        60,
        FactKind::Checkpoint,
        Some(1),
        true,
        "only api ran",
    )];
    let p = progress(&r, &readings, &facts);
    assert_eq!(
        stalls(&p, at(-10), at(70), window),
        vec![Stall {
            part: 0,
            since: at(0),
            spell: 1
        }],
        "a checkpoint on the judged part never resets the stuck CI"
    );
    assert_eq!(
        stalls(&p, at(-10), at(100), window),
        vec![
            Stall {
                part: 0,
                since: at(0),
                spell: 1
            },
            Stall {
                part: 1,
                since: at(60),
                spell: 0
            }
        ]
    );
    // Better CI resets only its own part.
    let better = [
        reading(0, 0, Some(22.0), false),
        reading(80, 0, Some(9.0), false),
    ];
    let p = progress(&r, &better, &facts);
    assert_eq!(
        stalls(&p, at(-10), at(100), window),
        vec![Stall {
            part: 1,
            since: at(60),
            spell: 0
        }]
    );
    // Never read, nor shown: stalled since it was agreed.
    let p = progress(&r, &[], &[]);
    assert_eq!(stalls(&p, at(0), at(29), window), vec![]);
    assert_eq!(stalls(&p, at(0), at(30), window).len(), 2);
}

#[test]
fn a_met_part_never_stalls() {
    let r = result(vec![ci_minutes(5.0, 1)]);
    let p = progress(&r, &[reading(0, 0, Some(4.0), true)], &[]);
    assert!(met(&p));
    assert!(stalls(&p, at(0), at(600), Duration::minutes(30)).is_empty());
}

#[test]
fn failed_reads_are_counted_in_a_row_from_the_first_and_reset_by_a_good_one() {
    let r = result(vec![ci_minutes(5.0, 1)]);
    let failed = [reading(10, 0, None, false), reading(20, 0, None, false)];
    let p = progress(&r, &failed, &[]);
    assert_eq!(p.parts[0].failing, 2);
    assert_eq!(p.parts[0].failing_since, Some(at(10)));
    let mut again = failed.to_vec();
    again.push(reading(30, 0, Some(9.0), false));
    again.push(reading(40, 0, None, false));
    let p = progress(&r, &again, &[]);
    assert_eq!(p.parts[0].failing, 1);
    assert_eq!(p.parts[0].failing_since, Some(at(40)));
}

fn pull(n: u64) -> Evidence {
    Evidence::Pull {
        repo: "acme/api".into(),
        number: n,
    }
}

#[test]
fn a_goal_names_its_branches_and_pull_requests_and_those_its_heads_found() {
    let r = result(vec![
        ci_minutes(5.0, 1),
        Measure::PrMerged {
            repo: "Acme/API".into(),
            pr: PrPick::Head("fix-ci".into()),
        },
        Measure::ChecksGreen {
            repo: "acme/web".into(),
            pr: PrPick::Number(7),
        },
        judged(),
    ]);
    let mut found = reading(0, 1, Some(1.0), false);
    found.evidence = vec![pull(12)];
    let mut other = reading(0, 0, Some(9.0), false);
    other.evidence = vec![pull(99)];
    let refs = GoalRefs::of(&r, &[found, other]);
    assert!(refs.names_branch("acme/api", "main"));
    assert!(refs.names_branch("ACME/api", "fix-ci"));
    assert!(!refs.names_branch("acme/api", "spike"));
    assert!(refs.names_pull("acme/api", 12));
    assert!(refs.names_pull("acme/web", 7));
    assert!(
        !refs.names_pull("acme/api", 99),
        "a CI part's evidence names no PR of the goal"
    );
    let mut branches: Vec<&str> = refs.branches_of("acme/api").collect();
    branches.sort_unstable();
    assert_eq!(branches, vec!["fix-ci", "main"]);
    assert_eq!(refs.branches_of("acme/web").count(), 0);
}

#[test]
fn a_checkpoint_shows_only_new_evidence_of_the_goal_s_own_work() {
    let commit = |sha: &str| Evidence::Commit {
        repo: "acme/api".into(),
        sha: sha.into(),
    };
    let (a, b) = (commit("aaaa111"), commit("bbbb222"));
    assert_eq!(checkpoint_shows(&[], &[], &[]), Err(NotShown::Nothing));
    assert_eq!(
        checkpoint_shows(std::slice::from_ref(&a), &[Found::OnGoal], &[]),
        Ok(())
    );
    assert_eq!(
        checkpoint_shows(
            &[a.clone(), b.clone()],
            &[Found::OnGoal, Found::OffGoal],
            &[]
        ),
        Err(NotShown::OffGoal(b.clone())),
        "22 Nov: a commit on another branch doesn't count"
    );
    assert_eq!(
        checkpoint_shows(std::slice::from_ref(&b), &[Found::Missing], &[]),
        Err(NotShown::Missing(b.clone()))
    );
    assert_eq!(
        checkpoint_shows(std::slice::from_ref(&a), &[Found::OnGoal], &[&a]),
        Err(NotShown::NothingNew)
    );
    assert_eq!(
        checkpoint_shows(
            &[a.clone(), b.clone()],
            &[Found::OnGoal, Found::OnGoal],
            &[&a]
        ),
        Ok(())
    );
    assert_eq!(
        checkpoint_shows(&[a.clone(), b.clone()], &[Found::OnGoal], &[]),
        Err(NotShown::Missing(b))
    );
}

#[test]
fn a_goal_wakes_its_boss_once_a_spell_of_its_leading_stall_with_every_part() {
    let stall = |part, since, spell| Stall {
        part,
        since: at(since),
        spell,
    };
    let mut said = GoalSaid::default();
    // The judged part stalled from agreement, CI from its first reading.
    let first = [stall(1, 0, 0)];
    assert!(wakes(&first, &said));
    said.tell(&first);
    assert!(!wakes(&first, &said), "once a spell");
    let both = [stall(0, 3, 0), stall(1, 0, 0)];
    assert!(
        !wakes(&both, &said),
        "CI crossing its first window rides the next wake"
    );
    let next = [stall(0, 3, 0), stall(1, 0, 1)];
    assert_eq!(leading(&next), Some(&next[1]));
    assert!(wakes(&next, &said));
    said.tell(&next);
    assert!(
        !wakes(&[stall(0, 3, 1), stall(1, 0, 1)], &said),
        "CI's spell 1 waits for the leader's"
    );
    // The judged part shown: CI leads, at a spell already said.
    assert!(!wakes(&[stall(0, 3, 0)], &said));
    assert!(wakes(&[stall(0, 3, 1)], &said));
    // A stall that began again starts its spells again.
    assert!(wakes(&[stall(1, 40, 0)], &said));
    assert!(!wakes(&[], &said));
    // Ties lead by the longest stuck.
    let tie = [stall(0, 5, 2), stall(1, 1, 2)];
    assert_eq!(leading(&tie), Some(&tie[1]));
}

#[test]
fn the_messages_said_to_have_waited_are_kept_to_the_newest() {
    let mut said = GoalSaid::default();
    let ids: Vec<Uuid> = (0..UNDELIVERED_KEPT + 3).map(|_| Uuid::new_v4()).collect();
    for id in &ids {
        said.undelivered_told(*id);
    }
    assert_eq!(said.undelivered, ids[3..]);
}

#[test]
fn what_a_goal_names_is_said_in_a_steady_order_and_none_takes_any_work() {
    let r = result(vec![
        ci_minutes(5.0, 3),
        Measure::ChecksGreen {
            repo: "acme/web".into(),
            pr: PrPick::Number(7),
        },
        Measure::PrMerged {
            repo: "acme/api".into(),
            pr: PrPick::Head("fix/ci".into()),
        },
        judged(),
    ]);
    let refs = GoalRefs::of(&r, &[]);
    assert!(!refs.is_empty());
    assert_eq!(
        refs.words(),
        "fix/ci of acme/api, main of acme/api, PR #7 of acme/web"
    );
    let refs = GoalRefs::of(&result(vec![judged()]), &[]);
    assert!(refs.is_empty());
    let e = Evidence::Commit {
        repo: "acme/api".into(),
        sha: "d05e8a4".into(),
    };
    assert_eq!(
        checkpoint_shows(std::slice::from_ref(&e), &[Found::Unplaced], &[]),
        Err(NotShown::Unplaced(e))
    );
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

#[test]
fn a_parts_head_is_where_its_last_checkpoint_left_it_else_where_it_was_agreed() {
    let main = head_key("Acme/Web", "main");
    let heads = Heads::from([
        (main.clone(), "a".repeat(40)),
        (part_head_key(1, &main), "b".repeat(40)),
    ]);
    assert_eq!(head_for(&heads, 1, &main), Some(&*"b".repeat(40)));
    assert_eq!(head_for(&heads, 2, &main), Some(&*"a".repeat(40)));
    assert_eq!(head_for(&heads, 1, &head_key("acme/web", "#4")), None);
    assert!(heads_agreed(&heads));
    let moved = Heads::from([(part_head_key(1, &main), "b".repeat(40))]);
    assert!(
        !heads_agreed(&moved),
        "a part's alone isn't where it was agreed"
    );
    let unread = head_key("acme/web", "fix/x");
    let heads = Heads::from([
        (unread.clone(), ABSENT.to_string()),
        (part_head_key(2, &main), ABSENT.to_string()),
        (main.clone(), "a".repeat(40)),
    ]);
    assert_eq!(
        head_for(&heads, 2, &unread),
        None,
        "absent: all on it is new"
    );
    assert_eq!(head_for(&heads, 2, &main), Some(&*"a".repeat(40)));

    let pull = head_key("acme/web", "#4");
    let heads = Heads::from([
        (main.clone(), UNREAD.to_string()),
        (unread.clone(), "a".repeat(40)),
    ]);
    assert_eq!(head_for(&heads, 1, &main), None, "not read yet: no head");
    assert!(head_due(&heads, &main), "read again");
    assert!(!head_due(&heads, &unread));
    assert!(!head_due(&heads, &pull), "it came later");
    assert!(head_due(&Heads::new(), &pull), "nothing read yet");
}

#[test]
fn a_short_reply_says_yes_with_a_yes_word_and_no_refusing_one() {
    let yes = [
        "yes",
        "Yes!",
        "ok, go ahead",
        "Yeah, do it.",
        "sounds good",
        "LGTM",
        "👍",
        "yes please",
        "Okay 👍",
        "yes please do",
        "go for it",
        "yes thanks",
        "perfect",
        "great",
        "looks good",
        "ship it",
        "approved",
        "confirmed",
        "fine",
        "alright",
        "yup",
        "y",
        "Yes, no rush",
        "do it",
    ];
    for text in yes.into_iter().chain([
        "keep going",
        "Yes, keep it up",
        "ok, move on",
        "Looks good, keep going with it",
    ]) {
        assert_eq!(consent(text), Consent::Yes, "{text}");
    }
    for text in yes {
        assert_eq!(plainly(text), Consent::Yes, "{text}");
    }
    let no = [
        "no",
        "not yet",
        "wait",
        "yes but wait",
        "ok, change it",
        "don't",
        "hold on",
        "later",
        "nope",
    ];
    for text in no {
        assert_eq!(plainly(text), Consent::No, "{text}");
        assert_eq!(consent(text), Consent::No, "{text}");
    }
    for text in [
        "please",
        "hmm",
        "",
        "thanks",
        "ok?",
        "sure?",
        "approve?",
        "good question",
        "ok drop staging",
    ] {
        assert_eq!(plainly(text), Consent::Unclear, "{text}");
        assert_eq!(consent(text), Consent::Unclear, "{text}");
    }
}

#[test]
fn a_longer_answer_is_no_only_when_it_opens_with_a_refusal_or_qualifies_it() {
    let changes = [
        "ok, change step 3",
        "Sure, drop the staging part",
        "Yes, and remove step 2",
        "Looks good, though keep staging",
        "Please change the result to the CI check",
        "Yes, and keep it fast",
        "Is this the CI check on main?",
    ];
    for text in changes {
        assert_eq!(consent(text), Consent::Unclear, "{text}");
    }
    let goal = "Keep the docs build green";
    assert_eq!(
        agreement("Yes, keep the docs build green", goal),
        Consent::Yes,
        "agreeing in their own words, the words are the work"
    );
    assert_eq!(agreement("No, keep the old goal", goal), Consent::No);
    for changes in [
        "Sure, drop the staging part",
        "Yes start it, skip the deploy step",
    ] {
        assert_eq!(agreement(changes, goal), Consent::Unclear, "{changes}");
    }
    assert_eq!(agreement("Yes, that looks right to me", goal), Consent::Yes);
    let moves = "Yes, move the deploy step after tests";
    assert_eq!(
        consent(moves),
        Consent::Unclear,
        "an answer asking to move a step"
    );
    assert_eq!(
        agreement("Move it to devbox and start it now", goal),
        Consent::Yes,
        "their own words saying where the work they start runs"
    );
    let yes = [
        "Yes, that's not a problem",
        "Sounds good, can't wait",
        "Yes, no rush on the docs",
        "Apply the goal as you wrote it",
    ];
    for text in yes {
        assert_eq!(consent(text), Consent::Yes, "{text}");
    }
    let no = [
        "Yes, but not the docs",
        "No, keep the old goal",
        "Not yet, the release comes first",
        "Fine, except the staging part",
        "Ok, only if CI stays green",
        "Let's fix it, but later",
    ];
    for text in no {
        assert_eq!(consent(text), Consent::No, "{text}");
    }
}

#[test]
fn a_plain_reply_settles_only_what_the_bosss_message_before_it_proposed() {
    let proposal = "**Proposed done-when goal:**\n\n| Goal | Done when |\n|---|---|\n| Eden’s v1 is live for staff | **Eden v1 checklist** is ticked |\n\nYes, or change it?";
    let boss = vec![
        (at(0), "Earlier news.".to_string()),
        (at(5), proposal.to_string()),
    ];
    let said = |texts: &[(i64, &str)]| -> Vec<(DateTime<Utc>, String)> {
        texts.iter().map(|(m, t)| (at(*m), t.to_string())).collect()
    };
    let proposed = ["Eden's v1 is live for staff", "Eden v1 checklist is ticked"];
    let reply = |theirs: &[(DateTime<Utc>, String)], quote: &str| {
        replied_to(theirs, &boss, quote, &proposed, at(-60))
    };
    let yes = said(&[(1, "hello"), (6, "Yes.")]);
    assert_eq!(reply(&yes, "yes"), Ok(at(6)));
    assert_eq!(
        replied_to(&yes, &boss, "yes", &["Eden v2 is live"], at(-60)),
        Err(NotReplied::NotProposed(0))
    );
    assert_eq!(
        reply(&said(&[(4, "yes")]), "yes"),
        Err(NotReplied::NotProposed(0)),
        "a yes before the proposal answers the message before it"
    );
    assert_eq!(
        replied_to(&yes, &boss, "yes", &proposed, at(6)),
        Err(NotReplied::NoPlainReply),
        "a yes applied already, or older than the goal was last said back"
    );
    let button = format!("{ANSWER_LEAD}Shall I merge #5?”: Yes");
    let unclear = [
        said(&[(6, "yes"), (7, "wait, drop staging")]),
        said(&[(6, "drop staging"), (7, "yes")]),
        said(&[(6, "yes but drop the staging part")]),
        said(&[(6, "no")]),
        said(&[(6, "not yet")]),
        said(&[(6, button.as_str())]),
    ];
    for theirs in &unclear {
        let quote = &theirs.last().unwrap().1;
        assert!(reply(theirs, quote).is_err(), "{theirs:?}");
        assert!(reply(theirs, "yes").is_err(), "{theirs:?}");
    }
    assert_eq!(
        reply(&said(&[(6, "not yet")]), "not yet"),
        Err(NotReplied::Says(Consent::No))
    );
    assert_eq!(
        reply(&said(&[(6, "hmm")]), "hmm"),
        Err(NotReplied::Says(Consent::Unclear))
    );
    assert_eq!(reply(&said(&[(6, "go for it")]), "go for it"), Ok(at(6)));
    let two = vec![(at(5), format!("Shall I merge #5 first?\n\n{proposal}"))];
    assert_eq!(
        replied_to(&yes, &two, "yes", &proposed, at(-60)),
        Err(NotReplied::AsksMore)
    );
    assert_eq!(
        replied_to(&said(&[(6, "yes")]), &[], "yes", &proposed, at(-60)),
        Err(NotReplied::NothingProposed)
    );
}

#[test]
fn a_short_answer_or_agreement_counts_only_when_it_agrees_and_none_that_refuses() {
    for said in [Said::Answering, Said::Agreeing] {
        let messages = vec![
            (at(0), "no".to_string()),
            (at(1), "not yet".to_string()),
            (at(2), "Yes, but not the docs".to_string()),
            (at(3), "ok".to_string()),
            (at(4), "Yes, that looks right to me".to_string()),
        ];
        let by = |quote: &str| said_by(&messages, quote, at(-60), said);
        for refused in ["no", "not yet", "Yes, but not the docs"] {
            assert_eq!(by(refused), Err(NotSaid::Refuses), "{said:?} {refused}");
        }
        assert_eq!(by("ok"), Ok(at(3)), "{said:?}");
        assert_eq!(
            said_by(&[(at(0), "hmm".to_string())], "hmm", at(-60), said),
            Err(NotSaid::Unclear),
            "{said:?}"
        );
        assert_eq!(by("Yes, that looks right to me"), Ok(at(4)), "{said:?}");
    }
}

#[test]
fn a_plan_is_ended_on_the_persons_words_asking_for_it_or_their_reply_to_the_bosss_question() {
    let said = |texts: &[(i64, &str)]| -> Vec<(DateTime<Utc>, String)> {
        texts.iter().map(|(m, t)| (at(*m), t.to_string())).collect()
    };
    let theirs = said(&[
        (1, "ok"),
        (2, "the tests are green"),
        (3, "Please end the Mantis bugs plan now"),
    ]);
    let asking = |quote: &str| said_by(&theirs, quote, at(-60), Said::Asking);
    assert!(asks_to_end("end the Mantis bugs plan"));
    assert!(!asks_to_end("ok") && !asks_to_end("the tests"));
    assert_eq!(asking("end the Mantis bugs plan"), Ok(at(3)));
    assert_eq!(asking("Please end the Mantis bugs plan now"), Ok(at(3)));
    assert!(asking("ok").is_err(), "a short whole message isn't asking");
    assert!(matches!(asking("end the"), Err(NotSaid::TooShort { .. })));

    let boss = said(&[(4, "Every ticket is done. Shall I end the plan?")]);
    let yes = said(&[(5, "yes")]);
    assert_eq!(replied_to_end(&yes, &boss, "yes", at(-60)), Ok(at(5)));
    let other = said(&[(4, "The tests are green. Shall I open the PR?")]);
    assert_eq!(
        replied_to_end(&yes, &other, "yes", at(-60)),
        Err(NotReplied::NothingProposed),
        "a yes to something else"
    );
    assert!(replied_to_end(&said(&[(5, "no")]), &boss, "no", at(-60)).is_err());
}
