//! Act fully's directives: a few standing questions the boss asks the
//! person when Act fully goes on ("Can I merge pull requests?"), so it has
//! a baseline to decide from when no one is there. Each answer is one of
//! the person's preferences ([`crate::preference`]), marked with the
//! directive it answers, so it is read, cited, paused and forgotten as
//! every preference is. The person can add their own in their own words.
//!
//! Nothing waits on a directive: Act fully acts whatever is answered. An
//! unanswered one is decided by the boss's own judgment, which it says.

use chrono::{DateTime, Duration, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::preference::{PrefScope, PrefSource, PrefState, Preference};

/// One answer to a directive.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Answer {
    pub key: &'static str,
    /// What the person taps.
    pub label: &'static str,
    /// The rule it keeps (a preference's `then`).
    pub then: &'static str,
}

/// One of the starter set.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Directive {
    pub key: &'static str,
    /// A few words, as a nudge lists it: "merging PRs".
    pub topic: &'static str,
    pub question: &'static str,
    /// The case its preference covers (a preference's `when`).
    pub when: &'static str,
    pub answers: &'static [Answer],
}

impl Directive {
    pub fn answer(&self, key: &str) -> Option<&'static Answer> {
        let key = key.trim();
        self.answers
            .iter()
            .find(|a| a.key.eq_ignore_ascii_case(key) || a.label.eq_ignore_ascii_case(key))
    }
}

const fn a(key: &'static str, label: &'static str, then: &'static str) -> Answer {
    Answer { key, label, then }
}

/// The key of a directive in the person's own words.
pub const OWN: &str = "own";
/// The case a directive of their own is kept for.
pub const OWN_WHEN: &str = "anything you decide";
/// The most characters one of their own has.
pub const OWN_CHARS: usize = 300;

/// The starter set, in the order the card shows it.
pub const DIRECTIVES: &[Directive] = &[
    Directive {
        key: "merge",
        topic: "merging PRs",
        question: "Can I merge pull requests?",
        when: "a session's pull request is ready to merge",
        answers: &[
            a("yes", "Yes", "merge it when the work is done"),
            a(
                "checks",
                "After checks pass",
                "merge only once CI is green and any review is done",
            ),
            a(
                "no",
                "No, leave it to me",
                "never merge; leave merging to me",
            ),
        ],
    },
    Directive {
        key: "production",
        topic: "production",
        question: "Can I deploy to production, or change what affects it?",
        when: "work would deploy to production or change what affects it",
        answers: &[
            a(
                "yes",
                "Yes",
                "deploy and change production as the work needs",
            ),
            a(
                "staging",
                "Staging only",
                "deploy to staging only; never to production",
            ),
            a(
                "ask",
                "Ask me first",
                "ask me before anything that touches production",
            ),
        ],
    },
    Directive {
        key: "data",
        topic: "migrations and data",
        question: "Can I run database migrations or change data?",
        when: "work would run a database migration or change data",
        answers: &[
            a(
                "yes",
                "Yes",
                "run migrations and data changes as the work needs",
            ),
            a(
                "additive",
                "Additive only",
                "only additive migrations; never drop, rewrite or delete data",
            ),
            a(
                "ask",
                "Ask me first",
                "ask me before any migration or data change",
            ),
        ],
    },
    Directive {
        key: "delete",
        topic: "deleting things",
        question: "Can I delete things: branches, files, data?",
        when: "work would delete branches, files or data",
        answers: &[
            a("yes", "Yes", "delete what the work no longer needs"),
            a(
                "scratch",
                "Merged branches and scratch files only",
                "delete only merged branches and scratch files; nothing else",
            ),
            a("ask", "Ask me first", "ask me before deleting anything"),
        ],
    },
    Directive {
        key: "spend",
        topic: "spending",
        question: "How freely can I spend on models and paid services?",
        when: "work would spend on models or paid services",
        answers: &[
            a(
                "budget",
                "Within the budget",
                "spend as the work needs, within the daily budget",
            ),
            a(
                "cheapest",
                "Cheapest that works",
                "use the cheapest model or service that does the job",
            ),
            a(
                "ask",
                "Ask before anything paid",
                "ask me before any new paid service or a pricier model",
            ),
        ],
    },
    Directive {
        key: "messages",
        topic: "messaging people",
        question: "Can I message people for you (Slack, PR comments, Jira)?",
        when: "work would message people on my behalf",
        answers: &[
            a(
                "yes",
                "Yes",
                "message people as Desk on my behalf, as the work needs",
            ),
            a(
                "work",
                "PR comments and tickets only",
                "comment on PRs and tickets only; no Slack or direct messages",
            ),
            a("ask", "No, ask me first", "ask me before messaging anyone"),
        ],
    },
    Directive {
        key: "dependencies",
        topic: "new dependencies",
        question: "Can I add dependencies or new services?",
        when: "work would add a dependency or a new service",
        answers: &[
            a("yes", "Yes", "add what the work needs"),
            a(
                "known",
                "Well-known ones only",
                "only widely used, maintained dependencies; no new services",
            ),
            a(
                "ask",
                "Ask me first",
                "ask me before adding any dependency or service",
            ),
        ],
    },
    Directive {
        key: "urgent",
        topic: "when to interrupt you",
        question: "When should I interrupt you?",
        when: "something might need me",
        answers: &[
            a(
                "production",
                "Only if production is at risk",
                "interrupt me only when production or data is at risk",
            ),
            a(
                "blocked",
                "When work is blocked",
                "interrupt me when work is blocked and only I can unblock it",
            ),
            a(
                "decisions",
                "For any real decision",
                "interrupt me for any decision I'd want a say in",
            ),
        ],
    },
];

/// The starter directive `key`.
pub fn find(key: &str) -> Option<&'static Directive> {
    let key = key.trim();
    DIRECTIVES.iter().find(|d| d.key.eq_ignore_ascii_case(key))
}

/// The preference answer `answer` to `d` keeps for the boss of
/// `boss_scope`.
pub fn answered(boss_scope: &str, d: &Directive, answer: &Answer) -> Preference {
    let mut p = Preference::new(
        boss_scope,
        d.when,
        answer.then,
        PrefScope::All,
        PrefSource::Said,
    );
    p.directive = Some(d.key.to_string());
    p.directive_answer = Some(answer.key.to_string());
    p
}

/// One of the person's own, in their words, for the boss of `boss_scope`.
pub fn own(boss_scope: &str, text: &str) -> Result<Preference, String> {
    let t = text.trim();
    if t.is_empty() {
        return Err("Say the directive.".into());
    }
    if t.chars().count() > OWN_CHARS {
        return Err(format!("A directive is at most {OWN_CHARS} characters."));
    }
    let mut p = Preference::new(boss_scope, OWN_WHEN, t, PrefScope::All, PrefSource::Said);
    p.directive = Some(OWN.to_string());
    Ok(p)
}

/// How a directive stands for one boss.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Question {
    pub key: &'static str,
    pub topic: &'static str,
    pub question: &'static str,
    pub answers: Vec<AnswerView>,
    /// The answer's key, when answered.
    pub answer: Option<String>,
    /// Answered for the main boss, not this group's own.
    pub inherited: bool,
    /// The preference that holds the answer.
    pub preference_id: Option<Uuid>,
    /// How often the boss applied it.
    pub applied: i32,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct AnswerView {
    pub key: &'static str,
    pub label: &'static str,
}

/// One of the person's own.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Own {
    pub id: Uuid,
    pub text: String,
    pub inherited: bool,
}

/// What a boss's directives are now, as the card and the Boss sheet show
/// them.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct View {
    pub boss_scope: String,
    pub questions: Vec<Question>,
    pub own: Vec<Own>,
    pub stop_asking: bool,
    pub skipped: bool,
}

impl View {
    /// The ones not answered, here or for the main boss.
    pub fn open(&self) -> Vec<&Question> {
        self.questions
            .iter()
            .filter(|q| q.answer.is_none())
            .collect()
    }
}

/// The asking kept for a person and boss scope.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Asking {
    pub full_since: Option<DateTime<Utc>>,
    pub card_message: Option<String>,
    pub card_at: Option<DateTime<Utc>>,
    pub nudged_at: Option<DateTime<Utc>>,
    pub skipped_at: Option<DateTime<Utc>>,
    pub stop_asking: bool,
}

/// The active directive preferences of `prefs` (any boss's), by directive.
fn active(prefs: &[Preference]) -> impl Iterator<Item = &Preference> {
    prefs
        .iter()
        .filter(|p| p.state == PrefState::Active && p.directive.is_some())
}

/// The directives of the boss of `boss_scope` from `here` (its own active
/// preferences) and `main` (the main boss's, which a group's boss falls back
/// on; empty for the main boss), with `asking`.
pub fn view(boss_scope: &str, here: &[Preference], main: &[Preference], asking: &Asking) -> View {
    let newest = |prefs: &[Preference], key: &str| -> Option<Preference> {
        active(prefs)
            .filter(|p| p.directive.as_deref() == Some(key))
            .filter(|p| p.directive_answer.is_some())
            .max_by_key(|p| p.created_at)
            .cloned()
    };
    let questions = DIRECTIVES
        .iter()
        .map(|d| {
            let (p, inherited) = match newest(here, d.key) {
                Some(p) => (Some(p), false),
                None => (newest(main, d.key), true),
            };
            // An answer no longer in the set reads as unanswered.
            let p = p.filter(|p| {
                p.directive_answer
                    .as_deref()
                    .and_then(|a| d.answer(a))
                    .is_some()
            });
            Question {
                key: d.key,
                topic: d.topic,
                question: d.question,
                answers: d
                    .answers
                    .iter()
                    .map(|a| AnswerView {
                        key: a.key,
                        label: a.label,
                    })
                    .collect(),
                answer: p.as_ref().and_then(|p| p.directive_answer.clone()),
                inherited: p.is_some() && inherited,
                preference_id: p.as_ref().map(|p| p.id),
                applied: p.as_ref().map_or(0, |p| p.applied_count),
            }
        })
        .collect();
    let own = |prefs: &[Preference], inherited: bool| -> Vec<Own> {
        let mut v: Vec<&Preference> = active(prefs)
            .filter(|p| p.directive.as_deref() == Some(OWN))
            .collect();
        v.sort_by_key(|p| (p.created_at, p.id));
        v.into_iter()
            .map(|p| Own {
                id: p.id,
                text: p.then_text.clone(),
                inherited,
            })
            .collect()
    };
    let mut mine = own(here, false);
    mine.extend(own(main, true));
    View {
        boss_scope: boss_scope.to_string(),
        questions,
        own: mine,
        stop_asking: asking.stop_asking,
        skipped: asking.skipped_at.is_some(),
    }
}

/// How long after the card, or the last nudge, the boss may say again
/// what's still open.
pub const NUDGE_EVERY: Duration = Duration::hours(24);

/// Whether the card should go out now: the boss is at Act fully and wasn't
/// last time it was looked at, something is unanswered, and the person
/// hasn't said to stop asking.
pub fn card_due(at_full: bool, asking: &Asking, open: usize) -> bool {
    at_full && asking.full_since.is_none() && open > 0 && !asking.stop_asking
}

/// Whether an update going out at `now` may carry one line on what's still
/// open: at Act fully, something unanswered, not stopped, a quiet update
/// (nothing in it needs the person), and a day since the card or the last
/// nudge.
pub fn nudge_due(
    at_full: bool,
    asking: &Asking,
    open: usize,
    quiet: bool,
    now: DateTime<Utc>,
) -> bool {
    if !at_full || open == 0 || asking.stop_asking || !quiet {
        return false;
    }
    let last = asking.nudged_at.max(asking.card_at);
    last.is_none_or(|l| now - l >= NUDGE_EVERY)
}

/// The one line an update says of what's still open, in its extras.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MentionedNudge {
    pub boss_scope: String,
    pub open: usize,
    #[serde(default)]
    pub topics: Vec<String>,
}

/// The line in words, for where the extras aren't read.
pub fn nudge_line(n: &MentionedNudge) -> String {
    let what = match n.open {
        1 => "1 directive still open".to_string(),
        k => format!("{k} directives still open"),
    };
    format!(
        "{what}: {}. Answer any time in the Boss sheet.",
        n.topics.join(", ")
    )
}

/// The card, in its extras: the boss whose directives it asks.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MentionedDirectives {
    pub boss_scope: String,
}

/// The card's own words, for where its extras aren't read (an older
/// Desk), and as the DM keeps it: one line, never the questions, so the
/// boss doesn't take them up again in the chat.
pub fn card_text(v: &View) -> String {
    let topics: Vec<&str> = v.open().iter().map(|q| q.topic).collect();
    format!(
        "Before I run on my own: a few standing answers ({}), so I decide the way you would. Answer any in the Boss sheet, or tell me.",
        topics.join(", ")
    )
}

/// What the boss's turns are told of the directives: the answered ones as
/// orders, with their preference ids, and what's unanswered with how to
/// decide it. `None` with nothing answered and nothing to say.
pub fn context(v: &View, prefs: &[Preference], unbound: bool) -> Option<String> {
    let by_id = |id: Uuid| prefs.iter().find(|p| p.id == id);
    let mut lines = Vec::new();
    for q in &v.questions {
        let (Some(id), Some(_)) = (q.preference_id, q.answer.as_deref()) else {
            continue;
        };
        let then = by_id(id).map_or("", |p| p.then_text.as_str());
        let from = if q.inherited { ", the main boss's" } else { "" };
        lines.push(format!(
            "- {}: {then} (preference {id}{from})",
            cap(q.topic)
        ));
    }
    for o in &v.own {
        lines.push(format!("- Their own: {} (preference {})", o.text, o.id));
    }
    let open: Vec<&str> = v.open().iter().map(|q| q.topic).collect();
    if lines.is_empty() && (open.is_empty() || !unbound) {
        return None;
    }
    let mut s = String::new();
    if !lines.is_empty() {
        s.push_str(
            "The person's directives (their standing orders for when you act on your own: they win over your own judgment, both ways; apply one by its id as preference, which cites it):\n",
        );
        s.push_str(&lines.join("\n"));
        s.push('\n');
    }
    if unbound && !open.is_empty() {
        s.push_str(&format!(
            "Directives not answered yet: {}. When a decision falls under one, decide it yourself as Act fully does (nothing waits on them) and say \"my own call: no directive on <topic> yet\" in its why. Don't ask the person for them yourself.\n",
            open.join(", ")
        ));
    }
    Some(s)
}

fn cap(s: &str) -> String {
    let mut c = s.chars();
    match c.next() {
        Some(f) => f.to_uppercase().collect::<String>() + c.as_str(),
        None => String::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::preference::MAIN;

    #[test]
    fn the_starter_set_has_distinct_keys_and_answers() {
        assert!((6..=8).contains(&DIRECTIVES.len()));
        let mut keys: Vec<&str> = DIRECTIVES.iter().map(|d| d.key).collect();
        keys.sort();
        keys.dedup();
        assert_eq!(keys.len(), DIRECTIVES.len());
        assert!(!keys.contains(&OWN));
        for d in DIRECTIVES {
            assert!((2..=3).contains(&d.answers.len()), "{}", d.key);
            for an in d.answers {
                assert_eq!(d.answer(an.key), Some(an));
                assert_eq!(d.answer(&an.label.to_uppercase()), Some(an));
            }
        }
        assert!(find("MERGE").is_some() && find("nope").is_none());
    }

    #[test]
    fn a_groups_boss_falls_back_on_the_main_bosss_answers() {
        let merge = find("merge").unwrap();
        let main = vec![
            answered(MAIN, merge, merge.answer("checks").unwrap()),
            own(MAIN, "Never touch billing").unwrap(),
        ];
        let v = view("g1", &[], &main, &Asking::default());
        let q = &v.questions[0];
        assert_eq!(q.answer.as_deref(), Some("checks"));
        assert!(q.inherited);
        assert_eq!(v.own.len(), 1);
        assert!(v.own[0].inherited);
        assert_eq!(v.open().len(), DIRECTIVES.len() - 1);
        // Its own answer wins.
        let here = vec![answered("g1", merge, merge.answer("no").unwrap())];
        let v = view("g1", &here, &main, &Asking::default());
        assert_eq!(v.questions[0].answer.as_deref(), Some("no"));
        assert!(!v.questions[0].inherited);
        // A forgotten one is unanswered.
        let mut gone = here.clone();
        gone[0].state = PrefState::Retired;
        let v = view("g1", &gone, &[], &Asking::default());
        assert!(v.questions[0].answer.is_none());
    }

    #[test]
    fn the_card_goes_once_per_act_fully_and_never_when_stopped() {
        let a = Asking::default();
        assert!(card_due(true, &a, 3));
        assert!(!card_due(false, &a, 3));
        assert!(!card_due(true, &a, 0));
        let seen = Asking {
            full_since: Some(Utc::now()),
            ..a.clone()
        };
        assert!(!card_due(true, &seen, 3));
        let stopped = Asking {
            stop_asking: true,
            ..a
        };
        assert!(!card_due(true, &stopped, 3));
    }

    #[test]
    fn a_nudge_is_once_a_day_at_a_quiet_moment() {
        let now = Utc::now();
        let a = Asking {
            card_at: Some(now - Duration::hours(25)),
            ..Default::default()
        };
        assert!(nudge_due(true, &a, 2, true, now));
        assert!(!nudge_due(true, &a, 2, false, now), "not quiet");
        assert!(!nudge_due(false, &a, 2, true, now), "not at Act fully");
        assert!(!nudge_due(true, &a, 0, true, now), "all answered");
        let soon = Asking {
            card_at: Some(now - Duration::hours(25)),
            nudged_at: Some(now - Duration::hours(3)),
            ..Default::default()
        };
        assert!(!nudge_due(true, &soon, 2, true, now));
        let stopped = Asking {
            stop_asking: true,
            ..a
        };
        assert!(!nudge_due(true, &stopped, 2, true, now));
    }

    #[test]
    fn the_boss_is_told_the_answers_as_orders_and_what_is_its_own_call() {
        let merge = find("merge").unwrap();
        let prefs = vec![answered(MAIN, merge, merge.answer("checks").unwrap())];
        let v = view(MAIN, &prefs, &[], &Asking::default());
        let s = context(&v, &prefs, true).unwrap();
        assert!(s.contains("they win over your own judgment"), "{s}");
        assert!(
            s.contains("Merging PRs: merge only once CI is green"),
            "{s}"
        );
        assert!(s.contains(&prefs[0].id.to_string()));
        assert!(s.contains("my own call: no directive on"), "{s}");
        assert!(!s.contains("merging PRs,"), "answered isn't open: {s}");
        // Not at Act fully: only the orders.
        let s = context(&v, &prefs, false).unwrap();
        assert!(!s.contains("not answered yet"), "{s}");
        assert!(context(&view(MAIN, &[], &[], &Asking::default()), &[], false).is_none());
    }

    #[test]
    fn ones_own_is_said_and_bounded() {
        assert!(own(MAIN, "  ").is_err());
        assert!(own(MAIN, &"x".repeat(OWN_CHARS + 1)).is_err());
        let p = own(MAIN, "Never touch billing.").unwrap();
        assert_eq!(p.directive.as_deref(), Some(OWN));
        assert_eq!(p.then_text, "Never touch billing");
        assert!(card_text(&view(MAIN, &[], &[], &Asking::default()))
            .contains("merging PRs, production"));
    }
}
